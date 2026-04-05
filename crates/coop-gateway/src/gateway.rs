#[path = "model_switch.rs"]
mod model_switch;
#[path = "request_metrics.rs"]
mod request_metrics;

use anyhow::{Result, bail};
use coop_core::prompt::{PromptBuilder, SkillEntry, WorkspaceIndex, scan_skills};
use coop_core::{
    Content, InboundMessage, Message, Provider, Role, SessionKey, SessionKind, ToolContext,
    ToolDef, ToolExecutor, TrustLevel, TurnConfig, TurnEvent, TurnResult, TypingNotifier, Usage,
};
use coop_memory::Memory;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use uuid::Uuid;

use self::request_metrics::estimate_provider_request_metrics;
use crate::compaction::{self, CompactionState};
use crate::compaction_store::CompactionStore;
use crate::config::{
    Config, CronDeliveryMode, SharedConfig, StreamPolicy, find_group_config_by_session,
};
use crate::cron_delivery;
use crate::final_reply::FinalReplyPolicy;
use crate::group_history::{GroupHistoryBuffer, GroupHistoryEntry};
use crate::group_trigger::{self, SILENT_REPLY_TOKEN};
use crate::memory_auto_capture;
use crate::memory_prompt_index;
use crate::model_capabilities::{EffectiveModelCapabilities, model_capabilities};
use crate::model_catalog::{
    AvailableModel, find_available_model, model_aliases_for, normalize_model_key,
    resolve_available_model, resolve_model_reference,
};
use crate::overflow_recovery;
use crate::provider_factory;
use crate::provider_registry::ProviderRegistry;
use crate::session_store::DiskSessionStore;
use crate::subagents::{SubagentManager, TurnOverrides};
use crate::user_model_store::UserModelStore;

pub(crate) struct Gateway {
    config: SharedConfig,
    workspace: PathBuf,
    workspace_index: Mutex<WorkspaceIndex>,
    skills: Mutex<Vec<SkillEntry>>,
    providers: ProviderRegistry,
    main_providers: Mutex<HashMap<String, Arc<dyn Provider>>>,
    user_models: UserModelStore,
    subagents: Arc<SubagentManager>,
    executor: Arc<dyn ToolExecutor>,
    memory: Option<Arc<dyn Memory>>,
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
    session_store: DiskSessionStore,
    compaction_store: CompactionStore,
    compaction_cache: Mutex<HashMap<SessionKey, (CompactionState, usize)>>,
    /// Per-session cumulative usage and last-turn input tokens (context size).
    session_usage: Mutex<HashMap<SessionKey, SessionUsage>>,
    /// Per-session cancellation tokens for in-progress turns.
    active_turns: Mutex<HashMap<SessionKey, CancellationToken>>,
    /// Per-session async mutexes to prevent concurrent turns on the same session.
    session_turn_locks: Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Per-group-session pending message buffer for non-triggering messages.
    group_history: Mutex<GroupHistoryBuffer>,
    /// Messages injected mid-turn, drained at the start of each iteration.
    pending_inbound: Mutex<HashMap<SessionKey, Vec<String>>>,
    /// Per-session conversation epoch — incremented on `/new` so the
    /// session search index can distinguish separate conversations
    /// within the same DM channel.
    session_epochs: Mutex<HashMap<SessionKey, u64>>,
}

/// Tracks cumulative token usage and context size for a session.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionUsage {
    pub cumulative: Usage,
    /// Input tokens from the last turn (approximates current context size).
    pub last_input_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedMainModel {
    pub model: String,
    pub context_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSwitchOutcome {
    pub selection: ResolvedMainModel,
    pub compacted_for_handoff: bool,
}

/// Re-send interval for typing indicators. Signal's client-side timeout is
/// ~10 s, so we refresh well within that window.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

struct TypingGuard {
    cancel: CancellationToken,
}

impl TypingGuard {
    fn new(notifier: Arc<dyn TypingNotifier>, session_key: SessionKey) -> Self {
        let cancel = CancellationToken::new();
        let child = cancel.child_token();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(TYPING_REFRESH_INTERVAL) => {
                        debug!(
                            session = %session_key,
                            "typing notifier refresh",
                        );
                        notifier.set_typing(&session_key, true).await;
                    }
                    () = child.cancelled() => {
                        emit_typing_notifier_event(&session_key, false);
                        notifier.set_typing(&session_key, false).await;
                        break;
                    }
                }
            }
        });

        Self { cancel }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn emit_typing_notifier_event(session_key: &SessionKey, started: bool) {
    let event_name = if started {
        "typing notifier start"
    } else {
        "typing notifier stop"
    };

    if let Some((target_kind, target)) = signal_target_from_session(session_key) {
        debug!(
            session = %session_key,
            signal.started = started,
            signal.target_kind = target_kind,
            signal.target = %target,
            "{event_name}"
        );
    } else {
        debug!(
            session = %session_key,
            signal.started = started,
            "{event_name}"
        );
    }
}

fn signal_target_from_session(session_key: &SessionKey) -> Option<(&'static str, String)> {
    match &session_key.kind {
        SessionKind::Dm(identity) => {
            let target = identity.strip_prefix("signal:").unwrap_or(identity);
            Some(("direct", target.to_owned()))
        }
        SessionKind::Group(group_id) => {
            let target = group_id.strip_prefix("signal:").unwrap_or(group_id);
            let target = if target.starts_with("group:") {
                target.to_owned()
            } else {
                format!("group:{target}")
            };
            Some(("group", target))
        }
        SessionKind::Main
        | SessionKind::Isolated(_)
        | SessionKind::Subagent(_)
        | SessionKind::Cron(_) => None,
    }
}

