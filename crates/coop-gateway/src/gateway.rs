use anyhow::Result;
use coop_core::prompt::{PromptBuilder, SkillEntry, WorkspaceIndex, scan_skills};
use coop_core::{
    InboundMessage, Message, Role, SessionKey, SessionKind, ToolContext, ToolDef, ToolExecutor,
    TrustLevel, TurnConfig, TurnEvent, TurnResult, TypingNotifier, Usage,
};
use coop_memory::Memory;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use uuid::Uuid;

use crate::compaction::{self, CompactionState};
use crate::compaction_store::CompactionStore;
use crate::config::{SharedConfig, find_group_config_by_session};
use crate::group_history::{GroupCeilingCache, GroupHistoryBuffer, GroupHistoryEntry};
use crate::group_trigger::{self, SILENT_REPLY_TOKEN};
use crate::memory_auto_capture;
use crate::memory_prompt_index;
use crate::provider_registry::ProviderRegistry;
use crate::session_store::DiskSessionStore;

pub(crate) struct Gateway {
    config: SharedConfig,
    workspace: PathBuf,
    workspace_index: Mutex<WorkspaceIndex>,
    skills: Mutex<Vec<SkillEntry>>,
    providers: ProviderRegistry,
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
    /// Cached group membership ceilings (revision-based invalidation).
    /// Used by `min_member` trust ceiling mode (wired up when Signal GroupMembers query lands).
    #[allow(dead_code)]
    group_ceiling_cache: Mutex<GroupCeilingCache>,
}

/// Tracks cumulative token usage and context size for a session.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionUsage {
    pub cumulative: Usage,
    /// Input tokens from the last turn (approximates current context size).
    pub last_input_tokens: u32,
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
        SessionKind::Main | SessionKind::Isolated(_) | SessionKind::Cron(_) => None,
    }
}

impl Gateway {
    pub(crate) fn new(
        config: SharedConfig,
        workspace: PathBuf,
        providers: ProviderRegistry,
        executor: Arc<dyn ToolExecutor>,
        typing_notifier: Option<Arc<dyn TypingNotifier>>,
        memory: Option<Arc<dyn Memory>>,
    ) -> Result<Self> {
        let file_configs = config.load().prompt.shared_core_configs();
        let workspace_index = WorkspaceIndex::scan(&workspace, &file_configs)?;
        let skills = scan_skills(&workspace);
        if !skills.is_empty() {
            debug!(count = skills.len(), "loaded skills");
        }

        let session_store = DiskSessionStore::new(workspace.join("sessions"))?;
        let compaction_store = CompactionStore::new(workspace.join("sessions"))?;

        Ok(Self {
            config,
            workspace,
            workspace_index: Mutex::new(workspace_index),
            skills: Mutex::new(skills),
            providers,
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
            group_ceiling_cache: Mutex::new(GroupCeilingCache::new()),
        })
    }

