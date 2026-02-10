# `coop init` — Interactive First-Run Setup

Add a `coop init` command that guides new users through API key configuration, scaffolds a minimal config and workspace at `~/.coop`, and creates default prompt files including a `BOOTSTRAP.md` that triggers the agent to personalize itself on first chat.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Today a new user must manually create a `coop.toml`, workspace directory, and prompt files before they can use coop. There is no guided setup. The config search order in `Config::find_config_path()` already checks `./coop.toml`, `$XDG_CONFIG_HOME/coop/coop.toml`, and `~/.config/coop/coop.toml`, but nothing creates these files.

The `coop init` command fills this gap: one interactive command that takes a user from zero to a working setup, then launches into a first-conversation bootstrap where the agent learns about itself and its owner.

## Goals

1. **Zero-to-running in one command.** After `coop init`, the user can immediately `coop chat` and talk to a personalized agent.
2. **API key setup.** Detect existing keys, guide OAuth token extraction, or accept a pasted API key. Store as an env var reference — never write keys to config files.
3. **Minimal config.** Generate a `coop.toml` with one user (full trust), one agent, and sensible defaults. The user picks a directory (default `~/.coop`).
4. **Workspace scaffolding.** Create the workspace directory and all default prompt files with starter content.
5. **Bootstrap conversation.** A `BOOTSTRAP.md` file triggers the agent to run a guided personalization conversation on startup. After the conversation, the agent writes the answers into SOUL.md, IDENTITY.md, and USER.md, then deletes BOOTSTRAP.md.

## Design

### Command

Add `Init` to the `Commands` enum in `crates/coop-gateway/src/cli.rs`:

```rust
Init {
    /// Directory to initialize (default: ~/.coop)
    #[arg(short, long)]
    dir: Option<String>,
}
```

Wire it in `main.rs`:

```rust
Commands::Init { dir } => cmd_init(dir.as_deref()),
```

`cmd_init` is a **sync** function (no async needed). It does not initialize tracing (same as `cmd_check` — output goes to stdout). Add it to the `console_log` exclusion list.

### Implementation: `crates/coop-gateway/src/init.rs`

Create a new module `init.rs` in `coop-gateway`. This keeps init logic separate from the main file. Target: under 400 lines.

#### Step 1: Choose directory

Start with the nice format_tui_welcome to get the logo and correct colors

```
  ████████    
██▓▓▓▓▓▓▓▓██  
██▓▓▓▓▓▓▓▓██  
██▓▓██▓▓██▓▓██
██▓▓▓▓▓▓▓▓██  
  ████████    


Welcome to Coop!

Where should coop live? [~/.coop]:
```

Accept user input or default to `~/.coop` (expand `~` via `$HOME`). If the directory already contains a `coop.toml`, ask:

```
Found existing coop.toml in ~/.coop. Overwrite? [y/N]:
```

Default to No. If No, exit with a helpful message ("Run `coop chat` to start, or `coop init --dir /other/path` to init elsewhere.").

Create the directory if it doesn't exist.

#### Step 2: Configure API key

Check for existing API key availability in this order:

1. `ANTHROPIC_API_KEY` environment variable already set
2. Claude Code OAuth token at `~/.claude/.credentials.json`

**If `ANTHROPIC_API_KEY` is already set:**

```
✓ Found ANTHROPIC_API_KEY in environment.
```

Skip to step 3.

**If not set but Claude Code credentials exist:**

```
Found Claude Code credentials at ~/.claude/.credentials.json.
Use your Claude Code subscription? [Y/n]:
```

If yes, print instructions:

```
Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):

  export ANTHROPIC_API_KEY=$(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)

Then restart your shell or run `source ~/.bashrc`.

Note: OAuth tokens expire periodically. If you get auth errors,
re-run the export command or open Claude Code to refresh the token.
```

Wait for the user to confirm they've done it or want to proceed anyway:

```
Press Enter to continue (the key will be validated when you run `coop chat`):
```

**If neither exists:**

```
Coop needs an Anthropic API key to talk to Claude.

Options:
  1. Regular API key from console.anthropic.com
  2. Claude Code OAuth token (Pro/Max subscription)

Choose [1/2]:
```

For option 1:

```
Paste your API key (starts with sk-ant-api):
```

Read input (hide with terminal raw mode if possible, but a simple read is fine). Validate it starts with `sk-ant-api` or `sk-ant-oat`. Print:

