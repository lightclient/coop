use crate::config::CronDeliveryMode;
use crate::group_trigger::SILENT_REPLY_TOKEN;
use coop_core::prompt::channel_family;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalReplyPolicy {
    None,
    Signal,
    CronAlways,
    CronAsNeeded,
}

impl FinalReplyPolicy {
    pub(crate) fn for_turn(
        channel: Option<&str>,
        cron_delivery_mode: Option<CronDeliveryMode>,
    ) -> Self {
        match cron_delivery_mode {
            Some(CronDeliveryMode::Always) => Self::CronAlways,
            Some(CronDeliveryMode::AsNeeded) => Self::CronAsNeeded,
            None if channel.is_some_and(|value| channel_family(value) == "signal") => Self::Signal,
            None => Self::None,
        }
    }

    pub(crate) fn needs_repair(self, text: &str) -> bool {
        !matches!(self, Self::None) && text.trim().is_empty()
    }

    pub(crate) fn repair_prompt(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Signal => Some(format!(
                "Your previous turn ended without the final Signal reply. Reply now with exactly one short, non-empty final message for the user. Do not use tools. Do not explain your reasoning. If higher-priority instructions in this conversation explicitly require a suppression token such as {SILENT_REPLY_TOKEN}, reply with that token exactly. Otherwise, do not stay silent."
            )),
            Self::CronAlways => Some(
                "Your previous turn ended without the scheduled message to deliver. Reply now with exactly one non-empty final message to deliver to the user. Do not use tools. Do not explain your reasoning. Do not reply with NO_ACTION_NEEDED.".to_owned(),
            ),
            Self::CronAsNeeded => Some(
                "Your previous turn ended without the final scheduled output. Reply now with exactly one of the following: (1) NO_ACTION_NEEDED if nothing should be delivered, or (2) the non-empty message to deliver. Do not use tools. Do not explain your reasoning.".to_owned(),
            ),
        }
    }

    pub(crate) fn fallback_text(self) -> Option<&'static str> {
        match self {
            Self::Signal => Some("Done."),
            Self::CronAlways => Some("Completed."),
            Self::CronAsNeeded | Self::None => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Signal => "signal",
            Self::CronAlways => "cron_always",
            Self::CronAsNeeded => "cron_as_needed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_turns_require_repair() {
        let policy = FinalReplyPolicy::for_turn(Some("signal"), None);
        assert_eq!(policy, FinalReplyPolicy::Signal);
        assert!(policy.needs_repair("   \n"));
    }

    #[test]
    fn cron_delivery_takes_priority_over_signal_channel() {
        let policy = FinalReplyPolicy::for_turn(Some("signal"), Some(CronDeliveryMode::Always));
        assert_eq!(policy, FinalReplyPolicy::CronAlways);
    }

    #[test]
    fn terminal_channel_does_not_require_repair() {
        let policy = FinalReplyPolicy::for_turn(Some("terminal:default"), None);
        assert_eq!(policy, FinalReplyPolicy::None);
        assert!(!policy.needs_repair(""));
    }

    #[test]
    fn signal_has_fallback() {
        assert_eq!(FinalReplyPolicy::Signal.fallback_text(), Some("Done."));
    }

    #[test]
    fn as_needed_has_no_fallback() {
        assert!(FinalReplyPolicy::CronAsNeeded.fallback_text().is_none());
    }
}
