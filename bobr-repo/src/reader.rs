//! Authenticated immutable repository snapshots.

use crate::{
    BuildIndex, BuildIndexHash, CurrentPublication, FetchRequest, FetchResult, FsFileList,
    FsFileListHash, HttpTransport, MAX_ENCODED_CONTENT_BYTES, MAX_MASTER_BYTES, MAX_METADATA_BYTES,
    Master, ObjectKind, ObjectList, ObjectListHash, RepositoryCache, RepositoryError,
    RepositoryTransport, ReuseIndex, ReuseIndexHash, Slot, TrustedKeys, VerifiedMaster,
    decode_fs_file, decode_object,
};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_store::fs_tree::FsFileHash;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{OnceCell, Semaphore};
use url::Url;

const OCTET_STREAM: &str = "application/octet-stream";
const OBJECT_MEDIA_TYPE: &str = "application/vnd.bobr.repository-object+cbor";
const FS_FILE_MEDIA_TYPE: &str = "application/vnd.bobr.repository-fs-file+cbor";

/// Configured authenticated repository reader.
#[derive(Clone)]
pub struct RepositoryReader {
    inner: Arc<ReaderInner>,
}

struct ReaderInner {
    master_url: Url,
    trusted_keys: TrustedKeys,
    cache: RepositoryCache,
    transport: Arc<dyn RepositoryTransport>,
    blocking_decoders: Arc<Semaphore>,
}

/// Runtime policy for local repository-reader work.
///
/// These values do not affect the repository wire format or whether remote
/// data is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderPolicy {
    /// Maximum number of simultaneous synchronous content decoders.
    pub max_blocking_decoders: usize,
}

impl Default for ReaderPolicy {
    fn default() -> Self {
        Self {
            max_blocking_decoders: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        }
    }
}

/// One immutable, internally consistent view selected by a verified master.
pub struct RepositorySnapshot {
    reader: RepositoryReader,
    master: Master,
    slots: Vec<SnapshotSlot>,
}

impl std::fmt::Debug for RepositoryReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryReader")
            .field("master_url", &self.inner.master_url)
            .field("cache", &self.inner.cache)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RepositorySnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositorySnapshot")
            .field("master", &self.master)
            .field("current_slot_count", &self.slots.len())
            .finish_non_exhaustive()
    }
}

struct SnapshotSlot {
    descriptor: Slot,
    build: Arc<BuildIndex>,
    reuse: OnceCell<Arc<ReuseIndex>>,
    objects: Arc<ObjectList>,
    files: Arc<FsFileList>,
}

impl RepositoryReader {
    /// Creates a reader using an explicit metadata cache and transport.
    pub fn new(
        master_url: Url,
        trusted_keys: TrustedKeys,
        cache_root: &Path,
        transport: Arc<dyn RepositoryTransport>,
    ) -> Result<Self, RepositoryError> {
        Self::with_policy(
            master_url,
            trusted_keys,
            cache_root,
            transport,
            ReaderPolicy::default(),
        )
    }

    /// Creates a reader with explicit runtime scheduling policy.
    pub fn with_policy(
        master_url: Url,
        trusted_keys: TrustedKeys,
        cache_root: &Path,
        transport: Arc<dyn RepositoryTransport>,
        policy: ReaderPolicy,
    ) -> Result<Self, RepositoryError> {
        if master_url.scheme() != "https" {
            return Err(RepositoryError::new("master URL must use HTTPS"));
        }
        if policy.max_blocking_decoders == 0 {
            return Err(RepositoryError::new(
                "max_blocking_decoders must be greater than zero",
            ));
        }
        let cache = RepositoryCache::open(cache_root, &master_url)?;
        Ok(Self {
            inner: Arc::new(ReaderInner {
                master_url,
                trusted_keys,
                cache,
                transport,
                blocking_decoders: Arc::new(Semaphore::new(policy.max_blocking_decoders)),
            }),
        })
    }

