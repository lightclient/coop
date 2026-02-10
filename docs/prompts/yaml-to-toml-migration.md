# Prompt: Migrate config format from YAML to TOML

Convert coop's configuration from YAML (`coop.yaml` / `serde_yaml`) to TOML (`coop.toml` / `toml`). This is a mechanical migration — no config schema changes, no behavioral changes, no new features.

## Why

TOML is the Rust ecosystem standard (Cargo, rustfmt, clippy, taplo, deny — all use TOML). The project already has `taplo fmt` in CI for TOML formatting. YAML is the only outlier. TOML also eliminates the `serde_yaml` and `unsafe-libyaml` dependencies.

## Scope

Every file that references YAML config must be updated. The list below is exhaustive — grep for `yaml`, `YAML`, `Yaml`, `serde_yaml`, `coop.yaml`, `.yaml`, `.yml` to verify nothing is missed.

## Migration plan

Work in this order. Each step must compile and pass tests before moving to the next.

### Step 1: Add `toml` dependency, remove `serde_yaml`

**Cargo.toml (workspace root):**
- Add `toml = "0.8"` to `[workspace.dependencies]`
- Remove `serde_yaml = "0.9"` from `[workspace.dependencies]`

**crates/coop-gateway/Cargo.toml:**
- Replace `serde_yaml = { workspace = true }` with `toml = { workspace = true }`

**supply-chain/config.toml:**
- Remove the `[[exemptions.serde_yaml]]` entry
- Remove the `[[exemptions.unsafe-libyaml]]` entry
- Run `cargo deny check` to see if `toml` needs an entry (it shouldn't — it's well-audited)

### Step 2: Convert the config file

**Delete:** `coop.yaml`

**Create:** `coop.toml` with equivalent content:

```toml
[agent]
id = "coop"
model = "anthropic/claude-opus-4-5-20251101"
workspace = "./workspaces/default"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]

[channels.signal]
db_path = "./db/signal.db"

[provider]
name = "anthropic"
# API key from env: ANTHROPIC_API_KEY

[memory]
db_path = "./db/memory.db"
```

**.gitignore:**
- Change `dev.yaml` to `dev.toml` (if present)

### Step 3: Update `config.rs` — the core parser

**File:** `crates/coop-gateway/src/config.rs`

Changes:
1. Replace `use serde_yaml` with `use toml` (no use statement needed — just call `toml::from_str`/`toml::to_string`)
2. In `Config::load()`: change `serde_yaml::from_str(&content)` → `toml::from_str(&content)`
3. In `Config::find_config_path()`: change all `"coop.yaml"` → `"coop.toml"` (local path, XDG path, HOME path)
4. Update the doc comment from "Load config from a YAML file" → "Load config from a TOML file"
5. In tests: convert every `serde_yaml::from_str(yaml)` to `toml::from_str(toml_str)` and rewrite every inline YAML string literal as TOML. There are ~20 test functions here.

**TOML syntax notes for converting test strings:**
- YAML `key: value` → TOML `key = "value"` (strings must be quoted)
- YAML arrays `[a, b]` → TOML `["a", "b"]`
- YAML `- name: x` (array of tables) → TOML `[[section]]` headers
- YAML nested objects → TOML `[section.subsection]` headers
- YAML booleans `true`/`false` → TOML `true`/`false` (same)
- YAML integers → TOML integers (same)
- `serde_yaml::to_string` → `toml::to_string` (roundtrip test)

**Important:** serde_yaml is lenient about quoting; TOML requires explicit string quoting. For cron expressions, use TOML literal strings: `cron = '*/30 * * * *'`.

### Step 4: Update `config_write.rs` — atomic write / backup

**File:** `crates/coop-gateway/src/config_write.rs`

Changes:
1. `backup_config`: change `.with_extension("yaml.bak")` → `.with_extension("toml.bak")`
2. `atomic_write`: change `.with_extension("yaml.tmp")` → `.with_extension("toml.tmp")`
3. `safe_write_config`: change `.with_extension("yaml.staging")` → `.with_extension("toml.staging")`
4. Tests: change all `dir.join("coop.yaml")` → `dir.join("coop.toml")`
5. Tests: change inline YAML config strings to TOML format
6. Tests: update assertion strings from `"yaml"` references to `"toml"`
7. Test `test_safe_write_invalid_config`: change `"{{not valid yaml"` to `"{{not valid toml"` or similar invalid TOML

