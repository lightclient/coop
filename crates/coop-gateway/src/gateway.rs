use anyhow::Result;
use coop_core::{Message, Provider, SessionKey, SessionKind};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::Config;

/// Core gateway: manages sessions and routes messages to the agent runtime.
pub(crate) struct Gateway {
    config: Config,
    system_prompt: String,
    provider: Arc<dyn Provider>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
}

impl Gateway {
    pub(crate) fn new(
        config: Config,
        system_prompt: String,
        provider: Arc<dyn Provider>,
    ) -> Self {
        Self {
            config,
            system_prompt,
            provider,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The default session key for the configured agent.
    pub(crate) fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    /// Handle a user message: append to session, call provider, return response text.
    ///
    /// This is the simple synchronous path — no tool calling yet.
    /// The agent loop (with tool dispatch) will replace this.
    pub(crate) async fn handle_message(
        &self,
        session_key: &SessionKey,
        user_input: &str,
    ) -> Result<String> {
        // Append user message to session
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(session_key.clone())
                .or_default()
                .push(Message::user().with_text(user_input));
        }

        // Get full history
        let messages = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_key).cloned().unwrap_or_default()
        };

        // Call provider directly (no tool loop yet)
        let (response, _usage) = self
            .provider
            .complete(&self.system_prompt, &messages, &[])
            .await?;

        let text = response.text();

        // Append assistant response to session
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(session_key.clone())
                .or_default()
                .push(response);
        }

        Ok(text)
    }

    /// Clear a session.
    #[allow(dead_code)]
    pub(crate) fn clear_session(&self, session_key: &SessionKey) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_key);
    }
}
