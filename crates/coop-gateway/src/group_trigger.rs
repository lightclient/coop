use coop_core::InboundMessage;
use regex::Regex;

use crate::config::{GroupConfig, GroupTrigger};

pub(crate) const SILENT_REPLY_TOKEN: &str = "NO_REPLY";

pub(crate) const DEFAULT_TRIGGER_PROMPT: &str = "\
You are evaluating whether the assistant should respond to the latest message \
in this group chat. You have the full conversation context and system \
instructions above.

Evaluate the most recent message and reply with ONLY \"YES\" or \"NO\":
- YES: The assistant should respond (message is directed at the assistant, \
asks a question, requests help, or the assistant can add clear value)
- NO: The assistant should stay silent (casual chatter between other people, \
reactions, status updates, messages not relevant to the assistant)

Reply with only YES or NO, nothing else.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TriggerDecision {
    Respond,
    Skip,
}

/// Evaluate non-LLM triggers (always, mention, regex).
/// For trigger = "llm", the caller must use `Gateway::evaluate_llm_trigger()`.
///
/// `agent_id` is the agent's configured name — mention trigger automatically
/// fires on `@agent_id` without requiring explicit `mention_names` config.
// TODO: implicit mention on reply-to (requires tracking message timestamps)
pub(crate) fn evaluate_trigger(
    msg: &InboundMessage,
    group_config: &GroupConfig,
    agent_id: &str,
) -> TriggerDecision {
    match group_config.trigger {
        GroupTrigger::Always => TriggerDecision::Respond,
        GroupTrigger::Mention => {
            let body = strip_envelope_prefix(&msg.content);
            let lower = body.to_lowercase();
            // Always trigger on @agent_name
            let agent_mention = format!("@{}", agent_id.to_lowercase());
            if lower.contains(&agent_mention) {
                return TriggerDecision::Respond;
            }
            if group_config
                .mention_names
                .iter()
                .any(|name| lower.contains(&name.to_lowercase()))
            {
                TriggerDecision::Respond
            } else {
                TriggerDecision::Skip
            }
        }
        GroupTrigger::Regex => {
            let body = strip_envelope_prefix(&msg.content);
            if let Some(pattern) = &group_config.trigger_regex {
                match Regex::new(pattern) {
                    Ok(re) => {
                        if re.is_match(body) {
                            TriggerDecision::Respond
                        } else {
                            TriggerDecision::Skip
                        }
                    }
                    Err(_) => TriggerDecision::Skip,
                }
            } else {
                TriggerDecision::Skip
            }
        }
        // LLM trigger is handled async by Gateway::evaluate_llm_trigger()
        GroupTrigger::Llm => unreachable!("LLM trigger must be evaluated by gateway"),
    }
}

/// Check if assistant output is the silent reply token.
/// Used by signal_loop (cfg=signal) — allow dead_code for non-signal builds.
#[allow(dead_code)]
pub(crate) fn is_silent_reply(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == SILENT_REPLY_TOKEN
        || trimmed == format!("\"{SILENT_REPLY_TOKEN}\"")
        || trimmed == format!("'{SILENT_REPLY_TOKEN}'")
}

/// Strip the `[from ... at ...]` envelope prefix from a message body.
pub(crate) fn strip_envelope_prefix(content: &str) -> &str {
    // Format: "[from <sender> in <group> at <timestamp>] <body>"
    if content.starts_with("[from ")
        && let Some(close) = content.find("] ")
    {
        return &content[close + 2..];
    }
    content
}

// TODO: implicit mention on reply-to

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GroupTrigger, TrustCeiling};
    use chrono::Utc;
    use coop_core::{InboundKind, InboundMessage, TrustLevel};

    fn make_msg(content: &str) -> InboundMessage {
        InboundMessage {
            channel: "signal".to_owned(),
            sender: "alice-uuid".to_owned(),
            content: content.to_owned(),
            chat_id: Some("group:deadbeef".to_owned()),
            is_group: true,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
            group_revision: None,
        }
    }

    fn mention_config(names: &[&str]) -> GroupConfig {
        GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Mention,
            mention_names: names.iter().map(|s| (*s).to_owned()).collect(),
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        }
    }

    fn regex_config(pattern: &str) -> GroupConfig {
        GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Regex,
            mention_names: vec![],
            trigger_regex: Some(pattern.to_owned()),
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        }
    }

    fn always_config() -> GroupConfig {
        GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Always,
            mention_names: vec![],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        }
    }

    #[test]
    fn always_trigger_responds() {
        let msg = make_msg("random chatter");
        assert_eq!(
            evaluate_trigger(&msg, &always_config(), "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn mention_matches_case_insensitively() {
        let msg = make_msg("Hey COOP, what's up?");
        let cfg = mention_config(&["coop"]);
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn mention_skips_unrelated() {
        let msg = make_msg("hey everyone, random chatter");
        let cfg = mention_config(&["coop"]);
        assert_eq!(evaluate_trigger(&msg, &cfg, "reid"), TriggerDecision::Skip);
    }

    #[test]
    fn mention_strips_envelope_prefix() {
        let msg = make_msg("[from alice-uuid in group:dead at 12345] hey coop help me");
        let cfg = mention_config(&["coop"]);
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn mention_matches_partial_word() {
        let msg = make_msg("@coop please help");
        let cfg = mention_config(&["coop"]);
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn regex_matches_pattern() {
        let msg = make_msg("!ask what time is it");
        let cfg = regex_config("^!(ask|help)");
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn regex_skips_non_matching() {
        let msg = make_msg("just chatting");
        let cfg = regex_config("^!(ask|help)");
        assert_eq!(evaluate_trigger(&msg, &cfg, "reid"), TriggerDecision::Skip);
    }

    #[test]
    fn regex_strips_envelope_prefix() {
        let msg = make_msg("[from alice in group:dead at 123] !ask something");
        let cfg = regex_config("^!(ask|help)");
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn is_silent_reply_detects_token() {
        assert!(is_silent_reply("NO_REPLY"));
    }

    #[test]
    fn is_silent_reply_detects_with_whitespace() {
        assert!(is_silent_reply("  NO_REPLY  \n"));
    }

    #[test]
    fn is_silent_reply_rejects_normal_text() {
        assert!(!is_silent_reply("Sure, I can help with that."));
    }

    #[test]
    fn is_silent_reply_rejects_mid_sentence() {
        assert!(!is_silent_reply("I think NO_REPLY is the answer"));
    }

    #[test]
    fn strip_envelope_prefix_strips_correctly() {
        assert_eq!(
            strip_envelope_prefix("[from alice in group:dead at 123] hello"),
            "hello"
        );
    }

    #[test]
    fn strip_envelope_prefix_handles_no_prefix() {
        assert_eq!(strip_envelope_prefix("just a message"), "just a message");
    }

    #[test]
    fn mention_auto_triggers_on_agent_name() {
        let msg = make_msg("hey @reid can you help?");
        let cfg = mention_config(&[]); // no explicit mention_names
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn mention_auto_trigger_case_insensitive() {
        let msg = make_msg("@REID what do you think?");
        let cfg = mention_config(&[]);
        assert_eq!(
            evaluate_trigger(&msg, &cfg, "reid"),
            TriggerDecision::Respond
        );
    }

    #[test]
    fn mention_skips_without_agent_or_config_names() {
        let msg = make_msg("hey @someone else");
        let cfg = mention_config(&[]);
        assert_eq!(evaluate_trigger(&msg, &cfg, "reid"), TriggerDecision::Skip);
    }
}