### Step 5: Update `config_check.rs` — validation

**File:** `crates/coop-gateway/src/config_check.rs`

Changes:
1. Rename check `"yaml_parse"` → `"toml_parse"` everywhere (the check name string, all test assertions that match on it)
2. Update the success message from `"config syntax valid"` (no change needed unless you want to be explicit)
3. Tests: change all `dir.join("coop.yaml")` → `dir.join("coop.toml")`
4. Tests: convert every inline YAML config string to TOML
5. Tests: update `"{{not valid yaml"` → `"{{not valid toml"` or other invalid TOML
6. Tests: update assertions that check for `"yaml_parse"` → `"toml_parse"`

### Step 6: Update `config_tool.rs` — agent-facing tools

**File:** `crates/coop-gateway/src/config_tool.rs`

Changes:
1. `ConfigReadTool::definition()`: change description from `"Read the current coop.yaml configuration file."` → `"Read the current coop.toml configuration file."`
2. `ConfigWriteTool::definition()`: change description from `"Validate and write coop.yaml..."` → `"Validate and write coop.toml..."`
3. `ConfigWriteTool::definition()`: change the `content` parameter description from `"Complete YAML content for coop.yaml..."` → `"Complete TOML content for coop.toml..."`
4. Tracing messages: update `"config_read"` / `"config_write"` log messages if they mention YAML
5. Tests: change all `dir.join("coop.yaml")` → `dir.join("coop.toml")`
6. Tests: convert inline YAML config strings to TOML
7. Test names/comments: update any `yaml` references

### Step 7: Update `config_watcher.rs` — hot reload

**File:** `crates/coop-gateway/src/config_watcher.rs`

Changes:
1. `write_config` helper: change `dir.join("coop.yaml")` → `dir.join("coop.toml")`
2. `minimal_yaml` → rename to `minimal_toml`, convert the format string to TOML
3. All `serde_yaml::from_str(...)` → `toml::from_str(...)`
4. All inline YAML string literals → TOML format
5. Test `try_reload_rejects_invalid_yaml` → rename to `try_reload_rejects_invalid_toml`, update `"{{not yaml"` → `"{{not toml"` or similar invalid TOML

### Step 8: Update `scheduler.rs` tests

**File:** `crates/coop-gateway/src/scheduler.rs`

Changes (tests section, ~line 1180+):
1. All `serde_yaml::from_str(...)` → `toml::from_str(...)`
2. All `serde_yaml::to_string(...)` → `toml::to_string(...)` (used for serializing trust levels in test helpers)
3. The `yaml` variable names → rename to `toml_str` or similar
4. Convert all inline YAML strings to TOML format
5. The test helper functions that build config strings (around line 1329 and 1399) construct YAML dynamically — rewrite them to construct TOML dynamically. Pay attention to `[[users]]` and `[[cron]]` array-of-tables syntax.

### Step 9: Update remaining source files with `serde_yaml` calls

**Files with `serde_yaml::from_str` in tests:**
- `crates/coop-gateway/src/main.rs` (lines ~1320, ~1423) — test helper `test_config()` and other test configs
- `crates/coop-gateway/src/router.rs` (line ~384) — test config
- `crates/coop-gateway/src/gateway.rs` (line ~1229) — test config  
- `crates/coop-gateway/src/signal_loop/tests.rs` (lines ~147, ~671) — test configs
- `crates/coop-gateway/tests/memory_prompt_index.rs` (line ~218) — test config
- `crates/coop-gateway/tests/memory_reconciliation_e2e.rs` (line ~236) — test config

For each: replace `serde_yaml::from_str(...)` with `toml::from_str(...)` and convert the inline string from YAML to TOML.

