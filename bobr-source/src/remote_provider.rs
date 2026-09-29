//! Remote repository backends exposed through secondary capabilities.

use crate::{ContentProvider, LocalIoScheduler, MappingProvider, ProviderError};
use async_trait::async_trait;
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_repo::{
    FetchedFsFileRepresentation, RepositoryError, RepositoryReader, RepositorySnapshot,
    publish_fs_file_batch,
};
use bobr_runtime::runtime_provider::RuntimeProvider;
use bobr_store::fs_tree::{FsFileHash, FsTreeManifest, read_manifest_if_marked};
use bobr_store::{
    ContentImportOutcome, ContentTransferMode, RepositoryObjectStaging, Store, StoreError,
    TrustedResolution,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tokio::task::JoinSet;

const FS_FILE_IMPORT_BATCH_SIZE: usize = 256;

/// One authenticated remote repository shared by complementary capabilities.
#[derive(Clone)]
pub struct RemoteRepositoryBackend {
    inner: Arc<RemoteRepositoryBackendInner>,
}

struct RemoteRepositoryBackendInner {
    identity: String,
    reader: RepositoryReader,
    snapshot: OnceCell<Arc<RepositorySnapshot>>,
}

impl fmt::Debug for RemoteRepositoryBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteRepositoryBackend")
            .field("identity", &self.inner.identity)
            .field("snapshot_initialized", &self.inner.snapshot.initialized())
            .finish()
    }
}

impl RemoteRepositoryBackend {
    /// Creates one unopened per-run backend.
    pub fn new(identity: impl Into<String>, reader: RepositoryReader) -> Self {
        Self {
            inner: Arc::new(RemoteRepositoryBackendInner {
                identity: identity.into(),
                reader,
                snapshot: OnceCell::new(),
            }),
        }
    }

    /// Returns the stable diagnostic identity of this backend.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// Lazily authenticates the master and freezes one snapshot for this run.
    pub async fn snapshot(&self) -> Result<Arc<RepositorySnapshot>, RepositoryError> {
        self.inner
            .snapshot
            .get_or_try_init(|| async { self.inner.reader.snapshot().await.map(Arc::new) })
            .await
            .cloned()
    }
}

/// Per-run registry deduplicating physical remote repository backends.
#[derive(Debug, Default)]
pub struct RemoteBackendRegistry {
    repositories: HashMap<String, RemoteRepositoryBackend>,
}

impl RemoteBackendRegistry {
    /// Opens or reuses one backend without invoking `create` on a cache hit.
    pub fn open_with<F>(
        &mut self,
        identity: impl Into<String>,
        create: F,
    ) -> Result<RemoteRepositoryBackend, RepositoryError>
    where
        F: FnOnce() -> Result<RepositoryReader, RepositoryError>,
    {
        let identity = identity.into();
        if let Some(repository) = self.repositories.get(&identity) {
            return Ok(repository.clone());
        }
        let repository = RemoteRepositoryBackend::new(identity.clone(), create()?);
        self.repositories.insert(identity, repository.clone());
        Ok(repository)
    }

    /// Returns the number of distinct remote backends opened in this run.
    pub fn len(&self) -> usize {
        self.repositories.len()
    }

    /// Returns whether no remote backend has been opened.
    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }
}

/// Authoritative build/reuse mappings from one signed remote repository.
#[derive(Debug, Clone)]
pub struct RemoteMappingProvider {
    backend: RemoteRepositoryBackend,
}

impl RemoteMappingProvider {
    /// Exposes the mapping capability of one remote backend.
    pub fn new(backend: RemoteRepositoryBackend) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl MappingProvider for RemoteMappingProvider {
    async fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError> {
        let snapshot = self.backend.snapshot().await?;
        let mut resolutions = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if !seen.insert(*key) {
                continue;
            }
            for object_hash in snapshot.build_candidates(*key).await? {
                resolutions.push(TrustedResolution {
                    key: *key,
                    object_hash,
                });
            }
        }
        Ok(resolutions)
    }

    async fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError> {
        let snapshot = self.backend.snapshot().await?;
        let mut resolutions = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if !seen.insert(*key) {
                continue;
            }
            for object_hash in snapshot.reuse_candidates(*key).await? {
                resolutions.push(TrustedResolution {
                    key: *key,
                    object_hash,
                });
            }
        }
        Ok(resolutions)
    }
}