    /// Creates an anonymous HTTPS reader with default network policy.
    pub fn https(
        master_url: Url,
        trusted_keys: TrustedKeys,
        cache_root: &Path,
    ) -> Result<Self, RepositoryError> {
        Self::new(
            master_url,
            trusted_keys,
            cache_root,
            Arc::new(HttpTransport::anonymous()?),
        )
    }

    /// Creates an HTTPS reader using a caller-configured HTTP client.
    pub fn https_with_client(
        master_url: Url,
        trusted_keys: TrustedKeys,
        cache_root: &Path,
        client: reqwest::Client,
    ) -> Result<Self, RepositoryError> {
        Self::new(
            master_url,
            trusted_keys,
            cache_root,
            Arc::new(HttpTransport::new(client)),
        )
    }

    /// Revalidates `/master` and loads the current slot metadata it authenticates.
    pub async fn snapshot(&self) -> Result<RepositorySnapshot, RepositoryError> {
        let master = self.fetch_master().await?.master;
        let mut slots = Vec::new();
        for descriptor in master.current_slots_newest_first() {
            let build = Arc::new(
                self.fetch_build(master.data_base_url(), descriptor.build)
                    .await?,
            );
            let objects = Arc::new(
                self.fetch_object_list(master.data_base_url(), descriptor.object_list)
                    .await?,
            );
            let files = Arc::new(
                self.fetch_file_list(master.data_base_url(), descriptor.file_list)
                    .await?,
            );
            slots.push(SnapshotSlot {
                descriptor: descriptor.clone(),
                build,
                reuse: OnceCell::new(),
                objects,
                files,
            });
        }
        Ok(RepositorySnapshot {
            reader: self.clone(),
            master,
            slots,
        })
    }

    /// Loads every current and retained metadata object needed by a publisher
    /// or garbage collector to reconstruct durable repository state.
    pub async fn publication_metadata(&self) -> Result<CurrentPublication, RepositoryError> {
        let verified = self.fetch_master().await?;
        let master_hash = verified.signed_hash;
        let master = verified.master;
        let mut builds = std::collections::BTreeMap::new();
        let mut reuses = std::collections::BTreeMap::new();
        let mut object_lists = std::collections::BTreeMap::new();
        let mut file_lists = std::collections::BTreeMap::new();
        for slot in master.slots() {
            let build = self.fetch_build(master.data_base_url(), slot.build).await?;
            let reuse = self.fetch_reuse(master.data_base_url(), slot.reuse).await?;
            let objects = self
                .fetch_object_list(master.data_base_url(), slot.object_list)
                .await?;
            let files = self
                .fetch_file_list(master.data_base_url(), slot.file_list)
                .await?;
            builds.insert(slot.build, build.as_bytes().to_vec());
            reuses.insert(slot.reuse, reuse.as_bytes().to_vec());
            object_lists.insert(slot.object_list, objects.as_bytes().to_vec());
            file_lists.insert(slot.file_list, files.as_bytes().to_vec());
        }
        Ok(CurrentPublication {
            master_hash,
            metadata: crate::PublicationMetadata {
                master,
                builds,
                reuses,
                object_lists,
                file_lists,
            },
        })
    }

    async fn fetch_master(&self) -> Result<VerifiedMaster, RepositoryError> {
        let cache = &self.inner.cache;
        let cached_path = cache.master_path();
        let etag = std::fs::read_to_string(cache.master_etag_path()).ok();
        let temporary = cache.create_temporary("master-")?;
        let result = self
            .inner
            .transport
            .fetch(FetchRequest {
                url: self.inner.master_url.clone(),
                destination: temporary.path().to_path_buf(),
                max_bytes: MAX_MASTER_BYTES,
                if_none_match: etag,
            })
            .await?;
        let bytes = match result {
            FetchResult::Stored(metadata) => {
                require_master_representation(&metadata)?;
                let bytes = std::fs::read(temporary.path())?;
                self.inner.trusted_keys.verify(&bytes)?;
                persist_replace(temporary, &cached_path)?;
                match metadata.etag {
                    Some(etag) => atomic_write(cache.master_etag_path(), etag.as_bytes())?,
                    None => remove_if_exists(&cache.master_etag_path())?,
                }
                bytes
            }
            FetchResult::NotModified => read_cached_master(&cached_path).map_err(|error| {
                RepositoryError::new(format!(
                    "origin returned 304 but cached master is unavailable: {error}"
                ))
            })?,
            FetchResult::Missing => {
                return Err(RepositoryError::new("repository master is missing"));
            }
        };
        self.inner.trusted_keys.verify(&bytes)
    }

