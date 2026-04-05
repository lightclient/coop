use anyhow::{Result, bail};
use coop_core::{Provider, SessionKey, TrustLevel};
use tracing::info;

use super::{Gateway, ModelSwitchOutcome, ResolvedMainModel};
use crate::compaction;
use crate::model_catalog::find_available_model;

struct ModelHandoffPlan<'a> {
    current_model: &'a str,
    current_provider: &'a dyn Provider,
    current_context_limit: usize,
    selected_model: &'a str,
    selected_context_limit: usize,
}

impl Gateway {
    pub(super) fn resolve_user_model_selection(
        &self,
        user_name: &str,
        requested_model: &str,
    ) -> Result<(String, String)> {
        let config = self.config.load();
        let selected = find_available_model(&config, requested_model).ok_or_else(|| {
            let available = crate::model_catalog::available_main_models(&config)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "unknown model '{requested_model}'. Use /models to list options. Available: {available}"
            )
        })?;
        let default_model = Self::configured_model_name_from_config(&config, Some(user_name));
        Ok((selected.id, default_model))
    }

    pub(super) fn persist_user_model_selection(
        &self,
        user_name: &str,
        selected_model: &str,
        default_model: &str,
    ) -> Result<()> {
        if Self::same_model(selected_model, default_model) {
            self.user_models.clear(user_name)?;
            info!(user = %user_name, model = %default_model, "cleared user model override");
        } else {
            self.user_models.set(user_name, selected_model)?;
            info!(user = %user_name, model = %selected_model, "updated user model override");
        }

        Ok(())
    }

    pub(crate) async fn set_user_model_for_session(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        requested_model: &str,
    ) -> Result<ModelSwitchOutcome> {
        let Some(user_name) = user_name else {
            bail!("model selection requires a named user");
        };

        let current_model = self.model_name_for_user(Some(user_name));
        let current_provider = self.main_provider_for_model(&current_model)?;
        let current_context_limit = current_provider.model_info().context_limit;
        let (selected_model, default_model) =
            self.resolve_user_model_selection(user_name, requested_model)?;
        let selected_capabilities = self.configured_model_capabilities(&selected_model);
        if selected_capabilities.subagent_only {
            bail!(
                "model '{selected_model}' is configured as subagent-only; use a subagent profile instead"
            );
        }
        if !selected_capabilities.supports_tools
            && self
                .messages(session_key)
                .iter()
                .any(|message| message.has_tool_requests() || message.has_tool_results())
        {
            bail!(
                "cannot switch this session to non-tool-capable model '{selected_model}' because the existing history contains tool calls; start a new session or use a subagent profile instead"
            );
        }
        let selected_provider = self.main_provider_for_model(&selected_model)?;
        let selected_context_limit = selected_provider.model_info().context_limit;

        let mut compacted_for_handoff = false;
        if !Self::same_model(&current_model, &selected_model)
            && selected_context_limit < current_context_limit
        {
            let session_lock = self.session_turn_lock(session_key);
            let Ok(_switch_guard) = session_lock.try_lock() else {
                bail!(
                    "cannot switch models while a turn is running on session '{session_key}'. Stop the turn first"
                );
            };

            compacted_for_handoff = self
                .compact_before_model_handoff(
                    session_key,
                    trust,
                    Some(user_name),
                    channel,
                    ModelHandoffPlan {
                        current_model: &current_model,
                        current_provider: current_provider.as_ref(),
                        current_context_limit,
                        selected_model: &selected_model,
                        selected_context_limit,
                    },
                )
                .await?;
        }

        self.persist_user_model_selection(user_name, &selected_model, &default_model)?;

        Ok(ModelSwitchOutcome {
            selection: ResolvedMainModel {
                model: selected_model,
                context_limit: selected_context_limit,
            },
            compacted_for_handoff,
        })
    }

    async fn compact_before_model_handoff(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        plan: ModelHandoffPlan<'_>,
    ) -> Result<bool> {
        let all_messages = self.messages(session_key);
        if all_messages.is_empty() {
            return Ok(false);
        }

        let previous_compaction = self.get_compaction(session_key);
        let context_messages = match &previous_compaction {
            Some((state, msg_count_before)) => {
                compaction::build_provider_context(&all_messages, Some(state), *msg_count_before)
            }
            None => all_messages.clone(),
        };

        let last_input_tokens = self.session_usage(session_key).last_input_tokens;
        let estimated_input_tokens = compaction::estimate_messages_tokens(&context_messages);
        let effective_input_tokens = last_input_tokens.max(estimated_input_tokens);

        if !compaction::should_compact(effective_input_tokens, plan.selected_context_limit) {
            return Ok(false);
        }

        if let Some((_, msg_count_at_compaction)) = &previous_compaction {
            let current_count = all_messages.len();
            if current_count <= *msg_count_at_compaction {
                info!(
                    session = %session_key,
                    old_model = %plan.current_model,
                    new_model = %plan.selected_model,
                    old_context_limit = plan.current_context_limit,
                    new_context_limit = plan.selected_context_limit,
                    last_input_tokens,
                    estimated_input_tokens,
                    "session already compacted for current history before model handoff"
                );
                return Ok(false);
            }
        }

        let system_prompt = self
            .build_prompt(
                session_key,
                trust,
                user_name,
                plan.current_model,
                channel,
                "",
                None,
            )
            .await?;
        let previous_state = previous_compaction.as_ref().map(|(state, _)| state);

        info!(
            session = %session_key,
            old_model = %plan.current_model,
            new_model = %plan.selected_model,
            old_context_limit = plan.current_context_limit,
            new_context_limit = plan.selected_context_limit,
            last_input_tokens,
            estimated_input_tokens,
            effective_input_tokens,
            message_count = all_messages.len(),
            is_iterative = previous_state.is_some(),
            "compacting session before lower-context model handoff"
        );

        let state = compaction::compact(
            &all_messages,
            plan.current_provider,
            &system_prompt,
            previous_state,
            compaction::DEFAULT_RECENT_CONTEXT_TARGET,
        )
        .await?;
        let cut_point = state.messages_at_compaction.unwrap_or(all_messages.len());

        info!(
            session = %session_key,
            old_model = %plan.current_model,
            new_model = %plan.selected_model,
            new_context_limit = plan.selected_context_limit,
            summary_len = state.summary.len(),
            compaction_count = state.compaction_count,
            cut_point,
            "session compacted before model handoff"
        );

        self.set_compaction(session_key, state, cut_point);
        Ok(true)
    }
}
