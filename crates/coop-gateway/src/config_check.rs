use std::collections::HashSet;
use std::path::Path;

use coop_core::TrustLevel;
use coop_core::prompt::{PromptBuilder, WorkspaceIndex};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CheckResult {
    pub name: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
}

#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    pub results: Vec<CheckResult>,
}

impl CheckReport {
    pub(crate) fn push(&mut self, result: CheckResult) {
        self.results.push(result);
    }

    pub(crate) fn has_errors(&self) -> bool {
        self.results
            .iter()
            .any(|r| r.severity == Severity::Error && !r.passed)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn has_warnings(&self) -> bool {
        self.results
            .iter()
            .any(|r| r.severity == Severity::Warning && !r.passed)
    }

    fn error_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == Severity::Error && !r.passed)
            .count()
    }

    fn warning_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == Severity::Warning && !r.passed)
            .count()
    }

    fn format_result(r: &CheckResult) -> String {
        let icon = if r.severity == Severity::Info {
            "·"
        } else if r.passed {
            "✓"
        } else {
            match r.severity {
                Severity::Warning => "⚠",
                _ => "✗",
            }
        };
        format!("{icon} {}", r.message)
    }

    pub(crate) fn to_summary_string(&self) -> String {
        let mut lines: Vec<String> = self.results.iter().map(Self::format_result).collect();
        let errors = self.error_count();
        let warnings = self.warning_count();
        if errors == 0 && warnings == 0 {
            lines.push("\nall checks passed".to_owned());
        } else {
            lines.push(format!("\n{errors} error(s), {warnings} warning(s)"));
        }
        lines.join("\n")
    }

    pub(crate) fn print_human(&self) {
        println!("{}", self.to_summary_string());
    }

    pub(crate) fn print_json(&self) {
        let value = self.to_json_value();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    }

    pub(crate) fn to_json_value(&self) -> serde_json::Value {
        let checks: Vec<serde_json::Value> = self
            .results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "severity": r.severity.as_str(),
                    "passed": r.passed,
                    "message": r.message,
                })
            })
            .collect();

        serde_json::json!({
            "passed": !self.has_errors(),
            "errors": self.error_count(),
            "warnings": self.warning_count(),
            "checks": checks,
        })
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn validate_config(config_path: &Path, config_dir: &Path) -> CheckReport {
    let mut report = CheckReport::default();

    // 1. toml_parse
    let config = match Config::load(config_path) {
        Ok(c) => {
            report.push(CheckResult {
                name: "toml_parse",
                severity: Severity::Error,
                passed: true,
                message: "config syntax valid".to_owned(),
            });
            c
        }
        Err(e) => {
            report.push(CheckResult {
                name: "toml_parse",
                severity: Severity::Error,
                passed: false,
                message: format!("{e:#}"),
            });
            return report;
        }
    };

    // 2. required_fields
    let fields_ok = !config.agent.id.is_empty() && !config.agent.model.is_empty();
    report.push(CheckResult {
        name: "required_fields",
        severity: Severity::Error,
        passed: fields_ok,
        message: if fields_ok {
            format!(
                "agent.id='{}', agent.model='{}'",
                config.agent.id, config.agent.model
            )
        } else {
            "agent.id and agent.model must be non-empty".to_owned()
        },
    });

    // 3. workspace_exists
    let workspace = match config.resolve_workspace(config_dir) {
        Ok(ws) => {
            report.push(CheckResult {
                name: "workspace_exists",
                severity: Severity::Error,
                passed: true,
                message: format!("workspace: {}", ws.display()),
            });
            Some(ws)
        }
        Err(e) => {
            report.push(CheckResult {
                name: "workspace_exists",
                severity: Severity::Error,
                passed: false,
                message: format!("{e}"),
            });
            None
        }
    };

    // 4. provider_known
    let provider_ok = config.provider.name == "anthropic";
    report.push(CheckResult {
        name: "provider_known",
        severity: Severity::Error,
        passed: provider_ok,
        message: if provider_ok {
            format!("provider: {}", config.provider.name)
        } else {
            format!(
                "unknown provider '{}' (only 'anthropic' supported)",
                config.provider.name
            )
        },
    });

    // 5. api_key_present
    if config.provider.api_keys.is_empty() {
        let api_key_ok = std::env::var("ANTHROPIC_API_KEY").is_ok();
        report.push(CheckResult {
            name: "api_key_present",
            severity: Severity::Error,
            passed: api_key_ok,
            message: if api_key_ok {
                "API key: present".to_owned()
            } else {
                "ANTHROPIC_API_KEY environment variable not set".to_owned()
            },
        });
    } else {
        let mut all_ok = true;

        for entry in &config.provider.api_keys {
            if let Some(var_name) = entry.strip_prefix("env:") {
                let is_set = std::env::var(var_name).is_ok();
                if !is_set {
                    report.push(CheckResult {
                        name: "api_key_present",
                        severity: Severity::Error,
                        passed: false,
                        message: format!(
                            "{var_name} environment variable not set (from api_keys entry '{entry}')"
                        ),
                    });
                    all_ok = false;
                }
            } else {
                report.push(CheckResult {
                    name: "api_key_present",
                    severity: Severity::Error,
                    passed: false,
                    message: format!(
                        "api_keys entry '{entry}' must use 'env:' prefix (e.g. env:ANTHROPIC_API_KEY)"
                    ),
                });
                all_ok = false;
            }
        }

        if all_ok {
            report.push(CheckResult {
                name: "api_key_present",
                severity: Severity::Info,
                passed: true,
                message: format!(
                    "API keys: {} configured (rotation enabled)",
                    config.provider.api_keys.len()
                ),
            });
        }
    }

    // 6. memory config
    check_memory(&mut report, &config, config_dir);

    // 6b. prompt config
    check_prompt(&mut report, &config);

    // 7-8 depend on workspace
    if let Some(ref ws) = workspace {
        check_workspace_files(&mut report, ws, &config);

        // bootstrap_pending
        let bootstrap_path = ws.join("BOOTSTRAP.md");
        if bootstrap_path.exists() {
            report.push(CheckResult {
                name: "bootstrap_pending",
                severity: Severity::Info,
                passed: false,
                message:
                    "Bootstrap conversation pending — run `coop chat` to personalize your agent."
                        .to_owned(),
            });
        }
    }

    // 9. users
    check_users(&mut report, &config);

    // 10. signal_channel
    if let Some(ref signal) = config.channels.signal {
        let db_path = crate::tui_helpers::resolve_config_path(config_dir, &signal.db_path);
        let exists = db_path.exists();
        report.push(CheckResult {
            name: "signal_channel",
            severity: Severity::Warning,
            passed: exists,
            message: if exists {
                format!("signal db: {}", db_path.display())
            } else {
                format!("signal db not found: {}", db_path.display())
            },
        });
    }

    // 11-13. cron checks
    check_cron(&mut report, &config);

    // 14. web tools config
    check_web_tools(&mut report, &config);

    // 15. binary_exists
    check_binary_exists(&mut report);

    report
}

