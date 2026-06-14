use std::fmt;

/// Error returned when a public store publication name is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationNameError {
    message: String,
}

impl PublicationNameError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the user-facing validation message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PublicationNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicationNameError {}

/// Validates a store publication name.
///
/// Publication names are used as filenames under public store ref
/// directories, so they must be non-empty, must not be `.` or `..`, and may
/// contain only ASCII letters, digits, `.`, `_`, or `-`.
pub fn validate_publication_name(name: &str) -> Result<(), PublicationNameError> {
    if name.is_empty() {
        return Err(PublicationNameError::new(
            "publication name must not be empty",
        ));
    }
    if name == "." || name == ".." {
        return Err(PublicationNameError::new(format!(
            "invalid publication name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(PublicationNameError::new(format!(
            "invalid publication name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_names_accept_store_safe_ascii_names() {
        for name in ["name", "name-1", "name_1", "name.1"] {
            validate_publication_name(name).unwrap();
        }
    }

    #[test]
    fn publication_names_reject_invalid_names() {
        for name in ["", ".", "..", "bad/name", "bad name", "юникод"] {
            let error = validate_publication_name(name).unwrap_err();
            assert!(!error.to_string().is_empty());
        }
    }
}
