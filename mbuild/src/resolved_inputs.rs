use crate::execution::{ExecutionError, map_store_error};
use bobr_core::{ObjectHash, RuntimeProvider};
use bobr_store::Store;
use mbuild_builder::{
    BuilderError, BuilderInputPath, BuilderInputs, InputKind, InputSpec, materialize_fs_tree_root,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedDependency {
    pub(crate) object_hash: ObjectHash,
    pub(crate) object_path: PathBuf,
    pub(crate) materialization_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedInputs {
    slots: BTreeMap<String, ResolvedDependency>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ResolvedInputs {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn new(slots: BTreeMap<String, ResolvedDependency>) -> Self {
        Self { slots }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn insert(&mut self, name: impl Into<String>, value: ResolvedDependency) {
        self.slots.insert(name.into(), value);
    }

    fn required(&self, name: &str) -> Result<&ResolvedDependency, BuilderError> {
        match self.slots.get(name) {
            Some(object) => Ok(object),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    fn optional(&self, name: &str) -> Option<&ResolvedDependency> {
        self.slots.get(name)
    }

    fn get(&self, name: &str) -> Option<&ResolvedDependency> {
        self.slots.get(name)
    }

    fn extra<'a>(&'a self, spec: &InputSpec, name: &str) -> Option<&'a ResolvedDependency> {
        if spec.is_reserved_input(name) {
            None
        } else {
            self.slots.get(name)
        }
    }

    fn extras<'a>(
        &'a self,
        spec: &'a InputSpec,
    ) -> impl Iterator<Item = (&'a str, &'a ResolvedDependency)> + 'a {
        self.slots.iter().filter_map(move |(name, dep)| {
            if spec.is_reserved_input(name) {
                None
            } else {
                Some((name.as_str(), dep))
            }
        })
    }

    pub(crate) fn ordered_reuse_input_hashes(
        &self,
        spec: &InputSpec,
    ) -> Result<Vec<ObjectHash>, BuilderError> {
        let mut ordered = Vec::new();
        for name in spec.ordered_present_input_names(&self.slots) {
            if let Some(object) = self.get(name) {
                ordered.push(object.object_hash);
            }
        }
        Ok(ordered)
    }

    pub(crate) fn prepare_builder_inputs(
        self,
        spec: &InputSpec,
        store: &Store,
        runtime: &RuntimeProvider,
    ) -> Result<BuilderInputs, ExecutionError> {
        let mut slots = BTreeMap::new();
        for (name, value) in self.slots {
            let path = match spec.input_kind(&name).unwrap_or(InputKind::Object) {
                InputKind::Object => value.object_path,
                InputKind::FsTreeRoot => prepare_fs_tree_root_input(
                    store,
                    runtime,
                    value.object_hash,
                    value.materialization_name.as_deref(),
                )?,
            };
            slots.insert(name, BuilderInputPath { path });
        }
        Ok(BuilderInputs::new(slots))
    }
}

fn prepare_fs_tree_root_input(
    store: &Store,
    runtime: &RuntimeProvider,
    object_hash: ObjectHash,
    materialization_name: Option<&str>,
) -> Result<PathBuf, ExecutionError> {
    let fs_tree = store.fs_tree();
    if let Some(root) = fs_tree
        .lookup_materialized_root(object_hash)
        .map_err(map_store_error)?
    {
        if let Some(name) = materialization_name {
            fs_tree
                .ensure_materialized_root(Some(name), object_hash)
                .map_err(map_store_error)?;
        }
        return Ok(root);
    }
    materialize_fs_tree_root(runtime, fs_tree, object_hash, materialization_name).map_err(|error| {
        ExecutionError::Build(format!(
            "failed to materialize fs-tree input '{}': {error}",
            object_hash
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_store::{Store, import_object};
    use mbuild_builder::InputSlot;
    use std::fs;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn sample_object() -> ResolvedDependency {
        ResolvedDependency {
            object_hash: ObjectHash::from_str(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            object_path: PathBuf::from("/tmp/object"),
            materialization_name: Some("sample".to_string()),
        }
    }

    #[test]
    fn resolved_inputs_helpers_work() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", object.clone());
        inputs.insert("source", object.clone());

        static SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::object("rootfs")],
            optional_inputs: &[InputSlot::object("base")],
            allow_extra_inputs: true,
        };

        assert_eq!(
            inputs.required("script").unwrap().object_path,
            object.object_path
        );
        assert!(inputs.optional("base").is_none());
        assert!(inputs.extra(&SPEC, "source").is_some());
        assert_eq!(inputs.extras(&SPEC).count(), 2);
    }

    #[test]
    fn resolved_inputs_missing_required_input_is_an_error() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", object);

        assert!(matches!(
            inputs.required("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(inputs.optional("missing").is_none());
    }

    #[test]
    fn resolved_inputs_extras_follow_lexical_order() {
        let first = sample_object();
        let second = sample_object();

        static SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::object("rootfs")],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };
        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("source_b".to_string(), second.clone()),
            ("source_a".to_string(), first.clone()),
        ]));

        let extras = inputs.extras(&SPEC).collect::<Vec<_>>();
        assert_eq!(extras[0].0, "source_a");
        assert_eq!(extras[0].1.object_path, first.object_path);
        assert_eq!(extras[1].0, "source_b");
        assert_eq!(extras[1].1.object_path, second.object_path);
    }

    #[test]
    fn resolved_inputs_new_and_is_empty_work() {
        assert!(ResolvedInputs::empty().is_empty());
        let mut slots = BTreeMap::new();
        slots.insert("script".to_string(), sample_object());
        assert!(!ResolvedInputs::new(slots).is_empty());
    }

    #[test]
    fn resolved_inputs_optional_some_returns_object() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([("base".to_string(), object.clone())]));

        let resolved = inputs.optional("base").unwrap();
        assert_eq!(resolved.object_path, object.object_path);
    }

    #[test]
    fn resolved_inputs_convert_to_builder_inputs() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([("script".to_string(), object.clone())]));
        static SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::object("script")],
            optional_inputs: &[],
            allow_extra_inputs: false,
        };

        let builder_inputs = inputs
            .prepare_builder_inputs(&SPEC, &store, &RuntimeProvider::host())
            .unwrap();
        let resolved = builder_inputs.required("script").unwrap();
        assert_eq!(resolved.path, object.object_path);
    }

    #[test]
    fn resolved_inputs_prepare_fs_tree_root_materializes_cache() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"hello\n").unwrap();
        let manifest = store.fs_tree().scan(&source).unwrap();
        let staged_manifest = temp.path().join("manifest.jsonl");
        manifest.write_canonical(&staged_manifest).unwrap();
        let object_hash = import_object(&store, &staged_manifest).unwrap();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "tree".to_string(),
            ResolvedDependency {
                object_hash,
                object_path: store.object_path(object_hash),
                materialization_name: Some("source-tree".to_string()),
            },
        )]));
        static SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::fs_tree_root("tree")],
            optional_inputs: &[],
            allow_extra_inputs: false,
        };

        let builder_inputs = inputs
            .prepare_builder_inputs(&SPEC, &store, &RuntimeProvider::host())
            .unwrap();

        let resolved = builder_inputs.required("tree").unwrap();
        assert_eq!(
            resolved.path,
            store
                .fs_tree()
                .lookup_materialized_root(object_hash)
                .unwrap()
                .unwrap()
        );
        assert_eq!(fs::read(resolved.path.join("file")).unwrap(), b"hello\n");
        assert_eq!(
            fs::read_link(store_root.join("fs-tree-refs").join("source-tree")).unwrap(),
            PathBuf::from("..")
                .join("fs-trees")
                .join(object_hash.to_string())
        );
    }

    #[test]
    fn resolved_inputs_prepare_fs_tree_root_reuses_cache_without_runtime_setup() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"hello\n").unwrap();
        let manifest = store.fs_tree().scan(&source).unwrap();
        let staged_manifest = temp.path().join("manifest.jsonl");
        manifest.write_canonical(&staged_manifest).unwrap();
        let object_hash = import_object(&store, &staged_manifest).unwrap();
        let root = store
            .fs_tree()
            .ensure_materialized_root(None, object_hash)
            .unwrap();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "tree".to_string(),
            ResolvedDependency {
                object_hash,
                object_path: store.object_path(object_hash),
                materialization_name: Some("cached-tree".to_string()),
            },
        )]));
        static SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::fs_tree_root("tree")],
            optional_inputs: &[],
            allow_extra_inputs: false,
        };

        let builder_inputs = inputs
            .prepare_builder_inputs(&SPEC, &store, &RuntimeProvider::namespace())
            .unwrap();

        assert_eq!(builder_inputs.required("tree").unwrap().path, root);
        assert_eq!(
            fs::read_link(store_root.join("fs-tree-refs").join("cached-tree")).unwrap(),
            PathBuf::from("..")
                .join("fs-trees")
                .join(object_hash.to_string())
        );
    }

    static ORDERED_SPEC: InputSpec = InputSpec {
        required_inputs: &[InputSlot::object("first")],
        optional_inputs: &[InputSlot::object("optional")],
        allow_extra_inputs: true,
    };

    #[test]
    fn ordered_reuse_input_hashes_follow_input_spec_order() {
        let mut first = sample_object();
        first.object_hash = ObjectHash::from_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let mut optional = sample_object();
        optional.object_hash = ObjectHash::from_str(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        let mut extra_a = sample_object();
        extra_a.object_hash = ObjectHash::from_str(
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        let mut extra_b = sample_object();
        extra_b.object_hash = ObjectHash::from_str(
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        )
        .unwrap();

        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("first".to_string(), first),
            ("optional".to_string(), optional),
            ("extra_b".to_string(), extra_b),
            ("extra_a".to_string(), extra_a),
        ]));

        let ordered = inputs.ordered_reuse_input_hashes(&ORDERED_SPEC).unwrap();
        assert_eq!(
            ordered,
            vec![
                ObjectHash::from_str(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap(),
                ObjectHash::from_str(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .unwrap(),
                ObjectHash::from_str(
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                )
                .unwrap(),
                ObjectHash::from_str(
                    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                )
                .unwrap(),
            ]
        );
    }
}
