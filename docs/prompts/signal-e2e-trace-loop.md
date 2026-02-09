# Signal E2E Trace Loop — Agent-Driven Debugging via Real Signal Interactions

## Goal

Execute a structured test plan against a live coop instance over real Signal, logging every bug found and fix applied. The agent works through an ordered list of test scenarios, verifies each one passes via traces, and stops when the full plan is green.

This catches bugs that unit tests can't: protocol edge cases, timing issues, presage/Signal quirks, message ordering, history bootstrap races, typing indicator lifecycle, and real-world content parsing.

## Duration & Completion

**Estimated time:** 90–150 minutes depending on bugs found.

- Scenarios 1-9 (protocol basics): ~1-2 minutes each when clean.
- Scenarios 10-18 (tool use): ~2-4 minutes each (LLM calls + tool execution).
- Scenario 19 (compaction): ~10-15 minutes (many sequential messages to build up tokens).
- A bug adds 10–20 minutes (diagnose, write test, fix, rebuild, restart, re-verify).
- A clean run with zero bugs: ~50 minutes.
- A run that finds 3-4 bugs: ~90-150 minutes.

**The task is finished when:**
1. Every scenario in the Test Plan (1-19) has status ✅ PASS, AND
2. All bugs found have status `Fixed` in `docs/bugs/`, AND
3. `cargo test --workspace` passes, AND
4. The session log entry in `docs/bugs/SESSION-LOG.md` is written.

**If you get stuck** on a bug for more than 20 minutes (3+ failed fix attempts), mark it `Open` in the bug file, note what you tried, and move to the next scenario. Report it in the session log.

## Execution Order

1. **Setup** — Build coop, start it in tmux with tracing, verify signal-cli connectivity (Prerequisites + Workflow steps 1-2)
2. **Phase 1: Protocol basics (scenarios 1-9)** — Message delivery, commands, reactions, serialization. Fast — no LLM tool loops.
3. **Phase 2: Tool use (scenarios 10-18)** — File I/O, bash, memory, config, multi-tool chains. Slower — each involves LLM reasoning + tool execution.
4. **Phase 3: Compaction (scenario 19)** — Long conversation that builds up >100k tokens to trigger context summarization. Do not clear the session before this — it depends on accumulated history from Phase 2.
5. **Clean Sweep** — Restart with empty traces, run representative subset back-to-back, verify zero errors.
6. **Wrap Up** — Run `cargo test --workspace`. Write session log entry. Done.

Do not skip scenarios. Do not reorder. Each scenario builds on the previous (e.g., scenario 6 clears the session that scenarios 2-5 built up, scenario 14 writes memory that scenario 15 might find, scenario 19 needs the accumulated tokens from 10-18).

## Architecture

The test setup uses **two separate Signal accounts on two separate hosts**:

1. **Remote host** (`signal-host`): Runs signal-cli with its own registered Signal account. This is the **test sender** — it plays the role of "Alice" sending messages to coop.
2. **This host** (coop server): Runs coop with presage, linked as a **secondary device** to a different Signal account. There is also a local signal-cli installed, which acts as the **primary device** for coop's account (used only for linking, not for sending test messages).

The key insight: the **local signal-cli** and **coop** share the same Signal account (primary + secondary device). The **remote signal-cli** is a completely separate account that messages coop's account from outside.

```
  Remote Host (signal-host)                      This Host (coop server)
┌───────────────────────┐                       ┌──────────────────────────────┐
│  signal-cli            │   Signal Protocol     │  local signal-cli             │
│  (Alice's account)     │ ───────────────────▶  │  (Bob's account - primary)    │
│  /opt/homebrew/bin/    │ ◀───────────────────  │  /usr/local/bin/signal-cli    │
│  signal-cli            │                       │  (used only for linking)      │
│                        │                       │                              │
│  Test sender           │                       │  coop (presage)              │
│                        │                       │  (Bob's account - secondary) │
│                        │                       │  COOP_TRACE_FILE=traces.jsonl│
└───────────────────────┘                       └──────────────────────────────┘
        ▲                                                    │
        │ ssh signal-host                                    │ read tool
        │                                                    ▼
┌───────────────────────────────────────────────────────────────────┐
│                        pi (you, the agent)                        │
│   Sends signal-cli commands via SSH to remote host                │
│   Reads traces.jsonl locally                                      │
└───────────────────────────────────────────────────────────────────┘
```

