//! Compile-time identity of a `bobr` executable.

use std::fmt;

use serde::Serialize;

const GIT_COMMIT: Option<&str> = option_env!("BOBR_BUILD_GIT_COMMIT");
const GIT_DIRTY: Option<&str> = option_env!("BOBR_BUILD_GIT_DIRTY");

/// Git source state embedded in a build by a trusted build entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GitProvenance {
    /// Full object id of the checkout's `HEAD` commit.
    pub git_commit: &'static str,

    /// Whether tracked or untracked source-tree changes were present.
    pub git_dirty: bool,
}

/// Machine-readable identity and compatibility information for this build.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BuildInfo {
    /// Bobr workspace version.
    pub version: &'static str,

    /// Request schema accepted by this build.
    pub request_schema: &'static str,

    /// Git source state, or `None` for an ordinary unannotated Cargo build.
    pub provenance: Option<GitProvenance>,
}

/// Invalid compile-time build provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildInfoError(String);

impl BuildInfo {
    /// Returns the identity embedded in the current executable.
    pub fn current() -> Result<Self, BuildInfoError> {
        Self::from_compile_metadata(GIT_COMMIT, GIT_DIRTY)
    }

    fn from_compile_metadata(
        git_commit: Option<&'static str>,
        git_dirty: Option<&'static str>,
    ) -> Result<Self, BuildInfoError> {
        let provenance = match (git_commit, git_dirty) {
            (None, None) => None,
            (Some(git_commit), Some(git_dirty)) => {
                validate_git_commit(git_commit)?;
                let git_dirty = match git_dirty {
                    "false" => false,
                    "true" => true,
                    value => {
                        return Err(BuildInfoError(format!(
                            "BOBR_BUILD_GIT_DIRTY must be 'true' or 'false', got '{value}'"
                        )));
                    }
                };
                Some(GitProvenance {
                    git_commit,
                    git_dirty,
                })
            }
            _ => {
                return Err(BuildInfoError(
                    "BOBR_BUILD_GIT_COMMIT and BOBR_BUILD_GIT_DIRTY must be set together"
                        .to_string(),
                ));
            }
        };

        Ok(Self {
            version: env!("CARGO_PKG_VERSION"),
            request_schema: crate::REQUEST_SCHEMA,
            provenance,
        })
    }
}

impl fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bobr {} (request {}) ",
            self.version, self.request_schema
        )?;
        match self.provenance {
            Some(provenance) => {
                write!(f, "({}", provenance.git_commit)?;
                if provenance.git_dirty {
                    f.write_str("-dirty")?;
                }
                f.write_str(")")
            }
            None => f.write_str("(provenance unknown)"),
        }
    }
}

impl fmt::Display for BuildInfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildInfoError {}

fn validate_git_commit(value: &str) -> Result<(), BuildInfoError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(BuildInfoError(format!(
        "BOBR_BUILD_GIT_COMMIT must be a full lowercase hexadecimal Git object id, got '{value}'"
    )))
}

#[cfg(test)]
mod tests {
    use super::{BuildInfo, GitProvenance};

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn accepts_unknown_provenance() {
        let info = BuildInfo::from_compile_metadata(None, None).unwrap();
        assert_eq!(info.provenance, None);
    }

    #[test]
    fn accepts_clean_and_dirty_provenance() {
        for (value, expected) in [("false", false), ("true", true)] {
            let info = BuildInfo::from_compile_metadata(Some(COMMIT), Some(value)).unwrap();
            assert_eq!(
                info.provenance,
                Some(GitProvenance {
                    git_commit: COMMIT,
                    git_dirty: expected,
                })
            );
        }
    }

    #[test]
    fn formats_human_readable_identity() {
        let unknown = BuildInfo::from_compile_metadata(None, None).unwrap();
        assert_eq!(
            unknown.to_string(),
            format!(
                "bobr {} (request {}) (provenance unknown)",
                env!("CARGO_PKG_VERSION"),
                crate::REQUEST_SCHEMA
            )
        );

        let clean = BuildInfo::from_compile_metadata(Some(COMMIT), Some("false")).unwrap();
        assert_eq!(
            clean.to_string(),
            format!(
                "bobr {} (request {}) ({COMMIT})",
                env!("CARGO_PKG_VERSION"),
                crate::REQUEST_SCHEMA
            )
        );

        let dirty = BuildInfo::from_compile_metadata(Some(COMMIT), Some("true")).unwrap();
        assert_eq!(
            dirty.to_string(),
            format!(
                "bobr {} (request {}) ({COMMIT}-dirty)",
                env!("CARGO_PKG_VERSION"),
                crate::REQUEST_SCHEMA
            )
        );
    }

    #[test]
    fn rejects_partial_provenance() {
        for result in [
            BuildInfo::from_compile_metadata(Some(COMMIT), None),
            BuildInfo::from_compile_metadata(None, Some("false")),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must be set together")
            );
        }
    }

    #[test]
    fn rejects_invalid_dirty_marker() {
        let error = BuildInfo::from_compile_metadata(Some(COMMIT), Some("0")).unwrap_err();
        assert!(error.to_string().contains("must be 'true' or 'false'"));
    }

    #[test]
    fn rejects_invalid_commit() {
        for commit in [
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456g",
            "0123456789ABCDEF0123456789ABCDEF01234567",
        ] {
            let error = BuildInfo::from_compile_metadata(Some(commit), Some("false")).unwrap_err();
            assert!(error.to_string().contains("Git object id"));
        }
    }

    #[test]
    fn serializes_stable_compact_shape() {
        let unknown = BuildInfo::from_compile_metadata(None, None).unwrap();
        assert_eq!(
            serde_json::to_string(&unknown).unwrap(),
            format!(
                r#"{{"version":"{}","request_schema":"{}","provenance":null}}"#,
                env!("CARGO_PKG_VERSION"),
                crate::REQUEST_SCHEMA
            )
        );

        let clean = BuildInfo::from_compile_metadata(Some(COMMIT), Some("false")).unwrap();
        assert_eq!(
            serde_json::to_string(&clean).unwrap(),
            format!(
                r#"{{"version":"{}","request_schema":"{}","provenance":{{"git_commit":"{COMMIT}","git_dirty":false}}}}"#,
                env!("CARGO_PKG_VERSION"),
                crate::REQUEST_SCHEMA
            )
        );
    }
}
