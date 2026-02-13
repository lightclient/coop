# Verify Cron Scheduler Fixes — E2E Trace Verification

## Goal

Verify three cron scheduler bug fixes by running coop with tracing enabled, letting real cron jobs fire against the Anthropic API, and checking the JSONL traces for correct behavior. This catches regressions that unit tests cannot: real timer precision, real LLM responses, real delivery code paths.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Duration & Completion

**Estimated time:** 5–10 minutes.

**The task is finished when:**
1. All three verifications pass (confirmed via trace evidence), AND
2. `cargo test -p coop-gateway` passes, AND
3. `cargo clippy --all-targets --all-features -- -D warnings` passes.

## Background

Three bugs were found via trace analysis of production JSONL logs. All are in `crates/coop-gateway/src/scheduler.rs`.

### Bug 1: Scheduler double-fire

The scheduler called `sched.upcoming(Utc).next()` to compute the next fire time. The `cron` crate's `upcoming()` internally queries from `Utc::now() + 1s`, but the `+1s` operates at sub-second precision while matching operates at second granularity. When `tokio::time::sleep` wakes a few milliseconds early (normal timer behavior), `now + 1s` can land in the same second as the target, causing the cron library to return the same tick again.

**Fix:** The scheduler now tracks the last *scheduled* fire time per cron and queries `sched.after(&last_fired_time)` instead of `sched.upcoming(Utc)`. Since `after(T)` internally starts from `T + 1s` at exact second granularity, the same tick is never returned twice. No debounce needed.

**Also fixed:** The scheduler previously used `min_by_key` to pick a single cron per iteration. If two crons shared the same fire time, only one would fire. Now it collects all crons at the minimum fire time and fires all of them.

### Bug 2: Empty response from skipped turn treated as HEARTBEAT_OK

When a duplicate fire hit the turn lock (from Bug 1), the gateway returned an empty string. `strip_heartbeat_token("")` returned `Suppress` (empty → suppress), logging `"heartbeat suppressed: HEARTBEAT_OK token detected"` — misleading, since no HEARTBEAT_OK was in the response.

**Fix:** `deliver_cron_response()` checks `response.trim().is_empty()` before calling `strip_heartbeat_token`. Empty responses log `"cron produced empty response, skipping delivery"`.

### Bug 3: HEARTBEAT_OK prompt sent to all cron types

All crons with delivery targets were told `"Reply HEARTBEAT_OK if nothing needs attention"`, including morning briefings and mood check-ins. A briefing should always deliver.

**Fix:** The HEARTBEAT_OK instruction and suppression logic are now scoped to crons whose message contains `"HEARTBEAT.md"`. Non-heartbeat crons get a simpler prefix and their responses are delivered unconditionally.

## Setup

### 1. Build

```bash
cargo build
```

### 2. Create test config

Write `coop.e2e-test.toml` in the project root. This config runs two cron jobs that fire every minute at different offsets so both fire within one observation window:

```toml
[agent]
id = "coop"
model = "anthropic/claude-sonnet-4-20250514"
workspace = "./workspaces/default"

[[users]]
match = ["terminal:default"]
name = "alice"
trust = "full"

[provider]
name = "anthropic"

# Heartbeat cron: references HEARTBEAT.md → gets HEARTBEAT_OK suppression.
# Fires at second 0 of each minute (7-field: sec min hour dom month dow year).
[[cron]]
name = "heartbeat"
cron = "0 * * * * * *"
message = "Check HEARTBEAT.md and report anything that needs attention."
user = "alice"
[cron.deliver]
channel = "signal"
target = "alice-uuid"

# Non-heartbeat cron: no HEARTBEAT.md → should NOT offer HEARTBEAT_OK.
# Fires at second 30 of each minute.
[[cron]]
name = "evening-mood-checkin"
cron = "30 * * * * * *"
message = "Send Alice a brief, friendly mood check-in. Ask how her evening is going. Keep it to one sentence."
user = "alice"
[cron.deliver]
channel = "signal"
target = "alice-uuid"

[memory]
db_path = "./db/memory.db"
```

