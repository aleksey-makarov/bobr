use mbuild_binary::BinaryBuilder;
use mbuild_compose::Ext4RootfsBuilder;
use mbuild_core::Builder;
use mbuild_image::{ContainerImageBuilder, ImageBuilder};
use mbuild_text::TextBuilder;
use mbuild_tree::TreeBuilder;

static CONTAINER_IMAGE_BUILDER: ContainerImageBuilder = ContainerImageBuilder;
static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;
static EXT4_ROOTFS_BUILDER: Ext4RootfsBuilder = Ext4RootfsBuilder;
static IMAGE_BUILDER: ImageBuilder = ImageBuilder;
static TEXT_BUILDER: TextBuilder = TextBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 6] {
    [
        &TEXT_BUILDER,
        &TREE_BUILDER,
        &CONTAINER_IMAGE_BUILDER,
        &BINARY_BUILDER,
        &EXT4_ROOTFS_BUILDER,
        &IMAGE_BUILDER,
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
