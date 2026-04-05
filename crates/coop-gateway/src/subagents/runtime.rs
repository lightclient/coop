use anyhow::{Context, Result, bail};
use coop_core::traits::ToolContext;
use coop_core::{Message, OutboundMessage, SessionKey, SessionKind, ToolDef, ToolOutput};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info_span};
use uuid::Uuid;

use crate::config::{SharedConfig, SubagentProfileConfig, SubagentPromptMode};
use crate::gateway::Gateway;

use super::policy::filter_child_tools;
use super::prompt::{
    PreparedChildPrompt, ResolvedSpawnPath, build_initial_message, parse_child_response,
    prepare_minimal_child_prompt,
};
use super::registry::{SubagentRegistry, SubagentRunRecord, SubagentRunStatus};
use super::{
    SubagentCompletion, SubagentControlAction, SubagentSpawnRequest, SubagentsControlRequest,
    TurnOverrides,
};

#[derive(Debug)]
pub(crate) struct SubagentManager {
    config: SharedConfig,
    workspace: PathBuf,
    registry: Arc<SubagentRegistry>,
    lane: Arc<SubagentLane>,
    cancel_tokens: Mutex<std::collections::HashMap<Uuid, CancellationToken>>,
    gateway: Mutex<Weak<Gateway>>,
    delivery: Mutex<Option<mpsc::Sender<OutboundMessage>>>,
}

impl SubagentManager {
    pub(crate) fn new(config: SharedConfig, workspace: PathBuf) -> Result<Self> {
        let registry = Arc::new(SubagentRegistry::new(&workspace)?);
        Ok(Self {
            config,
            workspace,
            registry,
            lane: Arc::new(SubagentLane::default()),
            cancel_tokens: Mutex::new(std::collections::HashMap::new()),
            gateway: Mutex::new(Weak::new()),
            delivery: Mutex::new(None),
        })
    }

    pub(crate) fn bind_gateway(&self, gateway: &Arc<Gateway>) {
        *self
            .gateway
            .lock()
            .expect("subagent gateway mutex poisoned") = Arc::downgrade(gateway);
    }

    pub(crate) fn bind_delivery(&self, delivery: Option<mpsc::Sender<OutboundMessage>>) {
        *self
            .delivery
            .lock()
            .expect("subagent delivery mutex poisoned") = delivery;
    }

    fn delivery_sender(&self) -> Option<mpsc::Sender<OutboundMessage>> {
        self.delivery
            .lock()
            .expect("subagent delivery mutex poisoned")
            .clone()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.load().agent.subagents.enabled
    }

