use crate::{
    BuildContext, Builder, BuilderInputs, InputSpec, StagedBuildResult, validate_input_name,
};
use fsobj_hash::ObjectHash;
use mbuild_core::{
    BuildKey, BuilderError, IdentityError, ReuseKey, compute_build_key, compute_reuse_key,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// Error returned while planning a builder subject.
#[derive(Debug)]
pub enum BuilderPlanError {
    /// The builder tag is not registered in the registry used for planning.
    UnknownBuilder {
        tag: String,
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
    pub(crate) fn recipe(message: impl Into<String>) -> Self {
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
pub struct BuilderPlannedSubject {
    builder: &'static dyn Builder,
    name: String,
    config: Value,
    inputs: BTreeMap<String, BuildKey>,
    build_key: BuildKey,
}

impl fmt::Debug for BuilderPlannedSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BuilderPlannedSubject")
            .field("tag", &self.tag())
            .field("name", &self.name)
            .field("inputs", &self.inputs)
            .field("build_key", &self.build_key)
            .finish_non_exhaustive()
    }
}

impl BuilderPlannedSubject {
    pub(crate) fn new(
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

        for required in spec.required_inputs {
            if !inputs.contains_key(*required) {
                return Err(BuilderPlanError::invalid_request(format!(
                    "builder '{}' is missing required input '{}' in recipe '{}'",
                    tag, required, name
                )));
            }
        }

        let ordered_direct_deps = spec
            .ordered_present_input_names(&inputs)
            .into_iter()
            .filter_map(|input_name| inputs.get(input_name).copied())
            .collect::<Vec<_>>();
        let build_key = compute_build_key(tag, &config, &ordered_direct_deps)
            .map_err(BuilderPlanError::identity)?;

        Ok(Self {
            builder,
            name,
            config,
            inputs,
            build_key,
        })
    }

    /// Returns the user-facing recipe name for this planned builder.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the builder class tag.
    pub fn tag(&self) -> &'static str {
        self.builder.tag()
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
        self.builder.spec()
    }

    /// Computes the reuse key for this builder and realized input hashes.
    pub fn compute_reuse_key(
        &self,
        input_hashes: &[ObjectHash],
    ) -> Result<ReuseKey, BuilderPlanError> {
        compute_reuse_key(self.tag(), &self.config, input_hashes)
            .map_err(BuilderPlanError::identity)
    }

    /// Executes the underlying builder implementation.
    pub fn build_erased(
        &self,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        self.builder.build_erased(self.config.clone(), inputs, cx)
    }
}
