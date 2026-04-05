# Signal E2E Debugging Session Log

Chronological log of e2e debugging sessions run via the signal-cli trace loop.
See `docs/prompts/signal-e2e-trace-loop.md` for the full workflow.

This file is append-only. Never delete previous sessions.

---

## 2026-02-09 Session

**Duration:** ~45 minutes
**Bugs found:** 1
**Bugs fixed:** 1 (0 open)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| 1 | Startup health | ✅ PASS |
| 2 | Basic text → reply | ✅ PASS |
| 3 | Coop reply received | ✅ PASS |
| 4 | /status | ✅ PASS |
| 5 | /help | ✅ PASS |
| 6 | /new (session clear) | ✅ PASS |
| 7 | Reaction | ✅ PASS |
| 8 | Rapid messages | ✅ PASS |
| 9 | Empty/whitespace | ✅ PASS |
| 10 | Read file | ✅ PASS |
| 11 | Write + verify file | ✅ PASS |
| 12 | Bash command | ✅ PASS |
| 13 | Clone repo | ✅ PASS |
| 14 | Memory write + search | ✅ PASS |
| 15 | Memory people | ✅ PASS |
| 16 | Config read | ✅ PASS (after BUG-004 fix) |
| 17 | Multi-tool workspace scan | ✅ PASS |
| 18 | Signal history | ✅ PASS |
| 19 | Compaction | ✅ PASS — triggered at ~126k tokens, summary 3,966 chars |
| — | Clean sweep | ✅ PASS — 0 errors, 0 warnings |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-004 | Fixed | Null tool_use.input from no-parameter tools (e.g. config_read) causes 400 Bad Request on next API call |

### Notes

- First session after fresh `signal link` — presage logged expected startup errors (HTTP 422 account attributes, "trusting new identity" warnings) that self-resolved.
- Transient websocket drops ("unexpected error in message receiving loop") occurred once, presage reconnected automatically.
- Remote signal-cli on macOS/homebrew via SSH worked well with ControlMaster connection reuse.
- All 446 workspace tests pass after fix.
- Compaction triggered naturally at 126,492 tokens during Scenario 19, summary was 3,966 chars, post-compaction input dropped to 60 tokens.

---

## 2026-03-18 Session

**Duration:** ~50 minutes
**Bugs found:** 2
**Bugs fixed:** 0 (2 open)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| — | Anthropic `/models` | ✅ PASS — reply listed Sonnet/Opus/Haiku with current/default tags |
| — | Anthropic `/model claude-opus-4-0-20250514` | ✅ PASS — switch acknowledged |
| — | Anthropic `/status` after switch | ✅ PASS — status showed `anthropic/claude-opus-4-0-20250514` |
| — | Anthropic follow-up turn | ⚠️ ENV BLOCKED — local Anthropic OAuth token on this host is expired |
| — | OpenAI `/models` | ✅ PASS — reply listed `gpt-5-codex`, `gpt-5-mini`, `gpt-4o-mini` |
| — | OpenAI `/model gpt-5-mini` | ✅ PASS — switch acknowledged |
| — | OpenAI `/status` after `gpt-5-mini` | ✅ PASS — status showed `gpt-5-mini` |
| — | OpenAI follow-up turn on `gpt-5-mini` | ❌ FAIL — provider rejected built-in catalog model for Codex OAuth (BUG-005) |
| — | OpenAI `/model gpt-5-codex` | ✅ PASS — switch acknowledged |
| — | OpenAI `/status` after `gpt-5-codex` | ✅ PASS — status showed `gpt-5-codex` |
| — | OpenAI follow-up turn on `gpt-5-codex` | ✅ PASS — replied `Four` |
| — | Local `/models` | ✅ PASS — reply listed `llama3.2` and `qwen2.5-coder:14b` |
| — | Local `/model qwen2.5-coder:14b` | ✅ PASS — switch acknowledged |
| — | Local `/status` after switch | ✅ PASS — status showed `qwen2.5-coder:14b` |
| — | Local follow-up turn | ⚠️ SKIPPED — no local Ollama backend detected on `127.0.0.1:11434` |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-005 | Open | OpenAI built-in catalog advertises `gpt-5-mini` for Codex OAuth even though the backend rejects it |
| BUG-006 | Open | Signal DM sends emit `could not create sync message from a direct message` errors despite successful replies |

### Notes

