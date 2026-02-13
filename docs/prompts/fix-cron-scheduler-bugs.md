# Fix Cron Scheduler Bugs

Fix three bugs in the cron scheduler discovered via trace analysis of `bug.jsonl`. These are logic bugs in the scheduler and heartbeat suppression — Signal delivery itself works correctly.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Bug 1: Scheduler double-fire race condition

### Problem

Every cron tick fires the job twice. The scheduler loop computes `next_fire`, sleeps until that time, then spawns `fire_cron`. But the cron expression can match two nearby instants (sub-second precision), causing the scheduler loop to iterate again immediately and spawn a second `fire_cron` for the same logical tick.

Trace evidence — every single cron in `bug.jsonl` shows this pattern:

```
02:59:59.994 | cleared cron session  (1st fire acquired turn lock, started LLM call)
03:00:00.012 | skipping turn         (2nd fire, lock held → empty response)
03:00:00.012 | cron completed        (2nd fire returns immediately)
03:00:03.213 | cron completed        (1st fire, actual LLM response)
03:00:03.213 | cron delivery sent    (real delivery happens here)
```

The 2nd fire is always wasted — it hits the `try_lock()` in `gateway.rs:337`, gets rejected, and returns an empty response. This happens for every cron type (heartbeat, morning-briefing, evening-mood-checkin) on every tick.

### Fix

Track the last fire time per cron job in the scheduler loop. After a fire, skip re-firing the same cron if less than 30 seconds have elapsed since its last fire. This is a simple debounce — no changes to the cron expression parsing or the gateway turn lock needed.

In `crates/coop-gateway/src/scheduler.rs`, add a `HashMap<String, DateTime<Utc>>` for last-fire tracking. In the sleep handler, before spawning `fire_cron`, check:

```rust
let now = Utc::now();
if let Some(last) = last_fired.get(&cfg.name) {
    if now - *last < chrono::Duration::seconds(30) {
        debug!(cron.name = %cfg.name, "debounced: fired recently");
        continue;
    }
}
last_fired.insert(cfg.name.clone(), now);
```

### Tests

Add a test in the `#[cfg(test)]` block of `scheduler.rs`:

- `scheduler_debounces_rapid_fires` — call `fire_cron` twice in quick succession, verify only one LLM call is made. Use the existing `FakeProvider` pattern from adjacent tests and check its call count.

## Bug 2: Empty response from skipped turn treated as HEARTBEAT_OK

### Problem

When the 2nd (duplicate) fire hits the turn lock and gets skipped, `dispatch_collect_text_with_channel` returns `Ok((decision, ""))` — an empty string. Back in `fire_cron`, this empty string goes through:

```rust
match strip_heartbeat_token(&response) {  // response = ""
```

In `heartbeat.rs:29`:
```rust
let trimmed = text.trim();
if trimmed.is_empty() {
    return HeartbeatResult::Suppress;
}
```

Empty → `Suppress`. The log then says `"heartbeat suppressed: HEARTBEAT_OK token detected"` which is misleading — no HEARTBEAT_OK was in the response. The turn was simply skipped.

This doesn't cause delivery failures (the 1st fire delivers correctly), but it generates 22 false "heartbeat suppressed" log entries per day, making it impossible to distinguish real suppressions from skipped-turn artifacts.

### Fix

**In `fire_cron` (scheduler.rs):** Check for empty response before heartbeat token stripping. An empty response means the turn was skipped or the LLM returned nothing — it should not be treated as HEARTBEAT_OK.

```rust
Ok((decision, response)) => {
    info!(
        session = %decision.session_key,
        trust = ?decision.trust,
        user = ?decision.user_name,
        "cron completed"
    );

    if delivery_targets.is_empty() {
        return;
    }

    // Empty response means the turn was skipped (session lock held)
    // or the LLM returned nothing. Don't treat as HEARTBEAT_OK.
    if response.trim().is_empty() {
        debug!(cron.name = %cfg.name, "cron produced empty response, skipping delivery");
        return;
    }

    match strip_heartbeat_token(&response) {
        // ...
    }
}
```

