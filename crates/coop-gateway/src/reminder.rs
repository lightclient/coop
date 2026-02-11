use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use coop_core::traits::{Tool, ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::info;

use crate::config::{Config, SharedConfig};

// ---------------------------------------------------------------------------
// Reminder data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Reminder {
    pub id: String,
    pub fire_at: DateTime<Utc>,
    pub message: String,
    pub user: Option<String>,
    pub delivery: Vec<(String, String)>,
    pub source_session: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// ReminderStore — in-memory + JSON file persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ReminderStore {
    inner: Arc<Mutex<Vec<Reminder>>>,
    path: PathBuf,
}

impl ReminderStore {
    pub(crate) fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let path = dir.as_ref().join("reminders.json");
        let reminders = Self::load_from_disk(&path)?;
        let count = reminders.len();
        if count > 0 {
            info!(count, path = %path.display(), "loaded pending reminders from disk");
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(reminders)),
            path,
        })
    }

    fn load_from_disk(path: &Path) -> Result<Vec<Reminder>> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let reminders: Vec<Reminder> =
                    serde_json::from_str(&content).with_context(|| {
                        format!("failed to parse reminders file: {}", path.display())
                    })?;
                Ok(reminders)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn flush(&self, reminders: &[Reminder]) {
        if let Err(e) = Self::write_to_disk(&self.path, reminders) {
            tracing::error!(error = %e, path = %self.path.display(), "failed to persist reminders");
        }
    }

    fn write_to_disk(path: &Path, reminders: &[Reminder]) -> Result<()> {
        let content = serde_json::to_string_pretty(reminders)?;
        std::fs::write(path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn add(&self, reminder: Reminder) -> String {
        let id = reminder.id.clone();
        let mut store = self.inner.lock().expect("reminder store mutex poisoned");
        store.push(reminder);
        self.flush(&store);
        drop(store);
        id
    }

    pub(crate) fn take_due(&self) -> Vec<Reminder> {
        let now = Utc::now();
        let mut store = self.inner.lock().expect("reminder store mutex poisoned");
        let (due, remaining): (Vec<_>, Vec<_>) = store.drain(..).partition(|r| r.fire_at <= now);
        *store = remaining;
        if !due.is_empty() {
            self.flush(&store);
        }
        drop(store);
        due
    }

    pub(crate) fn list_for_user(&self, user: &str) -> Vec<Reminder> {
        self.inner
            .lock()
            .expect("reminder store mutex poisoned")
            .iter()
            .filter(|r| r.user.as_deref() == Some(user))
            .cloned()
            .collect()
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        let mut store = self.inner.lock().expect("reminder store mutex poisoned");
        let len_before = store.len();
        store.retain(|r| r.id != id);
        let removed = store.len() < len_before;
        if removed {
            self.flush(&store);
        }
        drop(store);
        removed
    }

    pub(crate) fn next_fire_time(&self) -> Option<DateTime<Utc>> {
        self.inner
            .lock()
            .expect("reminder store mutex poisoned")
            .iter()
            .map(|r| r.fire_at)
            .min()
    }
}

// ---------------------------------------------------------------------------
// Delivery target resolution
// ---------------------------------------------------------------------------

pub(crate) fn resolve_reminder_delivery(
    config: &Config,
    session_id: &str,
    user_name: Option<&str>,
) -> Vec<(String, String)> {
    // Try to extract channel:target from a DM session key.
    // Format: "{agent_id}:dm:{channel}:{target}"
    if let Some(rest) = session_id.split_once(":dm:").map(|(_, rest)| rest)
        && let Some((channel, target)) = rest.split_once(':')
        && channel != "terminal"
    {
        return vec![(channel.to_owned(), target.to_owned())];
    }

    // Fallback: all non-terminal channels from user config.
    let Some(user_name) = user_name else {
        return Vec::new();
    };

    let Some(user) = config.users.iter().find(|u| u.name == user_name) else {
        return Vec::new();
    };

    user.r#match
        .iter()
        .filter_map(|pattern| {
            let (channel, target) = pattern.split_once(':')?;
            if channel == "terminal" {
                None
            } else {
                Some((channel.to_owned(), target.to_owned()))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ReminderTool
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct ReminderTool {
    store: ReminderStore,
    config: SharedConfig,
    scheduler_notify: Arc<tokio::sync::Notify>,
}

impl ReminderTool {
    fn new(
        store: ReminderStore,
        config: SharedConfig,
        scheduler_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            store,
            config,
            scheduler_notify,
        }
    }

    fn handle_set(&self, arguments: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let time_str = arguments
            .get("time")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: time"))?;

        let message = arguments
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: message"))?;

        let fire_at: DateTime<Utc> = time_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid time format: {e}"))?;

        if fire_at <= Utc::now() {
            return Ok(ToolOutput::error("reminder time must be in the future"));
        }

        let config = self.config.load();
        let delivery =
            resolve_reminder_delivery(&config, &ctx.session_id, ctx.user_name.as_deref());

        if delivery.is_empty() {
            return Ok(ToolOutput::error(
                "no delivery channel found for this user — \
                 reminders require a non-terminal channel (e.g. Signal) \
                 configured in the user's match patterns",
            ));
        }

        let id = format!("rem_{}", uuid::Uuid::new_v4().simple());

        let reminder = Reminder {
            id: id.clone(),
            fire_at,
            message: message.to_owned(),
            user: ctx.user_name.clone(),
            delivery,
            source_session: ctx.session_id.clone(),
            created_at: Utc::now(),
        };

        info!(
            reminder.id = %id,
            fire_at = %fire_at,
            user = ?ctx.user_name,
            delivery_count = reminder.delivery.len(),
            "reminder scheduled"
        );

        self.store.add(reminder);
        self.scheduler_notify.notify_one();

        Ok(ToolOutput::success(format!(
            "Reminder scheduled (id: {id}) for {fire_at}"
        )))
    }

    fn handle_list(&self, ctx: &ToolContext) -> ToolOutput {
        let user = ctx.user_name.as_deref().unwrap_or("unknown");
        let reminders = self.store.list_for_user(user);

        if reminders.is_empty() {
            return ToolOutput::success("No pending reminders.");
        }

        let mut lines = Vec::new();
        for r in &reminders {
            lines.push(format!("- [{}] {} → \"{}\"", r.id, r.fire_at, r.message));
        }
        ToolOutput::success(lines.join("\n"))
    }

    fn handle_cancel(&self, arguments: &serde_json::Value) -> Result<ToolOutput> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: id"))?;

        if self.store.cancel(id) {
            info!(reminder.id = %id, "reminder cancelled");
            Ok(ToolOutput::success(format!("Reminder {id} cancelled.")))
        } else {
            Ok(ToolOutput::error(format!("Reminder {id} not found.")))
        }
    }
}

#[async_trait]
impl Tool for ReminderTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "reminder",
            "Schedule, list, or cancel one-off reminders. When a reminder fires, \
             you get a fresh session with full tool access to execute the request — \
             so reminders can trigger actions (run commands, call APIs) not just \
             send text.\n\
             \n\
             BEFORE creating an action reminder (anything beyond a simple nudge), \
             confirm you have a concrete execution plan. If the user says 'turn off \
             the lights in 30 min' and you don't know the command, API endpoint, or \
             tool to use — ASK. Say 'I can set that reminder, but I want to make \
             sure I can actually do it — what system controls your lights?' or check \
             your workspace files (TOOLS.md, memory) first. Don't create a reminder \
             you can't confidently execute.\n\
             \n\
             Write the message as self-contained as possible: resolve references \
             ('this', 'that') and include the specific execution plan (commands, \
             API endpoints, file paths). The reminder session can read the \
             originating conversation for additional context, but a complete \
             message means faster, more reliable execution.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["set", "list", "cancel"],
                        "description": "Action to perform"
                    },
                    "time": {
                        "type": "string",
                        "description": "ISO 8601 UTC timestamp for when the reminder should fire (e.g. '2026-02-11T18:00:00Z'). Required for 'set'."
                    },
                    "message": {
                        "type": "string",
                        "description": "What to do when the reminder fires. For nudges: 'Remind the user to call Dr. Smith at 555-0123'. For actions: include the full execution plan. The reminder session has no conversation history (though it can read the source session file), so resolve references and be specific. Required for 'set'."
                    },
                    "id": {
                        "type": "string",
                        "description": "Reminder ID to cancel. Required for 'cancel'."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "reminder tool requires Full or Inner trust level",
            ));
        }

        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: action"))?;

        match action {
            "set" => self.handle_set(&arguments, ctx),
            "list" => Ok(self.handle_list(ctx)),
            "cancel" => self.handle_cancel(&arguments),
            other => Ok(ToolOutput::error(format!("unknown action: {other}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// ReminderToolExecutor
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub(crate) struct ReminderToolExecutor {
    tool: ReminderTool,
}

impl ReminderToolExecutor {
    pub(crate) fn new(
        store: ReminderStore,
        config: SharedConfig,
        scheduler_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            tool: ReminderTool::new(store, config, scheduler_notify),
        }
    }
}

#[async_trait]
impl ToolExecutor for ReminderToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if name == "reminder" {
            self.tool.execute(arguments, ctx).await
        } else {
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.tool.definition()]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_store() -> (ReminderStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(dir.path()).unwrap();
        (store, dir)
    }

    fn sample_reminder(id: &str, hours_from_now: i64, user: &str) -> Reminder {
        Reminder {
            id: id.to_owned(),
            fire_at: Utc::now() + chrono::Duration::hours(hours_from_now),
            message: format!("reminder {id}"),
            user: Some(user.to_owned()),
            delivery: vec![("signal".to_owned(), format!("{user}-uuid"))],
            source_session: "test:dm:signal:alice-uuid".to_owned(),
            created_at: Utc::now(),
        }
    }

    // -- ReminderStore unit tests --

    #[test]
    fn store_add_and_list() {
        let (store, _dir) = make_store();
        store.add(sample_reminder("rem_1", 1, "alice"));
        let list = store.list_for_user("alice");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rem_1");
    }

    #[test]
    fn store_persists_to_disk_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();

        {
            let store = ReminderStore::new(dir.path()).unwrap();
            store.add(sample_reminder("rem_persist", 1, "alice"));
        }

        let store2 = ReminderStore::new(dir.path()).unwrap();
        let list = store2.list_for_user("alice");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rem_persist");
    }

    #[test]
    fn store_cancel_persists_removal_to_disk() {
        let dir = tempfile::tempdir().unwrap();

        {
            let store = ReminderStore::new(dir.path()).unwrap();
            store.add(sample_reminder("rem_1", 1, "alice"));
            store.add(sample_reminder("rem_2", 2, "alice"));
            assert!(store.cancel("rem_1"));
        }

        let store2 = ReminderStore::new(dir.path()).unwrap();
        let list = store2.list_for_user("alice");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rem_2");
    }

    #[test]
    fn store_take_due_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();

        {
            let store = ReminderStore::new(dir.path()).unwrap();
            store.add(Reminder {
                id: "rem_past".to_owned(),
                fire_at: Utc::now() - chrono::Duration::seconds(10),
                message: "overdue".to_owned(),
                user: Some("alice".to_owned()),
                delivery: vec![],
                source_session: "test:main".to_owned(),
                created_at: Utc::now(),
            });
            store.add(sample_reminder("rem_future", 1, "alice"));

            let due = store.take_due();
            assert_eq!(due.len(), 1);
            assert_eq!(due[0].id, "rem_past");
        }

        let store2 = ReminderStore::new(dir.path()).unwrap();
        let list = store2.list_for_user("alice");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rem_future");
    }

    #[test]
    fn store_loads_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(dir.path()).unwrap();
        assert!(store.next_fire_time().is_none());
    }

    #[test]
    fn store_take_due_removes_past_reminders() {
        let (store, _dir) = make_store();
        store.add(Reminder {
            id: "rem_past".to_owned(),
            fire_at: Utc::now() - chrono::Duration::seconds(10),
            message: "overdue".to_owned(),
            user: Some("alice".to_owned()),
            delivery: vec![],
            source_session: "test:main".to_owned(),
            created_at: Utc::now(),
        });
        store.add(sample_reminder("rem_future", 1, "alice"));

        let due = store.take_due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "rem_past");

        let remaining = store.list_for_user("alice");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "rem_future");
    }

    #[test]
    fn store_cancel_removes_by_id() {
        let (store, _dir) = make_store();
        store.add(sample_reminder("rem_1", 1, "alice"));
        assert!(store.cancel("rem_1"));
        assert!(store.list_for_user("alice").is_empty());
    }

    #[test]
    fn store_cancel_returns_false_for_unknown_id() {
        let (store, _dir) = make_store();
        assert!(!store.cancel("rem_nonexistent"));
    }

    #[test]
    fn store_next_fire_time() {
        let (store, _dir) = make_store();
        assert!(store.next_fire_time().is_none());

        let t1 = Utc::now() + chrono::Duration::hours(2);
        let t2 = Utc::now() + chrono::Duration::hours(1);
        store.add(Reminder {
            id: "rem_1".to_owned(),
            fire_at: t1,
            message: "later".to_owned(),
            user: None,
            delivery: vec![],
            source_session: "test:main".to_owned(),
            created_at: Utc::now(),
        });
        store.add(Reminder {
            id: "rem_2".to_owned(),
            fire_at: t2,
            message: "sooner".to_owned(),
            user: None,
            delivery: vec![],
            source_session: "test:main".to_owned(),
            created_at: Utc::now(),
        });

        assert_eq!(store.next_fire_time(), Some(t2));
    }

    #[test]
    fn store_list_filters_by_user() {
        let (store, _dir) = make_store();
        store.add(sample_reminder("rem_alice", 1, "alice"));
        store.add(sample_reminder("rem_bob", 1, "bob"));

        let alice_reminders = store.list_for_user("alice");
        assert_eq!(alice_reminders.len(), 1);
        assert_eq!(alice_reminders[0].id, "rem_alice");
    }

    // -- Delivery resolution tests --

    #[test]
    fn resolve_delivery_uses_originating_signal_channel() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid", "telegram:alice-tg"]