    pub(crate) async fn spawn_from_tool(
        self: &Arc<Self>,
        request: SubagentSpawnRequest,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let gateway = self.gateway()?;
        let run = PreparedRun::new(Arc::clone(self), gateway, request, ctx)?;
        run.start().await
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn control_from_tool(&self, request: SubagentsControlRequest) -> Result<ToolOutput> {
        match request.action {
            SubagentControlAction::List => {
                let runs = self.registry.list_recent();
                Ok(ToolOutput::success(json!({ "runs": runs }).to_string()))
            }
            SubagentControlAction::Inspect => {
                let run_id = parse_run_id(request.run_id.as_deref())?;
                let record = self
                    .registry
                    .get(run_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown subagent run id: {run_id}"))?;
                Ok(ToolOutput::success(json!({ "run": record }).to_string()))
            }
            SubagentControlAction::Kill => {
                let run_id = parse_run_id(request.run_id.as_deref())?;
                let killed = self.cancel_run(run_id, "stopped by subagents tool")?;
                Ok(ToolOutput::success(
                    json!({ "run_id": run_id, "killed": killed }).to_string(),
                ))
            }
        }
    }

    pub(crate) fn list_runs(&self) -> Vec<SubagentRunRecord> {
        self.registry.list_recent()
    }

    pub(crate) fn inspect_run(&self, run_id: &str) -> Result<SubagentRunRecord> {
        let run_id = parse_run_id(Some(run_id))?;
        self.registry
            .get(run_id)
            .ok_or_else(|| anyhow::anyhow!("unknown subagent run id: {run_id}"))
    }

    pub(crate) fn cancel_run(&self, run_id: Uuid, reason: &str) -> Result<bool> {
        let mut found = false;
        if let Some(token) = self
            .cancel_tokens
            .lock()
            .expect("subagent cancel map mutex poisoned")
            .get(&run_id)
            .cloned()
        {
            token.cancel();
            found = true;
        }

        if found {
            let _ = self.registry.finish(
                run_id,
                SubagentRunStatus::Cancelled,
                None,
                Vec::new(),
                Some(reason.to_owned()),
            )?;
        }

        Ok(found)
    }

    pub(crate) fn cancel_for_parent_session(&self, parent_session: &SessionKey) -> bool {
        let root_runs = self
            .registry
            .active_run_ids_for_parent_session(parent_session);
        for run_id in &root_runs {
            self.cancel_tree(*run_id, "stopped because the parent session was cancelled");
        }
        !root_runs.is_empty()
    }

    fn cancel_tree(&self, run_id: Uuid, reason: &str) {
        let _ = self.cancel_run(run_id, reason);
        for child_run_id in self.registry.child_run_ids(run_id) {
            self.cancel_tree(child_run_id, reason);
        }
    }

    fn gateway(&self) -> Result<Arc<Gateway>> {
        self.gateway
            .lock()
            .expect("subagent gateway mutex poisoned")
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("subagent runtime is not attached to a gateway"))
    }
}

struct PreparedRun {
    manager: Arc<SubagentManager>,
    gateway: Arc<Gateway>,
    request: SubagentSpawnRequest,
    parent_session_key: SessionKey,
    child_session_key: SessionKey,
    child_depth: u32,
    trust: coop_core::TrustLevel,
    resolved_model: String,
    allowed_tools: Vec<ToolDef>,
    timeout_seconds: u64,
    prompt_overrides: TurnOverrides,
    record: SubagentRunRecord,
    cancel: CancellationToken,
}

impl PreparedRun {
    #[allow(clippy::too_many_lines)]
    fn new(
        manager: Arc<SubagentManager>,
        gateway: Arc<Gateway>,
        request: SubagentSpawnRequest,
        ctx: &ToolContext,
    ) -> Result<Self> {
        if !manager.enabled() {
            bail!("subagents are disabled in config");
        }
        if request.task.trim().is_empty() {
            bail!("task is required");
        }

        let config = manager.config.load();
        let subagents = &config.agent.subagents;
        let parent_session_key = SessionKey {
            agent_id: config.agent.id.clone(),
            kind: ctx.session_kind.clone(),
        };
        let parent_run_id = match ctx.session_kind {
            SessionKind::Subagent(run_id) => Some(run_id),
            _ => None,
        };
        let parent_depth = parent_run_id
            .and_then(|run_id| manager.registry.get(run_id).map(|record| record.depth))
            .unwrap_or(0);
        let child_depth = parent_depth + 1;
        if child_depth > subagents.max_spawn_depth {
            bail!(
                "subagent depth {} exceeds configured max_spawn_depth {}",
                child_depth,
                subagents.max_spawn_depth
            );
        }
        if manager.registry.active_count() >= subagents.max_active_children {
            bail!(
                "subagent limit reached: {} active child runs (max {})",
                manager.registry.active_count(),
                subagents.max_active_children
            );
        }

        let profile = profile_for_request(&subagents.profiles, request.profile.as_deref())?;
        let resolved_model = request
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| profile.as_ref().and_then(|profile| profile.model.clone()))
            .or_else(|| subagents.model.clone())
            .or_else(|| ctx.model.clone())
            .unwrap_or_else(|| config.agent.model.clone());

