pub mod builder;
mod fs_tree;
pub mod group;
pub mod oci_extract;
mod registry;
pub mod subject;
pub mod tree;

pub use builder::*;
pub use fs_tree::{
    ErofsRootfsBuilder, ErofsRootfsConfig, InitramfsBuilder, InitramfsConfig, TreeBuilder,
    TreeConfig, TreeMergeBuilder, TreeMergeConfig, TreeSubsetBuilder, TreeSubsetConfig,
};
pub use group::{GroupBuilder, GroupConfig};
pub use oci_extract::{OciExtractBuilder, OciExtractConfig};
pub use registry::{BuilderRegistry, register_in_tree_builders};
pub use subject::{BuilderPlanError, BuilderPlannedSubject};

/// Return runtime functions supported by built-in builders.
///
/// The registry is intentionally empty until a builder needs a namespace-backed
/// runtime function. `mbuild` still routes worker invocations through this
/// function so new runtime functions can be registered in one place.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_function_registry_is_empty_until_builders_register_functions() {
        assert!(crate::runtime_functions().is_empty());
    }
}
