use crate::StoreError;

/// Validates a store ref name.
///
/// Ref names are used as filenames under store ref directories, so they must be
/// non-empty, must not be `.` or `..`, and may contain only ASCII letters,
/// digits, `.`, `_`, or `-`.
pub fn validate_ref_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::InvalidInput(
            "ref name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(StoreError::InvalidInput(format!(
            "invalid ref name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidInput(format!(
            "invalid ref name '{name}'; allowed chars: [A-Za-z0-9._-]"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names_accept_store_safe_ascii_names() {
        for name in ["name", "name-1", "name_1", "name.1"] {
            validate_ref_name(name).unwrap();
        }
    }

    #[test]
    fn ref_names_reject_invalid_names() {
        for name in ["", ".", "..", "bad/name", "bad name", "юникод"] {
            let error = validate_ref_name(name).unwrap_err();
            assert!(!error.to_string().is_empty());
        }
    }
}
