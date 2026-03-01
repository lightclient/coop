use std::collections::HashMap;

/// Resolved identity for a Signal user.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedIdentity {
    pub(crate) display_name: String,
    pub(crate) coop_name: Option<String>,
    pub(crate) is_self: bool,
}

/// Maps Signal ACIs to human-readable names and coop identities.
///
/// Built once at startup from config data and the presage contact store.
/// Immutable after construction — safe to share via `Arc`.
#[derive(Debug, Clone)]
pub(crate) struct SignalNameResolver {
    #[allow(dead_code)] // used in tests + future public API
    self_aci: String,
    identities: HashMap<String, ResolvedIdentity>,
    /// ACI → disambiguation suffix for colliding display names.
    suffixes: HashMap<String, String>,
}

impl SignalNameResolver {
    /// Build a resolver from config data and contacts.
    ///
    /// - `self_aci`: agent's own Signal ACI
    /// - `agent_name`: agent's display name (typically `agent.id`)
    /// - `coop_users`: `(signal_aci, coop_name)` from `[[users]]` match patterns
    /// - `contacts`: `(signal_aci, contact_name)` from the presage contact store
    pub(crate) fn build(
        self_aci: String,
        agent_name: String,
        coop_users: &[(String, String)],
        contacts: &[(String, String)],
    ) -> Self {
        let mut identities = HashMap::new();

        // 1. Agent's own entry
        identities.insert(
            self_aci.clone(),
            ResolvedIdentity {
                display_name: agent_name,
                coop_name: None,
                is_self: true,
            },
        );

        // 2. Presage contacts — provides Signal-visible display names.
        // Lowest priority: coop config can override the display name.
        for (aci, name) in contacts {
            if *aci == self_aci {
                continue;
            }
            identities
                .entry(aci.clone())
                .or_insert_with(|| ResolvedIdentity {
                    display_name: name.clone(),
                    coop_name: None,
                    is_self: false,
                });
        }

        // 3. Coop users — overlay coop_name. If no contact exists for this
        // ACI, the coop name becomes the display name too.
        for (aci, coop_name) in coop_users {
            if *aci == self_aci {
                continue;
            }
            let entry = identities
                .entry(aci.clone())
                .or_insert_with(|| ResolvedIdentity {
                    display_name: coop_name.clone(),
                    coop_name: None,
                    is_self: false,
                });
            entry.coop_name = Some(coop_name.clone());
        }

        let suffixes = compute_disambiguation(&identities);

        Self {
            self_aci,
            identities,
            suffixes,
        }
    }

    #[allow(dead_code)] // used in tests + future public API
    pub(crate) fn resolve(&self, aci: &str) -> Option<&ResolvedIdentity> {
        self.identities.get(aci)
    }

    /// Format a mention: `@DisplayName` or `@DisplayName#xxxx` for collisions.
    pub(crate) fn mention_text(&self, aci: &str) -> String {
        let name = self.display_name(aci);
        if let Some(suffix) = self.suffixes.get(aci) {
            format!("@{name}#{suffix}")
        } else {
            format!("@{name}")
        }
    }

    /// Format a sender header for `[from ...]` context.
    ///
    /// Returns `DisplayName (self)` for the agent, `DisplayName (user:coop_name)`
    /// for coop users, or just `DisplayName` for others.
    pub(crate) fn sender_header(&self, aci: &str) -> String {
        match self.identities.get(aci) {
            Some(id) if id.is_self => format!("{} (self)", id.display_name),
            Some(ResolvedIdentity {
                display_name,
                coop_name: Some(coop_name),
                ..
            }) => format!("{display_name} (user:{coop_name})"),
            Some(id) => id.display_name.clone(),
            None => "unknown".to_owned(),
        }
    }

    pub(crate) fn display_name(&self, aci: &str) -> &str {
        self.identities
            .get(aci)
            .map_or("unknown", |id| &id.display_name)
    }

    #[allow(dead_code)] // used in tests
    pub(crate) fn is_self(&self, aci: &str) -> bool {
        aci == self.self_aci
    }

    #[allow(dead_code)] // used in tests
    pub(crate) fn self_display_name(&self) -> &str {
        self.identities
            .get(&self.self_aci)
            .map_or("unknown", |id| &id.display_name)
    }

    #[allow(dead_code)] // used in tests
    pub(crate) fn self_aci(&self) -> &str {
        &self.self_aci
    }
}

