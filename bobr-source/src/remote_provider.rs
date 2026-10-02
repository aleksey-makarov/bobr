//! Remote repository backends exposed through secondary capabilities.

use crate::{
    ContentProvider, ContentProviderImport, LocalIoScheduler, MappingProvider, ProviderError,
};
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

fn remote_store_validation_error(error: StoreError) -> ProviderError {
    match error {
        StoreError::InvalidData(message) => {
            ProviderError::repository_content(RepositoryError::invalid_repository(message))
        }
        error => ProviderError::Store(error),
    }
}

/// One authenticated remote repository shared by complementary capabilities.
#[derive(Clone)]
pub struct RemoteRepositoryBackend {
    inner: Arc<RemoteRepositoryBackendInner>,
}

struct RemoteRepositoryBackendInner {
    identity: String,
    reader: RepositoryReader,
    snapshot: OnceCell<Arc<RepositorySnapshot>>,
    disabled: Mutex<Option<DisabledRemote>>,
}

#[derive(Debug, Clone)]
struct DisabledRemote {
    kind: bobr_repo::RepositoryErrorKind,
    message: String,
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
                disabled: Mutex::new(None),
            }),
        }
    }

    /// Returns the stable diagnostic identity of this backend.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// Lazily authenticates the master and freezes one snapshot for this run.
    pub async fn snapshot(&self) -> Result<Arc<RepositorySnapshot>, RepositoryError> {
        self.ensure_enabled()?;
        let result = self
            .inner
            .snapshot
            .get_or_try_init(|| async { self.inner.reader.snapshot().await.map(Arc::new) })
            .await
            .cloned();
        self.repository_result(result)
    }

    fn ensure_enabled(&self) -> Result<(), RepositoryError> {
        let disabled = self.inner.disabled.lock().map_err(|_| {
            RepositoryError::local_io("remote repository disabled-state lock is poisoned")
        })?;
        if let Some(disabled) = disabled.as_ref() {
            return Err(RepositoryError::with_kind(
                disabled.kind,
                disabled.message.clone(),
            ));
        }
        Ok(())
    }

    fn repository_result<T>(
        &self,
        result: Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if matches!(
                    error.kind(),
                    bobr_repo::RepositoryErrorKind::Transport { .. }
                ) && let Ok(mut disabled) = self.inner.disabled.lock()
                    && disabled.is_none()
                {
                    *disabled = Some(DisabledRemote {
                        kind: error.kind(),
                        message: format!(
                            "remote repository '{}' is disabled for this run after a transport failure: {error}",
                            self.identity()
                        ),
                    });
                }
                Err(error)
            }
        }
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
            let candidates = self
                .backend
                .repository_result(snapshot.build_candidates(*key).await)?;
            for object_hash in candidates {
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
            let candidates = self
                .backend
                .repository_result(snapshot.reuse_candidates(*key).await)?;
            for object_hash in candidates {
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
    encoded_bytes: u64,
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
        let found = self
            .backend
            .repository_result(snapshot.fetch_object(hash, staging.path()).await)
            .map_err(ProviderError::repository_content)?;
        let Some(found) = found else {
            return Err(ProviderError::repository_content(
                RepositoryError::invalid_repository(format!(
                    "remote repository '{}' no longer advertises object '{hash}'",
                    self.backend.identity()
                )),
            ));
        };
        state.encoded_bytes = found.encoded_bytes;
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
            if !seen.insert(*hash) {
                continue;
            }
            let contains = self
                .backend
                .repository_result(snapshot.contains_object(*hash).await)?;
            if contains {
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
            .map_err(remote_store_validation_error)
    }

    async fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<HashSet<FsFileHash>, ProviderError> {
        let snapshot = self.backend.snapshot().await?;
        let mut available = HashSet::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            let contains = self
                .backend
                .repository_result(snapshot.contains_fs_file(*hash).await)?;
            if contains {
                available.insert(*hash);
            }
        }
        Ok(available)
    }

    async fn import_fs_files(
        &self,
        working: &Store,
        hashes: &[FsFileHash],
    ) -> Result<ContentProviderImport<()>, ProviderError> {
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
        let mut encoded_bytes = 0_u64;
        for batch in hashes.chunks(FS_FILE_IMPORT_BATCH_SIZE) {
            let mut tasks = JoinSet::new();
            for hash in batch {
                let snapshot = snapshot.clone();
                let backend = self.backend.clone();
                let hash = *hash;
                tasks.spawn(async move {
                    let representation = backend
                        .repository_result(snapshot.fetch_fs_file_representation(hash).await)
                        .map_err(ProviderError::repository_content)?;
                    Ok::<_, ProviderError>((hash, representation))
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
                    ProviderError::repository_content(RepositoryError::invalid_repository(format!(
                        "remote repository '{}' no longer advertises fs-file '{hash}'",
                        self.backend.identity()
                    )))
                })?;
                encoded_bytes = encoded_bytes.saturating_add(representation.encoded_bytes());
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
            })?
            .map_err(ProviderError::repository_content)?;
        }
        Ok(ContentProviderImport::remote((), encoded_bytes))
    }

    async fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError> {
        if working.root() != self.working.root() {
            return Err(StoreError::InvalidInput(format!(
                "remote content provider for '{}' cannot publish into working store '{}'",
                self.working.root().display(),
                working.root().display()
            ))
            .into());
        }
        if self.working_object_path(hash).await?.is_some() {
            return Ok(ContentProviderImport::remote(
                ContentImportOutcome::AlreadyPresent,
                0,
            ));
        }
        let staged = self.staged_object(hash)?;
        let mut state = staged.lock().await;
        if self.working_object_path(hash).await?.is_some() {
            return Ok(ContentProviderImport::remote(
                ContentImportOutcome::AlreadyPresent,
                0,
            ));
        }
        self.ensure_staged_locked(hash, &mut state).await?;
        let staging = state
            .staging
            .take()
            .expect("staged object exists after successful download");
        let encoded_bytes = std::mem::take(&mut state.encoded_bytes);
        let working = self.working.clone();
        self.local_io
            .run(move || working.publish_repository_object(staging, hash))
            .await
            .map(|outcome| ContentProviderImport::remote(outcome, encoded_bytes))
            .map_err(remote_store_validation_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NamedContentProvider, NamedMappingProvider, ReuseQuery, SecondaryResolver};
    use bobr_core::CancellationToken;
    use bobr_repo::{
        BuildIndex, BuildIndexHash, Compression, FetchRequest, FetchResult, FsFileList,
        FsFileListHash, Master, MemoryTransport, ObjectList, ObjectListHash, RepositoryTransport,
        ReuseIndex, ReuseIndexHash, Slot, TrustedKeys, encode_fs_file, encode_object,
    };
    use bobr_store::fs_tree::FsTreeEntry;
    use bobr_store::{load_build_object_hash, load_reuse_object_hash};
    use ed25519_dalek::SigningKey;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    struct Fixture {
        _temp: tempfile::TempDir,
        transport: MemoryTransport,
        master_url: Url,
        build_url: Url,
        reuse_url: Url,
        object_list_url: Url,
        object_url: Url,
        build_key: BuildKey,
        reuse_key: ReuseKey,
        object_hash: ObjectHash,
        backend: RemoteRepositoryBackend,
        working: Store,
    }

    #[derive(Debug, Clone, Default)]
    struct TransportFailure {
        requests: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RepositoryTransport for TransportFailure {
        async fn fetch(&self, _request: FetchRequest) -> Result<FetchResult, RepositoryError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Err(RepositoryError::transport("test endpoint is down", false))
        }
    }

    fn fixture() -> Fixture {
        fixture_with_source(|source| std::fs::write(source, b"remote object\n").unwrap())
    }

    fn fixture_with_source(populate: impl FnOnce(&std::path::Path)) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let working_root = temp.path().join("working");
        std::fs::create_dir(&working_root).unwrap();
        let working = Store::create(&working_root).unwrap();
        let object_source = temp.path().join("object-source");
        let object_encoded = temp.path().join("object.cbor");
        populate(&object_source);
        let object_hash =
            encode_object(&object_source, Compression::Zstd, &object_encoded).unwrap();
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
            (reuse_url.clone(), reuse_bytes),
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
            reuse_url,
            object_list_url,
            object_url,
            build_key,
            reuse_key,
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
    async fn transport_failure_disables_the_shared_backend_for_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let transport = TransportFailure::default();
        let signing = SigningKey::from_bytes(&[9; 32]);
        let trusted = TrustedKeys::new([(b"key".to_vec(), signing.verifying_key())]).unwrap();
        let master_url = Url::parse("https://unavailable.example/master").unwrap();
        let reader = RepositoryReader::new(
            master_url.clone(),
            trusted,
            &temp.path().join("cache"),
            Arc::new(transport.clone()),
        )
        .unwrap();
        let backend = RemoteRepositoryBackend::new(master_url.to_string(), reader);
        let mapping = RemoteMappingProvider::new(backend.clone());
        let working_root = temp.path().join("working");
        std::fs::create_dir(&working_root).unwrap();
        let content = RemoteContentProvider::new(
            backend,
            Store::create(&working_root).unwrap(),
            RuntimeProvider::host(),
            LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
        );

        let first = mapping
            .resolve_builds(&[BuildKey::from_bytes([1; 32])])
            .await;
        let second = content
            .locate_objects(&[ObjectHash::from_bytes([2; 32])])
            .await;

        assert!(matches!(
            first.unwrap_err().repository_kind(),
            Some(bobr_repo::RepositoryErrorKind::Transport { .. })
        ));
        assert!(matches!(
            second.unwrap_err().repository_kind(),
            Some(bobr_repo::RepositoryErrorKind::Transport { .. })
        ));
        assert_eq!(transport.requests.load(Ordering::SeqCst), 1);
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
        let imported = content
            .import_object(&fixture.working, fixture.object_hash)
            .await
            .unwrap();
        assert_eq!(imported.value, ContentImportOutcome::Imported);
        assert!(imported.encoded_bytes.is_some_and(|bytes| bytes > 0));
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
    async fn exact_remote_hit_publishes_local_record_and_build_mapping() {
        let fixture = fixture();
        let mapping = NamedMappingProvider::new(
            "remote",
            Arc::new(RemoteMappingProvider::new(fixture.backend.clone())),
        );
        let content = NamedContentProvider::new(
            "remote",
            Arc::new(RemoteContentProvider::new(
                fixture.backend,
                fixture.working.clone(),
                RuntimeProvider::host(),
                LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
            )),
        );
        let resolver = SecondaryResolver::new(
            fixture.working.clone(),
            "remote-test",
            vec![mapping],
            vec![content],
        )
        .unwrap();

        let report = resolver
            .resolve_builds(&[fixture.build_key])
            .await
            .unwrap()
            .remove(0);

        assert_eq!(report.resolved.unwrap().object_hash, fixture.object_hash);
        assert_eq!(
            load_build_object_hash(&fixture.working, fixture.build_key).unwrap(),
            Some(fixture.object_hash)
        );
        assert!(
            fixture
                .working
                .root()
                .join("object-records")
                .join(format!("{}.json", fixture.object_hash))
                .is_file()
        );
    }

    #[tokio::test]
    async fn remote_reuse_hit_publishes_local_reuse_and_build_mappings() {
        let fixture = fixture();
        let current_build_key = BuildKey::from_bytes([3; 32]);
        let mapping = NamedMappingProvider::new(
            "remote",
            Arc::new(RemoteMappingProvider::new(fixture.backend.clone())),
        );
        let content = NamedContentProvider::new(
            "remote",
            Arc::new(RemoteContentProvider::new(
                fixture.backend,
                fixture.working.clone(),
                RuntimeProvider::host(),
                LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
            )),
        );
        let resolver = SecondaryResolver::new(
            fixture.working.clone(),
            "remote-reuse-test",
            vec![mapping],
            vec![content],
        )
        .unwrap();

        let query = ReuseQuery {
            build_key: current_build_key,
            reuse_key: fixture.reuse_key,
        };
        let report = resolver.resolve_reuses(&[query]).await.unwrap().remove(0);

        assert_eq!(report.resolved.unwrap().object_hash, fixture.object_hash);
        assert_eq!(fixture.transport.request_count(&fixture.reuse_url), 1);
        assert_eq!(fixture.transport.request_count(&fixture.build_url), 0);
        assert_eq!(
            load_reuse_object_hash(&fixture.working, fixture.reuse_key).unwrap(),
            Some(fixture.object_hash)
        );
        assert_eq!(
            load_build_object_hash(&fixture.working, current_build_key).unwrap(),
            Some(fixture.object_hash)
        );
    }

    #[tokio::test]
    async fn directory_object_is_downloaded_verified_and_published() {
        let fixture = fixture_with_source(|source| {
            std::fs::create_dir(source).unwrap();
            std::fs::create_dir(source.join("subdir")).unwrap();
            std::fs::write(source.join("top"), b"top-level\n").unwrap();
            std::fs::write(source.join("subdir/leaf"), b"nested\n").unwrap();
            std::os::unix::fs::symlink("../top", source.join("subdir/link")).unwrap();
        });
        let content = RemoteContentProvider::new(
            fixture.backend,
            fixture.working.clone(),
            RuntimeProvider::host(),
            LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
        );

        let imported = content
            .import_object(&fixture.working, fixture.object_hash)
            .await
            .unwrap();
        assert_eq!(imported.value, ContentImportOutcome::Imported);
        let published = fixture
            .working
            .object_path(fixture.object_hash)
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(published.join("top")).unwrap(),
            b"top-level\n"
        );
        assert_eq!(
            std::fs::read(published.join("subdir/leaf")).unwrap(),
            b"nested\n"
        );
        assert_eq!(
            std::fs::read_link(published.join("subdir/link")).unwrap(),
            PathBuf::from("../top")
        );
        assert_eq!(fixture.transport.request_count(&fixture.object_url), 1);
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
        let imported = content.import_fs_files(&working, &hashes).await.unwrap();
        assert_eq!(
            imported.encoded_bytes,
            Some(
                files
                    .iter()
                    .map(|(_, path)| std::fs::metadata(path).unwrap().len())
                    .sum()
            )
        );
        for hash in hashes {
            assert!(working.fs_file_path(hash).is_file());
            assert_eq!(
                transport.request_count(&base_url.join(&format!("f/{hash}")).unwrap()),
                1
            );
        }
    }

    #[tokio::test]
    async fn remote_fs_tree_imports_manifest_and_complete_file_closure() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("source-store");
        let working_root = temp.path().join("working");
        std::fs::create_dir(&source_root).unwrap();
        std::fs::create_dir(&working_root).unwrap();
        let source_store = Store::create(&source_root).unwrap();
        let working = Store::create(&working_root).unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("first"), b"first payload\n").unwrap();
        std::fs::write(tree.join("second"), b"second payload\n").unwrap();
        let manifest = source_store.fs_tree().intern_tree(tree).unwrap();
        let fs_file_hashes = manifest
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(fs_file_hashes.len(), 2);
        let manifest_source = temp.path().join("manifest.jsonl");
        let manifest_encoded = temp.path().join("manifest.cbor");
        manifest.write_canonical(&manifest_source).unwrap();
        let object_hash =
            encode_object(&manifest_source, Compression::Zstd, &manifest_encoded).unwrap();
        let mut encoded_fs_files = Vec::new();
        for (index, hash) in fs_file_hashes.iter().enumerate() {
            let encoded = temp.path().join(format!("fs-file-{index}.cbor"));
            assert_eq!(
                encode_fs_file(
                    &source_store.fs_file_path(*hash),
                    Compression::Zstd,
                    &encoded,
                )
                .unwrap(),
                *hash
            );
            encoded_fs_files.push((*hash, encoded));
        }

        let build_key = BuildKey::from_bytes([11; 32]);
        let reuse_key = ReuseKey::from_bytes([12; 32]);
        let build_bytes = BuildIndex::encode(&[(build_key, object_hash)]).unwrap();
        let reuse_bytes = ReuseIndex::encode(&[(reuse_key, object_hash)]).unwrap();
        let object_list_bytes = ObjectList::encode([object_hash]);
        let file_list_bytes = FsFileList::encode(fs_file_hashes.iter().copied());
        let build_hash = BuildIndexHash::digest(&build_bytes);
        let reuse_hash = ReuseIndexHash::digest(&reuse_bytes);
        let object_list_hash = ObjectListHash::digest(&object_list_bytes);
        let file_list_hash = FsFileListHash::digest(&file_list_bytes);
        let base_url = Url::parse("https://tree-data.example/").unwrap();
        let master_url = Url::parse("https://tree-master.example/master").unwrap();
        let signing = SigningKey::from_bytes(&[13; 32]);
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
        transport.insert(
            base_url.join(&format!("o/{object_hash}")).unwrap(),
            std::fs::read(&manifest_encoded).unwrap(),
            "application/vnd.bobr.repository-object+cbor",
            "public, max-age=31536000, immutable",
        );
        for (hash, encoded) in &encoded_fs_files {
            transport.insert(
                base_url.join(&format!("f/{hash}")).unwrap(),
                std::fs::read(encoded).unwrap(),
                "application/vnd.bobr.repository-fs-file+cbor",
                "public, max-age=31536000, immutable",
            );
        }
        let trusted = TrustedKeys::new([(b"key".to_vec(), signing.verifying_key())]).unwrap();
        let backend = RemoteRepositoryBackend::new(
            master_url.to_string(),
            RepositoryReader::new(
                master_url,
                trusted,
                &temp.path().join("cache"),
                Arc::new(transport),
            )
            .unwrap(),
        );
        let resolver = SecondaryResolver::new(
            working.clone(),
            "remote-tree-test",
            vec![NamedMappingProvider::new(
                "remote",
                Arc::new(RemoteMappingProvider::new(backend.clone())),
            )],
            vec![NamedContentProvider::new(
                "remote",
                Arc::new(RemoteContentProvider::new(
                    backend,
                    working.clone(),
                    RuntimeProvider::host(),
                    LocalIoScheduler::new(2, CancellationToken::new()).unwrap(),
                )),
            )],
        )
        .unwrap();

        let resolved = resolver
            .resolve_builds(&[build_key])
            .await
            .unwrap()
            .remove(0)
            .resolved
            .unwrap();
        assert_eq!(resolved.object_hash, object_hash);
        assert_eq!(resolved.content_sources, ["remote"]);
        assert_eq!(resolved.transfers.len(), 1);
        assert_eq!(
            resolved.transfers[0].transfer_mode,
            ContentTransferMode::Download
        );
        assert_eq!(resolved.transfers[0].files, 3);
        let expected_encoded_bytes = std::fs::metadata(&manifest_encoded).unwrap().len()
            + encoded_fs_files
                .iter()
                .map(|(_, path)| std::fs::metadata(path).unwrap().len())
                .sum::<u64>();
        assert_eq!(
            resolved.transfers[0].encoded_bytes,
            Some(expected_encoded_bytes)
        );
        assert!(working.object_is_complete(object_hash).unwrap());
        for hash in fs_file_hashes {
            working.verify_fs_file(hash).unwrap();
        }
    }
}
