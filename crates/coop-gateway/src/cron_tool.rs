use anyhow::Result;
use async_trait::async_trait;
use coop_core::SessionKind;
use coop_core::traits::{Tool, ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{Instrument, info, info_span};

use crate::config::{Config, SharedConfig};
use crate::cron_runner::{CronCommand, CronCommandSender, CronTriggerResult, CronTriggerStatus};
use crate::trust::resolve_trust;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CronTriggerArgs {
    name: String,
    #[serde(default)]
    deliver: bool,
}

#[derive(Debug)]
struct CronTriggerTool {
    config: SharedConfig,
    command_tx: CronCommandSender,
}

impl CronTriggerTool {
    fn new(config: SharedConfig, command_tx: CronCommandSender) -> Self {
        Self { config, command_tx }
    }
}

#[async_trait]
impl Tool for CronTriggerTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "cron_trigger",
            "Rerun a configured cron job immediately. Use this only when the user explicitly asks to rerun or test a cron. By default it returns the cron's result in the current conversation without sending scheduled delivery messages. Set deliver=true only when the user wants to test the real delivery path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Configured cron name to rerun exactly as defined in coop.toml"
                    },
                    "deliver": {
                        "type": "boolean",
                        "description": "When true, also use the cron's configured delivery targets (for example Signal). Default: false. Only do this when the user explicitly wants to test or force delivery."
                    }
                },
                "required": ["name"]
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: CronTriggerArgs = serde_json::from_value(arguments)?;
        let span = info_span!(
            "cron_trigger_tool",
            cron.name = %args.name,
            cron.deliver = args.deliver,
            session = %ctx.session_id,
            trust = ?ctx.trust,
        );

        async move {
            if ctx.trust > TrustLevel::Inner {
                return Ok(ToolOutput::error(
                    "cron_trigger requires Full or Inner trust level",
                ));
            }

            if matches!(ctx.session_kind, SessionKind::Cron(_)) {
                return Ok(ToolOutput::error(
                    "cron_trigger cannot be used from a cron session",
                ));
            }

            let config = self.config.load();
            let matching: Vec<_> = config
                .cron
                .iter()
                .filter(|cron| cron.name.as_str() == args.name.as_str())
                .collect();
            let cron = match matching.as_slice() {
                [] => {
                    return Ok(ToolOutput::error(format!(
                        "unknown cron: {}",
                        args.name
                    )));
                }
                [cron] => *cron,
                _ => {
                    return Ok(ToolOutput::error(format!(
                        "cron name '{}' is not unique",
                        args.name
                    )));
                }
            };

            let cron_trust = cron_effective_trust(&config, cron.user.as_deref());
            if ctx.trust > cron_trust {
                return Ok(ToolOutput::error(format!(
                    "cron_trigger cannot run cron '{}' because it executes with {} trust, but this session only has {} trust",
                    args.name,
                    trust_label(cron_trust),
                    trust_label(ctx.trust),
                )));
            }
            drop(config);

            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .send(CronCommand::RunNow {
                    name: args.name.clone(),
                    deliver: args.deliver,
                    origin_session_id: ctx.session_id.clone(),
                    reply: reply_tx,
                })
                .await
                .map_err(|_send_err| anyhow::anyhow!("cron scheduler command channel closed"))?;

            let result = reply_rx
                .await
                .map_err(|_recv_err| anyhow::anyhow!("cron scheduler reply lost"))??;

            info!(
                cron.name = %result.cron_name,
                cron.deliver = args.deliver,
                cron.status = ?result.status,
                cron.delivered_to = result.delivered_to,
                "manual cron trigger finished"
            );

            Ok(format_result(result, args.deliver))
        }
        .instrument(span)
        .await
    }
}

fn cron_effective_trust(config: &Config, user_name: Option<&str>) -> TrustLevel {
    let user_trust = user_name
        .and_then(|user| {
            config
                .users
                .iter()
                .find(|candidate| candidate.name.as_str() == user)
        })
        .map_or(TrustLevel::Full, |user| user.trust);
    resolve_trust(user_trust, TrustLevel::Owner)
}

fn trust_label(trust: TrustLevel) -> &'static str {
    match trust {
        TrustLevel::Owner => "Owner",
        TrustLevel::Full => "Full",
        TrustLevel::Inner => "Inner",
        TrustLevel::Familiar => "Familiar",
        TrustLevel::Public => "Public",
    }
}

fn format_result(result: CronTriggerResult, deliver_requested: bool) -> ToolOutput {
    match result.status {
        CronTriggerStatus::Completed => {
            let response = result.response.unwrap_or_default();
            if deliver_requested {
                if result.delivered_to > 0 {
                    ToolOutput::success(format!(
                        "Cron '{}' completed and delivered to {} target(s).\n\n{}",
                        result.cron_name, result.delivered_to, response
                    ))
                } else if result.attempted_to > 0 {
                    ToolOutput::success(format!(
                        "Cron '{}' completed, but delivery failed for all {} target(s).\n\n{}",
                        result.cron_name, result.attempted_to, response
                    ))
                } else {
                    ToolOutput::success(format!(
                        "Cron '{}' completed, but no delivery targets were resolved.\n\n{}",
                        result.cron_name, response
                    ))
                }
            } else {
                ToolOutput::success(format!(
                    "Cron '{}' completed.\n\n{}",
                    result.cron_name, response
                ))
            }
        }
        CronTriggerStatus::CompletedEmpty => ToolOutput::success(format!(
            "Cron '{}' completed, but it returned an empty response.",
            result.cron_name
        )),
        CronTriggerStatus::Suppressed => ToolOutput::success(format!(
            "Cron '{}' ran, but delivery was suppressed because no action was needed.",
            result.cron_name
        )),
        CronTriggerStatus::SkippedHeartbeat => ToolOutput::success(format!(
            "Cron '{}' was skipped because HEARTBEAT.md is empty.",
            result.cron_name
        )),
        CronTriggerStatus::Busy => {
            ToolOutput::error(format!("Cron '{}' is already running.", result.cron_name))
        }
    }
}