```
Add this to your shell profile:

  export ANTHROPIC_API_KEY=sk-ant-api...

Never store API keys in config files. Coop reads them from environment variables.
```

For option 2: same Claude Code instructions as above.

In all cases, the config will reference `env:ANTHROPIC_API_KEY` — the key is never written to `coop.toml`.

#### Step 3: Choose model

```
Which model? [claude-opus-4-0-20250514]:
  1. claude-sonnet-4-20250514 (fast, recommended)
  2. claude-opus-4-0-20250514 (smartest, slower)
  3. claude-haiku-3-5-20241022 (cheapest, fastest)
  4. Custom model ID

Choose [1]:
```

Default to 1 (Opus). The model list should be a const array in `init.rs` so it's easy to update. Store the selected model ID.

#### Step 4: Choose agent name

```
What should your agent be called? [cooper]:
```

Default to "Cooper". This becomes `agent.id` in the config. Validate: non-empty, ascii alphanumeric plus hyphens/underscores, max 32 chars.

#### Step 5: Enter user name

```
What's your name? [alice]:
```

Default to "alice". This becomes the first user entry. Validate: non-empty, ascii alphanumeric plus hyphens/underscores, max 32 chars, lowercase.

#### Step 6: Write config

Generate `coop.toml` in the chosen directory:

```toml
[agent]
id = "{agent_name}"
model = "anthropic/{model_id}"
workspace = "./workspace"

[[users]]
name = "{user_name}"
trust = "full"
match = ["terminal:default"]

[provider]
name = "anthropic"

[memory]
db_path = "./db/memory.db"
```

Note: workspace is `./workspace` (relative to the config dir, i.e. `~/.coop/workspace/`). This keeps everything under `~/.coop`.

Generate the TOML using string formatting (not `toml::to_string`) to produce clean, commented output. The `toml` crate's serializer doesn't produce comments, and the output format matters for readability. Use a template string with `format!()` substitution for the variable parts.

#### Step 7: Scaffold workspace

Create `{dir}/workspace/` and write the default files:

**SOUL.md:**
```markdown
# Soul

<!-- This file defines your agent's personality, voice, and values. -->
<!-- Edit it directly, or let the bootstrap conversation fill it in. -->
```

**IDENTITY.md:**
```markdown
# Identity

<!-- Who is this agent? Name, background, role. -->
<!-- The bootstrap conversation will help fill this in. -->
```

**USER.md** (in `{dir}/workspace/users/{user_name}/`):
```markdown
# User

<!-- About the primary user. Preferences, context, goals. -->
<!-- The bootstrap conversation will help fill this in. -->
```

**AGENTS.md:**
```markdown
# Instructions

You are an AI agent running inside Coop, a personal agent gateway.
Help the user with their tasks. Be concise, direct, and useful.

When using tools, explain what you're doing briefly.

## Heartbeat Protocol

Cron heartbeat messages ask you to check HEARTBEAT.md for pending tasks.
If nothing needs attention, reply with exactly **HEARTBEAT_OK**.
If there is something to report, reply with the actual content.
Keep heartbeat responses concise — these are push notifications, not conversations.
```

**TOOLS.md:**
```markdown
# Tools

## Configuration

Coop is configured via `coop.toml`. You can read, validate, and write config
changes conversationally using the config tools. The config file is automatically
watched — most changes take effect within seconds without a restart.

### Config workflow

1. **Read** the current config with `config_read` to see what's set
2. **Modify** — produce the complete new TOML (config_write requires the full file, not a patch)
3. **Write** with `config_write` — it validates before writing, backs up the old file, and rejects invalid configs

## File tools

- `read_file` — read file contents (params: path, optional offset/limit)
- `write_file` — create or overwrite a file
- `edit_file` — find-and-replace in a file
- `bash` — execute a shell command (120s timeout)

## Memory tools

- `memory_search` — search observations by text, type, people, store
- `memory_get` — fetch full observation details by ID
- `memory_write` — create a new observation
- `memory_timeline` — browse observations around a specific ID
- `memory_history` — view mutation history for an observation
- `memory_people` — search known people

## Config tools

- `config_read` — read the current coop.toml
- `config_write` — validate and write coop.toml (backs up first)
```

**HEARTBEAT.md:**
```markdown
# Heartbeat Tasks

<!-- Add periodic check items here. The agent reviews this file on heartbeat. -->
<!-- Example: -->
<!-- - [ ] Check server status at https://example.test/health -->
```

