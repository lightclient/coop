use coop_core::{SessionKey, TrustLevel};
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

    #[allow(dead_code)]
    pub(crate) fn clear(&mut self, key: &SessionKey) {
        self.buffers.remove(key);
    }
}

fn format_history(buf: &VecDeque<GroupHistoryEntry>) -> String {
    // The entry.body already contains `[from DisplayName ... at timestamp]`
    // from the inbound parser, so we don't wrap it again.
    let mut lines = vec!["[Chat messages since your last reply — for context]".to_owned()];
    for entry in buf {
        lines.push(entry.body.clone());
    }
    lines.push("[Current message — respond to this]".to_owned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Group ceiling cache for min_member mode
// ---------------------------------------------------------------------------

/// Wired up when Signal `GroupMembers` query lands — suppress until then.
#[allow(dead_code)]
pub(crate) struct GroupCeilingCache {
    cache: HashMap<SessionKey, (u32, TrustLevel)>,
}

impl GroupCeilingCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get(&self, key: &SessionKey, revision: u32) -> Option<TrustLevel> {
        self.cache
            .get(key)
            .filter(|(cached_rev, _)| *cached_rev == revision)
            .map(|(_, ceiling)| *ceiling)
    }

    #[allow(dead_code)]
    pub(crate) fn set(&mut self, key: SessionKey, revision: u32, ceiling: TrustLevel) {
        self.cache.insert(key, (revision, ceiling));
    }
}

/// Compute the min_member trust ceiling by cross-referencing group
/// members with `[[users]]` config. Members not in config get `default_trust`.
#[allow(dead_code)]
pub(crate) fn compute_min_member_ceiling(
    member_uuids: &[String],
    users: &[crate::config::UserConfig],
    default_trust: TrustLevel,
) -> TrustLevel {
    member_uuids
        .iter()
        .map(|uuid| {
            let signal_id = format!("signal:{uuid}");
            users
                .iter()
                .find(|u| u.r#match.iter().any(|p| p == &signal_id || p == uuid))
                .map_or(default_trust, |u| u.trust)
        })
        .max() // max in Ord = least privileged = most restrictive
        .unwrap_or(default_trust)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{SessionKey, SessionKind, TrustLevel};

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
    fn history_respects_limit() {
        let mut buf = GroupHistoryBuffer::new();
        let key = session_key("signal:group:dead");
        for i in 0..5 {
            buf.record(&key, entry("Alice", &format!("msg {i}"), i), 3);
        }
        let ctx = buf.peek_context(&key).unwrap();
        // Limit 3: only last 3 messages
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

    #[test]
    fn ceiling_cache_returns_none_for_uncached() {
        let cache = GroupCeilingCache::new();
        assert!(cache.get(&session_key("g"), 0).is_none());
    }

    #[test]
    fn ceiling_cache_returns_none_on_revision_mismatch() {
        let mut cache = GroupCeilingCache::new();
        let key = session_key("g");
        cache.set(key.clone(), 1, TrustLevel::Familiar);
        assert!(cache.get(&key, 2).is_none());
    }

    #[test]
    fn ceiling_cache_returns_hit_on_match() {
        let mut cache = GroupCeilingCache::new();
        let key = session_key("g");
        cache.set(key.clone(), 5, TrustLevel::Inner);
        assert_eq!(cache.get(&key, 5), Some(TrustLevel::Inner));
    }

    #[test]
    fn ceiling_cache_overwrites_on_new_revision() {
        let mut cache = GroupCeilingCache::new();
        let key = session_key("g");
        cache.set(key.clone(), 1, TrustLevel::Familiar);
        cache.set(key.clone(), 2, TrustLevel::Full);
        assert!(cache.get(&key, 1).is_none());
        assert_eq!(cache.get(&key, 2), Some(TrustLevel::Full));
    }

    fn user(name: &str, trust: TrustLevel, patterns: &[&str]) -> crate::config::UserConfig {
        crate::config::UserConfig {
            name: name.to_owned(),
            trust,
            r#match: patterns.iter().map(|s| (*s).to_owned()).collect(),
            sandbox: None,
        }
    }

    #[test]
    fn min_member_returns_least_privileged() {
        let users = vec![
            user("alice", TrustLevel::Full, &["signal:alice-uuid"]),
            user("bob", TrustLevel::Inner, &["signal:bob-uuid"]),
        ];
        let members = vec!["alice-uuid".to_owned(), "bob-uuid".to_owned()];
        let ceiling = compute_min_member_ceiling(&members, &users, TrustLevel::Familiar);
        // Inner > Full in ordering (less privileged), so ceiling = Inner
        assert_eq!(ceiling, TrustLevel::Inner);
    }

    #[test]
    fn min_member_uses_default_for_unknown() {
        let users = vec![user("alice", TrustLevel::Full, &["signal:alice-uuid"])];
        let members = vec!["alice-uuid".to_owned(), "unknown-uuid".to_owned()];
        let ceiling = compute_min_member_ceiling(&members, &users, TrustLevel::Familiar);
        // unknown → Familiar, Alice → Full; max = Familiar
        assert_eq!(ceiling, TrustLevel::Familiar);
    }

    #[test]
    fn min_member_all_known_full() {
        let users = vec![
            user("alice", TrustLevel::Full, &["signal:alice-uuid"]),
            user("bob", TrustLevel::Full, &["signal:bob-uuid"]),
        ];
        let members = vec!["alice-uuid".to_owned(), "bob-uuid".to_owned()];
        let ceiling = compute_min_member_ceiling(&members, &users, TrustLevel::Familiar);
        assert_eq!(ceiling, TrustLevel::Full);
    }
}
