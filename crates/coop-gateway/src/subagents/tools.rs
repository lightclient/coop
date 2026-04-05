use anyhow::Result;
use async_trait::async_trait;
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::{ToolDef, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

use super::runtime::SubagentManager;

pub(crate) struct SubagentToolExecutor {
    manager: Arc<SubagentManager>,
}

impl SubagentToolExecutor {
    pub(crate) fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl ToolExecutor for SubagentToolExecutor {
    async fn execute(&self, name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let result: Result<ToolOutput> = match name {
            "subagent_spawn" => match serde_json::from_value(arguments) {
                Ok(request) => {
                    Arc::clone(&self.manager)
                        .spawn_from_tool(request, ctx)
                        .await
                }
                Err(error) => Err(anyhow::Error::from(error)),
            },
            "subagents" => match serde_json::from_value(arguments) {
                Ok(request) => self.manager.control_from_tool(request),
                Err(error) => Err(anyhow::Error::from(error)),
            },
            _ => Ok(ToolOutput::error(format!("unknown tool: {name}"))),
        };

        Ok(match result {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        })
    }

    fn tools(&self) -> Vec<ToolDef> {
        if !self.manager.enabled() {
            return Vec::new();
        }

        vec![
            ToolDef::new(
                "subagent_spawn",
                "Spawn a child agent in a fresh isolated session to complete a delegated task.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Self-contained delegated task for the child session." },
                        "context": { "type": "string", "description": "Optional background context for the child." },
                        "profile": { "type": "string", "description": "Optional subagent profile from agent.subagents.profiles." },
                        "model": { "type": "string", "description": "Optional model override for this child session." },
                        "tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional further narrowing of the child tool set."
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Workspace-relative or scope-resolved file paths handed to the child explicitly."
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["wait", "background"],
                            "description": "wait blocks until completion; background returns immediately."
                        },
                        "max_turns": { "type": "integer", "minimum": 1 },
                        "timeout_seconds": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["task"]
                }),
            ),
            ToolDef::new(
                "subagents",
                "List, inspect, or stop active and recent subagent runs.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "inspect", "kill"]
                        },
                        "run_id": { "type": "string" }
                    },
                    "required": ["action"]
                }),
            ),
        ]
    }
}