### Account roles

| Host | Tool | Signal Account | Role |
|------|------|---------------|------|
| Remote (`signal-host`) | signal-cli (homebrew) | Alice's number | Test sender — sends messages TO coop |
| This host | signal-cli (local) | Bob's number | Primary device — used only for `addDevice` linking |
| This host | coop (presage) | Bob's number | Secondary device — receives and processes messages |

### Why two signal-cli installs?

- The **remote** signal-cli is the test driver. It has Alice's account and sends messages to Bob's number (which coop receives).
- The **local** signal-cli is Bob's primary device. It's needed only once: to run `signal-cli addDevice --uri <provisioning_url>` when linking coop as a secondary device. After linking, it's not used during testing.

## Prerequisites

### Remote signal-cli (test sender)

The remote host has signal-cli installed via **Homebrew** at `/opt/homebrew/bin/signal-cli` with a registered Signal account. SSH access must be configured with key-based auth (no password prompts).

Verify:

```bash
ssh signal-host /opt/homebrew/bin/signal-cli -o json listAccounts
# Should return: [{"number":"+1XXXXXXXXXX"}]
```

SSH config for the remote host (in `~/.ssh/config`):

```
Host signal-host
    HostName <remote-hostname>
    User <remote-user>
    ControlMaster auto
    ControlPath ~/.ssh/sockets/%r@%h-%p
    ControlPersist 600
```

Create the socket directory: `mkdir -p ~/.ssh/sockets`

### Local signal-cli (primary device)

The local host has signal-cli at `/usr/local/bin/signal-cli` with its own registered account. This is the **primary device** for coop's Signal identity.

Verify:

```bash
signal-cli -o json listAccounts
# Should return: [{"number":"+1YYYYYYYYYY"}]
```

This account's phone number (`+1YYYYYYYYYY`) is the **TARGET** that the remote signal-cli sends messages to.

### Convenience wrapper

All examples in this doc use `$SIGNAL_CLI` as the command prefix for the **remote** test sender:

```bash
export SIGNAL_CLI="ssh signal-host /opt/homebrew/bin/signal-cli"
export TARGET="+1YYYYYYYYYY"  # coop's phone number (local signal-cli's account)
```

### coop

**coop** must be built with the `signal` feature and configured with a linked Signal account. The config is at `coop.yaml` and the Signal DB at `./db/signal.db`.

Verify:

```bash
# coop builds with signal
cargo build --features signal

# coop config has signal channel
grep -A2 'signal:' coop.yaml
```

### Linking coop as a secondary device

If `./db/signal.db` doesn't exist, coop needs to be linked to the local signal-cli's account:

```bash
# 1. Create the db directory
mkdir -p db

# 2. Start the linking process — coop shows a QR code and provisioning URL
cargo run --features signal --bin coop -- signal link

# 3. In another terminal, use the LOCAL signal-cli to accept the link.
#    Copy the provisioning URL from coop's output (starts with sgnl://linkdevice?...)
signal-cli addDevice --uri 'sgnl://linkdevice?uuid=...&pub_key=...'

# 4. Coop prints "signal linking completed" with service_ids (ACI + PNI UUIDs).
#    The ACI UUID is coop's identity.
```

After linking, coop can receive messages sent to the local signal-cli's phone number.

### Configuring the user match pattern

The `coop.yaml` users section must contain the **remote sender's UUID** (not coop's UUID) so that incoming messages get matched to a user with the correct trust level.

To discover the remote sender's UUID:

1. Start coop and send a test message from the remote signal-cli
2. Check traces for the sender UUID: `grep '"signal inbound dispatched"' traces.jsonl | jq -r '.fields."signal.sender"'`
3. Update `coop.yaml`:

```yaml
users:
  - name: alice
    trust: full
    match: ["terminal:default", "signal:<remote-sender-uuid>"]
```

Coop hot-reloads user config changes — no restart needed.

## Step-by-Step Workflow

### 1. Start coop with tracing in a tmux session

Use the tmux skill to run coop in the background. It must be the `start` command (daemon mode, not TUI) so it doesn't need terminal input:

```bash
# In a tmux pane:
COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start
```

Or use the justfile:

```bash
COOP_FEATURES=signal just trace-gateway
```

