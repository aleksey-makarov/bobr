//! `OciRegistry` sources: images pulled from registries into OCI layouts,
//! which the store then imports like any other source tree.
//!
//! This is the runtime acquisition implementation: asynchronous, cancellable,
//! retry-aware, and integrated with the unified progress log. The older
//! synchronous OCI module remains only as parsing/test support.

mod registry;

use crate::http::HttpRetryPolicy;
use bobr_core::BuildLogger;
use bobr_core::oci::OciPlatform;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the staged layout goes inside the source's workspace. The build
/// stages it under the same name, and it is part of what is hashed: the
/// declared object hash covers this directory, not its contents alone.
const LAYOUT_SUBDIR: &str = "image";

/// An `OciRegistry` origin: which image, pinned to which digest, for which
/// platform.
#[derive(Debug, Clone)]
pub(super) struct OciOrigin {
    image: String,
    digest: String,
    platform: OciPlatform,
}

impl OciOrigin {
    /// The registry this origin will talk to: the connection-limit key, and
    /// the host the live log files this source under.
    pub(super) fn host(&self) -> String {
        registry_host(&self.image)
    }
}

/// The registry an image reference resolves to, or a placeholder when it does
/// not parse -- this is a label and a semaphore key, and neither is worth
/// failing a source over before the parser has had its say.
pub(super) fn registry_host(image: &str) -> String {
    match registry::parse_image_ref(image) {
        Ok((host, _, _)) => host,
        Err(_) => "oci-registry".to_string(),
    }
}

/// Parses an `OciRegistry` origin out of a Source node.
///
/// Unknown fields are rejected because an ignored field would silently change
/// the meaning of pinned content acquisition.
pub(super) fn parse_oci_origin(origin: &Value, field_path: &str) -> Result<OciOrigin, String> {
    let Value::Object(mut object) = origin.clone() else {
        return Err(format!("{field_path}: expected object"));
    };
    let tag = take_string(&mut object, field_path, "tag")?;
    debug_assert_eq!(tag, "OciRegistry");
    let image = take_string(&mut object, field_path, "image")?;
    if image.trim().is_empty() {
        return Err(format!("{field_path}.image: image must not be empty"));
    }
    let digest = take_string(&mut object, field_path, "digest")?;
    if !is_valid_sha256_digest(&digest) {
        return Err(format!(
            "{field_path}.digest: invalid digest '{digest}'; expected format: sha256:<64 hex chars>"
        ));
    }
    let platform = take_platform(&mut object, field_path, "platform")?;
    if !object.is_empty() {
        return Err(format!(
            "{field_path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(OciOrigin {
        image,
        digest,
        platform,
    })
}

/// Pulls the image into `temp_root` and returns the staged layout.
///
/// Cancellation is the caller's: this is a future, and dropping it stops the
/// pull where it stands. Whatever was written stays in the workspace, which is
/// removed with the run.
pub(super) async fn materialize(
    client: &reqwest::Client,
    logger: &Arc<dyn BuildLogger>,
    policy: HttpRetryPolicy,
    origin: &OciOrigin,
    temp_root: &Path,
) -> Result<PathBuf, String> {
    let staged = temp_root.join(LAYOUT_SUBDIR);
    fs::create_dir(&staged).map_err(|error| {
        format!(
            "failed to create staging dir '{}': {error}",
            staged.display()
        )
    })?;

    let session = registry::Session::new(client, logger, policy, &origin.image)
        .map_err(|error| error.to_string())?;
    match registry::pull_image(
        &session,
        &origin.image,
        &origin.digest,
        &origin.platform,
        &staged,
    )
    .await
    {
        Ok(_) => Ok(staged),
        Err(error) => {
            // What the tag points at now, appended in the form the recipe wants
            // pasted: a pin fails most often because the tag moved, and the
            // answer is one request away. Best effort -- if this fails too, the
            // original failure is the one that matters.
            let hint = session
                .resolve_current_digest()
                .await
                .map(|digest| format!("\n    digest = \"{digest}\","))
                .unwrap_or_default();
            Err(format!("{error}{hint}"))
        }
    }
}

/// Removes whatever a pull left in `temp_root`, so a failed or cancelled image
/// leaves no partial layout behind.
pub(super) fn discard(temp_root: &Path) {
    let _ = fs::remove_dir_all(temp_root.join(LAYOUT_SUBDIR));
}

fn take_string(object: &mut Map<String, Value>, path: &str, field: &str) -> Result<String, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{path}.{field}: expected string"))
}

fn take_platform(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<OciPlatform, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    let Value::Object(mut platform) = value else {
        return Err(format!("{path}.{field}: expected object"));
    };
    let field_path = format!("{path}.{field}");
    let os = take_string(&mut platform, &field_path, "os")?;
    let architecture = take_string(&mut platform, &field_path, "architecture")?;
    if !platform.is_empty() {
        return Err(format!(
            "{field_path}: unexpected fields: {}",
            platform.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if os.trim().is_empty() {
        return Err(format!("{field_path}.os must not be empty"));
    }
    if architecture.trim().is_empty() {
        return Err(format!("{field_path}.architecture must not be empty"));
    }
    Ok(OciPlatform { os, architecture })
}

fn is_valid_sha256_digest(value: &str) -> bool {
    const PREFIX: &str = "sha256:";
    let Some(hex) = value.strip_prefix(PREFIX) else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn origin_value() -> Value {
        json!({
            "tag": "OciRegistry",
            "image": "quay.io/team/app:1.2",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "platform": { "os": "linux", "architecture": "amd64" }
        })
    }

    #[test]
    fn parses_a_pinned_image() {
        let origin = parse_oci_origin(&origin_value(), "origin").unwrap();
        assert_eq!(origin.image, "quay.io/team/app:1.2");
        assert_eq!(origin.platform.architecture, "amd64");
        assert_eq!(origin.host(), "quay.io");
    }

    #[test]
    fn an_unpinned_or_misspelled_origin_is_refused() {
        let mut value = origin_value();
        value["digest"] = json!("latest");
        let error = parse_oci_origin(&value, "origin").unwrap_err();
        assert!(error.contains("invalid digest"), "{error}");

        let mut value = origin_value();
        value["platfrom"] = json!({});
        let error = parse_oci_origin(&value, "origin").unwrap_err();
        assert!(error.contains("unexpected fields: platfrom"), "{error}");
    }

    #[test]
    fn an_unparseable_image_still_has_a_host_to_be_filed_under() {
        assert_eq!(registry_host("alpine"), "registry-1.docker.io");
        assert_eq!(registry_host(""), "oci-registry");
    }
}