#[derive(Debug, Default)]
struct StagedObjectState {
    staging: Option<RepositoryObjectStaging>,
}

/// Self-verifying object and fs-file content from one remote repository.
#[derive(Clone)]
pub struct RemoteContentProvider {
    backend: RemoteRepositoryBackend,
    working: Store,
    runtime: RuntimeProvider,
    local_io: LocalIoScheduler,
    staged_objects: Arc<Mutex<HashMap<ObjectHash, Arc<AsyncMutex<StagedObjectState>>>>>,
}

impl fmt::Debug for RemoteContentProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteContentProvider")
            .field("backend", &self.backend)
            .field("working", &self.working.root())
            .finish_non_exhaustive()
    }
}

impl RemoteContentProvider {
    /// Exposes the content capability of one remote backend.
    pub fn new(
        backend: RemoteRepositoryBackend,
        working: Store,
        runtime: RuntimeProvider,
        local_io: LocalIoScheduler,
    ) -> Self {
        Self {
            backend,
            working,
            runtime,
            local_io,
            staged_objects: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn staged_object(
        &self,
        hash: ObjectHash,
    ) -> Result<Arc<AsyncMutex<StagedObjectState>>, ProviderError> {
        let mut objects = self.staged_objects.lock().map_err(|_| {
            StoreError::Io("remote object staging registry is poisoned".to_string())
        })?;
        Ok(objects
            .entry(hash)
            .or_insert_with(|| Arc::new(AsyncMutex::new(StagedObjectState::default())))
            .clone())
    }

    async fn ensure_staged_locked(
        &self,
        hash: ObjectHash,
        state: &mut StagedObjectState,
    ) -> Result<(), ProviderError> {
        if state.staging.is_some() {
            return Ok(());
        }
        let working = self.working.clone();
        let staging = self
            .local_io
            .run(move || working.allocate_repository_object_staging())
            .await?;
        let snapshot = self.backend.snapshot().await?;
        let found = snapshot.fetch_object(hash, staging.path()).await?;
        if found.is_none() {
            return Err(RepositoryError::invalid_repository(format!(
                "remote repository '{}' no longer advertises object '{hash}'",
                self.backend.identity()
            ))
            .into());
        }
        state.staging = Some(staging);
        Ok(())
    }

    async fn working_object_path(
        &self,
        hash: ObjectHash,
    ) -> Result<Option<PathBuf>, ProviderError> {
        let working = self.working.clone();
        self.local_io
            .run(move || working.object_path(hash))
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl ContentProvider for RemoteContentProvider {
    fn transfer_mode(&self) -> ContentTransferMode {
        ContentTransferMode::Download
    }

    async fn locate_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<HashSet<ObjectHash>, ProviderError> {
        let snapshot = self.backend.snapshot().await?;
        let mut available = HashSet::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if seen.insert(*hash) && snapshot.contains_object(*hash).await? {
                available.insert(*hash);
            }
        }
        Ok(available)
    }

    async fn object_manifest(
        &self,
        hash: ObjectHash,
    ) -> Result<Option<FsTreeManifest>, ProviderError> {
        if let Some(path) = self.working_object_path(hash).await? {
            return self
                .local_io
                .run(move || read_manifest_if_marked(&path))
                .await
                .map_err(Into::into);
        }
        let staged = self.staged_object(hash)?;
        let mut state = staged.lock().await;
        if let Some(path) = self.working_object_path(hash).await? {
            return self
                .local_io
                .run(move || read_manifest_if_marked(&path))
                .await
                .map_err(Into::into);
        }
        self.ensure_staged_locked(hash, &mut state).await?;
        let path = state
            .staging
            .as_ref()
            .expect("staged object exists after successful download")
            .path()
            .to_path_buf();
        self.local_io
            .run(move || read_manifest_if_marked(&path))
            .await
            .map_err(Into::into)
    }

    async fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<HashSet<FsFileHash>, ProviderError> {
        let snapshot = self.backend.snapshot().await?;
        let mut available = HashSet::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if seen.insert(*hash) && snapshot.contains_fs_file(*hash).await? {
                available.insert(*hash);
            }
        }
        Ok(available)
    }

    async fn import_fs_files(
        &self,
        working: &Store,
        hashes: &[FsFileHash],
    ) -> Result<(), ProviderError> {
        if working.root() != self.working.root() {
            return Err(StoreError::InvalidInput(format!(
                "remote content provider for '{}' cannot publish into working store '{}'",
                self.working.root().display(),
                working.root().display()
            ))
            .into());
        }
        let snapshot = self.backend.snapshot().await?;
        let mut seen = HashSet::new();
        let hashes = hashes
            .iter()
            .copied()
            .filter(|hash| seen.insert(*hash))
            .collect::<Vec<_>>();
        for batch in hashes.chunks(FS_FILE_IMPORT_BATCH_SIZE) {
            let mut tasks = JoinSet::new();
            for hash in batch {
                let snapshot = snapshot.clone();
                let hash = *hash;
                tasks.spawn(async move {
                    let representation = snapshot.fetch_fs_file_representation(hash).await?;
                    Ok::<_, RepositoryError>((hash, representation))
                });
            }
            let mut representations = Vec::<FetchedFsFileRepresentation>::new();
            while let Some(result) = tasks.join_next().await {
                let (hash, representation) = result.map_err(|error| {
                    ProviderError::Store(StoreError::Io(format!(
                        "remote fs-file task failed: {error}"
                    )))
                })??;
                let representation = representation.ok_or_else(|| {
                    RepositoryError::invalid_repository(format!(
                        "remote repository '{}' no longer advertises fs-file '{hash}'",
                        self.backend.identity()
                    ))
                })?;
                representations.push(representation);
            }
            let runtime = self.runtime.clone();
            let working = self.working.clone();
            let permit = self.local_io.acquire().await?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                publish_fs_file_batch(&runtime, &working, &representations)
            })
            .await
            .map_err(|error| {
                ProviderError::Store(StoreError::Io(format!(
                    "remote fs-file publication task failed: {error}"
                )))
            })??;
        }
        Ok(())
    }