Wait ~10 seconds for startup (presage key exchange with Signal servers). Verify by checking traces:

```bash
grep '"coop starting"' traces.jsonl | tail -1
grep '"signal channel configured"' traces.jsonl | tail -1
```

**Expected warnings on first startup after linking:** presage may log `"trusting new identity"` warnings and `"failed to set account attributes"` ERROR from the Signal server (HTTP 422). These are normal for a newly linked secondary device — presage reconnects automatically and messaging works despite these errors. They should not recur on subsequent startups.

If you see `"failed to initialize signal channel"` in the traces, the Signal DB is missing or the account is not linked. See "Linking coop as a secondary device" above.

### 2. Identify the accounts

```bash
# Remote signal-cli's phone number (the test sender — Alice)
$SIGNAL_CLI -o json listAccounts | jq -r '.[0].number'

# Local signal-cli's phone number (coop's primary device — Bob)
# This is the TARGET that Alice sends messages to
signal-cli -o json listAccounts | jq -r '.[0].number'

# Coop's ACI UUID — visible in the linking output or in traces
grep 'service_ids' traces.jsonl | tail -1
```

The remote sender's UUID is discovered from traces after the first message (see "Configuring the user match pattern" above).

### 3. Send a test message via signal-cli

```bash
# Send a simple text message (replace TARGET with coop's phone number or UUID)
$SIGNAL_CLI -o json send -m "Hello from the test harness" TARGET

# Send to trigger specific behavior:
$SIGNAL_CLI -o json send -m "What is 2+2?" TARGET          # triggers agent turn
$SIGNAL_CLI -o json send -m "/status" TARGET                # triggers command handling
$SIGNAL_CLI -o json send -m "/help" TARGET                  # triggers help command
```

### 4. Wait and read traces

After sending, wait 5-10 seconds for coop to receive and process:

```bash
sleep 8
```

Then read the traces to find what happened:

```bash
# Find the inbound message
grep 'signal_receive_event' traces.jsonl | tail -5

# Find the dispatch
grep 'signal inbound dispatched' traces.jsonl | tail -5

# Find the agent turn
grep 'agent_turn' traces.jsonl | tail -10

# Find the outbound reply
grep 'signal_action_send' traces.jsonl | tail -5

# Find any errors
grep '"level":"ERROR"' traces.jsonl | tail -10
grep '"level":"WARN"' traces.jsonl | tail -10

# Full trace of the last interaction (find by timestamp)
tail -100 traces.jsonl | jq -r 'select(.timestamp > "2026-02-09T05:00:00") | "\(.timestamp) [\(.level)] \(.fields.message // .fields // .)"'
```

### 5. Analyze the trace for bugs

Look for:

- **Missing spans**: Was `signal_receive_event` logged? Was `route_message` entered? Did `agent_turn` start?
- **Wrong field values**: Check `signal.sender`, `signal.target`, `signal.content_body`, session key
- **Errors**: Any `ERROR` or `WARN` entries in the processing chain
- **Timing issues**: Did typing start before the turn? Did it stop after?
- **Message ordering**: Pre-tool text flushed before tool execution?
- **Missing receipts**: Did delivery/read receipts get sent back?
- **History bootstrap**: On first message, was `signal_history` loaded?

### 6. Fix → Rebuild → Restart → Retest

```bash
# 1. Fix the code
# 2. Format and lint
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings

# 3. Rebuild
cargo build --features signal

# 4. Kill the old coop process (find it in tmux or by PID)
# In the tmux pane: Ctrl+C

# 5. Clear traces for a clean run (optional)
> traces.jsonl

# 6. Restart coop
COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start

# 7. Send the same test message again
$SIGNAL_CLI -o json send -m "What is 2+2?" TARGET

# 8. Read traces again to verify the fix
sleep 8
grep '"level":"ERROR"' traces.jsonl | tail -10
```

## Test Plan

Execute these scenarios **in order**. Each has explicit pass criteria checked via traces. Mark each ✅ PASS or ❌ FAIL as you go. On failure, stop, file a bug, fix it, then re-run the failing scenario before continuing.

After all scenarios pass, do a **clean sweep**: restart coop with empty traces, run all 9 scenarios back-to-back, and verify zero errors in the final trace file.

### Scenario 1: Startup health