The `deliver` targets reference a placeholder — without the Signal feature enabled, delivery attempts log a warning but the full scheduler → gateway → response-handling code path is still exercised.

### 3. Ensure HEARTBEAT.md has content

The heartbeat cron reads HEARTBEAT.md. If it's empty (only headers/empty checklists), the scheduler skips the LLM call entirely (`should_skip_heartbeat`). Write real content so the LLM call happens and the agent can respond with HEARTBEAT_OK:

```bash
cat workspaces/default/HEARTBEAT.md
```

If the file contains only `# Heartbeat Tasks` and blank lines, add a task:

```bash
cat > workspaces/default/HEARTBEAT.md << 'EOF'
# Heartbeat Tasks

- [ ] Check if any reminders need attention
EOF
```

### 4. Verify config

```bash
cargo run -- check -c coop.e2e-test.toml
```

All checks must pass including both cron entries showing `✓`.

### 5. Start coop with tracing

Start coop in a tmux session (use the tmux skill) so it runs in the background:

```bash
rm -f e2e-traces.jsonl
cd /root/coop/main && COOP_TRACE_FILE=e2e-traces.jsonl RUST_LOG=debug \
  cargo run -- start -c coop.e2e-test.toml
```

Verify startup in the traces:

```bash
grep '"scheduler started"' e2e-traces.jsonl
grep '"scheduler cron entries updated"' e2e-traces.jsonl
```

Both should appear. The scheduler log should show `count=2`.

### 6. Wait for cron fires

Both crons must fire at least once each. The heartbeat fires at second :00, the mood-checkin at second :30. Wait for at least 1 full minute plus buffer for LLM response time:

```bash
sleep 100
```

### 7. Stop coop

Send Ctrl-C to the tmux session.

## Verification

Analyze `e2e-traces.jsonl` for each bug fix. All three must pass.

### Verification 1: No double-fires (Bug 1)

**What to check:** Each cron fires exactly once per scheduled tick. No duplicate `cron firing` events within the same second for the same cron.

```bash
# Count fires per cron name
grep '"cron firing"' e2e-traces.jsonl | \
  python3 -c "
import sys, json
counts = {}
for line in sys.stdin:
    d = json.loads(line)
    for s in d.get('spans', []):
        name = s.get('cron.name', '')
        if name:
            counts[name] = counts.get(name, 0) + 1
for name, count in sorted(counts.items()):
    print(f'  {name}: {count} fires')
"
```

**Pass criteria:**
- `heartbeat` fires 1-2 times (depending on timing within the observation window)
- `evening-mood-checkin` fires 1-2 times
- No `"skipping turn: another turn is already running"` events for cron sessions
- No `"debounced"` events (the debounce was removed — if this appears, old code is running)

**Detailed check — no same-second duplicates:**

```bash
grep '"cron firing"' e2e-traces.jsonl | \
  python3 -c "
import sys, json
from collections import defaultdict
fires = defaultdict(list)
for line in sys.stdin:
    d = json.loads(line)
    ts = d['timestamp'][:19]  # truncate to second
    for s in d.get('spans', []):
        name = s.get('cron.name', '')
        if name:
            fires[name].append(ts)
ok = True
for name, times in sorted(fires.items()):
    dupes = len(times) - len(set(times))
    status = '✅' if dupes == 0 else '❌'
    print(f'  {status} {name}: {len(times)} fires, {dupes} same-second duplicates')
    if dupes > 0:
        ok = False
if ok:
    print('  ✅ PASS: no double-fires')
else:
    print('  ❌ FAIL: double-fires detected')
"
```

### Verification 2: Empty response handling (Bug 2)

