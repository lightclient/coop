# BUG-004: Null tool_use.input causes 400 Bad Request on subsequent API calls

**Status:** Fixed
**Found:** 2026-02-09
**Scenario:** `config_read` tool (no parameters) → next turn fails with 400

## Symptom

When the model calls a tool with no parameters (e.g., `config_read`), the API returns `input: null` in the tool_use block. The tool executes successfully, but on the **next** API call, the replayed history contains `"input": null` which the Anthropic API rejects:

```
Invalid request (400 Bad Request): messages.1.content.0.tool_use.input: Input should be a valid dictionary
```

## Trace Evidence

```
2026-02-09T06:33:01.606028Z tool arguments {"message":"tool arguments","arguments":"null"}
2026-02-09T06:33:01.852649Z provider request failed, rolling back session
  error: "Invalid request (400 Bad Request): messages.1.content.0.tool_use.input: Input should be a valid dictionary"
```

## Root Cause

In `anthropic_provider.rs`, the conversion from internal `Content::ToolRequest` to API `tool_use` only handles `Value::String` specially; `Value::Null` passes through as-is. The Anthropic API requires `input` to be a JSON object (`{}`), never `null`.

Two places need fixing:
1. **Outbound (history replay):** `build_messages()` should coerce null arguments to `{}`
2. **Inbound (response parsing):** `with_tool_request()` should normalize null input when received

## Fix

Added `Value::Null => json!({})` case in the outbound conversion in `build_messages()`, and added the same normalization at the `Content::tool_request()` constructor level to catch it at the source.

## Test Coverage

Added unit test `test_null_tool_input_coerced_to_empty_object` in `coop-core` types tests.
