use std::collections::HashSet;
use std::path::Path;

use coop_core::TrustLevel;
use coop_core::prompt::{PromptBuilder, WorkspaceIndex};

use crate::config::{Config, ProviderConfig};
use crate::model_capabilities::{model_capabilities, provider_model_capabilities};
use crate::model_catalog::{
    normalize_model_key, provider_model_candidates, resolve_available_model,
    resolve_configured_model, resolve_default_main_model, resolve_model_reference,
};

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

    let agent_context_limit_ok = config.agent.context_limit.is_none_or(|limit| limit > 0);
    report.push(CheckResult {
        name: "agent_context_limit",
        severity: Severity::Error,
        passed: agent_context_limit_ok,
        message: if agent_context_limit_ok {
            config.agent.context_limit.map_or_else(
                || "agent.context_limit: auto-detect".to_owned(),
                |limit| format!("agent.context_limit: {limit}"),
            )
        } else {
            "agent.context_limit must be > 0 when set".to_owned()
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

    check_model_aliases(&mut report, &config);

    check_provider_config(&mut report, &config);

    // 6. memory config
    check_memory(&mut report, &config, config_dir);

    // 6b. prompt config
    check_prompt(&mut report, &config);

    // 6c. subagent config
    check_subagents(&mut report, &config);

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

    // 9. sandbox
    check_sandbox(&mut report, &config);

    // 10. users
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

    // 11. groups
    check_groups(&mut report, &config);

    // 12-14. cron checks
    check_cron(&mut report, &config);

    // 15. web tools config
    check_web_tools(&mut report, &config);

    // 15. binary_exists
    check_binary_exists(&mut report);

    report
}

#[allow(clippy::too_many_lines)]
fn check_provider_config(report: &mut CheckReport, config: &Config) {
    if config.providers.is_empty() {
        check_provider_entry(report, &config.provider, "provider");
        return;
    }

    report.push(CheckResult {
        name: "providers_configured",
        severity: Severity::Info,
        passed: true,
        message: format!("providers: {} configured", config.providers.len()),
    });

    for (index, provider) in config.providers.iter().enumerate() {
        check_provider_entry(report, provider, &format!("providers[{index}]"));
    }

    let mut seen = std::collections::HashMap::new();
    let mut duplicates = Vec::new();
    for (index, provider) in config.providers.iter().enumerate() {
        for model in provider_model_candidates(provider) {
            let key = normalize_model_key(&model.id);
            if key.is_empty() {
                continue;
            }
            let current = format!("providers[{index}] -> {}", model.id);
            if let Some(previous) = seen.insert(key, current.clone()) {
                duplicates.push(format!("{previous}, {current}"));
            }
        }
    }
    report.push(CheckResult {
        name: "providers_models_unique",
        severity: Severity::Error,
        passed: duplicates.is_empty(),
        message: if duplicates.is_empty() {
            "providers: all model ids are unique across providers".to_owned()
        } else {
            format!(
                "duplicate model ids across providers: {}",
                duplicates.join("; ")
            )
        },
    });

    let default_model = resolve_default_main_model(config);
    let default_reference = resolve_model_reference(config, &config.agent.model);
    report.push(CheckResult {
        name: "agent_model_provider",
        severity: Severity::Error,
        passed: default_model.is_some(),
        message: if let Some(resolved) = default_model {
            default_reference.alias.as_ref().map_or_else(
                || {
                    format!(
                        "agent.model '{}' resolved via provider '{}'",
                        resolved.model.id, resolved.provider.name
                    )
                },
                |alias| {
                    format!(
                        "agent.model alias '{}' -> '{}' via provider '{}'",
                        alias, resolved.model.id, resolved.provider.name
                    )
                },
            )
        } else {
            format!(
                "agent.model '{}' must appear in one configured provider's model list or built-in catalog",
                config.agent.model
            )
        },
    });

    let main_model_visible = model_capabilities(config, &config.agent.model)
        .is_some_and(|caps| caps.visible_in_main_models());
    report.push(CheckResult {
        name: "agent_model_main_safe",
        severity: Severity::Error,
        passed: main_model_visible,
        message: if main_model_visible {
            format!("agent.model '{}' is available for main sessions", config.agent.model)
        } else {
            format!(
                "agent.model '{}' is hidden or subagent-only; choose a main-session model and keep specialist models in subagent profiles",
                config.agent.model
            )
        },
    });
}

fn check_model_aliases(report: &mut CheckReport, config: &Config) {
    if config.models.aliases.is_empty() {
        return;
    }

    let mut alias_keys = std::collections::HashMap::new();
    let mut alias_errors = Vec::new();

    for (alias, target) in &config.models.aliases {
        let alias_key = normalize_model_key(alias);
        if alias_key.is_empty() {
            alias_errors.push("empty alias key".to_owned());
            continue;
        }
        if target.trim().is_empty() {
            alias_errors.push(format!("alias '{alias}' has an empty target"));
        }
        if let Some(previous) = alias_keys.insert(alias_key.clone(), alias.clone()) {
            alias_errors.push(format!(
                "duplicate normalized alias keys: '{previous}' and '{alias}'"
            ));
        }
    }

    let mut real_model_keys = HashSet::new();
    if let Some(default_model) = resolve_default_main_model(config) {
        real_model_keys.insert(normalize_model_key(&default_model.model.id));
    }
    for provider in config.main_provider_configs() {
        for model in provider_model_candidates(provider) {
            real_model_keys.insert(normalize_model_key(&model.id));
        }
    }

    for (alias, target) in &config.models.aliases {
        let alias_key = normalize_model_key(alias);
        if alias_key.is_empty() {
            continue;
        }

        if real_model_keys.contains(&alias_key) {
            alias_errors.push(format!(
                "alias '{alias}' collides with a configured model id"
            ));
        }

        let target_key = normalize_model_key(target);
        if alias_keys.contains_key(&target_key) {
            alias_errors.push(format!(
                "alias '{alias}' points to another alias target '{target}' — alias chaining is not supported"
            ));
            continue;
        }

        if resolve_configured_model(config, target).is_none() {
            alias_errors.push(format!(
                "alias '{alias}' target '{target}' does not resolve to a configured model"
            ));
        }
    }

    report.push(CheckResult {
        name: "model_aliases",
        severity: Severity::Error,
        passed: alias_errors.is_empty(),
        message: if alias_errors.is_empty() {
            format!("models.aliases: {} configured", config.models.aliases.len())
        } else {
            format!("models.aliases invalid: {}", alias_errors.join("; "))
        },
    });
}

#[allow(clippy::too_many_lines)]
fn check_provider_entry(report: &mut CheckReport, provider: &ProviderConfig, path: &str) {
    let provider_name = provider.normalized_name();
    let provider_ok = matches!(
        provider_name.as_str(),
        "anthropic" | "gemini" | "openai" | "openai-compatible" | "ollama"
    );
    report.push(CheckResult {
        name: "provider_known",
        severity: Severity::Error,
        passed: provider_ok,
        message: if provider_ok {
            format!("{path}.name: {}", provider.name)
        } else {
            format!(
                "{path}.name '{}' is unsupported (supported: anthropic, gemini, openai, openai-compatible, ollama)",
                provider.name
            )
        },
    });

    if provider_name == "openai-compatible" {
        let base_url_ok = provider.base_url.as_ref().is_some_and(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty()
                && (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        });
        report.push(CheckResult {
            name: "provider_base_url",
            severity: Severity::Error,
            passed: base_url_ok,
            message: if base_url_ok {
                format!(
                    "{path}.base_url: {}",
                    provider.base_url.as_deref().unwrap_or_default()
                )
            } else {
                format!("{path} requires base_url with http:// or https:// for openai-compatible")
            },
        });
    } else if let Some(base_url) = &provider.base_url {
        let valid = {
            let trimmed = base_url.trim();
            !trimmed.is_empty()
                && (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        };
        report.push(CheckResult {
            name: "provider_base_url",
            severity: Severity::Error,
            passed: valid,
            message: if valid {
                format!("{path}.base_url: {base_url}")
            } else {
                format!("{path}.base_url must start with http:// or https://")
            },
        });
    }

    if let Some(api_key_env) = &provider.api_key_env {
        let valid = !api_key_env.trim().is_empty();
        report.push(CheckResult {
            name: "provider_api_key_env",
            severity: Severity::Error,
            passed: valid,
            message: if valid {
                format!("{path}.api_key_env: {api_key_env}")
            } else {
                format!("{path}.api_key_env must not be empty")
            },
        });
    }

    report.push(CheckResult {
        name: "provider_stream_policy",
        severity: Severity::Info,
        passed: true,
        message: format!("{path}.stream_policy: {}", provider.stream_policy),
    });

    let reasoning_check = match provider.reasoning.as_ref() {
        None => CheckResult {
            name: "provider_reasoning",
            severity: Severity::Info,
            passed: true,
            message: format!("{path}.reasoning: not configured"),
        },
        Some(_reasoning) if provider_name != "openai" => CheckResult {
            name: "provider_reasoning",
            severity: Severity::Error,
            passed: false,
            message: format!("{path}.reasoning is only supported for openai providers"),
        },
        Some(reasoning) if reasoning.effort.is_none() => CheckResult {
            name: "provider_reasoning",
            severity: Severity::Error,
            passed: false,
            message: format!("{path}.reasoning.effort must be set when reasoning is configured"),
        },
        Some(reasoning) => CheckResult {
            name: "provider_reasoning",
            severity: Severity::Info,
            passed: true,
            message: format!(
                "{path}.reasoning: effort={}, summary={}",
                reasoning.effort.map_or("default", |value| value.as_str()),
                reasoning.summary.map_or("auto", |value| value.as_str())
            ),
        },
    };
    report.push(reasoning_check);

    let empty_models = provider.models.iter().any(|model| model.trim().is_empty());
    report.push(CheckResult {
        name: "provider_models_valid",
        severity: Severity::Error,
        passed: !empty_models,
        message: if empty_models {
            format!("{path}.models must not contain empty entries")
        } else if provider.models.is_empty() {
            format!("{path}.models: using built-in defaults")
        } else {
            format!("{path}.models: {} configured", provider.models.len())
        },
    });

    let mut capability_errors = Vec::new();
    let mut capability_keys = HashSet::new();
    for (model, caps) in &provider.model_capabilities {
        let key = normalize_model_key(model);
        if key.is_empty() {
            capability_errors.push("empty model capability key".to_owned());
            continue;
        }
        if !capability_keys.insert(key) {
            capability_errors.push(format!("duplicate normalized capability key: {model}"));
        }
        if caps.input_modalities.as_ref().is_some_and(Vec::is_empty) {
            capability_errors.push(format!("{model} input_modalities must not be empty"));
        }
        if caps.output_modalities.as_ref().is_some_and(Vec::is_empty) {
            capability_errors.push(format!("{model} output_modalities must not be empty"));
        }

        let effective = provider_model_capabilities(provider, model);
        if !effective.supports_input(crate::config::ModelModality::Text) {
            capability_errors.push(format!("{model} must support text input"));
        }
        if !effective.supports_output(crate::config::ModelModality::Text)
            && !effective.supports_output(crate::config::ModelModality::Image)
        {
            capability_errors.push(format!("{model} must support text or image output"));
        }
    }
    report.push(CheckResult {
        name: "provider_model_capabilities",
        severity: Severity::Error,
        passed: capability_errors.is_empty(),
        message: if capability_errors.is_empty() {
            format!(
                "{path}.model_capabilities: {} configured",
                provider.model_capabilities.len()
            )
        } else {
            format!(
                "{path}.model_capabilities contains invalid entries: {}",
                capability_errors.join(", ")
            )
        },
    });

    if !provider.models.is_empty() {
        let mut seen = HashSet::new();
        let mut duplicates = Vec::new();
        for model in &provider.models {
            let key = normalize_model_key(model);
            if !key.is_empty() && !seen.insert(key) {
                duplicates.push(model.clone());
            }
        }
        report.push(CheckResult {
            name: "provider_models_unique",
            severity: Severity::Warning,
            passed: duplicates.is_empty(),
            message: if duplicates.is_empty() {
                format!("{path}.models: no duplicates")
            } else {
                format!(
                    "{path}.models contains duplicates: {}",
                    duplicates.join(", ")
                )
            },
        });
    }

    let mut override_errors = Vec::new();
    let mut override_keys = HashSet::new();
    for (model, limit) in &provider.model_context_limits {
        let key = normalize_model_key(model);
        if key.is_empty() {
            override_errors.push("empty model key".to_owned());
            continue;
        }
        if *limit == 0 {
            override_errors.push(format!("{model} => 0"));
        }
        if !override_keys.insert(key) {
            override_errors.push(format!("duplicate normalized key: {model}"));
        }
    }
    report.push(CheckResult {
        name: "provider_model_context_limits",
        severity: Severity::Error,
        passed: override_errors.is_empty(),
        message: if override_errors.is_empty() {
            format!(
                "{path}.model_context_limits: {} configured",
                provider.model_context_limits.len()
            )
        } else {
            format!(
                "{path}.model_context_limits contains invalid entries: {}",
                override_errors.join(", ")
            )
        },
    });

    let mut all_env_refs_valid = true;
    for entry in &provider.api_keys {
        if let Some(var_name) = entry.strip_prefix("env:") {
            let is_set = std::env::var(var_name).is_ok();
            if !is_set {
                report.push(CheckResult {
                    name: "api_key_present",
                    severity: Severity::Error,
                    passed: false,
                    message: format!(
                        "{var_name} environment variable not set (from {path}.api_keys entry '{entry}')"
                    ),
                });
                all_env_refs_valid = false;
            }
        } else {
            report.push(CheckResult {
                name: "api_key_present",
                severity: Severity::Error,
                passed: false,
                message: format!(
                    "{path}.api_keys entry '{entry}' must use 'env:' prefix (e.g. env:ANTHROPIC_API_KEY)"
                ),
            });
            all_env_refs_valid = false;
        }
    }

    if !provider.api_keys.is_empty() {
        if all_env_refs_valid {
            report.push(CheckResult {
                name: "api_key_present",
                severity: Severity::Info,
                passed: true,
                message: format!(
                    "{path}: {} API keys configured (rotation enabled)",
                    provider.api_keys.len()
                ),
            });
        }
        return;
    }

    let effective_env = provider.effective_api_key_env();
    let key_required = matches!(provider_name.as_str(), "anthropic" | "gemini" | "openai")
        || provider.api_key_env.is_some();

    if let Some(env_name) = effective_env {
        let api_key_ok = std::env::var(&env_name).is_ok();
        report.push(CheckResult {
            name: "api_key_present",
            severity: if key_required {
                Severity::Error
            } else {
                Severity::Info
            },
            passed: api_key_ok || !key_required,
            message: if api_key_ok {
                format!("{path}: {env_name} present")
            } else if key_required {
                format!("{path}: {env_name} environment variable not set")
            } else {
                format!("{path}: {env_name} environment variable not set (optional)")
            },
        });
    } else {
        report.push(CheckResult {
            name: "api_key_present",
            severity: Severity::Info,
            passed: true,
            message: format!("{path}: provider authentication not required"),
        });
    }

    let mut header_error = false;
    for (name, value) in &provider.extra_headers {
        let valid = !name.trim().is_empty()
            && !name.contains(['\n', '\r'])
            && !value.contains(['\n', '\r']);
        if !valid {
            report.push(CheckResult {
                name: "provider_extra_headers",
                severity: Severity::Error,
                passed: false,
                message: format!("{path}.extra_headers contains invalid entry '{name}'"),
            });
            header_error = true;
        }
    }
    if !header_error && !provider.extra_headers.is_empty() {
        report.push(CheckResult {
            name: "provider_extra_headers",
            severity: Severity::Info,
            passed: true,
            message: format!(
                "{path}.extra_headers: {} configured",
                provider.extra_headers.len()
            ),
        });
    }
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

#[allow(clippy::too_many_lines)]
fn check_subagents(report: &mut CheckReport, config: &Config) {
    let subagents = &config.agent.subagents;
    let enabled = subagents.enabled;
    let valid = subagents.max_spawn_depth >= 1
        && subagents.max_active_children > 0
        && subagents.max_concurrent > 0
        && subagents.default_timeout_seconds > 0
        && subagents.default_max_turns > 0;

    report.push(CheckResult {
        name: "subagents_config",
        severity: Severity::Error,
        passed: valid,
        message: if valid {
            format!(
                "subagents: enabled={enabled}, max_spawn_depth={}, max_active_children={}, max_concurrent={}, default_timeout_seconds={}, default_max_turns={}",
                subagents.max_spawn_depth,
                subagents.max_active_children,
                subagents.max_concurrent,
                subagents.default_timeout_seconds,
                subagents.default_max_turns,
            )
        } else {
            "agent.subagents requires max_spawn_depth >= 1, max_active_children > 0, max_concurrent > 0, default_timeout_seconds > 0, and default_max_turns > 0".to_owned()
        },
    });

    if let Some(model) = &subagents.model {
        let model_ok = resolve_configured_model(config, model).is_some();
        report.push(CheckResult {
            name: "subagents_model",
            severity: Severity::Error,
            passed: model_ok,
            message: if model_ok {
                format!("agent.subagents.model '{model}' is available")
            } else {
                format!(
                    "agent.subagents.model '{model}' must appear in one configured provider's model list or built-in catalog"
                )
            },
        });
    }

    for (name, profile) in &subagents.profiles {
        let tools = profile.tools.as_deref().unwrap_or(&[]);
        let has_empty_tool = tools.iter().any(|tool| tool.trim().is_empty());
        let mut seen = HashSet::new();
        let duplicate_tools: Vec<&str> = tools
            .iter()
            .map(String::as_str)
            .filter(|tool| !seen.insert(*tool))
            .collect();
        let timeouts_ok = profile
            .default_timeout_seconds
            .is_none_or(|value| value > 0);
        let turns_ok = profile.default_max_turns.is_none_or(|value| value > 0);
        let profile_ok = !has_empty_tool && duplicate_tools.is_empty() && timeouts_ok && turns_ok;

        report.push(CheckResult {
            name: "subagents_profiles",
            severity: Severity::Error,
            passed: profile_ok,
            message: if profile_ok {
                format!(
                    "subagent profile '{name}': {} tools{}{}",
                    tools.len(),
                    profile
                        .default_timeout_seconds
                        .map_or_else(String::new, |value| format!(", timeout={value}")),
                    profile
                        .default_max_turns
                        .map_or_else(String::new, |value| format!(", max_turns={value}")),
                )
            } else if has_empty_tool {
                format!("subagent profile '{name}' contains an empty tool name")
            } else if !duplicate_tools.is_empty() {
                format!(
                    "subagent profile '{name}' contains duplicate tools: {}",
                    duplicate_tools.join(", ")
                )
            } else if !timeouts_ok {
                format!("subagent profile '{name}' requires default_timeout_seconds > 0 when set")
            } else {
                format!("subagent profile '{name}' requires default_max_turns > 0 when set")
            },
        });

        if let Some(model) = &profile.model {
            let model_ok = resolve_configured_model(config, model).is_some();
            report.push(CheckResult {
                name: "subagents_profile_model",
                severity: Severity::Error,
                passed: model_ok,
                message: if model_ok {
                    format!("subagent profile '{name}' model '{model}' is available")
                } else {
                    format!(
                        "subagent profile '{name}' model '{model}' must appear in one configured provider's model list or built-in catalog"
                    )
                },
            });
        }

        if profile.allow_spawn && subagents.max_spawn_depth < 2 {
            report.push(CheckResult {
                name: "subagents_profile_allow_spawn",
                severity: Severity::Warning,
                passed: false,
                message: format!(
                    "subagent profile '{name}' sets allow_spawn=true but max_spawn_depth={} prevents child subagents",
                    subagents.max_spawn_depth
                ),
            });
        }
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

#[allow(clippy::too_many_lines)]
fn check_sandbox(report: &mut CheckReport, config: &Config) {
    if !config.sandbox.enabled {
        return;
    }

    // Check sandbox platform availability
    match coop_sandbox::probe() {
        Ok(info) => {
            report.push(CheckResult {
                name: "sandbox_available",
                severity: Severity::Info,
                passed: true,
                message: format!("sandbox: {}", info.name),
            });

            if !info.capabilities.landlock {
                report.push(CheckResult {
                    name: "sandbox_available",
                    severity: Severity::Warning,
                    passed: false,
                    message: "landlock not available — filesystem isolation is degraded".to_owned(),
                });
            }
            if !info.capabilities.seccomp {
                report.push(CheckResult {
                    name: "sandbox_available",
                    severity: Severity::Warning,
                    passed: false,
                    message: "seccomp not available — syscall filtering disabled".to_owned(),
                });
            }
            if !info.capabilities.cgroups_v2 {
                report.push(CheckResult {
                    name: "sandbox_available",
                    severity: Severity::Warning,
                    passed: false,
                    message:
                        "cgroups v2 not writable — using setrlimit fallback for resource limits"
                            .to_owned(),
                });
            }
            if !info.capabilities.internet_only
                && config.sandbox.allow_network
                && config.users.iter().any(|u| u.trust > TrustLevel::Full)
            {
                report.push(CheckResult {
                    name: "sandbox_internet_only",
                    severity: Severity::Warning,
                    passed: false,
                    message: "pasta (passt) not available — users below Full trust will have no \
                         network instead of internet-only. Install: apt install passt"
                        .to_owned(),
                });
            }
        }
        Err(e) => {
            report.push(CheckResult {
                name: "sandbox_available",
                severity: Severity::Error,
                passed: false,
                message: format!("sandbox not available: {e}"),
            });
        }
    }

    // Validate memory format
    if coop_sandbox::parse_memory_size(&config.sandbox.memory).is_err() {
        report.push(CheckResult {
            name: "sandbox_memory",
            severity: Severity::Error,
            passed: false,
            message: format!(
                "sandbox.memory '{}' is not a valid size (use number with K/M/G suffix)",
                config.sandbox.memory
            ),
        });
    }

    // Validate pids_limit
    if config.sandbox.pids_limit == 0 {
        report.push(CheckResult {
            name: "sandbox_pids",
            severity: Severity::Error,
            passed: false,
            message: "sandbox.pids_limit must be > 0".to_owned(),
        });
    }

    // Check for multiple owners
    let owner_count = config
        .users
        .iter()
        .filter(|u| u.trust == TrustLevel::Owner)
        .count();
    if owner_count > 1 {
        report.push(CheckResult {
            name: "sandbox_multiple_owners",
            severity: Severity::Warning,
            passed: false,
            message: format!("{owner_count} users with trust=owner — there should be at most one"),
        });
    }

    // Check if sandbox is enabled but no owner exists
    let has_owner = owner_count > 0;
    if !has_owner {
        let terminal_user_exists = config
            .users
            .iter()
            .any(|u| u.r#match.iter().any(|m| m.starts_with("terminal")));
        if !terminal_user_exists {
            report.push(CheckResult {
                name: "sandbox_no_owner",
                severity: Severity::Info,
                passed: true,
                message: "no owner configured — terminal will default to owner trust when sandbox is enabled".to_owned(),
            });
        }
    }

    // Validate per-user sandbox overrides
    for user in &config.users {
        if let Some(ref overrides) = user.sandbox {
            if let Some(ref memory) = overrides.memory
                && coop_sandbox::parse_memory_size(memory).is_err()
            {
                report.push(CheckResult {
                    name: "sandbox_user_overrides",
                    severity: Severity::Error,
                    passed: false,
                    message: format!(
                        "user '{}' sandbox.memory '{}' is not a valid size",
                        user.name, memory
                    ),
                });
            }
            if let Some(pids) = overrides.pids_limit
                && pids == 0
            {
                report.push(CheckResult {
                    name: "sandbox_user_overrides",
                    severity: Severity::Error,
                    passed: false,
                    message: format!("user '{}' sandbox.pids_limit must be > 0", user.name),
                });
            }
        }
    }

    // Check unprivileged user namespaces on Linux
    #[cfg(target_os = "linux")]
    if let Ok(content) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        && content.trim() == "0"
    {
        report.push(CheckResult {
            name: "sandbox_user_namespaces",
            severity: Severity::Error,
            passed: false,
            message: "unprivileged user namespaces disabled (kernel.unprivileged_userns_clone=0)"
                .to_owned(),
        });
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

    for user in &config.users {
        if let Some(model) = &user.model {
            let resolved = resolve_available_model(config, model);
            let requested = resolve_model_reference(config, model);
            report.push(CheckResult {
                name: "users_models",
                severity: Severity::Error,
                passed: resolved.is_some(),
                message: if let Some(resolved) = resolved {
                    requested.alias.as_ref().map_or_else(
                        || {
                            format!(
                                "user '{}' model '{}' resolved via provider '{}'",
                                user.name, resolved.model.id, resolved.provider.name
                            )
                        },
                        |alias| {
                            format!(
                                "user '{}' model alias '{}' -> '{}' via provider '{}'",
                                user.name, alias, resolved.model.id, resolved.provider.name
                            )
                        },
                    )
                } else {
                    format!(
                        "user '{}' model '{}' must appear in one configured provider's model list or built-in catalog",
                        user.name, model
                    )
                },
            });
        }

        match crate::cron_timezone::resolve_user_timezone(user) {
            Ok(parsed) => report.push(CheckResult {
                name: "users_timezones",
                severity: Severity::Info,
                passed: true,
                message: format!("user '{}': timezone {}", user.name, parsed),
            }),
            Err(error) => report.push(CheckResult {
                name: "users_timezones",
                severity: Severity::Warning,
                passed: false,
                message: format!("user '{}': {error}", user.name),
            }),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn check_groups(report: &mut CheckReport, config: &Config) {
    use crate::config::{GroupTrigger, TrustCeiling};
    use std::collections::HashSet;

    if config.groups.is_empty() {
        return;
    }

    let mut seen_patterns: HashSet<String> = HashSet::new();

    for (i, group) in config.groups.iter().enumerate() {
        if group.r#match.is_empty() {
            report.push(CheckResult {
                name: "groups",
                severity: Severity::Error,
                passed: false,
                message: format!("groups[{i}]: match list is empty"),
            });
        }

        for pattern in &group.r#match {
            if pattern != "*" && !pattern.starts_with("signal:group:") {
                report.push(CheckResult {
                    name: "groups",
                    severity: Severity::Warning,
                    passed: false,
                    message: format!(
                        "groups[{i}]: match pattern '{pattern}' does not look like a group id \
                         (expected 'signal:group:<hex>' or '*')"
                    ),
                });
            }
            if !seen_patterns.insert(pattern.clone()) {
                report.push(CheckResult {
                    name: "groups",
                    severity: Severity::Warning,
                    passed: false,
                    message: format!("groups[{i}]: duplicate match pattern '{pattern}'"),
                });
            }
        }

        if group.trigger == GroupTrigger::Mention && group.mention_names.is_empty() {
            report.push(CheckResult {
                name: "groups",
                severity: Severity::Error,
                passed: false,
                message: format!("groups[{i}]: trigger='mention' requires non-empty mention_names"),
            });
        }

        if group.trigger == GroupTrigger::Regex {
            match &group.trigger_regex {
                None => {
                    report.push(CheckResult {
                        name: "groups",
                        severity: Severity::Error,
                        passed: false,
                        message: format!("groups[{i}]: trigger='regex' requires trigger_regex"),
                    });
                }
                Some(pattern) => {
                    if regex::Regex::new(pattern).is_err() {
                        report.push(CheckResult {
                            name: "groups",
                            severity: Severity::Error,
                            passed: false,
                            message: format!(
                                "groups[{i}]: trigger_regex '{pattern}' is not a valid regex"
                            ),
                        });
                    }
                }
            }
        }

        if group.trigger == GroupTrigger::Llm || group.trigger_model.is_some() {
            let trigger_model = group.trigger_model_or_default();
            let resolved = resolve_available_model(config, trigger_model);
            let requested = resolve_model_reference(config, trigger_model);
            report.push(CheckResult {
                name: "groups_trigger_models",
                severity: Severity::Error,
                passed: resolved.is_some(),
                message: if let Some(resolved) = resolved {
                    requested.alias.as_ref().map_or_else(
                        || {
                            format!(
                                "groups[{i}] trigger model '{}' resolved via provider '{}'",
                                resolved.model.id, resolved.provider.name
                            )
                        },
                        |alias| {
                            format!(
                                "groups[{i}] trigger model alias '{}' -> '{}' via provider '{}'",
                                alias, resolved.model.id, resolved.provider.name
                            )
                        },
                    )
                } else {
                    format!(
                        "groups[{i}] trigger model '{trigger_model}' must appear in one configured provider's model list or built-in catalog"
                    )
                },
            });
        }

        if group.default_trust == TrustLevel::Owner {
            report.push(CheckResult {
                name: "groups",
                severity: Severity::Warning,
                passed: false,
                message: format!(
                    "groups[{i}]: default_trust='owner' grants owner trust to unknown senders"
                ),
            });
        }

        if group.trust_ceiling == TrustCeiling::MinMember {
            report.push(CheckResult {
                name: "groups",
                severity: Severity::Error,
                passed: false,
                message: format!("groups[{i}]: trust_ceiling='min_member' is not supported yet"),
            });
        }
    }

    if config.channels.signal.is_none() {
        report.push(CheckResult {
            name: "groups",
            severity: Severity::Warning,
            passed: false,
            message: "groups configured but no signal channel is set up".to_owned(),
        });
    }

    // Summary if no errors
    let group_errors = report
        .results
        .iter()
        .any(|r| matches!(r.name, "groups" | "groups_trigger_models") && !r.passed);
    if !group_errors {
        report.push(CheckResult {
            name: "groups",
            severity: Severity::Info,
            passed: true,
            message: format!("{} group(s) configured", config.groups.len()),
        });
    }
}

#[allow(clippy::too_many_lines)]
fn check_cron(report: &mut CheckReport, config: &Config) {
    let mut seen = HashSet::new();
    let dupes: Vec<&str> = config
        .cron
        .iter()
        .map(|entry| entry.name.as_str())
        .filter(|name| !seen.insert(*name))
        .collect();
    if !dupes.is_empty() {
        report.push(CheckResult {
            name: "cron_names",
            severity: Severity::Error,
            passed: false,
            message: format!("duplicate cron names: {}", dupes.join(", ")),
        });
    }

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

    // 12. cron_timezones
    for entry in &config.cron {
        match crate::cron_timezone::resolve_cron_timezone(entry, &config.users) {
            Ok(timezone) => report.push(CheckResult {
                name: "cron_timezones",
                severity: Severity::Info,
                passed: true,
                message: format!("cron '{}': timezone {timezone}", entry.name),
            }),
            Err(error) => report.push(CheckResult {
                name: "cron_timezones",
                severity: Severity::Warning,
                passed: false,
                message: format!("cron '{}': {error}", entry.name),
            }),
        }
    }

    // 13. cron_delivery
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

    // 14. cron_user_no_deliverable_channels
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
                         cron will have no delivery targets",
                        entry.name, user_name,
                    ),
                });
            }
        }
    }

    // 15. cron_delivery_mode_legacy_heuristic
    for entry in &config.cron {
        if entry.uses_legacy_delivery_mode() && entry.message.contains("HEARTBEAT.md") {
            report.push(CheckResult {
                name: "cron_delivery_mode",
                severity: Severity::Warning,
                passed: false,
                message: format!(
                    "cron '{}' relies on the legacy HEARTBEAT.md heuristic for delivery mode; set delivery = \"as_needed\" explicitly",
                    entry.name
                ),
            });
        }
    }

    // 16. cron_message_contains_internal_delivery_tokens
    for entry in &config.cron {
        if entry.message.contains("HEARTBEAT_OK") || entry.message.contains("NO_ACTION_NEEDED") {
            report.push(CheckResult {
                name: "cron_message_tokens",
                severity: Severity::Warning,
                passed: false,
                message: format!(
                    "cron '{}' message embeds an internal delivery token; move that behavior to cron.delivery instead",
                    entry.name
                ),
            });
        }
    }

    // 17. cron_review_prompt_non_empty
    for entry in &config.cron {
        if let Some(review_prompt) = &entry.review_prompt
            && review_prompt.trim().is_empty()
        {
            report.push(CheckResult {
                name: "cron_review_prompt",
                severity: Severity::Error,
                passed: false,
                message: format!(
                    "cron '{}' review_prompt must be non-empty if set",
                    entry.name
                ),
            });
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
    fn test_invalid_agent_context_limit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\ncontext_limit = 0\nworkspace = \"{}\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "agent_context_limit")
            .unwrap();
        assert!(!check.passed);
    }

    #[test]
    fn test_invalid_provider_model_context_limit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gpt-5.4\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\n\n[provider.model_context_limits]\n\"gpt-5.4\" = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "provider_model_context_limits")
            .unwrap();
        assert!(!check.passed);
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
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"invalid-provider\"\n",
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
        assert!(provider_check.message.contains("invalid-provider"));
    }

    #[test]
    fn test_gemini_provider_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gemini-2.5-flash\"\nworkspace = \"{}\"\n\n[provider]\nname = \"gemini\"\napi_key_env = \"HOME\"\n",
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
        assert!(provider_check.passed, "expected gemini to be accepted");
    }

    #[test]
    fn test_multiple_providers_require_agent_model_to_match_one_provider() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"missing-model\"\nworkspace = \"{}\"\n\n[[providers]]\nname = \"anthropic\"\nmodels = [\"anthropic/claude-sonnet-4-20250514\"]\n\n[[providers]]\nname = \"openai\"\nmodels = [\"gpt-5-codex\"]\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let model_check = report
            .results
            .iter()
            .find(|r| r.name == "agent_model_provider")
            .unwrap();
        assert!(!model_check.passed);
        assert!(model_check.message.contains("missing-model"));
    }

    #[test]
    fn test_model_aliases_allow_agent_and_user_models() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"main\"\nworkspace = \"{}\"\n\n[models.aliases]\nmain = \"gpt-5-mini\"\ncodex = \"gpt-5-codex\"\n\n[[providers]]\nname = \"anthropic\"\nmodels = [\"anthropic/claude-sonnet-4-20250514\"]\n\n[[providers]]\nname = \"openai\"\nmodels = [\"gpt-5-mini\", \"gpt-5-codex\"]\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmodel = \"codex\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let alias_check = report
            .results
            .iter()
            .find(|r| r.name == "model_aliases")
            .unwrap();
        let agent_check = report
            .results
            .iter()
            .find(|r| r.name == "agent_model_provider")
            .unwrap();
        let user_check = report
            .results
            .iter()
            .find(|r| r.name == "users_models")
            .unwrap();

        assert!(alias_check.passed, "{alias_check:?}");
        assert!(agent_check.passed, "{agent_check:?}");
        assert!(agent_check.message.contains("alias 'main'"));
        assert!(user_check.passed, "{user_check:?}");
        assert!(user_check.message.contains("alias 'codex'"));
    }

    #[test]
    fn test_model_aliases_reject_alias_chains() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"main\"\nworkspace = \"{}\"\n\n[models.aliases]\nmain = \"gpt-5-mini\"\nfast = \"main\"\n\n[provider]\nname = \"openai\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let alias_check = report
            .results
            .iter()
            .find(|r| r.name == "model_aliases")
            .unwrap();

        assert!(!alias_check.passed);
        assert!(
            alias_check
                .message
                .contains("alias chaining is not supported")
        );
    }

    #[test]
    fn test_groups_trigger_model_accepts_alias() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"main\"\nworkspace = \"{}\"\n\n[models.aliases]\nmain = \"anthropic/claude-sonnet-4-20250514\"\nfast = \"anthropic/claude-haiku-3-5-20241022\"\n\n[channels.signal]\ndb_path = \"./db/signal.db\"\n\n[provider]\nname = \"anthropic\"\n\n[[groups]]\nmatch = [\"signal:group:deadbeef\"]\ntrigger = \"llm\"\ntrigger_model = \"fast\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let trigger_check = report
            .results
            .iter()
            .find(|r| r.name == "groups_trigger_models")
            .unwrap();

        assert!(trigger_check.passed, "{trigger_check:?}");
        assert!(trigger_check.message.contains("alias 'fast'"));
    }

    #[test]
    fn test_multiple_providers_reject_duplicate_model_ids() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gpt-5-codex\"\nworkspace = \"{}\"\n\n[[providers]]\nname = \"openai\"\nmodels = [\"gpt-5-codex\"]\n\n[[providers]]\nname = \"openai-compatible\"\nbase_url = \"http://localhost:8000/v1\"\nmodels = [\"gpt-5-codex\"]\napi_key_env = \"HOME\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let duplicate_check = report
            .results
            .iter()
            .find(|r| r.name == "providers_models_unique")
            .unwrap();
        assert!(!duplicate_check.passed);
        assert!(duplicate_check.message.contains("gpt-5-codex"));
    }

    #[test]
    fn test_user_model_must_match_available_model() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gpt-5-mini\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmodel = \"gpt-5-codex\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let model_check = report
            .results
            .iter()
            .find(|r| r.name == "users_models")
            .unwrap();
        assert!(model_check.passed);
        assert!(model_check.message.contains("alice"));
        assert!(model_check.message.contains("gpt-5-codex"));
    }

    #[test]
    fn test_user_model_rejects_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gpt-5-mini\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmodel = \"missing-model\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let model_check = report
            .results
            .iter()
            .find(|r| r.name == "users_models")
            .unwrap();
        assert!(!model_check.passed);
        assert!(model_check.message.contains("alice"));
        assert!(model_check.message.contains("missing-model"));
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
    fn test_duplicate_cron_names_error() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[cron]]\nname = \"heartbeat\"\ncron = \"*/30 * * * *\"\nmessage = \"check one\"\n\n[[cron]]\nname = \"heartbeat\"\ncron = \"0 8 * * *\"\nmessage = \"check two\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "cron_names" && !r.passed)
            .unwrap();
        assert!(check.message.contains("heartbeat"));
        assert!(report.has_errors());
    }

    #[test]
    fn test_invalid_user_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\ntimezone = \"Mars/Olympus_Mons\"\nmatch = [\"signal:alice-uuid\"]\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "users_timezones" && !r.passed)
            .unwrap();
        assert!(check.message.contains("alice"));
        assert!(check.message.contains("invalid timezone"));
    }

    #[test]
    fn test_invalid_cron_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[cron]]\nname = \"briefing\"\ncron = \"0 8 * * *\"\ntimezone = \"Mars/Olympus_Mons\"\nmessage = \"Morning briefing\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "cron_timezones" && !r.passed)
            .unwrap();
        assert!(check.message.contains("briefing"));
        assert!(check.message.contains("invalid timezone"));
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
    fn test_cron_review_prompt_must_be_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[[cron]]\nname = \"heartbeat\"\ncron = \"*/30 * * * *\"\ndelivery = \"as_needed\"\nreview_prompt = \"   \"\nmessage = \"check HEARTBEAT.md\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "cron_review_prompt")
            .unwrap();
        assert!(!check.passed);
        assert!(check.message.contains("review_prompt must be non-empty"));
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
        assert!(check.unwrap().message.contains("1 API keys configured"));
        assert!(check.unwrap().message.contains("rotation enabled"));
    }

    #[test]
    fn test_config_check_rejects_openai_reasoning_without_effort() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"gpt-5.4\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\nmodels = [\"gpt-5.4\"]\n\n[provider.reasoning]\nsummary = \"concise\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "provider_reasoning" && !r.passed);
        assert!(check.is_some(), "should reject reasoning without effort");
        assert!(
            check
                .unwrap()
                .message
                .contains("reasoning.effort must be set")
        );
    }

    #[test]
    fn test_config_check_rejects_reasoning_for_non_openai_provider() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"claude-sonnet-4-20250514\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[provider.reasoning]\neffort = \"high\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "provider_reasoning" && !r.passed);
        assert!(
            check.is_some(),
            "should reject reasoning for non-openai provider"
        );
        assert!(check.unwrap().message.contains("only supported for openai"));
    }

    fn write_config_with_groups(dir: &Path, groups_toml: &str) -> std::path::PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n{groups_toml}\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    #[test]
    fn test_valid_group_config_passes() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:deadbeef\"]\ntrigger = \"mention\"\nmention_names = [\"coop\"]\n",
        );
        let report = validate_config(&config_path, dir.path());
        let checks: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.name == "groups")
            .collect();
        // Should have the "groups configured but no signal channel" warning
        // and the summary, but no errors
        let errors: Vec<_> = checks
            .iter()
            .filter(|r| r.severity == Severity::Error && !r.passed)
            .collect();
        assert!(
            errors.is_empty(),
            "valid group should have no errors: {errors:?}"
        );
    }

    #[test]
    fn test_group_empty_match_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path =
            write_config_with_groups(dir.path(), "[[groups]]\nmatch = []\ntrigger = \"always\"\n");
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && !r.passed && r.message.contains("match list is empty"));
        assert!(check.is_some(), "empty match should fail");
    }

    #[test]
    fn test_group_mention_without_names_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"mention\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && !r.passed && r.message.contains("mention_names"));
        assert!(check.is_some(), "mention without names should fail");
    }

    #[test]
    fn test_group_regex_without_pattern_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"regex\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && !r.passed && r.message.contains("trigger_regex"));
        assert!(check.is_some(), "regex without pattern should fail");
    }

    #[test]
    fn test_group_invalid_regex_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"regex\"\ntrigger_regex = \"[invalid\"",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && !r.passed && r.message.contains("not a valid regex"));
        assert!(check.is_some(), "invalid regex should fail");
    }

    #[test]
    fn test_group_owner_default_trust_warns() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"always\"\ndefault_trust = \"owner\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report.results.iter().find(|r| {
            r.name == "groups"
                && r.severity == Severity::Warning
                && r.message.contains("owner trust")
        });
        assert!(check.is_some(), "owner default_trust should warn");
    }

    #[test]
    fn test_group_min_member_trust_ceiling_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"always\"\ntrust_ceiling = \"min_member\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report.results.iter().find(|r| {
            r.name == "groups" && r.severity == Severity::Error && r.message.contains("min_member")
        });
        assert!(check.is_some(), "min_member trust_ceiling should fail");
    }

    #[test]
    fn test_group_no_signal_channel_warns() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"always\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && r.message.contains("no signal channel"));
        assert!(check.is_some(), "groups without signal should warn");
    }

    #[test]
    fn test_group_duplicate_match_warns() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_groups(
            dir.path(),
            "[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"always\"\n\n[[groups]]\nmatch = [\"signal:group:aabb\"]\ntrigger = \"always\"\n",
        );
        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|r| r.name == "groups" && r.message.contains("duplicate"));
        assert!(check.is_some(), "duplicate match should warn");
    }

    #[test]
    fn test_invalid_subagent_defaults_fail_validation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[agent.subagents]\nmax_spawn_depth = 0\ndefault_timeout_seconds = 0\ndefault_max_turns = 0\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|result| result.name == "subagents_config")
            .unwrap();
        assert!(!check.passed);
    }

    #[test]
    fn test_unknown_subagent_profile_model_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test soul").unwrap();

        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[agent.subagents.profiles.code]\nmodel = \"missing-model\"\n",
                workspace.display()
            ),
        )
        .unwrap();

        let report = validate_config(&config_path, dir.path());
        let check = report
            .results
            .iter()
            .find(|result| result.name == "subagents_profile_model")
            .unwrap();
        assert!(!check.passed);
        assert!(check.message.contains("missing-model"));
    }
}
