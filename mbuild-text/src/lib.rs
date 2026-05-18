use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputs, BuilderSpec, StagedBuildResult,
    TypedBuilder, fsutil,
};
use serde::Deserialize;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextConfig {
    source: String,
    #[serde(default)]
    executable: bool,
}

pub struct TextBuilder;

static TEXT_SPEC: BuilderSpec = BuilderSpec {
    tag: "Text",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for TextBuilder {
    type Config = TextConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &TEXT_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        if !inputs.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "Text builder does not accept input objects".to_string(),
            ));
        }

        cx.log_event(
            BuildLogLevel::Info,
            "stage",
            "writing text output".to_string(),
        );

        let now_nanos = fsutil::current_epoch_nanos()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        let tmp_path = cx.temp_dir.join(format!("text-{now_nanos}.obj"));

        if tmp_path.exists() {
            fs::remove_file(&tmp_path).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to remove previous temporary file '{}': {error}",
                    tmp_path.display()
                ))
            })?;
        }

        fs::write(&tmp_path, &config.source).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to write staged text output '{}': {error}",
                tmp_path.display()
            ))
        })?;

        #[cfg(unix)]
        if config.executable {
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&tmp_path, perms).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to set executable mode on staged text output '{}': {error}",
                    tmp_path.display()
                ))
            })?;
        }

        Ok(StagedBuildResult {
            staged_path: tmp_path,
            object_hash: None,
            object_index: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{Builder, BuilderInputs};
    use tempfile::tempdir;

    fn build_context(root: &std::path::Path) -> BuildContext {
        let state_dir = root.join("text");
        let temp_dir = state_dir.join("tmp");
        std::fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    #[test]
    fn build_typed_creates_staged_file() {
        let builder = TextBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
                TextConfig {
                    source: "hello".to_string(),
                    executable: false,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(&result.staged_path).unwrap(), "hello");
    }

    #[test]
    fn build_script_sets_executable_bit() {
        let builder = TextBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
                TextConfig {
                    source: "#!/bin/sh\necho hi\n".to_string(),
                    executable: true,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        #[cfg(unix)]
        {
            let mode = fs::metadata(&result.staged_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn non_build_script_does_not_set_executable_bit() {
        let builder = TextBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
                TextConfig {
                    source: "hello".to_string(),
                    executable: false,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        #[cfg(unix)]
        {
            let mode = fs::metadata(&result.staged_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0);
        }
    }

    #[test]
    fn text_builder_rejects_non_empty_inputs() {
        let builder = TextBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "script",
            mbuild_core::BuilderInputObject {
                object_path: temp.path().join("dummy"),
                object_hash: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
            },
        );

        let error = builder
            .build_typed(
                TextConfig {
                    source: "hello".to_string(),
                    executable: false,
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let builder = TextBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_erased(
                serde_json::json!({
                    "source": "hello",
                    "executable": false,
                    "extra": true
                }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }
}
