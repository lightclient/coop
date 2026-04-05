mod anthropic_provider;
mod codex_provider;
mod genai_provider;
mod image_prep;
mod key_pool;
mod message_mapping;
mod model_context;
mod model_mapping;
mod models_dev;
mod openai_codex;
mod openai_refresh;
mod provider_spec;
mod stream_mapping;
mod sync_http;
mod usage_mapping;

use anyhow::Result;
use std::sync::Arc;

use coop_core::Provider;

pub use anthropic_provider::AnthropicProvider;
pub use key_pool::{KeyPool, resolve_key_refs};
pub use provider_spec::{ProviderKind, ProviderSpec};

use codex_provider::CodexProvider;
use genai_provider::GenAiProvider;

pub fn create_provider(spec: ProviderSpec) -> Result<Arc<dyn Provider>> {
    match spec.kind {
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicProvider::from_spec(&spec)?)),
        ProviderKind::Gemini
        | ProviderKind::OpenAi
        | ProviderKind::OpenAiCompatible
        | ProviderKind::Ollama => {
            // If the OpenAI key is a Codex OAuth JWT, use the Codex provider.
            if spec.kind == ProviderKind::OpenAi && is_codex_oauth_token(&spec) {
                return Ok(Arc::new(CodexProvider::new(&spec)?));
            }
            Ok(Arc::new(GenAiProvider::new(spec)?))
        }
    }
}

fn is_codex_oauth_token(spec: &ProviderSpec) -> bool {
    let Ok(keys) = spec.resolved_api_keys() else {
        return false;
    };
    keys.first()
        .is_some_and(|key| openai_codex::extract_account_id(key).is_some())
}