        let timeout_seconds = request
            .timeout_seconds
            .or_else(|| {
                profile
                    .as_ref()
                    .and_then(|profile| profile.default_timeout_seconds)
            })
            .unwrap_or(subagents.default_timeout_seconds);
        if timeout_seconds == 0 {
            bail!("timeout_seconds must be greater than zero");
        }

        let max_turns = request
            .max_turns
            .or_else(|| {
                profile
                    .as_ref()
                    .and_then(|profile| profile.default_max_turns)
            })
            .unwrap_or(subagents.default_max_turns);
        if max_turns == 0 {
            bail!("max_turns must be greater than zero");
        }

        let child_session_id = Uuid::new_v4();
        let child_session_key = SessionKey {
            agent_id: config.agent.id.clone(),
            kind: SessionKind::Subagent(child_session_id),
        };
        drop(config);

        let all_tools = gateway.all_tool_defs();
        let allowed_tools = filter_child_tools(
            &all_tools,
            &ctx.visible_tools,
            profile.as_ref(),
            &request.tools,
            child_depth,
            manager.config.load().agent.subagents.max_spawn_depth,
        );

        let child_scope = coop_core::WorkspaceScope::for_turn(
            &manager.workspace,
            &child_session_key.kind,
            ctx.trust,
            ctx.user_name.as_deref(),
        );
        let resolved_paths = resolve_spawn_paths(&child_scope, &request.paths)?;
        let prompt_overrides = build_turn_overrides(
            &manager.workspace,
            &child_scope,
            &request,
            profile.as_ref(),
            &resolved_model,
            child_depth,
            &resolved_paths,
            &allowed_tools,
            manager.config.load().agent.subagents.prompt_mode,
            manager.config.load().agent.subagents.inherit_memory,
            max_turns,
        );

        let tool_names = allowed_tools.iter().map(|tool| tool.name.clone()).collect();
        let record = SubagentRunRecord::new(
            child_session_id,
            child_session_key.clone(),
            parent_session_key.clone(),
            parent_run_id,
            ctx.user_name.clone(),
            request.task.clone(),
            request.profile.clone(),
            resolved_model.clone(),
            request.mode,
            tool_names,
            child_depth,
            timeout_seconds,
            max_turns,
            resolved_paths
                .iter()
                .map(|path| path.display_path.clone())
                .collect(),
        );

        Ok(Self {
            manager,
            gateway,
            request,
            parent_session_key,
            child_session_key,
            child_depth,
            trust: ctx.trust,
            resolved_model,
            allowed_tools,
            timeout_seconds,
            prompt_overrides,
            record,
            cancel: CancellationToken::new(),
        })
    }

    async fn start(self) -> Result<ToolOutput> {
        let run_id = self.record.run_id;
        let child_session = self.child_session_key.to_string();
        let mode = self.request.mode;
        self.manager.registry.insert(self.record.clone())?;
        self.manager
            .cancel_tokens
            .lock()
            .expect("subagent cancel map mutex poisoned")
            .insert(run_id, self.cancel.clone());

        let (wait_tx, wait_rx) = oneshot::channel();
        tokio::spawn(self.run(Some(wait_tx)));

        match mode {
            super::SubagentMode::Background => Ok(ToolOutput::success(
                json!({
                    "success": true,
                    "status": "accepted",
                    "run_id": run_id,
                    "child_session": child_session,
                })
                .to_string(),
            )),
            super::SubagentMode::Wait => {
                let completion = wait_rx
                    .await
                    .context("subagent wait channel closed unexpectedly")?;
                let success = completion.status == SubagentRunStatus::Completed;
                Ok(ToolOutput::success(
                    json!({
                        "success": success,
                        "run_id": run_id,
                        "child_session": child_session,
                        "status": completion.status,
                        "summary": completion.summary,
                        "artifact_paths": completion.artifact_paths,
                        "error": completion.error,
                    })
                    .to_string(),
                ))
            }
        }
    }

