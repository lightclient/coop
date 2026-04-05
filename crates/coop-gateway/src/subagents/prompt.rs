use coop_core::{Content, Message, WorkspaceScope};
use std::path::Path;

const OVERRIDE_MARKER: &str = "<!-- override -->";
const TOOLS_DEFAULT: &str = include_str!("../../../../workspaces/default/TOOLS.md");
const AGENTS_DEFAULT: &str = "\
# Instructions

You are an AI agent running inside Coop, a personal agent gateway.
Help the user with their tasks. Be concise, direct, and useful.

When using tools, explain what you're doing briefly.

## Scheduled Tasks

Some cron sessions are auto-delivered to the user's channels.
Follow any runtime scheduled-delivery instructions exactly.
For `delivery = \"as_needed\"`, be highly selective: only interrupt the user for
important, actionable, or time-sensitive items.
If the runtime instructions say to reply with **NO_ACTION_NEEDED**, do so only when
nothing needs attention. Never include NO_ACTION_NEEDED alongside real content.
Keep scheduled-task responses concise when they are destined for messaging channels.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSpawnPath {
    pub display_path: String,
    pub is_image: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedChildPrompt {
    pub system_blocks: Vec<String>,
    pub initial_message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedChildResponse {
    pub summary: String,
    pub artifact_paths: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_minimal_child_prompt(
    workspace: &Path,
    scope: &WorkspaceScope,
    task: &str,
    context: Option<&str>,
    profile: Option<&str>,
    model: &str,
    depth: u32,
    paths: &[ResolvedSpawnPath],
) -> PreparedChildPrompt {
    let instructions = render_subagent_instructions(profile, model, depth, paths.len());
    let agents = format!(
        "## AGENTS.md\n{}",
        load_with_default(workspace, "AGENTS.md", AGENTS_DEFAULT)
    );
    let tools = format!(
        "## TOOLS.md\n{}",
        load_with_default(workspace, "TOOLS.md", TOOLS_DEFAULT)
    );
    let initial_message = build_initial_message(scope, task, context, paths);

    PreparedChildPrompt {
        system_blocks: vec![instructions, agents, tools],
        initial_message,
    }
}

pub(crate) fn build_initial_message(
    scope: &WorkspaceScope,
    task: &str,
    context: Option<&str>,
    paths: &[ResolvedSpawnPath],
) -> Message {
    let mut parts = vec!["# Delegated Task".to_owned(), task.trim().to_owned()];

    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        parts.push(String::new());
        parts.push("# Context".to_owned());
        parts.push(context.to_owned());
    }

    if !paths.is_empty() {
        parts.push(String::new());
        parts.push("# Provided Paths".to_owned());
        parts.extend(paths.iter().map(|path| format!("- {}", path.display_path)));
        parts.push(
            "Use these files directly. If image blocks are attached below, they correspond to the provided paths above."
                .to_owned(),
        );
    }

    parts.push(String::new());
    parts.push("# Completion Contract".to_owned());
    parts.push(
        "Finish with this exact structure:\n\nSummary:\n<concise summary for the parent>\n\nArtifacts:\n- ./relative/path\n- ./another/path\n\nOnly include the Artifacts section when there are relevant files to report."
            .to_owned(),
    );

    let mut message = Message::user().with_text(parts.join("\n"));

    for path in paths {
        if !path.is_image {
            continue;
        }
        if let Ok((data, mime_type)) = coop_core::images::load_image(&path.display_path, scope) {
            message = message.with_content(Content::image(data, mime_type));
        }
    }

    message
}

pub(crate) fn parse_child_response(text: &str) -> ParsedChildResponse {
    let trimmed = text.trim();
    let (summary_block, artifacts_block) = if let Some(idx) = trimmed.find("\nArtifacts:") {
        (&trimmed[..idx], Some(&trimmed[idx + 1..]))
    } else if let Some(idx) = trimmed.find("Artifacts:\n") {
        (&trimmed[..idx], Some(&trimmed[idx..]))
    } else {
        (trimmed, None)
    };

    let summary = summary_block
        .strip_prefix("Summary:")
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| summary_block.trim())
        .to_owned();

    let artifact_paths = artifacts_block
        .map(|block| {
            block
                .lines()
                .skip_while(|line| !line.trim_start().starts_with("Artifacts:"))
                .skip(1)
                .filter_map(|line| line.trim().strip_prefix('-'))
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    ParsedChildResponse {
        summary,
        artifact_paths,
    }
}

fn render_subagent_instructions(
    profile: Option<&str>,
    model: &str,
    depth: u32,
    path_count: usize,
) -> String {
    let mut lines = vec![
        "You are a Coop subagent running in a fresh, isolated child session.".to_owned(),
        "Complete the delegated task independently. Do not ask the parent for clarification unless the task is impossible without it.".to_owned(),
        "You do not have the parent transcript or hidden reasoning. Only use the task, context, supplied files, and the tools available in this session.".to_owned(),
        "Do not call tools that are not listed in the tool definitions for this session.".to_owned(),
        "Your final response is returned to the parent as a summary, not as a full transcript.".to_owned(),
        format!("Current model: {model}"),
        format!("Spawn depth: {depth}"),
        format!("Supplied paths: {path_count}"),
    ];

    if let Some(profile) = profile {
        lines.push(format!("Profile: {profile}"));
    }

    lines.join("\n")
}

fn load_with_default(workspace: &Path, relative: &str, default: &str) -> String {
    let user_content = std::fs::read_to_string(workspace.join(relative)).ok();
    match user_content {
        Some(content) => resolve_default_content(default, &content),
        None => default.to_owned(),
    }
}

fn resolve_default_content(default: &str, user_content: &str) -> String {
    let trimmed = user_content.trim_start();
    if let Some(stripped) = trimmed.strip_prefix(OVERRIDE_MARKER) {
        stripped.trim_start().to_owned()
    } else if user_content.trim().is_empty() {
        default.to_owned()
    } else {
        format!("{default}\n\n{user_content}")
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{SessionKind, TrustLevel};
    use std::fs;

    fn scope(root: &Path) -> WorkspaceScope {
        WorkspaceScope::for_turn(root, &SessionKind::Main, TrustLevel::Full, Some("alice"))
    }

    #[test]
    fn parse_child_response_extracts_summary_and_artifacts() {
        let parsed = parse_child_response(
            "Summary:\nFinished the task.\n\nArtifacts:\n- ./one.txt\n- ./two.txt",
        );
        assert_eq!(parsed.summary, "Finished the task.");
        assert_eq!(parsed.artifact_paths, vec!["./one.txt", "./two.txt"]);
    }

    #[test]
    fn parse_child_response_falls_back_to_plain_text() {
        let parsed = parse_child_response("Plain answer without headings");
        assert_eq!(parsed.summary, "Plain answer without headings");
        assert!(parsed.artifact_paths.is_empty());
    }

    #[test]
    fn prepare_prompt_includes_default_agents_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare_minimal_child_prompt(
            dir.path(),
            &scope(dir.path()),
            "Do the thing",
            Some("extra context"),
            Some("code"),
            "gpt-5-codex",
            1,
            &[],
        );

        assert!(prepared.system_blocks[0].contains("Coop subagent"));
        assert!(prepared.system_blocks[1].contains("AI agent running inside Coop"));
        assert!(prepared.system_blocks[2].contains("Configuration workflow"));
        assert!(prepared.initial_message.text().contains("# Delegated Task"));
        assert!(prepared.initial_message.text().contains("extra context"));
    }

    #[test]
    fn workspace_files_extend_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Custom AGENTS").unwrap();
        fs::write(dir.path().join("TOOLS.md"), "Custom TOOLS").unwrap();

        let prepared = prepare_minimal_child_prompt(
            dir.path(),
            &scope(dir.path()),
            "Task",
            None,
            None,
            "gpt-5-codex",
            1,
            &[],
        );

        assert!(prepared.system_blocks[1].contains("Custom AGENTS"));
        assert!(prepared.system_blocks[2].contains("Custom TOOLS"));
    }
}
