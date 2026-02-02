use anyhow::Result;
use coop_core::{AgentRuntime, Message, SessionKey, SessionKind};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::Config;

/// Core gateway: manages sessions and routes messages to the agent runtime.
pub struct Gateway {
    config: Config,
    system_prompt: String,
    runtime: Arc<dyn AgentRuntime>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
}

impl Gateway {
    pub fn new(
        config: Config,
        system_prompt: String,
        runtime: Arc<dyn AgentRuntime>,
    ) -> Self {
        Self {
            config,
            system_prompt,
            runtime,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The default session key for the configured agent.
    pub fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    /// Handle a user message: append to session, call agent, return response.
    pub async fn handle_message(
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
                .push(Message::user(user_input));
        }

        // Get full history
        let messages = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_key).cloned().unwrap_or_default()
        };

        // Call the agent runtime
        let response = self.runtime.turn(&messages, &self.system_prompt).await?;

        // Append assistant response to session
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(session_key.clone())
                .or_default()
                .push(Message::assistant(&response.content));
        }

        Ok(response.content)
    }

    /// Clear a session.
    pub fn clear_session(&self, session_key: &SessionKey) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_key);
    }
}
