# BUG-005: `/models` advertises `gpt-5-mini` for Codex OAuth, but the provider rejects it

**Status:** Open
**Found:** 2026-03-18
**Scenario:** Signal e2e verification of `/models` and `/model` with `[provider] name = "openai"`

## Symptom

When Coop is configured for OpenAI using a ChatGPT/Codex OAuth credential, `/models` lists `gpt-5-mini` as an available built-in model and `/model gpt-5-mini` succeeds. But the first real agent turn after switching fails with a provider error.

From the user's perspective:

1. `/models` says `gpt-5-mini` is selectable
2. `/model gpt-5-mini` responds `Model set to gpt-5-mini ✅`
3. `/status` reports `Model: gpt-5-mini`
4. The next normal message fails instead of producing an answer

## Trace Evidence

```text
2026-03-18T13:36:59Z signal_action_send signal.raw_content="Available models:
  * gpt-5-codex — coding / responses API (current, default)
  - gpt-5-mini — smart reasoning
  - gpt-4o-mini — fast, recommended"

2026-03-18T13:37:07Z signal_action_send signal.raw_content="Model set to gpt-5-mini ✅
Context window: 128000 tokens"

2026-03-18T13:37:14Z signal_action_send signal.raw_content="Session: coop:dm:signal:[redacted-id]
Agent: coop
Model: gpt-5-mini"

2026-03-18T13:37:22Z route_message:agent_turn ERROR
  provider request failed, rolling back session
  error="OpenAI Codex API error 400: The 'gpt-5-mini' model is not supported when using Codex with a ChatGPT account."
```

## Root Cause

The built-in OpenAI catalog is static and does not account for the active auth mode.

`gpt-5-mini` is valid in the generic OpenAI catalog, but not for the Codex OAuth backend being used here. `/model` validates only against the configured/known catalog, not against provider/account-specific runtime support.

## Fix

Not fixed in this session.

Likely fixes:

1. Filter built-in OpenAI models when the provider is using Codex OAuth
2. Or validate a requested model against provider/auth-mode compatibility before accepting `/model`
3. Or annotate `/models` entries as unsupported for the current auth mode and block switching to them

## Test Coverage

No regression test added yet.

A good follow-up test would cover: OpenAI provider + Codex OAuth token + `/model gpt-5-mini` rejecting early with a clear compatibility error.