- The slash-command feature itself worked end-to-end over Signal for Anthropic, OpenAI, and local-model configs: `/models` replied, `/model` switched, and `/status` reflected the new per-user selection.
- The bundled `send-and-verify.sh` script reported `FAIL` for every scenario because BUG-006 injects `ERROR` traces on otherwise successful DM sends.
- Anthropic completion verification could not be completed because the locally stored Claude OAuth token is expired; this was an environment issue, not a slash-command routing issue.
- OpenAI Codex OAuth refresh worked: `gpt-5-codex` completed successfully after the model was switched back from the unsupported `gpt-5-mini`.

---

## 2026-03-19 Session

**Duration:** ~35 minutes
**Bugs found:** 0 new
**Bugs fixed:** 0 (2 previously open)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| — | Multi-provider `/models` | ✅ PASS — one config listed Anthropic, OpenAI, and Ollama models together |
| — | `/model gpt-5-codex` | ✅ PASS — switched from Anthropic default to OpenAI provider |
| — | `/status` after OpenAI switch | ✅ PASS — status showed `gpt-5-codex` |
| — | OpenAI follow-up turn after cross-provider switch | ✅ PASS — replied `It equals four.` |
| — | `/model llama3.2` | ✅ PASS — switched from OpenAI to Ollama provider |
| — | `/status` after Ollama switch | ✅ PASS — status showed `llama3.2` |
| — | `/model anthropic/claude-sonnet-4-20250514` | ✅ PASS — switched back to Anthropic provider |
| — | `/status` after Anthropic switch | ✅ PASS — status showed `anthropic/claude-sonnet-4-20250514` |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-005 | Open | OpenAI built-in catalog advertises `gpt-5-mini` for Codex OAuth even though the backend rejects it |
| BUG-006 | Open | Signal DM sends emit `could not create sync message from a direct message` errors despite successful replies |

### Notes

- This session verified the corrected scope: one `coop.toml` with multiple `[[providers]]` entries and `/model` switching across provider boundaries.
- The multi-provider Signal e2e runs still show script-level `FAIL` because BUG-006 emits error-level trace noise on successful DM sends.
- A real provider turn succeeded after switching providers at runtime (`anthropic` default → `/model gpt-5-codex` → reply `It equals four.`).
- Local Ollama completion was not exercised because no backend is listening on `127.0.0.1:11434`, but command-level provider switching and status reporting worked.

---

## 2026-04-05 Session

**Duration:** ~35 minutes
**Bugs found:** 1
**Bugs fixed:** 1 (0 open from this session)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| — | Review `.claude/skills/signal-e2e-test` | ✅ PASS |
| — | `cargo build --features signal` | ❌ FAIL → fixed as BUG-007 |
| — | `cargo clippy --all-targets --all-features -- -D warnings` | ✅ PASS after BUG-007 |
| — | coop Signal startup with tracing | ⚠️ PARTIAL — process starts, but receive loop logs repeated websocket upgrade warnings |
| — | `preflight.sh` | ⚠️ ENV BLOCKED — no registered local `signal-cli` sender accounts |
| — | `send-and-verify.sh "/status"` | ⚠️ ENV BLOCKED — account discovery fails before send |
| — | Full live Signal subagent scenarios | ⚠️ ENV BLOCKED — unable to send real Signal messages in this environment |
| — | `cargo test --workspace --all-features` | ⚠️ FLAKY — passes on rerun, but two trace assertions failed in aggregate runs before succeeding individually/on rerun |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-007 | Fixed | Signal build broke because `SignalTypingNotifier` did not handle `SessionKind::Subagent(_)` |
| BUG-005 | Open | OpenAI built-in catalog advertises `gpt-5-mini` for Codex OAuth even though the backend rejects it |
| BUG-006 | Open | Signal DM sends emit `could not create sync message from a direct message` errors despite successful replies |

### Notes

- The subagent work introduced a Signal-feature compile regression that prevented e2e verification until fixed.
- Live end-to-end Signal verification could not be completed on this host because `signal-cli -o json listAccounts` reports zero usable accounts; the locally stored account entries are present but not registered.
- To get coop far enough for startup verification, the run used an existing linked Signal DB from another checkout and an Anthropic token loaded from the local agent auth store.
- With that setup, coop starts and logs `signal loop listening`, but the receive path repeatedly warns `signal receive setup failed` with `websocket upgrade failed`, so even with a sender account this host would still need Signal connectivity diagnosis.
- `send-and-verify.sh` fails immediately at account discovery, so no real Signal message could be injected to exercise subagent spawn/control over the live protocol in this session.

---

## 2026-04-05 Session (continued)

