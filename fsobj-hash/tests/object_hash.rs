use fsobj_hash::ObjectHash;
use std::str::FromStr;

#[test]
fn display_uses_bare_lowercase_hex() {
    let hash =
        ObjectHash::from_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
    assert_eq!(
        hash.to_string(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn parse_accepts_bare_hex() {
    assert!(
        ObjectHash::from_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .is_ok()
    );
}

#[test]
fn parse_rejects_wrong_length() {
    assert!(ObjectHash::from_str("abcd").is_err());
}

#[test]
fn parse_rejects_uppercase_hex() {
    assert!(
        ObjectHash::from_str("0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef")
            .is_err()
    );
}
