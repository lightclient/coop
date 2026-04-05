use std::collections::BTreeSet;

use crate::config::{Config, ModelCapabilitiesConfig, ModelModality, ProviderConfig};
use crate::model_catalog::{
    normalize_model_key, resolve_configured_model, resolve_model_reference,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveModelCapabilities {
    pub supports_tools: bool,
    pub input_modalities: BTreeSet<ModelModality>,
    pub output_modalities: BTreeSet<ModelModality>,
    pub subagent_only: bool,
    pub hide_from_models: bool,
}

impl Default for EffectiveModelCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: true,
            input_modalities: BTreeSet::from([ModelModality::Text, ModelModality::Image]),
            output_modalities: BTreeSet::from([ModelModality::Text]),
            subagent_only: false,
            hide_from_models: false,
        }
    }
}

impl EffectiveModelCapabilities {
    pub(crate) fn supports_input(&self, modality: ModelModality) -> bool {
        self.input_modalities.contains(&modality)
    }

    pub(crate) fn supports_output(&self, modality: ModelModality) -> bool {
        self.output_modalities.contains(&modality)
    }

    pub(crate) fn visible_in_main_models(&self) -> bool {
        !self.subagent_only && !self.hide_from_models
    }
}

pub(crate) fn model_capabilities(
    config: &Config,
    model: &str,
) -> Option<EffectiveModelCapabilities> {
    let requested = resolve_model_reference(config, model);
    let resolved = resolve_configured_model(config, &requested.resolved)?;
    Some(provider_model_capabilities(
        resolved.provider,
        &resolved.model.id,
    ))
}

pub(crate) fn provider_model_capabilities(
    provider: &ProviderConfig,
    model: &str,
) -> EffectiveModelCapabilities {
    let model_key = normalize_model_key(model);
    let override_config = provider
        .model_capabilities
        .iter()
        .find(|(candidate, _)| normalize_model_key(candidate) == model_key)
        .map(|(_, config)| config);
    apply_capabilities(EffectiveModelCapabilities::default(), override_config)
}

fn apply_capabilities(
    mut base: EffectiveModelCapabilities,
    override_config: Option<&ModelCapabilitiesConfig>,
) -> EffectiveModelCapabilities {
    let Some(override_config) = override_config else {
        return base;
    };

    if let Some(supports_tools) = override_config.supports_tools {
        base.supports_tools = supports_tools;
    }
    if let Some(input_modalities) = override_config.input_modalities.as_ref() {
        base.input_modalities = input_modalities.iter().copied().collect();
    }
    if let Some(output_modalities) = override_config.output_modalities.as_ref() {
        base.output_modalities = output_modalities.iter().copied().collect();
    }
    if override_config.subagent_only {
        base.subagent_only = true;
    }
    if override_config.hide_from_models {
        base.hide_from_models = true;
    }

    base
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn default_capabilities_preserve_existing_behavior() {
        let caps = EffectiveModelCapabilities::default();
        assert!(caps.supports_tools);
        assert!(caps.supports_input(ModelModality::Text));
        assert!(caps.supports_input(ModelModality::Image));
        assert!(caps.supports_output(ModelModality::Text));
        assert!(!caps.supports_output(ModelModality::Image));
        assert!(caps.visible_in_main_models());
    }

    #[test]
    fn provider_override_can_disable_tools_and_hide_model() {
        let cfg = config(
            r#"
[agent]
id = "test"
model = "gpt-5.4"

[provider]
name = "openai"
models = ["gpt-5.4"]

[provider.model_capabilities."gpt-5.4"]
supports_tools = false
subagent_only = true
hide_from_models = true
input_modalities = ["text"]
output_modalities = ["text", "image"]
"#,
        );

        let caps = model_capabilities(&cfg, "gpt-5.4").unwrap();
        assert!(!caps.supports_tools);
        assert!(caps.supports_input(ModelModality::Text));
        assert!(!caps.supports_input(ModelModality::Image));
        assert!(caps.supports_output(ModelModality::Image));
        assert!(!caps.visible_in_main_models());
    }
}
