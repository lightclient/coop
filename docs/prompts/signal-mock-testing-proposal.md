# Signal Mock Testing Proposal

## Goal

Build a deterministic, no-network test harness for Signal flows so we can validate message parsing/routing, tool calls, outbound behavior, and typing lifecycle entirely in tests.

This proposal focuses on:

1. Inbound message handling across all relevant message kinds
2. Gateway filtering/dispatch behavior
3. Signal tool call behavior (`signal_react`, `signal_reply`)
4. Outbound send behavior and payload integrity
5. Typing notifier lifecycle behavior

---

## 1) Add a `MockSignalChannel` testkit in `coop-channels`

### New module

- `crates/coop-channels/src/signal/testkit.rs` (test-only and/or feature-gated)

### Core design

Implement a mock channel that conforms to `coop_core::Channel` and exposes test controls:

- `inject_inbound(InboundMessage)`
- `take_outbound() -> Vec<OutboundMessage>`
- `take_actions() -> Vec<SignalAction>` (or async receiver access)
- `set_health(ChannelHealth)`

### Why

This gives us one reusable Signal test surface for both:

- `coop-channels` unit/integration tests
- `coop-gateway` routing/loop tests

without requiring real Signal runtime, DB state, or external process coordination.

---

## 2) Extract a single-message handler from `run_signal_loop`

Current `run_signal_loop` is an infinite loop. Extract a focused helper to process one message at a time:

- `handle_signal_inbound_once(...) -> Result<()>`

Then:

- `run_signal_loop` remains production loop and calls the helper
- tests call helper directly for deterministic assertions

### Why

Avoids timeout-driven tests and makes behavior testable without introducing protocol behavior changes.

---

## 3) Add a scripted provider for tool-loop tests

For end-to-end tool flow tests, introduce a test-only scripted provider (local to `coop-gateway/tests` or shared fake) that can emit:

- assistant tool requests (`signal_reply`, `signal_react`)
- follow-up assistant text

### Assertions enabled

- `ToolStart` and `ToolResult` events happen
- expected `SignalAction` is queued with correct payload fields
- final outbound response behavior remains correct

---

## 4) Test matrix

### A) `coop-channels` tests

#### Inbound parse matrix

- `Text`
- `Reaction`
- `Edit`
- `Attachment`
- `Typing`
- `Receipt`
- `SynchronizeMessage` variants

#### Tool executor matrix

- `signal_react`: direct target + group target
- `signal_reply`: direct target + group target
- invalid `chat_id` handling
- closed action channel handling

### B) `coop-gateway` tests (with mock Signal channel)

#### Filter behavior

- `Typing` and `Receipt` are filtered
- `Text`/`Reaction`/`Edit`/`Attachment` are dispatched

#### Reply target behavior

- `reply_to` takes precedence
- group fallback uses `group:...`
- DM fallback uses sender identity

#### Outbound behavior

- non-empty assistant response sends outbound message
- empty assistant response does not send outbound message

#### Tool-call flow

- scripted provider emits `signal_reply` request
- tool executes and queues `SignalAction::Reply` with correct fields

#### Typing lifecycle

- typing starts at turn begin
- typing stops via guard/drop path

Use placeholder/fake identities only (e.g. `alice-uuid`, `bob-uuid`, `group:...`).

---

## 5) Rollout plan (small PR sequence)

1. **PR 1:** Add `signal::testkit` mock channel + harness
2. **PR 2:** Refactor `run_signal_loop` into testable single-message helper
3. **PR 3:** Add gateway integration tests for message filtering/dispatch/outbound
4. **PR 4:** Add scripted-provider tool-call integration tests
5. **PR 5 (optional):** Add JSONL trace assertions for `signal_*` spans/events

---

## Acceptance criteria

- CI tests require no live Signal dependency
- Deterministic coverage of key inbound kinds and tool flows
- Existing tests continue to pass
- New tests assert payload correctness (targets, timestamps, author IDs, raw content fields)
- Runtime production behavior unchanged except refactoring for testability

---

## Notes

- Keep compile-time impact low (avoid adding heavy dependencies)
- Prefer reusing `coop-core` fake patterns where possible
- Keep test APIs narrowly scoped to avoid over-abstraction
