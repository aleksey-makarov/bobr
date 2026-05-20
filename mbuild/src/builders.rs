use mbuild_binary::SandboxBuilder;
use mbuild_core::Builder;
use mbuild_group::GroupBuilder;
use mbuild_image::{ImageBuilder, OciExtractBuilder};
use mbuild_text::TextBuilder;
use mbuild_tree::{ErofsRootfsBuilder, TreeBuilder, TreeMergeBuilder, TreeSubsetBuilder};

static SANDBOX_BUILDER: SandboxBuilder = SandboxBuilder;
static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static IMAGE_BUILDER: ImageBuilder = ImageBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TEXT_BUILDER: TextBuilder = TextBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;
static EROFS_ROOTFS_BUILDER: ErofsRootfsBuilder = ErofsRootfsBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 9] {
    [
        &TEXT_BUILDER,
        &GROUP_BUILDER,
        &TREE_BUILDER,
        &TREE_SUBSET_BUILDER,
        &TREE_MERGE_BUILDER,
        &EROFS_ROOTFS_BUILDER,
        &SANDBOX_BUILDER,
        &IMAGE_BUILDER,
        &OCI_EXTRACT_BUILDER,
    ]
}

pub fn get_builder(tag: &str) -> Option<&'static dyn Builder> {
    registered_builders()
        .iter()
        .find(|builder| builder.spec().tag.eq_ignore_ascii_case(tag))
        .copied()
}

pub fn supported_builder_tags() -> Vec<&'static str> {
    registered_builders()
        .iter()
        .map(|builder| builder.spec().tag)
        .collect()
}
