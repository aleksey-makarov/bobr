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

#[test]
fn serde_json_uses_bare_lowercase_hex() {
    let value = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let hash = ObjectHash::from_str(value).unwrap();

    assert_eq!(
        serde_json::to_string(&hash).unwrap(),
        format!("\"{value}\"")
    );
    assert_eq!(
        serde_json::from_str::<ObjectHash>(&format!("\"{value}\"")).unwrap(),
        hash
    );
}
