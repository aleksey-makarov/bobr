use mbuild_binary::{BinaryBuilder, ContainerBuilder, SandboxBuilder};
use mbuild_compose::{Ext4RootfsBuilder, RootfsBuilder};
use mbuild_core::Builder;
use mbuild_image::{ImageBuilder, OciExtractBuilder};
use mbuild_text::TextBuilder;
use mbuild_tree::{TreeBuilder, TreeMergeBuilder};

static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;
static CONTAINER_BUILDER: ContainerBuilder = ContainerBuilder;
static SANDBOX_BUILDER: SandboxBuilder = SandboxBuilder;
static EXT4_ROOTFS_BUILDER: Ext4RootfsBuilder = Ext4RootfsBuilder;
static ROOTFS_BUILDER: RootfsBuilder = RootfsBuilder;
static IMAGE_BUILDER: ImageBuilder = ImageBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TEXT_BUILDER: TextBuilder = TextBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 10] {
    [
        &TEXT_BUILDER,
        &TREE_BUILDER,
        &TREE_MERGE_BUILDER,
        &BINARY_BUILDER,
        &CONTAINER_BUILDER,
        &SANDBOX_BUILDER,
        &EXT4_ROOTFS_BUILDER,
        &ROOTFS_BUILDER,
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
