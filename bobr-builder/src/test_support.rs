//! Shared test helpers.

use bobr_store::Store;
use bobr_store::fs_tree::FsTree;
use std::path::Path;

/// An [`FsTree`] backed by a throwaway store created under `root`.
///
/// Builders that never touch the store still need an [`FsTree`] to construct a
/// [`BuildContext`](crate::BuildContext); this gives them a real one (there is
/// no public `FsTree` constructor — it comes only from a [`Store`]). The store
/// is rooted at `<root>/store`, clear of a builder temp dir at `<root>/tmp`.
pub(crate) fn store_fs_tree(root: &Path) -> FsTree {
    let store_root = root.join("store");
    std::fs::create_dir_all(&store_root).unwrap();
    Store::create(&store_root).unwrap().fs_tree()
}