"#,
        )
        .unwrap();

        let targets =
            resolve_reminder_delivery(&config, "test:dm:signal:alice-uuid", Some("alice"));
        assert_eq!(
            targets,
            vec![("signal".to_owned(), "alice-uuid".to_owned())]
        );
    }

    #[test]
    fn resolve_delivery_uses_originating_telegram_channel() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid", "telegram:alice-tg"]
"#,
        )
        .unwrap();

        let targets =
            resolve_reminder_delivery(&config, "test:dm:telegram:alice-tg", Some("alice"));
        assert_eq!(
            targets,
            vec![("telegram".to_owned(), "alice-tg".to_owned())]
        );
    }

    #[test]
    fn resolve_delivery_terminal_falls_back_to_all_channels() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]
"#,
        )
        .unwrap();

        let targets = resolve_reminder_delivery(&config, "test:main", Some("alice"));
        assert_eq!(
            targets,
            vec![("signal".to_owned(), "alice-uuid".to_owned())]
        );
    }

    #[test]
    fn resolve_delivery_empty_for_terminal_only_user() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default"]
"#,
        )
        .unwrap();

        let targets = resolve_reminder_delivery(&config, "test:main", Some("alice"));
        assert!(targets.is_empty());
    }

    #[test]
    fn resolve_delivery_empty_for_unknown_user() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"
