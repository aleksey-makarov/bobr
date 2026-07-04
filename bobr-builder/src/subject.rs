use crate::{
    BuildContext, Builder, BuilderError, BuilderInputs, InputSpec, PlannedBuild,
    validate_input_name,
};
use bobr_core::{BuildKey, BuildLogSubject, IdentityError, ReuseKey, SubjectRunContext, Workspace};
use bobr_store::fs_tree::FsTree;
use fsobj_hash::ObjectHash;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// Error returned while planning a builder subject.
#[derive(Debug)]
pub enum BuilderPlanError {
    /// The builder tag is not registered in the registry used for planning.
    UnknownBuilder {
        /// The unknown builder tag from the recipe.
        tag: String,
        /// Tags the planning registry does support.
        supported_tags: Vec<&'static str>,
    },
    /// The recipe object does not match the builder recipe shape.
    Recipe(String),
    /// The recipe asks a builder to use an invalid set of inputs.
    InvalidRequest(String),
    /// A stable identity could not be computed.
    Identity(IdentityError),
}

impl BuilderPlanError {
    /// Builds a [`Recipe`](BuilderPlanError::Recipe) error from a message.
    pub fn recipe(message: impl Into<String>) -> Self {
        Self::Recipe(message.into())
    }

    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    pub(crate) fn identity(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}

impl fmt::Display for BuilderPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBuilder {
                tag,
                supported_tags,
            } => write!(
                formatter,
                "unknown builder tag '{}'; supported builders: {}",
                tag,
                supported_tags.join(", ")
            ),
            Self::Recipe(message) | Self::InvalidRequest(message) => formatter.write_str(message),
            Self::Identity(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for BuilderPlanError {}

/// Planned builder node with its validated inputs and stable build key.
///
/// Holds a type-erased [`PlannedBuild`] handle (the builder plus its
/// deserialized typed config); `tag` and `spec` are copied out of the builder at
/// plan time so the subject needs no `&dyn Builder` afterwards.
pub struct BuilderPlannedSubject {
    tag: &'static str,
    name: String,
    spec: &'static InputSpec,
    inputs: BTreeMap<String, BuildKey>,
    build_key: BuildKey,
    plan: Box<dyn PlannedBuild>,
}

impl fmt::Debug for BuilderPlannedSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BuilderPlannedSubject")
            .field("tag", &self.tag)
            .field("name", &self.name)
            .field("inputs", &self.inputs)
            .field("build_key", &self.build_key)
            .finish_non_exhaustive()
    }
}

impl BuilderPlannedSubject {
    /// Plans a builder subject from the `builder`, recipe `name`, raw `config`,
    /// and resolved input keys: validates the inputs against the builder's spec
    /// and computes the subject's build key.
    pub fn new(
        builder: &'static dyn Builder,
        name: String,
        config: Value,
        inputs: BTreeMap<String, BuildKey>,
    ) -> Result<Self, BuilderPlanError> {
        let tag = builder.tag();
        let spec = builder.spec();
        let reserved_inputs = spec.reserved_input_names().collect::<Vec<_>>();
        for input_name in inputs.keys() {
            validate_input_name(input_name).map_err(|error| {
                BuilderPlanError::recipe(format!("invalid input '{input_name}': {error}"))
            })?;
            if !spec.allow_extra_inputs && !spec.is_reserved_input(input_name) {
                return Err(BuilderPlanError::invalid_request(format!(
                    "builder '{}' does not accept extra input '{}'; allowed inputs: {}",
                    tag,
                    input_name,
                    reserved_inputs.join(", ")
                )));
            }
        }

        for &required in spec.required_inputs {
            if !inputs.contains_key(required) {
                return Err(BuilderPlanError::invalid_request(format!(
                    "builder '{}' is missing required input '{}' in recipe '{}'",
                    tag, required, name
                )));
            }
        }

        // Deserialize the config once into a type-erased handle that owns the
        // typed config and computes this node's keys. (The handle also folds
        // BOBR_BUILD_CORE_VERSION and, for arch-dependent builders, the target
        // arch into its version token.)
        let plan = builder
            .plan(config)
            .map_err(|error| BuilderPlanError::recipe(error.to_string()))?;
        let build_key = plan
            .build_key(&inputs)
            .map_err(BuilderPlanError::identity)?;

        Ok(Self {
            tag,
            name,
            spec,
            inputs,
            build_key,
            plan,
        })
    }

