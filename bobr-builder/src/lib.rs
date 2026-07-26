//! In-tree builders for bobr.
//!
//! Defines the [`Builder`]/[`TypedBuilder`] traits, the [`BuildContext`] and
//! input contract ([`InputSpec`]/[`BuilderInputs`]), and the concrete builders
//! bobr ships — tree, bundle, group, tree-merge, tree-subset, fs-tree-import,
//! fs-tree-export, tree-move, HostBundle, and OCI extract. [`BUILDERS`]
//! is the registry of all of them.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod builder;
mod bundle;
mod error;
mod fs_tree_export;
mod fs_tree_import;
mod fs_tree_materialize;
mod group;
mod host_bundle;
mod host_bundle_verify;
mod oci_extract;
mod plain_tree_copy;
mod registry;
mod subject;
#[cfg(test)]
mod test_support;
mod tree;
mod tree_merge;
mod tree_move;
mod tree_subset;

pub use builder::*;
pub use bundle::{BundleBuilder, BundleConfig};
pub use error::BuilderError;
pub use fs_tree_export::{CopyCommand, FsTreeExportBuilder, FsTreeExportConfig};
pub use fs_tree_import::{FsTreeImportBuilder, FsTreeImportConfig};
pub use fs_tree_materialize::materialize_fs_tree_root;
pub use group::{GroupBuilder, GroupConfig};
pub use host_bundle::{
    HOST_BUNDLE_INPUT_SPEC, HostBundleBuilder, HostBundleConfig, HostBundleConfigError,
    HostBundleEnvironmentRule, HostBundlePath, HostBundleToolConfig,
};
pub use oci_extract::{OciExtractBuilder, OciExtractConfig};
pub use registry::BUILDERS;
pub use subject::{BuilderPlanError, BuilderPlannedSubject};
pub use tree::{TreeBuilder, TreeConfig};
pub use tree_merge::{TreeMergeBuilder, TreeMergeConfig};
pub use tree_move::{TreeMoveBuilder, TreeMoveConfig};
pub use tree_subset::{TreeSubsetBuilder, TreeSubsetConfig};

/// Return runtime functions supported by built-in builders.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_export::FsTreeExportFunction),
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_import::FsTreeImportFunction),
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_materialize::FsTreeMaterializeFunction),
        bobr_runtime::runtime_ns::NsFunction::new(oci_extract::OciExtractFunction),
        bobr_runtime::runtime_ns::NsFunction::new(plain_tree_copy::PlainTreeCopyFunction),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_function_registry_includes_fs_tree_import() {
        let functions = crate::runtime_functions();

        assert_eq!(functions.len(), 5);
        assert_eq!(functions[0].name(), "fs-tree-export");
        assert_eq!(functions[1].name(), "fs-tree-import");
        assert_eq!(functions[2].name(), "fs-tree-materialize");
        assert_eq!(functions[3].name(), "oci-extract");
        assert_eq!(functions[4].name(), "plain-tree-copy");
    }
}
