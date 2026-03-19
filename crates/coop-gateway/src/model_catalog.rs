use std::collections::HashSet;

use crate::config::{Config, ProviderConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelCatalogEntry {
    pub id: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvailableModel {
    pub id: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedAvailableModel<'a> {
    pub provider: &'a ProviderConfig,
    pub model: AvailableModel,
}

pub(crate) const ANTHROPIC_MODELS: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        id: "anthropic/claude-sonnet-4-20250514",
        description: "fast, recommended",
    },
    ModelCatalogEntry {
        id: "anthropic/claude-opus-4-0-20250514",
        description: "smartest, slower",
    },
    ModelCatalogEntry {
        id: "anthropic/claude-haiku-3-5-20241022",
        description: "cheapest, fastest",
    },
];

pub(crate) const OPENAI_MODELS: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        id: "gpt-4o-mini",
        description: "fast, recommended",
    },
    ModelCatalogEntry {
        id: "gpt-5-mini",
        description: "smart reasoning",
    },
    ModelCatalogEntry {
        id: "gpt-5-codex",
        description: "coding / responses API",
    },
];

pub(crate) const OLLAMA_MODELS: &[ModelCatalogEntry] = &[ModelCatalogEntry {
    id: "llama3.2",
    description: "default local model",
}];

const PREFIXES: &[&str] = &["anthropic/", "openai/", "ollama/", "openai-compatible/"];

pub(crate) fn normalize_model_key(model: &str) -> String {
    let trimmed = model.trim();
    for prefix in PREFIXES {
        if let Some(stripped) = trimmed.strip_prefix(prefix) {
            return stripped.to_owned();
        }
    }
    trimmed.to_owned()
}

pub(crate) fn builtin_models(provider_name: &str) -> &'static [ModelCatalogEntry] {
    match provider_name.trim().to_ascii_lowercase().as_str() {
        "anthropic" => ANTHROPIC_MODELS,
        "openai" => OPENAI_MODELS,
        "ollama" => OLLAMA_MODELS,
        _ => &[],
    }
}

pub(crate) fn provider_model_candidates(provider: &ProviderConfig) -> Vec<AvailableModel> {
    let builtins = builtin_models(&provider.normalized_name());
    let mut available = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |id: &str, description: Option<&str>| {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(normalize_model_key(trimmed)) {
            available.push(AvailableModel {
                id: trimmed.to_owned(),
                description: description.map(str::to_owned),
            });
        }
    };

    if provider.models.is_empty() {
        for entry in builtins {
            push(entry.id, Some(entry.description));
        }
    } else {
        for model in &provider.models {
            push(model, builtin_description(builtins, model));
        }
    }

    available
}

pub(crate) fn available_main_models(config: &Config) -> Vec<AvailableModel> {
    let mut available = Vec::new();
    let mut seen = HashSet::new();

    if let Some(resolved) = resolve_default_main_model(config) {
        let key = normalize_model_key(&resolved.model.id);
        seen.insert(key);
        available.push(resolved.model);
    }

    for provider in config.main_provider_configs() {
        for model in provider_model_candidates(provider) {
            let key = normalize_model_key(&model.id);
            if seen.insert(key) {
                available.push(model);
            }
        }
    }

    available
}

pub(crate) fn find_available_model(config: &Config, requested: &str) -> Option<AvailableModel> {
    resolve_available_model(config, requested).map(|resolved| resolved.model)
}

pub(crate) fn resolve_available_model<'a>(
    config: &'a Config,
    requested: &str,
) -> Option<ResolvedAvailableModel<'a>> {
    let requested_key = normalize_model_key(requested);
    let mut seen = HashSet::new();

    if let Some(resolved) = resolve_default_main_model(config) {
        let key = normalize_model_key(&resolved.model.id);
        if seen.insert(key.clone()) && key == requested_key {
            return Some(resolved);
        }
    }

    for provider in config.main_provider_configs() {
        for model in provider_model_candidates(provider) {
            let key = normalize_model_key(&model.id);
            if !seen.insert(key.clone()) {
                continue;
            }
            if key == requested_key {
                return Some(ResolvedAvailableModel { provider, model });
            }
        }
    }

    None
}