**What to check:** No false `"heartbeat suppressed: HEARTBEAT_OK token detected"` events. Suppression should only occur when the LLM actually responds with HEARTBEAT_OK, never for empty/skipped responses.

```bash
# Check all suppression events — each must be on a heartbeat cron
grep '"heartbeat suppressed"' e2e-traces.jsonl | \
  python3 -c "
import sys, json
ok = True
count = 0
for line in sys.stdin:
    d = json.loads(line)
    cron = d.get('fields', {}).get('cron.name', '')
    count += 1
    if cron != 'heartbeat':
        print(f'  ❌ suppression on non-heartbeat cron: {cron}')
        ok = False
if count == 0:
    print('  ⚠️  No suppressions observed (heartbeat may have returned real content)')
    print('     This is OK if the LLM chose to report on the task instead of HEARTBEAT_OK')
else:
    print(f'  {count} suppression(s), all on heartbeat cron')
if ok:
    print('  ✅ PASS: no false suppressions')
"
```

**Also verify:** No `"cron produced empty response"` events (these would indicate a skipped turn from a lock collision — which Bug 1's fix prevents):

```bash
count=$(grep -c '"cron produced empty response"' e2e-traces.jsonl 2>/dev/null || echo 0)
echo "  Empty response events: $count (expected: 0)"
```

### Verification 3: HEARTBEAT_OK scope (Bug 3)

**What to check:** Two things — the prompt content and the suppression behavior differ between heartbeat and non-heartbeat crons.

#### 3a. Prompt content

The heartbeat cron's prompt must include `"Reply HEARTBEAT_OK if nothing needs attention"`. The mood-checkin's prompt must NOT include that instruction.

```bash
grep "user_input" e2e-traces.jsonl | \
  python3 -c "
import sys, json
seen = {}
for line in sys.stdin:
    d = json.loads(line)
    for s in d.get('spans', []):
        session = s.get('session', '')
        ui = s.get('user_input', '')
        if ui and session not in seen:
            seen[session] = ui

for session, ui in sorted(seen.items()):
    if 'cron:heartbeat' in session:
        has = 'HEARTBEAT_OK' in ui
        status = '✅' if has else '❌'
        print(f'  {status} heartbeat prompt includes HEARTBEAT_OK: {has}')
    elif 'mood' in session:
        has = 'HEARTBEAT_OK' in ui
        status = '✅' if not has else '❌'
        print(f'  {status} mood-checkin prompt omits HEARTBEAT_OK: {not has}')
"
```

#### 3b. Non-heartbeat crons deliver unconditionally

The mood-checkin cron must attempt delivery even if the LLM happens to include "HEARTBEAT_OK" in its response. Check that delivery is attempted (the `"delivery target resolved but no delivery sender available"` warning proves the code reached the delivery path):

```bash
grep '"delivery target resolved"' e2e-traces.jsonl | \
  python3 -c "
import sys, json
from collections import defaultdict
deliveries = defaultdict(int)
for line in sys.stdin:
    d = json.loads(line)
    for s in d.get('spans', []):
        name = s.get('cron.name', '')
        if name:
            deliveries[name] += 1
for name, count in sorted(deliveries.items()):
    print(f'  {name}: {count} delivery attempt(s)')

mood = deliveries.get('evening-mood-checkin', 0)
if mood > 0:
    print(f'  ✅ PASS: mood-checkin delivered {mood} time(s) (never suppressed)')
else:
    print(f'  ❌ FAIL: mood-checkin never delivered')
"
```

### Verification 4: Concurrent cron fire (multi-cron fix)

If both crons happen to be computed for the same tick (unlikely with the :00/:30 stagger, but worth checking), verify both would fire. This is validated structurally: check that the scheduler collects all upcoming crons, not just one:

```bash
# Both crons must have fired at least once
heartbeat=$(grep '"cron firing"' e2e-traces.jsonl | grep -c '"heartbeat"' || echo 0)
mood=$(grep '"cron firing"' e2e-traces.jsonl | grep -c '"mood"' || echo 0)

if [ "$heartbeat" -ge 1 ] && [ "$mood" -ge 1 ]; then
    echo "  ✅ PASS: both crons fired (heartbeat=$heartbeat, mood=$mood)"
else
    echo "  ❌ FAIL: missing fires (heartbeat=$heartbeat, mood=$mood)"
fi
```

### Full timeline (for manual inspection)

Print a human-readable timeline of all scheduler activity:

```bash
grep '"cron firing"\|"cron completed"\|"heartbeat suppressed"\|"delivery target resolved"\|"cron produced empty response"\|"scheduler waiting"' e2e-traces.jsonl | \
  python3 -c "
import sys, json
for line in sys.stdin:
    d = json.loads(line)
    ts = d['timestamp'][11:23]
    msg = d['fields']['message']
    cron = d['fields'].get('cron.name', '')
    if not cron:
        for s in d.get('spans', []):
            if 'cron.name' in s:
                cron = s['cron.name']
                break
    icon = {'cron firing': '🔥', 'cron completed': '✅', 'heartbeat suppressed': '🚫',
            'delivery target resolved': '📤', 'cron produced empty response': '⚠️',
            'scheduler waiting': '⏰'}.get(msg, '  ')
    print(f'{icon} {ts} | {cron:25s} | {msg}')
"
```

Expected pattern for a healthy run:

```
⏰ HH:MM:SS | heartbeat                 | scheduler waiting for next fire
🔥 HH:MM:00 | heartbeat                 | cron firing
✅ HH:MM:0X | heartbeat                 | cron completed
🚫 HH:MM:0X | heartbeat                 | heartbeat suppressed: HEARTBEAT_OK token detected
⏰ HH:MM:0X | evening-mood-checkin      | scheduler waiting for next fire
🔥 HH:MM:30 | evening-mood-checkin      | cron firing
✅ HH:MM:3X | evening-mood-checkin      | cron completed
📤 HH:MM:3X | evening-mood-checkin      | delivery target resolved but no delivery sender available
```

Key observations:
- Each cron fires once per tick (🔥), never doubled
- Heartbeat: fires → completed → **suppressed** (HEARTBEAT_OK)
- Mood-checkin: fires → completed → **delivered** (📤), never suppressed
- No ⚠️ empty response events

## Cleanup

After verification, restore the original state:

```bash
# Restore HEARTBEAT.md to empty (if it was modified)
printf '# Heartbeat Tasks\n\n' > workspaces/default/HEARTBEAT.md

# Remove test config and traces
rm -f coop.e2e-test.toml e2e-traces.jsonl
```

## Failure Modes

If a verification fails, check these common issues:

| Symptom | Likely cause |
|---------|-------------|
| Only one cron fires | Cron expressions parse to the same time; `min_by_key` picks one. Check that 7-field expressions are used (`0 * * * * * *` and `30 * * * * * *`). |
| Double-fires appear | Old code using `sched.upcoming(Utc)` instead of `sched.after(&last_fired)`. Check `scheduler.rs` around line 148. |
| `"heartbeat suppressed"` on mood-checkin | Bug 3 fix not applied — `strip_heartbeat_token` is still called for all crons. Check `deliver_cron_response()`. |
| `"cron produced empty response"` | A second fire hit the session lock (Bug 1 not fixed), or the LLM returned nothing. Check for double-fires first. |
| Heartbeat cron skipped entirely | HEARTBEAT.md is empty (only headers/blank checkboxes). Add a real task. |
| No fires at all | `ANTHROPIC_API_KEY` not set, or cron expressions invalid. Run `cargo run -- check -c coop.e2e-test.toml`. |
| `"cron delivery configured but no delivery sender available"` at startup | Expected — Signal is not enabled. This is a startup warning, not a runtime failure. The scheduler still fires and the response-handling code path is fully exercised. |
