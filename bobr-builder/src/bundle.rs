use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, TypedBuilder};
use bobr_core::BuildLogLevel;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Configuration for [`BundleBuilder`] (no options).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleConfig {}

/// Collects an arbitrary number of file inputs into a single directory object.
///
/// Each extra input must be a regular file. The output is a plain directory
/// object holding one hardlink per input, named by the input's own name. It
/// lets a downstream build receive many inputs as a single directory (one bind
/// mount) instead of one bind mount per input — useful for bundling many
/// fetched source archives (e.g. vendored crates) into one object.
///
/// Unlike the fs-tree builders, the output is ordinary data: it carries no
/// ownership or mode identity. Inputs are hardlinked (never copied): both the
/// staged output and the store objects live on the same filesystem.
#[derive(Debug)]
pub struct BundleBuilder;

static BUNDLE_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for BundleBuilder {
    type Config = BundleConfig;

    fn tag(&self) -> &'static str {
        "Bundle"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &BUNDLE_SPEC
    }

    fn build_typed(
        &self,
        _config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        build_bundle(inputs, cx)
    }
}

fn build_bundle(inputs: BuilderInputs, cx: &mut BuildContext) -> Result<PathBuf, BuilderError> {
    // `slots` is a BTreeMap, so extras arrive in lexical name order; the output
    // object hash is order-independent anyway (directory entries are sorted by
    // name when hashed).
    let extras = inputs.extras(&BUNDLE_SPEC).collect::<Vec<_>>();
    if extras.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "Bundle builder requires at least one file input".to_string(),
        ));
    }

    // `temp_dir` exists and is empty on entry, so a fixed name is safe.
    let output_dir = cx.temp_dir.join("bundle");
    fs::create_dir(&output_dir).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create staged bundle directory '{}': {error}",
            output_dir.display()
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "stage",
        format!("bundling {} file input(s)", extras.len()),
    );

    for (name, path) in extras {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to inspect Bundle input '{name}' ('{}'): {error}",
                path.display()
            ))
        })?;
        if !metadata.file_type().is_file() {
            return Err(BuilderError::ExecutionFailed(format!(
                "Bundle input '{name}' ('{}') is not a regular file",
                path.display()
            )));
        }
        let destination = output_dir.join(name);
        fs::hard_link(path, &destination).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to hardlink Bundle input '{name}' ('{}') into '{}': {error}",
                path.display(),
                destination.display()
            ))
        })?;
    }

    Ok(output_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Builder;
    use crate::test_support::store_fs_tree;
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use tempfile::tempdir;

    fn build_context(root: &Path) -> BuildContext {
        let temp_dir = root.join("tmp");
        fs::create_dir(&temp_dir).unwrap();
        BuildContext::with_noop_logger(temp_dir.clone(), store_fs_tree(root))
    }

    fn inputs(entries: Vec<(&str, PathBuf)>) -> BuilderInputs {
        BuilderInputs::new(
            entries
                .into_iter()
                .map(|(name, path)| (name.to_string(), path))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn bundles_file_inputs_as_hardlinks_named_by_input() {
        let temp = tempdir().unwrap();
        let a = temp.path().join("a.crate");
        let b = temp.path().join("b.crate");
        fs::write(&a, b"crate a\n").unwrap();
        fs::write(&b, b"crate b\n").unwrap();
        let mut cx = build_context(temp.path());

        let result = BundleBuilder
            .build_typed(
                BundleConfig {},
                inputs(vec![
                    ("crate_a_1_0_0", a.clone()),
                    ("crate_b_2_0_0", b.clone()),
                ]),
                &mut cx,
            )
            .unwrap();

        assert!(result.is_dir());
        assert_eq!(
            fs::read(result.join("crate_a_1_0_0")).unwrap(),
            b"crate a\n"
        );
        assert_eq!(
            fs::read(result.join("crate_b_2_0_0")).unwrap(),
            b"crate b\n"
        );
        // The bundled entries are hardlinks to the inputs (same inode, no copy).
        assert_eq!(
            fs::metadata(result.join("crate_a_1_0_0")).unwrap().ino(),
            fs::metadata(&a).unwrap().ino()
        );
    }

    #[test]
    fn rejects_non_file_input() {
        let temp = tempdir().unwrap();
        let dir_input = temp.path().join("a-dir");
        fs::create_dir(&dir_input).unwrap();
        let mut cx = build_context(temp.path());

        let error = BundleBuilder
            .build_typed(
                BundleConfig {},
                inputs(vec![("crate_dir", dir_input)]),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("is not a regular file"),
            "{error}"
        );
    }

    #[test]
    fn rejects_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = BundleBuilder
            .build_typed(BundleConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(
            error.to_string().contains("at least one file input"),
            "{error}"
        );
    }

    #[test]
    fn plan_rejects_unknown_config_field() {
        static BUILDER: BundleBuilder = BundleBuilder;
        let error = BUILDER
            .plan(serde_json::json!({ "prefix": "crates" }))
            .err()
            .expect("plan should reject unknown config fields");

        assert!(
            error.to_string().contains("invalid builder config"),
            "{error}"
        );
    }
}
