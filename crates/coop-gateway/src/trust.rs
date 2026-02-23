use coop_core::TrustLevel;

/// Resolve effective trust: the *least* privileged of user trust and situation ceiling.
///
/// TrustLevel ordering: Owner < Full < Inner < Familiar < Public (most trusted is "smallest").
/// We pick the max in our Ord, which is the least trusted of the two.
pub(crate) fn resolve_trust(user_trust: TrustLevel, situation_ceiling: TrustLevel) -> TrustLevel {
    std::cmp::max(user_trust, situation_ceiling)
}

/// Memory stores accessible at a given trust level.
#[allow(dead_code)]
pub(crate) fn accessible_stores(trust: TrustLevel) -> Vec<&'static str> {
    match trust {
        TrustLevel::Owner | TrustLevel::Full => vec!["private", "shared", "social"],
        TrustLevel::Inner => vec!["shared", "social"],
        TrustLevel::Familiar => vec!["social"],
        TrustLevel::Public => vec![],
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_in_dm_gets_owner() {
        // DM ceiling is Owner (most permissive non-group context)
        assert_eq!(
            resolve_trust(TrustLevel::Owner, TrustLevel::Owner),
            TrustLevel::Owner
        );
    }

    #[test]
    fn full_user_still_full_with_owner_ceiling() {
        assert_eq!(
            resolve_trust(TrustLevel::Full, TrustLevel::Owner),
            TrustLevel::Full
        );
    }

    #[test]
    fn owner_in_group_gets_familiar() {
        assert_eq!(
            resolve_trust(TrustLevel::Owner, TrustLevel::Familiar),
            TrustLevel::Familiar
        );
    }

    #[test]
    fn accessible_stores_owner_same_as_full() {
        assert_eq!(
            accessible_stores(TrustLevel::Owner),
            accessible_stores(TrustLevel::Full)
        );
    }

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
