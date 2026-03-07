use coop_core::SessionKey;
use std::collections::{HashMap, VecDeque};

pub(crate) struct GroupHistoryEntry {
    pub body: String,
}

pub(crate) struct GroupHistoryBuffer {
    buffers: HashMap<SessionKey, VecDeque<GroupHistoryEntry>>,
}

impl GroupHistoryBuffer {
    pub(crate) fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    pub(crate) fn record(&mut self, key: &SessionKey, entry: GroupHistoryEntry, limit: usize) {
        if limit == 0 {
            return;
        }
        let buf = self.buffers.entry(key.clone()).or_default();
        if buf.len() >= limit {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    pub(crate) fn peek_context(&self, key: &SessionKey) -> Option<String> {
        let buf = self.buffers.get(key)?;
        if buf.is_empty() {
            return None;
        }
        Some(format_history(buf))
    }

    pub(crate) fn drain_context(&mut self, key: &SessionKey) -> Option<String> {
        let buf = self.buffers.get_mut(key)?;
        if buf.is_empty() {
            return None;
        }
        let result = format_history(buf);
        buf.clear();
        Some(result)
    }

    pub(crate) fn clear(&mut self, key: &SessionKey) {
        self.buffers.remove(key);
    }
}

fn format_history(buf: &VecDeque<GroupHistoryEntry>) -> String {
    let mut lines = vec!["[Chat messages since your last reply — for context]".to_owned()];
    for entry in buf {
        lines.push(entry.body.clone());
    }
    lines.push("[Current message — respond to this]".to_owned());
    lines.join("\n")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{SessionKey, SessionKind};

    fn session_key(group: &str) -> SessionKey {
        SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Group(group.to_owned()),
        }
    }

    fn entry(_sender: &str, body: &str, _ts: u64) -> GroupHistoryEntry {
        GroupHistoryEntry {
            body: body.to_owned(),
        }
    }

    #[test]
    fn record_and_drain_returns_formatted() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        buf.record(&key, entry("Alice", "[from Alice at 100]\nhello", 100), 50);
        buf.record(&key, entry("Bob", "[from Bob at 101]\nhi there", 101), 50);

        let ctx = buf.drain_context(&key).unwrap();
        assert!(ctx.contains("[Chat messages since your last reply"));
        assert!(ctx.contains("[from Alice at 100]\nhello"));
        assert!(ctx.contains("[from Bob at 101]\nhi there"));
        assert!(ctx.contains("[Current message — respond to this]"));
    }

    #[test]
    fn peek_returns_without_consuming() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        buf.record(&key, entry("Alice", "hello", 100), 50);

        let ctx1 = buf.peek_context(&key).unwrap();
        let ctx2 = buf.peek_context(&key).unwrap();
        assert_eq!(ctx1, ctx2);
    }

    #[test]
    fn drain_clears_buffer() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        buf.record(&key, entry("Alice", "hello", 100), 50);

        assert!(buf.drain_context(&key).is_some());
        assert!(buf.drain_context(&key).is_none());
    }

    #[test]
    fn clear_discards_buffered_history() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        buf.record(&key, entry("Alice", "hello", 100), 50);

        buf.clear(&key);

        assert!(buf.peek_context(&key).is_none());
    }

    #[test]
    fn history_respects_limit() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        for i in 0..5 {
            buf.record(&key, entry("Alice", &format!("msg {i}"), i), 3);
        }
        let ctx = buf.peek_context(&key).unwrap();
        assert!(!ctx.contains("msg 0"));
        assert!(!ctx.contains("msg 1"));
        assert!(ctx.contains("msg 2"));
        assert!(ctx.contains("msg 3"));
        assert!(ctx.contains("msg 4"));
    }

    #[test]
    fn empty_buffer_returns_none() {
        let buf = GroupHistoryBuffer::new();
        assert!(
            buf.peek_context(&session_key("signal:group:dead"))
                .is_none()
        );
    }

    #[test]
    fn multiple_sessions_independent() {
        let mut buf = GroupHistoryBuffer::new();
        let key1 = session_key("signal:group:aaaa");
        let key2 = session_key("signal:group:bbbb");
        buf.record(&key1, entry("Alice", "session1", 1), 50);
        buf.record(&key2, entry("Bob", "session2", 2), 50);

        let c1 = buf.peek_context(&key1).unwrap();
        let c2 = buf.peek_context(&key2).unwrap();
        assert!(c1.contains("session1"));
        assert!(!c1.contains("session2"));
        assert!(c2.contains("session2"));
        assert!(!c2.contains("session1"));
    }
}