#[allow(missing_debug_implementations)]
pub(crate) struct CronToolExecutor {
    tool: CronTriggerTool,
}

impl CronToolExecutor {
    pub(crate) fn new(config: SharedConfig, command_tx: CronCommandSender) -> Self {
        Self {
            tool: CronTriggerTool::new(config, command_tx),
        }
    }
}

#[async_trait]
impl ToolExecutor for CronToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if name == "cron_trigger" {
            self.tool.execute(arguments, ctx).await
        } else {
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.tool.definition()]
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CronConfig, UserConfig, shared_config};
    use coop_core::ToolContext;
    use std::path::PathBuf;

    fn tool_context(session_kind: SessionKind, trust: TrustLevel) -> ToolContext {
        ToolContext::new(
            "test:dm:signal:alice-uuid",
            session_kind,
            trust,
            PathBuf::from("."),
            None,
        )
    }

    fn make_config(cron: CronConfig, users: Vec<UserConfig>) -> SharedConfig {
        let mut config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\n\n[provider]\nname = \"anthropic\"\n",
        )
        .unwrap();
        config.cron = vec![cron];
        config.users = users;
        shared_config(config)
    }

    fn inner_user(name: &str) -> UserConfig {
        UserConfig {
            name: name.to_owned(),
            trust: TrustLevel::Inner,
            model: None,
            r#match: vec![format!("signal:{name}-uuid")],
            timezone: None,
            sandbox: None,
        }
    }

    fn owner_user(name: &str) -> UserConfig {
        UserConfig {
            name: name.to_owned(),
            trust: TrustLevel::Owner,
            model: None,
            r#match: vec![format!("signal:{name}-uuid")],
            timezone: None,
            sandbox: None,
        }
    }

    #[tokio::test]
    async fn cron_trigger_rejects_cron_sessions() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let executor = CronToolExecutor::new(
            make_config(
                CronConfig {
                    name: "heartbeat".to_owned(),
                    cron: "*/30 * * * *".to_owned(),
                    timezone: None,
                    message: "check HEARTBEAT.md".to_owned(),
                    user: None,
                    delivery: None,
                    deliver: None,
                    review_prompt: None,
                    sandbox: None,
                },
                Vec::new(),
            ),
            command_tx,
        );

        let output = executor
            .execute(
                "cron_trigger",
                serde_json::json!({"name": "heartbeat"}),
                &tool_context(SessionKind::Cron("heartbeat".to_owned()), TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("cannot be used from a cron session")
        );
    }

    #[tokio::test]
    async fn cron_trigger_rejects_lower_trust_than_target_cron() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let executor = CronToolExecutor::new(
            make_config(
                CronConfig {
                    name: "owner-task".to_owned(),
                    cron: "0 8 * * *".to_owned(),
                    timezone: None,
                    message: "do owner task".to_owned(),
                    user: Some("alice".to_owned()),
                    delivery: None,
                    deliver: None,
                    review_prompt: None,
                    sandbox: None,
                },
                vec![owner_user("alice")],
            ),
            command_tx,
        );

        let output = executor
            .execute(
                "cron_trigger",
                serde_json::json!({"name": "owner-task"}),
                &tool_context(SessionKind::Main, TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("executes with Owner trust"));
    }

    #[tokio::test]
    async fn cron_trigger_sends_command_and_formats_result() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let executor = CronToolExecutor::new(
            make_config(
                CronConfig {
                    name: "inner-task".to_owned(),
                    cron: "0 8 * * *".to_owned(),
                    timezone: None,
                    message: "do inner task".to_owned(),
                    user: Some("bob".to_owned()),
                    delivery: None,
                    deliver: None,
                    review_prompt: None,
                    sandbox: None,
                },
                vec![inner_user("bob")],
            ),
            command_tx,
        );

        let reply_task = tokio::spawn(async move {
            let Some(CronCommand::RunNow {
                name,
                deliver,
                origin_session_id,
                reply,
            }) = command_rx.recv().await
            else {
                panic!("expected cron command");
            };
            assert_eq!(name, "inner-task");
            assert!(!deliver);
            assert_eq!(origin_session_id, "test:dm:signal:alice-uuid");
            reply
                .send(Ok(CronTriggerResult {
                    cron_name: "inner-task".to_owned(),
                    status: CronTriggerStatus::Completed,
                    response: Some("cron response ok".to_owned()),
                    delivered_to: 0,
                    attempted_to: 0,
                }))
                .unwrap();
        });

        let output = executor
            .execute(
                "cron_trigger",
                serde_json::json!({"name": "inner-task"}),
                &tool_context(SessionKind::Main, TrustLevel::Inner),
            )
            .await
            .unwrap();

        reply_task.await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Cron 'inner-task' completed."));
        assert!(output.content.contains("cron response ok"));
    }
}
