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
    format!(
        "Session: {}{active}\nAgent: {}\nModel: {}\nMessages: {}\nContext: {} / {} tokens ({:.1}%)\nTotal tokens used: {} in / {} out",
        session_key,
        gateway.agent_id(),
        model,
        count,
        format_number(u64::from(usage.last_input_tokens)),
        format_number(context_limit as u64),
        context_pct,
        format_number(u64::from(usage.cumulative.input_tokens.unwrap_or(0))),
        format_number(u64::from(usage.cumulative.output_tokens.unwrap_or(0))),
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

    lines.push("Use /model <id> to switch.".to_owned());
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

fn help_text() -> &'static str {
    "Available commands:\n\
         /new, /clear  — Start a new session (clears history)\n\
         /stop         — Stop the current agent turn\n\
         /status       — Show session info\n\
         /models       — List available models\n\
         /model <id>   — Switch your current model\n\
         /help, /?     — Show this help"
}