    /// Build a trust-gated system prompt for this turn.
    ///
    /// Returns cache-friendly blocks: the stable prefix (workspace files,
    /// identity, tools) is separated from the volatile suffix (runtime
    /// context, memory index). This lets providers with prefix caching
    /// avoid full cache misses when only the volatile part changes.
    async fn build_prompt(
        &self,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        user_input: &str,
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
                .model(&cfg.agent.model)
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

    fn tool_context(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
    ) -> ToolContext {
        ToolContext {
            session_id: session_key.to_string(),
            trust,
            workspace: self.workspace.clone(),
            user_name: user_name.map(str::to_owned),
        }
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub(crate) async fn run_turn_with_trust(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        channel: Option<&str>,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let span = info_span!(
            "agent_turn",
            session = %session_key,
            input_len = user_input.len(),
            user_input = user_input,
            trust = ?trust,
            user = ?user_name,
            channel = ?channel,
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

            // Sync provider model with config (picks up hot-reloaded agent.model).
            self.sync_provider_model();

            // Cron sessions always start fresh — clear any previous run's
            // context. This happens after the turn lock to avoid a race where
            // a concurrent fire clears a session mid-turn.
            if matches!(session_key.kind, SessionKind::Cron(_)) {
                self.clear_session(session_key);
                debug!(session = %session_key, "cleared cron session for fresh execution");
            }

            let mut system_prompt =
                self.build_prompt(trust, user_name, channel, user_input).await?;

            // Inject group-specific intro when this is a group session.
            // Append to the last block to avoid exceeding the cache_control block limit.
            if let SessionKind::Group(_) = &session_key.kind {
                let cfg = self.config.load();
                if let Some(group_config) = find_group_config_by_session(session_key, &cfg) {
                    let intro = build_group_intro(&group_config.trigger, &cfg.agent.id);
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
            self.maybe_compact(session_key, &system_prompt, &event_tx)
                .await?;

            let session_len_before = self.messages(session_key).len();
            self.append_message(session_key, Message::user().with_text(user_input));

            let tool_defs = self.executor.tools();
            // Cron sessions don't need channel-specific tools like signal_send;
            // delivery is handled by the scheduler after the turn completes.
            let tool_defs = if matches!(session_key.kind, SessionKind::Cron(_)) {
                tool_defs
                    .into_iter()
                    .filter(|t| t.name != "signal_send")
                    .filter(|t| t.name != "signal_react")
                    .filter(|t| t.name != "signal_reply")
                    .collect()
            } else {
                tool_defs
            };
            let ctx = self.tool_context(session_key, trust, user_name);
            let turn_config = TurnConfig::default();

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
                    let messages = coop_core::images::inject_images_for_provider(&messages);
                    let (response, usage) = self
                        .assistant_response(&system_prompt, &messages, &tool_defs, &event_tx)
                        .await?;

                    self.update_last_input_tokens(session_key, &usage);
                    total_usage += usage;
                    self.append_message(session_key, response.clone());
                    new_messages.push(response.clone());

                    let _ = event_tx
                        .send(TurnEvent::AssistantMessage(response.clone()))
                        .await;

                    debug!(
                        has_tool_requests = response.has_tool_requests(),
                        response_text_len = response.text().len(),
                        "iteration complete"
                    );

                    let has_tool_requests = response.has_tool_requests();
                    Ok((response, !has_tool_requests))
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
                                messages: new_messages,
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

                let mut result_msg = Message::user();

                for req in response.tool_requests() {
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
                    info!("turn cancelled after tool execution");
                    break;
                }

                self.append_message(session_key, result_msg.clone());
                new_messages.push(result_msg);

                // Compact mid-turn if the context grew past the threshold
                // during this iteration. The next iteration will use the
                // compacted context automatically via build_provider_context.
                self.maybe_compact(session_key, &system_prompt, &event_tx)
                    .await?;

                if iteration + 1 >= turn_config.max_iterations {
                    hit_limit = true;
                }
            }

            let cancelled = turn_cancel.is_cancelled();

            // If we hit the iteration limit while the model still wanted to use
            // tools, inject a user message explaining the situation and do one
            // final provider call with no tools so the model is forced to
            // produce a text summary for the user.
            if hit_limit && !cancelled {
                let final_span = info_span!("turn_limit_completion");
                let final_result: Result<()> = async {
                    info!("forcing final completion (iteration limit reached)");

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
                    let messages = coop_core::images::inject_images_for_provider(&messages);

                    let (response, usage) = self
                        .assistant_response(&system_prompt, &messages, &[], &event_tx)
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

            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: new_messages,
                    usage: total_usage,
                    hit_limit,
                }))
                .await;

            if let Some(memory) = &self.memory {
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
                    let provider = Arc::clone(self.providers.primary());
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
    }

    /// Cancel the active turn for a session, if one is running.
    /// Returns `true` if a turn was cancelled.
    pub(crate) fn cancel_active_turn(&self, session_key: &SessionKey) -> bool {
        let tokens = self
            .active_turns
            .lock()
            .expect("active_turns mutex poisoned");
        if let Some(token) = tokens.get(session_key) {
            token.cancel();
            info!(session = %session_key, "active turn cancelled via /stop");
            true
        } else {
            false
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
    /// Uses `last_input_tokens` from session usage as the signal — this is
    /// the input token count the provider reported on the most recent call,
    /// reflecting how large the context actually was.
    ///
    /// Returns `Ok(true)` if compaction was performed.
    async fn maybe_compact(
        &self,
        session_key: &SessionKey,
        system_prompt: &[String],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<bool> {
        let input_tokens = self
            .session_usage
            .lock()
            .expect("session_usage mutex poisoned")
            .get(session_key)
            .map_or(0, |u| u.last_input_tokens);

        if !compaction::should_compact(input_tokens) {
            return Ok(false);
        }

        // If we already have a compaction state and no new messages have been
        // added since, there's nothing to re-compact.
        let previous_compaction = self.get_compaction(session_key);
        if let Some((_, msg_count_at_compaction)) = &previous_compaction {
            let current_count = self.messages(session_key).len();
            if current_count <= *msg_count_at_compaction {
                return Ok(false);
            }
        }

        let _ = event_tx.send(TurnEvent::Compacting).await;

        let all_messages = self.messages(session_key);
        let msg_count = all_messages.len();

        let previous_state = previous_compaction.as_ref().map(|(state, _)| state);

        info!(
            session = %session_key,
            input_tokens,
            message_count = msg_count,
            is_iterative = previous_state.is_some(),
            "compaction triggered"
        );

        match compaction::compact(
            &all_messages,
            self.providers.primary().as_ref(),
            system_prompt,
            previous_state,
        )
        .await
        {
            Ok(state) => {
                let cut_point = state.messages_at_compaction.unwrap_or(msg_count);
                info!(
                    session = %session_key,
                    summary_len = state.summary.len(),
                    compaction_count = state.compaction_count,
                    files_tracked = state.files_touched.len(),
                    cut_point,
                    "session compacted"
                );
                self.set_compaction(session_key, state, cut_point);
                Ok(true)
            }
            Err(e) => {
                warn!(
                    session = %session_key,
                    error = %e,
                    "compaction failed, continuing with full context"
                );
                Ok(false)
            }
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

    /// If the session ends with an assistant message containing tool_use blocks
    /// but no subsequent user message with matching tool_result blocks, append a
    /// synthetic tool_result message so the API doesn't reject the history.
    fn repair_dangling_tool_use(&self, session_key: &SessionKey) {
        let msgs = self.messages(session_key);
        let Some(last) = msgs.last() else { return };

        if last.role != Role::Assistant || !last.has_tool_requests() {
            return;
        }

        let tool_ids: Vec<String> = last.tool_requests().iter().map(|r| r.id.clone()).collect();

        warn!(
            session = %session_key,
            dangling_tool_ids = ?tool_ids,
            "repairing session with dangling tool_use blocks from interrupted turn"
        );

        let mut repair_msg = Message::user();
        for id in &tool_ids {
            repair_msg = repair_msg.with_tool_result(
                id,
                "error: previous turn was interrupted before this tool result was recorded",
                true,
            );
        }
        self.append_message(session_key, repair_msg);
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
    #[allow(dead_code)]
    pub(crate) fn session_is_empty(&self, session_key: &SessionKey) -> bool {
        self.messages(session_key).is_empty()
    }

    /// Number of messages in a session.
    pub(crate) fn session_message_count(&self, session_key: &SessionKey) -> usize {
        self.messages(session_key).len()
    }

    /// Push the current config model into the provider if it has changed.
    fn sync_provider_model(&self) {
        let config_model = self.config.load().agent.model.clone();
        let provider_model = self.providers.primary().model_info().name;
        // Strip prefix for comparison (config may have "anthropic/" prefix, provider won't)
        let config_api_model = config_model
            .strip_prefix("anthropic/")
            .unwrap_or(&config_model);
        if provider_model != config_api_model {
            debug!(
                old = %provider_model,
                new = %config_api_model,
                "syncing provider model from hot-reloaded config"
            );
            self.providers.sync_primary_model(&config_model);
        }
    }

    /// Agent model name from config.
    pub(crate) fn model_name(&self) -> String {
        self.config.load().agent.model.clone()
    }

    /// Agent ID from config.
    pub(crate) fn agent_id(&self) -> String {
        self.config.load().agent.id.clone()
    }

    /// Context window size in tokens.
    pub(crate) fn context_limit(&self) -> usize {
        self.providers.primary().model_info().context_limit
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
    #[allow(dead_code)]
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
            sender: msg.sender.clone(),
            body: msg.content.clone(),
            timestamp: msg.timestamp.timestamp().unsigned_abs(),
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
        let trigger_model = group_config.trigger_model_or_default();
        let provider = self.providers.get(trigger_model);

        let system_prompt = match self
            .build_prompt(trust, user_name, channel, user_input)
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
                    model = trigger_model,
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

    async fn assistant_response(
        &self,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let streaming = self.providers.primary().supports_streaming();
        let span = info_span!(
            "provider_request",
            message_count = messages.len(),
            tool_count = tool_defs.len(),
            streaming,
        );

        async {
            if streaming {
                self.assistant_response_streaming(system_prompt, messages, tool_defs, event_tx)
                    .await
            } else {
                self.assistant_response_non_streaming(system_prompt, messages, tool_defs, event_tx)
                    .await
            }
        }
        .instrument(span)
        .await
    }

    async fn assistant_response_streaming(
        &self,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let mut stream = self
            .providers
            .primary()
            .stream(system_prompt, messages, tool_defs)
            .await?;

        let mut response = Message::assistant();
        let mut usage = Usage::default();

        while let Some(item) = stream.next().await {
            let (msg_opt, usage_opt) = item?;

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

        debug!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_tokens = usage.cache_read_tokens,
            cache_write_tokens = usage.cache_write_tokens,
            stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
            "provider response complete"
        );

        Ok((response, usage))
    }

    async fn assistant_response_non_streaming(
        &self,
        system_prompt: &[String],
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let (response, usage) = self
            .providers
            .primary()
            .complete(system_prompt, messages, tool_defs)
            .await?;

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

        Ok((response, usage))
    }
}

fn build_group_intro(trigger: &crate::config::GroupTrigger, _agent_id: &str) -> String {
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
        format!("Activation: {activation}."),
    ];

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
    lines.push("Address the specific sender noted in the message context.".to_owned());

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
    use coop_core::Provider;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use coop_core::traits::ProviderStream;
    use coop_core::types::{Content, ModelInfo};
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
    fn parse_rejects_other_agent() {
        assert!(parse_session_key("other:main", "coop").is_none());
    }

    fn test_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "You are a test agent.").unwrap();
        dir
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

        let mut turn1_msg_count = 0;
        while let Ok(event) = rx1.try_recv() {
            if let TurnEvent::Done(result) = event {
                turn1_msg_count = result.messages.len();
            }
        }
        assert!(
            turn1_msg_count > 0,
            "first turn should have produced messages"
        );

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
    async fn sync_provider_model_picks_up_config_change() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(test_config());

        let gateway = Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            registry(provider),
            executor,
            None,
            None,
        )
        .unwrap();

        // Default model from FakeProvider is "fake-model".
        assert_eq!(gateway.providers.primary().model_info().name, "fake-model");

        // Simulate hot-reload: change agent.model.
        let mut new_config = shared.load().as_ref().clone();
        new_config.agent.model = "new-model".to_owned();
        shared.store(Arc::new(new_config));

        gateway.sync_provider_model();

        // FakeProvider implements set_model, so the provider should now
        // report the new model name — proving the full hot-reload path works.
        assert_eq!(gateway.providers.primary().model_info().name, "new-model");
        assert_eq!(gateway.model_name(), "new-model");
    }

    #[tokio::test]
    async fn sync_provider_model_detects_prefixed_model() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(test_config());

        let gateway = Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            registry(provider),
            executor,
            None,
            None,
        )
        .unwrap();

        // Config uses "anthropic/" prefix — sync_provider_model compares
        // after stripping the prefix and calls set_model with the raw config
        // value. Prefix stripping is the provider's responsibility.
        // FakeProvider stores the value as-is; AnthropicProvider strips it.
        let mut new_config = shared.load().as_ref().clone();
        new_config.agent.model = "anthropic/claude-haiku-3-20250514".to_owned();
        shared.store(Arc::new(new_config));

        gateway.sync_provider_model();

        // FakeProvider doesn't strip prefix, but the sync did execute.
        // The actual prefix stripping is tested in coop-agent's
        // set_model_strips_anthropic_prefix test.
        let provider_model = gateway.providers.primary().model_info().name;
        assert!(
            provider_model.contains("claude-haiku-3-20250514"),
            "provider should have received the new model: {provider_model}"
        );
    }

    #[tokio::test]
    async fn sync_provider_model_noop_when_unchanged() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(test_config());

        let gateway = Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            registry(provider),
            executor,
            None,
            None,
        )
        .unwrap();

        // Config model is "test-model", provider model is "fake-model".
        // They differ, so first sync will update.
        gateway.sync_provider_model();
        assert_eq!(gateway.providers.primary().model_info().name, "test-model");

        // Second sync should be a no-op (same model).
        gateway.sync_provider_model();
        assert_eq!(gateway.providers.primary().model_info().name, "test-model");
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
}
