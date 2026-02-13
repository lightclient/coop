---
name: signal-e2e-test
description: "End-to-end Signal integration testing for coop. Use after implementing or fixing any Signal-related feature, provider interaction, or tool behavior to verify it works through the real Signal protocol and LLM API — not just unit tests. Sends real Signal messages, reads traces, diagnoses failures, and iterates until the feature works."
---

# Signal E2E Test Skill

Verify coop features work end-to-end over real Signal messaging. Catches bugs that unit tests miss: protocol edge cases, timing issues, presage quirks, message ordering, LLM API failures, tool execution through the full stack.

**When to use:** After implementing or fixing anything that touches Signal message handling, agent turns, tool execution, provider calls, prompt building, compaction, or slash commands.

## Architecture

Two Signal accounts are registered to signal-cli on this VM. Coop is linked as a presage secondary device to one. The other sends test messages simulating a human user.

```
  signal-cli (Alice)          signal-cli (Bob — primary)
  Test sender                  ↕ shared identity
      │                       coop/presage (Bob — secondary)
      │  Signal Protocol       COOP_TRACE_FILE=traces.jsonl
      └─────────────────────▶ │
                               ▼
                          traces.jsonl ← you read this
```

The scripts auto-detect which account is coop (multi-device) and which is the sender.

## Scripts

All scripts are in `scripts/` relative to this skill:

```
SKILL_DIR=/root/coop/main/.claude/skills/signal-e2e-test
```

### discover-accounts.sh

Auto-detects the coop account and test sender from local signal-cli. Probes that the sender can actually deliver.

```bash
bash $SKILL_DIR/scripts/discover-accounts.sh
# Outputs: COOP_NUMBER, SENDER_CMD, SENDER_NUMBER, SENDER_UUID
```

### preflight.sh

Checks coop is running, signal channel is active, traces are flowing.

```bash
bash $SKILL_DIR/scripts/preflight.sh traces.jsonl
```

### send-and-verify.sh

Sends a message, polls traces for completion, reports PASS/FAIL. Automatically patches `coop.toml` with the sender's UUID for the test and restores the placeholder on exit.

```bash
bash $SKILL_DIR/scripts/send-and-verify.sh "What is 2+2?"
bash $SKILL_DIR/scripts/send-and-verify.sh "/status"
bash $SKILL_DIR/scripts/send-and-verify.sh "Read SOUL.md and summarize it" --timeout 60
bash $SKILL_DIR/scripts/send-and-verify.sh "Run 'uname -a'" --expect-tool bash --timeout 60
```

Options: `--timeout N`, `--expect-tool NAME`, `--expect-command`, `--no-reply-check`, `--trace-file FILE`, `--sender-cmd CMD`, `--target NUM`.

## Verification Workflow

### 1. Build

```bash
cargo fmt
cargo build --features signal
```

### 2. Start or restart coop in tmux

```bash
# Kill existing, clear traces, restart
tmux send-keys -t coop C-c 2>/dev/null; sleep 2
> traces.jsonl
tmux send-keys -t coop 'COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start' Enter

# Or create fresh session
tmux new-session -d -s coop
tmux send-keys -t coop 'cd /root/coop/main && COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start' Enter
```

Wait ~10s, then:

```bash
bash $SKILL_DIR/scripts/preflight.sh
```

### 3. Send and verify

```bash
bash $SKILL_DIR/scripts/send-and-verify.sh "YOUR TEST MESSAGE"
```

### 4. If FAIL — read traces, fix, repeat from step 1

The script prints the trace tail on failure. For deeper analysis:

```bash
grep '"level":"ERROR"' traces.jsonl | tail -10
grep -E 'signal_receive|signal inbound|route_message|agent_turn|signal_action' traces.jsonl | tail -20
```

## Diagnosing Failures

| Checklist item that failed | Likely cause | Fix |
|---------------------------|--------------|-----|
| Message received by coop | Presage not connected | Restart coop, check `signal.db` exists |
| Message dispatched | Parse error in signal channel | Check `inbound_from_content`, look for panics |
| Slash command handled | Command not recognized | Check command dispatch in signal loop |
| Agent turn started | Routing failure, trust mismatch | Check `coop.toml` users match sender UUID |
| Reply sent via Signal | Turn completed but reply not routed | Check signal loop reply path |
| Tool 'X' executed | Agent didn't call the expected tool | Check tool definitions, prompt, trust level |
| No errors | Something threw an error | Read the error from traces |
| Completed within timeout | Processing hung or too slow | Increase `--timeout`, check for infinite loops |

### First-time setup

**Sender not recognized (trust mismatch):** After Alice's first message, get her UUID from traces and add to `coop.toml`:

```bash
grep '"signal inbound dispatched"' traces.jsonl | tail -1
# Look for signal.sender field — that's Alice's UUID
# Add to coop.toml [[users]] match list. Coop hot-reloads.
```

## Common Test Messages

| Feature changed | Test message | Extra flags |
|----------------|-------------|-------------|
| Basic message flow | `"What is 2+2?"` | |
| Slash commands | `"/status"` | |
| Session management | `"/new"` | |
| File tools | `"Read SOUL.md and summarize it"` | `--timeout 60 --expect-tool read_file` |
| Bash tool | `"Run 'uname -a'"` | `--timeout 60 --expect-tool bash` |
| Memory | `"Remember: test value. Save to memory."` | `--expect-tool memory_write` |
| Signal history | `"Use signal_history to see recent messages"` | `--expect-tool signal_history` |

### Reactions

```bash
eval "$(bash $SKILL_DIR/scripts/discover-accounts.sh 2>/dev/null)"
RESULT=$(${SENDER_CMD} send -m "React to this" ${COOP_NUMBER})
TS=$(echo "$RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['timestamp'])")
${SENDER_CMD} sendReaction -t ${COOP_NUMBER} -e "👍" -a ${SENDER_NUMBER} --target-timestamp "$TS"
sleep 8
grep 'signal inbound dispatched' traces.jsonl | tail -3
# Pass if: inbound_kind contains "reaction", no crash
```

### Rapid messages (turn serialization)

```bash
eval "$(bash $SKILL_DIR/scripts/discover-accounts.sh 2>/dev/null)"
${SENDER_CMD} send -m "First" ${COOP_NUMBER}; sleep 1
${SENDER_CMD} send -m "Second" ${COOP_NUMBER}; sleep 1
${SENDER_CMD} send -m "Third" ${COOP_NUMBER}
sleep 30
# Pass if: 3 signal_receive_event, 3 signal_action_send, no panics
grep -c 'signal_action_send' traces.jsonl
```

## Full Test Plan

For comprehensive pre-release validation (19 scenarios including compaction), see `docs/prompts/signal-e2e-trace-loop.md`.

## Bug Logging

1. `ls docs/bugs/ | sort -n | tail -1` — find next bug number
2. Create `docs/bugs/NNN-short-description.md` with symptom + trace evidence
3. Fix → test → rebuild → restart → re-verify with `send-and-verify.sh`
4. Update bug file with root cause, fix, mark `Fixed`
5. Append to `docs/bugs/SESSION-LOG.md`

Template in `docs/prompts/signal-e2e-trace-loop.md`.
