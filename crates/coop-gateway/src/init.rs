use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::init_templates::{
    AGENTS_MD_INIT, BOOTSTRAP_MD, HEARTBEAT_MD, IDENTITY_MD, SIGNAL_MD, SOUL_MD, TOOLS_MD_INIT,
    USER_MD,
};
use coop_core::user_workspace_dir_name;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-20250514", "fast, recommended"),
    ("claude-opus-4-0-20250514", "smartest, slower"),
    ("claude-haiku-3-5-20241022", "cheapest, fastest"),
];
const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4o-mini", "fast, recommended"),
    ("gpt-5-mini", "smart reasoning"),
    ("gpt-5-codex", "coding / responses API"),
];
const OLLAMA_DEFAULT_MODEL: &str = "llama3.2";
const OPENAI_COMPATIBLE_DEFAULT_MODEL: &str = "gpt-4o-mini";

const NAME_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitProvider {
    Anthropic,
    OpenAi,
    OpenAiCompatible,
    Ollama,
}

impl InitProvider {
    fn provider_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
        }
    }

    fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::OpenAiCompatible | Self::Ollama => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Input helpers
// ---------------------------------------------------------------------------

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

fn prompt_choice(prompt: &str, option_count: usize, default: usize) -> usize {
    print!("{prompt} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return default;
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n >= 1 && n <= option_count => n,
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

fn prompt_continue() {
    print!("Press Enter to continue: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
}

fn choose_model(prompt: &str, models: &[(&str, &str)]) -> String {
    println!("{prompt}");
    for (i, (id, desc)) in models.iter().enumerate() {
        println!("  {}. {id} ({desc})", i + 1);
    }
    println!("  {}. Custom model ID\n", models.len() + 1);

    let choice = prompt_choice("Choose", models.len() + 1, 1);
    if choice == models.len() + 1 {
        prompt_input("Model ID", models[0].0)
    } else {
        let idx = choice.saturating_sub(1).min(models.len() - 1);
        models[idx].0.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name cannot be empty".to_owned());
    }
    if name.len() > 32 {
        return Err("name must be 32 characters or less".to_owned());
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("name must start with a lowercase letter or digit".to_owned());
    }
    for ch in name.chars() {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-' {
            return Err(format!(
                "name can only contain lowercase letters, digits, hyphens, and underscores (pattern: {NAME_PATTERN})"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config generation
// ---------------------------------------------------------------------------

fn generate_config(
    agent_name: &str,
    provider: InitProvider,
    model_id: &str,
    user_name: &str,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
) -> String {
    let agent_model = match provider {
        InitProvider::Anthropic => format!("anthropic/{model_id}"),
        _ => model_id.to_owned(),
    };

    let mut provider_block = format!("[provider]\nname = \"{}\"\n", provider.provider_name());
    if let Some(base_url) = base_url.filter(|value| !value.trim().is_empty()) {
        let _ = writeln!(provider_block, "base_url = \"{}\"", base_url.trim());
    }
    if let Some(api_key_env) = api_key_env.filter(|value| !value.trim().is_empty()) {
        let _ = writeln!(provider_block, "api_key_env = \"{}\"", api_key_env.trim());
    }

    format!(
        r#"[agent]
id = "{agent_name}"
model = "{agent_model}"
workspace = "./workspace"

[[users]]
name = "{user_name}"
trust = "full"
match = ["terminal:default"]

{provider_block}
[memory]
db_path = "./db/memory.db"

# Uncomment to enable group chat responses:
# [[groups]]
# match = ["signal:group:YOUR_GROUP_ID_HERE"]
# trigger = "mention"
# mention_names = ["{agent_name}"]
# default_trust = "familiar"
# trust_ceiling = {{ fixed = "familiar" }}
"#
    )
}

// ---------------------------------------------------------------------------
// Workspace scaffolding
// ---------------------------------------------------------------------------

fn scaffold_workspace(dir: &Path, user_name: &str) -> anyhow::Result<Vec<String>> {
    let workspace = dir.join("workspace");
    let users_dir = workspace
        .join("users")
        .join(user_workspace_dir_name(user_name));
    let channels_dir = workspace.join("channels");
    let sessions_dir = workspace.join("sessions");
    let db_dir = dir.join("db");

    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&users_dir)?;
    std::fs::create_dir_all(&channels_dir)?;
    std::fs::create_dir_all(&sessions_dir)?;
    std::fs::create_dir_all(&db_dir)?;

    let files: &[(&Path, &str)] = &[
        (&workspace.join("SOUL.md"), SOUL_MD),
        (&workspace.join("IDENTITY.md"), IDENTITY_MD),
        (&workspace.join("AGENTS.md"), AGENTS_MD_INIT),
        (&workspace.join("TOOLS.md"), TOOLS_MD_INIT),
        (&workspace.join("HEARTBEAT.md"), HEARTBEAT_MD),
        (&workspace.join("BOOTSTRAP.md"), BOOTSTRAP_MD),
        (&users_dir.join("USER.md"), USER_MD),
        (&channels_dir.join("signal.md"), SIGNAL_MD),
    ];

    let mut created = Vec::new();
    for (path, content) in files {
        std::fs::write(path, content)?;
        created.push(path.display().to_string());
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// Expand ~ in paths
// ---------------------------------------------------------------------------

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if path == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(path)
}

fn collapse_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home_path = PathBuf::from(&home);
        if let Ok(stripped) = path.strip_prefix(&home_path) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// API key detection
// ---------------------------------------------------------------------------

fn detect_anthropic_api_key() -> ApiKeyStatus {
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return ApiKeyStatus::EnvSet("ANTHROPIC_API_KEY");
    }
    if let Ok(home) = std::env::var("HOME") {
        let creds = PathBuf::from(home).join(".claude/.credentials.json");
        if creds.exists() {
            return ApiKeyStatus::ClaudeCodeCreds(creds);
        }
    }
    ApiKeyStatus::None
}

fn detect_openai_api_key() -> ApiKeyStatus {
    if std::env::var("OPENAI_API_KEY").is_ok() {
        ApiKeyStatus::EnvSet("OPENAI_API_KEY")
    } else {
        ApiKeyStatus::None
    }
}

enum ApiKeyStatus {
    EnvSet(&'static str),
    ClaudeCodeCreds(PathBuf),
    None,
}

// ---------------------------------------------------------------------------
// cmd_init entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_init(dir_arg: Option<&str>) -> anyhow::Result<()> {
    // Step 1: Welcome + choose directory
    let welcome = crate::tui_helpers::format_tui_welcome(env!("CARGO_PKG_VERSION"), "", "");
    println!("\n{welcome}\n");
    println!("Welcome to Coop!\n");

    let default_dir = "~/.coop";
    let dir_input = if let Some(d) = dir_arg {
        d.to_owned()
    } else {
        prompt_input("Where should coop live?", default_dir)
    };
    let dir = expand_home(&dir_input);
    let display_dir = collapse_home(&dir);

    let config_path = dir.join("coop.toml");
    if config_path.exists()
        && !prompt_yes_no(
            &format!("Found existing coop.toml in {display_dir}. Overwrite?"),
            false,
        )
    {
        println!("\nRun `coop chat` to start, or `coop init --dir /other/path` to init elsewhere.");
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;

    // Step 2: Provider
    println!("Choose a provider:");
    println!("  1. Anthropic");
    println!("  2. OpenAI");
    println!("  3. OpenAI-compatible");
    println!("  4. Ollama\n");

    let provider = match prompt_choice("Choose", 4, 1) {
        1 => InitProvider::Anthropic,
        2 => InitProvider::OpenAi,
        3 => InitProvider::OpenAiCompatible,
        _ => InitProvider::Ollama,
    };

    // Step 3: Provider auth / endpoint
    let mut provider_base_url: Option<String> = None;
    let mut provider_api_key_env: Option<String> = None;

    println!();
    match provider {
        InitProvider::Anthropic => match detect_anthropic_api_key() {
            ApiKeyStatus::EnvSet(env_name) => {
                println!("✓ Found {env_name} in environment.");
            }
            ApiKeyStatus::ClaudeCodeCreds(creds_path) => {
                println!("Found Claude Code credentials at {}.", creds_path.display());
                if prompt_yes_no("Use your Claude Code subscription?", true) {
                    println!("\nAdd this to your shell profile (~/.bashrc, ~/.zshrc, etc.):\n");
                    println!(
                        "  export ANTHROPIC_API_KEY=$(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)\n"
                    );
                    println!("Then restart your shell or run `source ~/.bashrc`.\n");
                    println!("Note: OAuth tokens expire periodically. If you get auth errors,");
                    println!(
                        "re-run the export command or open Claude Code to refresh the token.\n"
                    );
                    prompt_continue();
                } else {
                    print_manual_key_instructions(provider);
                    prompt_continue();
                }
            }
            ApiKeyStatus::None => {
                println!("Coop needs an Anthropic API key to talk to Claude.\n");
                println!("Options:");
                println!("  1. Regular API key from console.anthropic.com");
                println!("  2. Claude Code OAuth token (Pro/Max subscription)\n");

                let choice = prompt_choice("Choose", 2, 1);
                if choice == 2 {
                    println!("\nAdd this to your shell profile (~/.bashrc, ~/.zshrc, etc.):\n");
                    println!(
                        "  export ANTHROPIC_API_KEY=$(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)\n"
                    );
                    println!("Then restart your shell or run `source ~/.bashrc`.\n");
                    prompt_continue();
                } else {
                    print_manual_key_instructions(provider);
                    prompt_continue();
                }
            }
        },
        InitProvider::OpenAi => match detect_openai_api_key() {
            ApiKeyStatus::EnvSet(env_name) => {
                println!("✓ Found {env_name} in environment.");
            }
            ApiKeyStatus::ClaudeCodeCreds(_) | ApiKeyStatus::None => {
                print_manual_key_instructions(provider);
                prompt_continue();
            }
        },
        InitProvider::OpenAiCompatible => {
            let base_url = prompt_input("Base URL", "http://localhost:8000/v1");
            provider_base_url = Some(base_url);
            let api_key_env = prompt_input(
                "API key env var (leave blank if your server does not require one)",
                "",
            );
            if !api_key_env.trim().is_empty() {
                provider_api_key_env = Some(api_key_env.trim().to_owned());
                println!(
                    "\nSet {} in your shell profile before running coop.",
                    api_key_env.trim()
                );
                prompt_continue();
            }
        }
        InitProvider::Ollama => {
            let base_url = prompt_input("Ollama base URL", "http://localhost:11434");
            if base_url != "http://localhost:11434" {
                provider_base_url = Some(base_url);
            }
        }
    }

    // Step 4: Model
    println!();
    let model_id = match provider {
        InitProvider::Anthropic => choose_model("Which Anthropic model?", ANTHROPIC_MODELS),
        InitProvider::OpenAi => choose_model("Which OpenAI model?", OPENAI_MODELS),
        InitProvider::OpenAiCompatible => prompt_input("Model ID", OPENAI_COMPATIBLE_DEFAULT_MODEL),
        InitProvider::Ollama => prompt_input("Model ID", OLLAMA_DEFAULT_MODEL),
    };

    // Step 5: Agent name
    println!();
    let agent_name = loop {
        let name = prompt_input("What should your agent be called?", "cooper");
        match validate_name(&name) {
            Ok(()) => break name,
            Err(e) => println!("Invalid name: {e}. Try again."),
        }
    };

    // Step 5: User name
    println!();
    let user_name = loop {
        let name = prompt_input("What's your name?", "alice");
        let lower = name.to_lowercase();
        match validate_name(&lower) {
            Ok(()) => break lower,
            Err(e) => println!("Invalid name: {e}. Try again."),
        }
    };

    // Step 7: Write config
    let config_content = generate_config(
        &agent_name,
        provider,
        &model_id,
        &user_name,
        provider_base_url.as_deref(),
        provider_api_key_env.as_deref(),
    );
    std::fs::write(&config_path, &config_content)?;

    // Step 8 + 9: Scaffold workspace + directories
    let created_files = scaffold_workspace(&dir, &user_name)?;

    // Step 9: Summary
    println!();
    println!("✓ Created {}", collapse_home(&config_path));
    for file in &created_files {
        let p = PathBuf::from(file);
        println!("✓ Created {}", collapse_home(&p));
    }

    // Validate the generated config
    let report = crate::config_check::validate_config(&config_path, &dir);
    let api_key_missing = report
        .results
        .iter()
        .any(|r| r.name == "api_key_present" && !r.passed);
    if api_key_missing
        && let Some(env_name) = provider_api_key_env
            .as_deref()
            .or_else(|| provider.default_api_key_env())
    {
        println!(
            "\nNote: {env_name} is not set. Set it in your shell profile before running `coop chat`."
        );
    }

    // Check for bootstrap
    let bootstrap_check = report
        .results
        .iter()
        .find(|r| r.name == "bootstrap_pending");
    if let Some(check) = bootstrap_check
        && !check.passed
    {
        println!("· {}", check.message);
    }

    // Check sandbox availability and warn if needed (sandbox is enabled by default)
    check_sandbox_availability();

    println!("\n🐔 Ready! Run:\n");

    let is_auto_discovered = is_auto_discover_path(&dir);
    if is_auto_discovered {
        println!("  coop chat");
    } else {
        println!("  coop --config {} chat", collapse_home(&config_path));
    }

    println!("\nYour first conversation will be a bootstrap session where the agent");
    println!("learns about itself and you. Answer its questions to personalize it.");

    Ok(())
}

fn print_manual_key_instructions(provider: InitProvider) {
    let env_name = provider.default_api_key_env().unwrap_or("API_KEY_ENV");
    let example = match provider {
        InitProvider::Anthropic => "sk-ant-api...",
        InitProvider::OpenAi | InitProvider::OpenAiCompatible => "sk-test-xxx",
        InitProvider::Ollama => "token-if-needed",
    };
    println!("\nAdd this to your shell profile:\n");
    println!("  export {env_name}={example}\n");
    println!("Never store API keys in config files. Coop reads them from environment variables.");
}

fn check_sandbox_availability() {
    match coop_sandbox::probe() {
        Ok(sandbox_info) => {
            println!("✓ Sandbox available: {}", sandbox_info.name);
        }
        Err(e) => {
            println!("\n⚠️  Sandbox Warning:");
            println!("   {e}\n");
            #[cfg(target_os = "macos")]
            {
                println!("   Install with: brew install apple/apple/container");
                println!("   Or disable sandbox: coop config set sandbox.enabled false\n");
            }
            #[cfg(target_os = "linux")]
            {
                println!("   Check that unprivileged user namespaces are enabled.");
                println!("   Or disable sandbox: coop config set sandbox.enabled false\n");
            }
        }
    }
}

fn is_auto_discover_path(dir: &Path) -> bool {
    if let Ok(home) = std::env::var("HOME") {
        let dot_coop = PathBuf::from(&home).join(".coop");
        if dir == dot_coop {
            return true;
        }
        let config_coop = PathBuf::from(&home).join(".config/coop");
        if dir == config_coop {
            return true;
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let xdg_coop = PathBuf::from(xdg).join("coop");
        if dir == xdg_coop {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_name_valid() {
        assert!(validate_name("cooper").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("agent-1").is_ok());
        assert!(validate_name("my_agent").is_ok());
        assert!(validate_name("0abc").is_ok());
    }

    #[test]
    fn test_validate_name_invalid() {
        assert!(validate_name("").is_err());
        assert!(validate_name("A").is_err());
        assert!(validate_name("Agent").is_err());
        assert!(validate_name("-bad").is_err());
        assert!(validate_name("_bad").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("has.dot").is_err());
        let long = "a".repeat(33);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn test_validate_name_max_length() {
        let exactly32 = "a".repeat(32);
        assert!(validate_name(&exactly32).is_ok());
    }

    #[test]
    fn test_generate_config() {
        let config = generate_config(
            "cooper",
            InitProvider::Anthropic,
            "claude-sonnet-4-20250514",
            "alice",
            None,
            None,
        );
        assert!(config.contains("id = \"cooper\""));
        assert!(config.contains("model = \"anthropic/claude-sonnet-4-20250514\""));
        assert!(config.contains("name = \"alice\""));
        assert!(config.contains("workspace = \"./workspace\""));
        assert!(config.contains("[provider]"));
        assert!(config.contains("[memory]"));
    }

    #[test]
    fn test_generate_config_roundtrip() {
        let config_str = generate_config(
            "cooper",
            InitProvider::Anthropic,
            "claude-sonnet-4-20250514",
            "alice",
            None,
            None,
        );
        let config: crate::config::Config = toml::from_str(&config_str).unwrap();
        assert_eq!(config.agent.id, "cooper");
        assert_eq!(config.agent.model, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(config.agent.workspace, "./workspace");
        assert_eq!(config.users.len(), 1);
        assert_eq!(config.users[0].name, "alice");
        assert_eq!(config.users[0].trust, coop_core::TrustLevel::Full);
        assert_eq!(config.provider.name, "anthropic");
        assert_eq!(config.memory.db_path, "./db/memory.db");
    }

    #[test]
    fn test_generate_openai_compatible_config_roundtrip() {
        let config_str = generate_config(
            "cooper",
            InitProvider::OpenAiCompatible,
            "gpt-4o-mini",
            "alice",
            Some("http://localhost:8000/v1"),
            Some("OPENAI_COMPAT_API_KEY"),
        );
        let config: crate::config::Config = toml::from_str(&config_str).unwrap();
        assert_eq!(config.provider.name, "openai-compatible");
        assert_eq!(
            config.provider.base_url.as_deref(),
            Some("http://localhost:8000/v1")
        );
        assert_eq!(
            config.provider.api_key_env.as_deref(),
            Some("OPENAI_COMPAT_API_KEY")
        );
        assert_eq!(config.agent.model, "gpt-4o-mini");
    }

    #[test]
    fn test_scaffold_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let created = scaffold_workspace(dir.path(), "alice").unwrap();

        assert!(!created.is_empty());

        let ws = dir.path().join("workspace");
        assert!(ws.join("SOUL.md").exists());
        assert!(ws.join("IDENTITY.md").exists());
        assert!(ws.join("AGENTS.md").exists());
        assert!(ws.join("TOOLS.md").exists());
        assert!(ws.join("HEARTBEAT.md").exists());
        assert!(ws.join("BOOTSTRAP.md").exists());
        assert!(ws.join("users/alice/USER.md").exists());
        assert!(ws.join("channels/signal.md").exists());
        assert!(ws.join("sessions").is_dir());
        assert!(dir.path().join("db").is_dir());

        // Verify files have content
        let soul = std::fs::read_to_string(ws.join("SOUL.md")).unwrap();
        assert!(soul.contains("# Soul"));

        let bootstrap = std::fs::read_to_string(ws.join("BOOTSTRAP.md")).unwrap();
        assert!(bootstrap.contains("# Bootstrap"));
    }

    #[test]
    fn test_existing_config_detected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(&config_path, "[agent]\nid = \"test\"\nmodel = \"test\"\n").unwrap();
        assert!(config_path.exists());
    }

    #[test]
    fn test_expand_home() {
        let path = expand_home("/absolute/path");
        assert_eq!(path, PathBuf::from("/absolute/path"));

        let path = expand_home("relative/path");
        assert_eq!(path, PathBuf::from("relative/path"));

        if std::env::var("HOME").is_ok() {
            let path = expand_home("~/test");
            assert!(!path.to_string_lossy().contains('~'));
            assert!(path.to_string_lossy().ends_with("/test"));
        }
    }

    #[test]
    fn test_bootstrap_in_default_shared_files() {
        let config: crate::config::Config =
            toml::from_str("[agent]\nid = \"test\"\nmodel = \"test\"\n").unwrap();
        let has_bootstrap = config
            .prompt
            .shared_files
            .iter()
            .any(|f| f.path == "BOOTSTRAP.md");
        assert!(
            has_bootstrap,
            "BOOTSTRAP.md should be in default shared_files"
        );
    }
}