**Action:** Start coop, wait 10 seconds, read traces.
**Pass criteria:**
- Trace contains `"coop starting"` with version and PID
- Trace contains `"signal channel configured"` or successful signal connect
- No `ERROR` level entries in startup sequence
- No `"signal receive setup failed"` within first 10 seconds

### Scenario 2: Basic text → agent turn → reply

**Action:**
```bash
$SIGNAL_CLI -o json send -m "What is 2+2?" TARGET
```
Wait 15 seconds (agent turn includes LLM call).

**Pass criteria:**
- `signal_receive_event` span with `signal.content_body = "data_message"`
- `signal inbound dispatched` with `signal.inbound_kind = "text"` and correct sender
- `route_message` span entered with correct session key and `trust = "Full"`
- `agent_turn` span started and completed without error
- `signal_action_send` with `signal.action = "send_text"` and non-empty content
- Delivery receipt sent: `signal_action_send` with `signal.action = "delivery_receipt"`
- Read receipt sent: `signal_action_send` with `signal.action = "read_receipt"`
- No `ERROR` or `WARN` entries for this interaction

### Scenario 3: Coop reply received by signal-cli

**Action:**
```bash
$SIGNAL_CLI -o json receive --timeout 15
```

**Pass criteria:**
- signal-cli receives a message from coop's account
- Message body is non-empty and coherent (not an error dump)

### Scenario 4: Slash command — /status

**Action:**
```bash
$SIGNAL_CLI -o json send -m "/status" TARGET
```
Wait 8 seconds.

**Pass criteria:**
- `signal inbound dispatched` with `signal.inbound_kind = "command"`
- `channel slash command handled` in traces with `command = "/status"`
- `signal_action_send` with response containing "Session:" and "Model:"
- No `agent_turn` span (commands bypass the LLM)
- Verify via `$SIGNAL_CLI -o json receive --timeout 10` that the status text arrives

### Scenario 5: Slash command — /help

**Action:**
```bash
$SIGNAL_CLI -o json send -m "/help" TARGET
```
Wait 8 seconds.

**Pass criteria:**
- `channel slash command handled` with `command = "/help"`
- Response contains "/new", "/stop", "/status"
- No `agent_turn` span

### Scenario 6: Slash command — /new (session clear)

**Action:**
```bash
$SIGNAL_CLI -o json send -m "/new" TARGET
```
Wait 5 seconds. Then send another text message and check it starts a fresh session.

**Pass criteria:**
- `channel slash command handled` with `command = "/new"`
- Response contains "Session cleared"
- Subsequent text message starts a new `agent_turn` (no prior conversation context)

### Scenario 7: Reaction handling

**Action:**
```bash
# First send a message and capture its timestamp from the send output
$SIGNAL_CLI -o json send -m "React to this message" TARGET
# Parse the timestamp from the JSON output, then:
$SIGNAL_CLI -o json sendReaction -t TARGET -e "👍" -a SENDER_NUMBER --target-timestamp TIMESTAMP
```
Wait 8 seconds.

**Pass criteria:**
- `signal_receive_event` with `signal.content_body = "data_message"` for the reaction
- `signal inbound dispatched` with `signal.inbound_kind = "reaction"`
- Reaction is either dispatched to agent or gracefully filtered (no crash, no error)

### Scenario 8: Rapid messages (turn serialization)

**Action:**
```bash
$SIGNAL_CLI -o json send -m "First message" TARGET
sleep 1
$SIGNAL_CLI -o json send -m "Second message" TARGET
sleep 1
$SIGNAL_CLI -o json send -m "Third message" TARGET
```
Wait 30 seconds (3 sequential agent turns).

**Pass criteria:**
- All 3 `signal_receive_event` entries logged
- Turns are serialized: second `agent_turn` starts after first completes
- No panics, no "previous signal turn task failed" warnings
- All 3 get replies (3 `signal_action_send` with `signal.action = "send_text"`)
- No `ERROR` entries

### Scenario 9: Empty/whitespace message

**Action:**
```bash
$SIGNAL_CLI -o json send -m " " TARGET
```
Wait 5 seconds.

**Pass criteria:**
- Message either filtered before dispatch or handled gracefully
- No crash, no `ERROR` entries
- No `agent_turn` started (whitespace-only should not trigger LLM)
  OR agent turn completes normally with empty/no reply