/// Detect display name collisions and assign `#xxxx` suffixes.
///
/// Rules: in a collision group, coop users keep the clean name (they have
/// the `(user:...)` annotation for disambiguation). Self keeps clean name.
/// Everyone else gets a suffix from the first 4 hex chars of their ACI.
/// If multiple coop users collide, all of them get suffixes too.
fn compute_disambiguation(
    identities: &HashMap<String, ResolvedIdentity>,
) -> HashMap<String, String> {
    let mut by_name: HashMap<String, Vec<&str>> = HashMap::new();
    for (aci, id) in identities {
        by_name
            .entry(id.display_name.to_lowercase())
            .or_default()
            .push(aci.as_str());
    }

    let mut suffixes = HashMap::new();
    for acis in by_name.values() {
        if acis.len() <= 1 {
            continue;
        }

        let coop_count = acis
            .iter()
            .filter(|aci| {
                identities[**aci]
                    .coop_name
                    .as_ref()
                    .is_some_and(|_| !identities[**aci].is_self)
            })
            .count();

        for &aci in acis {
            let id = &identities[aci];
            if id.is_self {
                continue;
            }
            if id.coop_name.is_some() && coop_count == 1 {
                continue;
            }
            let end = aci.find('-').unwrap_or(aci.len()).min(4);
            suffixes.insert(aci.to_owned(), aci[..end].to_owned());
        }
    }

    suffixes
}

