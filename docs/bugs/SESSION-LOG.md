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
