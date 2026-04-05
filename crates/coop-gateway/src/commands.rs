use coop_core::SessionKey;
use coop_core::TrustLevel;

use crate::gateway::Gateway;

fn format_number(value: u64) -> String {
    let digits = value.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);

    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }

    reversed.chars().rev().collect()
}

pub(crate) async fn handle_slash_command(
    gateway: &Gateway,
    input: &str,
    session_key: &SessionKey,
    trust: TrustLevel,
    channel: Option<&str>,
    user_name: Option<&str>,
) -> Option<String> {
    let trimmed = input.trim();
    let command = trimmed.split_whitespace().next()?;

    match command {
        "/new" | "/clear" | "/reset" => {
            gateway.clear_session(session_key);
            Some("New session ✅".to_owned())
        }
        "/stop" => {
            if gateway.cancel_active_turn(session_key) {
                Some("Stopping agent…".to_owned())
            } else {
                Some("No active turn to stop.".to_owned())
            }
        }
        "/status" => Some(format_status(gateway, session_key, user_name)),
        "/models" => Some(format_models(gateway, user_name)),
        "/model" => Some(
            handle_model_command(gateway, trimmed, session_key, trust, channel, user_name).await,
        ),
        "/subagents" => Some(handle_subagents_command(gateway, trimmed)),
        "/help" | "/?" => Some(help_text().to_owned()),
        _ => None,
    }
}

fn format_status(gateway: &Gateway, session_key: &SessionKey, user_name: Option<&str>) -> String {
    let count = gateway.session_message_count(session_key);
    let usage = gateway.session_usage(session_key);
    let resolved = gateway.resolve_main_model(user_name);
    let (model, context_limit) = match resolved {
        Ok(selection) => (selection.model, selection.context_limit),
        Err(error) => (format!("unavailable ({error})"), 0),
    };
    #[allow(clippy::cast_precision_loss)]
    let context_pct = if context_limit > 0 {
        f64::from(usage.last_input_tokens) / (context_limit as f64) * 100.0
    } else {
        0.0
    };
    let active = if gateway.has_active_turn(session_key) {
        " (running)"
    } else {
        ""
    };
    let active_subagents = gateway
        .subagents()
        .list_runs()
        .into_iter()
        .filter(|run| run.parent_session_key == *session_key && run.status.is_active())
        .count();
    format!(
        "Session: {}{active}\nAgent: {}\nModel: {}\nMessages: {}\nContext: {} / {} tokens ({:.1}%)\nTotal tokens used: {} in / {} out\nActive subagents: {}",
        session_key,
        gateway.agent_id(),
        model,
        count,
        format_number(u64::from(usage.last_input_tokens)),
        format_number(context_limit as u64),
        context_pct,
        format_number(u64::from(usage.cumulative.input_tokens.unwrap_or(0))),
        format_number(u64::from(usage.cumulative.output_tokens.unwrap_or(0))),
        active_subagents,
    )
}

fn format_models(gateway: &Gateway, user_name: Option<&str>) -> String {
    let current = gateway.model_name_for_user(user_name);
    let default_model = gateway.configured_model_name_for_user(user_name);
    let mut lines = vec!["Available models:".to_owned()];

    for model in gateway.available_main_models() {
        let mut tags = Vec::new();
        if Gateway::same_model(&model.id, &current) {
            tags.push("current");
        }
        if Gateway::same_model(&model.id, &default_model) {
            tags.push("default");
        }

        let mut line = format!(
            "  {} {}",
            if tags.contains(&"current") { "*" } else { "-" },
            model.id
        );

        let mut details = Vec::new();
        if let Some(description) = model.description {
            details.push(description);
        }
        if let Some(context_limit) = gateway.configured_context_limit_for_model(&model.id) {
            details.push(format!("{} tokens", format_number(context_limit as u64)));
        }
        if !details.is_empty() {
            line.push_str(" — ");
            line.push_str(&details.join(" · "));
        }

        let aliases = gateway.model_aliases(&model.id);
        if !aliases.is_empty() || !tags.is_empty() {
            line.push_str(" (");
            if !aliases.is_empty() {
                line.push_str("aliases: ");
                line.push_str(&aliases.join(", "));
                if !tags.is_empty() {
                    line.push_str("; ");
                }
            }
            if !tags.is_empty() {
                line.push_str(&tags.join(", "));
            }
            line.push(')');
        }
        lines.push(line);
    }

    lines.push("Use /model <id> to switch the primary session model.".to_owned());
    lines.push(
        "Use subagent profiles for specialized models and bounded delegated work.".to_owned(),
    );
    lines.join("\n")
}

