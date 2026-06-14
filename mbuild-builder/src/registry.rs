use crate::{
    ErofsRootfsBuilder, GroupBuilder, InitramfsBuilder, OciExtractBuilder, TreeBuilder,
    TreeMergeBuilder, TreeSubsetBuilder,
};
use mbuild_core::Builder;
use mbuild_sandbox::SandboxBuilder;
use std::sync::OnceLock;

static SANDBOX_BUILDER: SandboxBuilder = SandboxBuilder;
static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;
static EROFS_ROOTFS_BUILDER: ErofsRootfsBuilder = ErofsRootfsBuilder;
static INITRAMFS_BUILDER: InitramfsBuilder = InitramfsBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 8] {
    [
        &GROUP_BUILDER,
        &TREE_BUILDER,
        &TREE_SUBSET_BUILDER,
        &TREE_MERGE_BUILDER,
        &EROFS_ROOTFS_BUILDER,
        &INITRAMFS_BUILDER,
        &SANDBOX_BUILDER,
        &OCI_EXTRACT_BUILDER,
    ]
}

static REGISTERED_BUILDERS_VALID: OnceLock<Result<(), String>> = OnceLock::new();

pub fn ensure_registered_builders_valid() -> Result<(), String> {
    REGISTERED_BUILDERS_VALID
        .get_or_init(validate_registered_builders)
        .clone()
}

fn validate_registered_builders() -> Result<(), String> {
    for builder in registered_builders() {
        builder.spec().validate_for_builder(builder.tag())?;
    }
    Ok(())
}

pub fn get_builder(tag: &str) -> Option<&'static dyn Builder> {
    registered_builders()
        .iter()
        .find(|builder| builder.tag().eq_ignore_ascii_case(tag))
        .copied()
}

pub fn supported_builder_tags() -> Vec<&'static str> {
    registered_builders()
        .iter()
        .map(|builder| builder.tag())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_input_specs_are_valid() {
        ensure_registered_builders_valid().unwrap();
    }
}
