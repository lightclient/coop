use coop_core::TrustLevel;

/// Resolve effective trust: always the minimum of user trust and situation ceiling.
/// TrustLevel ordering: Full < Inner < Familiar < Public (Full is most trusted, "smallest").
/// We want the *least* privileged of the two, which is the *max* in our Ord.
pub(crate) fn resolve_trust(user_trust: TrustLevel, situation_ceiling: TrustLevel) -> TrustLevel {
    std::cmp::max(user_trust, situation_ceiling)
}

/// Memory stores accessible at a given trust level.
#[allow(dead_code)]
pub(crate) fn accessible_stores(trust: TrustLevel) -> Vec<&'static str> {
    match trust {
        TrustLevel::Full => vec!["private", "shared", "social"],
        TrustLevel::Inner => vec!["shared", "social"],
        TrustLevel::Familiar => vec!["social"],
        TrustLevel::Public => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_user_in_dm_gets_full() {
        assert_eq!(
            resolve_trust(TrustLevel::Full, TrustLevel::Full),
            TrustLevel::Full
        );
    }

    #[test]
    fn full_user_in_group_gets_familiar() {
        assert_eq!(
            resolve_trust(TrustLevel::Full, TrustLevel::Familiar),
            TrustLevel::Familiar
        );
    }

    #[test]
    fn inner_user_in_dm_gets_inner() {
        assert_eq!(
            resolve_trust(TrustLevel::Inner, TrustLevel::Full),
            TrustLevel::Inner
        );
    }

    #[test]
    fn inner_user_in_group_gets_familiar() {
        assert_eq!(
            resolve_trust(TrustLevel::Inner, TrustLevel::Familiar),
            TrustLevel::Familiar
        );
    }

    #[test]
    fn public_user_always_public() {
        assert_eq!(
            resolve_trust(TrustLevel::Public, TrustLevel::Full),
            TrustLevel::Public
        );
    }

    #[test]
    fn situation_can_only_lower_trust() {
        assert_eq!(
            resolve_trust(TrustLevel::Inner, TrustLevel::Full),
            TrustLevel::Inner
        );
    }

    #[test]
    fn accessible_stores_full() {
        assert_eq!(
            accessible_stores(TrustLevel::Full),
            vec!["private", "shared", "social"]
        );
    }

    #[test]
    fn accessible_stores_public_is_empty() {
        assert!(accessible_stores(TrustLevel::Public).is_empty());
    }
}