    async fn fetch_build(
        &self,
        base: &Url,
        hash: BuildIndexHash,
    ) -> Result<BuildIndex, RepositoryError> {
        let path = self
            .fetch_metadata(base, "b", hash.to_string(), |path| {
                BuildIndex::open(path, hash).map(|_| ())
            })
            .await?;
        BuildIndex::open(&path, hash)
    }

    async fn fetch_reuse(
        &self,
        base: &Url,
        hash: ReuseIndexHash,
    ) -> Result<ReuseIndex, RepositoryError> {
        let path = self
            .fetch_metadata(base, "r", hash.to_string(), |path| {
                ReuseIndex::open(path, hash).map(|_| ())
            })
            .await?;
        ReuseIndex::open(&path, hash)
    }

    async fn fetch_object_list(
        &self,
        base: &Url,
        hash: ObjectListHash,
    ) -> Result<ObjectList, RepositoryError> {
        let path = self
            .fetch_metadata(base, "lo", hash.to_string(), |path| {
                ObjectList::open(path, hash).map(|_| ())
            })
            .await?;
        ObjectList::open(&path, hash)
    }

    async fn fetch_file_list(
        &self,
        base: &Url,
        hash: FsFileListHash,
    ) -> Result<FsFileList, RepositoryError> {
        let path = self
            .fetch_metadata(base, "lf", hash.to_string(), |path| {
                FsFileList::open(path, hash).map(|_| ())
            })
            .await?;
        FsFileList::open(&path, hash)
    }

    async fn fetch_metadata(
        &self,
        base: &Url,
        namespace: &str,
        hash: String,
        validate: impl Fn(&Path) -> Result<(), RepositoryError>,
    ) -> Result<PathBuf, RepositoryError> {
        let path = self.inner.cache.metadata_path(namespace, &hash);
        if path.is_file() && validate(&path).is_ok() {
            return Ok(path);
        }
        remove_if_exists(&path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.inner.cache.create_temporary("metadata-")?;
        let result = self
            .inner
            .transport
            .fetch(FetchRequest {
                url: base.join(&format!("{namespace}/{hash}")).map_err(|error| {
                    RepositoryError::new(format!("failed to construct repository URL: {error}"))
                })?,
                destination: temporary.path().to_path_buf(),
                max_bytes: MAX_METADATA_BYTES,
                if_none_match: None,
            })
            .await?;
        match result {
            FetchResult::Stored(metadata) => {
                require_immutable_representation(&metadata, OCTET_STREAM)?;
                persist_noclobber(temporary, &path)?;
                validate(&path)?;
                Ok(path)
            }
            FetchResult::Missing => Err(RepositoryError::new(format!(
                "repository metadata '{namespace}/{hash}' is missing"
            ))),
            FetchResult::NotModified => Err(RepositoryError::new(
                "unexpected 304 response for uncached immutable metadata",
            )),
        }
    }

    async fn fetch_content(
        &self,
        master: &Master,
        namespace: &str,
        hash: &str,
        media_type: &str,
    ) -> Result<tempfile::NamedTempFile, RepositoryError> {
        let temporary = self.inner.cache.create_temporary("content-")?;
        let result = self
            .inner
            .transport
            .fetch(FetchRequest {
                url: data_url_for(master, namespace, hash)?,
                destination: temporary.path().to_path_buf(),
                max_bytes: MAX_ENCODED_CONTENT_BYTES,
                if_none_match: None,
            })
            .await?;
        match result {
            FetchResult::Stored(metadata) => {
                require_immutable_representation(&metadata, media_type)?;
                Ok(temporary)
            }
            FetchResult::Missing => Err(RepositoryError::new(format!(
                "repository advertises unavailable content '{namespace}/{hash}'"
            ))),
            FetchResult::NotModified => Err(RepositoryError::new(
                "unexpected 304 response for uncached repository content",
            )),
        }
    }
}

impl RepositorySnapshot {
    /// Returns the authenticated logical master for this immutable snapshot.
    pub fn master(&self) -> &Master {
        &self.master
    }

