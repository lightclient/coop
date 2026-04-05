use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::resolve_key_refs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    OpenAiCompatible,
    Ollama,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
        }
    }

    pub fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::OpenAiCompatible | Self::Ollama => None,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAi),
            "openai-compatible" => Ok(Self::OpenAiCompatible),
            "ollama" => Ok(Self::Ollama),
            other => anyhow::bail!("unsupported provider '{other}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    pub kind: ProviderKind,
    pub model: String,
    pub default_model: Option<String>,
    pub default_model_context_limit: Option<usize>,
    pub model_context_limits: BTreeMap<String, usize>,
    pub api_keys: Vec<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub refresh_token: Option<String>,
}

impl ProviderSpec {
    pub fn new(kind: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            kind,
            model: model.into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: None,
            extra_headers: BTreeMap::new(),
            refresh_token: None,
        }
    }

    pub fn name(&self) -> &'static str {
        self.kind.as_str()
    }

    pub fn effective_api_key_env(&self) -> Option<&str> {
        self.api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| self.kind.default_api_key_env())
    }

    pub fn resolved_api_keys(&self) -> Result<Vec<String>> {
        if !self.api_keys.is_empty() {
            return resolve_key_refs(&self.api_keys);
        }

        let Some(env_name) = self.effective_api_key_env() else {
            return Ok(Vec::new());
        };

        let value = std::env::var(env_name)
            .with_context(|| format!("{env_name} environment variable not set"))?;
        Ok(vec![value])
    }

    pub fn normalized_base_url(&self) -> Option<String> {
        self.base_url.as_deref().map(normalize_base_url)
    }

    pub fn configured_context_limit(&self, model: &str) -> Option<usize> {
        self.configured_context_limit_for_models([model])
    }

    pub fn configured_context_limit_for_models<'a, I>(&self, models: I) -> Option<usize>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let requested = models
            .into_iter()
            .map(normalize_model_key)
            .collect::<Vec<_>>();

        if let (Some(default_model), Some(limit)) = (
            self.default_model.as_deref(),
            self.default_model_context_limit,
        ) {
            let default_key = normalize_model_key(default_model);
            if requested.iter().any(|candidate| candidate == &default_key) {
                return Some(limit);
            }
        }

        self.model_context_limits
            .iter()
            .find_map(|(candidate, limit)| {
                let candidate_key = normalize_model_key(candidate);
                requested
                    .iter()
                    .any(|requested_key| requested_key == &candidate_key)
                    .then_some(*limit)
            })
    }
}

fn normalize_model_key(model: &str) -> String {
    let trimmed = model.trim();
    for prefix in ["anthropic/", "openai/", "ollama/", "openai-compatible/"] {
        if let Some(stripped) = trimmed.strip_prefix(prefix) {
            return stripped.to_owned();
        }
    }
    trimmed.to_owned()
}

pub(crate) fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_adds_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://example.test/v1"),
            "https://example.test/v1/"
        );
        assert_eq!(
            normalize_base_url("https://example.test/v1/"),
            "https://example.test/v1/"
        );
    }

    #[test]
    fn openai_compatible_has_no_default_api_key_env() {
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompatible, "gpt-4o-mini");
        assert_eq!(spec.effective_api_key_env(), None);
    }

    #[test]
    fn anthropic_uses_default_api_key_env() {
        let spec = ProviderSpec::new(ProviderKind::Anthropic, "claude-sonnet-4-20250514");
        assert_eq!(spec.effective_api_key_env(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn configured_context_limit_matches_prefixed_and_unprefixed_models() {
        let mut spec = ProviderSpec::new(ProviderKind::OpenAi, "gpt-5.4");
        spec.model_context_limits
            .insert("gpt-5.4".to_owned(), 1_050_000);
        assert_eq!(
            spec.configured_context_limit("openai/gpt-5.4"),
            Some(1_050_000)
        );
    }

    #[test]
    fn default_model_context_limit_has_priority() {
        let mut spec = ProviderSpec::new(ProviderKind::OpenAi, "gpt-5.4");
        spec.default_model = Some("gpt-5.4".to_owned());
        spec.default_model_context_limit = Some(1_050_000);
        spec.model_context_limits
            .insert("gpt-5.4".to_owned(), 400_000);
        assert_eq!(spec.configured_context_limit("gpt-5.4"), Some(1_050_000));
    }
}
