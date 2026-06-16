pub mod builder;
pub mod fs_tree;
mod fs_tree_import;
mod fs_tree_legacy;
pub mod group;
mod oci_extract;
mod oci_extract_legacy;
mod registry;
pub mod subject;
pub mod tree;

pub use builder::*;
pub use fs_tree::{
    ErofsRootfsBuilder, ErofsRootfsConfig, InitramfsBuilder, InitramfsConfig, TreeBuilder,
    TreeConfig, TreeMergeBuilder, TreeMergeConfig, TreeSubsetBuilder, TreeSubsetConfig,
};
pub use fs_tree_import::{FsTreeImportBuilder, FsTreeImportConfig};
pub use group::{GroupBuilder, GroupConfig};
pub use oci_extract::{OciExtractNewBuilder, OciExtractNewConfig};
pub use oci_extract_legacy::{OciExtractBuilder, OciExtractConfig};
pub use registry::{BuilderRegistry, register_in_tree_builders};
pub use subject::{BuilderPlanError, BuilderPlannedSubject};
pub use tree::{TreeNewBuilder, TreeNewConfig};

/// Return runtime functions supported by built-in builders.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![
        bobr_runtime::runtime_ns::NsFunction::new(fs_tree_import::FsTreeImportFunction),
        bobr_runtime::runtime_ns::NsFunction::new(oci_extract::OciExtractFunction),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_function_registry_includes_fs_tree_import() {
        let functions = crate::runtime_functions();

        assert_eq!(functions.len(), 2);
        assert_eq!(functions[0].name(), "fs-tree-import");
        assert_eq!(functions[1].name(), "oci-extract");
    }
}