**Non-test source reference:**
- `crates/coop-gateway/src/main.rs` line ~1073: change `"signal channel is not configured in coop.yaml"` → `"signal channel is not configured in coop.toml"`

### Step 10: Update `coop-core` comment

**File:** `crates/coop-core/src/prompt.rs` line 193

Change comment `/// Parse YAML frontmatter from a SKILL.md file` → `/// Parse YAML frontmatter from a SKILL.md file` — **keep this one as-is**. SKILL.md files use YAML frontmatter (that's a markdown convention, not related to coop config). Do NOT change this.

### Step 11: Update workspace docs and agent instructions

**File:** `workspaces/default/TOOLS.md`

Major rewrite. All references to `coop.yaml` → `coop.toml`. All `YAML` → `TOML`. The config_write tool description changes to accept TOML. The full config reference block must be rewritten from YAML syntax to TOML syntax. Specifically:
- "configured via `coop.yaml`" → "configured via `coop.toml`"
- "produce the complete new YAML" → "produce the complete new TOML"
- "config_write requires the full file, not a patch" — keep as-is
- "Returns the current coop.yaml contents" → "Returns the current coop.toml contents"  
- "Validates and writes coop.yaml" → "Validates and writes coop.toml"
- "Backs up the previous version to `coop.yaml.bak`" → "Backs up the previous version to `coop.toml.bak`"
- "`content` (string, required) — the complete YAML file contents" → "`content` (string, required) — the complete TOML file contents"
- "## coop.yaml reference" → "## coop.toml reference"
- Convert the entire `yaml` code block to a `toml` code block

**File:** `workspaces/default/AGENTS.md`

- "YAML config parsing" → "TOML config parsing"
- "Config loaded from YAML (`coop.yaml`)" → "Config loaded from TOML (`coop.toml`)"

### Step 12: Update `.claude/skills/tui-validate/SKILL.md`

Change:
- All ` ```yaml ` fenced code blocks that show coop config to ` ```toml ` and convert content
- `"Verify coop.yaml exists"` → `"Verify coop.toml exists"`

### Step 13: Update `README.md`

Convert all YAML config examples to TOML:
- Line ~45: API key rotation example
- Line ~62: `coop.yaml` mention → `coop.toml`
- Line ~70: memory config full reference
- Lines ~141-168: embedding provider examples  
- Line ~202: cron config example

All ` ```yaml ` fenced blocks showing coop config → ` ```toml ` with converted content.

Change prose: "add a `memory:` section to your `coop.yaml`" → "add a `[memory]` section to your `coop.toml`", etc.

### Step 14: Update design and architecture docs

These files contain YAML config examples and references to `coop.yaml`:

- `docs/config-safety.md` — heavy rewrite, many references to `coop.yaml`, YAML parsing, `.yaml.bak`, staging files, etc.
- `docs/design.md` — config example block, mentions of YAML
- `docs/design-principles.md` — `coop.yaml` references, `.coop.yaml.bak`, YAML snapshot mentions
- `docs/phase1-plan.md` — mentions YAML choice rationale (update to explain TOML choice), `serde_yaml` dep → `toml` dep
- `docs/memory-design.md` — config examples
- `docs/signal-integration-plan.md` — config examples  
- `docs/system-prompt-design.md` — config examples
- `docs/architecture.md` — may have references

For each: convert ` ```yaml ` config blocks to ` ```toml `, change `coop.yaml` → `coop.toml`, change `serde_yaml` → `toml`, change `.yaml.bak` → `.toml.bak`, etc.

### Step 15: Update prompt docs in `docs/prompts/`

These files reference YAML config. Convert examples and references:

- `docs/prompts/config-safety-impl.md` — extensive YAML references (`.yaml.bak`, `.yaml.tmp`, `.yaml.staging`, YAML parse, etc.)
- `docs/prompts/configurable-prompt-files.md` — config examples
- `docs/prompts/cron-delivery.md` — config examples
- `docs/prompts/cron-scheduler.md` — config examples
- `docs/prompts/memory-prompt-bootstrap-index-injection.md` — config example
- `docs/prompts/memory-embedding-provider-expansion.md` — config example
- `docs/prompts/memory-retention-compression-archive.md` — config example
- `docs/prompts/memory-remaining-implementation.md` — `coop.yaml` reference
- `docs/prompts/signal-e2e-trace-loop.md` — `coop.yaml` references and config examples

### Step 16: Verify and clean up

1. `cargo build` — must compile
2. `cargo test -p coop-gateway` — all tests pass
3. `cargo test --workspace` — everything passes
4. `cargo clippy --all-targets --all-features -- -D warnings` — no warnings
5. `cargo fmt` — formatted
6. `taplo fmt` — TOML files formatted (including the new `coop.toml`)
7. `grep -rn "serde_yaml\|serde_yml" crates/` — must return zero results
8. `grep -rn "coop\.yaml" .` — must return zero results (except maybe git history references)
9. `grep -rn "\.yaml\.bak\|\.yaml\.tmp\|\.yaml\.staging" crates/` — must return zero results
10. `cargo deny check` — supply chain checks pass without the removed exemptions
11. Spot-check: `cargo run --bin coop -- check` with the new `coop.toml` in place

## TOML syntax cheat sheet for config conversion

YAML → TOML equivalents for coop config patterns:

```yaml
# YAML                              # TOML equivalent
agent:                               [agent]
  id: coop                           id = "coop"
  model: anthropic/claude-...        model = "anthropic/claude-..."
  workspace: ./workspaces/default    workspace = "./workspaces/default"

users:                               [[users]]
  - name: alice                      name = "alice"
    trust: full                      trust = "full"
    match:                           match = ["terminal:default", "signal:alice-uuid"]
      - "terminal:default"
      - "signal:alice-uuid"
  - name: bob                        [[users]]
    trust: inner                     name = "bob"
    match:                           trust = "inner"
      - "signal:bob-uuid"            match = ["signal:bob-uuid"]

channels:                            [channels.signal]
  signal:                            db_path = "./db/signal.db"
    db_path: ./db/signal.db          verbose = false
    verbose: false

provider:                            [provider]
  name: anthropic                    name = "anthropic"
  api_keys:                          api_keys = ["env:ANTHROPIC_API_KEY", "env:ANTHROPIC_API_KEY_2"]
    - env:ANTHROPIC_API_KEY
    - env:ANTHROPIC_API_KEY_2

memory:                              [memory]
  db_path: ./db/memory.db            db_path = "./db/memory.db"
  prompt_index:                      [memory.prompt_index]
    enabled: true                    enabled = true
    limit: 12                        limit = 12
    max_tokens: 1200                 max_tokens = 1200
  retention:                         [memory.retention]
    enabled: true                    enabled = true
    archive_after_days: 30           archive_after_days = 30
  embedding:                         [memory.embedding]
    provider: voyage                 provider = "voyage"
    model: voyage-3-large            model = "voyage-3-large"
    dimensions: 1024                 dimensions = 1024

cron:                                [[cron]]
  - name: heartbeat                  name = "heartbeat"
    cron: "*/30 * * * *"             cron = "*/30 * * * *"
    user: alice                      user = "alice"
    message: check HEARTBEAT.md      message = "check HEARTBEAT.md"
    deliver:                         [cron.deliver]
      channel: signal                channel = "signal"
      target: alice-uuid             target = "alice-uuid"
  - name: cleanup                    [[cron]]
    cron: "0 3 * * *"               name = "cleanup"
    message: run cleanup             cron = "0 3 * * *"
                                     message = "run cleanup"

prompt:                              [prompt]
  shared_files:                      [[prompt.shared_files]]
    - path: SOUL.md                  path = "SOUL.md"
      trust: familiar                trust = "familiar"
      cache: stable                  cache = "stable"
      description: Agent personality description = "Agent personality"
    - path: TOOLS.md                 [[prompt.shared_files]]
  user_files:                        path = "TOOLS.md"
    - path: AGENTS.md               [[prompt.user_files]]
                                     path = "AGENTS.md"
```

**Gotcha: `[[cron]]` with inline `[cron.deliver]`**

In TOML, a `[cron.deliver]` sub-table inside a `[[cron]]` entry must appear *after* all scalar keys of that `[[cron]]` entry and *before* the next `[[cron]]`. Example:

```toml
[[cron]]
name = "morning-briefing"
cron = "0 8 * * *"
user = "alice"
message = "Morning briefing"

[cron.deliver]
channel = "signal"
target = "alice-uuid"

[[cron]]
name = "cleanup"
cron = "0 3 * * *"
message = "run cleanup"
```

**Gotcha: serde `#[serde(default)]` and TOML**

serde's `#[serde(default)]` works the same with `toml` as with `serde_yaml`. Optional sections like `[memory]`, `[provider]`, `[prompt]` will still get their defaults when omitted. No struct changes needed.

**Gotcha: `TrustLevel` serialization**

`TrustLevel` derives `Serialize`/`Deserialize` and uses `#[serde(rename_all = "lowercase")]`. This works identically in TOML — values serialize as `"full"`, `"inner"`, `"familiar"`, `"public"`. The scheduler tests that use `serde_yaml::to_string(&u.trust)` to build config strings should use `toml::to_string(&u.trust)` — but note that `toml::to_string` of a bare enum variant produces `"full"\n` (with quotes and newline). You may need `.trim().trim_matches('"')` or just hardcode the string values in the test helpers.

Actually, `toml` serializes a bare string value differently — `toml::to_string` expects a top-level table, not a bare value. For the scheduler test helpers that do `serde_yaml::to_string(&u.trust).unwrap().trim()`, replace with `u.trust` formatted directly (e.g., match on the enum or use `serde_json::to_string` and strip quotes, or just `format!("{:?}", u.trust).to_lowercase()`). The simplest approach: add a `.as_str()` method to `TrustLevel` or just hardcode in the test helper.

## Files changed (complete list)

### Cargo/dependency files
- `Cargo.toml` — workspace deps: add `toml`, remove `serde_yaml`
- `crates/coop-gateway/Cargo.toml` — dep: `serde_yaml` → `toml`
- `supply-chain/config.toml` — remove `serde_yaml` and `unsafe-libyaml` exemptions

### Config file
- `coop.yaml` → **delete**
- `coop.toml` → **create**

### Core source files
- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_write.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/config_tool.rs`
- `crates/coop-gateway/src/config_watcher.rs`
- `crates/coop-gateway/src/scheduler.rs` (tests)
- `crates/coop-gateway/src/main.rs` (error message + tests)
- `crates/coop-gateway/src/router.rs` (tests)
- `crates/coop-gateway/src/gateway.rs` (tests)
- `crates/coop-gateway/src/signal_loop/tests.rs`
- `crates/coop-gateway/tests/memory_prompt_index.rs`
- `crates/coop-gateway/tests/memory_reconciliation_e2e.rs`

### NOT changed (has "YAML" but unrelated to config)
- `crates/coop-core/src/prompt.rs` — refers to YAML frontmatter in SKILL.md files (markdown convention, not coop config)

### Workspace/agent docs
- `workspaces/default/TOOLS.md`
- `workspaces/default/AGENTS.md`

### Project docs
- `README.md`
- `.claude/skills/tui-validate/SKILL.md`

### Design/architecture docs
- `docs/config-safety.md`
- `docs/design.md`
- `docs/design-principles.md`
- `docs/phase1-plan.md`
- `docs/memory-design.md`
- `docs/signal-integration-plan.md`
- `docs/system-prompt-design.md`

### Prompt docs
- `docs/prompts/config-safety-impl.md`
- `docs/prompts/configurable-prompt-files.md`
- `docs/prompts/cron-delivery.md`
- `docs/prompts/cron-scheduler.md`
- `docs/prompts/memory-prompt-bootstrap-index-injection.md`
- `docs/prompts/memory-embedding-provider-expansion.md`
- `docs/prompts/memory-retention-compression-archive.md`
- `docs/prompts/memory-remaining-implementation.md`
- `docs/prompts/signal-e2e-trace-loop.md`

### Other
- `.gitignore` — `dev.yaml` → `dev.toml`