#[allow(clippy::too_many_lines)]
fn check_memory(report: &mut CheckReport, config: &Config, config_dir: &Path) {
    let db_path = crate::tui_helpers::resolve_config_path(config_dir, &config.memory.db_path);
    let (passed, message) = match db_path.parent() {
        Some(parent) if parent.exists() && parent.is_dir() => {
            (true, format!("memory db: {}", db_path.display()))
        }
        Some(parent) if !parent.exists() => (
            true,
            format!(
                "memory db parent will be created on first start: {}",
                parent.display()
            ),
        ),
        Some(parent) => (
            false,
            format!("memory db parent is not a directory: {}", parent.display()),
        ),
        None => (
            false,
            format!("invalid memory db path: {}", db_path.display()),
        ),
    };

    report.push(CheckResult {
        name: "memory_db_path",
        severity: Severity::Error,
        passed,
        message,
    });

    let prompt_index = &config.memory.prompt_index;
    let prompt_index_valid = prompt_index.limit > 0
        && prompt_index.max_tokens > 0
        && prompt_index.recent_days > 0
        && prompt_index.recent_days <= 30;
    report.push(CheckResult {
        name: "memory_prompt_index",
        severity: Severity::Error,
        passed: prompt_index_valid,
        message: if prompt_index_valid {
            format!(
                "memory.prompt_index: enabled={}, include_file_links={}, limit={}, max_tokens={}, recent_days={}",
                prompt_index.enabled,
                prompt_index.include_file_links,
                prompt_index.limit,
                prompt_index.max_tokens,
                prompt_index.recent_days
            )
        } else {
            "memory.prompt_index requires limit > 0, max_tokens > 0, and recent_days in 1..=30"
                .to_owned()
        },
    });

    let auto_capture = &config.memory.auto_capture;
    let auto_capture_valid = auto_capture.min_turn_messages >= 1;
    report.push(CheckResult {
        name: "memory_auto_capture",
        severity: Severity::Error,
        passed: auto_capture_valid,
        message: if auto_capture_valid {
            format!(
                "memory.auto_capture: enabled={}, min_turn_messages={}",
                auto_capture.enabled, auto_capture.min_turn_messages
            )
        } else {
            "memory.auto_capture requires min_turn_messages >= 1".to_owned()
        },
    });

    let retention = &config.memory.retention;
    let retention_valid = retention.archive_after_days > 0
        && retention.delete_archive_after_days > 0
        && retention.compress_after_days > 0
        && retention.compression_min_cluster_size > 1
        && retention.max_rows_per_run > 0
        && retention.delete_archive_after_days >= retention.archive_after_days;

    report.push(CheckResult {
        name: "memory_retention",
        severity: Severity::Error,
        passed: retention_valid,
        message: if retention_valid {
            format!(
                "memory.retention: enabled={}, archive_after_days={}, delete_archive_after_days={}, compress_after_days={}, compression_min_cluster_size={}, max_rows_per_run={}",
                retention.enabled,
                retention.archive_after_days,
                retention.delete_archive_after_days,
                retention.compress_after_days,
                retention.compression_min_cluster_size,
                retention.max_rows_per_run,
            )
        } else {
            "memory.retention requires positive day values, compression_min_cluster_size > 1, max_rows_per_run > 0, and delete_archive_after_days >= archive_after_days"
                .to_owned()
        },
    });

    if let Some(embedding) = &config.memory.embedding {
        let provider = embedding.normalized_provider();
        let has_required_fields = !provider.is_empty()
            && !embedding.model.trim().is_empty()
            && embedding.dimensions > 0
            && embedding.dimensions <= 8_192
            && embedding.is_supported_provider();

        let provider_fields_valid = if provider == "openai-compatible" {
            embedding.base_url.as_ref().is_some_and(|value| {
                let trimmed = value.trim();
                !trimmed.is_empty()
                    && (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
            }) && embedding
                .api_key_env
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
        } else {
            true
        };

        let valid = has_required_fields && provider_fields_valid;

        report.push(CheckResult {
            name: "memory_embedding",
            severity: Severity::Error,
            passed: valid,
            message: if valid {
                format!(
                    "embedding: provider={}, model={}, dimensions={}",
                    provider, embedding.model, embedding.dimensions
                )
            } else {
                "memory.embedding requires provider in {openai,voyage,cohere,openai-compatible}, non-empty model, dimensions in 1..=8192, and openai-compatible base_url/api_key_env"
                    .to_owned()
            },
        });

        if valid {
            let env_var = embedding.required_api_key_env().unwrap_or_default();
            let api_key_present = !env_var.is_empty() && std::env::var(&env_var).is_ok();
            report.push(CheckResult {
                name: "memory_embedding_api_key",
                severity: Severity::Error,
                passed: api_key_present,
                message: if api_key_present {
                    format!("{env_var}: present")
                } else {
                    format!("{env_var} environment variable not set")
                },
            });
        }
    }
}

fn check_prompt(report: &mut CheckReport, config: &Config) {
    let all_entries: Vec<(&str, &crate::config::PromptFileEntry)> = config
        .prompt
        .shared_files
        .iter()
        .map(|e| ("shared", e))
        .chain(config.prompt.user_files.iter().map(|e| ("user", e)))
        .collect();

    let mut has_error = false;

    for (scope, entry) in &all_entries {
        if entry.path.is_empty() {
            report.push(CheckResult {
                name: "prompt_files",
                severity: Severity::Error,
                passed: false,
                message: format!("prompt.{scope}_files has an entry with empty path"),
            });
            has_error = true;
        }
        if entry.path.contains("..") || entry.path.starts_with('/') {
            report.push(CheckResult {
                name: "prompt_files",
                severity: Severity::Error,
                passed: false,
                message: format!(
                    "prompt.{scope}_files path '{}' must be relative (no '..' or absolute paths)",
                    entry.path
                ),
            });
            has_error = true;
        }
    }

    // Check for duplicates within each scope.
    let shared_paths: Vec<&str> = config
        .prompt
        .shared_files
        .iter()
        .map(|e| e.path.as_str())
        .collect();
    let mut seen = HashSet::new();
    for path in &shared_paths {
        if !seen.insert(*path) {
            report.push(CheckResult {
                name: "prompt_files",
                severity: Severity::Warning,
                passed: false,
                message: format!("duplicate path '{path}' in prompt.shared_files"),
            });
        }
    }

    let user_paths: Vec<&str> = config
        .prompt
        .user_files
        .iter()
        .map(|e| e.path.as_str())
        .collect();
    let mut seen = HashSet::new();
    for path in &user_paths {
        if !seen.insert(*path) {
            report.push(CheckResult {
                name: "prompt_files",
                severity: Severity::Warning,
                passed: false,
                message: format!("duplicate path '{path}' in prompt.user_files"),
            });
        }
    }

    if !has_error {
        report.push(CheckResult {
            name: "prompt_files",
            severity: Severity::Info,
            passed: true,
            message: format!(
                "prompt files: {} shared, {} per-user",
                config.prompt.shared_files.len(),
                config.prompt.user_files.len(),
            ),
        });
    }
}

fn check_workspace_files(report: &mut CheckReport, ws: &Path, config: &Config) {
    let shared_configs = config.prompt.shared_core_configs();
    let user_configs = config.prompt.user_core_configs();

    match WorkspaceIndex::scan(ws, &shared_configs) {
        Ok(index) => {
            let entries = index.entries_for_trust(TrustLevel::Full);
            let file_list = if entries.is_empty() {
                "no workspace files found".to_owned()
            } else {
                entries
                    .iter()
                    .map(|e| format!("{} ({} tok)", e.path, e.tokens))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            report.push(CheckResult {
                name: "workspace_files",
                severity: Severity::Info,
                passed: true,
                message: format!("workspace files: {file_list}"),
            });

            match PromptBuilder::new(ws.to_path_buf(), config.agent.id.clone())
                .trust(TrustLevel::Full)
                .file_configs(shared_configs)
                .user_file_configs(user_configs)
                .build(&index)
            {
                Ok(built) => {
                    report.push(CheckResult {
                        name: "prompt_builds",
                        severity: Severity::Warning,
                        passed: true,
                        message: format!("prompt: {} / 30000 tokens", built.total_tokens),
                    });
                }
                Err(e) => {
                    report.push(CheckResult {
                        name: "prompt_builds",
                        severity: Severity::Warning,
                        passed: false,
                        message: format!("prompt build failed: {e}"),
                    });
                }
            }
        }
        Err(e) => {
            report.push(CheckResult {
                name: "workspace_files",
                severity: Severity::Info,
                passed: true,
                message: format!("workspace scan failed: {e}"),
            });
        }
    }
}

fn check_users(report: &mut CheckReport, config: &Config) {
    if config.users.is_empty() {
        report.push(CheckResult {
            name: "users",
            severity: Severity::Info,
            passed: true,
            message: "no users configured".to_owned(),
        });
        return;
    }

    let names: Vec<&str> = config.users.iter().map(|u| u.name.as_str()).collect();
    report.push(CheckResult {
        name: "users",
        severity: Severity::Info,
        passed: true,
        message: format!("{} user(s): {}", names.len(), names.join(", ")),
    });

    let mut seen = HashSet::new();
    let dupes: Vec<&str> = names
        .iter()
        .filter(|n| !seen.insert(**n))
        .copied()
        .collect();
    if !dupes.is_empty() {
        report.push(CheckResult {
            name: "users_duplicates",
            severity: Severity::Warning,
            passed: false,
            message: format!("duplicate user names: {}", dupes.join(", ")),
        });
    }
}

fn check_cron(report: &mut CheckReport, config: &Config) {
    // 10. cron_expressions
    for entry in &config.cron {
        match crate::scheduler::parse_cron(&entry.cron) {
            Ok(_) => report.push(CheckResult {
                name: "cron_expressions",
                severity: Severity::Warning,
                passed: true,
                message: format!("cron '{}': valid", entry.name),
            }),
            Err(e) => report.push(CheckResult {
                name: "cron_expressions",
                severity: Severity::Warning,
                passed: false,
                message: format!("cron '{}': {e}", entry.name),
            }),
        }
    }

    // 11. cron_users
    for entry in &config.cron {
        if let Some(ref user) = entry.user {
            let exists = config.users.iter().any(|u| u.name == *user);
            if !exists {
                report.push(CheckResult {
                    name: "cron_users",
                    severity: Severity::Warning,
                    passed: false,
                    message: format!("cron '{}' references unknown user '{user}'", entry.name),
                });
            }
        }
    }

    // 12. cron_delivery
    for entry in &config.cron {
        if let Some(ref deliver) = entry.deliver
            && deliver.channel != "signal"
        {
            report.push(CheckResult {
                name: "cron_delivery",
                severity: Severity::Warning,
                passed: false,
                message: format!(
                    "cron '{}' delivery channel '{}' not supported (only 'signal')",
                    entry.name, deliver.channel
                ),
            });
        }
    }

    // 13. cron_user_no_deliverable_channels
    for entry in &config.cron {
        if entry.deliver.is_some() {
            continue;
        }
        if let Some(ref user_name) = entry.user
            && let Some(user) = config.users.iter().find(|u| u.name == *user_name)
        {
            let has_deliverable = user.r#match.iter().any(|pattern| {
                pattern
                    .split_once(':')
                    .is_some_and(|(channel, _)| channel != "terminal")
            });
            if !has_deliverable {
                report.push(CheckResult {
                    name: "cron_user_no_deliverable_channels",
                    severity: Severity::Warning,
                    passed: false,
                    message: format!(
                        "cron '{}' has user '{}' but no deliver override, \
                         and user has no non-terminal match patterns — \
                         heartbeat will have no delivery targets",
                        entry.name, user_name,
                    ),
                });
            }
        }
    }
}