---

### The following scenarios exercise multi-turn tool use. They take longer because the agent makes LLM calls, invokes tools, and loops. Allow 30-60 seconds per scenario.

---

### Scenario 10: File system — read a file

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Read the file SOUL.md from your workspace and tell me what it says" TARGET
```
Wait 30 seconds.

**Pass criteria:**
- `agent_turn` span started
- `tool_execute` or `tool_start` trace with tool name `read_file`
- Tool result contains actual file content (not an error)
- `signal_action_send` with reply containing content from SOUL.md
- No `ERROR` entries

### Scenario 11: File system — write and verify

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Create a file called e2e-test.txt in your workspace with the content 'signal e2e test passed'. Then read it back to confirm." TARGET
```
Wait 30 seconds.

**Pass criteria:**
- `tool_execute` trace with tool name `write_file`
- `tool_execute` trace with tool name `read_file` (verification read)
- Both tool results successful (not errors)
- `signal_action_send` with reply confirming the file was written
- File actually exists: verify via `ls workspaces/default/e2e-test.txt`
- Clean up after: `rm workspaces/default/e2e-test.txt`

### Scenario 12: Bash tool — run a command

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Run 'uname -a' and tell me what OS you're on" TARGET
```
Wait 30 seconds.

**Pass criteria:**
- `tool_execute` trace with tool name `bash`
- Tool input contains `uname` command
- Tool result contains OS info (not an error, not a trust rejection)
- `signal_action_send` with reply describing the OS
- No `ERROR` entries

### Scenario 13: Bash tool — clone a repo

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Clone https://github.com/octocat/Hello-World into /tmp/e2e-hello-world using git, list the files, then remove it" TARGET
```
Wait 60 seconds (git clone over network).

**Pass criteria:**
- Multiple `tool_execute` traces with tool name `bash`
- First bash call contains `git clone`
- Second bash call contains `ls` (listing files)
- Third bash call contains `rm` (cleanup)
- All tool results successful
- `signal_action_send` with reply listing the repo contents
- Directory does NOT exist after cleanup: `! test -d /tmp/e2e-hello-world`
- No `ERROR` entries

### Scenario 14: Memory — write and search

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Remember this: Alice's favorite color is blue. Save it to memory." TARGET
```
Wait 20 seconds. Then:
```bash
$SIGNAL_CLI -o json send -m "What is Alice's favorite color? Search your memory." TARGET
```
Wait 20 seconds.

**Pass criteria:**
- First message: `tool_execute` with tool name `memory_write`, result shows `"outcome":"added"` or `"outcome":"updated"`
- Second message: `tool_execute` with tool name `memory_search`, result contains the observation about Alice
- `signal_action_send` for second message mentions "blue"
- No `ERROR` entries
- No trust rejection (sender must be full trust in config)

### Scenario 15: Memory — people search

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Search your memory for any people you know about" TARGET
```
Wait 20 seconds.

**Pass criteria:**
- `tool_execute` with tool name `memory_people`
- Tool result is valid JSON (even if empty — `"count":0` is fine)
- Reply is coherent (lists people or says none found)
- No `ERROR` entries

### Scenario 16: Config — read config

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Read your config file and tell me what model you're using" TARGET
```
Wait 20 seconds.

**Pass criteria:**
- `tool_execute` with tool name `config_read`
- Tool result contains the YAML config content
- Reply mentions the model name from coop.yaml
- No `ERROR` entries

### Scenario 17: Multi-tool chain — investigate the workspace

**Action:**
```bash
$SIGNAL_CLI -o json send -m "List all files in your workspace, read each .md file, and give me a brief summary of what each one contains" TARGET
```
Wait 60 seconds (multiple tool calls in a loop).

**Pass criteria:**
- `tool_execute` with `bash` (for `ls`) or multiple `read_file` calls
- Multiple `read_file` tool executions (one per .md file)
- Agent completes the turn without hitting the tool loop limit
- `signal_action_send` with a coherent summary
- No `ERROR` entries
- No "tool loop limit" or "max iterations" warnings

### Scenario 18: Signal history tool

**Action:**
```bash
$SIGNAL_CLI -o json send -m "Use the signal_history tool to look at our recent conversation" TARGET
```
Wait 20 seconds.

**Pass criteria:**
- `tool_execute` with tool name `signal_history`
- Tool result returns message history (not "only available in Signal chat sessions" error)
- Reply references actual recent messages from this test session
- No `ERROR` entries

### Scenario 19: Compaction — long conversation that triggers context summarization

Compaction triggers when total tokens for a session exceed 100,000 (`COMPACTION_THRESHOLD` in `compaction.rs`). This requires many back-and-forth messages with tool use to accumulate tokens. Each tool-heavy turn uses ~5-15k tokens, so ~10-15 turns should cross the threshold.

**Important:** Do NOT send `/new` before this scenario — it depends on the accumulated conversation history from earlier scenarios (especially 10-18 which included tool use). If the session was cleared, the token count resets to zero. If previous scenarios used `/new`, send a few warm-up tool messages first.

**Action:** Send a series of messages that each trigger tool use to build up tokens rapidly. Use bash commands that produce verbose output, file reads of large files, and memory operations:

```bash
# Each message triggers tool use, accumulating tokens in the session.
# Space them ~20s apart to let each turn complete.

