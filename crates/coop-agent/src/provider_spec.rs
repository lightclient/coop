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
    pub api_keys: Vec<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
}

impl ProviderSpec {
    pub fn new(kind: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            kind,
            model: model.into(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: None,
            extra_headers: BTreeMap::new(),
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
}
