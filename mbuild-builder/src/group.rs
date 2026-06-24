use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use mbuild_core::BuildLogLevel;
use serde::Deserialize;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupConfig {}

pub struct GroupBuilder;

static GROUP_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for GroupBuilder {
    type Config = GroupConfig;

    fn tag(&self) -> &'static str {
        "Group"
    }

    fn spec(&self) -> &'static InputSpec {
        &GROUP_SPEC
    }

    fn build_typed(
        &self,
        _config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        if inputs.extras(&GROUP_SPEC).next().is_none() {
            return Err(BuilderError::ExecutionFailed(
                "Group builder requires at least one input".to_string(),
            ));
        }

        cx.log_event(
            BuildLogLevel::Info,
            "stage",
            "writing group marker".to_string(),
        );

        let marker_path = cx.temp_dir.join("group-marker");
        if marker_path.exists() {
            fs::remove_file(&marker_path).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to remove previous group marker '{}': {error}",
                    marker_path.display()
                ))
            })?;
        }

        fs::write(&marker_path, b"").map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to write staged group marker '{}': {error}",
                marker_path.display()
            ))
        })?;

        #[cfg(unix)]
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644)).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to set mode on staged group marker '{}': {error}",
                marker_path.display()
            ))
        })?;

        Ok(StagedBuildResult {
            staged_path: marker_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Builder, BuilderInputPath};
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::tempdir;

    fn build_context(root: &Path) -> BuildContext {
        let temp_dir = root.join("group").join("tmp");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        BuildContext::with_noop_logger(temp_dir)
    }

    fn sample_input() -> BuilderInputPath {
        BuilderInputPath {
            path: std::path::PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn spec_accepts_extra_inputs_only() {
        let builder = GroupBuilder;
        let spec = crate::TypedBuilder::spec(&builder);

        assert_eq!(crate::TypedBuilder::tag(&builder), "Group");
        assert!(spec.required_inputs.is_empty());
        assert!(spec.optional_inputs.is_empty());
        assert!(spec.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_empty_inputs() {
        let builder = GroupBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed(GroupConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Group builder requires at least one input"
        );
    }

    #[test]
    fn build_creates_empty_marker_file() {
        let builder = GroupBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let inputs = BuilderInputs::new(BTreeMap::from([("first".to_string(), sample_input())]));

        let result = builder
            .build_typed(GroupConfig {}, inputs, &mut cx)
            .unwrap();

        assert_eq!(std::fs::read(&result.staged_path).unwrap(), b"");

        #[cfg(unix)]
        {
            let metadata = std::fs::metadata(&result.staged_path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
        }
    }

    #[test]
    fn erased_config_rejects_unknown_fields() {
        let builder = GroupBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let inputs = BuilderInputs::new(BTreeMap::from([("first".to_string(), sample_input())]));

        let error = builder
            .build_erased(serde_json::json!({ "unexpected": true }), inputs, &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("invalid builder config"));
    }
}
