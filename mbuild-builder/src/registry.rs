use crate::{
    Builder, BuilderPlanError, BuilderPlannedSubject, ErofsRootfsBuilder, ErofsRootfsNewBuilder,
    FsTreeImportBuilder, GroupBuilder, InitramfsBuilder, InitramfsNewBuilder, OciExtractBuilder,
    OciExtractNewBuilder, TreeBuilder, TreeMergeBuilder, TreeNewBuilder, TreeSubsetBuilder,
};
use mbuild_core::{BuildKey, validate_publication_name};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static FS_TREE_IMPORT_BUILDER: FsTreeImportBuilder = FsTreeImportBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static OCI_EXTRACT_NEW_BUILDER: OciExtractNewBuilder = OciExtractNewBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_NEW_BUILDER: TreeNewBuilder = TreeNewBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;
static EROFS_ROOTFS_BUILDER: ErofsRootfsBuilder = ErofsRootfsBuilder;
static EROFS_ROOTFS_NEW_BUILDER: ErofsRootfsNewBuilder = ErofsRootfsNewBuilder;
static INITRAMFS_BUILDER: InitramfsBuilder = InitramfsBuilder;
static INITRAMFS_NEW_BUILDER: InitramfsNewBuilder = InitramfsNewBuilder;

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

    fn get(&self, tag: &str) -> Option<&'static dyn Builder> {
        self.builders
            .iter()
            .find(|builder| builder.tag().eq_ignore_ascii_case(tag))
            .copied()
    }

    /// Parses and plans one builder recipe object.
    pub fn parse_subject(
        &self,
        tag: &str,
        mut object: Map<String, Value>,
        inputs: BTreeMap<String, BuildKey>,
    ) -> Result<BuilderPlannedSubject, BuilderPlanError> {
        let name = take_string(&mut object, "name")?;
        validate_publication_name(&name)
            .map_err(|error| BuilderPlanError::recipe(format!("name: {error}")))?;
        let config = object
            .remove("config")
            .ok_or_else(|| BuilderPlanError::recipe("missing required field 'config'"))?;
        if !object.is_empty() {
            return Err(BuilderPlanError::recipe(format!(
                "unexpected fields: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }

        let builder = self
            .get(tag)
            .ok_or_else(|| BuilderPlanError::UnknownBuilder {
                tag: tag.to_string(),
                supported_tags: self.supported_tags(),
            })?;
        BuilderPlannedSubject::new(builder, name, config, inputs)
    }

    /// Returns registered builder tags in registration order.
    pub fn supported_tags(&self) -> Vec<&'static str> {
        self.builders.iter().map(|builder| builder.tag()).collect()
    }
}

fn take_string(object: &mut Map<String, Value>, field: &str) -> Result<String, BuilderPlanError> {
    let value = object
        .remove(field)
        .ok_or_else(|| BuilderPlanError::recipe(format!("missing required field '{field}'")))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| BuilderPlanError::recipe(format!("{field}: expected string")))
}

impl Default for BuilderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registers builders that currently live inside the `mbuild-builder` crate.
pub fn register_in_tree_builders(registry: &mut BuilderRegistry) -> Result<(), String> {
    registry.register(&GROUP_BUILDER)?;
    registry.register(&FS_TREE_IMPORT_BUILDER)?;
    registry.register(&TREE_BUILDER)?;
    registry.register(&TREE_NEW_BUILDER)?;
    registry.register(&TREE_SUBSET_BUILDER)?;
    registry.register(&TREE_MERGE_BUILDER)?;
    registry.register(&EROFS_ROOTFS_BUILDER)?;
    registry.register(&EROFS_ROOTFS_NEW_BUILDER)?;
    registry.register(&INITRAMFS_BUILDER)?;
    registry.register(&INITRAMFS_NEW_BUILDER)?;
    registry.register(&OCI_EXTRACT_BUILDER)?;
    registry.register(&OCI_EXTRACT_NEW_BUILDER)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildContext, BuilderInputs, InputSlot, InputSpec, StagedBuildResult};
    use mbuild_core::BuilderError;
    use serde_json::Value;
    use std::str::FromStr;

    static DUPLICATE_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    static INVALID_SPEC: InputSpec = InputSpec {
        required_inputs: &[InputSlot::object("input"), InputSlot::object("input")],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    static REQUIRED_SPEC: InputSpec = InputSpec {
        required_inputs: &[InputSlot::object("rootfs")],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    struct DuplicateTagBuilder;
    struct DuplicateTagBuilderUppercase;
    struct InvalidSpecBuilder;
    struct RequiredInputBuilder;

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

    impl Builder for RequiredInputBuilder {
        fn tag(&self) -> &'static str {
            "RequiredInput"
        }

        fn spec(&self) -> &'static InputSpec {
            &REQUIRED_SPEC
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
    static REQUIRED_INPUT: RequiredInputBuilder = RequiredInputBuilder;

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
                "FsTreeImport",
                "Tree",
                "TreeNew",
                "TreeSubset",
                "TreeMerge",
                "ErofsRootfs",
                "ErofsRootfsNew",
                "Initramfs",
                "InitramfsNew",
                "OciExtract",
                "OciExtractNew",
            ]
        );
        assert!(registry.get("Sandbox").is_none());
    }

    #[test]
    fn parse_subject_reports_unknown_builder() {
        let registry = BuilderRegistry::new();

        let error = registry
            .parse_subject(
                "Missing",
                serde_json::json!({"name": "node", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();

        assert!(matches!(error, BuilderPlanError::UnknownBuilder { .. }));
        assert!(error.to_string().contains("unknown builder tag 'Missing'"));
    }

    #[test]
    fn parse_subject_requires_name_and_config() {
        let mut registry = BuilderRegistry::new();
        registry.register(&DUPLICATE).unwrap();

        let missing_name = registry
            .parse_subject(
                "Duplicate",
                serde_json::json!({"config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();
        let missing_config = registry
            .parse_subject(
                "Duplicate",
                serde_json::json!({"name": "node"})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();

        assert_eq!(missing_name.to_string(), "missing required field 'name'");
        assert_eq!(
            missing_config.to_string(),
            "missing required field 'config'"
        );
    }

    #[test]
    fn parse_subject_rejects_invalid_name_and_unexpected_fields() {
        let mut registry = BuilderRegistry::new();
        registry.register(&DUPLICATE).unwrap();

        let invalid_name = registry
            .parse_subject(
                "Duplicate",
                serde_json::json!({"name": "bad/name", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();
        let unexpected = registry
            .parse_subject(
                "Duplicate",
                serde_json::json!({"name": "node", "config": {}, "extra": true})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();

        assert!(invalid_name.to_string().contains("name:"));
        assert_eq!(unexpected.to_string(), "unexpected fields: extra");
    }

    #[test]
    fn parse_subject_rejects_extra_inputs() {
        let mut registry = BuilderRegistry::new();
        registry.register(&DUPLICATE).unwrap();
        let input_key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();

        let error = registry
            .parse_subject(
                "Duplicate",
                serde_json::json!({"name": "node", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::from([("unexpected".to_string(), input_key)]),
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not accept extra input 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_rejects_invalid_actual_input_names() {
        let mut registry = BuilderRegistry::new();
        register_in_tree_builders(&mut registry).unwrap();
        let input_key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();

        let error = registry
            .parse_subject(
                "Group",
                serde_json::json!({"name": "group", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::from([("bad-name".to_string(), input_key)]),
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid input 'bad-name': input name 'bad-name'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_rejects_missing_required_inputs() {
        let mut registry = BuilderRegistry::new();
        registry.register(&REQUIRED_INPUT).unwrap();

        let error = registry
            .parse_subject(
                "RequiredInput",
                serde_json::json!({"name": "node", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("builder 'RequiredInput' is missing required input 'rootfs'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_computes_build_key_from_ordered_inputs() {
        let mut registry = BuilderRegistry::new();
        register_in_tree_builders(&mut registry).unwrap();
        let config = serde_json::json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": "hello.txt",
                    "text": "hello",
                    "executable": false
                }]
            }
        });
        let subject = registry
            .parse_subject(
                "Tree",
                serde_json::json!({"name": "tree", "config": config.clone()})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let expected = mbuild_core::compute_build_key("Tree", &config, &[]).unwrap();

        assert_eq!(subject.name(), "tree");
        assert_eq!(subject.tag(), "Tree");
        assert_eq!(subject.build_key(), expected);
        assert!(subject.inputs().is_empty());

        let subject = registry
            .parse_subject(
                "TreeNew",
                serde_json::json!({"name": "tree-new", "config": config.clone()})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let expected = mbuild_core::compute_build_key("TreeNew", &config, &[]).unwrap();

        assert_eq!(subject.name(), "tree-new");
        assert_eq!(subject.tag(), "TreeNew");
        assert_eq!(subject.build_key(), expected);
        assert!(subject.inputs().is_empty());
    }
}
