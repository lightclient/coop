use super::SignalTarget;

#[test]
fn parse_direct_target() {
    let target = SignalTarget::parse("alice-uuid").unwrap();
    assert_eq!(target, SignalTarget::Direct("alice-uuid".to_string()));
}

#[test]
fn parse_prefixed_direct_target() {
    let target = SignalTarget::parse("signal:alice-uuid").unwrap();
    assert_eq!(target, SignalTarget::Direct("alice-uuid".to_string()));
}

#[test]
fn parse_group_target() {
    let target = SignalTarget::parse(
        "group:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    )
    .unwrap();
    assert_eq!(
        target,
        SignalTarget::Group {
            master_key: hex::decode(
                "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
            )
            .unwrap(),
        }
    );
}

#[test]
fn reject_invalid_group_key() {
    assert!(SignalTarget::parse("group:not-hex").is_err());
}

#[test]
fn reject_wrong_group_key_size() {
    assert!(SignalTarget::parse("group:deadbeef").is_err());
}