$SIGNAL_CLI -o json send -m "Read your SOUL.md file and summarize each section in detail" TARGET
sleep 20

$SIGNAL_CLI -o json send -m "List all files in your workspace recursively and describe each one" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Run 'cat /etc/os-release && free -h && df -h && ps aux | head -30' and explain the output" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Search your memory for all observations, then write a new observation summarizing what we've done in this conversation so far" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Read your config file, explain every section, and suggest improvements" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Run 'find /root/coop/signal-loop/crates -name \"*.rs\" | head -40' and describe the project structure" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Read the files crates/coop-core/src/types.rs and crates/coop-core/src/traits.rs and explain the core type system" TARGET
sleep 30

$SIGNAL_CLI -o json send -m "What have we discussed so far? Give me a detailed recap of our entire conversation" TARGET
sleep 25

# If compaction hasn't triggered yet (check traces), keep going:
$SIGNAL_CLI -o json send -m "Read crates/coop-gateway/src/compaction.rs and explain how compaction works" TARGET
sleep 25

$SIGNAL_CLI -o json send -m "Run 'cargo test -p coop-core --list 2>&1 | head -40' and describe the test suite" TARGET
sleep 25
```

After each batch, check if compaction triggered:

```bash
grep 'session compacted\|compaction' traces.jsonl | tail -5
```

**Pass criteria:**
- After enough messages, traces contain `"session compacted"` with `tokens_before` and `summary_len` fields
- The compaction summary is non-empty (`summary_len > 0`)
- No `"compaction failed"` warnings
- After compaction, send one more message and verify the agent still responds coherently:
  ```bash
  $SIGNAL_CLI -o json send -m "What's 2+2? Also, do you remember what files we looked at earlier?" TARGET
  ```
- The post-compaction reply should work normally (agent_turn completes, reply sent)
- Trace shows `build_provider_context` using the compacted summary (the input_tokens should be much lower than before compaction)
- No `ERROR` entries throughout

**Failure modes to watch for:**
- Compaction API call fails (check for `"compaction failed"` warning)
- Orphaned tool_use blocks cause 400 errors during compaction summarization (stripped by `prepare_compaction_messages`, but verify)
- Post-compaction context is broken (agent can't respond, gets confused)
- Token count doesn't reset properly after compaction
- Compaction state not persisted to disk (check `workspaces/default/sessions/` for `*_compaction.json`)

### Clean Sweep

After all 19 pass individually:

1. Stop coop
2. `> traces.jsonl`
3. `rm -f workspaces/default/e2e-test.txt` (clean up test artifacts)
4. Restart coop
5. Run a representative subset back-to-back — scenarios 2, 4, 7, 8, 12, 14, 17 (skip 19 — compaction takes too long for a sweep)
6. Wait 90 seconds
7. `grep '"level":"ERROR"' traces.jsonl` — must be empty
8. `grep '"level":"WARN"' traces.jsonl` — review each, ensure none are bugs

## Trace Analysis Recipes

### Full interaction timeline
```bash
# Get all events from the last 60 seconds
tail -200 traces.jsonl | jq -r '
  select(.timestamp > (now - 60 | todate)) |
  "\(.timestamp) [\(.level)] \(.span.name // "-") :: \(.fields.message // (.fields | keys | join(",")))"
