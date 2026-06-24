pub mod builder;
mod erofs;
mod error;
mod fs_tree_import;
mod fs_tree_materialize;
pub mod group;
mod initramfs;
mod oci_extract;
mod registry;
pub mod subject;
pub mod tree;
mod tree_merge;
mod tree_subset;

pub use builder::*;
pub use erofs::{ErofsRootfsBuilder, ErofsRootfsConfig};
pub use error::BuilderError;
pub use fs_tree_import::{FsTreeImportBuilder, FsTreeImportConfig};
pub use fs_tree_materialize::materialize_fs_tree_root;
pub use group::{GroupBuilder, GroupConfig};
pub use initramfs::{InitramfsBuilder, InitramfsConfig};
pub use oci_extract::{OciExtractBuilder, OciExtractConfig};
pub use registry::{BuilderRegistry, register_in_tree_builders};
pub use subject::{BuilderPlanError, BuilderPlannedSubject};
pub use tree::{TreeBuilder, TreeConfig};
pub use tree_merge::{TreeMergeBuilder, TreeMergeConfig};
pub use tree_subset::{TreeSubsetBuilder, TreeSubsetConfig};

/// Return runtime functions supported by built-in builders.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![
        bobr_runtime::runtime_ns::NsFunction::new(erofs::ErofsRootfsFunction),
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_import::FsTreeImportFunction),
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_materialize::FsTreeMaterializeFunction),
        bobr_runtime::runtime_ns::NsFunction::new(initramfs::InitramfsFunction),
        bobr_runtime::runtime_ns::NsFunction::new(oci_extract::OciExtractFunction),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_function_registry_includes_fs_tree_import() {
        let functions = crate::runtime_functions();

        assert_eq!(functions.len(), 5);
        assert_eq!(functions[0].name(), "erofs-rootfs");
        assert_eq!(functions[1].name(), "fs-tree-import");
        assert_eq!(functions[2].name(), "fs-tree-materialize");
        assert_eq!(functions[3].name(), "initramfs");
        assert_eq!(functions[4].name(), "oci-extract");
    }
}