**BOOTSTRAP.md:**

See the dedicated section below.

**channels/signal.md:**
```markdown
Format all replies as plain text. Do not use markdown formatting, asterisks,
backticks, code fences, bullet markers, or any other markup. Signal renders
messages as plain text and formatting characters appear literally.

Keep messages concise. Signal is a mobile messaging app — long walls of text
are hard to read on a phone screen.

When sharing code or technical output, keep it brief and describe what matters
rather than pasting raw output.
```

#### Step 8: Create directories

Create these directories (they may be needed later even if empty now):

- `{dir}/db/` — for memory.db and signal.db
- `{dir}/workspace/sessions/` — for session persistence
- `{dir}/workspace/users/{user_name}/` — for per-user files
- `{dir}/workspace/channels/` — for channel prompt overrides

#### Step 9: Print summary

```
✓ Created ~/.coop/coop.toml
✓ Created ~/.coop/workspace/
✓ Created ~/.coop/workspace/SOUL.md
✓ Created ~/.coop/workspace/IDENTITY.md
✓ Created ~/.coop/workspace/AGENTS.md
✓ Created ~/.coop/workspace/TOOLS.md
✓ Created ~/.coop/workspace/HEARTBEAT.md
✓ Created ~/.coop/workspace/BOOTSTRAP.md
✓ Created ~/.coop/workspace/users/{user_name}/USER.md

🐔 Ready! Run:

  coop chat

Your first conversation will be a bootstrap session where the agent
learns about itself and you. Answer its questions to personalize it.
```