This is the minimal fix. Do NOT change `strip_heartbeat_token` itself — its behavior of suppressing empty strings is correct for its own API contract. The fix belongs in the caller.

### Tests

- `fire_cron_empty_response_does_not_suppress` — configure a provider that returns `""`, verify no delivery is sent AND the log message is "empty response" not "HEARTBEAT_OK".
- `fire_cron_heartbeat_ok_still_suppresses` — provider returns `"HEARTBEAT_OK"`, verify suppression still works.

## Bug 3: HEARTBEAT_OK prompt sent to all cron types

### Problem

In `fire_cron`, the message prefix for ALL crons with delivery targets is:

```
[Your response will be delivered to the user via {channel}. Reply HEARTBEAT_OK if nothing needs attention. Your response is delivered automatically.]
```

This means morning-briefing and evening-mood-checkin crons are told they can reply HEARTBEAT_OK to suppress delivery. A morning briefing should ALWAYS deliver — it's not a health check. The HEARTBEAT_OK escape hatch only makes sense for heartbeat/monitoring crons where "nothing to report" is a valid outcome.

### Fix

Only include the HEARTBEAT_OK instruction when the cron message references `HEARTBEAT.md` (same heuristic already used by `should_skip_heartbeat`). Otherwise, use a simpler prefix that doesn't offer the suppression option.

In `fire_cron`, replace the single `format!` with a conditional:

```rust
let content = if delivery_targets.is_empty() {
    cfg.message.clone()
} else if cfg.message.contains("HEARTBEAT.md") {
    format!(
        "[Your response will be delivered to the user via {}. Reply HEARTBEAT_OK if nothing needs attention. Your response is delivered automatically.]\n\n{}",
        prompt_channel.as_deref().unwrap_or("messaging"),
        cfg.message
    )
} else {
    format!(
        "[Your response will be delivered to the user via {}. Your response is delivered automatically.]\n\n{}",
        prompt_channel.as_deref().unwrap_or("messaging"),
        cfg.message
    )
};
```

And correspondingly, skip the `strip_heartbeat_token` check for non-heartbeat crons. After the empty-response check from Bug 2, add:

```rust
// Only heartbeat crons support HEARTBEAT_OK suppression.
if cfg.message.contains("HEARTBEAT.md") {
    match strip_heartbeat_token(&response) {
        HeartbeatResult::Suppress => {
            debug!(cron.name = %cfg.name, "heartbeat suppressed: HEARTBEAT_OK token detected");
            return;
        }
        HeartbeatResult::Deliver(cleaned) => {
            deliver(&cleaned, ...);
        }
    }
} else {
    deliver(&response, ...);
}
```

Extract the delivery loop into a local closure or helper to avoid duplicating the `announce_to_session` calls.

### Tests

- `fire_cron_non_heartbeat_always_delivers` — cron message is "Send Matt an 8pm mood check-in" (no HEARTBEAT.md reference), provider returns "HEARTBEAT_OK", verify delivery IS sent (not suppressed).
- `fire_cron_heartbeat_still_suppresses` — cron message contains "HEARTBEAT.md", provider returns "HEARTBEAT_OK", verify delivery is suppressed.

## File summary

| File | Changes |
|---|---|
| `crates/coop-gateway/src/scheduler.rs` | Debounce logic, empty-response guard, heartbeat-only suppression scope |
| `crates/coop-gateway/src/heartbeat.rs` | No changes needed |
| `crates/coop-gateway/src/gateway.rs` | No changes needed |
| `crates/coop-gateway/src/router.rs` | No changes needed |

## Verification

After implementing, run the full trace-driven verification:

```bash
cargo test -p coop-gateway
cargo clippy --all-targets --all-features -- -D warnings
```

Then run with `COOP_TRACE_FILE=traces.jsonl` and verify:
1. Each cron tick produces exactly ONE `cron_fired` span (no duplicate)
2. Heartbeat crons with HEARTBEAT_OK response show `"heartbeat suppressed"` (correct)
3. Non-heartbeat crons never show `"heartbeat suppressed"`
4. No `"skipping turn: another turn is already running"` for cron sessions
