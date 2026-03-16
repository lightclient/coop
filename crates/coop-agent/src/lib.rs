mod anthropic_provider;
mod genai_provider;
mod image_prep;
mod key_pool;
mod message_mapping;
mod model_mapping;
mod provider_spec;
mod stream_mapping;
mod usage_mapping;

use anyhow::Result;
use std::sync::Arc;

use coop_core::Provider;

pub use anthropic_provider::AnthropicProvider;
pub use key_pool::{KeyPool, resolve_key_refs};
pub use provider_spec::{ProviderKind, ProviderSpec};

use genai_provider::GenAiProvider;

pub fn create_provider(spec: ProviderSpec) -> Result<Arc<dyn Provider>> {
    match spec.kind {
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicProvider::from_spec(&spec)?)),
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible | ProviderKind::Ollama => {
            Ok(Arc::new(GenAiProvider::new(spec)?))
        }
    }
}