'
```

### Signal-specific events only
```bash
grep -E '"signal\.' traces.jsonl | tail -30 | jq -r '
  "\(.timestamp) \(.fields.message) sender=\(.fields."signal.sender" // "-") target=\(.fields."signal.target" // "-")"
'
```

### Provider request/response
```bash
grep 'provider_request\|anthropic_request' traces.jsonl | tail -10 | jq .
```

### Tool execution chain
```bash
grep 'tool_execute\|signal_tool' traces.jsonl | tail -10 | jq -r '
  "\(.timestamp) \(.fields.message) tool=\(.fields."tool.name" // "-")"
'
```

### Session state
```bash
grep 'route_message\|session' traces.jsonl | tail -20 | jq -r '
  "\(.timestamp) \(.fields.message // "-") session=\(.span.session // "-") trust=\(.span.trust // "-")"
'
```

## Common Bugs to Look For

1. **presage reconnect storms**: Signal websocket drops and reconnects rapidly. Look for repeated `signal receive setup failed` warnings followed by short backoff sleeps.

2. **Typing indicator stuck on**: If the agent turn errors, does typing stop? Check for unmatched `typing started=true` without a corresponding `started=false`.

3. **Receipt handling**: Are delivery and read receipts sent back? Look for `signal_action_send` with `signal.action = "delivery_receipt"` and `"read_receipt"`.

4. **History bootstrap race**: On first message to a new session, does history get loaded before the turn starts? Check ordering of `seed_signal_history` vs `agent_turn`.

5. **Group message routing**: Group messages need `group_v2` context in outbound. Check `signal_action_send` for group targets — is `group_context_for_target` returning the right master key?

6. **Edit messages**: Signal edit messages replace the original. Does coop parse them correctly as `InboundKind::Edit`?

7. **SynchronizeMessage**: Messages sent from the primary device appear as sync messages. Does coop handle or filter them correctly?

8. **Tool ordering**: When the agent calls `signal_reply` tool, does the pre-tool text get flushed before the tool reply arrives? Check outbound message ordering.

## Working with tmux

Use the tmux skill to manage coop in the background:

```bash
# Create a session for coop
tmux new-session -d -s coop

# Start coop in that session
tmux send-keys -t coop 'cd /root/coop/signal-loop && COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start' Enter

# Check if it's running
tmux capture-pane -t coop -p | tail -5

# Kill and restart
tmux send-keys -t coop C-c
sleep 1
tmux send-keys -t coop 'COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start' Enter
```

## Bug Log

Every bug found and fix applied during the e2e loop **must** be logged in `docs/bugs/`. This builds institutional knowledge about Signal integration issues and prevents regressions.

### Existing convention

Bug files are numbered sequentially: `docs/bugs/001-*.md`, `002-*.md`, etc. Check the current highest number before creating a new one:

```bash
ls docs/bugs/ | sort -n | tail -1
```

### When to log

Log a bug **as soon as you identify it in the traces**, before you start fixing. Update the file with the fix details after.

### Template

Create `docs/bugs/NNN-short-description.md`:

```markdown
# BUG-NNN: Short description of the problem

**Status:** Fixed | Open | Workaround
**Found:** YYYY-MM-DD
**Scenario:** What signal-cli command triggered it

## Symptom

What the user would see (or not see). What went wrong from the outside.

## Trace Evidence

Paste the relevant trace lines that revealed the bug. Include timestamps,
span names, field values — enough that someone can understand the failure
without re-running it.

```
<paste key trace lines here>
```

## Root Cause

Why it happened. Which code path, what assumption was wrong.

## Fix

What changed and why. Reference the specific files/functions modified.

## Test Coverage