async fn handle_model_command(
    gateway: &Gateway,
    input: &str,
    session_key: &SessionKey,
    trust: TrustLevel,
    channel: Option<&str>,
    user_name: Option<&str>,
) -> String {
    let requested = input
        .strip_prefix("/model")
        .map(str::trim)
        .unwrap_or_default();

    if requested.is_empty() {
        let current = gateway.model_name_for_user(user_name);
        return format!(
            "Current model: {current}\nUse /models to list available models.\nUse /model <id> to switch."
        );
    }

    match gateway
        .set_user_model_for_session(session_key, trust, user_name, channel, requested)
        .await
    {
        Ok(outcome) => {
            let mut response = format!(
                "Model set to {} ✅\nContext window: {} tokens",
                outcome.selection.model,
                format_number(outcome.selection.context_limit as u64)
            );
            if outcome.compacted_for_handoff {
                response.push_str("\nSession compacted before handoff ✅");
            }
            response
        }
        Err(error) => format!("Could not change model: {error}"),
    }
}

fn handle_subagents_command(gateway: &Gateway, input: &str) -> String {
    let mut parts = input.split_whitespace();
    let _command = parts.next();
    let action = parts.next().unwrap_or("list");

    match action {
        "list" => {
            let runs = gateway.subagents().list_runs();
            if runs.is_empty() {
                return "No subagent runs yet.".to_owned();
            }
            let mut lines = vec!["Subagent runs:".to_owned()];
            for run in runs.into_iter().take(10) {
                lines.push(format!(
                    "- {}  {}  {}  {}",
                    run.run_id,
                    run.status.as_str(),
                    run.model,
                    run.task.lines().next().unwrap_or_default()
                ));
            }
            lines.join("\n")
        }
        "inspect" => {
            let Some(run_id) = parts.next() else {
                return "Usage: /subagents inspect <run_id>".to_owned();
            };
            match gateway.subagents().inspect_run(run_id) {
                Ok(run) => {
                    let mut lines = vec![
                        format!("Run: {}", run.run_id),
                        format!("Status: {}", run.status.as_str()),
                        format!("Model: {}", run.model),
                        format!("Child session: {}", run.child_session_key),
                        format!("Parent session: {}", run.parent_session_key),
                        format!("Depth: {}", run.depth),
                        format!("Timeout: {}s", run.timeout_seconds),
                        format!("Max turns: {}", run.max_turns),
                        format!("Task: {}", run.task),
                    ];
                    if let Some(profile) = run.profile {
                        lines.push(format!("Profile: {profile}"));
                    }
                    if !run.tool_names.is_empty() {
                        lines.push(format!("Tools: {}", run.tool_names.join(", ")));
                    }
                    if !run.paths.is_empty() {
                        lines.push(format!("Paths: {}", run.paths.join(", ")));
                    }
                    if !run.artifact_paths.is_empty() {
                        lines.push(format!("Artifacts: {}", run.artifact_paths.join(", ")));
                    }
                    if let Some(summary) = run.summary {
                        lines.push(String::new());
                        lines.push("Summary:".to_owned());
                        lines.push(summary);
                    }
                    if let Some(error) = run.error {
                        lines.push(format!("Error: {error}"));
                    }
                    lines.join("\n")
                }
                Err(error) => format!("Could not inspect subagent run: {error}"),
            }
        }
        "kill" => {
            let Some(run_id) = parts.next() else {
                return "Usage: /subagents kill <run_id>".to_owned();
            };
            match uuid::Uuid::parse_str(run_id) {
                Ok(run_id) => match gateway
                    .subagents()
                    .cancel_run(run_id, "stopped via /subagents kill")
                {
                    Ok(true) => format!("Stopped subagent {run_id} ✅"),
                    Ok(false) => format!("No active subagent found for {run_id}."),
                    Err(error) => format!("Could not stop subagent: {error}"),
                },
                Err(error) => format!("Invalid subagent run id: {error}"),
            }
        }
        _ => "Usage: /subagents [list|inspect <run_id>|kill <run_id>]".to_owned(),
    }
}

fn help_text() -> &'static str {
    "Available commands:\n\
         /new, /clear        — Start a new session (clears history)\n\
         /stop               — Stop the current agent turn and any active child runs\n\
         /status             — Show session info\n\
         /models             — List available primary models\n\
         /model <id>         — Switch your current primary model\n\
         /subagents          — List active and recent subagent runs\n\
         /subagents inspect <run_id> — Show subagent run details\n\
         /subagents kill <run_id>    — Stop an active subagent run\n\
         /help, /?           — Show this help"
}
