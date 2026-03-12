use fsobj_hash::ObjectHash;
use std::str::FromStr;

#[test]
fn display_uses_prefixed_lowercase_hex() {
    let hash = ObjectHash::from_str(
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    assert_eq!(
        hash.to_string(),
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn parse_rejects_missing_prefix() {
    assert!(ObjectHash::from_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_err());
}

#[test]
fn parse_rejects_wrong_length() {
    assert!(ObjectHash::from_str("sha256:abcd").is_err());
}

#[test]
fn parse_rejects_uppercase_hex() {
    assert!(ObjectHash::from_str(
        "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
    )
    .is_err());
}