/// Sanitize outbound text by replacing UUID-shaped patterns with `[redacted-id]`.
///
/// Returns the sanitized text and the number of UUIDs redacted.
/// Callers should log a warning when the redaction count is > 0.
pub(crate) fn sanitize_uuids(text: &str) -> (String, usize) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 36 {
        return (text.to_owned(), 0);
    }

    let mut result = Vec::with_capacity(len);
    let mut i = 0;
    let mut redacted = 0;

    while i < len {
        if i + 36 <= len && is_uuid_at(bytes, i) {
            result.extend_from_slice(b"[redacted-id]");
            i += 36;
            redacted += 1;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    if redacted > 0 {
        // Safe: we only replaced ASCII UUID chars with ASCII replacement text.
        (
            String::from_utf8(result).unwrap_or_else(|_| text.to_owned()),
            redacted,
        )
    } else {
        (text.to_owned(), 0)
    }
}

/// Check if bytes at `start` match UUID v4 format: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
fn is_uuid_at(bytes: &[u8], start: usize) -> bool {
    const DASH_POSITIONS: [usize; 4] = [8, 13, 18, 23];
    for i in 0..36 {
        let b = bytes[start + i];
        if DASH_POSITIONS.contains(&i) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn agent_aci() -> String {
        "eedf560a-1201-4cde-a863-4e5f82142ebf".to_owned()
    }

    fn alice_aci() -> String {
        "80d43956-a7cb-40f9-8d7b-901f752d17db".to_owned()
    }

    fn bob_aci() -> String {
        "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_owned()
    }

    fn unknown_aci() -> String {
        "99999999-9999-9999-9999-999999999999".to_owned()
    }

    fn basic_resolver() -> SignalNameResolver {
        SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[(alice_aci(), "alice".to_owned())],
            &[(alice_aci(), "Alice Walker".to_owned())],
        )
    }

    #[test]
    fn self_identity() {
        let r = basic_resolver();
        assert!(r.is_self(&agent_aci()));
        assert!(!r.is_self(&alice_aci()));
        assert_eq!(r.self_display_name(), "reid");
        assert_eq!(r.self_aci(), agent_aci());
    }

    #[test]
    fn coop_user_with_contact_uses_contact_name() {
        let r = basic_resolver();
        let id = r.resolve(&alice_aci()).unwrap();
        assert_eq!(id.display_name, "Alice Walker");
        assert_eq!(id.coop_name.as_deref(), Some("alice"));
    }

    #[test]
    fn coop_user_without_contact_falls_back_to_coop_name() {
        let r = SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[(bob_aci(), "bob".to_owned())],
            &[],
        );
        let id = r.resolve(&bob_aci()).unwrap();
        assert_eq!(id.display_name, "bob");
        assert_eq!(id.coop_name.as_deref(), Some("bob"));
    }

    #[test]
    fn unknown_aci_returns_none() {
        let r = basic_resolver();
        assert!(r.resolve(&unknown_aci()).is_none());
        assert_eq!(r.display_name(&unknown_aci()), "unknown");
    }

    #[test]
    fn mention_text_formatting() {
        let r = basic_resolver();
        assert_eq!(r.mention_text(&agent_aci()), "@reid");
        assert_eq!(r.mention_text(&alice_aci()), "@Alice Walker");
        assert_eq!(r.mention_text(&unknown_aci()), "@unknown");
    }

    #[test]
    fn sender_header_formatting() {
        let r = basic_resolver();
        assert_eq!(r.sender_header(&agent_aci()), "reid (self)");
        assert_eq!(r.sender_header(&alice_aci()), "Alice Walker (user:alice)");
        assert_eq!(r.sender_header(&unknown_aci()), "unknown");
    }

    #[test]
    fn sender_header_contact_without_coop() {
        let r = SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[],
            &[(bob_aci(), "Robert Chen".to_owned())],
        );
        assert_eq!(r.sender_header(&bob_aci()), "Robert Chen");
    }

    #[test]
    fn disambiguation_on_name_collision() {
        // Two non-coop contacts named "Matt"
        let matt1 = "11111111-aaaa-bbbb-cccc-dddddddddddd".to_owned();
        let matt2 = "22222222-aaaa-bbbb-cccc-dddddddddddd".to_owned();
        let r = SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[],
            &[
                (matt1.clone(), "Matt".to_owned()),
                (matt2.clone(), "Matt".to_owned()),
            ],
        );

        let m1 = r.mention_text(&matt1);
        let m2 = r.mention_text(&matt2);
        assert_ne!(m1, m2);
        assert!(m1.starts_with("@Matt#"));
        assert!(m2.starts_with("@Matt#"));
    }

    #[test]
    fn disambiguation_coop_user_keeps_clean_name() {
        let matt_coop = "11111111-aaaa-bbbb-cccc-dddddddddddd".to_owned();
        let matt_other = "22222222-aaaa-bbbb-cccc-dddddddddddd".to_owned();
        let r = SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[(matt_coop.clone(), "matt".to_owned())],
            &[
                (matt_coop.clone(), "Matt".to_owned()),
                (matt_other.clone(), "Matt".to_owned()),
            ],
        );

        // Coop user keeps clean name
        assert_eq!(r.mention_text(&matt_coop), "@Matt");
        // Non-coop gets suffix
        assert!(r.mention_text(&matt_other).starts_with("@Matt#"));
    }

    #[test]
    fn self_keeps_clean_name_in_collision() {
        let r = SignalNameResolver::build(
            agent_aci(),
            "reid".to_owned(),
            &[],
            &[(bob_aci(), "reid".to_owned())],
        );
        assert_eq!(r.mention_text(&agent_aci()), "@reid");
        assert!(r.mention_text(&bob_aci()).starts_with("@reid#"));
    }

    #[test]
    fn sanitize_uuids_replaces_uuids() {
        let text = "user eedf560a-1201-4cde-a863-4e5f82142ebf said hi";
        let (result, count) = sanitize_uuids(text);
        assert_eq!(result, "user [redacted-id] said hi");
        assert_eq!(count, 1);
    }

    #[test]
    fn sanitize_uuids_replaces_multiple() {
        let text =
            "from eedf560a-1201-4cde-a863-4e5f82142ebf to 80d43956-a7cb-40f9-8d7b-901f752d17db";
        let (result, count) = sanitize_uuids(text);
        assert_eq!(result, "from [redacted-id] to [redacted-id]");
        assert_eq!(count, 2);
    }

    #[test]
    fn sanitize_uuids_preserves_non_uuid_text() {
        let text = "hello world, no UUIDs here! 12345-abcde";
        let (result, count) = sanitize_uuids(text);
        assert_eq!(result, text);
        assert_eq!(count, 0);
    }

    #[test]
    fn sanitize_uuids_handles_short_text() {
        let (result, count) = sanitize_uuids("hi");
        assert_eq!(result, "hi");
        assert_eq!(count, 0);
    }

    #[test]
    fn sanitize_uuids_preserves_non_ascii() {
        let text = "héllo eedf560a-1201-4cde-a863-4e5f82142ebf wörld";
        let (result, count) = sanitize_uuids(text);
        assert_eq!(result, "héllo [redacted-id] wörld");
        assert_eq!(count, 1);
    }
}
