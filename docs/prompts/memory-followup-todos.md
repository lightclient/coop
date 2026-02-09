# Memory Follow-up TODO Prompts

These prompts cover the remaining memory follow-up tasks identified after the initial implementation pass.

Recommended execution order:

1. `docs/prompts/memory-reconciliation-e2e-validation.md`
2. `docs/prompts/memory-prompt-bootstrap-index-injection.md`
3. `docs/prompts/memory-retention-compression-archive.md`
4. `docs/prompts/memory-embedding-provider-expansion.md`

Rationale:
- Start by hardening end-to-end confidence and trace visibility.
- Then ship user-facing prompt improvements (boot-time DB index).
- Then add lifecycle maintenance (retention/compression/archive).
- Finally expand embedding provider surface once core behavior is stable.
