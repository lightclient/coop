use std::collections::HashSet;

use crate::config::Config;

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

pub(crate) fn available_main_models(config: &Config) -> Vec<AvailableModel> {
    let builtins = builtin_models(&config.provider.normalized_name());
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

    push(
        &config.agent.model,
        builtin_description(builtins, &config.agent.model),
    );

    if config.provider.models.is_empty() {
        for entry in builtins {
            push(entry.id, Some(entry.description));
        }
    } else {
        for model in &config.provider.models {
            push(model, builtin_description(builtins, model));
        }
    }

    available
}

pub(crate) fn find_available_model(config: &Config, requested: &str) -> Option<AvailableModel> {
    let requested_key = normalize_model_key(requested);
    available_main_models(config)
        .into_iter()
        .find(|model| normalize_model_key(&model.id) == requested_key)
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
}