pub(crate) fn resolve_default_main_model(config: &Config) -> Option<ResolvedAvailableModel<'_>> {
    if config.providers.is_empty() {
        let builtins = builtin_models(&config.provider.normalized_name());
        return Some(ResolvedAvailableModel {
            provider: &config.provider,
            model: AvailableModel {
                id: config.agent.model.clone(),
                description: builtin_description(builtins, &config.agent.model).map(str::to_owned),
            },
        });
    }

    let default_key = normalize_model_key(&config.agent.model);
    for provider in &config.providers {
        if let Some(model) = provider_model_candidates(provider)
            .into_iter()
            .find(|model| normalize_model_key(&model.id) == default_key)
        {
            return Some(ResolvedAvailableModel { provider, model });
        }
    }

    None
}

fn builtin_description<'a>(builtins: &'a [ModelCatalogEntry], model: &str) -> Option<&'a str> {
    let key = normalize_model_key(model);
    builtins
        .iter()
        .find(|entry| normalize_model_key(entry.id) == key)
        .map(|entry| entry.description)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn normalize_model_key_strips_known_provider_prefixes() {
        assert_eq!(
            normalize_model_key("anthropic/claude-sonnet-4-20250514"),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(normalize_model_key("openai/gpt-5-mini"), "gpt-5-mini");
        assert_eq!(
            normalize_model_key("openai-compatible/meta-llama/Llama-3.3-70B-Instruct"),
            "meta-llama/Llama-3.3-70B-Instruct"
        );
    }

    #[test]
    fn available_main_models_falls_back_to_builtin_catalog() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "anthropic/claude-sonnet-4-20250514"

[provider]
name = "anthropic"
"#,
        );

        let models = available_main_models(&cfg);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(models[1].id, "anthropic/claude-opus-4-0-20250514");
        assert_eq!(models[2].id, "anthropic/claude-haiku-3-5-20241022");
    }

    #[test]
    fn available_main_models_uses_configured_provider_models_when_present() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "llama3.2"

[provider]
name = "ollama"
models = ["llama3.2", "qwen2.5-coder:14b"]
"#,
        );

        let models = available_main_models(&cfg);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3.2");
        assert_eq!(models[1].id, "qwen2.5-coder:14b");
    }

    #[test]
    fn available_main_models_keeps_default_model_even_if_not_in_configured_list() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "anthropic/custom-sonnet"

[provider]
name = "anthropic"
models = ["anthropic/claude-sonnet-4-20250514"]
"#,
        );

        let models = available_main_models(&cfg);
        assert_eq!(models[0].id, "anthropic/custom-sonnet");
        assert_eq!(models[1].id, "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn available_main_models_aggregates_multiple_providers() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "gpt-5-codex"

[[providers]]
name = "anthropic"
models = ["anthropic/claude-sonnet-4-20250514"]

[[providers]]
name = "openai"
models = ["gpt-5-codex", "gpt-5-mini"]

[[providers]]
name = "ollama"
models = ["llama3.2"]
"#,
        );

        let models = available_main_models(&cfg);
        assert_eq!(models[0].id, "gpt-5-codex");
        assert_eq!(models[1].id, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(models[2].id, "gpt-5-mini");
        assert_eq!(models[3].id, "llama3.2");
    }

    #[test]
    fn find_available_model_matches_prefixed_and_unprefixed_variants() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "anthropic/claude-sonnet-4-20250514"

[provider]
name = "anthropic"
"#,
        );

        let found = find_available_model(&cfg, "claude-sonnet-4-20250514").unwrap();
        assert_eq!(found.id, "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn resolve_available_model_returns_matching_provider() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "gpt-5-codex"

[[providers]]
name = "anthropic"
models = ["anthropic/claude-sonnet-4-20250514"]

[[providers]]
name = "openai"
models = ["gpt-5-codex"]
"#,
        );

        let resolved = resolve_available_model(&cfg, "gpt-5-codex").unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.model.id, "gpt-5-codex");
    }
}