"#,
        )
        .unwrap();

        let targets =
            resolve_reminder_delivery(&config, "test:dm:signal:mallory-uuid", Some("mallory"));
        assert_eq!(
            targets,
            vec![("signal".to_owned(), "mallory-uuid".to_owned())]
        );
    }

    #[test]
    fn resolve_delivery_empty_for_none_user_and_main_session() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"
"#,
        )
        .unwrap();

        let targets = resolve_reminder_delivery(&config, "test:main", None);
        assert!(targets.is_empty());
    }

    // -- Tool execution tests --

    fn test_config_with_signal_user() -> Config {
        toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]
"#,
        )
        .unwrap()
    }

    fn test_config_terminal_only() -> Config {
        toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default"]
"#,
        )
        .unwrap()
    }

    fn tool_context_with_user(user: &str) -> ToolContext {
        ToolContext {
            session_id: "test:dm:signal:alice-uuid".to_owned(),
            trust: TrustLevel::Full,
            workspace: PathBuf::from("."),
            user_name: Some(user.to_owned()),
        }
    }

    fn tool_context_with_trust(trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "test:dm:signal:alice-uuid".to_owned(),
            trust,
            workspace: PathBuf::from("."),
            user_name: Some("alice".to_owned()),
        }
    }

    fn make_tool_with_config(config: Config) -> (ReminderTool, ReminderStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(dir.path()).unwrap();
        let shared = crate::config::shared_config(config);
        let notify = Arc::new(tokio::sync::Notify::new());
        let tool = ReminderTool::new(store.clone(), shared, notify);
        (tool, store, dir)
    }

    fn make_tool() -> (ReminderTool, ReminderStore, tempfile::TempDir) {
        make_tool_with_config(test_config_with_signal_user())
    }

    #[tokio::test]
    async fn tool_set_creates_reminder() {
        let (tool, store, _dir) = make_tool();

        let future_time = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

        let output = tool
            .execute(
                serde_json::json!({
                    "action": "set",
                    "time": future_time,
                    "message": "call the dentist"
                }),
                &tool_context_with_user("alice"),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("Reminder scheduled"));

        let reminders = store.list_for_user("alice");
        assert_eq!(reminders.len(), 1);
        assert_eq!(reminders[0].message, "call the dentist");
    }

    #[tokio::test]
    async fn tool_set_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(dir.path()).unwrap();
        let config = crate::config::shared_config(test_config_with_signal_user());
        let notify = Arc::new(tokio::sync::Notify::new());
        let tool = ReminderTool::new(store, config, notify);

        let future_time = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

        tool.execute(
            serde_json::json!({
                "action": "set",
                "time": future_time,
                "message": "persisted reminder"
            }),
            &tool_context_with_user("alice"),
        )
        .await
        .unwrap();

        let store2 = ReminderStore::new(dir.path()).unwrap();
        let list = store2.list_for_user("alice");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message, "persisted reminder");
    }

    #[tokio::test]
    async fn tool_set_rejects_past_time() {
        let (tool, _, _dir) = make_tool();

        let past_time = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();

        let output = tool
            .execute(
                serde_json::json!({
                    "action": "set",
                    "time": past_time,
                    "message": "too late"
                }),
                &tool_context_with_user("alice"),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("future"));
    }

    #[tokio::test]
    async fn tool_set_rejects_no_delivery_channel() {
        let (tool, _, _dir) = make_tool_with_config(test_config_terminal_only());

        let future_time = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

        let output = tool
            .execute(
                serde_json::json!({
                    "action": "set",
                    "time": future_time,
                    "message": "no channel"
                }),
                &ToolContext {
                    session_id: "test:main".to_owned(),
                    trust: TrustLevel::Full,
                    workspace: PathBuf::from("."),
                    user_name: Some("alice".to_owned()),
                },
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("no delivery channel"));
    }

    #[tokio::test]
    async fn tool_list_shows_pending_reminders() {
        let (tool, store, _dir) = make_tool();
        store.add(sample_reminder("rem_1", 1, "alice"));

        let output = tool
            .execute(
                serde_json::json!({"action": "list"}),
                &tool_context_with_user("alice"),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("rem_1"));
    }

    #[tokio::test]
    async fn tool_cancel_removes_reminder() {
        let (tool, store, _dir) = make_tool();
        store.add(sample_reminder("rem_1", 1, "alice"));

        let output = tool
            .execute(
                serde_json::json!({"action": "cancel", "id": "rem_1"}),
                &tool_context_with_user("alice"),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(store.list_for_user("alice").is_empty());
    }

    #[tokio::test]
    async fn tool_rejects_low_trust() {
        let (tool, _, _dir) = make_tool();

        let output = tool
            .execute(
                serde_json::json!({"action": "list"}),
                &tool_context_with_trust(TrustLevel::Familiar),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("trust"));
    }
}
