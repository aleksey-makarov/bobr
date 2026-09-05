//! Explicit on-disk cache for authenticated repository metadata.

use crate::RepositoryError;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use url::Url;

/// Cache namespace for one configured master URL.
#[derive(Debug, Clone)]
pub struct RepositoryCache {
    root: PathBuf,
}

impl RepositoryCache {
    /// Opens an explicit cache directory and creates its fixed layout.
    pub fn open(cache_root: &Path, master_url: &Url) -> Result<Self, RepositoryError> {
        let digest: [u8; 32] = Sha256::digest(master_url.as_str().as_bytes()).into();
        let namespace = crate::MetadataHash::<()>::from_bytes(digest).to_string();
        let root = cache_root.join("repositories").join(namespace);
        std::fs::create_dir_all(root.join("metadata"))?;
        Ok(Self { root })
    }

    pub(crate) fn master_path(&self) -> PathBuf {
        self.root.join("master.cose")
    }

    pub(crate) fn master_etag_path(&self) -> PathBuf {
        self.root.join("master.etag")
    }

    pub(crate) fn metadata_path(&self, namespace: &str, hash: &str) -> PathBuf {
        self.root.join("metadata").join(namespace).join(hash)
    }

    pub(crate) fn create_temporary(
        &self,
        prefix: &str,
    ) -> Result<tempfile::NamedTempFile, RepositoryError> {
        Ok(tempfile::Builder::new()
            .prefix(prefix)
            .tempfile_in(&self.root)?)
    }
}
