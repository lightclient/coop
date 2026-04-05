mod policy;
pub(crate) mod prompt;
pub(crate) mod registry;
pub(crate) mod runtime;
pub(crate) mod tools;

use coop_core::Message;
use serde::{Deserialize, Serialize};

pub(crate) use runtime::SubagentManager;
#[allow(unused_imports)]
pub(crate) use tools::SubagentToolExecutor;

use registry::SubagentRunStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentMode {
    #[default]
    Wait,
    Background,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubagentSpawnRequest {
    pub task: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub mode: SubagentMode,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentControlAction {
    List,
    Inspect,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubagentsControlRequest {
    pub action: SubagentControlAction,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnOverrides {
    pub model: Option<String>,
    pub prompt_blocks: Option<Vec<String>>,
    pub tool_names: Option<Vec<String>>,
    pub initial_message: Option<Message>,
    pub max_iterations: Option<u32>,
}

impl TurnOverrides {
    #[must_use]
    pub(crate) fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub(crate) fn with_prompt_blocks(mut self, prompt_blocks: Vec<String>) -> Self {
        self.prompt_blocks = Some(prompt_blocks);
        self
    }

    #[must_use]
    pub(crate) fn with_tool_names<I, S>(mut self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tool_names = Some(tool_names.into_iter().map(Into::into).collect());
        self
    }

    #[must_use]
    pub(crate) fn with_initial_message(mut self, initial_message: Message) -> Self {
        self.initial_message = Some(initial_message);
        self
    }

    #[must_use]
    pub(crate) fn with_max_iterations(mut self, max_iterations: u32) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCompletion {
    pub status: SubagentRunStatus,
    pub summary: Option<String>,
    pub artifact_paths: Vec<String>,
    pub error: Option<String>,
}

impl SubagentCompletion {
    pub(crate) fn new(
        status: SubagentRunStatus,
        summary: Option<String>,
        artifact_paths: Vec<String>,
        error: Option<String>,
    ) -> Self {
        Self {
            status,
            summary,
            artifact_paths,
            error,
        }
    }
}

impl SubagentRunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}