impl Gateway {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        config: SharedConfig,
        workspace: PathBuf,
        providers: ProviderRegistry,
        executor: Arc<dyn ToolExecutor>,
        typing_notifier: Option<Arc<dyn TypingNotifier>>,
        memory: Option<Arc<dyn Memory>>,
    ) -> Result<Self> {
        let subagents = Arc::new(SubagentManager::new(
            Arc::clone(&config),
            workspace.clone(),
        )?);
        Self::new_with_subagents(
            config,
            workspace,
            providers,
            executor,
            typing_notifier,
            memory,
            subagents,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_subagents(
        config: SharedConfig,
        workspace: PathBuf,
        providers: ProviderRegistry,
        executor: Arc<dyn ToolExecutor>,
        typing_notifier: Option<Arc<dyn TypingNotifier>>,
        memory: Option<Arc<dyn Memory>>,
        subagents: Arc<SubagentManager>,
    ) -> Result<Self> {
        let file_configs = config.load().prompt.shared_core_configs();
        let workspace_index = WorkspaceIndex::scan(&workspace, &file_configs)?;
        let skills = scan_skills(&workspace);
        if !skills.is_empty() {
            debug!(count = skills.len(), "loaded skills");
        }

        let session_store = DiskSessionStore::new(workspace.join("sessions"))?;
        let compaction_store = CompactionStore::new(workspace.join("sessions"))?;
        let user_models = UserModelStore::new(&workspace)?;
        let mut main_providers = HashMap::new();
        let config_snapshot = config.load();
        let default_reference =
            resolve_model_reference(&config_snapshot, &config_snapshot.agent.model);
        let default_model = find_available_model(&config_snapshot, &default_reference.resolved)
            .map(|model| model.id)
            .unwrap_or(default_reference.resolved);
        main_providers.insert(
            normalize_model_key(&default_model),
            Arc::clone(providers.primary()),
        );

        Ok(Self {
            config,
            workspace,
            workspace_index: Mutex::new(workspace_index),
            skills: Mutex::new(skills),
            providers,
            main_providers: Mutex::new(main_providers),
            user_models,
            subagents,
            executor,
            memory,
            typing_notifier,
            sessions: Mutex::new(HashMap::new()),
            session_store,
            compaction_store,
            compaction_cache: Mutex::new(HashMap::new()),
            session_usage: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashMap::new()),
            session_turn_locks: Mutex::new(HashMap::new()),
            group_history: Mutex::new(GroupHistoryBuffer::new()),
            pending_inbound: Mutex::new(HashMap::new()),
            session_epochs: Mutex::new(HashMap::new()),
        })
    }

    /// Build a trust-gated system prompt for this turn.
    ///
    /// Returns cache-friendly blocks: the stable prefix (workspace files,
    /// identity, tools) is separated from the volatile suffix (runtime
    /// context, memory index). This lets providers with prefix caching
    /// avoid full cache misses when only the volatile part changes.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn build_prompt(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
        model_name: &str,
        channel: Option<&str>,
        user_input: &str,
        cron_delivery_mode: Option<CronDeliveryMode>,
    ) -> Result<Vec<String>> {
        let cfg = self.config.load();
        let shared_configs = cfg.prompt.shared_core_configs();
        let user_configs = cfg.prompt.user_core_configs();
        let mut system_blocks = {
            let mut index = self
                .workspace_index
                .lock()
                .expect("workspace index mutex poisoned");
            let refreshed = index
                .refresh(&self.workspace, &shared_configs)
                .unwrap_or(false);
            if refreshed {
                debug!("workspace index refreshed");
            }

            let current_skills = scan_skills(&self.workspace);
            {
                let mut cached = self.skills.lock().expect("skills mutex poisoned");
                if cached.len() != current_skills.len()
                    || cached
                        .iter()
                        .zip(&current_skills)
                        .any(|(a, b)| a.name != b.name || a.path != b.path)
                {
                    let added: Vec<&str> = current_skills
                        .iter()
                        .filter(|s| !cached.iter().any(|c| c.name == s.name))
                        .map(|s| s.name.as_str())
                        .collect();
                    let removed: Vec<&str> = cached
                        .iter()
                        .filter(|c| !current_skills.iter().any(|s| s.name == c.name))
                        .map(|c| c.name.as_str())
                        .collect();
                    info!(
                        added = ?added,
                        removed = ?removed,
                        total = current_skills.len(),
                        "workspace skills changed"
                    );
                    cached.clone_from(&current_skills);
                }
            }

            let mut builder = PromptBuilder::new(self.workspace.clone(), cfg.agent.id.clone())
                .trust(trust)
                .session_kind(&session_key.kind)
                .model(model_name)
                .file_configs(shared_configs)
                .user_file_configs(user_configs)
                .skills(current_skills);
            if let Some(name) = user_name {
                builder = builder.user(name);
            }
            if let Some(ch) = channel {
                builder = builder.channel(ch);
            }
            let prompt = builder.build(&index)?;
            drop(index);
            prompt.to_cache_blocks()
        };

        if matches!(session_key.kind, SessionKind::Cron(_))
            && let Some(delivery_mode) = cron_delivery_mode
        {
            let block = build_cron_delivery_prompt_block(delivery_mode, channel);
            if let Some(first) = system_blocks.first_mut() {
                first.push_str("\n\n");
                first.push_str(&block);
            } else {
                system_blocks.push(block);
            }
        }

        if let Some(memory) = &self.memory {
            let cfg = self.config.load();
            match memory_prompt_index::build_prompt_index(
                memory.as_ref(),
                trust,
                &cfg.memory.prompt_index,
                user_input,
            )
            .await
            {
                Ok(Some(memory_index)) => {
                    debug!(
                        trust = ?trust,
                        index_len = memory_index.len(),
                        "memory prompt index injected"
                    );
                    // Append to the last (volatile) block, or add a new block.
                    if let Some(last) = system_blocks.last_mut() {
                        last.push_str("\n\n");
                        last.push_str(&memory_index);
                    } else {
                        system_blocks.push(memory_index);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        error = %error,
                        trust = ?trust,
                        "memory prompt index generation failed, continuing without index"
                    );
                }
            }
        }

        Ok(system_blocks)
    }

    pub(crate) fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.load().agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    pub(crate) fn list_sessions(&self) -> Vec<SessionKey> {
        let mut keys: Vec<_> = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .keys()
            .cloned()
            .collect();
        keys.push(self.default_session_key());
        keys.sort_by_cached_key(ToString::to_string);
        keys.dedup_by(|a, b| a.to_string() == b.to_string());
        keys
    }

    pub(crate) fn resolve_session(&self, session: &str) -> Option<SessionKey> {
        parse_session_key(session, &self.config.load().agent.id)
    }

    pub(crate) fn all_tool_defs(&self) -> Vec<ToolDef> {
        self.executor.tools()
    }

    pub(crate) fn subagents(&self) -> &Arc<SubagentManager> {
        &self.subagents
    }

    fn turn_workspace_scope(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
    ) -> coop_core::WorkspaceScope {
        coop_core::WorkspaceScope::for_turn(&self.workspace, &session_key.kind, trust, user_name)
    }

    fn tool_context(
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
        scope: &coop_core::WorkspaceScope,
        model_name: &str,
        tool_defs: &[ToolDef],
    ) -> ToolContext {
        let _ = scope.ensure_scope_root_exists();
        ToolContext::new(
            session_key.to_string(),
            session_key.kind.clone(),
            trust,
            scope.workspace_root(),
            user_name,
        )
        .with_model(model_name)
        .with_visible_tools(tool_defs.iter().map(|tool| tool.name.clone()))
    }

    fn model_capabilities_for(&self, model: &str) -> EffectiveModelCapabilities {
        model_capabilities(&self.config.load(), model).unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turn_with_options(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        cron_delivery_mode: Option<CronDeliveryMode>,
        overrides: TurnOverrides,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        self.run_turn_inner(
            session_key,
            user_input,
            trust,
            user_name,
            channel,
            cron_delivery_mode,
            overrides,
            event_tx,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turn_with_trust(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        self.run_turn_with_trust_and_cron_delivery(
            session_key,
            user_input,
            trust,
            user_name,
            channel,
            None,
            event_tx,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turn_with_trust_and_cron_delivery(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        cron_delivery_mode: Option<CronDeliveryMode>,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        self.run_turn_inner(
            session_key,
            user_input,
            trust,
            user_name,
            channel,
            cron_delivery_mode,
            TurnOverrides::default(),
            event_tx,
        )
        .await
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn run_turn_inner(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        cron_delivery_mode: Option<CronDeliveryMode>,
        overrides: TurnOverrides,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let span = info_span!(
            "agent_turn",
            session = %session_key,
            input_len = user_input.len(),
            user_input = user_input,
            model = tracing::field::Empty,
            trust = ?trust,
            user = ?user_name,
            channel = ?channel,
            cron.delivery_mode = ?cron_delivery_mode,
        );

        // Acquire per-session turn lock to prevent concurrent turns from
        // interleaving messages in the same session history (which corrupts
        // the tool_use/tool_result pairing the API requires).
        let session_lock = self.session_turn_lock(session_key);
        let Ok(_turn_guard) = session_lock.try_lock() else {
            warn!(
                session = %session_key,
                "skipping turn: another turn is already running on this session"
            );
            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: Vec::new(),
                    usage: Usage::default(),
                    hit_limit: false,
                }))
                .await;
            return Ok(());
        };

        // Register a cancellation token for this turn so `/stop` can cancel it.
        let turn_cancel = CancellationToken::new();
        self.active_turns
            .lock()
            .expect("active_turns mutex poisoned")
            .insert(session_key.clone(), turn_cancel.clone());

        let result = async {
            let _typing_guard = if let Some(notifier) = &self.typing_notifier {
                emit_typing_notifier_event(session_key, true);
                notifier.set_typing(session_key, true).await;
                Some(TypingGuard::new(Arc::clone(notifier), session_key.clone()))
            } else {
                None
            };

            let selected_model = overrides
                .model
                .clone()
                .unwrap_or_else(|| self.model_name_for_user(user_name));
            tracing::Span::current().record("model", tracing::field::display(&selected_model));
            let provider = self.main_provider_for_model(&selected_model)?;
            let selected_capabilities = self.model_capabilities_for(&selected_model);
            if selected_capabilities.subagent_only
                && !matches!(session_key.kind, SessionKind::Subagent(_))
            {
                bail!(
                    "model '{selected_model}' is configured as subagent-only; use a subagent profile instead"
                );
            }
            let context_limit = provider.model_info().context_limit;
            debug!(
                session = %session_key,
                user = ?user_name,
                model = %selected_model,
                context_limit,
                supports_tools = selected_capabilities.supports_tools,
                supports_image_input = selected_capabilities.supports_input(crate::config::ModelModality::Image),
                subagent_only = selected_capabilities.subagent_only,
                "resolved main model for turn"
            );

            // Cron sessions always start fresh — clear any previous run's
            // context. This happens after the turn lock to avoid a race where
            // a concurrent fire clears a session mid-turn.
            if matches!(session_key.kind, SessionKind::Cron(_)) {
                self.clear_session(session_key);
                debug!(session = %session_key, "cleared cron session for fresh execution");
            }

            let workspace_scope = self.turn_workspace_scope(session_key, trust, user_name);
            let mut system_prompt = if let Some(prompt_blocks) = overrides.prompt_blocks.clone() {
                prompt_blocks
            } else {
                self.build_prompt(
                    session_key,
                    trust,
                    user_name,
                    &selected_model,
                    channel,
                    user_input,
                    cron_delivery_mode,
                )
                .await?
            };

            // Inject group-specific intro when this is a group session.
            // Append to the last block to avoid exceeding the cache_control block limit.
            if let SessionKind::Group(_) = &session_key.kind {
                let cfg = self.config.load();
                if let Some(group_config) = find_group_config_by_session(session_key, &cfg) {
                    let intro =
                        build_group_intro(&group_config.trigger, &cfg.agent.id, &cfg.users);
                    if let Some(last) = system_prompt.last_mut() {
                        last.push_str("\n\n");
                        last.push_str(&intro);
                    } else {
                        system_prompt.push(intro);
                    }
                }
            }

            // Repair corrupt session state: if the last message is an assistant
            // with tool_use blocks but no following tool_result message, append
            // synthetic error results so the API doesn't reject the history.
            // This can happen when a previous turn panicked mid-tool-execution.
            self.repair_dangling_tool_use(session_key);

            // Compact before appending the new message if the session is
            // already over the threshold from a previous turn.
            self.maybe_compact(
                session_key,
                provider.as_ref(),
                context_limit,
                &system_prompt,
                &event_tx,
                "threshold",
                false,
            )
            .await?;

            let session_len_before = self.messages(session_key).len();
            if let Some(initial_message) = overrides.initial_message.clone() {
                self.append_message(session_key, initial_message);
            } else {
                self.append_message(session_key, Message::user().with_text(user_input));
            }

            let tool_defs = self.tool_defs_for_session(session_key);
            let tool_defs = if let Some(tool_names) = &overrides.tool_names {
                let allowed: HashSet<&str> = tool_names
                    .iter()
                    .map(String::as_str)
                    .collect::<HashSet<_>>();
                tool_defs
                    .into_iter()
                    .filter(|tool| allowed.contains(tool.name.as_str()))
                    .collect()
            } else {
                tool_defs
            };
            let tool_defs = if selected_capabilities.supports_tools {
                tool_defs
            } else {
                Vec::new()
            };
            let ctx = Self::tool_context(
                session_key,
                trust,
                user_name,
                &workspace_scope,
                &selected_model,
                &tool_defs,
            );
            let mut turn_config = TurnConfig::default();
            if let Some(max_iterations) = overrides.max_iterations {
                turn_config.max_iterations = max_iterations;
            }
            let final_reply_policy = FinalReplyPolicy::for_turn(channel, cron_delivery_mode);

            let mut total_usage = Usage::default();
            let mut new_messages = Vec::new();
            let mut hit_limit = false;

            for iteration in 0..turn_config.max_iterations {
                if turn_cancel.is_cancelled() {
                    info!("turn cancelled before iteration {iteration}");
                    break;
                }

                let iter_span = info_span!(
                    "turn_iteration",
                    iteration,
                    max = turn_config.max_iterations,
                );

                let iter_result: Result<(Message, bool)> = async {
                    // Drain any messages injected mid-turn before reading
                    // session state. This keeps them after the last
                    // tool_result and before the next provider call.
                    self.drain_pending_inbound(session_key);

                    let mut retried_after_compaction = false;

                    loop {
                        let all_messages = self.messages(session_key);
                        let compaction_state = self.get_compaction(session_key);
                        let messages = match &compaction_state {
                            Some((state, msg_count_before)) => compaction::build_provider_context(
                                &all_messages,
                                Some(state),
                                *msg_count_before,
                            ),
                            None => all_messages,
                        };
                        let messages = prepare_messages_for_provider(
                            &messages,
                            &workspace_scope,
                            &selected_capabilities,
                        );

                        match self
                            .assistant_response(
                                provider.as_ref(),
                                &selected_model,
                                &system_prompt,
                                &messages,
                                &tool_defs,
                                &event_tx,
                            )
                            .await
                        {
                            Ok((response, usage)) => {
                                if retried_after_compaction {
                                    info!(iteration, "overflow compaction retry succeeded");
                                }

                                self.update_last_input_tokens(session_key, &usage);
                                total_usage += usage;

                                let mut response = response;
                                if !response.has_tool_requests()
                                    && final_reply_policy.needs_repair(&response.text())
                                {
                                    let (repaired_response, repair_usage) = self
                                        .repair_terminal_reply(
                                            provider.as_ref(),
                                            &selected_model,
                                            &system_prompt,
                                            &messages,
                                            response.clone(),
                                            final_reply_policy,
                                        )
                                        .await;
                                    if repair_usage.context_input_tokens() > 0
                                        || repair_usage.output_tokens.unwrap_or(0) > 0
                                        || repair_usage.stop_reason.is_some()
                                    {
                                        self.update_last_input_tokens(session_key, &repair_usage);
                                        total_usage += repair_usage;
                                    }
                                    response = repaired_response;
                                }

                                self.append_message(session_key, response.clone());
                                new_messages.push(response.clone());

                                let has_tool_requests = response.has_tool_requests();
                                if !has_tool_requests {
                                    let _ = event_tx
                                        .send(TurnEvent::AssistantMessage(response.clone()))
                                        .await;
                                }

                                debug!(
                                    has_tool_requests,
                                    response_text_len = response.text().len(),
                                    "iteration complete"
                                );

                                break Ok((response, !has_tool_requests));
                            }
                            Err(err)
                                if !retried_after_compaction
                                    && overflow_recovery::should_force_compact_after_error(&err) =>
                            {
                                warn!(
                                    error = %err,
                                    iteration,
                                    "provider failed with overflow-like error; compacting and retrying iteration"
                                );

                                let compacted = self
                                    .maybe_compact(
                                        session_key,
                                        provider.as_ref(),
                                        context_limit,
                                        &system_prompt,
                                        &event_tx,
                                        "overflow",
                                        true,
                                    )
                                    .await?;
                                if !compacted {
                                    break Err(err);
                                }
                                retried_after_compaction = true;
                            }
                            Err(err) => break Err(err),
                        }
                    }
                }
                .instrument(iter_span)
                .await;

                let (response, should_break) = match iter_result {
                    Ok(result) => result,
                    Err(err) => {
                        let session_len_now = self.messages(session_key).len();
                        let rolled_back = session_len_now - session_len_before;
                        error!(
                            error = %err,
                            iteration,
                            trust = ?trust,
                            messages_rolled_back = rolled_back,
                            "provider request failed, rolling back session"
                        );
                        self.truncate_session(session_key, session_len_before);
                        let user_msg = if trust <= TrustLevel::Full {
                            format!("{err:#}")
                        } else {
                            "Something went wrong. Please try again later.".to_owned()
                        };
                        let _ = event_tx.send(TurnEvent::Error(user_msg)).await;
                        let _ = event_tx
                            .send(TurnEvent::Done(TurnResult {
                                messages: Vec::new(),
                                usage: total_usage,
                                hit_limit: false,
                            }))
                            .await;
                        return Ok(());
                    }
                };

                if should_break {
                    break;
                }

                let tool_requests = response.tool_requests();
                let mut result_msg = Message::user();
                let mut completed_tool_ids = HashSet::new();

                for req in &tool_requests {
                    if turn_cancel.is_cancelled() {
                        info!("turn cancelled before tool execution: {}", req.name);
                        break;
                    }

                    let _ = event_tx
                        .send(TurnEvent::ToolStart {
                            id: req.id.clone(),
                            name: req.name.clone(),
                            arguments: req.arguments.clone(),
                        })
                        .await;

                    let tool_span = info_span!(
                        "tool_execute",
                        tool.name = %req.name,
                        tool.id = %req.id,
                    );

                    let output = async {
                        debug!(arguments = %req.arguments, "tool arguments");
                        if !tool_defs.iter().any(|tool| tool.name == req.name) {
                            return coop_core::ToolOutput::error(format!(
                                "tool not available in this session: {}",
                                req.name
                            ));
                        }
                        match self
                            .executor
                            .execute(&req.name, req.arguments.clone(), &ctx)
                            .await
                        {
                            Ok(output) => {
                                debug!(
                                    output_len = output.content.len(),
                                    is_error = output.is_error,
                                    output_preview =
                                        &output.content[..output.content.floor_char_boundary(500)],
                                    "tool complete"
                                );
                                output
                            }
                            Err(err) => {
                                error!(tool = %req.name, error = %err, "tool execution failed");
                                coop_core::ToolOutput::error(format!("internal error: {err}"))
                            }
                        }
                    }
                    .instrument(tool_span)
                    .await;

                    completed_tool_ids.insert(req.id.clone());
                    result_msg =
                        result_msg.with_tool_result(&req.id, &output.content, output.is_error);

                    let _ = event_tx
                        .send(TurnEvent::ToolResult {
                            id: req.id.clone(),
                            message: Message::user().with_tool_result(
                                &req.id,
                                &output.content,
                                output.is_error,
                            ),
                        })
                        .await;
                }

                if turn_cancel.is_cancelled() {
                    for req in &tool_requests {
                        if completed_tool_ids.contains(&req.id) {
                            continue;
                        }

                        let output = coop_core::ToolOutput::error(
                            "tool execution was cancelled because the turn was stopped by the user",
                        );
                        result_msg =
                            result_msg.with_tool_result(&req.id, &output.content, output.is_error);

                        let _ = event_tx
                            .send(TurnEvent::ToolResult {
                                id: req.id.clone(),
                                message: Message::user().with_tool_result(
                                    &req.id,
                                    &output.content,
                                    output.is_error,
                                ),
                            })
                            .await;
                    }
                }

                if result_msg.has_tool_results() {
                    self.append_message(session_key, result_msg.clone());
                    new_messages.push(result_msg);
                }

                if turn_cancel.is_cancelled() {
                    info!("turn cancelled after tool execution");
                    break;
                }

                // Compact mid-turn if the context grew past the threshold
                // during this iteration. The next iteration will use the
                // compacted context automatically via build_provider_context.
                self.maybe_compact(
                    session_key,
                    provider.as_ref(),
                    context_limit,
                    &system_prompt,
                    &event_tx,
                    "threshold",
                    false,
                )
                .await?;

                if iteration + 1 >= turn_config.max_iterations {
                    hit_limit = true;
                }
            }

            // Drain any messages that were injected during the last
            // iteration's provider call. They won't get their own AI response
            // in this turn, but they'll be in the session for the next turn
            // rather than silently lost.
            self.drain_pending_inbound(session_key);

            let cancelled = turn_cancel.is_cancelled();

            // If we hit the iteration limit while the model still wanted to use
            // tools, inject a user message explaining the situation and do one
            // final provider call with no tools so the model is forced to
            // produce a text summary for the user.
            if hit_limit && !cancelled {
                let final_span = info_span!("turn_limit_completion");
                let final_result: Result<()> = async {
                    info!("forcing final completion (iteration limit reached)");

                    self.drain_pending_inbound(session_key);

                    let limit_msg = Message::user().with_text(
                        "You have reached the maximum number of tool-call iterations for this turn. \
                         You cannot call any more tools. Summarize what you accomplished, what is \
                         still incomplete, and what the user should know to continue.",
                    );
                    self.append_message(session_key, limit_msg.clone());
                    new_messages.push(limit_msg);

                    let all_messages = self.messages(session_key);
                    let compaction_state = self.get_compaction(session_key);
                    let messages = match &compaction_state {
                        Some((state, msg_count_before)) => compaction::build_provider_context(
                            &all_messages,
                            Some(state),
                            *msg_count_before,
                        ),
                        None => all_messages,
                    };
                    let messages = prepare_messages_for_provider(
                        &messages,
                        &workspace_scope,
                        &selected_capabilities,
                    );

                    let (response, usage) = self
                        .assistant_response(
                            provider.as_ref(),
                            &selected_model,
                            &system_prompt,
                            &messages,
                            &[],
                            &event_tx,
                        )
                        .await?;

                    self.update_last_input_tokens(session_key, &usage);
                    total_usage += usage;
                    self.append_message(session_key, response.clone());
                    new_messages.push(response.clone());

                    let _ = event_tx
                        .send(TurnEvent::AssistantMessage(response.clone()))
                        .await;

                    debug!(
                        response_text_len = response.text().len(),
                        "limit completion done"
                    );
                    Ok(())
                }
                .instrument(final_span)
                .await;

                if let Err(e) = final_result {
                    error!(error = %e, "limit completion failed");
                    let _ = event_tx
                        .send(TurnEvent::Error(
                            "Hit iteration limit and failed to generate summary.".to_owned(),
                        ))
                        .await;
                }
            }

            if cancelled {
                info!("turn stopped by user");
            } else {
                info!(
                    input_tokens = total_usage.input_tokens,
                    output_tokens = total_usage.output_tokens,
                    cache_read_tokens = total_usage.cache_read_tokens,
                    cache_write_tokens = total_usage.cache_write_tokens,
                    hit_limit,
                    "turn complete"
                );
            }

            // Track session-level cumulative usage.
            self.record_turn_usage(session_key, &total_usage);

            let post_turn_messages = new_messages.clone();
            let turn_result_messages = terminal_turn_messages(&new_messages);

            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: turn_result_messages,
                    usage: total_usage,
                    hit_limit,
                }))
                .await;

            if let Some(memory) = &self.memory {
                // Index turn messages for session search (FTS5).
                // Use the epoch-tagged key so /new resets create distinct
                // conversations in the search index.
                let memory_for_index = Arc::clone(memory);
                let index_session_key = self.search_index_key(session_key);
                let index_messages = post_turn_messages.clone();
                tokio::spawn(async move {
                    let session_msgs = crate::session_search::messages_to_session_messages(
                        &index_session_key,
                        &index_messages,
                    );
                    let count = session_msgs.len();
                    for msg in &session_msgs {
                        if let Err(error) = memory_for_index.index_session_message(msg).await {
                            warn!(
                                session = %index_session_key,
                                error = %error,
                                "failed to index session message"
                            );
                            break;
                        }
                    }
                    if count > 0 {
                        debug!(
                            session = %index_session_key,
                            indexed_count = count,
                            "session messages indexed for search"
                        );
                    }
                });

                let memory_for_summary = Arc::clone(memory);
                let summary_session_key = session_key.clone();
                tokio::spawn(async move {
                    match memory_for_summary.summarize_session(&summary_session_key).await {
                        Ok(summary) if summary.observation_count > 0 => {
                            debug!(
                                session = %summary_session_key,
                                observation_count = summary.observation_count,
                                "session summary written"
                            );
                        }
                        Ok(_) => {
                            debug!(
                                session = %summary_session_key,
                                "session summary skipped: no session observations"
                            );
                        }
                        Err(error) => {
                            warn!(
                                session = %summary_session_key,
                                error = %error,
                                "failed to write session summary"
                            );
                        }
                    }
                });

                let auto_capture = self.config.load().memory.auto_capture.clone();
                if auto_capture.enabled
                    && post_turn_messages.len() >= auto_capture.min_turn_messages
                {
                    let memory_for_capture = Arc::clone(memory);
                    let provider = Arc::clone(&provider);
                    let capture_session_key = session_key.clone();
                    let turn_messages = post_turn_messages.clone();
                    tokio::spawn(async move {
                        match memory_auto_capture::extract_turn_observations(
                            provider.as_ref(),
                            &turn_messages,
                            &capture_session_key,
                            trust,
                        )
                        .await
                        {
                            Ok(observations) if !observations.is_empty() => {
                                let extracted_count = observations.len();
                                let mut written_count = 0usize;

                                for observation in observations {
                                    match memory_for_capture.write(observation).await {
                                        Ok(_) => written_count += 1,
                                        Err(error) => {
                                            warn!(
                                                session = %capture_session_key,
                                                error = %error,
                                                "auto-capture memory write failed"
                                            );
                                        }
                                    }
                                }

                                debug!(
                                    session = %capture_session_key,
                                    extracted_count,
                                    written_count,
                                    "post-turn auto-capture complete"
                                );
                            }
                            Ok(_) => {
                                debug!(
                                    session = %capture_session_key,
                                    "post-turn auto-capture: nothing to extract"
                                );
                            }
                            Err(error) => {
                                warn!(
                                    session = %capture_session_key,
                                    error = %error,
                                    "post-turn auto-capture extraction failed"
                                );
                            }
                        }
                    });
                } else {
                    debug!(
                        session = %session_key,
                        enabled = auto_capture.enabled,
                        message_count = post_turn_messages.len(),
                        min_turn_messages = auto_capture.min_turn_messages,
                        "post-turn auto-capture skipped"
                    );
                }
            }

            Ok(())
        }
        .instrument(span)
        .await;

        // Deregister the cancellation token for this session.
        self.active_turns
            .lock()
            .expect("active_turns mutex poisoned")
            .remove(session_key);

        result
    }

    pub(crate) fn clear_session(&self, session_key: &SessionKey) {
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .remove(session_key);
        self.group_history
            .lock()
            .expect("group_history mutex poisoned")
            .clear(session_key);
        if let Err(e) = self.session_store.delete(session_key) {
            warn!(session = %session_key, error = %e, "failed to delete persisted session");
        }
        self.compaction_cache
            .lock()
            .expect("compaction cache mutex poisoned")
            .remove(session_key);
        if let Err(e) = self.compaction_store.delete(session_key) {
            warn!(session = %session_key, error = %e, "failed to delete compaction state");
        }
        self.session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .remove(session_key);

        // Bump the conversation epoch so session search can distinguish
        // conversations before vs after this /new reset.
        let new_epoch = self.bump_session_epoch(session_key);
        debug!(session = %session_key, epoch = new_epoch, "session epoch incremented");
    }

    /// Return a search-index key that includes the conversation epoch.
    ///
    /// For session key `agent:dm:signal:uuid` at epoch 2, returns
    /// `agent:dm:signal:uuid#2`. Epoch 0 (first conversation) omits the
    /// suffix for backward compatibility with messages indexed before the
    /// epoch system existed.
    pub(crate) fn search_index_key(&self, session_key: &SessionKey) -> String {
        let epoch = self.session_epoch(session_key);
        if epoch == 0 {
            session_key.to_string()
        } else {
            format!("{session_key}#{epoch}")
        }
    }

    fn session_epoch(&self, session_key: &SessionKey) -> u64 {
        self.session_epochs
            .lock()
            .expect("session_epochs mutex poisoned")
            .get(session_key)
            .copied()
            .unwrap_or(0)
    }

    #[allow(clippy::significant_drop_tightening)]
    fn bump_session_epoch(&self, session_key: &SessionKey) -> u64 {
        let mut epochs = self
            .session_epochs
            .lock()
            .expect("session_epochs mutex poisoned");
        let epoch = epochs.entry(session_key.clone()).or_insert(0);
        *epoch += 1;
        *epoch
    }

    /// Cancel the active turn for a session, if one is running.
    /// Returns `true` if a turn was cancelled.
    pub(crate) fn cancel_active_turn(&self, session_key: &SessionKey) -> bool {
        let cancelled_children = self.subagents.cancel_for_parent_session(session_key);
        let tokens = self
            .active_turns
            .lock()
            .expect("active_turns mutex poisoned");
        if let Some(token) = tokens.get(session_key) {
            token.cancel();
            info!(session = %session_key, "active turn cancelled via /stop");
            true
        } else {
            cancelled_children
        }
    }

    /// Returns `true` if a turn is currently running for this session.
    pub(crate) fn has_active_turn(&self, session_key: &SessionKey) -> bool {
        self.active_turns
            .lock()
            .expect("active_turns mutex poisoned")
            .contains_key(session_key)
    }

    /// Returns the per-session turn lock. Callers that need to append messages
    /// to a session without corrupting an in-progress turn (e.g. cron
    /// injections) should `.lock().await` this before writing.
    pub(crate) fn session_turn_lock(
        &self,
        session_key: &SessionKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .session_turn_locks
            .lock()
            .expect("session_turn_locks mutex poisoned");
        Arc::clone(
            locks
                .entry(session_key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    fn get_compaction(&self, session_key: &SessionKey) -> Option<(CompactionState, usize)> {
        {
            let cache = self
                .compaction_cache
                .lock()
                .expect("compaction cache mutex poisoned");

            if let Some(entry) = cache.get(session_key) {
                return Some(entry.clone());
            }
        }

        match self.compaction_store.load(session_key) {
            Ok(Some(state)) => {
                // Use the persisted message count if available; otherwise
                // fall back to current session length (legacy state files).
                let msg_count = state
                    .messages_at_compaction
                    .unwrap_or_else(|| self.messages(session_key).len());
                let entry = (state, msg_count);
                self.compaction_cache
                    .lock()
                    .expect("compaction cache mutex poisoned")
                    .insert(session_key.clone(), entry.clone());
                Some(entry)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(session = %session_key, error = %e, "failed to load compaction state");
                None
            }
        }
    }

    fn set_compaction(
        &self,
        session_key: &SessionKey,
        state: CompactionState,
        messages_before: usize,
    ) {
        if let Err(e) = self.compaction_store.save(session_key, &state) {
            warn!(session = %session_key, error = %e, "failed to persist compaction state");
        }
        self.compaction_cache
            .lock()
            .expect("compaction cache mutex poisoned")
            .insert(session_key.clone(), (state, messages_before));
    }

    /// Check if the session needs compaction and perform it if so.
    ///
    /// Uses the larger of:
    /// - `last_input_tokens` from session usage, i.e. the input token count
    ///   the provider reported on the most recent successful call
    /// - a fresh estimate from the current provider-context messages
    ///
    /// The estimate matters for cold-loaded sessions or newly selected
    /// lower-context models: persisted history may be large even when this
    /// process has not seen a successful provider call for the session yet.
    ///
    /// When `force` is true, compaction runs even if the threshold has not
    /// been reached yet. This mirrors pi's overflow recovery path: compact the
    /// current branch state, then retry the same prompt once.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn maybe_compact(
        &self,
        session_key: &SessionKey,
        provider: &dyn Provider,
        context_limit: usize,
        system_prompt: &[String],
        event_tx: &mpsc::Sender<TurnEvent>,
        reason: &'static str,
        force: bool,
    ) -> Result<bool> {
        let all_messages = self.messages(session_key);
        let previous_compaction = self.get_compaction(session_key);
        let context_messages = match &previous_compaction {
            Some((state, msg_count_before)) => {
                compaction::build_provider_context(&all_messages, Some(state), *msg_count_before)
            }
            None => all_messages.clone(),
        };

        let last_input_tokens = self
            .session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .get(session_key)
            .map_or(0, |u| u.last_input_tokens);
        let estimated_input_tokens = compaction::estimate_messages_tokens(&context_messages);
        let effective_input_tokens = last_input_tokens.max(estimated_input_tokens);
        let reserve_tokens = compaction::reserve_tokens(context_limit);
        let input_budget_tokens =
            u32::try_from(context_limit.saturating_sub(reserve_tokens)).unwrap_or(u32::MAX);

        if !force && !compaction::should_compact(effective_input_tokens, context_limit) {
            return Ok(false);
        }

        // If we already have a compaction state and no new messages have been
        // added since, there's nothing to re-compact.
        if let Some((_, msg_count_at_compaction)) = &previous_compaction {
            let current_count = all_messages.len();
            if current_count <= *msg_count_at_compaction {
                return Ok(false);
            }
        }

        let _ = event_tx.send(TurnEvent::Compacting).await;

        let msg_count = all_messages.len();
        let tool_defs = self.tool_defs_for_session(session_key);
        let mut previous_state = previous_compaction.map(|(state, _)| state);
        let mut recent_context_target = compaction::DEFAULT_RECENT_CONTEXT_TARGET;

        info!(
            session = %session_key,
            reason,
            force,
            last_input_tokens,
            estimated_input_tokens,
            effective_input_tokens,
            context_limit,
            reserve_tokens,
            message_count = msg_count,
            input_budget_tokens,
            is_iterative = previous_state.is_some(),
            "compaction triggered"
        );

        for pass in 0..MAX_COMPACTION_PASSES {
            match compaction::compact(
                &all_messages,
                provider,
                system_prompt,
                previous_state.as_ref(),
                recent_context_target,
            )
            .await
            {
                Ok(state) => {
                    let cut_point = state.messages_at_compaction.unwrap_or(msg_count);
                    let provider_context =
                        compaction::build_provider_context(&all_messages, Some(&state), cut_point);
                    let request_metrics = estimate_provider_request_metrics(
                        system_prompt,
                        &provider_context,
                        &tool_defs,
                    );
                    let estimated_request_tokens =
                        estimate_tokens_from_json_bytes(request_metrics.estimated_json_bytes);

                    info!(
                        session = %session_key,
                        reason,
                        force,
                        pass,
                        summary_len = state.summary.len(),
                        compaction_count = state.compaction_count,
                        files_tracked = state.files_touched.len(),
                        cut_point,
                        recent_context_target,
                        provider_message_count = provider_context.len(),
                        estimated_request_tokens,
                        request_estimated_bytes = request_metrics.estimated_json_bytes,
                        request_message_chars = request_metrics.message_chars,
                        request_system_chars = request_metrics.system_chars,
                        request_tool_schema_bytes = request_metrics.tool_schema_bytes,
                        "session compacted"
                    );

                    self.set_compaction(session_key, state.clone(), cut_point);

                    if estimated_request_tokens <= input_budget_tokens {
                        return Ok(true);
                    }

                    if pass + 1 >= MAX_COMPACTION_PASSES {
                        warn!(
                            session = %session_key,
                            reason,
                            force,
                            estimated_request_tokens,
                            input_budget_tokens,
                            "compaction exhausted without fitting request budget"
                        );
                        return Ok(true);
                    }

                    let next_target = shrink_recent_context_target(
                        recent_context_target,
                        estimated_request_tokens,
                        input_budget_tokens,
                    );

                    if next_target >= recent_context_target {
                        warn!(
                            session = %session_key,
                            reason,
                            force,
                            estimated_request_tokens,
                            input_budget_tokens,
                            recent_context_target,
                            "compaction target could not be reduced further"
                        );
                        return Ok(true);
                    }

                    previous_state = Some(state);
                    recent_context_target = next_target;
                }
                Err(e) => {
                    warn!(
                        session = %session_key,
                        reason,
                        force,
                        error = %e,
                        "compaction failed, continuing with full context"
                    );
                    return Ok(false);
                }
            }
        }

        Ok(false)
    }

    fn tool_defs_for_session(&self, session_key: &SessionKey) -> Vec<ToolDef> {
        let tool_defs = self.executor.tools();
        if matches!(session_key.kind, SessionKind::Cron(_)) {
            tool_defs
                .into_iter()
                .filter(|t| t.name != "signal_send")
                .filter(|t| t.name != "signal_react")
                .filter(|t| t.name != "signal_reply")
                .filter(|t| t.name != "cron_trigger")
                .collect()
        } else {
            tool_defs
        }
    }

    pub(crate) fn append_message(&self, session_key: &SessionKey, message: Message) {
        if let Err(e) = self.session_store.append(session_key, &message) {
            warn!(session = %session_key, error = %e, "failed to persist message");
        }
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        sessions
            .entry(session_key.clone())
            .or_default()
            .push(message);
    }

    /// Truncate session history back to `len` messages.
    fn truncate_session(&self, session_key: &SessionKey, len: usize) {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        if let Some(msgs) = sessions.get_mut(session_key) {
            msgs.truncate(len);
            if let Err(e) = self.session_store.replace(session_key, msgs) {
                warn!(session = %session_key, error = %e, "failed to persist truncated session");
            }
        }
    }

    fn replace_session_messages(&self, session_key: &SessionKey, messages: Vec<Message>) {
        if let Err(e) = self.session_store.replace(session_key, &messages) {
            warn!(session = %session_key, error = %e, "failed to persist repaired session");
        }
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .insert(session_key.clone(), messages);
    }

    /// Ensure every assistant `tool_use` has matching `tool_result` blocks in
    /// the immediately following user message.
    fn repair_dangling_tool_use(&self, session_key: &SessionKey) {
        let mut msgs = self.messages(session_key);
        if msgs.is_empty() {
            return;
        }

        let mut repaired_any = false;
        let mut i = 0;
        while i < msgs.len() {
            if msgs[i].role != Role::Assistant || !msgs[i].has_tool_requests() {
                i += 1;
                continue;
            }

            let tool_requests = msgs[i].tool_requests();
            let next_idx = i + 1;
            let next_is_user = next_idx < msgs.len() && msgs[next_idx].role == Role::User;

            let mut matched_results = HashMap::new();
            let mut remainder = Vec::new();
            if next_is_user {
                let expected_ids: HashSet<String> =
                    tool_requests.iter().map(|req| req.id.clone()).collect();
                for content in msgs[next_idx].content.iter().cloned() {
                    match &content {
                        Content::ToolResult { id, .. } if expected_ids.contains(id) => {
                            matched_results.entry(id.clone()).or_insert(content);
                        }
                        _ => remainder.push(content),
                    }
                }
            }

            let missing_ids: Vec<String> = tool_requests
                .iter()
                .filter_map(|req| {
                    (!matched_results.contains_key(&req.id)).then_some(req.id.clone())
                })
                .collect();

            if missing_ids.is_empty() && next_is_user {
                i += 1;
                continue;
            }

            repaired_any = true;
            warn!(
                session = %session_key,
                dangling_tool_ids = ?missing_ids,
                "repairing session with dangling tool_use blocks from interrupted turn"
            );

            let mut repaired_content = Vec::new();
            for req in &tool_requests {
                if let Some(content) = matched_results.remove(&req.id) {
                    repaired_content.push(content);
                } else {
                    repaired_content.push(Content::tool_result(
                        &req.id,
                        "error: previous turn was interrupted before this tool result was recorded",
                        true,
                    ));
                }
            }
            repaired_content.extend(remainder);

            if next_is_user {
                msgs[next_idx].content = repaired_content;
            } else {
                let mut repair_msg = Message::user();
                repair_msg.content = repaired_content;
                msgs.insert(next_idx, repair_msg);
                i += 1;
            }

            i += 1;
        }

        if repaired_any {
            self.replace_session_messages(session_key, msgs);
        }
    }

    pub(crate) fn messages(&self, session_key: &SessionKey) -> Vec<Message> {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        if !sessions.contains_key(session_key) {
            match self.session_store.load(session_key) {
                Ok(msgs) if !msgs.is_empty() => {
                    sessions.insert(session_key.clone(), msgs);
                }
                Err(e) => {
                    warn!(session = %session_key, error = %e, "failed to load session from disk");
                }
                _ => {}
            }
        }
        sessions.get(session_key).cloned().unwrap_or_default()
    }

    /// Returns true if a session has no messages (checks disk too).
    #[cfg(feature = "signal")]
    pub(crate) fn session_is_empty(&self, session_key: &SessionKey) -> bool {
        self.messages(session_key).is_empty()
    }

    /// Number of messages in a session.
    pub(crate) fn session_message_count(&self, session_key: &SessionKey) -> usize {
        self.messages(session_key).len()
    }

    /// Queue a message for injection into a running turn.
    ///
    /// The message is appended to the session at the next safe point
    /// (between turn iterations, after tool results are committed).
    pub(crate) fn inject_pending_inbound(&self, session_key: &SessionKey, content: String) {
        let mut pending = self
            .pending_inbound
            .lock()
            .expect("pending_inbound mutex poisoned");
        pending
            .entry(session_key.clone())
            .or_default()
            .push(content);
    }

    /// Drain any pending inbound messages into the session as user messages.
    ///
    /// Called at the start of each turn iteration so injected messages appear
    /// in the provider context without breaking tool_use/tool_result pairing.
    fn drain_pending_inbound(&self, session_key: &SessionKey) {
        let messages: Vec<String> = {
            let mut pending = self
                .pending_inbound
                .lock()
                .expect("pending_inbound mutex poisoned");
            pending.remove(session_key).unwrap_or_default()
        };
        for content in &messages {
            info!(
                session = %session_key,
                content_len = content.len(),
                "injecting mid-turn inbound message into session"
            );
            self.append_message(session_key, Message::user().with_text(content));
        }
    }

    pub(crate) fn configured_model_name_for_user(&self, user_name: Option<&str>) -> String {
        Self::configured_model_name_from_config(&self.config.load(), user_name)
    }

    pub(crate) fn configured_model_capabilities(&self, model: &str) -> EffectiveModelCapabilities {
        self.model_capabilities_for(model)
    }

    pub(crate) fn available_main_models(&self) -> Vec<AvailableModel> {
        crate::model_catalog::available_main_models(&self.config.load())
    }

    pub(crate) fn configured_context_limit_for_model(&self, model: &str) -> Option<usize> {
        let config = self.config.load();
        let requested = resolve_model_reference(&config, model);
        let resolved = resolve_available_model(&config, &requested.resolved)?;

        if let Some(limit) = config.agent.context_limit {
            let default_reference = resolve_model_reference(&config, &config.agent.model);
            if normalize_model_key(&default_reference.resolved)
                == normalize_model_key(&requested.resolved)
            {
                return Some(limit);
            }
        }

        let model_key = normalize_model_key(&requested.resolved);
        resolved
            .provider
            .model_context_limits
            .iter()
            .find_map(|(candidate, limit)| {
                (normalize_model_key(candidate) == model_key).then_some(*limit)
            })
    }

    pub(crate) fn model_aliases(&self, model: &str) -> Vec<String> {
        model_aliases_for(&self.config.load(), model)
    }

    pub(crate) fn same_model(left: &str, right: &str) -> bool {
        normalize_model_key(left) == normalize_model_key(right)
    }

    fn configured_model_name_from_config(config: &Config, user_name: Option<&str>) -> String {
        let default_reference = resolve_model_reference(config, &config.agent.model);
        let default_model = find_available_model(config, &default_reference.resolved)
            .map(|model| model.id)
            .unwrap_or(default_reference.resolved);
        let Some(user_name) = user_name else {
            return default_model;
        };

        let Some(user_model) = config
            .users
            .iter()
            .find(|user| user.name == user_name)
            .and_then(|user| user.model.as_deref())
        else {
            return default_model;
        };

        find_available_model(config, user_model).map_or(default_model, |model| model.id)
    }

    fn configured_group_trigger_model(&self, group: &crate::config::GroupConfig) -> String {
        let config = self.config.load();
        let requested = resolve_model_reference(&config, group.trigger_model_or_default());
        find_available_model(&config, &requested.resolved)
            .map(|model| model.id)
            .unwrap_or(requested.resolved)
    }

    pub(crate) fn model_name_for_user(&self, user_name: Option<&str>) -> String {
        let config = self.config.load();
        let default_model = Self::configured_model_name_from_config(&config, user_name);
        let Some(user_name) = user_name else {
            return default_model;
        };

        let Some(override_model) = self.user_models.get(user_name) else {
            return default_model;
        };

        if Self::same_model(&override_model, &default_model) {
            return default_model;
        }

        if let Some(model) = find_available_model(&config, &override_model) {
            return model.id;
        }

        debug!(
            user = %user_name,
            model = %override_model,
            "ignoring unavailable user model override"
        );
        default_model
    }

    fn main_provider_for_model(&self, model: &str) -> Result<Arc<dyn Provider>> {
        let config = self.config.load();
        let requested = resolve_model_reference(&config, model);
        let resolved_model = requested.resolved;
        let key = normalize_model_key(&resolved_model);

        if let Some(alias) = requested.alias.as_ref() {
            debug!(
                requested_model = %requested.requested,
                alias = %alias,
                resolved_model = %resolved_model,
                "resolved model alias for provider lookup"
            );
        }

        if let Some(provider) = self
            .main_providers
            .lock()
            .expect("main_providers mutex poisoned")
            .get(&key)
            .cloned()
        {
            return Ok(provider);
        }

        if let Some(provider) = self.providers.get_exact(&resolved_model) {
            let provider = Arc::clone(provider);
            self.main_providers
                .lock()
                .expect("main_providers mutex poisoned")
                .insert(key, Arc::clone(&provider));
            return Ok(provider);
        }

        let provider = provider_factory::create_provider_for_model(&config, &resolved_model)?;
        debug!(model = %resolved_model, provider = provider.name(), "created main model provider");
        self.main_providers
            .lock()
            .expect("main_providers mutex poisoned")
            .insert(key, Arc::clone(&provider));
        Ok(provider)
    }

    pub(crate) fn resolve_main_model(&self, user_name: Option<&str>) -> Result<ResolvedMainModel> {
        let model = self.model_name_for_user(user_name);
        let provider = self.main_provider_for_model(&model)?;
        Ok(ResolvedMainModel {
            model,
            context_limit: provider.model_info().context_limit,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_user_model(
        &self,
        user_name: Option<&str>,
        requested_model: &str,
    ) -> Result<ResolvedMainModel> {
        let Some(user_name) = user_name else {
            bail!("model selection requires a named user");
        };

        let (selected_model, default_model) =
            self.resolve_user_model_selection(user_name, requested_model)?;
        self.persist_user_model_selection(user_name, &selected_model, &default_model)?;

        self.resolve_main_model(Some(user_name))
    }

    /// Agent ID from config.
    pub(crate) fn agent_id(&self) -> String {
        self.config.load().agent.id.clone()
    }

    /// Session-level usage stats (cumulative + last context size).
    pub(crate) fn session_usage(&self, session_key: &SessionKey) -> SessionUsage {
        self.session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .get(session_key)
            .cloned()
            .unwrap_or_default()
    }

    /// Simulate an active turn for testing. Returns a `CancellationToken` that
    /// the caller can drop or cancel to "end" the simulated turn.
    #[cfg(test)]
    pub(crate) fn simulate_active_turn(&self, session_key: &SessionKey) -> CancellationToken {
        let token = CancellationToken::new();
        self.active_turns
            .lock()
            .expect("active_turns mutex poisoned")
            .insert(session_key.clone(), token.clone());
        token
    }

    #[cfg(test)]
    pub(crate) fn set_session_usage(&self, session_key: &SessionKey, usage: SessionUsage) {
        self.session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .insert(session_key.clone(), usage);
    }

    /// Update the last-seen input token count for a session.
    ///
    /// Called after each provider response so that `maybe_compact` can
    /// check mid-turn whether the context has grown past the threshold.
    ///
    /// Uses `context_input_tokens()` which includes cached tokens
    /// (cache_read + cache_write) to reflect actual context window usage.
    fn update_last_input_tokens(&self, session_key: &SessionKey, usage: &Usage) {
        let mut map = self
            .session_usage
            .lock()
            .expect("session_usage mutex poisoned");
        let entry = map.entry(session_key.clone()).or_default();
        entry.last_input_tokens = usage.context_input_tokens();
        drop(map);
    }

    /// Record usage from a completed turn into session-level cumulative stats.
    ///
    /// Does NOT overwrite `last_input_tokens` — that is already set correctly
    /// by `update_last_input_tokens` during the turn loop. The cumulative
    /// `turn_usage` sums input tokens across all iterations, which does not
    /// represent current context size.
    fn record_turn_usage(&self, session_key: &SessionKey, turn_usage: &Usage) {
        let mut map = self
            .session_usage
            .lock()
            .expect("session_usage mutex poisoned");
        let entry = map.entry(session_key.clone()).or_default();
        entry.cumulative += turn_usage.clone();
        drop(map);
    }

    /// Seed a session with formatted Signal chat history for context.
    #[cfg(feature = "signal")]
    pub(crate) fn seed_signal_history(&self, session_key: &SessionKey, history: &[InboundMessage]) {
        if history.is_empty() {
            return;
        }

        let mut context = String::from("[Recent Signal chat history for context]\n");
        for msg in history {
            context.push_str(&msg.content);
            context.push('\n');
        }
        context.push_str("[End of history context]");

        info!(
            session = %session_key,
            message_count = history.len(),
            "seeding session with signal history"
        );

        self.append_message(session_key, Message::user().with_text(context));
        self.append_message(
            session_key,
            Message::assistant().with_text("I have context from the recent conversation history."),
        );
    }

    // ── Group chat helpers ──────────────────────────────────────────────

    /// Record a non-triggering group message for future context injection.
    pub(crate) fn record_group_history(
        &self,
        session_key: &SessionKey,
        msg: &InboundMessage,
        limit: usize,
    ) {
        let entry = GroupHistoryEntry {
            body: msg.content.clone(),
        };
        self.group_history
            .lock()
            .expect("group_history mutex poisoned")
            .record(session_key, entry, limit);
    }

    /// Peek at buffered group history without consuming it.
    pub(crate) fn peek_group_history(&self, session_key: &SessionKey) -> Option<String> {
        self.group_history
            .lock()
            .expect("group_history mutex poisoned")
            .peek_context(session_key)
    }

    /// Drain buffered group history (consuming it).
    pub(crate) fn drain_group_history(&self, session_key: &SessionKey) -> Option<String> {
        self.group_history
            .lock()
            .expect("group_history mutex poisoned")
            .drain_context(session_key)
    }

    /// Evaluate LLM trigger: ask a cheap model if the assistant should respond.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn evaluate_llm_trigger(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        group_config: &crate::config::GroupConfig,
    ) -> bool {
        let trigger_model = self.configured_group_trigger_model(group_config);
        let provider = match self.main_provider_for_model(&trigger_model) {
            Ok(provider) => provider,
            Err(e) => {
                warn!(
                    error = %e,
                    model = %trigger_model,
                    "LLM trigger provider lookup failed, defaulting to skip"
                );
                return false;
            }
        };
        let selected_model = self.model_name_for_user(user_name);

        let system_prompt = match self
            .build_prompt(
                session_key,
                trust,
                user_name,
                &selected_model,
                channel,
                user_input,
                None,
            )
            .await
        {
            Ok(blocks) => blocks,
            Err(e) => {
                warn!(error = %e, "LLM trigger prompt build failed, defaulting to skip");
                return false;
            }
        };

        let trigger_prompt = group_config
            .trigger_prompt
            .as_deref()
            .unwrap_or(group_trigger::DEFAULT_TRIGGER_PROMPT);

        let messages = {
            let mut msgs = self.messages(session_key);
            let combined = format!("{user_input}\n\n---\n{trigger_prompt}");
            msgs.push(Message::user().with_text(combined));
            msgs
        };

        match provider.complete(&system_prompt, &messages, &[]).await {
            Ok((response, _usage)) => {
                let text = response.text();
                let decision = text.trim().to_uppercase().starts_with("YES");
                debug!(
                    session = %session_key,
                    model = %trigger_model,
                    response = text.trim(),
                    decision,
                    "LLM trigger evaluated"
                );
                decision
            }
            Err(e) => {
                warn!(
                    error = %e,
                    session = %session_key,
                    "LLM trigger call failed, defaulting to skip"
                );
                false
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn evaluate_as_needed_cron_delivery(
        &self,
        session_key: &SessionKey,
        cron_message: &str,
        proposed_response: &str,
        review_prompt_override: Option<&str>,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
    ) -> Result<bool> {
        let review_input =
            format!("{cron_message}\n\nProposed scheduled message:\n{proposed_response}");
        let selected_model = self.model_name_for_user(user_name);
        let system_prompt = self
            .build_prompt(
                session_key,
                trust,
                user_name,
                &selected_model,
                channel,
                &review_input,
                None,
            )
            .await?;

        let review_prompt = cron_delivery::build_as_needed_review_prompt(
            channel,
            cron_message,
            proposed_response,
            review_prompt_override,
        );
        let mut messages = self.messages(session_key);
        messages.push(Message::user().with_text(review_prompt));

        let provider = self.main_provider_for_model(&selected_model)?;
        let model = selected_model;
        let span = info_span!(
            "cron_delivery_review",
            session = %session_key,
            trust = ?trust,
            user = ?user_name,
            channel = ?channel,
            proposed_len = proposed_response.len(),
            model = %model,
            custom_prompt = review_prompt_override.is_some(),
        );

        async {
            let (response, _usage) = provider.complete(&system_prompt, &messages, &[]).await?;
            let text = response.text();
            let decision = cron_delivery::review_allows_delivery(&text);
            debug!(
                session = %session_key,
                model = %model,
                response = text.trim(),
                decision,
                "as-needed cron delivery reviewed"
            );
            Ok(decision)
        }
        .instrument(span)
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn repair_terminal_reply(
        &self,
        provider: &dyn Provider,
        model: &str,
        system_prompt: &[String],
        messages: &[Message],
        original_response: Message,
        policy: FinalReplyPolicy,
    ) -> (Message, Usage) {
        let Some(repair_prompt) = policy.repair_prompt() else {
            return (original_response, Usage::default());
        };

        let span = info_span!(
            "final_reply_repair",
            model = %model,
            policy = %policy.as_str(),
            message_count = messages.len(),
        );

        async {
            warn!(
                policy = %policy.as_str(),
                original_response_text_len = original_response.text().len(),
                "terminal reply was empty; requesting repaired final reply"
            );

            let mut repair_messages = messages.to_vec();
            repair_messages.push(Message::user().with_text(repair_prompt));

            match provider
                .complete_fast(system_prompt, &repair_messages, &[])
                .await
            {
                Ok((response, usage))
                    if !response.has_tool_requests() && !response.text().trim().is_empty() =>
                {
                    debug!(
                        policy = %policy.as_str(),
                        repaired_response_text_len = response.text().len(),
                        "repaired final reply generated"
                    );
                    (response, usage)
                }
                Ok((_response, usage)) => {
                    warn!(
                        policy = %policy.as_str(),
                        "repair call returned empty again"
                    );
                    if let Some(fallback) = policy.fallback_text() {
                        info!(policy = %policy.as_str(), fallback, "using fallback final reply");
                        (Message::assistant().with_text(fallback), usage)
                    } else {
                        (original_response, usage)
                    }
                }
                Err(error) => {
                    warn!(
                        policy = %policy.as_str(),
                        error = %error,
                        "repair request failed"
                    );
                    if let Some(fallback) = policy.fallback_text() {
                        info!(policy = %policy.as_str(), fallback, "using fallback final reply");
                        (Message::assistant().with_text(fallback), Usage::default())
                    } else {
                        (original_response, Usage::default())
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    fn stream_policy_for_model(&self, model: &str) -> StreamPolicy {
        let config = self.config.load();
        if config.providers.is_empty() {
            return config.provider.stream_policy;
        }

        resolve_available_model(&config, model).map_or(config.provider.stream_policy, |available| {
            available.provider.stream_policy
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn assistant_response(
        &self,
        provider: &dyn Provider,
        model: &str,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let stream_policy = self.stream_policy_for_model(model);
        if matches!(stream_policy, StreamPolicy::Require) && !provider.supports_streaming() {
            bail!(
                "provider '{}' does not support stream_policy=require for model '{}'",
                provider.name(),
                model
            );
        }

        let streaming =
            provider.supports_streaming() && !matches!(stream_policy, StreamPolicy::Disable);
        let request_metrics = estimate_provider_request_metrics(system_prompt, messages, tool_defs);
        let span = info_span!(
            "provider_request",
            model = %model,
            message_count = messages.len(),
            tool_count = tool_defs.len(),
            streaming,
            stream_policy = %stream_policy,
            request_estimated_bytes = request_metrics.estimated_json_bytes,
            request_system_chars = request_metrics.system_chars,
            request_message_chars = request_metrics.message_chars,
            request_tool_schema_bytes = request_metrics.tool_schema_bytes,
        );

        let (response, usage) = async {
            if streaming {
                self.assistant_response_streaming(
                    provider,
                    system_prompt,
                    messages,
                    tool_defs,
                    event_tx,
                    stream_policy,
                )
                .await
            } else {
                self.assistant_response_non_streaming(
                    provider,
                    system_prompt,
                    messages,
                    tool_defs,
                    event_tx,
                )
                .await
            }
        }
        .instrument(span)
        .await?;

        if usage.stop_reason.as_deref() == Some("tool_use") && !response.has_tool_requests() {
            bail!("provider returned stop_reason=tool_use but no tool requests were parsed");
        }

        Ok((response, usage))
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn assistant_response_streaming(
        &self,
        provider: &dyn Provider,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
        stream_policy: StreamPolicy,
    ) -> Result<(Message, Usage)> {
        'stream_attempts: for attempt in 0..=MAX_EARLY_STREAM_RETRIES {
            let mut stream = match provider.stream(system_prompt, messages, tool_defs).await {
                Ok(stream) => stream,
                Err(error) => {
                    if overflow_recovery::should_force_compact_after_error(&error) {
                        return Err(error);
                    }

                    if is_transient_transport_error(&error) && attempt < MAX_EARLY_STREAM_RETRIES {
                        let backoff = early_stream_retry_backoff(attempt);
                        let model = provider.model_info();
                        warn!(
                            provider = provider.name(),
                            model = %model.name,
                            attempt = attempt + 1,
                            max_attempts = MAX_EARLY_STREAM_RETRIES + 1,
                            backoff_ms = duration_to_millis_u64(backoff),
                            error = %error,
                            "streaming provider request failed before first output, retrying stream"
                        );
                        tokio::time::sleep(backoff).await;
                        continue 'stream_attempts;
                    }

                    if matches!(stream_policy, StreamPolicy::Require) {
                        return Err(error);
                    }

                    let model = provider.model_info();
                    warn!(
                        provider = provider.name(),
                        model = %model.name,
                        attempt = attempt + 1,
                        max_attempts = MAX_EARLY_STREAM_RETRIES + 1,
                        error = %error,
                        "streaming provider request failed before first output, falling back to non-streaming"
                    );
                    return self
                        .assistant_response_non_streaming(
                            provider,
                            system_prompt,
                            messages,
                            tool_defs,
                            event_tx,
                        )
                        .await;
                }
            };

            let mut response = Message::assistant();
            let mut usage = Usage::default();
            let mut saw_stream_output = false;

            while let Some(item) = stream.next().await {
                match item {
                    Ok((msg_opt, usage_opt)) => {
                        if msg_opt.is_some() || usage_opt.is_some() {
                            saw_stream_output = true;
                        }

                        if let Some(msg) = msg_opt {
                            if let Some(final_usage) = usage_opt {
                                usage += final_usage;
                                response = msg;
                            } else {
                                let text = msg.text();
                                if !text.is_empty() {
                                    let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if saw_stream_output {
                            return Err(error);
                        }

                        if overflow_recovery::should_force_compact_after_error(&error) {
                            return Err(error);
                        }

                        if is_transient_transport_error(&error)
                            && attempt < MAX_EARLY_STREAM_RETRIES
                        {
                            let backoff = early_stream_retry_backoff(attempt);
                            let model = provider.model_info();
                            warn!(
                                provider = provider.name(),
                                model = %model.name,
                                attempt = attempt + 1,
                                max_attempts = MAX_EARLY_STREAM_RETRIES + 1,
                                backoff_ms = duration_to_millis_u64(backoff),
                                error = %error,
                                "streaming provider request failed before first output, retrying stream"
                            );
                            tokio::time::sleep(backoff).await;
                            continue 'stream_attempts;
                        }

                        if matches!(stream_policy, StreamPolicy::Require) {
                            return Err(error);
                        }

                        let model = provider.model_info();
                        warn!(
                            provider = provider.name(),
                            model = %model.name,
                            attempt = attempt + 1,
                            max_attempts = MAX_EARLY_STREAM_RETRIES + 1,
                            error = %error,
                            "streaming provider request failed before first output, falling back to non-streaming"
                        );
                        return self
                            .assistant_response_non_streaming(
                                provider,
                                system_prompt,
                                messages,
                                tool_defs,
                                event_tx,
                            )
                            .await;
                    }
                }
            }

            debug!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                cache_write_tokens = usage.cache_write_tokens,
                stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
                "provider response complete"
            );

            return Ok((response, usage));
        }

        unreachable!("stream attempt loop always returns or retries")
    }

    async fn assistant_response_non_streaming(
        &self,
        provider: &dyn Provider,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        for attempt in 0..=MAX_EARLY_NON_STREAM_RETRIES {
            let (response, usage) =
                match provider.complete(system_prompt, messages, tool_defs).await {
                    Ok(result) => result,
                    Err(error) => {
                        if overflow_recovery::should_force_compact_after_error(&error) {
                            return Err(error);
                        }

                        if is_transient_transport_error(&error)
                            && attempt < MAX_EARLY_NON_STREAM_RETRIES
                        {
                            let backoff = early_non_stream_retry_backoff(attempt);
                            let model = provider.model_info();
                            warn!(
                                provider = provider.name(),
                                model = %model.name,
                                attempt = attempt + 1,
                                max_attempts = MAX_EARLY_NON_STREAM_RETRIES + 1,
                                backoff_ms = duration_to_millis_u64(backoff),
                                error = %error,
                                "non-streaming provider request failed before output, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            continue;
                        }

                        return Err(error);
                    }
                };

            let text = response.text();
            if !text.is_empty() {
                let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
            }

            debug!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                cache_write_tokens = usage.cache_write_tokens,
                stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
                "provider response complete"
            );

            return Ok((response, usage));
        }

        unreachable!("non-stream attempt loop always returns or retries")
    }
}

const MAX_EARLY_STREAM_RETRIES: u32 = 2;
const EARLY_STREAM_RETRY_BASE_BACKOFF_MS: u64 = 200;
const MAX_EARLY_NON_STREAM_RETRIES: u32 = 3;
const EARLY_NON_STREAM_RETRY_BASE_BACKOFF_MS: u64 = 200;
const MAX_COMPACTION_PASSES: u32 = 4;
const MIN_RECENT_CONTEXT_TARGET_TOKENS: u32 = 2_000;

fn early_stream_retry_backoff(attempt: u32) -> Duration {
    Duration::from_millis(
        EARLY_STREAM_RETRY_BASE_BACKOFF_MS.saturating_mul(1_u64 << attempt.min(8)),
    )
}

fn early_non_stream_retry_backoff(attempt: u32) -> Duration {
    Duration::from_millis(
        EARLY_NON_STREAM_RETRY_BASE_BACKOFF_MS.saturating_mul(1_u64 << attempt.min(8)),
    )
}

fn duration_to_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn estimate_tokens_from_json_bytes(bytes: usize) -> u32 {
    let tokens = bytes / 4;
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

fn shrink_recent_context_target(
    current_target: u32,
    estimated_request_tokens: u32,
    input_budget_tokens: u32,
) -> u32 {
    if estimated_request_tokens <= input_budget_tokens || current_target == 0 {
        return current_target;
    }

    let scaled = current_target
        .saturating_mul(input_budget_tokens)
        .checked_div(estimated_request_tokens)
        .unwrap_or(0);
    let reduced = scaled.min(current_target.saturating_sub(1));
    reduced.max(MIN_RECENT_CONTEXT_TARGET_TOKENS)
}

fn is_transient_transport_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    [
        "error sending request for url",
        "connection reset",
        "connection closed",
        "connection terminated",
        "network connection lost",
        "network error",
        "remote protocol error",
        "peer closed",
        "broken pipe",
        "unexpected eof",
        "timed out",
        "timeout",
        "deadline has elapsed",
        "body write aborted",
    ]
    .into_iter()
    .any(|needle| text.contains(needle))
}

fn prepare_messages_for_provider(
    messages: &[Message],
    scope: &coop_core::WorkspaceScope,
    capabilities: &EffectiveModelCapabilities,
) -> Vec<Message> {
    let messages = if capabilities.supports_input(crate::config::ModelModality::Image) {
        coop_core::images::inject_images_for_provider(messages, scope)
    } else {
        messages.to_vec()
    };

    if capabilities.supports_input(crate::config::ModelModality::Image) {
        messages
    } else {
        strip_images_from_messages(&messages)
    }
}

fn strip_images_from_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            message
                .content
                .retain(|content| !matches!(content, Content::Image { .. }));
            message
        })
        .collect()
}

fn terminal_turn_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && !message.has_tool_requests())
        .cloned()
        .into_iter()
        .collect()
}

fn build_cron_delivery_prompt_block(
    delivery_mode: CronDeliveryMode,
    channel: Option<&str>,
) -> String {
    let channel = channel.unwrap_or("messaging");
    match delivery_mode {
        CronDeliveryMode::Always => format!(
            "## Scheduled Delivery\n- This scheduled response will be delivered to the user via {channel}.\n- This cron uses delivery = \"always\". Always reply with the content to deliver.\n- End the turn with exactly one non-empty final message to deliver.\n- After any tool use, you must still send that final message. Never end silently.\n- Do not reply with NO_ACTION_NEEDED."
        ),
        CronDeliveryMode::AsNeeded => format!(
            "## Scheduled Delivery\n- This scheduled response will be delivered to the user via {channel}.\n- This cron uses delivery = \"as_needed\". Be highly selective: only send a message when the user would likely want the interruption now.\n- Routine status, unchanged conditions, low-confidence guesses, or information that can wait should be treated as no action needed.\n- If nothing needs attention, reply with exactly NO_ACTION_NEEDED.\n- If something truly needs attention, reply with only the content to deliver.\n- End the turn with exactly one final output: either NO_ACTION_NEEDED, or the non-empty message to deliver.\n- After any tool use, you must still send one of those final outputs. Never end silently.\n- Never include NO_ACTION_NEEDED alongside real content."
        ),
    }
}

fn build_group_intro(
    trigger: &crate::config::GroupTrigger,
    agent_id: &str,
    users: &[crate::config::UserConfig],
) -> String {
    use crate::config::GroupTrigger;

    let activation = match trigger {
        GroupTrigger::Always => "always-on (you receive every group message)",
        GroupTrigger::Llm => {
            "trigger-only (a classifier determined you should respond; \
             recent chat context may be included)"
        }
        GroupTrigger::Mention | GroupTrigger::Regex => {
            "trigger-only (you are invoked only when explicitly mentioned or triggered; \
             recent chat context may be included)"
        }
    };

    let mut lines = vec![
        "You are replying inside a group chat.".to_owned(),
        format!(
            "Your name in this group is \"{agent_id}\". \
             When a message contains @{agent_id}, the sender is addressing you directly."
        ),
        format!("Activation: {activation}."),
    ];

    // Participant roster: list coop users who have Signal match patterns.
    // Bounded by [[users]] config size (typically 2-5), not the people DB.
    let signal_users: Vec<_> = users
        .iter()
        .filter(|u| u.r#match.iter().any(|p| p.starts_with("signal:")))
        .collect();
    if !signal_users.is_empty() {
        let roster: Vec<String> = signal_users
            .iter()
            .map(|u| format!("{} (trust:{:?})", u.name, u.trust))
            .collect();
        lines.push(format!(
            "Known participants: {}. \
             Messages from known participants include a (user:name) tag in the sender header.",
            roster.join(", ")
        ));
    }

    if matches!(trigger, GroupTrigger::Always) {
        lines.push(format!(
            "If no response is needed, reply with exactly \"{SILENT_REPLY_TOKEN}\" \
             (and nothing else) so the system stays silent. Do not add any other \
             words, punctuation, or explanations.",
        ));
        lines.push(
            "Be extremely selective: reply only when directly addressed \
             or clearly helpful. Otherwise stay silent."
                .to_owned(),
        );
    }

    lines.push(
        "Be a good group participant: mostly lurk and follow the conversation; \
         reply only when directly addressed or you can add clear value."
            .to_owned(),
    );
    lines.push(
        "Address the specific sender noted in the message context. \
         Sender headers show names like \"Alice (user:alice)\" — \
         use the display name (Alice) when addressing them."
            .to_owned(),
    );

    lines.join(" ")
}

fn parse_session_key(session: &str, agent_id: &str) -> Option<SessionKey> {
    if session == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Main,
        });
    }

    let rest = session.strip_prefix(&format!("{agent_id}:"))?;
    if rest == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Main,
        });
    }

    if let Some(dm) = rest.strip_prefix("dm:") {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Dm(dm.to_owned()),
        });
    }

    if let Some(group) = rest.strip_prefix("group:") {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Group(group.to_owned()),
        });
    }

    if let Some(isolated) = rest.strip_prefix("isolated:") {
        let uuid = Uuid::parse_str(isolated).ok()?;
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Isolated(uuid),
        });
    }

    if let Some(subagent) = rest.strip_prefix("subagent:") {
        let uuid = Uuid::parse_str(subagent).ok()?;
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Subagent(uuid),
        });
    }

    if let Some(cron_name) = rest.strip_prefix("cron:")
        && !cron_name.is_empty()
    {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Cron(cron_name.to_owned()),
        });
    }

    None
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use coop_core::fakes::{FakeProvider, FakeTool, SimpleExecutor};
    use coop_core::tools::DefaultExecutor;
    use coop_core::traits::ProviderStream;
    use coop_core::types::{Content, ModelInfo};
    use coop_core::{Provider, Tool, ToolContext, ToolDef, ToolOutput};
    use std::collections::VecDeque;
    use std::io::BufRead;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::config::{Config, shared_config};
    use crate::provider_registry::ProviderRegistry;

    fn registry(provider: Arc<dyn Provider>) -> ProviderRegistry {
        ProviderRegistry::new(provider)
    }

    struct RecordingTypingNotifier {
        events: tokio::sync::Mutex<Vec<bool>>,
        notify: tokio::sync::Notify,
    }

    impl RecordingTypingNotifier {
        fn new() -> Self {
            Self {
                events: tokio::sync::Mutex::new(Vec::new()),
                notify: tokio::sync::Notify::new(),
            }
        }

        async fn wait_for_events(&self, count: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            loop {
                if self.events.lock().await.len() >= count {
                    return;
                }

                let now = tokio::time::Instant::now();
                assert!(now < deadline, "timed out waiting for typing events");
                let wait = deadline.saturating_duration_since(now);
                let _ = tokio::time::timeout(wait, self.notify.notified()).await;
            }
        }
    }

    #[async_trait]
    impl TypingNotifier for RecordingTypingNotifier {
        async fn set_typing(&self, _session_key: &SessionKey, started: bool) {
            let mut events = self.events.lock().await;
            events.push(started);
            drop(events);
            self.notify.notify_waiters();
        }
    }

    #[derive(Debug)]
    struct RecordingProvider {
        model: ModelInfo,
        tools: Mutex<Vec<Vec<ToolDef>>>,
        messages: Mutex<Vec<Vec<Message>>>,
        response: String,
    }

    impl RecordingProvider {
        fn new(model: &str, response: &str) -> Self {
            Self {
                model: ModelInfo {
                    name: model.to_owned(),
                    context_limit: 128_000,
                },
                tools: Mutex::new(Vec::new()),
                messages: Mutex::new(Vec::new()),
                response: response.to_owned(),
            }
        }

        fn last_tool_names(&self) -> Vec<String> {
            self.tools
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|tool| tool.name)
                .collect()
        }

        fn last_messages(&self) -> Vec<Message> {
            self.messages
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            messages: &[Message],
            tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            self.tools.lock().unwrap().push(tools.to_vec());
            self.messages.lock().unwrap().push(messages.to_vec());
            Ok((
                Message::assistant().with_text(self.response.clone()),
                Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("RecordingProvider does not support streaming")
        }
    }

    fn test_config() -> Config {
        toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"
"#,
        )
        .unwrap()
    }

    fn test_config_with_stream_policy(stream_policy: StreamPolicy) -> Config {
        let mut config = test_config();
        config.provider.stream_policy = stream_policy;
        config
    }

    fn read_trace_file(path: &std::path::Path) -> String {
        let file = std::fs::File::open(path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(file)
            .lines()
            .map(|line| line.unwrap())
            .filter(|line| !line.is_empty())
            .collect();
        lines.join("\n")
    }

    fn read_trace_file_with_retry(path: &std::path::Path, needle: &str) -> String {
        for _ in 0..400 {
            let text = read_trace_file(path);
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        read_trace_file(path)
    }

    #[test]
    fn parse_main_alias() {
        let key = parse_session_key("main", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Main,
            }
        );
    }

    #[test]
    fn parse_dm_session() {
        let key = parse_session_key("coop:dm:signal:alice-uuid", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
            }
        );
    }

    #[test]
    fn parse_group_session() {
        let key = parse_session_key("coop:group:signal:group:deadbeef", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Group("signal:group:deadbeef".to_owned()),
            }
        );
    }

    #[test]
    fn parse_cron_session() {
        let key = parse_session_key("coop:cron:heartbeat", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Cron("heartbeat".to_owned()),
            }
        );
    }

    #[test]
    fn parse_subagent_session() {
        let key = parse_session_key("coop:subagent:123e4567-e89b-12d3-a456-426614174000", "coop")
            .unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Subagent(
                    Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap(),
                ),
            }
        );
    }

    #[test]
    fn parse_rejects_other_agent() {
        assert!(parse_session_key("other:main", "coop").is_none());
    }

    fn test_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "You are a test agent.").unwrap();
        dir
    }

    #[tokio::test]
    async fn build_prompt_includes_as_needed_cron_delivery_block() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Cron("heartbeat".to_owned()),
        };
        let model = gateway.configured_model_name_for_user(None);

        let prompt = gateway
            .build_prompt(
                &session_key,
                TrustLevel::Full,
                Some("alice"),
                &model,
                Some("signal"),
                "check HEARTBEAT.md",
                Some(CronDeliveryMode::AsNeeded),
            )
            .await
            .unwrap()
            .join("\n\n");

        assert!(prompt.contains("delivery = \"as_needed\""));
        assert!(prompt.contains("Be highly selective"));
        assert!(prompt.contains("NO_ACTION_NEEDED"));
        assert!(prompt.contains("Never end silently"));
    }

    #[tokio::test]
    async fn build_prompt_includes_always_cron_delivery_block() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Cron("morning-briefing".to_owned()),
        };
        let model = gateway.configured_model_name_for_user(None);

        let prompt = gateway
            .build_prompt(
                &session_key,
                TrustLevel::Full,
                Some("alice"),
                &model,
                Some("signal"),
                "Morning briefing",
                Some(CronDeliveryMode::Always),
            )
            .await
            .unwrap()
            .join("\n\n");

        assert!(prompt.contains("delivery = \"always\""));
        assert!(prompt.contains("Do not reply with NO_ACTION_NEEDED"));
        assert!(prompt.contains("Never end silently"));
    }

    async fn trace_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    /// Provider that always fails with the given error message.
    #[derive(Debug)]
    struct FailingProvider {
        error_msg: String,
        model: ModelInfo,
    }

    impl FailingProvider {
        fn new(msg: impl Into<String>) -> Self {
            Self {
                error_msg: msg.into(),
                model: ModelInfo {
                    name: "fail-model".into(),
                    context_limit: 128_000,
                },
            }
        }
    }

    #[async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            anyhow::bail!("{}", self.error_msg)
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("{}", self.error_msg)
        }
    }

    /// Provider that succeeds on the first call (returning a tool_use),
    /// then fails on the second call.
    #[derive(Debug)]
    struct FailOnSecondCallProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
        error_msg: String,
    }

    impl FailOnSecondCallProvider {
        fn new(msg: impl Into<String>) -> Self {
            Self {
                model: ModelInfo {
                    name: "fail-second".into(),
                    context_limit: 128_000,
                },
                call_count: Mutex::new(0),
                error_msg: msg.into(),
            }
        }
    }

    #[async_trait]
    impl Provider for FailOnSecondCallProvider {
        fn name(&self) -> &'static str {
            "fail-second"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                // First call: return a tool_use so the turn continues
                Ok((
                    Message::assistant().with_tool_request(
                        "tool_1",
                        "bash",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                    Usage {
                        input_tokens: Some(100),
                        output_tokens: Some(50),
                        ..Default::default()
                    },
                ))
            } else {
                anyhow::bail!("{}", self.error_msg)
            }
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[derive(Debug)]
    struct RetryAfterCompactionProvider {
        model: ModelInfo,
        stream_calls: Arc<Mutex<u32>>,
        compaction_calls: Arc<Mutex<u32>>,
        saw_summary_on_retry: Arc<Mutex<bool>>,
    }

    impl RetryAfterCompactionProvider {
        fn new(
            stream_calls: Arc<Mutex<u32>>,
            compaction_calls: Arc<Mutex<u32>>,
            saw_summary_on_retry: Arc<Mutex<bool>>,
        ) -> Self {
            Self {
                model: ModelInfo {
                    name: "retry-after-compaction".into(),
                    context_limit: 128_000,
                },
                stream_calls,
                compaction_calls,
                saw_summary_on_retry,
            }
        }
    }

    #[async_trait]
    impl Provider for RetryAfterCompactionProvider {
        fn name(&self) -> &'static str {
            "retry-after-compaction"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            anyhow::bail!("not supported")
        }

        async fn stream(
            &self,
            _system: &[String],
            messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            let first_call = {
                let mut calls = self.stream_calls.lock().unwrap();
                *calls += 1;
                *calls == 1
            };

            if first_call {
                let stream = futures::stream::once(async {
                    Err(anyhow::anyhow!(
                        "Anthropic streamed tool_use for `bash` ended with invalid/incomplete input_json after max_tokens"
                    ))
                });
                return Ok(Box::pin(stream));
            }

            let saw_summary = messages.iter().any(|message| {
                message
                    .text()
                    .contains("<summary>Compacted summary of conversation.</summary>")
            });
            *self.saw_summary_on_retry.lock().unwrap() = saw_summary;

            let message = Message::assistant().with_text("Recovered after compaction");
            let usage = Usage {
                input_tokens: Some(120),
                output_tokens: Some(24),
                stop_reason: Some("end_turn".into()),
                ..Default::default()
            };
            let stream = futures::stream::once(async { Ok((Some(message), Some(usage))) });
            Ok(Box::pin(stream))
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        async fn complete_fast(
            &self,
            _system: &[String],
            messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            {
                let mut calls = self.compaction_calls.lock().unwrap();
                *calls += 1;
            }

            let is_compaction_call = messages
                .last()
                .is_some_and(|message| message.text().contains("continuation summary"));
            assert!(is_compaction_call, "expected compaction summary request");

            Ok((
                Message::assistant()
                    .with_text("<summary>Compacted summary of conversation.</summary>"),
                Usage {
                    input_tokens: Some(500),
                    output_tokens: Some(120),
                    stop_reason: Some("end_turn".into()),
                    ..Default::default()
                },
            ))
        }
    }

    #[tokio::test]
    async fn provider_error_mid_turn_rolls_back_all_messages() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailOnSecondCallProvider::new(
            "Anthropic API error: 500 - internal server error",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok(), "should not propagate error");

        let mut saw_error = false;
        let mut saw_tool_start = false;
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(_) => saw_error = true,
                TurnEvent::ToolStart { .. } => saw_tool_start = true,
                TurnEvent::Done(_) => saw_done = true,
                _ => {}
            }
        }

        assert!(saw_tool_start, "tool should have executed on iteration 0");
        assert!(saw_error, "should emit error on iteration 1 failure");
        assert!(saw_done, "should emit Done after error");

        // Session must be fully rolled back — no user msg, no assistant msg,
        // no tool result from the partial turn.
        assert!(
            gateway.messages(&session_key).is_empty(),
            "session should be fully rolled back after mid-turn error"
        );
    }

    #[tokio::test]
    async fn retries_truncated_tool_use_max_tokens_once_after_compaction() {
        let workspace = test_workspace();
        let stream_calls = Arc::new(Mutex::new(0));
        let compaction_calls = Arc::new(Mutex::new(0));
        let saw_summary_on_retry = Arc::new(Mutex::new(false));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterCompactionProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&compaction_calls),
            Arc::clone(&saw_summary_on_retry),
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok(), "retry should keep the turn alive");
        assert_eq!(
            *stream_calls.lock().unwrap(),
            2,
            "should retry exactly once"
        );
        assert_eq!(*compaction_calls.lock().unwrap(), 1, "should compact once");
        assert!(
            *saw_summary_on_retry.lock().unwrap(),
            "retry request should use the compacted summary"
        );
        assert!(
            gateway.get_compaction(&session_key).is_some(),
            "compaction state should be persisted for the retry"
        );

        let mut saw_error = false;
        let mut saw_done = false;
        let mut saw_compacting = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(_) => saw_error = true,
                TurnEvent::Compacting => saw_compacting = true,
                TurnEvent::Done(done) => {
                    saw_done = true;
                    assert!(!done.hit_limit, "retry should finish the turn");
                }
                _ => {}
            }
        }

        assert!(
            !saw_error,
            "successful retry should not emit an error event"
        );
        assert!(saw_compacting, "retry path should compact before retrying");
        assert!(saw_done, "turn should complete after retry");

        let messages = gateway.messages(&session_key);
        assert_eq!(messages.len(), 2, "session should keep the recovered turn");
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[1].text(), "Recovered after compaction");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_records_overflow_compaction_retry() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let _trace_guard = trace_test_guard().await;

        let dir = tempfile::tempdir().unwrap();
        let trace_file = dir.path().join("traces.jsonl");

        let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let jsonl_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let workspace = test_workspace();
        let stream_calls = Arc::new(Mutex::new(0));
        let compaction_calls = Arc::new(Mutex::new(0));
        let saw_summary_on_retry = Arc::new(Mutex::new(false));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterCompactionProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&compaction_calls),
            Arc::clone(&saw_summary_on_retry),
        ));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(DefaultExecutor::new()),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(*stream_calls.lock().unwrap(), 2);
        assert_eq!(*compaction_calls.lock().unwrap(), 1);
        assert!(*saw_summary_on_retry.lock().unwrap());

        drop(default_guard);
        drop(guard);

        let trace = read_trace_file_with_retry(&trace_file, "overflow compaction retry succeeded");

        assert!(
            trace.contains(
                "provider failed with overflow-like error; compacting and retrying iteration"
            ) || trace.contains("compaction triggered"),
            "expected overflow retry trace"
        );
        assert!(
            trace.contains("overflow compaction retry succeeded"),
            "expected retry success trace"
        );
    }

    #[tokio::test]
    async fn provider_error_sends_detail_to_full_trust_user() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new(
            "Anthropic API error: 400 - bad request",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok(), "should not propagate error");

        let mut saw_error = false;
        let mut error_msg = String::new();
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(msg) => {
                    saw_error = true;
                    error_msg = msg;
                }
                TurnEvent::Done(_) => saw_done = true,
                _ => {}
            }
        }

        assert!(saw_error, "should emit TurnEvent::Error");
        assert!(
            error_msg.contains("400"),
            "full-trust user should see actual error: {error_msg}"
        );
        assert!(saw_done, "should emit TurnEvent::Done after error");

        // Session should be rolled back — no leftover user message
        assert!(
            gateway.messages(&session_key).is_empty(),
            "session should be rolled back on error"
        );
    }

    #[tokio::test]
    async fn provider_error_hides_detail_from_public_trust_user() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new(
            "Anthropic API error: 400 - bad request",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Public,
                None,
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        let mut error_msg = String::new();
        while let Ok(event) = event_rx.try_recv() {
            if let TurnEvent::Error(msg) = event {
                error_msg = msg;
            }
        }

        assert!(
            !error_msg.contains("400"),
            "public user should NOT see API details: {error_msg}"
        );
        assert!(
            error_msg.contains("Something went wrong"),
            "public user should get generic message: {error_msg}"
        );
    }

    #[tokio::test]
    async fn typing_guard_sends_stop_on_drop() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());
        let notifier: Arc<RecordingTypingNotifier> = Arc::new(RecordingTypingNotifier::new());
        let typing_notifier: Arc<dyn TypingNotifier> = Arc::clone(&notifier) as _;

        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            executor,
            Some(typing_notifier),
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(16);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        notifier.wait_for_events(2).await;
        let events = notifier.events.lock().await.clone();
        assert!(events[0]);
        assert!(!events[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn typing_guard_refreshes_periodically() {
        let notifier = Arc::new(RecordingTypingNotifier::new());
        let session_key = SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Main,
        };

        // Initial set_typing(true) as the callsite does
        notifier.set_typing(&session_key, true).await;
        let guard = TypingGuard::new(
            Arc::clone(&notifier) as Arc<dyn TypingNotifier>,
            session_key,
        );

        // Let the spawned background task register its first sleep.
        tokio::task::yield_now().await;

        // Advance past one refresh interval and let the task fully
        // re-enter its loop before advancing again.
        tokio::time::advance(TYPING_REFRESH_INTERVAL).await;
        // Yield a few times so the background task processes the
        // wakeup, calls set_typing, and re-registers its next sleep.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true], "after first refresh");

        tokio::time::advance(TYPING_REFRESH_INTERVAL).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        // initial true + 2 refresh trues = 3 events
        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true, true], "after second refresh");

        // Drop the guard → cancellation triggers stop
        drop(guard);
        tokio::task::yield_now().await;

        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true, true, false]);
    }

    /// Provider that always returns tool_use when tools are available,
    /// and a text summary when called with no tools (the forced completion).
    #[derive(Debug)]
    struct AlwaysToolUseProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
    }

    impl AlwaysToolUseProvider {
        fn new() -> Self {
            Self {
                model: ModelInfo {
                    name: "always-tool".into(),
                    context_limit: 128_000,
                },
                call_count: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.call_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for AlwaysToolUseProvider {
        fn name(&self) -> &'static str {
            "always-tool"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            *self.call_count.lock().unwrap() += 1;
            let usage = Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..Default::default()
            };
            if tools.is_empty() {
                // Forced final completion — return text
                Ok((
                    Message::assistant().with_text("I hit the iteration limit. Here's a summary."),
                    usage,
                ))
            } else {
                // Normal iteration — return tool_use
                Ok((
                    Message::assistant().with_tool_request(
                        "tool_1",
                        "bash",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                    usage,
                ))
            }
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[derive(Debug)]
    struct SequencedProvider {
        model: ModelInfo,
        responses: Mutex<VecDeque<Message>>,
    }

    impl SequencedProvider {
        fn new(responses: Vec<Message>) -> Self {
            Self {
                model: ModelInfo {
                    name: "sequenced-model".into(),
                    context_limit: 128_000,
                },
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    #[async_trait]
    impl Provider for SequencedProvider {
        fn name(&self) -> &'static str {
            "sequenced"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("script exhausted"))?;

            Ok((
                response,
                Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    /// Provider that sleeps before returning a tool request, allowing cancellation.
    #[derive(Debug)]
    struct SlowToolProvider {
        model: ModelInfo,
        delay: Duration,
    }

    impl SlowToolProvider {
        fn new(delay: Duration) -> Self {
            Self {
                model: ModelInfo {
                    name: "slow-tool".into(),
                    context_limit: 128_000,
                },
                delay,
            }
        }
    }

    #[async_trait]
    impl Provider for SlowToolProvider {
        fn name(&self) -> &'static str {
            "slow-tool"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            tokio::time::sleep(self.delay).await;
            Ok((
                Message::assistant().with_tool_request(
                    "tool_slow",
                    "bash",
                    serde_json::json!({"command": "echo hi"}),
                ),
                Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[derive(Debug)]
    struct DelayedTool {
        delay: Duration,
    }

    #[async_trait]
    impl Tool for DelayedTool {
        fn definition(&self) -> ToolDef {
            ToolDef::new(
                "slow_tool",
                "A slow test tool",
                serde_json::json!({"type": "object"}),
            )
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput> {
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutput::success("slow tool finished"))
        }
    }

    #[derive(Debug)]
    struct CancelSafeHistoryProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
    }

    impl CancelSafeHistoryProvider {
        fn new() -> Self {
            Self {
                model: ModelInfo {
                    name: "cancel-safe-history".into(),
                    context_limit: 128_000,
                },
                call_count: Mutex::new(0),
            }
        }
    }

    fn validate_tool_history(messages: &[Message]) -> Result<()> {
        for (idx, message) in messages.iter().enumerate() {
            if message.role != Role::Assistant || !message.has_tool_requests() {
                continue;
            }

            let Some(next) = messages.get(idx + 1) else {
                bail!("assistant tool_use at index {idx} has no following user message");
            };
            if next.role != Role::User {
                bail!("assistant tool_use at index {idx} is not followed by a user message");
            }

            let result_ids: HashSet<String> = next
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::ToolResult { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();

            for req in message.tool_requests() {
                if !result_ids.contains(&req.id) {
                    bail!("missing tool_result for {}", req.id);
                }
            }
        }

        Ok(())
    }

    #[async_trait]
    impl Provider for CancelSafeHistoryProvider {
        fn name(&self) -> &'static str {
            "cancel-safe-history"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let count = {
                let mut call_count = self.call_count.lock().unwrap();
                *call_count += 1;
                *call_count
            };

            if count == 1 {
                return Ok((
                    Message::assistant().with_tool_request(
                        "tool_cancelled_turn",
                        "slow_tool",
                        serde_json::json!({}),
                    ),
                    Usage::default(),
                ));
            }

            validate_tool_history(messages)?;
            Ok((
                Message::assistant().with_text("history ok"),
                Usage::default(),
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[tokio::test]
    async fn cancel_active_turn_stops_iteration() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> =
            Arc::new(SlowToolProvider::new(Duration::from_millis(50)));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(256);

        let gw = Arc::clone(&gateway);
        let sk = session_key.clone();
        let turn_task = tokio::spawn(async move {
            gw.run_turn_with_trust(
                &sk,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
        });

        // Wait for the first tool to start executing
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(gateway.has_active_turn(&session_key));

        // Cancel
        let cancelled = gateway.cancel_active_turn(&session_key);
        assert!(cancelled);

        // Wait for the turn to finish
        let result = turn_task.await.unwrap();
        assert!(result.is_ok());

        // After turn finishes, the token should be deregistered
        assert!(!gateway.has_active_turn(&session_key));

        // Collect events — we should see a Done
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, TurnEvent::Done(_)) {
                saw_done = true;
            }
        }
        assert!(saw_done, "should emit Done after cancellation");
    }

    #[tokio::test]
    async fn cancel_nonexistent_turn_returns_false() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            executor,
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        assert!(!gateway.cancel_active_turn(&session_key));
        assert!(!gateway.has_active_turn(&session_key));
    }

    #[tokio::test]
    async fn cancelled_turn_commits_tool_results_before_injected_follow_up() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(CancelSafeHistoryProvider::new());
        let mut executor = SimpleExecutor::new();
        executor.add(Box::new(DelayedTool {
            delay: Duration::from_millis(100),
        }));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(executor),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (tx1, mut rx1) = mpsc::channel(256);
        let gw1 = Arc::clone(&gateway);
        let sk1 = session_key.clone();
        let first_turn = tokio::spawn(async move {
            gw1.run_turn_with_trust(&sk1, "start", TrustLevel::Full, Some("alice"), None, tx1)
                .await
        });

        loop {
            match rx1.recv().await {
                Some(TurnEvent::ToolStart { .. }) => break,
                Some(_) => {}
                None => panic!("first turn ended before tool execution started"),
            }
        }

        gateway.inject_pending_inbound(&session_key, "follow-up during tool".into());
        assert!(gateway.cancel_active_turn(&session_key));

        first_turn.await.unwrap().unwrap();
        while rx1.try_recv().is_ok() {}

        let msgs = gateway.messages(&session_key);
        assert_eq!(
            msgs.len(),
            4,
            "expected original, assistant, tool_result, follow-up"
        );
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(
            msgs[2].has_tool_results(),
            "cancelled turn should persist tool results"
        );
        assert_eq!(msgs[3].role, Role::User);
        assert_eq!(msgs[3].text(), "follow-up during tool");

        let result_ids: HashSet<String> = msgs[2]
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolResult { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert!(result_ids.contains("tool_cancelled_turn"));

        let (tx2, mut rx2) = mpsc::channel(256);
        gateway
            .run_turn_with_trust(
                &session_key,
                "next turn",
                TrustLevel::Full,
                Some("alice"),
                None,
                tx2,
            )
            .await
            .unwrap();
        while rx2.try_recv().is_ok() {}

        let msgs = gateway.messages(&session_key);
        assert!(
            msgs.iter()
                .any(|msg| msg.role == Role::Assistant && msg.text() == "history ok"),
            "second turn should succeed against repaired history"
        );
    }

    #[tokio::test]
    async fn concurrent_turn_on_same_session_is_skipped() {
        let workspace = test_workspace();
        // SlowToolProvider takes 200ms per call — long enough for us to
        // start a second turn while the first is still running.
        let provider: Arc<dyn Provider> =
            Arc::new(SlowToolProvider::new(Duration::from_millis(200)));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();

        // Start first turn — will be slow (provider takes 200ms).
        let (tx1, mut rx1) = mpsc::channel(256);
        let gw1 = Arc::clone(&gateway);
        let sk1 = session_key.clone();
        let turn1 = tokio::spawn(async move {
            gw1.run_turn_with_trust(&sk1, "first", TrustLevel::Full, Some("alice"), None, tx1)
                .await
        });

        // Give the first turn time to acquire the lock.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Start second turn on the same session — should be skipped.
        let (tx2, mut rx2) = mpsc::channel(256);
        let gw2 = Arc::clone(&gateway);
        let sk2 = session_key.clone();
        let turn2 = tokio::spawn(async move {
            gw2.run_turn_with_trust(&sk2, "second", TrustLevel::Full, Some("alice"), None, tx2)
                .await
        });

        // Second turn should return immediately with an empty Done.
        let result2 = turn2.await.unwrap();
        assert!(result2.is_ok());

        let mut saw_done2 = false;
        let mut turn2_messages = Vec::new();
        while let Ok(event) = rx2.try_recv() {
            if let TurnEvent::Done(result) = event {
                saw_done2 = true;
                turn2_messages = result.messages;
            }
        }
        assert!(saw_done2, "second turn should emit Done");
        assert!(
            turn2_messages.is_empty(),
            "second turn should produce no messages"
        );

        // Wait for first turn to finish.
        let result1 = turn1.await.unwrap();
        assert!(result1.is_ok());

        let mut saw_done1 = false;
        while let Ok(event) = rx1.try_recv() {
            if let TurnEvent::Done(_) = event {
                saw_done1 = true;
            }
        }
        assert!(saw_done1, "first turn should emit Done");

        // Session should only contain messages from the first turn —
        // "second" user input should NOT be in the session.
        let messages = gateway.messages(&session_key);
        let has_first = messages.iter().any(|m| m.text().contains("first"));
        let has_second = messages.iter().any(|m| m.text().contains("second"));
        assert!(has_first, "session should have first turn's message");
        assert!(
            !has_second,
            "session should NOT have second turn's message (it was skipped)"
        );
    }

    #[tokio::test]
    async fn assistant_message_event_only_emits_terminal_reply() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(SequencedProvider::new(vec![
            Message::assistant()
                .with_text("before tool")
                .with_tool_request("tool_1", "fake_tool", serde_json::json!({})),
            Message::assistant().with_text("after tool"),
        ]));
        let mut executor = SimpleExecutor::new();
        executor.add(Box::new(FakeTool::new("fake_tool", "ok")));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(executor),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(256);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        let mut assistant_messages = Vec::new();
        let mut done_messages = None;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::AssistantMessage(message) => assistant_messages.push(message),
                TurnEvent::Done(result) => done_messages = Some(result.messages),
                _ => {}
            }
        }

        assert_eq!(
            assistant_messages.len(),
            1,
            "only the terminal assistant reply should be emitted as an AssistantMessage event"
        );
        assert_eq!(assistant_messages[0].text(), "after tool");

        let done_messages = done_messages.expect("turn should emit Done");
        assert_eq!(
            done_messages.len(),
            1,
            "Done should only return the terminal assistant reply"
        );
        assert_eq!(done_messages[0].text(), "after tool");
    }

    #[tokio::test]
    async fn signal_turn_repairs_empty_terminal_reply_after_tool_use() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(SequencedProvider::new(vec![
            Message::assistant()
                .with_text("before tool")
                .with_tool_request("tool_1", "fake_tool", serde_json::json!({})),
            Message::assistant().with_text("   "),
            Message::assistant().with_text("All set."),
        ]));
        let mut executor = SimpleExecutor::new();
        executor.add(Box::new(FakeTool::new("fake_tool", "ok")));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(executor),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(256);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                Some("signal"),
                event_tx,
            )
            .await
            .unwrap();

        let mut assistant_messages = Vec::new();
        let mut done_messages = None;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::AssistantMessage(message) => assistant_messages.push(message),
                TurnEvent::Done(result) => done_messages = Some(result.messages),
                _ => {}
            }
        }

        assert_eq!(assistant_messages.len(), 1);
        assert_eq!(assistant_messages[0].text(), "All set.");

        let done_messages = done_messages.expect("turn should emit Done");
        assert_eq!(done_messages.len(), 1);
        assert_eq!(done_messages[0].text(), "All set.");
    }

    #[tokio::test]
    async fn cron_always_turn_repairs_empty_terminal_reply_after_tool_use() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(SequencedProvider::new(vec![
            Message::assistant()
                .with_text("checking status")
                .with_tool_request("tool_1", "fake_tool", serde_json::json!({})),
            Message::assistant().with_text(""),
            Message::assistant().with_text("Scheduled update ready."),
        ]));
        let mut executor = SimpleExecutor::new();
        executor.add(Box::new(FakeTool::new("fake_tool", "ok")));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(executor),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Cron("briefing".to_owned()),
        };
        let (event_tx, mut event_rx) = mpsc::channel(256);

        gateway
            .run_turn_with_trust_and_cron_delivery(
                &session_key,
                "check status",
                TrustLevel::Full,
                Some("alice"),
                Some("signal"),
                Some(CronDeliveryMode::Always),
                event_tx,
            )
            .await
            .unwrap();

        let mut assistant_messages = Vec::new();
        let mut done_messages = None;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::AssistantMessage(message) => assistant_messages.push(message),
                TurnEvent::Done(result) => done_messages = Some(result.messages),
                _ => {}
            }
        }

        assert_eq!(assistant_messages.len(), 1);
        assert_eq!(assistant_messages[0].text(), "Scheduled update ready.");

        let done_messages = done_messages.expect("turn should emit Done");
        assert_eq!(done_messages.len(), 1);
        assert_eq!(done_messages[0].text(), "Scheduled update ready.");
    }

    #[tokio::test]
    async fn hit_limit_forces_final_text_completion() {
        let workspace = test_workspace();
        let provider = Arc::new(AlwaysToolUseProvider::new());
        let provider_ref = Arc::clone(&provider);
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider as Arc<dyn Provider>),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(256);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        // 40 iterations + 1 forced final completion = 41 provider calls
        assert_eq!(provider_ref.calls(), 41);

        let mut assistant_messages = Vec::new();
        let mut hit_limit = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::AssistantMessage(msg) => assistant_messages.push(msg),
                TurnEvent::Done(result) => hit_limit = result.hit_limit,
                _ => {}
            }
        }

        assert!(hit_limit, "should report hit_limit=true");

        // The last assistant message should be the forced text completion
        let last = assistant_messages.last().expect("should have messages");
        assert!(
            !last.text().is_empty(),
            "final message should contain text summary"
        );
        assert!(
            last.text().contains("iteration limit"),
            "final message should be the forced completion: {}",
            last.text()
        );
        assert!(
            !last.has_tool_requests(),
            "final message should not request tools"
        );

        // The session should contain the injected limit explanation message
        let session_msgs = gateway.messages(&session_key);
        let has_limit_msg = session_msgs
            .iter()
            .any(|m| m.role == Role::User && m.text().contains("maximum number of tool-call"));
        assert!(
            has_limit_msg,
            "session should contain the limit explanation user message"
        );
    }

    /// Provider that always returns tool_use but fails when called with no tools
    /// (simulating a failed forced completion at the iteration limit).
    #[derive(Debug)]
    struct FailOnLimitCompletionProvider {
        model: ModelInfo,
    }

    #[async_trait]
    impl Provider for FailOnLimitCompletionProvider {
        fn name(&self) -> &'static str {
            "fail-on-limit"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let usage = Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..Default::default()
            };
            if tools.is_empty() {
                anyhow::bail!("API error during limit completion")
            }
            Ok((
                Message::assistant().with_tool_request(
                    "tool_1",
                    "bash",
                    serde_json::json!({"command": "echo hi"}),
                ),
                usage,
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[tokio::test]
    async fn hit_limit_completion_failure_emits_error_event() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailOnLimitCompletionProvider {
            model: ModelInfo {
                name: "fail-on-limit".into(),
                context_limit: 128_000,
            },
        });
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(256);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        let mut saw_error = false;
        let mut saw_done = false;
        let mut hit_limit = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(msg) => {
                    saw_error = true;
                    assert!(
                        msg.contains("iteration limit"),
                        "error should mention iteration limit: {msg}"
                    );
                }
                TurnEvent::Done(result) => {
                    saw_done = true;
                    hit_limit = result.hit_limit;
                }
                _ => {}
            }
        }

        assert!(hit_limit, "should report hit_limit=true");
        assert!(
            saw_error,
            "should emit error event when limit completion fails"
        );
        assert!(saw_done, "should still emit Done after error");
    }

    #[tokio::test]
    async fn resolve_main_model_picks_up_config_change() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "test-model", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "new-model".to_owned(),
            Arc::new(FakeProvider::with_model("hello", "new-model", 256_000)),
        );
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(test_config());

        let gateway = Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            providers,
            executor,
            None,
            None,
        )
        .unwrap();

        let mut new_config = shared.load().as_ref().clone();
        new_config.agent.model = "new-model".to_owned();
        shared.store(Arc::new(new_config));

        let resolved = gateway.resolve_main_model(None).unwrap();
        assert_eq!(resolved.model, "new-model");
        assert_eq!(resolved.context_limit, 256_000);
        assert_eq!(gateway.configured_model_name_for_user(None), "new-model");
    }

    #[tokio::test]
    async fn set_user_model_overrides_default_per_user() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "test-model", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "anthropic/claude-opus-4-0-20250514".to_owned(),
            Arc::new(FakeProvider::with_model(
                "hello",
                "anthropic/claude-opus-4-0-20250514",
                200_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"

[provider]
name = "anthropic"
models = ["anthropic/claude-opus-4-0-20250514"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let selection = gateway
            .set_user_model(Some("alice"), "anthropic/claude-opus-4-0-20250514")
            .unwrap();
        assert_eq!(selection.model, "anthropic/claude-opus-4-0-20250514");
        assert_eq!(gateway.model_name_for_user(Some("alice")), selection.model);
        assert_eq!(gateway.model_name_for_user(Some("bob")), "test-model");
    }

    #[tokio::test]
    async fn user_configured_model_is_used_before_runtime_override() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "test-model", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "anthropic/claude-opus-4-0-20250514".to_owned(),
            Arc::new(FakeProvider::with_model(
                "hello",
                "anthropic/claude-opus-4-0-20250514",
                200_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"

[[users]]
name = "alice"
trust = "full"
model = "anthropic/claude-opus-4-0-20250514"

[provider]
name = "anthropic"
models = ["anthropic/claude-opus-4-0-20250514"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            gateway.configured_model_name_for_user(Some("alice")),
            "anthropic/claude-opus-4-0-20250514"
        );
        assert_eq!(
            gateway.model_name_for_user(Some("alice")),
            "anthropic/claude-opus-4-0-20250514"
        );
        assert_eq!(gateway.model_name_for_user(Some("bob")), "test-model");
    }

    #[tokio::test]
    async fn set_user_model_switches_across_configured_providers() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> = Arc::new(FakeProvider::with_model(
            "hello",
            "anthropic/claude-sonnet-4-20250514",
            200_000,
        ));
        let mut providers = registry(primary);
        providers.register(
            "gpt-5-codex".to_owned(),
            Arc::new(FakeProvider::with_model("hello", "gpt-5-codex", 128_000)),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "anthropic/claude-sonnet-4-20250514"

[[providers]]
name = "anthropic"
models = ["anthropic/claude-sonnet-4-20250514"]

[[providers]]
name = "openai"
models = ["gpt-5-codex"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let selection = gateway
            .set_user_model(Some("alice"), "gpt-5-codex")
            .unwrap();

        assert_eq!(selection.model, "gpt-5-codex");
        assert_eq!(selection.context_limit, 128_000);
        assert_eq!(gateway.model_name_for_user(Some("alice")), "gpt-5-codex");
    }

    #[tokio::test]
    async fn set_user_model_rejects_non_tool_model_when_session_has_tool_history() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "gpt-5.4", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "gemini-specialist".to_owned(),
            Arc::new(FakeProvider::with_model(
                "hello",
                "gemini-specialist",
                128_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "gpt-5.4"

[[providers]]
name = "openai"
models = ["gpt-5.4"]

[[providers]]
name = "gemini"
models = ["gemini-specialist"]

[providers.model_capabilities."gemini-specialist"]
supports_tools = false
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        gateway.append_message(&session_key, Message::user().with_text("do work"));
        gateway.append_message(
            &session_key,
            Message::assistant().with_tool_request("call_1", "bash", serde_json::json!({})),
        );
        gateway.append_message(
            &session_key,
            Message::user().with_tool_result("call_1", "ok", false),
        );

        let error = gateway
            .set_user_model_for_session(
                &session_key,
                TrustLevel::Full,
                Some("alice"),
                None,
                "gemini-specialist",
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot switch this session to non-tool-capable model")
        );
    }

    #[tokio::test]
    async fn run_turn_omits_tools_and_images_for_model_capabilities() {
        let workspace = test_workspace();
        let image_path = workspace.path().join("input.png");
        std::fs::write(
            &image_path,
            [
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
                0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0x60, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x2B, 0x09, 0x4D, 0x84, 0x00,
                0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ],
        )
        .unwrap();

        let recording = Arc::new(RecordingProvider::new("test-model", "done"));
        let provider = Arc::clone(&recording) as Arc<dyn Provider>;
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"

[provider]
name = "openai"
models = ["test-model"]

[provider.model_capabilities."test-model"]
supports_tools = false
input_modalities = ["text"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);
        gateway
            .run_turn_with_trust(
                &session_key,
                &format!("please inspect {}", image_path.display()),
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        assert!(recording.last_tool_names().is_empty());
        assert!(recording.last_messages().iter().all(|message| {
            message
                .content
                .iter()
                .all(|content| !matches!(content, Content::Image { .. }))
        }));
    }

    #[tokio::test]
    async fn set_user_model_clearing_default_removes_override() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "test-model", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "anthropic/claude-opus-4-0-20250514".to_owned(),
            Arc::new(FakeProvider::with_model(
                "hello",
                "anthropic/claude-opus-4-0-20250514",
                200_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"

[provider]
name = "anthropic"
models = ["anthropic/claude-opus-4-0-20250514"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        gateway
            .set_user_model(Some("alice"), "anthropic/claude-opus-4-0-20250514")
            .unwrap();
        let selection = gateway.set_user_model(Some("alice"), "test-model").unwrap();

        assert_eq!(selection.model, "test-model");
        assert_eq!(gateway.model_name_for_user(Some("alice")), "test-model");
    }

    #[tokio::test]
    async fn set_user_model_clearing_user_configured_default_removes_override() {
        let workspace = test_workspace();
        let primary: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("hello", "test-model", 128_000));
        let mut providers = registry(primary);
        providers.register(
            "anthropic/claude-opus-4-0-20250514".to_owned(),
            Arc::new(FakeProvider::with_model(
                "hello",
                "anthropic/claude-opus-4-0-20250514",
                200_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"

[[users]]
name = "alice"
trust = "full"
model = "anthropic/claude-opus-4-0-20250514"

[provider]
name = "anthropic"
models = ["anthropic/claude-opus-4-0-20250514"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let selection = gateway.set_user_model(Some("alice"), "test-model").unwrap();
        assert_eq!(selection.model, "test-model");
        assert_eq!(gateway.model_name_for_user(Some("alice")), "test-model");

        let selection = gateway
            .set_user_model(Some("alice"), "anthropic/claude-opus-4-0-20250514")
            .unwrap();
        assert_eq!(selection.model, "anthropic/claude-opus-4-0-20250514");
        assert_eq!(
            gateway.model_name_for_user(Some("alice")),
            "anthropic/claude-opus-4-0-20250514"
        );

        let store = std::fs::read_to_string(workspace.path().join("user-models.json")).unwrap();
        assert_eq!(store.trim(), "{}");
    }

    #[derive(Debug)]
    struct HandoffCompactionProvider {
        model: ModelInfo,
        compaction_calls: Arc<Mutex<u32>>,
    }

    impl HandoffCompactionProvider {
        fn new(
            model_name: impl Into<String>,
            context_limit: usize,
            compaction_calls: Arc<Mutex<u32>>,
        ) -> Self {
            Self {
                model: ModelInfo {
                    name: model_name.into(),
                    context_limit,
                },
                compaction_calls,
            }
        }
    }

    #[async_trait]
    impl Provider for HandoffCompactionProvider {
        fn name(&self) -> &'static str {
            "handoff-compaction"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let is_compaction_call = messages
                .last()
                .is_some_and(|message| message.text().contains("continuation summary"));

            if is_compaction_call {
                *self.compaction_calls.lock().unwrap() += 1;
                return Ok((
                    Message::assistant().with_text("<summary>handoff summary</summary>"),
                    Usage {
                        input_tokens: Some(40_000),
                        output_tokens: Some(200),
                        ..Default::default()
                    },
                ));
            }

            Ok((Message::assistant().with_text("ok"), Usage::default()))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[tokio::test]
    async fn set_user_model_for_session_compacts_before_lower_context_handoff() {
        let workspace = test_workspace();
        let compaction_calls = Arc::new(Mutex::new(0));
        let primary: Arc<dyn Provider> = Arc::new(HandoffCompactionProvider::new(
            "gpt-5.4",
            200_000,
            Arc::clone(&compaction_calls),
        ));
        let mut providers = registry(primary);
        providers.register(
            "gemma-4-31B-it-UD-Q8_K_XL.gguf".to_owned(),
            Arc::new(FakeProvider::with_model(
                "ok",
                "gemma-4-31B-it-UD-Q8_K_XL.gguf",
                32_000,
            )),
        );
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "gpt-5.4"

[[providers]]
name = "openai"
models = ["gpt-5.4"]

[[providers]]
name = "openai-compatible"
base_url = "http://localhost:11434/v1"
models = ["gemma-4-31B-it-UD-Q8_K_XL.gguf"]
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            shared_config(config),
            workspace.path().to_path_buf(),
            providers,
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        gateway.append_message(&session_key, Message::user().with_text("old user message"));
        gateway.append_message(
            &session_key,
            Message::assistant().with_text("old assistant message"),
        );
        gateway.set_session_usage(
            &session_key,
            SessionUsage {
                cumulative: Usage::default(),
                last_input_tokens: 150_000,
            },
        );

        let outcome = gateway
            .set_user_model_for_session(
                &session_key,
                TrustLevel::Full,
                Some("alice"),
                Some("signal"),
                "gemma-4-31B-it-UD-Q8_K_XL.gguf",
            )
            .await
            .unwrap();

        assert_eq!(outcome.selection.model, "gemma-4-31B-it-UD-Q8_K_XL.gguf");
        assert_eq!(outcome.selection.context_limit, 32_000);
        assert!(outcome.compacted_for_handoff);
        assert_eq!(*compaction_calls.lock().unwrap(), 1);
        assert!(gateway.get_compaction(&session_key).is_some());
        assert_eq!(
            gateway.model_name_for_user(Some("alice")),
            "gemma-4-31B-it-UD-Q8_K_XL.gguf"
        );
    }

    #[test]
    fn repair_dangling_tool_use_appends_synthetic_results() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();

        // Simulate a corrupt session: assistant message with tool_use but no tool_result
        gateway.append_message(&session_key, Message::user().with_text("do something"));
        gateway.append_message(
            &session_key,
            Message::assistant()
                .with_tool_request("tool_a", "bash", serde_json::json!({"command": "echo hi"}))
                .with_tool_request("tool_b", "read_file", serde_json::json!({"path": "x.txt"})),
        );

        assert_eq!(gateway.messages(&session_key).len(), 2);

        gateway.repair_dangling_tool_use(&session_key);

        let msgs = gateway.messages(&session_key);
        assert_eq!(msgs.len(), 3);

        let repair_msg = &msgs[2];
        assert_eq!(repair_msg.role, Role::User);
        assert!(repair_msg.has_tool_results());

        let content_strs: Vec<_> = repair_msg
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolResult { id, is_error, .. } => Some((id.clone(), *is_error)),
                _ => None,
            })
            .collect();

        assert_eq!(content_strs.len(), 2);
        assert_eq!(content_strs[0].0, "tool_a");
        assert!(content_strs[0].1, "should be marked as error");
        assert_eq!(content_strs[1].0, "tool_b");
        assert!(content_strs[1].1, "should be marked as error");
    }

    #[test]
    fn repair_dangling_tool_use_repairs_orphan_before_follow_up_user_message() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        gateway.append_message(&session_key, Message::user().with_text("do something"));
        gateway.append_message(
            &session_key,
            Message::assistant().with_tool_request(
                "tool_a",
                "bash",
                serde_json::json!({"command": "echo hi"}),
            ),
        );
        gateway.append_message(
            &session_key,
            Message::user().with_text("follow-up question"),
        );

        gateway.repair_dangling_tool_use(&session_key);

        let msgs = gateway.messages(&session_key);
        assert_eq!(
            msgs.len(),
            3,
            "repair should augment the next user message in place"
        );
        assert_eq!(msgs[2].role, Role::User);
        assert_eq!(msgs[2].text(), "follow-up question");
        assert!(msgs[2].has_tool_results());

        let tool_results: Vec<_> = msgs[2]
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolResult { id, is_error, .. } => Some((id.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results, vec![("tool_a".to_owned(), true)]);
    }

    #[test]
    fn repair_dangling_tool_use_noop_when_session_is_clean() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();

        // Clean session: assistant with tool_use followed by user with tool_result
        gateway.append_message(&session_key, Message::user().with_text("do something"));
        gateway.append_message(
            &session_key,
            Message::assistant().with_tool_request(
                "tool_a",
                "bash",
                serde_json::json!({"command": "echo hi"}),
            ),
        );
        gateway.append_message(
            &session_key,
            Message::user().with_tool_result("tool_a", "hello", false),
        );

        assert_eq!(gateway.messages(&session_key).len(), 3);

        gateway.repair_dangling_tool_use(&session_key);

        // No change — session was already clean
        assert_eq!(gateway.messages(&session_key).len(), 3);
    }

    #[test]
    fn repair_dangling_tool_use_noop_on_empty_session() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        gateway.repair_dangling_tool_use(&session_key);
        assert!(gateway.messages(&session_key).is_empty());
    }

    /// Provider that reports high input tokens to trigger compaction.
    /// On non-tool calls (i.e. compaction summarization), returns a summary.
    /// On tool calls, returns a tool_use on the first call then text on subsequent calls.
    #[derive(Debug)]
    struct HighTokenProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
        input_tokens: u32,
    }

    impl HighTokenProvider {
        fn new(input_tokens: u32) -> Self {
            Self {
                model: ModelInfo {
                    name: "high-token".into(),
                    context_limit: 200_000,
                },
                call_count: Mutex::new(0),
                input_tokens,
            }
        }

        fn calls(&self) -> u32 {
            *self.call_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for HighTokenProvider {
        fn name(&self) -> &'static str {
            "high-token"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            messages: &[Message],
            tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;

            let is_compaction_call = messages
                .last()
                .is_some_and(|m| m.text().contains("continuation summary"));

            if is_compaction_call {
                return Ok((
                    Message::assistant()
                        .with_text("<summary>Compacted summary of conversation.</summary>"),
                    Usage {
                        input_tokens: Some(self.input_tokens),
                        output_tokens: Some(500),
                        ..Default::default()
                    },
                ));
            }

            let usage = Usage {
                input_tokens: Some(self.input_tokens),
                output_tokens: Some(500),
                ..Default::default()
            };

            if !tools.is_empty() && *count == 1 {
                // First real call: return tool_use to keep the turn going
                Ok((
                    Message::assistant()
                        .with_text("Let me check.")
                        .with_tool_request(
                            "tool_1",
                            "bash",
                            serde_json::json!({"command": "echo hi"}),
                        ),
                    usage,
                ))
            } else {
                // Subsequent calls or no tools: return text
                Ok((Message::assistant().with_text("Done."), usage))
            }
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[tokio::test]
    async fn pre_turn_compaction_fires_before_user_message() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(HighTokenProvider::new(200_000));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();

        // Seed the session with some messages so there's something to compact.
        gateway.append_message(&session_key, Message::user().with_text("first message"));
        gateway.append_message(
            &session_key,
            Message::assistant().with_text("first response"),
        );

        // Simulate that the previous turn used high input tokens — this is
        // what maybe_compact checks to decide whether to compact.
        gateway
            .session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .entry(session_key.clone())
            .or_default()
            .last_input_tokens = 200_000;

        let (event_tx, mut event_rx) = mpsc::channel(128);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "second message",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        // Collect events
        let mut saw_compacting = false;
        let mut saw_done = false;
        let mut saw_text = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Compacting => saw_compacting = true,
                TurnEvent::Done(_) => saw_done = true,
                TurnEvent::TextDelta(t) if !t.is_empty() => saw_text = true,
                _ => {}
            }
        }

        assert!(saw_compacting, "should emit Compacting event");
        assert!(
            saw_done,
            "should emit Done event (turn continues after compaction)"
        );
        assert!(saw_text, "should produce a text response after compaction");

        // Compaction state should exist
        assert!(
            gateway.get_compaction(&session_key).is_some(),
            "compaction state should be persisted"
        );
    }

    #[tokio::test]
    async fn pre_turn_compaction_uses_estimated_history_when_usage_missing() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> =
            Arc::new(FakeProvider::with_model("ok", "test-model", 32_000));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let large_text = "x".repeat(20_000);
        gateway.append_message(&session_key, Message::user().with_text(&large_text));
        gateway.append_message(&session_key, Message::assistant().with_text(&large_text));
        gateway.append_message(&session_key, Message::user().with_text(&large_text));
        gateway.append_message(&session_key, Message::assistant().with_text(&large_text));

        assert_eq!(gateway.session_usage(&session_key).last_input_tokens, 0);

        let (event_tx, mut event_rx) = mpsc::channel(128);
        gateway
            .run_turn_with_trust(
                &session_key,
                "second message",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        let mut saw_compacting = false;
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Compacting => saw_compacting = true,
                TurnEvent::Done(_) => saw_done = true,
                _ => {}
            }
        }

        assert!(
            saw_compacting,
            "should compact based on estimated history even without cached usage"
        );
        assert!(saw_done, "turn should still complete after compaction");
        assert!(gateway.get_compaction(&session_key).is_some());
    }

    #[tokio::test]
    async fn mid_turn_compaction_fires_between_iterations() {
        let workspace = test_workspace();
        // Use tokens over the threshold so compaction fires after the first
        // iteration's provider response + tool results are appended.
        let provider = Arc::new(HighTokenProvider::new(200_000));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(Arc::clone(&provider) as Arc<dyn Provider>),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(128);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "do something",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        // Collect events in order
        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }

        let saw_compacting = events.iter().any(|e| matches!(e, TurnEvent::Compacting));
        let saw_tool_start = events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolStart { .. }));
        let saw_done = events.iter().any(|e| matches!(e, TurnEvent::Done(_)));

        assert!(saw_tool_start, "should have a tool execution");
        assert!(
            saw_compacting,
            "compaction should fire mid-turn after first iteration"
        );
        assert!(saw_done, "turn should complete after compaction");

        // The compaction event should come after tool execution but before Done
        let compact_idx = events
            .iter()
            .position(|e| matches!(e, TurnEvent::Compacting))
            .unwrap();
        let done_idx = events
            .iter()
            .position(|e| matches!(e, TurnEvent::Done(_)))
            .unwrap();
        let tool_idx = events
            .iter()
            .position(|e| matches!(e, TurnEvent::ToolStart { .. }))
            .unwrap();

        assert!(
            compact_idx > tool_idx,
            "compaction should come after tool execution"
        );
        assert!(
            compact_idx < done_idx,
            "compaction should come before Done (not terminal)"
        );

        // Verify the user gets a text response AFTER compaction — the turn
        // must not leave the user hanging.
        let post_compaction_assistant = events[compact_idx..]
            .iter()
            .any(|e| matches!(e, TurnEvent::AssistantMessage(msg) if !msg.text().is_empty()));
        assert!(
            post_compaction_assistant,
            "user must receive an assistant message after compaction"
        );

        // Provider should have been called more than once:
        // 1. First iteration (tool_use) 2. Compaction summarization 3. Second iteration (text)
        assert!(
            provider.calls() >= 3,
            "expected at least 3 provider calls, got {}",
            provider.calls()
        );

        // Compaction state should exist
        assert!(
            gateway.get_compaction(&session_key).is_some(),
            "compaction state should be persisted"
        );
    }

    #[tokio::test]
    async fn no_compaction_when_below_threshold() {
        let workspace = test_workspace();
        // Use low token count — should NOT trigger compaction
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(128);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        let mut saw_compacting = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, TurnEvent::Compacting) {
                saw_compacting = true;
            }
        }

        assert!(!saw_compacting, "should NOT compact when below threshold");
        assert!(
            gateway.get_compaction(&session_key).is_none(),
            "no compaction state should exist"
        );
    }

    #[tokio::test]
    async fn skills_added_after_startup_appear_in_next_turn() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        // No skills at startup.
        assert!(
            gateway
                .skills
                .lock()
                .expect("skills mutex poisoned")
                .is_empty(),
            "should start with no skills"
        );

        // Run a turn — prompt should have no skills section.
        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);
        gateway
            .run_turn_with_trust(
                &session_key,
                "first",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();
        while event_rx.try_recv().is_ok() {}

        // Add a skill to the workspace while the gateway is running.
        let skill_dir = workspace.path().join("skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\nSkill content here.\n",
        )
        .unwrap();

        // Clear the session so we can run a fresh turn.
        gateway.clear_session(&session_key);

        // Run another turn — the new skill should be picked up.
        let (tx, mut rx) = mpsc::channel(32);
        gateway
            .run_turn_with_trust(
                &session_key,
                "second",
                TrustLevel::Full,
                Some("alice"),
                None,
                tx,
            )
            .await
            .unwrap();
        while rx.try_recv().is_ok() {}

        // The cached skills should now include the new skill.
        let cached = gateway.skills.lock().expect("skills mutex poisoned");
        assert_eq!(cached.len(), 1, "should have picked up the new skill");
        assert_eq!(cached[0].name, "test-skill");
        drop(cached);
    }

    // -----------------------------------------------------------------------
    // Provider that returns cache tokens in usage, simulating prompt caching.
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct CachingProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
    }

    impl CachingProvider {
        fn new() -> Self {
            Self {
                model: ModelInfo {
                    name: "caching-model".into(),
                    context_limit: 200_000,
                },
                call_count: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for CachingProvider {
        fn name(&self) -> &'static str {
            "caching"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            let call = *count;
            drop(count);

            if call == 1 {
                // First call: tool use with cache write (cold cache).
                // input_tokens=500 (non-cached), cache_write=9000, cache_read=0
                // Real context = 9500
                Ok((
                    Message::assistant().with_tool_request(
                        "tool_1",
                        "bash",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                    Usage {
                        input_tokens: Some(500),
                        output_tokens: Some(80),
                        cache_read_tokens: None,
                        cache_write_tokens: Some(9000),
                        ..Default::default()
                    },
                ))
            } else if !tools.is_empty() {
                // Second call (with tools): cache hit.
                // input_tokens=200 (non-cached new tokens), cache_read=9000
                // Real context = 9200
                Ok((
                    Message::assistant().with_text("Done with the task."),
                    Usage {
                        input_tokens: Some(200),
                        output_tokens: Some(60),
                        cache_read_tokens: Some(9000),
                        cache_write_tokens: None,
                        ..Default::default()
                    },
                ))
            } else {
                // Forced final completion (no tools)
                Ok((
                    Message::assistant().with_text("Summary."),
                    Usage {
                        input_tokens: Some(150),
                        output_tokens: Some(40),
                        cache_read_tokens: Some(9000),
                        cache_write_tokens: None,
                        ..Default::default()
                    },
                ))
            }
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    /// Verify that last_input_tokens includes cache_read + cache_write
    /// tokens, not just the non-cached input_tokens field.
    #[tokio::test]
    async fn last_input_tokens_includes_cache_tokens() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(CachingProvider::new());
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(128);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();
        while event_rx.try_recv().is_ok() {}

        let usage = gateway.session_usage(&session_key);

        // The last iteration (call 2) returns:
        //   input_tokens=200, cache_read=9000 → context = 9200
        // Without the fix, this would be just 200.
        assert_eq!(
            usage.last_input_tokens, 9200,
            "last_input_tokens must include cache_read + cache_write tokens"
        );
    }

    /// Verify that record_turn_usage does NOT overwrite last_input_tokens
    /// with the cumulative total across all iterations.
    #[tokio::test]
    async fn record_turn_usage_does_not_overwrite_last_input_tokens() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(CachingProvider::new());
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(128);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();
        while event_rx.try_recv().is_ok() {}

        let usage = gateway.session_usage(&session_key);

        // Cumulative input_tokens across both iterations: 500 + 200 = 700.
        // Cumulative cache: 9000 (write) + 9000 (read) = 18000.
        // If record_turn_usage overwrote last_input_tokens with cumulative
        // context_input_tokens, it would be 700 + 18000 = 18700 (wrong).
        // Correct value is the LAST iteration's context: 200 + 9000 = 9200.
        assert_ne!(
            usage.last_input_tokens, 18700,
            "must not be the cumulative total across iterations"
        );
        assert_eq!(
            usage.last_input_tokens, 9200,
            "must be the last iteration's context size only"
        );

        // Cumulative should correctly sum ALL tokens across iterations.
        assert_eq!(usage.cumulative.input_tokens, Some(700));
        assert_eq!(usage.cumulative.output_tokens, Some(140));
    }

    #[derive(Debug)]
    struct TruncatedToolUseStreamingProvider {
        model: ModelInfo,
    }

    impl TruncatedToolUseStreamingProvider {
        fn new() -> Self {
            Self {
                model: ModelInfo {
                    name: "truncated-tool-stream".into(),
                    context_limit: 128_000,
                },
            }
        }
    }

    #[derive(Debug)]
    struct ToolUseStopWithoutToolRequestStreamingProvider {
        model: ModelInfo,
    }

    #[derive(Debug)]
    struct StreamFailsBeforeFirstChunkProvider {
        model: ModelInfo,
        stream_calls: Arc<std::sync::atomic::AtomicUsize>,
        complete_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[derive(Debug)]
    struct StreamTransientThenSuccessProvider {
        model: ModelInfo,
        stream_calls: Arc<std::sync::atomic::AtomicUsize>,
        complete_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[derive(Debug)]
    struct CompleteTransientThenSuccessProvider {
        model: ModelInfo,
        complete_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolUseStopWithoutToolRequestStreamingProvider {
        fn new() -> Self {
            Self {
                model: ModelInfo {
                    name: "tool-use-stop-without-tool-request".into(),
                    context_limit: 128_000,
                },
            }
        }
    }

    impl StreamFailsBeforeFirstChunkProvider {
        fn new(
            stream_calls: Arc<std::sync::atomic::AtomicUsize>,
            complete_calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                model: ModelInfo {
                    name: "stream-fails-before-first-chunk".into(),
                    context_limit: 128_000,
                },
                stream_calls,
                complete_calls,
            }
        }
    }

    impl StreamTransientThenSuccessProvider {
        fn new(
            stream_calls: Arc<std::sync::atomic::AtomicUsize>,
            complete_calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                model: ModelInfo {
                    name: "stream-transient-then-success".into(),
                    context_limit: 128_000,
                },
                stream_calls,
                complete_calls,
            }
        }
    }

    impl CompleteTransientThenSuccessProvider {
        fn new(complete_calls: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                model: ModelInfo {
                    name: "complete-transient-then-success".into(),
                    context_limit: 128_000,
                },
                complete_calls,
            }
        }
    }

    #[async_trait]
    impl Provider for TruncatedToolUseStreamingProvider {
        fn name(&self) -> &'static str {
            "truncated-tool-stream"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            anyhow::bail!("not supported")
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            let stream = futures::stream::once(async {
                Err(anyhow::anyhow!(
                    "Anthropic streamed tool_use for `write_file` ended with invalid/incomplete input_json after max_tokens"
                ))
            });
            Ok(Box::pin(stream))
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl Provider for ToolUseStopWithoutToolRequestStreamingProvider {
        fn name(&self) -> &'static str {
            "tool-use-stop-without-tool-request"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            anyhow::bail!("not supported")
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            let usage = Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                stop_reason: Some("tool_use".into()),
                ..Default::default()
            };
            let stream = futures::stream::once(async move {
                Ok((
                    Some(Message::assistant().with_text("Right, let me check that.")),
                    Some(usage),
                ))
            });
            Ok(Box::pin(stream))
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl Provider for StreamFailsBeforeFirstChunkProvider {
        fn name(&self) -> &'static str {
            "stream-fails-before-first-chunk"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            self.complete_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                Message::assistant().with_text("fallback response"),
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(4),
                    stop_reason: Some("stop".into()),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let stream = futures::stream::once(async {
                Err(anyhow::anyhow!("Streaming is not supported for this model"))
            });
            Ok(Box::pin(stream))
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl Provider for StreamTransientThenSuccessProvider {
        fn name(&self) -> &'static str {
            "stream-transient-then-success"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            self.complete_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                Message::assistant().with_text("should not use non-streaming fallback"),
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(4),
                    stop_reason: Some("stop".into()),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            let call = self
                .stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;

            if call < 3 {
                let stream = futures::stream::once(async {
                    Err(anyhow::anyhow!(
                        "Web stream error for model 'stream-transient-then-success'.\nCause: error sending request for url (http://10.0.0.7:11434/v1/chat/completions)"
                    ))
                });
                return Ok(Box::pin(stream));
            }

            let usage = Usage {
                input_tokens: Some(15),
                output_tokens: Some(5),
                stop_reason: Some("stop".into()),
                ..Default::default()
            };
            let stream = futures::stream::once(async move {
                Ok((
                    Some(Message::assistant().with_text("stream recovered after retry")),
                    Some(usage),
                ))
            });
            Ok(Box::pin(stream))
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl Provider for CompleteTransientThenSuccessProvider {
        fn name(&self) -> &'static str {
            "complete-transient-then-success"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let call = self
                .complete_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;

            if call < 4 {
                anyhow::bail!(
                    "Web call failed for model 'complete-transient-then-success'.\nCause: Reqwest error: error sending request for url (http://10.0.0.7:11434/v1/chat/completions)"
                );
            }

            Ok((
                Message::assistant().with_text("complete recovered after retry"),
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(4),
                    stop_reason: Some("stop".into()),
                    ..Default::default()
                },
            ))
        }

        async fn stream(
            &self,
            _system: &[String],
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("streaming disabled for this provider")
        }

        fn supports_streaming(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn retries_non_streaming_when_stream_fails_before_first_output() {
        let workspace = test_workspace();
        let stream_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StreamFailsBeforeFirstChunkProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        let messages = gateway.messages(&session_key);
        let assistant = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .expect("assistant reply present");
        assert_eq!(assistant.text(), "fallback response");
        assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let deltas = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                TurnEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            deltas
                .iter()
                .any(|delta| delta.contains("fallback response")),
            "expected non-streaming fallback text delta, got {deltas:?}"
        );
    }

    #[tokio::test]
    async fn retries_transient_stream_failures_before_falling_back() {
        let workspace = test_workspace();
        let stream_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StreamTransientThenSuccessProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        let messages = gateway.messages(&session_key);
        let assistant = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .expect("assistant reply present");
        assert_eq!(assistant.text(), "stream recovered after retry");
        assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        let saw_error = std::iter::from_fn(|| event_rx.try_recv().ok())
            .any(|event| matches!(event, TurnEvent::Error(_)));
        assert!(
            !saw_error,
            "did not expect an error event after retry success"
        );
    }

    #[tokio::test]
    async fn require_stream_policy_does_not_fallback_to_non_streaming() {
        let workspace = test_workspace();
        let stream_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StreamFailsBeforeFirstChunkProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config_with_stream_policy(StreamPolicy::Require)),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(
            result.is_ok(),
            "turn should surface an error event, not panic"
        );
        assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(gateway.messages(&session_key).is_empty());

        let mut saw_error = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, TurnEvent::Error(_)) {
                saw_error = true;
            }
        }
        assert!(saw_error, "expected turn error when fallback is disabled");
    }

    #[tokio::test]
    async fn disable_stream_policy_uses_non_streaming_path() {
        let workspace = test_workspace();
        let stream_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StreamFailsBeforeFirstChunkProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config_with_stream_policy(StreamPolicy::Disable)),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let messages = gateway.messages(&session_key);
        assert_eq!(
            messages.last().map(Message::text),
            Some("fallback response".to_owned())
        );
    }

    #[tokio::test]
    async fn retries_transient_non_stream_failures_before_erroring() {
        let workspace = test_workspace();
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(CompleteTransientThenSuccessProvider::new(
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config_with_stream_policy(StreamPolicy::Disable)),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        let messages = gateway.messages(&session_key);
        assert_eq!(
            messages.last().map(Message::text),
            Some("complete recovered after retry".to_owned())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_records_non_stream_retry_before_success() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let _trace_guard = trace_test_guard().await;

        let dir = tempfile::tempdir().unwrap();
        let trace_file = dir.path().join("traces.jsonl");

        let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let jsonl_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let workspace = test_workspace();
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(CompleteTransientThenSuccessProvider::new(
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config_with_stream_policy(StreamPolicy::Disable)),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 4);

        drop(default_guard);
        drop(guard);

        let trace = read_trace_file_with_retry(
            &trace_file,
            "non-streaming provider request failed before output, retrying",
        );
        assert!(
            trace.contains("non-streaming provider request failed before output, retrying"),
            "expected non-stream retry trace"
        );
        assert!(
            trace.contains("\"backoff_ms\":"),
            "expected non-stream retry backoff field"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_records_early_stream_retry_before_success() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let _trace_guard = trace_test_guard().await;

        let dir = tempfile::tempdir().unwrap();
        let trace_file = dir.path().join("traces.jsonl");

        let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let jsonl_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let workspace = test_workspace();
        let stream_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StreamTransientThenSuccessProvider::new(
            Arc::clone(&stream_calls),
            Arc::clone(&complete_calls),
        ));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(complete_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        drop(default_guard);
        drop(guard);

        let trace = read_trace_file_with_retry(&trace_file, "retrying stream");
        assert!(
            trace
                .contains("streaming provider request failed before first output, retrying stream"),
            "expected stream retry trace"
        );
        assert!(
            trace.contains("\"stream_policy\":\"prefer\""),
            "expected stream policy trace field"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_records_provider_request_size_fields() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let _trace_guard = trace_test_guard().await;

        let dir = tempfile::tempdir().unwrap();
        let trace_file = dir.path().join("traces.jsonl");

        let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let jsonl_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(32);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        drop(default_guard);
        drop(guard);

        let trace = read_trace_file_with_retry(&trace_file, "request_estimated_bytes");
        assert!(
            trace.contains("\"request_estimated_bytes\":"),
            "expected request_estimated_bytes trace field"
        );
        assert!(
            trace.contains("\"request_message_chars\":"),
            "expected request_message_chars trace field"
        );
        assert!(
            trace.contains("\"request_tool_schema_bytes\":"),
            "expected request_tool_schema_bytes trace field"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_does_not_log_missing_required_parameter_after_truncated_tool_stream() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let _trace_guard = trace_test_guard().await;

        let dir = tempfile::tempdir().unwrap();
        let trace_file = dir.path().join("traces.jsonl");

        let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let jsonl_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(TruncatedToolUseStreamingProvider::new());
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());
        let mut saw_error_event = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, TurnEvent::Error(_)) {
                saw_error_event = true;
            }
        }
        assert!(saw_error_event, "expected turn error event");

        drop(default_guard);
        drop(guard);

        let all_text = read_trace_file_with_retry(
            &trace_file,
            "invalid/incomplete input_json after max_tokens",
        );
        assert!(
            all_text.contains("invalid/incomplete input_json after max_tokens"),
            "expected truncated tool-call error in trace"
        );
        assert!(
            !all_text.contains("tool execution failed"),
            "turn trace should not execute a truncated tool call"
        );
        assert!(
            !all_text.contains("missing required parameter"),
            "turn trace should not report missing required params from a truncated tool call"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_use_stop_without_tool_requests_emits_error_and_rolls_back() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> =
            Arc::new(ToolUseStopWithoutToolRequestStreamingProvider::new());
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await;

        assert!(result.is_ok());

        let mut saw_error_event = false;
        let mut saw_assistant_message = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(message) => {
                    saw_error_event = true;
                    assert!(
                        message.contains("stop_reason=tool_use")
                            || message.contains("no tool requests were parsed")
                    );
                }
                TurnEvent::AssistantMessage(_) => saw_assistant_message = true,
                _ => {}
            }
        }

        assert!(saw_error_event, "expected turn error event");
        assert!(
            !saw_assistant_message,
            "should not emit an assistant message for an impossible tool_use stop"
        );
        assert!(
            gateway.messages(&session_key).is_empty(),
            "failed turn should roll back session state"
        );
    }

    #[test]
    fn inject_pending_inbound_queues_and_drains() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("ok"));
        let gateway = Gateway::new(
            shared_config(test_config()),
            workspace.path().to_path_buf(),
            registry(provider),
            Arc::new(DefaultExecutor::new()),
            None,
            None,
        )
        .unwrap();

        let key = gateway.default_session_key();

        // Nothing pending initially — drain is a no-op.
        gateway.drain_pending_inbound(&key);
        assert!(gateway.messages(&key).is_empty());

        // Inject two messages.
        gateway.inject_pending_inbound(&key, "first message".into());
        gateway.inject_pending_inbound(&key, "second message".into());

        // Not yet in the session — only in the pending queue.
        assert!(gateway.messages(&key).is_empty());

        // Drain moves them into the session.
        gateway.drain_pending_inbound(&key);
        let msgs = gateway.messages(&key);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "first message");
        assert_eq!(msgs[1].text(), "second message");

        // Second drain is a no-op.
        gateway.drain_pending_inbound(&key);
        assert_eq!(gateway.messages(&key).len(), 2);
    }

    #[tokio::test]
    async fn mid_turn_injected_messages_appear_in_session() {
        use coop_core::fakes::SlowFakeProvider;

        let workspace = test_workspace();
        let delay = Duration::from_millis(200);
        let provider: Arc<dyn Provider> = Arc::new(SlowFakeProvider::new("slow reply", delay));
        let gateway = Arc::new(
            Gateway::new(
                shared_config(test_config()),
                workspace.path().to_path_buf(),
                registry(provider),
                Arc::new(DefaultExecutor::new()),
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        // Start a turn in the background.
        let gw = Arc::clone(&gateway);
        let key = session_key.clone();
        let turn_handle = tokio::spawn(async move {
            gw.run_turn_with_trust(
                &key,
                "original message",
                TrustLevel::Full,
                Some("alice"),
                None,
                event_tx,
            )
            .await
        });

        // Wait a bit for the turn to start, then inject a message.
        tokio::time::sleep(Duration::from_millis(50)).await;
        gateway.inject_pending_inbound(&session_key, "injected mid-turn follow-up".into());

        // Wait for the turn to complete.
        turn_handle.await.unwrap().unwrap();
        while event_rx.try_recv().is_ok() {}

        // The session should contain: user(original) + user(injected) + assistant
        let msgs = gateway.messages(&session_key);
        assert!(
            msgs.len() >= 3,
            "expected at least 3 messages (original + injected + assistant), got {}",
            msgs.len()
        );

        let user_texts: Vec<String> = msgs
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .map(Message::text)
            .collect();
        assert!(
            user_texts.iter().any(|t| t.contains("original message")),
            "original message should be in session"
        );
        assert!(
            user_texts
                .iter()
                .any(|t| t.contains("injected mid-turn follow-up")),
            "injected message should be in session"
        );
    }
}