    /// Returns ordered distinct build candidates across current slots.
    pub fn build_candidates(&self, key: BuildKey) -> Vec<ObjectHash> {
        combine_candidates(self.slots.iter().map(|slot| slot.build.candidates(key)))
    }

    /// Lazily fetches reuse indexes and returns ordered distinct candidates.
    pub async fn reuse_candidates(
        &self,
        key: ReuseKey,
    ) -> Result<Vec<ObjectHash>, RepositoryError> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for slot in &self.slots {
            let index = slot
                .reuse
                .get_or_try_init(|| async {
                    self.reader
                        .fetch_reuse(self.master.data_base_url(), slot.descriptor.reuse)
                        .await
                        .map(Arc::new)
                })
                .await?;
            for candidate in index.candidates(key) {
                if seen.insert(candidate) {
                    candidates.push(candidate);
                }
            }
        }
        Ok(candidates)
    }

    /// Returns whether current slot lists advertise an ordinary object.
    pub fn contains_object(&self, hash: ObjectHash) -> bool {
        self.slots.iter().any(|slot| slot.objects.contains(hash))
    }

    /// Returns whether current slot lists advertise a filesystem file.
    pub fn contains_fs_file(&self, hash: FsFileHash) -> bool {
        self.slots.iter().any(|slot| slot.files.contains(hash))
    }

    /// Downloads and verifies one advertised ordinary object.
    pub async fn fetch_object(
        &self,
        hash: ObjectHash,
        destination: &Path,
    ) -> Result<Option<ObjectKind>, RepositoryError> {
        if !self.contains_object(hash) {
            return Ok(None);
        }
        let encoded = self
            .reader
            .fetch_content(&self.master, "o", &hash.to_string(), OBJECT_MEDIA_TYPE)
            .await?;
        let encoded_path = encoded.path().to_path_buf();
        let destination = destination.to_path_buf();
        let permit = self
            .reader
            .inner
            .blocking_decoders
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RepositoryError::new("repository decoder semaphore is closed"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_object(&encoded_path, hash, &destination)
        })
        .await
        .map_err(|error| RepositoryError::new(format!("object decoder task failed: {error}")))?
        .map(Some)
    }

    /// Downloads and verifies one advertised filesystem file.
    pub async fn fetch_fs_file(
        &self,
        hash: FsFileHash,
        destination: &Path,
    ) -> Result<Option<crate::FsFileMetadata>, RepositoryError> {
        if !self.contains_fs_file(hash) {
            return Ok(None);
        }
        let encoded = self
            .reader
            .fetch_content(&self.master, "f", &hash.to_string(), FS_FILE_MEDIA_TYPE)
            .await?;
        let encoded_path = encoded.path().to_path_buf();
        let destination = destination.to_path_buf();
        let permit = self
            .reader
            .inner
            .blocking_decoders
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RepositoryError::new("repository decoder semaphore is closed"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_fs_file(&encoded_path, hash, &destination)
        })
        .await
        .map_err(|error| RepositoryError::new(format!("fs-file decoder task failed: {error}")))?
        .map(Some)
    }
}