fn check_web_tools(report: &mut CheckReport, config: &Config) {
    let web = &config.tools.web;

    if let Some(ref provider) = web.search.provider {
        let valid = matches!(provider.as_str(), "brave" | "perplexity" | "grok");
        report.push(CheckResult {
            name: "web_search_provider",
            severity: Severity::Error,
            passed: valid,
            message: if valid {
                format!("tools.web.search.provider: {provider}")
            } else {
                format!(
                    "tools.web.search.provider '{provider}' is invalid (must be brave, perplexity, or grok)"
                )
            },
        });
    }

    if let Some(timeout) = web.search.timeout_seconds
        && timeout == 0
    {
        report.push(CheckResult {
            name: "web_search_timeout",
            severity: Severity::Error,
            passed: false,
            message: "tools.web.search.timeout_seconds must be positive".to_owned(),
        });
    }

    if let Some(max_results) = web.search.max_results
        && !(1..=10).contains(&max_results)
    {
        report.push(CheckResult {
            name: "web_search_max_results",
            severity: Severity::Error,
            passed: false,
            message: "tools.web.search.max_results must be 1-10".to_owned(),
        });
    }

    if let Some(max_chars) = web.fetch.max_chars
        && max_chars < 100
    {
        report.push(CheckResult {
            name: "web_fetch_max_chars",
            severity: Severity::Error,
            passed: false,
            message: "tools.web.fetch.max_chars must be >= 100".to_owned(),
        });
    }

    if let Some(timeout) = web.fetch.timeout_seconds
        && timeout == 0
    {
        report.push(CheckResult {
            name: "web_fetch_timeout",
            severity: Severity::Error,
            passed: false,
            message: "tools.web.fetch.timeout_seconds must be positive".to_owned(),
        });
    }
}

