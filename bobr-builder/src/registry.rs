use crate::{
    Builder, BundleBuilder, FsTreeExportBuilder, FsTreeImportBuilder, GroupBuilder,
    OciExtractBuilder, TreeBuilder, TreeMergeBuilder, TreeMoveBuilder, TreeSubsetBuilder,
};

static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static BUNDLE_BUILDER: BundleBuilder = BundleBuilder;
static FS_TREE_IMPORT_BUILDER: FsTreeImportBuilder = FsTreeImportBuilder;
static FS_TREE_EXPORT_BUILDER: FsTreeExportBuilder = FsTreeExportBuilder;
static OCI_EXTRACT_BUILDER: OciExtractBuilder = OciExtractBuilder;
static TREE_BUILDER: TreeBuilder = TreeBuilder;
static TREE_SUBSET_BUILDER: TreeSubsetBuilder = TreeSubsetBuilder;
static TREE_MOVE_BUILDER: TreeMoveBuilder = TreeMoveBuilder;
static TREE_MERGE_BUILDER: TreeMergeBuilder = TreeMergeBuilder;

/// Builder classes provided in-tree by this crate.
pub static BUILDERS: &[&'static dyn Builder] = &[
    &GROUP_BUILDER,
    &BUNDLE_BUILDER,
    &FS_TREE_IMPORT_BUILDER,
    &FS_TREE_EXPORT_BUILDER,
    &TREE_BUILDER,
    &TREE_SUBSET_BUILDER,
    &TREE_MOVE_BUILDER,
    &TREE_MERGE_BUILDER,
    &OCI_EXTRACT_BUILDER,
];