fn combine_candidates(iter: impl IntoIterator<Item = Vec<ObjectHash>>) -> Vec<ObjectHash> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for group in iter {
        for candidate in group {
            if seen.insert(candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn data_url_for(master: &Master, namespace: &str, hash: &str) -> Result<Url, RepositoryError> {
    master
        .data_base_url()
        .join(&format!("{namespace}/{hash}"))
        .map_err(|error| {
            RepositoryError::new(format!("failed to construct repository URL: {error}"))
        })
}

fn require_master_representation(
    metadata: &crate::RepresentationMetadata,
) -> Result<(), RepositoryError> {
    let mut content_type = metadata
        .content_type
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .map(str::trim);
    if !content_type
        .next()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/cose"))
        || content_type.next() != Some("cose-type=\"cose-sign1\"")
        || content_type.next().is_some()
    {
        return Err(RepositoryError::new(
            "master has an invalid HTTP Content-Type",
        ));
    }
    if !metadata
        .cache_control
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .any(|item| item.trim().eq_ignore_ascii_case("no-cache"))
    {
        return Err(RepositoryError::new("master lacks Cache-Control: no-cache"));
    }
    Ok(())
}

fn require_immutable_representation(
    metadata: &crate::RepresentationMetadata,
    media_type: &str,
) -> Result<(), RepositoryError> {
    if metadata.content_encoding.is_some() {
        return Err(RepositoryError::new(
            "immutable repository response must not use Content-Encoding",
        ));
    }
    if !metadata
        .content_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(media_type))
    {
        return Err(RepositoryError::new(
            "immutable object has an invalid HTTP Content-Type",
        ));
    }
    let cache_control = metadata.cache_control.as_deref().unwrap_or_default();
    if !cache_control
        .split(',')
        .any(|item| item.trim().eq_ignore_ascii_case("immutable"))
    {
        return Err(RepositoryError::new(
            "immutable object lacks Cache-Control: immutable",
        ));
    }
    if !cache_control.split(',').map(str::trim).any(|item| {
        item.to_ascii_lowercase()
            .strip_prefix("max-age=")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|seconds| seconds >= 31_536_000)
    }) {
        return Err(RepositoryError::new(
            "immutable object lacks a one-year Cache-Control max-age",
        ));
    }
    Ok(())
}

fn persist_noclobber(file: tempfile::NamedTempFile, path: &Path) -> Result<(), RepositoryError> {
    match file.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

fn persist_replace(file: tempfile::NamedTempFile, path: &Path) -> Result<(), RepositoryError> {
    file.persist(path)
        .map(|_| ())
        .map_err(|error| error.error.into())
}

fn atomic_write(path: PathBuf, bytes: &[u8]) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::new("cache path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, bytes)?;
    persist_replace(file, &path)
}

fn remove_if_exists(path: &Path) -> Result<(), RepositoryError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_cached_master(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    if std::fs::metadata(path)?.len() > MAX_MASTER_BYTES {
        return Err(RepositoryError::new(
            "cached master exceeds the format size limit",
        ));
    }
    Ok(std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Compression, MasterHash, MemoryTransport, Slot, TrustedKeys, encode_object};
    use ed25519_dalek::SigningKey;

    #[tokio::test]
    async fn snapshot_looks_up_and_fetches_advertised_object() {
        let signing = SigningKey::from_bytes(&[4; 32]);
        let base = Url::parse("https://data.example/").unwrap();
        let master_url = Url::parse("https://master.example/master").unwrap();
        let transport = MemoryTransport::default();
        let temp = tempfile::tempdir().unwrap();

        let source = temp.path().join("source");
        let encoded = temp.path().join("object.cbor");
        std::fs::write(&source, b"repository object").unwrap();
        let object_hash = encode_object(&source, Compression::Zstd, &encoded).unwrap();
        let build_key = BuildKey::from_bytes([1; 32]);
        let reuse_key = ReuseKey::from_bytes([2; 32]);
        let build_bytes = BuildIndex::encode(&[(build_key, object_hash)]).unwrap();
        let reuse_bytes = ReuseIndex::encode(&[(reuse_key, object_hash)]).unwrap();
        let object_list_bytes = ObjectList::encode([object_hash]);
        let file_list_bytes = FsFileList::encode([]);
        let build_hash = BuildIndexHash::digest(&build_bytes);
        let reuse_hash = ReuseIndexHash::digest(&reuse_bytes);
        let object_list_hash = ObjectListHash::digest(&object_list_bytes);
        let file_list_hash = FsFileListHash::digest(&file_list_bytes);
        let master = Master::new(
            None,
            base.clone(),
            vec![Slot {
                serial: 1,
                build: build_hash,
                reuse: reuse_hash,
                object_list: object_list_hash,
                file_list: file_list_hash,
                retain_until: None,
            }],
        )
        .unwrap();
        let signed_master = master.sign(b"key", &signing).unwrap();
        let signed_master_hash = MasterHash::digest(&signed_master);
        transport.insert(
            master_url.clone(),
            signed_master,
            "application/cose; cose-type=\"cose-sign1\"",
            "no-cache",
        );
        for (namespace, hash, bytes) in [
            ("b", build_hash.to_string(), build_bytes),
            ("r", reuse_hash.to_string(), reuse_bytes),
            ("lo", object_list_hash.to_string(), object_list_bytes),
            ("lf", file_list_hash.to_string(), file_list_bytes),
        ] {
            transport.insert(
                base.join(&format!("{namespace}/{hash}")).unwrap(),
                bytes,
                OCTET_STREAM,
                "public, max-age=31536000, immutable",
            );
        }
        transport.insert(
            base.join(&format!("o/{object_hash}")).unwrap(),
            std::fs::read(encoded).unwrap(),
            OBJECT_MEDIA_TYPE,
            "public, max-age=31536000, immutable",
        );
        let keys = TrustedKeys::new([(b"key".to_vec(), signing.verifying_key())]).unwrap();
        let reader = RepositoryReader::new(
            master_url,
            keys,
            &temp.path().join("cache"),
            Arc::new(transport.clone()),
        )
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let reuse_url = base.join(&format!("r/{reuse_hash}")).unwrap();
        assert_eq!(transport.request_count(&reuse_url), 0);
        assert_eq!(snapshot.build_candidates(build_key), vec![object_hash]);
        assert_eq!(
            snapshot.reuse_candidates(reuse_key).await.unwrap(),
            vec![object_hash]
        );
        assert_eq!(transport.request_count(&reuse_url), 1);
        let destination = temp.path().join("decoded");
        assert!(
            snapshot
                .fetch_object(object_hash, &destination)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"repository object");
        assert!(
            snapshot
                .fetch_object(ObjectHash::from_bytes([9; 32]), &temp.path().join("absent"))
                .await
                .unwrap()
                .is_none()
        );

        let cached_build = reader
            .inner
            .cache
            .metadata_path("b", &build_hash.to_string());
        std::fs::write(&cached_build, b"corrupt cache entry").unwrap();
        let refreshed = reader.snapshot().await.unwrap();
        assert_eq!(refreshed.build_candidates(build_key), vec![object_hash]);

        let current = reader.publication_metadata().await.unwrap();
        assert_eq!(current.master_hash, signed_master_hash);
        assert_eq!(
            current
                .state()
                .unwrap()
                .metadata()
                .unwrap()
                .master
                .previous_master_hash(),
            Some(signed_master_hash)
        );
    }

    #[test]
    fn candidates_preserve_slot_order_and_deduplicate() {
        let first = ObjectHash::from_bytes([1; 32]);
        let second = ObjectHash::from_bytes([2; 32]);
        let third = ObjectHash::from_bytes([3; 32]);
        assert_eq!(
            combine_candidates([vec![first, second], vec![second, third]]),
            vec![first, second, third]
        );
    }
}