What unit test was added to prevent regression. Reference the test name
and crate.
```

### Rules

- **Log every bug**, even small ones. A one-line parse error is still worth recording.
- **Include raw trace evidence.** The trace lines are the proof. Without them, the bug report is just a story.
- **Update status.** Mark `Fixed` once the fix is verified in traces, not just when the code compiles.
- **Don't skip the test.** If you can't write a unit test for it (e.g., pure protocol timing), document why.

## Iteration Checklist

For each bug found:

- [ ] Create `docs/bugs/NNN-short-description.md` with symptom and trace evidence
- [ ] Identify the root cause in the code
- [ ] Write a unit test that reproduces it (use MockSignalChannel + test providers)
- [ ] Fix the code
- [ ] `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test -p <affected-crate>`
- [ ] Rebuild and restart coop with tracing
- [ ] Re-send the same Signal message
- [ ] Verify the fix in traces (correct spans, correct field values, no errors)
- [ ] Verify the user-visible outcome (message received correctly via `$SIGNAL_CLI -o json receive`)
- [ ] Update the bug file: add root cause, fix details, test coverage, mark status `Fixed`

## Session Log

In addition to individual bug files, maintain a running session log at `docs/bugs/SESSION-LOG.md` that tracks each e2e debugging session. This gives a quick chronological overview without reading every bug file.

Append to it each session:

```markdown
## YYYY-MM-DD Session

**Duration:** ~45 minutes
**Bugs found:** 2
**Bugs fixed:** 2 (0 open)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| 1 | Startup health | ✅ PASS |
| 2 | Basic text → reply | ✅ PASS (after BUG-004 fix) |
| 3 | Coop reply received | ✅ PASS |
| 4 | /status | ✅ PASS |
| 5 | /help | ✅ PASS |
| 6 | /new (session clear) | ✅ PASS |
| 7 | Reaction | ✅ PASS (after BUG-005 fix) |
| 8 | Rapid messages | ✅ PASS |
| 9 | Empty/whitespace | ✅ PASS |
| 10 | Read file | ✅ PASS |
| 11 | Write + verify file | ✅ PASS |
| 12 | Bash command | ✅ PASS |
| 13 | Clone repo | ✅ PASS |
| 14 | Memory write + search | ✅ PASS |
| 15 | Memory people | ✅ PASS |
| 16 | Config read | ✅ PASS |
| 17 | Multi-tool workspace scan | ✅ PASS |
| 18 | Signal history | ✅ PASS |
| 19 | Compaction | ✅ PASS — triggered at ~105k tokens, summary 2.3k chars |
| — | Clean sweep | ✅ PASS — 0 errors, 2 warnings (both benign) |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-004 | Fixed | Typing indicator not stopped on provider timeout |
| BUG-005 | Fixed | Edit message parsed as text instead of InboundKind::Edit |

### Notes

presage websocket stable throughout. Receipt handling looks correct.
Group routing untested (no shared group between accounts yet).
```

This file is append-only. Never delete previous sessions.

## Notes

- **Never commit real phone numbers, UUIDs, or hostnames.** Use the PII rules from AGENTS.md. In this doc, `TARGET`, `SENDER_NUMBER`, and `SIGNAL_HOST` are placeholders. The actual values are discovered during setup.
- **Three Signal identities are involved.** Remote signal-cli (Alice's account, the test sender), local signal-cli (Bob's account, coop's primary device), and coop/presage (Bob's account, secondary device). Alice sends messages to Bob's number; coop receives them as Bob's secondary device.
- **Remote signal-cli is the test sender.** Installed via Homebrew on `signal-host` at `/opt/homebrew/bin/signal-cli`. All `$SIGNAL_CLI` commands in this doc route through SSH to this host: `ssh signal-host /opt/homebrew/bin/signal-cli`.
- **Local signal-cli is only used for linking.** At `/usr/local/bin/signal-cli`, it's the primary device for coop's account. The only command you run on it is `signal-cli addDevice --uri <provisioning_url>` to link coop. After that, it sits idle.
- **SSH config is pre-configured.** The `~/.ssh/config` has a `signal-host` alias with `ControlMaster`/`ControlPersist` for connection reuse. Ensure `mkdir -p ~/.ssh/sockets` exists. All SSH calls should use `-o BatchMode=yes` to avoid interactive prompts.
- **First startup after linking has expected errors.** presage logs `"failed to set account attributes"` (HTTP 422) and `"trusting new identity"` on the first connection. These are normal and do not prevent message delivery. They go away on subsequent starts.
- **Coop uses presage, not signal-cli.** The Signal channel in coop (`crates/coop-channels/src/signal.rs`) uses the presage Rust library directly. signal-cli is only used here as an external test driver.
- **signal-cli's `-o json` flag** gives structured output — use it for all commands so you can parse results.
- **Traces rotate** at 50MB or UTC midnight. For long debugging sessions, use `just trace-list` to find archives.
