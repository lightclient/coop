use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use std::sync::RwLock;
use tracing::{Instrument, debug, info_span};

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Message, ModelInfo, ToolDef, Usage};

use crate::model_context::{ContextLimitInput, resolve_context_limit};
use crate::openai_codex::{CodexRequest, OpenAiAuthMode, api_model_name, complete_codex};
use crate::openai_refresh::RefreshState;
use crate::provider_spec::ProviderSpec;

pub(crate) struct CodexProvider {
    client: Client,
    api_key: String,
    auth_mode: OpenAiAuthMode,
    refresh: Option<RefreshState>,
    model: RwLock<ModelInfo>,
}

impl CodexProvider {
    pub(crate) fn new(spec: &ProviderSpec) -> Result<Self> {
        let keys = spec.resolved_api_keys()?;
        anyhow::ensure!(!keys.is_empty(), "at least one OpenAI API key is required");
        let api_key = keys[0].clone();
        anyhow::ensure!(!api_key.trim().is_empty(), "OpenAI API key is empty");

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("failed to create HTTP client")?;

        let auth_mode = OpenAiAuthMode::detect(&api_key);
        let api_model = api_model_name(&spec.model, &auth_mode);
        let context_limit = resolve_context_limit(ContextLimitInput {
            kind: spec.kind,
            model: &api_model,
            base_url: Some("https://chatgpt.com/backend-api/codex/"),
            api_key: Some(&api_key),
            configured_limit: spec
                .configured_context_limit_for_models([spec.model.as_str(), api_model.as_str()]),
        });
        let model = ModelInfo {
            name: api_model,
            context_limit,
        };

        let refresh = spec
            .refresh_token
            .as_ref()
            .and_then(|ref_str| crate::resolve_key_refs(std::slice::from_ref(ref_str)).ok())
            .and_then(|mut tokens| tokens.pop())
            .and_then(|refresh_token| {
                if let OpenAiAuthMode::CodexOAuth { ref account_id } = auth_mode {
                    Some(RefreshState::new(
                        api_key.clone(),
                        account_id.clone(),
                        refresh_token,
                    ))
                } else {
                    None
                }
            });

        Ok(Self {
            client,
            api_key,
            auth_mode,
            refresh,
            model: RwLock::new(model),
        })
    }

    fn model_snapshot(&self) -> ModelInfo {
        self.model.read().expect("model lock poisoned").clone()
    }
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model = self.model_snapshot();
        f.debug_struct("CodexProvider")
            .field("model", &model.name)
            .field("auth_mode", &self.auth_mode.label())
            .field("has_refresh", &self.refresh.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model_info(&self) -> ModelInfo {
        self.model_snapshot()
    }

    fn set_model(&self, model: &str) {
        let api_model = api_model_name(model, &self.auth_mode);
        let mut info = self.model.write().expect("model lock poisoned");
        if info.name != api_model {
            debug!(old = %info.name, new = %api_model, "codex provider model updated");
            info.name = api_model;
        }
    }

    async fn complete(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let model_name = self.model_snapshot().name;
        let auth_mode = self.auth_mode.label();
        let has_refresh = self.refresh.is_some();
        let span = info_span!(
            "codex_request",
            model = %model_name,
            auth_mode,
            has_refresh,
            message_count = messages.len(),
            tool_count = tools.len(),
        );

        async {
            let (token, account_id) = match &self.auth_mode {
                OpenAiAuthMode::CodexOAuth { account_id } => {
                    if let Some(ref refresh) = self.refresh {
                        let fresh = refresh.ensure_fresh(&self.client).await?;
                        (fresh.access_token, fresh.account_id)
                    } else {
                        (self.api_key.clone(), account_id.clone())
                    }
                }
                OpenAiAuthMode::ApiKey => {
                    anyhow::bail!(
                        "CodexProvider requires a Codex OAuth token, got a regular API key"
                    );
                }
            };

            let (message, usage) = complete_codex(
                &self.client,
                &token,
                &account_id,
                CodexRequest {
                    model: &model_name,
                    system,
                    messages,
                    tools,
                },
            )
            .await?;

            debug!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
                "Codex response complete"
            );
            Ok((message, usage))
        }
        .instrument(span)
        .await
    }

    async fn stream(
        &self,
        _system: &[String],
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("Codex streaming is not implemented")
    }
}
