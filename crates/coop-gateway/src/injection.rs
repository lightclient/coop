use coop_core::{SessionKey, TrustLevel};

/// An internally-generated message injected directly into a session.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SessionInjection {
    pub target: SessionKey,
    pub content: String,
    pub trust: TrustLevel,
    pub user_name: Option<String>,
    pub prompt_channel: Option<String>,
    pub source: InjectionSource,
}

/// Origin of a session injection for tracing and policy decisions.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum InjectionSource {
    Cron(String),
    Session(SessionKey),
    System,
}
