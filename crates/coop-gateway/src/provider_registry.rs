use coop_core::Provider;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of provider instances keyed by model name.
///
/// Each unique model gets its own provider instance. The primary model
/// is always present. Additional models are registered at startup from
/// config (e.g. group trigger models).
pub(crate) struct ProviderRegistry {
    primary: Arc<dyn Provider>,
    by_model: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub(crate) fn new(primary: Arc<dyn Provider>) -> Self {
        let model_name = primary.model_info().name;
        let mut by_model = HashMap::new();
        by_model.insert(model_name, Arc::clone(&primary));
        Self { primary, by_model }
    }

    pub(crate) fn register(&mut self, model: String, provider: Arc<dyn Provider>) {
        self.by_model.insert(model, provider);
    }

    pub(crate) fn primary(&self) -> &Arc<dyn Provider> {
        &self.primary
    }

    pub(crate) fn get(&self, model: &str) -> &Arc<dyn Provider> {
        self.by_model.get(model).unwrap_or(&self.primary)
    }

    #[allow(dead_code)]
    pub(crate) fn get_exact(&self, model: &str) -> Option<&Arc<dyn Provider>> {
        self.by_model.get(model)
    }

    pub(crate) fn sync_primary_model(&self, model: &str) {
        self.primary.set_model(model);
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::fakes::FakeProvider;

    fn make_provider(model: &str) -> Arc<dyn Provider> {
        Arc::new(FakeProvider::with_model("reply", model, 128_000))
    }

    #[test]
    fn new_registers_primary_in_by_model() {
        let registry = ProviderRegistry::new(make_provider("claude-sonnet"));
        assert!(registry.get_exact("claude-sonnet").is_some());
    }

    #[test]
    fn get_returns_primary_for_unknown() {
        let registry = ProviderRegistry::new(make_provider("claude-sonnet"));
        let p = registry.get("nonexistent-model");
        assert_eq!(p.model_info().name, "claude-sonnet");
    }

    #[test]
    fn get_exact_returns_none_for_unknown() {
        let registry = ProviderRegistry::new(make_provider("claude-sonnet"));
        assert!(registry.get_exact("nonexistent").is_none());
    }

    #[test]
    fn register_adds_model() {
        let mut registry = ProviderRegistry::new(make_provider("claude-sonnet"));
        registry.register("haiku".to_owned(), make_provider("haiku"));
        assert!(registry.get_exact("haiku").is_some());
        assert_eq!(registry.get("haiku").model_info().name, "haiku");
    }

    #[test]
    fn primary_returns_primary_provider() {
        let registry = ProviderRegistry::new(make_provider("claude-sonnet"));
        assert_eq!(registry.primary().model_info().name, "claude-sonnet");
    }

    #[test]
    fn multiple_registered_providers_independent() {
        let mut registry = ProviderRegistry::new(make_provider("main-model"));
        registry.register("model-a".to_owned(), make_provider("model-a"));
        registry.register("model-b".to_owned(), make_provider("model-b"));

        assert_eq!(registry.get("model-a").model_info().name, "model-a");
        assert_eq!(registry.get("model-b").model_info().name, "model-b");
        assert_eq!(registry.primary().model_info().name, "main-model");
    }
}