    async fn run(self, wait_tx: Option<oneshot::Sender<SubagentCompletion>>) {
        let queued_span = info_span!(
            "subagent_queue",
            run_id = %self.record.run_id,
            parent_session = %self.parent_session_key,
            child_session = %self.child_session_key,
            profile = ?self.request.profile,
            model = %self.resolved_model,
            mode = ?self.request.mode,
            depth = self.child_depth,
            timeout_seconds = self.timeout_seconds,
            tool_count = self.allowed_tools.len(),
        );

        async move {
            let limit = self.manager.config.load().agent.subagents.max_concurrent;
            let Some(lane) = self.manager.lane.acquire(limit, &self.cancel).await else {
                let completion = SubagentCompletion::new(
                    SubagentRunStatus::Cancelled,
                    None,
                    Vec::new(),
                    Some("cancelled before start".to_owned()),
                );
                let _ = self.manager.registry.finish(
                    self.record.run_id,
                    completion.status,
                    completion.summary.clone(),
                    completion.artifact_paths.clone(),
                    completion.error.clone(),
                );
                finish_wait(wait_tx, completion);
                return;
            };
            let _lane = lane;

            let _ = self.manager.registry.mark_running(self.record.run_id);

            let run_span = info_span!(
                "subagent_run",
                run_id = %self.record.run_id,
                parent_session = %self.parent_session_key,
                child_session = %self.child_session_key,
                profile = ?self.request.profile,
                model = %self.resolved_model,
                mode = ?self.request.mode,
                depth = self.child_depth,
                timeout_seconds = self.timeout_seconds,
                tool_count = self.allowed_tools.len(),
            );

            let completion = match collect_child_run(&self).instrument(run_span).await {
                Ok(result) => result,
                Err(error) => SubagentCompletion::new(
                    SubagentRunStatus::Failed,
                    None,
                    Vec::new(),
                    Some(format!("{error:#}")),
                ),
            };

            let _ = self.manager.registry.finish(
                self.record.run_id,
                completion.status,
                completion.summary.clone(),
                completion.artifact_paths.clone(),
                completion.error.clone(),
            );
            self.manager
                .cancel_tokens
                .lock()
                .expect("subagent cancel map mutex poisoned")
                .remove(&self.record.run_id);

            if self.request.mode == super::SubagentMode::Background {
                announce_completion(
                    &self.manager,
                    &self.gateway,
                    &self.parent_session_key,
                    self.record.run_id,
                    &self.child_session_key,
                    &completion,
                )
                .await;
            }

            debug!(
                run_id = %self.record.run_id,
                status = ?completion.status,
                artifact_paths = ?completion.artifact_paths,
                "subagent completion"
            );
            finish_wait(wait_tx, completion);
        }
        .instrument(queued_span)
        .await;
    }
}