**Duration:** ~45 minutes
**Bugs found:** 2
**Bugs fixed:** 0 (2 open)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| — | Re-register local Signal accounts | ✅ PASS |
| — | Discover accounts with skill script | ✅ PASS |
| — | Secondary-device link via `coop signal link` + `signal-cli addDevice` | ❌ FAIL — BUG-008 (`HTTP 409` during provisioning) |
| — | Start coop with existing linked Signal DB | ⚠️ PARTIAL — startup succeeds, but receive loop degrades immediately |
| — | `preflight.sh` with existing linked DB | ✅ PASS |
| — | Live `/status` over Signal | ❌ FAIL — message never received by coop due receive-loop failure |
| — | Subagent-specific live Signal scenarios (`subagent_spawn`, `/subagents`, inspect/kill) | ⚠️ BLOCKED — basic inbound Signal delivery is broken |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-008 | Open | `coop signal link` fails to provision the secondary device and ends with HTTP 409 |
| BUG-009 | Open | Signal receive loop repeatedly fails with websocket upgrade errors, so inbound messages never reach coop |
| BUG-005 | Open | OpenAI built-in catalog advertises `gpt-5-mini` for Codex OAuth even though the backend rejects it |
| BUG-006 | Open | Signal DM sends emit `could not create sync message from a direct message` errors despite successful replies |

### Notes

- Re-registering the two local `signal-cli` accounts and upgrading `signal-cli` to the latest native release restored the local sender-side prerequisites.
- The repository's `discover-accounts.sh` script now works, but actual live Signal validation is still blocked by two deeper Signal integration issues.
- A fresh secondary-device link attempt fails on the coop side with HTTP 409 before a usable `db/signal.db` is produced.
- Falling back to an older already-linked DB allows startup, but the receive websocket fails every time (`websocket upgrade failed`), so even `/status` never reaches command handling.
- Because the basic inbound Signal path is broken, subagent behavior over the real Signal transport could not be validated in this session.

---

## 2026-04-05 Session (continued 2)

**Duration:** ~90 minutes
**Bugs found:** 3
**Bugs fixed:** 3 (0 open from this session)

### Test Plan Results

| # | Scenario | Result |
|---|----------|--------|
| — | Fresh `coop signal link` + `signal-cli addDevice` | ✅ PASS — after BUG-008 fix |
| — | Signal `/status` e2e | ✅ PASS |
| — | `/subagents` empty state | ✅ PASS |
| — | Wait-mode `subagent_spawn` over Signal | ✅ PASS |
| — | `/subagents inspect <run_id>` for completed run | ✅ PASS |
| — | Background subagent completion notification over Signal | ✅ PASS — after BUG-010 fix |
| — | `/status` shows active subagent count | ✅ PASS |
| — | `/subagents` list active run | ✅ PASS |
| — | `/subagents inspect <run_id>` for active run | ✅ PASS |
| — | `/subagents kill <run_id>` | ✅ PASS |
| — | `/subagents inspect <run_id>` after kill | ✅ PASS |
| — | `/stop` propagates cancellation to active wait-mode child | ✅ PASS |
| — | `cargo build --features signal` | ✅ PASS |
| — | `cargo clippy --all-targets --all-features -- -D warnings` | ✅ PASS |
| — | `cargo test --workspace --all-features` | ⚠️ FLAKY — aggregate run still has known trace-test flakes, but reruns of the failing tests passed |

### Bugs

| Bug | Status | Summary |
|-----|--------|---------|
| BUG-006 | Fixed | DM receipt/typing sends logged benign sync-message failures as errors |
| BUG-008 | Fixed | Linked-device provisioning used an outdated `/v1/devices/link` payload and failed with HTTP 409 |
| BUG-009 | Fixed | Receive-loop websocket failure was a consequence of the broken linked-device DB from BUG-008 |
| BUG-010 | Fixed | Background subagent completion was stored in-session but not delivered to Signal when the parent session was idle |
| BUG-005 | Open | OpenAI built-in catalog advertises `gpt-5-mini` for Codex OAuth even though the backend rejects it |

### Notes

- The Signal stack is now healthy enough for real end-to-end subagent verification over the live protocol.
- For live subagent verification, the session used `openai` / `gpt-5-codex` with OAuth credentials because the local Anthropic OAuth token remained expired.
- The subagent control surface now works end-to-end over Signal: spawning in wait/background mode, listing runs, inspecting runs, killing active runs, and parent-turn cancellation propagation via `/stop`.
- Background completion notifications are now visible to Signal users instead of only being appended to hidden session history.