If the init dir is `~/.coop` or `~/.config/coop`, the config will be auto-discovered (it's in the search path), so the message just says `coop chat`. If a non-standard directory was used, include the `--config` flag:

```
  coop --config {dir}/coop.toml chat
```

### BOOTSTRAP.md — First-Run Personalization

This is the key innovation. `BOOTSTRAP.md` is a workspace file that, when present, triggers the agent to run a guided personalization conversation. After the conversation, the agent writes the answers to the appropriate files and deletes BOOTSTRAP.md.

**Content of BOOTSTRAP.md:**

```markdown
# Bootstrap

**This file exists because Coop was just initialized.** You should run the bootstrap
conversation to personalize this agent. After bootstrap is complete, delete this file.

## Instructions for the Agent

When you see this file in your workspace, start a bootstrap conversation with the user.
Do NOT immediately start asking all questions — introduce yourself first, explain
what's happening, then go through the sections conversationally.

### Opening

Greet the user warmly. Explain that this is a first-time setup conversation to
personalize the agent. Tell them:
- This will take a few minutes
- You'll ask some questions to understand who they are and how they want the agent to behave
- They can skip any question by saying "skip"
- They can end early with "done" and come back later
- Answers will be saved to workspace files that they can edit anytime

### Section 1: Agent Identity

Ask about the agent's identity. Use these questions as a guide (adapt naturally):

- What should I call myself? (name, or keep the default)
- What's my role? (personal assistant, coding partner, research helper, creative collaborator, etc.)
- Any particular personality traits? (formal/casual, concise/detailed, serious/playful)
- Are there things I should always or never do?

Write the answers to **IDENTITY.md** and **SOUL.md** using `edit_file` or `write_file`.

SOUL.md should capture personality and voice in 2-4 paragraphs. Write it in second
person ("You are..."). Example tone:

```
# Soul

You are {name}, a personal AI assistant. You are direct and concise — you get to
the point without filler. You have a dry sense of humor but know when to be serious.
You prefer showing over telling: when asked how to do something, you do it rather
than explaining how.

You value the user's time. You don't ask for confirmation before doing simple tasks.
You admit uncertainty clearly rather than hedging with qualifiers.
```

IDENTITY.md should capture factual identity info:

```
# Identity

- **Name:** Aria
- **Role:** Personal assistant and coding partner
- **Created:** February 2026
- **Traits:** Direct, concise, dry humor, action-oriented
```

### Section 2: User Profile

Ask about the user. Adapt based on what feels natural:

- What do you mostly want to use me for? (coding, writing, research, organization, etc.)
- What do you do? (profession/role — helps calibrate technical level)
- Any preferences for how I communicate? (length, format, tone)
- Anything I should know about your setup? (OS, tools, languages, etc.)

Write the answers to **USER.md** in the user's directory (`users/{username}/USER.md`).

USER.md example:

```
# User

- **Name:** Alice
- **Role:** Software engineer
- **Primary use:** Coding assistance, system administration, research
- **Technical level:** Advanced — skip basic explanations
- **Preferences:** Concise responses, show code not prose, use terminal commands
- **Environment:** Linux (Ubuntu), Rust/Python/TypeScript, neovim, tmux
```

### Section 3: Goals and Context (Optional)

If the conversation is flowing well, ask:

- Any ongoing projects I should know about?
- Regular tasks you'd want me to help with?
- Anything else that would help me be useful?

Add relevant info to USER.md or create memory observations for long-term context.

### Wrapping Up

After the conversation:

1. Write all files using `write_file` (SOUL.md, IDENTITY.md, USER.md)
2. Save key facts as memory observations using `memory_write`
3. Delete BOOTSTRAP.md using `bash` (`rm workspace/BOOTSTRAP.md` — adjust path for workspace)
4. Summarize what was set up and remind the user they can edit any file directly

Tell the user:

> "Bootstrap complete! I've saved your preferences. You can edit any of these
> files directly in your workspace, or just tell me to update them. From now on,
> every conversation starts with this context."
```

### How BOOTSTRAP.md integrates with the prompt

BOOTSTRAP.md is **not** a special-cased file in the prompt builder. It's a regular workspace file added to the `shared_files` list in the default config with `trust: full` and `cache: volatile`.

Add it to the default shared files in `crates/coop-gateway/src/config.rs`:

```rust
fn default_shared_files() -> Vec<PromptFileEntry> {
    vec![
        // ... existing entries ...
        PromptFileEntry {
            path: "BOOTSTRAP.md".to_owned(),
            trust: TrustLevel::Full,
            cache: CacheHintConfig::Volatile,
            description: Some("First-run bootstrap instructions".to_owned()),
        },
    ]
}
```

Since BOOTSTRAP.md is in the default shared file list, the prompt builder will:
- Include it if the file exists and the trust level is sufficient
- Skip it silently if the file doesn't exist (which is the case after bootstrap completes and the file is deleted)

This means no special code paths — the existing prompt file machinery handles everything. After bootstrap is complete and the file is deleted, it simply stops being included in the prompt.

### User input handling

For reading user input during `cmd_init`, use `std::io::stdin().read_line()`. The prompts go to stdout via `print!()` (no newline) followed by `std::io::stdout().flush()`. This is simple and works everywhere.

Helper function:

```rust
fn prompt_input(prompt: &str, default: &str) -> String {
    print!("{prompt} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        default.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn prompt_choice(prompt: &str, options: &[&str], default: usize) -> usize {
    print!("{prompt} [{}]: ", default + 1);
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return default;
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n >= 1 && n <= options.len() => n - 1,
        _ => default,
    }
}

fn prompt_yes_no(prompt: &str, default: bool) -> bool {
    let hint = if default { "Y/n" } else { "y/N" };
    print!("{prompt} [{hint}]: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return default;
    }
    matches!(trimmed.as_str(), "y" | "yes")
}
```

### Config search path update

Update `Config::find_config_path()` in `config.rs` to also check `~/.coop/coop.toml`:

```rust
pub(crate) fn find_config_path(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }

    // 1. Current directory
    let local = PathBuf::from("coop.toml");
    if local.exists() {
        return local;
    }

    // 2. ~/.coop (our default init location)
    if let Ok(home) = std::env::var("HOME") {
        let dot_coop = PathBuf::from(&home).join(".coop/coop.toml");
        if dot_coop.exists() {
            return dot_coop;
        }
    }

    // 3. XDG config
    if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
        let xdg = PathBuf::from(config_dir).join("coop/coop.toml");
        if xdg.exists() {
            return xdg;
        }
    }

    // 4. ~/.config/coop
    if let Ok(home) = std::env::var("HOME") {
        let home_config = PathBuf::from(home).join(".config/coop/coop.toml");
        if home_config.exists() {
            return home_config;
        }
    }

    // Default to local
    local
}
```

The key change: `~/.coop/coop.toml` is checked as priority 2, right after the current directory. This means after `coop init`, users can just run `coop chat` from anywhere.

### Validation

`cmd_init` should validate inputs inline (no need for the full `config_check` pipeline):

- Directory path: expandable, writable
- Agent name: `^[a-z0-9][a-z0-9_-]{0,31}$` (lowercase, start with alphanumeric)
- User name: same pattern as agent name
- Model: must be non-empty

After writing the config, run `config_check::validate_config` on the generated file and print a brief summary. The most common issue will be "API key not set" which the user may set up after init — print that as a note, not a hard failure:

```
Note: ANTHROPIC_API_KEY is not set. Set it in your shell profile before running `coop chat`.
```

### Update `config_check.rs`

Add a new info-level check:

- **`bootstrap_pending`** (Info): If BOOTSTRAP.md exists in the workspace, report "Bootstrap conversation pending — run `coop chat` to personalize your agent."

This gives `coop check` awareness of the bootstrap state.

## File Inventory

**New files:**
- `crates/coop-gateway/src/init.rs` — the `cmd_init` function and all init logic

**Modified files:**
- `crates/coop-gateway/src/cli.rs` — add `Init` variant to `Commands`
- `crates/coop-gateway/src/main.rs` — add `mod init;`, wire `Commands::Init`, add to `console_log` exclusion
- `crates/coop-gateway/src/config.rs` — update `find_config_path()` to check `~/.coop`, add BOOTSTRAP.md to default shared files
- `crates/coop-gateway/src/config_check.rs` — add `bootstrap_pending` info check

**No changes to `coop-core` or any other crate. No new dependencies.**

All file content (SOUL.md, AGENTS.md, etc.) is defined as `const &str` in `init.rs`. This keeps the templates co-located with the init logic and easy to update.

## Tests

Add tests in `crates/coop-gateway/src/init.rs` (inline `#[cfg(test)] mod tests`):

Tests cannot easily test interactive stdin input, so test the non-interactive parts:

- **`test_scaffold_workspace`** — Call the workspace scaffolding function with a tempdir. Verify all expected files exist with non-empty content. Verify directory structure is correct.
- **`test_generate_config`** — Call the config generation function with test parameters. Verify the output parses as valid `Config` via `toml::from_str`. Verify agent.id, model, user name, and workspace path are correct.
- **`test_config_roundtrip`** — Generate a config string, parse with `toml::from_str::<Config>`, verify fields match the inputs.
- **`test_validate_agent_name`** — Test the name validation function with valid and invalid inputs (empty, too long, special chars, uppercase).
- **`test_validate_user_name`** — Same for user names.
- **`test_existing_config_detected`** — Create a tempdir with a `coop.toml`, verify the "already exists" detection works.
- **`test_bootstrap_in_default_shared_files`** — Verify that `default_shared_files()` includes BOOTSTRAP.md.

All tests use `tempfile::tempdir()` and placeholder data per AGENTS.md rules.

The `find_config_path` change should be tested too:

- **`test_find_config_prefers_local`** — Verify `./coop.toml` is preferred over `~/.coop/coop.toml`. (Use env var manipulation or a helper that accepts paths.)

## Constraints

- No new crate dependencies. `std::io` for stdin, `std::fs` for file ops, `std::env` for HOME.
- Keep `init.rs` under 400 lines. The file templates are const strings — if they push past the limit, extract them to a `init_templates.rs` submodule.
- Never write API keys to config files. The config always uses `env:ANTHROPIC_API_KEY`.
- All user-facing text goes to stdout via `print!`/`println!`. No tracing.
- The command is fully sync — no tokio, no async.
- Exit code: 0 on success, 1 on error (e.g., can't create directory).
- Use only placeholder names in test code (alice, bob, etc.).
- Config is TOML format. Use `toml::from_str::<Config>()` to validate generated configs in tests.

## Development Loop

```bash
# After implementation:
cargo fmt
cargo build
cargo test -p coop-gateway
cargo clippy --all-targets --all-features -- -D warnings

# Manual verification:
# 1. Init in a temp directory:
cargo run --bin coop -- init --dir /tmp/coop-test

# 2. Verify files were created:
ls -la /tmp/coop-test/
ls -la /tmp/coop-test/workspace/
cat /tmp/coop-test/coop.toml
cat /tmp/coop-test/workspace/BOOTSTRAP.md

# 3. Verify config is valid:
cargo run --bin coop -- --config /tmp/coop-test/coop.toml check

# 4. Run chat (requires API key):
cargo run --bin coop -- --config /tmp/coop-test/coop.toml chat

# 5. Verify bootstrap conversation triggers (BOOTSTRAP.md in prompt)
# 6. After bootstrap, verify BOOTSTRAP.md is deleted and SOUL.md/IDENTITY.md are populated
```