    async fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentImportOutcome, ProviderError> {
        if working.root() != self.working.root() {
            return Err(StoreError::InvalidInput(format!(
                "remote content provider for '{}' cannot publish into working store '{}'",
                self.working.root().display(),
                working.root().display()
            ))
            .into());
        }
        if self.working_object_path(hash).await?.is_some() {
            return Ok(ContentImportOutcome::AlreadyPresent);
        }
        let staged = self.staged_object(hash)?;
        let mut state = staged.lock().await;
        if self.working_object_path(hash).await?.is_some() {
            return Ok(ContentImportOutcome::AlreadyPresent);
        }
        self.ensure_staged_locked(hash, &mut state).await?;
        let staging = state
            .staging
            .take()
            .expect("staged object exists after successful download");
        let working = self.working.clone();
        self.local_io
            .run(move || working.publish_repository_object(staging, hash))
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::CancellationToken;
    use bobr_repo::{
        BuildIndex, BuildIndexHash, Compression, FsFileList, FsFileListHash, Master,
        MemoryTransport, ObjectList, ObjectListHash, ReuseIndex, ReuseIndexHash, Slot, TrustedKeys,
        encode_fs_file, encode_object,
    };
    use ed25519_dalek::SigningKey;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    struct Fixture {
        _temp: tempfile::TempDir,
        transport: MemoryTransport,
        master_url: Url,
        build_url: Url,
        object_list_url: Url,
        object_url: Url,
        build_key: BuildKey,
        object_hash: ObjectHash,
        backend: RemoteRepositoryBackend,
        working: Store,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let working_root = temp.path().join("working");
        std::fs::create_dir(&working_root).unwrap();
        let working = Store::create(&working_root).unwrap();
        let object_source = temp.path().join("object-source");
        let object_encoded = temp.path().join("object.cbor");
        std::fs::write(&object_source, b"remote object\n").unwrap();
        let object_hash =
            encode_object(&object_source, Compression::Zstd, &object_encoded).unwrap();
        let build_key = BuildKey::from_bytes([1; 32]);
        let build_bytes = BuildIndex::encode(&[(build_key, object_hash)]).unwrap();
        let reuse_bytes = ReuseIndex::encode(&[]).unwrap();
        let object_list_bytes = ObjectList::encode([object_hash]);
        let file_list_bytes = FsFileList::encode([]);
        let build_hash = BuildIndexHash::digest(&build_bytes);
        let reuse_hash = ReuseIndexHash::digest(&reuse_bytes);
        let object_list_hash = ObjectListHash::digest(&object_list_bytes);
        let file_list_hash = FsFileListHash::digest(&file_list_bytes);
        let base_url = Url::parse("https://data.example/").unwrap();
        let master_url = Url::parse("https://master.example/master").unwrap();
        let signing = SigningKey::from_bytes(&[7; 32]);
        let master = Master::new(
            None,
            base_url.clone(),
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
        let transport = MemoryTransport::default();
        transport.insert(
            master_url.clone(),
            master.sign(b"key", &signing).unwrap(),
            "application/cose; cose-type=\"cose-sign1\"",
            "no-cache",
        );
        let build_url = base_url.join(&format!("b/{build_hash}")).unwrap();
        let reuse_url = base_url.join(&format!("r/{reuse_hash}")).unwrap();
        let object_list_url = base_url.join(&format!("lo/{object_list_hash}")).unwrap();
        let file_list_url = base_url.join(&format!("lf/{file_list_hash}")).unwrap();
        let object_url = base_url.join(&format!("o/{object_hash}")).unwrap();
        for (url, bytes) in [
            (build_url.clone(), build_bytes),
            (reuse_url, reuse_bytes),
            (object_list_url.clone(), object_list_bytes),
            (file_list_url, file_list_bytes),
        ] {
            transport.insert(
                url,
                bytes,
                "application/octet-stream",
                "public, max-age=31536000, immutable",
            );
        }
        transport.insert(
            object_url.clone(),
            std::fs::read(object_encoded).unwrap(),
            "application/vnd.bobr.repository-object+cbor",
            "public, max-age=31536000, immutable",
        );
        let trusted = TrustedKeys::new([(b"key".to_vec(), signing.verifying_key())]).unwrap();
        let reader = RepositoryReader::new(
            master_url.clone(),
            trusted,
            &temp.path().join("cache"),
            Arc::new(transport.clone()),
        )
        .unwrap();
        let backend = RemoteRepositoryBackend::new(master_url.to_string(), reader);
        Fixture {
            _temp: temp,
            transport,
            master_url,
            build_url,
            object_list_url,
            object_url,
            build_key,
            object_hash,
            backend,
            working,
        }
    }

    #[tokio::test]
    async fn registry_and_capabilities_share_one_lazy_snapshot() {
        let fixture = fixture();
        let mut registry = RemoteBackendRegistry::default();
        let creations = Arc::new(AtomicUsize::new(0));
        let first_backend = fixture.backend.clone();
        let first = registry
            .open_with("remote", {
                let creations = creations.clone();
                move || {
                    creations.fetch_add(1, Ordering::SeqCst);
                    Ok(first_backend.inner.reader.clone())
                }
            })
            .unwrap();
        let second = registry
            .open_with("remote", || {
                creations.fetch_add(1, Ordering::SeqCst);
                panic!("duplicate backend must not be constructed")
            })
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(creations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.transport.request_count(&fixture.master_url), 0);

        let mapping = RemoteMappingProvider::new(first);
        let local_io = LocalIoScheduler::new(2, CancellationToken::new()).unwrap();
        let content = RemoteContentProvider::new(
            second,
            fixture.working.clone(),
            RuntimeProvider::host(),
            local_io,
        );
        assert_eq!(
            mapping.resolve_builds(&[fixture.build_key]).await.unwrap(),
            vec![TrustedResolution {
                key: fixture.build_key,
                object_hash: fixture.object_hash,
            }]
        );
        assert_eq!(fixture.transport.request_count(&fixture.master_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.build_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.object_list_url), 0);

        assert_eq!(
            content
                .locate_objects(&[fixture.object_hash])
                .await
                .unwrap(),
            HashSet::from([fixture.object_hash])
        );
        assert_eq!(fixture.transport.request_count(&fixture.master_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.object_list_url), 1);
    }

    #[tokio::test]
    async fn content_capability_does_not_fetch_mapping_indexes() {
        let fixture = fixture();
        let content = RemoteContentProvider::new(
            fixture.backend,
            fixture.working,
            RuntimeProvider::host(),
            LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
        );
        assert_eq!(
            content
                .locate_objects(&[fixture.object_hash])
                .await
                .unwrap(),
            HashSet::from([fixture.object_hash])
        );
        assert_eq!(fixture.transport.request_count(&fixture.master_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.object_list_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.build_url), 0);
    }

    #[tokio::test]
    async fn manifest_probe_and_import_download_an_object_once() {
        let fixture = fixture();
        let local_io = LocalIoScheduler::new(2, CancellationToken::new()).unwrap();
        let content = RemoteContentProvider::new(
            fixture.backend,
            fixture.working.clone(),
            RuntimeProvider::host(),
            local_io,
        );
        assert_eq!(
            content.object_manifest(fixture.object_hash).await.unwrap(),
            None
        );
        assert_eq!(fixture.transport.request_count(&fixture.object_url), 1);
        assert_eq!(
            content
                .import_object(&fixture.working, fixture.object_hash)
                .await
                .unwrap(),
            ContentImportOutcome::Imported
        );
        assert_eq!(fixture.transport.request_count(&fixture.object_url), 1);
        assert!(
            fixture
                .working
                .object_path(fixture.object_hash)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn fs_files_are_downloaded_then_published_as_one_batch() {
        let temp = tempfile::tempdir().unwrap();
        let working_root = temp.path().join("working");
        std::fs::create_dir(&working_root).unwrap();
        let working = Store::create(&working_root).unwrap();
        let mut files = Vec::new();
        for (name, bytes) in [("first", b"first\n".as_slice()), ("second", b"second\n")] {
            let source = temp.path().join(format!("{name}.source"));
            let encoded = temp.path().join(format!("{name}.cbor"));
            std::fs::write(&source, bytes).unwrap();
            let hash = encode_fs_file(&source, Compression::Zstd, &encoded).unwrap();
            files.push((hash, encoded));
        }
        let build_bytes = BuildIndex::encode(&[]).unwrap();
        let reuse_bytes = ReuseIndex::encode(&[]).unwrap();
        let object_list_bytes = ObjectList::encode([]);
        let file_list_bytes = FsFileList::encode(files.iter().map(|(hash, _)| *hash));
        let build_hash = BuildIndexHash::digest(&build_bytes);
        let reuse_hash = ReuseIndexHash::digest(&reuse_bytes);
        let object_list_hash = ObjectListHash::digest(&object_list_bytes);
        let file_list_hash = FsFileListHash::digest(&file_list_bytes);
        let base_url = Url::parse("https://files.example/").unwrap();
        let master_url = Url::parse("https://files-master.example/master").unwrap();
        let signing = SigningKey::from_bytes(&[8; 32]);
        let master = Master::new(
            None,
            base_url.clone(),
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
        let transport = MemoryTransport::default();
        transport.insert(
            master_url.clone(),
            master.sign(b"key", &signing).unwrap(),
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
                base_url.join(&format!("{namespace}/{hash}")).unwrap(),
                bytes,
                "application/octet-stream",
                "public, max-age=31536000, immutable",
            );
        }
        for (hash, encoded) in &files {
            transport.insert(
                base_url.join(&format!("f/{hash}")).unwrap(),
                std::fs::read(encoded).unwrap(),
                "application/vnd.bobr.repository-fs-file+cbor",
                "public, max-age=31536000, immutable",
            );
        }
        let trusted = TrustedKeys::new([(b"key".to_vec(), signing.verifying_key())]).unwrap();
        let reader = RepositoryReader::new(
            master_url,
            trusted,
            &temp.path().join("cache"),
            Arc::new(transport.clone()),
        )
        .unwrap();
        let content = RemoteContentProvider::new(
            RemoteRepositoryBackend::new("files", reader),
            working.clone(),
            RuntimeProvider::host(),
            LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
        );
        let hashes = files.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        content.import_fs_files(&working, &hashes).await.unwrap();
        for hash in hashes {
            assert!(working.fs_file_path(hash).is_file());
            assert_eq!(
                transport.request_count(&base_url.join(&format!("f/{hash}")).unwrap()),
                1
            );
        }
    }
}
