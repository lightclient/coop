//! Goose-backed implementation of Coop's `Provider` trait.
//!
//! This wraps a `goose::providers::base::Provider` and converts between
//! Coop's types and Goose's types at the boundary.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use tracing::debug;

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Message, ModelInfo, ToolDef, Usage};
use goose::providers::base::Provider as GooseProviderTrait;

use crate::convert;

/// A Coop provider backed by Goose's provider infrastructure.
///
/// Constructed from a provider name and model name. Uses Goose's factory
/// to create the underlying provider, which handles auth, retry, and
/// streaming for 20+ LLM backends.
pub struct GooseProvider {
    inner: Arc<dyn GooseProviderTrait>,
    provider_name: String,
    model: ModelInfo,
}

impl GooseProvider {
    /// Create a new provider using Goose's factory.
    ///
    /// API keys should be set as environment variables before calling this
    /// (e.g., `ANTHROPIC_API_KEY`). Goose's config system reads env vars first.
    pub async fn new(provider_name: &str, model_name: &str) -> Result<Self> {
        debug!(provider = provider_name, model = model_name, "creating goose provider");

        let inner = goose::providers::create_with_named_model(provider_name, model_name)
            .await
            .with_context(|| {
                format!("failed to create provider {provider_name} with model {model_name}")
            })?;

        let model_config = inner.get_model_config();
        let model = convert::from_model_config(&model_config);

        Ok(Self {
            inner,
            provider_name: provider_name.to_string(),
            model,
        })
    }

    /// Session id for provider calls.
    /// Goose providers use this for session naming and logging.
    fn session_id() -> &'static str {
        "coop"
    }
}

impl std::fmt::Debug for GooseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GooseProvider")
            .field("provider", &self.provider_name)
            .field("model", &self.model.name)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for GooseProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let goose_messages = convert::to_goose_messages(messages);
        let mcp_tools = convert::to_mcp_tools(tools);

        let (response, provider_usage) = self
            .inner
            .complete(Self::session_id(), system, &goose_messages, &mcp_tools)
            .await
            .map_err(|e| anyhow::anyhow!("provider error: {e}"))?;

        let coop_message = convert::from_goose_message(&response);
        let usage = convert::from_goose_usage(&provider_usage);

        Ok((coop_message, usage))
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        let goose_messages = convert::to_goose_messages(messages);
        let mcp_tools = convert::to_mcp_tools(tools);

        let goose_stream = self
            .inner
            .stream(Self::session_id(), system, &goose_messages, &mcp_tools)
            .await
            .map_err(|e| anyhow::anyhow!("provider stream error: {e}"))?;

        // Map Goose's stream items to Coop's types
        let coop_stream = goose_stream.map(|item| {
            match item {
                Ok((maybe_msg, maybe_usage)) => {
                    let msg = maybe_msg.map(|m| convert::from_goose_message(&m));
                    let usage = maybe_usage.map(|u| convert::from_goose_usage(&u));
                    Ok((msg, usage))
                }
                Err(e) => Err(anyhow::anyhow!("provider stream error: {e}")),
            }
        });

        Ok(Box::pin(coop_stream))
    }

    async fn complete_fast(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let goose_messages = convert::to_goose_messages(messages);
        let mcp_tools = convert::to_mcp_tools(tools);

        let (response, provider_usage) = self
            .inner
            .complete_fast(Self::session_id(), system, &goose_messages, &mcp_tools)
            .await
            .map_err(|e| anyhow::anyhow!("provider error: {e}"))?;

        let coop_message = convert::from_goose_message(&response);
        let usage = convert::from_goose_usage(&provider_usage);

        Ok((coop_message, usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time test: GooseProvider implements Provider
    #[allow(dead_code)]
    const _: () = {
        fn assert_provider<T: Provider>() {}
        fn check() {
            assert_provider::<GooseProvider>();
        }
    };
}
