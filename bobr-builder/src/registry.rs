use crate::{
    Builder, BundleBuilder, FsTreeExportBuilder, FsTreeImportBuilder, GroupBuilder,
    HostBundleBuilder, OciExtractBuilder, TreeBuilder, TreeMergeBuilder, TreeMoveBuilder,
    TreeSubsetBuilder,
};

static GROUP_BUILDER: GroupBuilder = GroupBuilder;
static BUNDLE_BUILDER: BundleBuilder = BundleBuilder;
static FS_TREE_IMPORT_BUILDER: FsTreeImportBuilder = FsTreeImportBuilder;
static FS_TREE_EXPORT_BUILDER: FsTreeExportBuilder = FsTreeExportBuilder;
static HOST_BUNDLE_BUILDER: HostBundleBuilder = HostBundleBuilder;
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
    &HOST_BUNDLE_BUILDER,
    &TREE_BUILDER,
    &TREE_SUBSET_BUILDER,
    &TREE_MOVE_BUILDER,
    &TREE_MERGE_BUILDER,
    &OCI_EXTRACT_BUILDER,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn registry_contains_one_verified_host_bundle_builder() {
        let matches = BUILDERS
            .iter()
            .filter(|builder| builder.tag() == "HostBundle")
            .collect::<Vec<_>>();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].spec().required_inputs, &["_root", "_launcher"]);
        assert_eq!(matches[0].spec().optional_inputs, &["overrides"]);
        assert_eq!(matches[0].impl_version(), "2");
    }

    #[test]
    fn registered_builder_tags_are_unique() {
        let mut tags = BTreeSet::new();
        for builder in BUILDERS {
            assert!(
                tags.insert(builder.tag()),
                "duplicate registered builder tag '{}'",
                builder.tag()
            );
        }
    }
}
