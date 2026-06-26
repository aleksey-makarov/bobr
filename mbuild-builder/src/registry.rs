use crate::{
    Builder, ErofsRootfsBuilder, FsTreeImportBuilder, GroupBuilder, InitramfsBuilder,
    OciExtractBuilder, TreeBuilder, TreeMergeBuilder, TreeSubsetBuilder,
};

static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static FS_TREE_IMPORT_BUILDER: FsTreeImportBuilder = FsTreeImportBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;
static EROFS_ROOTFS_BUILDER: ErofsRootfsBuilder = ErofsRootfsBuilder;
static INITRAMFS_BUILDER: InitramfsBuilder = InitramfsBuilder;

/// Builder classes provided in-tree by this crate.
pub static BUILDERS: &[&'static dyn Builder] = &[
    &GROUP_BUILDER,
    &FS_TREE_IMPORT_BUILDER,
    &TREE_BUILDER,
    &TREE_SUBSET_BUILDER,
    &TREE_MERGE_BUILDER,
    &EROFS_ROOTFS_BUILDER,
    &INITRAMFS_BUILDER,
    &OCI_EXTRACT_BUILDER,
];