async fn collect_child_run(run: &PreparedRun) -> Result<SubagentCompletion> {
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let gateway = Arc::clone(&run.gateway);
    let session_key = run.child_session_key.clone();
    let trust = run.trust;
    let user_name = run.record.requesting_user.clone();
    let overrides = run.prompt_overrides.clone();
    let model = run.resolved_model.clone();
    let child_input = run.request.task.clone();
    let cancel = run.cancel.clone();

    let turn_future = async move {
        gateway
            .run_turn_with_options(
                &session_key,
                &child_input,
                trust,
                user_name.as_deref(),
                None,
                None,
                overrides,
                event_tx,
            )
            .await
    };

    tokio::pin!(turn_future);
    let mut final_text = String::new();
    let mut turn_error: Option<String> = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(run.timeout_seconds));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            result = &mut turn_future => {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        coop_core::TurnEvent::TextDelta(delta) => final_text.push_str(&delta),
                        coop_core::TurnEvent::AssistantMessage(message) => final_text = message.text(),
                        coop_core::TurnEvent::Error(message) => turn_error = Some(message),
                        coop_core::TurnEvent::Done(_) => break,
                        coop_core::TurnEvent::ToolStart { .. }
                        | coop_core::TurnEvent::ToolResult { .. }
                        | coop_core::TurnEvent::Compacting => {}
                    }
                }

                let result = result;
                if let Some(error) = turn_error {
                    return Ok(SubagentCompletion::new(
                        SubagentRunStatus::Failed,
                        None,
                        Vec::new(),
                        Some(error),
                    ));
                }

                result?;
                let parsed = parse_child_response(&final_text);
                return Ok(SubagentCompletion::new(
                    SubagentRunStatus::Completed,
                    Some(parsed.summary),
                    parsed.artifact_paths,
                    None,
                ));
            }
            () = cancel.cancelled() => {
                let _ = run.gateway.cancel_active_turn(&run.child_session_key);
                let _ = (&mut turn_future).await;
                return Ok(SubagentCompletion::new(
                    SubagentRunStatus::Cancelled,
                    None,
                    Vec::new(),
                    Some(format!("subagent run cancelled for model {model}")),
                ));
            }
            () = &mut timeout => {
                let _ = run.gateway.cancel_active_turn(&run.child_session_key);
                let _ = (&mut turn_future).await;
                return Ok(SubagentCompletion::new(
                    SubagentRunStatus::TimedOut,
                    None,
                    Vec::new(),
                    Some(format!("subagent run exceeded {} seconds", run.timeout_seconds)),
                ));
            }
            Some(event) = event_rx.recv() => {
                match event {
                    coop_core::TurnEvent::TextDelta(delta) => final_text.push_str(&delta),
                    coop_core::TurnEvent::AssistantMessage(message) => final_text = message.text(),
                    coop_core::TurnEvent::Error(message) => turn_error = Some(message),
                    coop_core::TurnEvent::Done(_)
                    | coop_core::TurnEvent::ToolStart { .. }
                    | coop_core::TurnEvent::ToolResult { .. }
                    | coop_core::TurnEvent::Compacting => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_turn_overrides(
    workspace: &std::path::Path,
    scope: &coop_core::WorkspaceScope,
    request: &SubagentSpawnRequest,
    profile: Option<&SubagentProfileConfig>,
    model: &str,
    depth: u32,
    paths: &[ResolvedSpawnPath],
    allowed_tools: &[ToolDef],
    default_prompt_mode: SubagentPromptMode,
    inherit_memory: bool,
    max_turns: u32,
) -> TurnOverrides {
    let prompt_mode = profile
        .and_then(|profile| profile.prompt_mode)
        .unwrap_or(default_prompt_mode);
    let initial_message =
        build_initial_message(scope, &request.task, request.context.as_deref(), paths);

    let mut overrides = TurnOverrides::default()
        .with_model(model)
        .with_tool_names(allowed_tools.iter().map(|tool| tool.name.clone()))
        .with_initial_message(initial_message)
        .with_max_iterations(max_turns);

    if !inherit_memory && prompt_mode == SubagentPromptMode::Minimal {
        let PreparedChildPrompt {
            system_blocks,
            initial_message,
        } = prepare_minimal_child_prompt(
            workspace,
            scope,
            &request.task,
            request.context.as_deref(),
            request.profile.as_deref(),
            model,
            depth,
            paths,
        );
        overrides = overrides
            .with_prompt_blocks(system_blocks)
            .with_initial_message(initial_message);
    }

    overrides
}

fn resolve_spawn_paths(
    scope: &coop_core::WorkspaceScope,
    requested_paths: &[String],
) -> Result<Vec<ResolvedSpawnPath>> {
    requested_paths
        .iter()
        .map(|requested| {
            let resolved = scope
                .resolve_host_path_for_read(requested)
                .or_else(|_| scope.resolve_user_path_for_read(requested))?;
            let display_path = scope
                .scope_relative_path(&resolved)
                .unwrap_or_else(|_| requested.to_owned());
            let is_image = coop_core::images::detect_image_paths(&display_path)
                .into_iter()
                .any(|candidate| candidate == display_path);
            Ok(ResolvedSpawnPath {
                display_path,
                is_image,
            })
        })
        .collect()
}

fn profile_for_request(
    profiles: &std::collections::BTreeMap<String, SubagentProfileConfig>,
    profile_name: Option<&str>,
) -> Result<Option<SubagentProfileConfig>> {
    let Some(profile_name) = profile_name.filter(|profile| !profile.trim().is_empty()) else {
        return Ok(None);
    };
    profiles
        .get(profile_name)
        .cloned()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("unknown subagent profile: {profile_name}"))
}

async fn announce_completion(
    manager: &SubagentManager,
    gateway: &Gateway,
    parent_session: &SessionKey,
    run_id: Uuid,
    child_session: &SessionKey,
    completion: &SubagentCompletion,
) {
    let mut lines = vec![
        format!("[subagent completion] run_id={run_id}"),
        format!("child_session={child_session}"),
        format!("status={}", completion.status.as_str()),
    ];
    if let Some(summary) = &completion.summary {
        lines.push("summary:".to_owned());
        lines.push(summary.clone());
    }
    if !completion.artifact_paths.is_empty() {
        lines.push("artifacts:".to_owned());
        lines.extend(
            completion
                .artifact_paths
                .iter()
                .map(|path| format!("- {path}")),
        );
    }
    if let Some(error) = &completion.error {
        lines.push(format!("error: {error}"));
    }
    let content = lines.join("\n");

    if gateway.has_active_turn(parent_session) {
        gateway.inject_pending_inbound(parent_session, content);
    } else {
        gateway.append_message(parent_session, Message::user().with_text(content.clone()));
        if let Some((channel, target)) = signal_delivery_target(parent_session)
            && let Some(delivery) = manager.delivery_sender()
            && let Err(error) = delivery
                .send(OutboundMessage {
                    channel,
                    target,
                    content,
                })
                .await
        {
            tracing::warn!(
                %error,
                %parent_session,
                "failed to deliver background subagent completion"
            );
        }
    }
}

fn signal_delivery_target(parent_session: &SessionKey) -> Option<(String, String)> {
    match &parent_session.kind {
        SessionKind::Dm(identity) => identity
            .strip_prefix("signal:")
            .map(|target| ("signal".to_owned(), target.to_owned())),
        SessionKind::Group(group_id) => group_id
            .strip_prefix("signal:")
            .map(|target| ("signal".to_owned(), target.to_owned())),
        SessionKind::Main
        | SessionKind::Isolated(_)
        | SessionKind::Cron(_)
        | SessionKind::Subagent(_) => None,
    }
}

fn finish_wait(
    wait_tx: Option<oneshot::Sender<SubagentCompletion>>,
    completion: SubagentCompletion,
) {
    if let Some(wait_tx) = wait_tx {
        let _ = wait_tx.send(completion);
    }
}

fn parse_run_id(run_id: Option<&str>) -> Result<Uuid> {
    let run_id = run_id.ok_or_else(|| anyhow::anyhow!("run_id is required"))?;
    Uuid::parse_str(run_id).context("run_id must be a UUID")
}

#[derive(Debug, Default)]
struct SubagentLane {
    active: tokio::sync::Mutex<usize>,
    notify: Notify,
}

impl SubagentLane {
    async fn acquire(
        self: &Arc<Self>,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Option<SubagentLaneGuard> {
        loop {
            {
                let mut active = self.active.lock().await;
                if *active < limit {
                    *active += 1;
                    drop(active);
                    return Some(SubagentLaneGuard {
                        lane: Arc::clone(self),
                    });
                }
            }

            tokio::select! {
                () = self.notify.notified() => {}
                () = cancel.cancelled() => return None,
            }
        }
    }
}

struct SubagentLaneGuard {
    lane: Arc<SubagentLane>,
}

impl Drop for SubagentLaneGuard {
    fn drop(&mut self) {
        let lane = Arc::clone(&self.lane);
        tokio::spawn(async move {
            let mut active = lane.active.lock().await;
            *active = active.saturating_sub(1);
            drop(active);
            lane.notify.notify_one();
        });
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, shared_config};
    use crate::provider_registry::ProviderRegistry;
    use coop_core::TrustLevel;
    use coop_core::fakes::{FakeProvider, SimpleExecutor};
    use coop_core::traits::ToolExecutor;
    use std::time::Duration;
    use tokio::sync::mpsc;

    struct Harness {
        _workspace: tempfile::TempDir,
        gateway: Arc<Gateway>,
        manager: Arc<SubagentManager>,
        ctx: ToolContext,
        parent_session: SessionKey,
    }

    fn harness(response: &str) -> Harness {
        let workspace = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"
workspace = "."
"#,
        )
        .unwrap();
        let shared = shared_config(config);
        let provider: Arc<dyn coop_core::Provider> =
            Arc::new(FakeProvider::with_model(response, "test-model", 128_000));
        let providers = ProviderRegistry::new(Arc::clone(&provider));
        let manager = Arc::new(
            SubagentManager::new(Arc::clone(&shared), workspace.path().to_path_buf()).unwrap(),
        );
        let executor: Arc<dyn ToolExecutor> = Arc::new(SimpleExecutor::new());
        let gateway = Arc::new(
            Gateway::new_with_subagents(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                providers,
                executor,
                None,
                None,
                Arc::clone(&manager),
            )
            .unwrap(),
        );
        manager.bind_gateway(&gateway);
        let parent_session = SessionKey {
            agent_id: "coop".into(),
            kind: SessionKind::Main,
        };
        let ctx = ToolContext::new(
            parent_session.to_string(),
            SessionKind::Main,
            TrustLevel::Full,
            workspace.path(),
            Some("alice"),
        )
        .with_model("test-model");

        Harness {
            _workspace: workspace,
            gateway,
            manager,
            ctx,
            parent_session,
        }
    }

    #[tokio::test]
    async fn wait_mode_returns_structured_completion() {
        let harness = harness("Summary:\nFinished the delegated task.\n\nArtifacts:\n- ./out.txt");
        let output = Arc::clone(&harness.manager)
            .spawn_from_tool(
                SubagentSpawnRequest {
                    task: "Write a concise summary".into(),
                    context: Some("Use no tools".into()),
                    profile: None,
                    model: None,
                    tools: Vec::new(),
                    paths: Vec::new(),
                    mode: super::super::SubagentMode::Wait,
                    max_turns: Some(4),
                    timeout_seconds: Some(30),
                },
                &harness.ctx,
            )
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "completed");
        assert_eq!(value["summary"], "Finished the delegated task.");
        assert_eq!(value["artifact_paths"], serde_json::json!(["./out.txt"]));

        let run_id = value["run_id"].as_str().unwrap();
        let record = harness.manager.inspect_run(run_id).unwrap();
        assert_eq!(record.status, SubagentRunStatus::Completed);
        assert_eq!(
            record.summary.as_deref(),
            Some("Finished the delegated task.")
        );
    }

    #[tokio::test]
    async fn background_mode_appends_completion_to_parent_session() {
        let harness = harness("Summary:\nBackground work finished.");
        let output = Arc::clone(&harness.manager)
            .spawn_from_tool(
                SubagentSpawnRequest {
                    task: "Run in background".into(),
                    context: None,
                    profile: None,
                    model: None,
                    tools: Vec::new(),
                    paths: Vec::new(),
                    mode: super::super::SubagentMode::Background,
                    max_turns: Some(4),
                    timeout_seconds: Some(30),
                },
                &harness.ctx,
            )
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["status"], "accepted");

        for _ in 0..50 {
            if harness
                .manager
                .list_runs()
                .iter()
                .any(|run| run.status == SubagentRunStatus::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let messages = harness.gateway.messages(&harness.parent_session);
        assert!(messages.iter().any(|message| {
            let text = message.text();
            text.contains("[subagent completion]") && text.contains("Background work finished.")
        }));
    }

    #[tokio::test]
    async fn background_mode_delivers_completion_to_signal_parent_when_idle() {
        let workspace = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(
            r#"
[agent]
id = "coop"
model = "test-model"
workspace = "."
"#,
        )
        .unwrap();
        let shared = shared_config(config);
        let provider: Arc<dyn coop_core::Provider> = Arc::new(FakeProvider::with_model(
            "Summary:\nDelivered.",
            "test-model",
            128_000,
        ));
        let providers = ProviderRegistry::new(Arc::clone(&provider));
        let manager = Arc::new(
            SubagentManager::new(Arc::clone(&shared), workspace.path().to_path_buf()).unwrap(),
        );
        let executor: Arc<dyn ToolExecutor> = Arc::new(SimpleExecutor::new());
        let gateway = Arc::new(
            Gateway::new_with_subagents(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                providers,
                executor,
                None,
                None,
                Arc::clone(&manager),
            )
            .unwrap(),
        );
        manager.bind_gateway(&gateway);

        let (tx, mut rx) = mpsc::channel(4);
        manager.bind_delivery(Some(tx));

        let parent_session = SessionKey {
            agent_id: "coop".into(),
            kind: SessionKind::Dm("signal:alice-uuid".into()),
        };
        let ctx = ToolContext::new(
            parent_session.to_string(),
            parent_session.kind.clone(),
            TrustLevel::Full,
            workspace.path(),
            Some("alice"),
        )
        .with_model("test-model");

        Arc::clone(&manager)
            .spawn_from_tool(
                SubagentSpawnRequest {
                    task: "Run in background".into(),
                    context: None,
                    profile: None,
                    model: None,
                    tools: Vec::new(),
                    paths: Vec::new(),
                    mode: super::super::SubagentMode::Background,
                    max_turns: Some(4),
                    timeout_seconds: Some(30),
                },
                &ctx,
            )
            .await
            .unwrap();

        let delivered = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.channel, "signal");
        assert_eq!(delivered.target, "alice-uuid");
        assert!(delivered.content.contains("[subagent completion]"));
        assert!(delivered.content.contains("Delivered."));
    }

    #[tokio::test]
    async fn traces_include_subagent_fields() {
        use std::path::PathBuf;
        use tracing_subscriber::prelude::*;

        let harness = harness("Summary:\nTrace verified.");
        let trace_path = PathBuf::from("/tmp/coop-subagent-trace.jsonl");
        let _ = std::fs::remove_file(&trace_path);

        let file_appender = tracing_appender::rolling::never(
            trace_path.parent().unwrap(),
            trace_path.file_name().unwrap(),
        );
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));
        let subscriber = tracing_subscriber::Registry::default().with(layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let output = Arc::clone(&harness.manager)
            .spawn_from_tool(
                SubagentSpawnRequest {
                    task: "Emit trace metadata".into(),
                    context: Some("trace test".into()),
                    profile: None,
                    model: None,
                    tools: Vec::new(),
                    paths: Vec::new(),
                    mode: super::super::SubagentMode::Wait,
                    max_turns: Some(4),
                    timeout_seconds: Some(30),
                },
                &harness.ctx,
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let run_id = value["run_id"].as_str().unwrap();
        let child_session = value["child_session"].as_str().unwrap();

        drop(default_guard);
        drop(guard);

        let trace = std::fs::read_to_string(&trace_path).unwrap();
        assert!(trace.contains("subagent_queue"));
        assert!(trace.contains("subagent_run"));
        assert!(trace.contains(run_id));
        assert!(trace.contains(child_session));
        assert!(trace.contains("timeout_seconds"));
        assert!(trace.contains("tool_count"));
    }
}