    /// Returns the user-facing recipe name for this planned builder.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the builder class tag.
    pub fn tag(&self) -> &'static str {
        self.tag
    }

    /// Returns the stable build key for this planned builder.
    pub fn build_key(&self) -> BuildKey {
        self.build_key
    }

    /// Returns input names mapped to dependency build keys.
    pub fn inputs(&self) -> &BTreeMap<String, BuildKey> {
        &self.inputs
    }

    /// Returns the input spec advertised by the builder class.
    pub fn input_spec(&self) -> &'static InputSpec {
        self.spec
    }

    /// Computes the reuse key for this builder and realized input hashes.
    pub fn compute_reuse_key(
        &self,
        input_hashes: &BTreeMap<String, ObjectHash>,
    ) -> Result<ReuseKey, BuilderPlanError> {
        self.plan
            .reuse_key(input_hashes)
            .map_err(BuilderPlanError::identity)
    }

    /// Executes the underlying builder implementation.
    pub fn build(
        &self,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        self.plan.build(inputs, cx)
    }

    /// Builds the per-run log subject from the runtime-allocated workspace.
    pub fn log_subject(&self, workspace: &Workspace) -> BuildLogSubject {
        BuildLogSubject::new(
            self.tag(),
            self.name(),
            self.build_key().to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        )
    }

    /// Builds the artifact into the run's temp directory and returns the staged
    /// result.
    ///
    /// This does not touch the object store: the caller resolves reuse and
    /// inputs beforehand and materializes the staged result afterwards.
    pub fn execute(
        &self,
        ctx: &SubjectRunContext,
        inputs: BuilderInputs,
        fs_tree: FsTree,
    ) -> Result<PathBuf, BuilderError> {
        let mut context = BuildContext::with_noop_logger(ctx.temp_dir().to_path_buf(), fs_tree)
            .with_logger(ctx.logger().clone())
            .with_cancellation_token(ctx.cancellation().clone())
            .with_runtime_provider(ctx.runtime().clone())
            .with_build_seed(ctx.build_seed());
        self.build(inputs, &mut context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store_fs_tree;
    use bobr_core::{CancellationToken, NoopBuildLogger};
    use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct StagingBuilder;

    #[derive(Debug, Clone, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct EmptyConfig {}

    static STAGING_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };
    static STAGING_BUILDER: StagingBuilder = StagingBuilder;

    impl crate::TypedBuilder for StagingBuilder {
        type Config = EmptyConfig;

        fn tag(&self) -> &'static str {
            "Staging"
        }

        fn impl_version(&self) -> &'static str {
            "test"
        }

        fn spec(&self) -> &'static InputSpec {
            &STAGING_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            _inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<PathBuf, BuilderError> {
            assert_eq!(cx.runtime().backend(), RuntimeBackend::Namespace);
            let out = cx.temp_dir.join("out");
            std::fs::create_dir_all(&out)
                .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
            std::fs::write(out.join("payload"), b"ok")
                .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
            Ok(out)
        }
    }

    #[test]
    fn execute_builds_into_ctx_temp_dir() {
        let subject = BuilderPlannedSubject::new(
            &STAGING_BUILDER,
            "demo".to_string(),
            serde_json::json!({}),
            BTreeMap::new(),
        )
        .unwrap();
        let temp = tempdir().unwrap();
        let workspace = Workspace::new(
            temp.path().join("log"),
            temp.path().join("log/raw"),
            temp.path().to_path_buf(),
        );
        let ctx = SubjectRunContext::new(
            workspace,
            Arc::new(NoopBuildLogger),
            CancellationToken::new(),
            RuntimeProvider::namespace(),
            bobr_core::BuildSeed::ZERO,
        );

        let staged = subject
            .execute(&ctx, BuilderInputs::empty(), store_fs_tree(temp.path()))
            .unwrap();

        assert_eq!(staged, temp.path().join("out"));
        assert!(staged.join("payload").is_file());
    }
}
