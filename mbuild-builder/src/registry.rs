use crate::{
    Builder, ErofsRootfsBuilder, GroupBuilder, InitramfsBuilder, OciExtractBuilder, TreeBuilder,
    TreeMergeBuilder, TreeSubsetBuilder,
};

static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;
static EROFS_ROOTFS_BUILDER: ErofsRootfsBuilder = ErofsRootfsBuilder;
static INITRAMFS_BUILDER: InitramfsBuilder = InitramfsBuilder;

/// Explicit registry of builder classes available to one runtime invocation.
pub struct BuilderRegistry {
    builders: Vec<&'static dyn Builder>,
}

impl BuilderRegistry {
    /// Creates an empty builder registry.
    pub fn new() -> Self {
        Self {
            builders: Vec::new(),
        }
    }

    /// Registers one builder class after validating its advertised input spec.
    ///
    /// Tags are matched case-insensitively, so registering two builders whose
    /// tags differ only by ASCII case is rejected.
    pub fn register(&mut self, builder: &'static dyn Builder) -> Result<(), String> {
        let tag = builder.tag();
        builder.spec().validate_for_builder(tag)?;
        if self
            .builders
            .iter()
            .any(|registered| registered.tag().eq_ignore_ascii_case(tag))
        {
            return Err(format!("duplicate builder tag '{tag}'"));
        }
        self.builders.push(builder);
        Ok(())
    }

    /// Returns the registered builder matching `tag`, ignoring ASCII case.
    pub fn get(&self, tag: &str) -> Option<&'static dyn Builder> {
        self.builders
            .iter()
            .find(|builder| builder.tag().eq_ignore_ascii_case(tag))
            .copied()
    }

    /// Returns registered builder tags in registration order.
    pub fn supported_tags(&self) -> Vec<&'static str> {
        self.builders.iter().map(|builder| builder.tag()).collect()
    }
}

impl Default for BuilderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registers builders that currently live inside the `mbuild-builder` crate.
pub fn register_in_tree_builders(registry: &mut BuilderRegistry) -> Result<(), String> {
    registry.register(&GROUP_BUILDER)?;
    registry.register(&TREE_BUILDER)?;
    registry.register(&TREE_SUBSET_BUILDER)?;
    registry.register(&TREE_MERGE_BUILDER)?;
    registry.register(&EROFS_ROOTFS_BUILDER)?;
    registry.register(&INITRAMFS_BUILDER)?;
    registry.register(&OCI_EXTRACT_BUILDER)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult};
    use mbuild_core::BuilderError;
    use serde_json::Value;

    static DUPLICATE_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    static INVALID_SPEC: InputSpec = InputSpec {
        required_inputs: &["input", "input"],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    struct DuplicateTagBuilder;
    struct DuplicateTagBuilderUppercase;
    struct InvalidSpecBuilder;

    impl Builder for DuplicateTagBuilder {
        fn tag(&self) -> &'static str {
            "Duplicate"
        }

        fn spec(&self) -> &'static InputSpec {
            &DUPLICATE_SPEC
        }

        fn build_erased(
            &self,
            _config: Value,
            _inputs: BuilderInputs,
            _cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            unreachable!("registry tests do not execute builders")
        }
    }

    impl Builder for DuplicateTagBuilderUppercase {
        fn tag(&self) -> &'static str {
            "DUPLICATE"
        }

        fn spec(&self) -> &'static InputSpec {
            &DUPLICATE_SPEC
        }

        fn build_erased(
            &self,
            _config: Value,
            _inputs: BuilderInputs,
            _cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            unreachable!("registry tests do not execute builders")
        }
    }

    impl Builder for InvalidSpecBuilder {
        fn tag(&self) -> &'static str {
            "Invalid"
        }

        fn spec(&self) -> &'static InputSpec {
            &INVALID_SPEC
        }

        fn build_erased(
            &self,
            _config: Value,
            _inputs: BuilderInputs,
            _cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            unreachable!("registry tests do not execute builders")
        }
    }

    static DUPLICATE: DuplicateTagBuilder = DuplicateTagBuilder;
    static DUPLICATE_UPPERCASE: DuplicateTagBuilderUppercase = DuplicateTagBuilderUppercase;
    static INVALID: InvalidSpecBuilder = InvalidSpecBuilder;

    #[test]
    fn register_rejects_duplicate_tags_case_insensitively() {
        let mut registry = BuilderRegistry::new();
        registry.register(&DUPLICATE).unwrap();

        let error = registry.register(&DUPLICATE_UPPERCASE).unwrap_err();

        assert!(error.contains("duplicate builder tag 'DUPLICATE'"));
    }

    #[test]
    fn register_rejects_invalid_input_spec() {
        let mut registry = BuilderRegistry::new();

        let error = registry.register(&INVALID).unwrap_err();

        assert_eq!(
            error,
            "builder 'Invalid' declares duplicate required input 'input'"
        );
        assert!(registry.supported_tags().is_empty());
    }

    #[test]
    fn register_in_tree_builders_registers_non_sandbox_builders() {
        let mut registry = BuilderRegistry::new();
        register_in_tree_builders(&mut registry).unwrap();

        assert_eq!(
            registry.supported_tags(),
            vec![
                "Group",
                "Tree",
                "TreeSubset",
                "TreeMerge",
                "ErofsRootfs",
                "Initramfs",
                "OciExtract",
            ]
        );
        assert!(registry.get("Sandbox").is_none());
    }
}