fn check_binary_exists(report: &mut CheckReport) {
    match std::env::current_exe() {
        Ok(path) => {
            let path_str = path.to_string_lossy();
            let in_build_dir =
                path_str.contains("target/debug") || path_str.contains("target/release");
            let exists = path.exists();
            let passed = exists && !in_build_dir;

            let message = if !exists {
                format!("current executable does not exist: {}", path.display())
            } else if in_build_dir {
                format!(
                    "binary is in a build directory: {} (service may break if binary moves)",
                    path.display()
                )
            } else {
                format!("binary: {}", path.display())
            };

            report.push(CheckResult {
                name: "binary_exists",
                severity: Severity::Warning,
                passed,
                message,
            });
        }
        Err(error) => {
            report.push(CheckResult {
                name: "binary_exists",
                severity: Severity::Warning,
                passed: false,
                message: format!("failed to resolve current executable: {error}"),
            });
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn write_minimal_config(dir: &Path) -> std::path::PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    /// Filter out api_key_present errors (env-dependent, can't safely set in tests).
    fn non_env_errors(report: &CheckReport) -> Vec<&CheckResult> {
        report
            .results
            .iter()
            .filter(|r| {
                r.severity == Severity::Error
                    && !r.passed
                    && r.name != "api_key_present"
                    && r.name != "memory_embedding_api_key"
            })
            .collect()
    }

    #[test]
    fn test_valid_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_minimal_config(dir.path());

        let report = validate_config(&config_path, dir.path());
        let errors = non_env_errors(&report);
        assert!(errors.is_empty(), "expected no config errors: {errors:?}");
    }

    #[test]
    fn test_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(&config_path, "{{not valid toml").unwrap();

        let report = validate_config(&config_path, dir.path());
        let toml_check = report
            .results
            .iter()
            .find(|r| r.name == "toml_parse")
            .unwrap();
        assert!(!toml_check.passed);
        assert!(report.has_errors());
        assert_eq!(
            report.results.len(),
            1,
            "should return early after parse failure"
        );
    }

    #[test]
    fn test_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"./nonexistent\"\n",
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let ws_check = report
            .results
            .iter()
            .find(|r| r.name == "workspace_exists")
            .unwrap();
        assert!(!ws_check.passed);
    }

    #[test]
    fn test_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let provider_check = report
            .results
            .iter()
            .find(|r| r.name == "provider_known")
            .unwrap();
        assert!(!provider_check.passed);
        assert!(provider_check.message.contains("openai"));
    }

    #[test]
    fn test_invalid_memory_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory]\ndb_path = \"./db/memory.db\"\n\n[memory.embedding]\nprovider = \"\"\nmodel = \"\"\ndimensions = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let embedding_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_embedding")
            .unwrap();
        assert!(!embedding_check.passed);
    }

    #[test]
    fn test_embedding_dimension_upper_bound() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.embedding]\nprovider = \"openai\"\nmodel = \"text-embedding-3-small\"\ndimensions = 99999\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let embedding_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_embedding")
            .unwrap();
        assert!(!embedding_check.passed);
    }

    #[test]
    fn test_openai_compatible_embedding_requires_extra_fields() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.embedding]\nprovider = \"openai-compatible\"\nmodel = \"text-embedding-3-small\"\ndimensions = 1536\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let embedding_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_embedding")
            .unwrap();
        assert!(!embedding_check.passed);
    }

    #[test]
    fn test_memory_embedding_api_key_check() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory]\ndb_path = \"./db/memory.db\"\n\n[memory.embedding]\nprovider = \"openai\"\nmodel = \"text-embedding-3-small\"\ndimensions = 8\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let api_key_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_embedding_api_key")
            .unwrap();
        assert_eq!(
            api_key_check.passed,
            std::env::var("OPENAI_API_KEY").is_ok()
        );
    }

    #[test]
    fn test_invalid_memory_prompt_index() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.prompt_index]\nenabled = true\nlimit = 0\nmax_tokens = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let prompt_index_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_prompt_index")
            .unwrap();
        assert!(!prompt_index_check.passed);
    }

    #[test]
    fn test_memory_prompt_index_recent_days_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");

        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.prompt_index]\nrecent_days = 0\n",
                workspace.display()
            ),
        )
        .unwrap();
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "memory_prompt_index")
            .unwrap();
        assert!(!check.passed, "recent_days=0 should fail");

        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.prompt_index]\nrecent_days = 31\n",
                workspace.display()
            ),
        )
        .unwrap();
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "memory_prompt_index")
            .unwrap();
        assert!(!check.passed, "recent_days=31 should fail");

        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.prompt_index]\nrecent_days = 3\n",
                workspace.display()
            ),
        )
        .unwrap();
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "memory_prompt_index")
            .unwrap();
        assert!(check.passed, "recent_days=3 should pass");
    }

    #[test]
    fn test_invalid_memory_auto_capture() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.auto_capture]\nenabled = true\nmin_turn_messages = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "memory_auto_capture")
            .unwrap();
        assert!(!check.passed);
    }

    #[test]
    fn test_invalid_memory_retention() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[memory.retention]\nenabled = true\narchive_after_days = 30\ndelete_archive_after_days = 10\ncompress_after_days = 0\ncompression_min_cluster_size = 1\nmax_rows_per_run = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let retention_check = report
            .results
            .iter()
            .find(|r| r.name == "memory_retention")
            .unwrap();
        assert!(!retention_check.passed);
    }

    #[test]
    fn test_invalid_cron() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[cron]]\nname = \"bad\"\ncron = \"not a cron\"\nmessage = \"test\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let cron_check = report
            .results
            .iter()
            .find(|r| r.name == "cron_expressions" && !r.passed)
            .unwrap();
        assert!(!cron_check.passed);
        assert!(report.has_warnings());
    }

    #[test]
    fn test_cron_user_not_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[cron]]\nname = \"test\"\ncron = \"*/30 * * * *\"\nuser = \"mallory\"\nmessage = \"test\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let cron_users_check = report
            .results
            .iter()
            .find(|r| r.name == "cron_users")
            .unwrap();
        assert!(!cron_users_check.passed);
        assert!(cron_users_check.message.contains("mallory"));
    }

    #[test]
    fn test_cron_user_no_deliverable_channels_warns() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"terminal:default\"]\n\n[[cron]]\nname = \"heartbeat\"\ncron = \"*/30 * * * *\"\nuser = \"alice\"\nmessage = \"check\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "cron_user_no_deliverable_channels");
        assert!(check.is_some(), "should warn about no deliverable channels");
        assert!(!check.unwrap().passed);
        assert!(check.unwrap().message.contains("alice"));
    }

    #[test]
    fn test_cron_user_with_signal_channel_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"signal:alice-uuid\"]\n\n[[cron]]\nname = \"heartbeat\"\ncron = \"*/30 * * * *\"\nuser = \"alice\"\nmessage = \"check\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "cron_user_no_deliverable_channels");
        assert!(
            check.is_none(),
            "should not warn when user has signal channel"
        );
    }

    #[test]
    fn test_report_has_errors() {
        let mut report = CheckReport::default();
        assert!(!report.has_errors());
        assert!(!report.has_warnings());

        report.push(CheckResult {
            name: "test",
            severity: Severity::Warning,
            passed: false,
            message: "warning".to_owned(),
        });
        assert!(!report.has_errors());
        assert!(report.has_warnings());

        report.push(CheckResult {
            name: "test2",
            severity: Severity::Error,
            passed: false,
            message: "error".to_owned(),
        });
        assert!(report.has_errors());
    }

    #[test]
    fn test_report_json_output() {
        let mut report = CheckReport::default();
        report.push(CheckResult {
            name: "test_pass",
            severity: Severity::Error,
            passed: true,
            message: "ok".to_owned(),
        });
        report.push(CheckResult {
            name: "test_fail",
            severity: Severity::Warning,
            passed: false,
            message: "bad".to_owned(),
        });

        let json = report.to_json_value();
        assert_eq!(json["passed"], true);
        assert_eq!(json["errors"], 0);
        assert_eq!(json["warnings"], 1);
        assert_eq!(json["checks"].as_array().unwrap().len(), 2);
        assert_eq!(json["checks"][0]["name"], "test_pass");
        assert_eq!(json["checks"][1]["severity"], "warning");
    }

    #[test]
    fn test_prompt_files_default_passes() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_minimal_config(dir.path());

        let report = validate_config(&config_path, dir.path());
        let prompt_check = report
            .results
            .iter()
            .find(|r| r.name == "prompt_files" && r.severity == Severity::Info);
        assert!(
            prompt_check.is_some(),
            "should have prompt_files info check"
        );
        assert!(prompt_check.unwrap().passed);
        assert!(prompt_check.unwrap().message.contains("4 shared"));
    }

    #[test]
    fn test_prompt_files_empty_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[prompt.shared_files]]\npath = \"\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let prompt_check = report
            .results
            .iter()
            .find(|r| r.name == "prompt_files" && !r.passed);
        assert!(prompt_check.is_some(), "should reject empty path");
        assert!(prompt_check.unwrap().message.contains("empty path"));
    }

    #[test]
    fn test_prompt_files_absolute_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[prompt.shared_files]]\npath = \"/etc/passwd\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let prompt_check = report
            .results
            .iter()
            .find(|r| r.name == "prompt_files" && !r.passed);
        assert!(prompt_check.is_some(), "should reject absolute path");
    }

    #[test]
    fn test_prompt_files_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[prompt.user_files]]\npath = \"../../etc/passwd\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let prompt_check = report
            .results
            .iter()
            .find(|r| r.name == "prompt_files" && !r.passed);
        assert!(prompt_check.is_some(), "should reject path traversal");
    }

    #[test]
    fn test_prompt_files_duplicate_warns() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[prompt.shared_files]]\npath = \"SOUL.md\"\n\n[[prompt.shared_files]]\npath = \"SOUL.md\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let dup_check = report
            .results
            .iter()
            .find(|r| r.name == "prompt_files" && r.severity == Severity::Warning && !r.passed);
        assert!(dup_check.is_some(), "should warn about duplicate path");
        assert!(dup_check.unwrap().message.contains("duplicate"));
    }

    #[test]
    fn test_config_check_rejects_missing_env_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\napi_keys = [\"ANTHROPIC_API_KEY\"]\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "api_key_present" && !r.passed);
        assert!(check.is_some(), "should reject entry without env: prefix");
        assert!(check.unwrap().message.contains("env:"));
    }

    #[test]
    fn test_config_check_reports_all_missing_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\napi_keys = [\"env:COOP_TEST_MISSING_KEY_A\", \"env:COOP_TEST_MISSING_KEY_B\"]\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let missing: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.name == "api_key_present" && !r.passed)
            .collect();
        assert_eq!(missing.len(), 2, "should report both missing env vars");
        assert!(missing[0].message.contains("COOP_TEST_MISSING_KEY_A"));
        assert!(missing[1].message.contains("COOP_TEST_MISSING_KEY_B"));
    }

    #[test]
    fn test_config_check_api_keys_with_home_env() {
        // HOME is always set — use it to test the "all env vars set" path.
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\napi_keys = [\"env:HOME\"]\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "api_key_present" && r.passed);
        assert!(check.is_some(), "should pass when env var is set");
        assert!(check.unwrap().message.contains("1 configured"));
        assert!(check.unwrap().message.contains("rotation enabled"));
    }
}
