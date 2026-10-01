//! Mapping-first, content-second resolution across secondary-store capabilities.

use crate::LocalIoScheduler;
use async_trait::async_trait;
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_repo::{RepositoryError, RepositoryErrorKind};
use bobr_store::fs_tree::{FsFileHash, FsTreeEntry, FsTreeManifest, read_manifest_if_marked};
use bobr_store::{
    ContentImportOutcome, ContentSource, ContentTransferMode, LocalRepository, ReadOnlyStore,
    Store, StoreError, TrustedKeyIndex, TrustedResolution, publish_existing_build_mapping,
    publish_existing_reuse_mapping, record_existing_object,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::hash::Hash;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinSet;

/// Typed failure produced by one mapping or content capability.
///
/// Local store failures and authenticated remote-repository failures stay
/// distinct until the resolver applies its provider fallback policy.
#[derive(Debug)]
pub enum ProviderError {
    /// Failure in a local store-backed capability.
    Store(StoreError),
    /// Failure in a remote repository reader or transport.
    Repository(RepositoryError),
    /// Failure of one advertised remote content representation.
    RepositoryContent(RepositoryError),
}

impl ProviderError {
    /// Converts a provider failure for resolver APIs that still expose the
    /// historical store-oriented error boundary.
    pub fn into_store_error(self) -> StoreError {
        match self {
            Self::Store(error) => error,
            Self::Repository(error) => {
                let message = error.to_string();
                match error.kind() {
                    RepositoryErrorKind::Configuration => StoreError::InvalidInput(message),
                    RepositoryErrorKind::Authentication
                    | RepositoryErrorKind::InvalidRepository => StoreError::InvalidData(message),
                    RepositoryErrorKind::Transport { .. }
                    | RepositoryErrorKind::LocalIo
                    | RepositoryErrorKind::Runtime
                    | RepositoryErrorKind::Cancelled => StoreError::Io(message),
                }
            }
            Self::RepositoryContent(error) => {
                let message = error.to_string();
                match error.kind() {
                    RepositoryErrorKind::Configuration => StoreError::InvalidInput(message),
                    RepositoryErrorKind::Authentication
                    | RepositoryErrorKind::InvalidRepository => StoreError::InvalidData(message),
                    RepositoryErrorKind::Transport { .. }
                    | RepositoryErrorKind::LocalIo
                    | RepositoryErrorKind::Runtime
                    | RepositoryErrorKind::Cancelled => StoreError::Io(message),
                }
            }
        }
    }

    /// Returns the remote category, or `None` for a local-store failure.
    pub fn repository_kind(&self) -> Option<RepositoryErrorKind> {
        match self {
            Self::Store(_) => None,
            Self::Repository(error) | Self::RepositoryContent(error) => Some(error.kind()),
        }
    }

    /// Marks a failure as belonging to one advertised content representation,
    /// not to the authenticated repository metadata that named it.
    pub fn repository_content(error: RepositoryError) -> Self {
        Self::RepositoryContent(error)
    }
}

impl From<StoreError> for ProviderError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<RepositoryError> for ProviderError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(error)
    }
}

impl From<ProviderError> for StoreError {
    fn from(error: ProviderError) -> Self {
        error.into_store_error()
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::Repository(error) | Self::RepositoryContent(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Per-run registry of canonical local repository backends.
///
/// Complementary logical capabilities clone the same [`LocalRepository`]
/// handle instead of reopening and revalidating its store layout.
#[derive(Debug, Default)]
pub struct LocalBackendRegistry {
    repositories: HashMap<PathBuf, LocalRepository>,
}

impl LocalBackendRegistry {
    /// Opens or reuses one local repository by canonical store root.
    pub fn open(&mut self, root: &Path) -> Result<LocalRepository, StoreError> {
        let canonical_root = canonical_local_root(root)?;
        if let Some(repository) = self.repositories.get(&canonical_root) {
            return Ok(repository.clone());
        }
        let store = ReadOnlyStore::open(&canonical_root)?;
        let repository = LocalRepository::new(store);
        self.repositories.insert(canonical_root, repository.clone());
        Ok(repository)
    }

    /// Returns the number of distinct canonical local stores opened.
    pub fn len(&self) -> usize {
        self.repositories.len()
    }

    /// Returns whether no local backend has been opened.
    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }
}

fn canonical_local_root(root: &Path) -> Result<PathBuf, StoreError> {
    if !root.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be absolute: '{}'",
            root.display()
        )));
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            StoreError::InvalidInput(format!("store root must exist: '{}'", root.display()))
        } else {
            StoreError::Io(format!(
                "failed to resolve store root '{}': {error}",
                root.display()
            ))
        }
    })?;
    let metadata = fs::metadata(&canonical_root).map_err(|error| {
        StoreError::Io(format!(
            "failed to inspect store root '{}': {error}",
            canonical_root.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be a directory: '{}'",
            root.display()
        )));
    }
    Ok(canonical_root)
}

/// Asynchronous authoritative build/reuse mapping capability.
#[async_trait]
pub trait MappingProvider: fmt::Debug + Send + Sync {
    /// Resolves every available build key in one batch.
    async fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError>;

    /// Resolves every available reuse key in one batch.
    async fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError>;
}

/// Asynchronous content capability for already-known object identities.
#[async_trait]
pub trait ContentProvider: fmt::Debug + Send + Sync {
    /// Physical transport used when content is imported.
    fn transfer_mode(&self) -> ContentTransferMode;

    /// Locates top-level objects in one batch.
    async fn locate_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<HashSet<ObjectHash>, ProviderError>;

    /// Reads an object as an fs-tree manifest when it carries that schema.
    async fn object_manifest(
        &self,
        hash: ObjectHash,
    ) -> Result<Option<FsTreeManifest>, ProviderError>;

    /// Locates filesystem files in one batch.
    async fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<HashSet<FsFileHash>, ProviderError>;

    /// Imports one batch of filesystem files into the working store.
    async fn import_fs_files(
        &self,
        working: &Store,
        hashes: &[FsFileHash],
    ) -> Result<ContentProviderImport<()>, ProviderError>;

    /// Imports one top-level object into the working store.
    async fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError>;
}

/// Result of one provider import together with transport-specific accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentProviderImport<T> {
    /// Provider-specific import result.
    pub value: T,
    /// Encoded bytes received from a remote representation, when applicable.
    pub encoded_bytes: Option<u64>,
}

impl<T> ContentProviderImport<T> {
    pub(crate) fn local(value: T) -> Self {
        Self {
            value,
            encoded_bytes: None,
        }
    }

    pub(crate) fn remote(value: T, encoded_bytes: u64) -> Self {
        Self {
            value,
            encoded_bytes: Some(encoded_bytes),
        }
    }
}

/// Async adapter for one synchronous local mapping index.
#[derive(Debug, Clone)]
pub struct LocalMappingProvider {
    index: Arc<dyn TrustedKeyIndex>,
    local_io: LocalIoScheduler,
}

impl LocalMappingProvider {
    /// Wraps a local mapping index under shared blocking-I/O scheduling.
    pub fn new(index: Arc<dyn TrustedKeyIndex>, local_io: LocalIoScheduler) -> Self {
        Self { index, local_io }
    }
}

#[async_trait]
impl MappingProvider for LocalMappingProvider {
    async fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError> {
        let index = self.index.clone();
        let keys = keys.to_vec();
        self.local_io
            .run(move || index.resolve_builds(&keys))
            .await
            .map_err(Into::into)
    }

    async fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError> {
        let index = self.index.clone();
        let keys = keys.to_vec();
        self.local_io
            .run(move || index.resolve_reuses(&keys))
            .await
            .map_err(Into::into)
    }
}

/// Async adapter for one synchronous local content source.
#[derive(Debug, Clone)]
pub struct LocalContentProvider {
    source: Arc<dyn ContentSource>,
    local_io: LocalIoScheduler,
}

impl LocalContentProvider {
    /// Wraps local content operations under shared blocking-I/O scheduling.
    pub fn new(source: Arc<dyn ContentSource>, local_io: LocalIoScheduler) -> Self {
        Self { source, local_io }
    }
}

#[async_trait]
impl ContentProvider for LocalContentProvider {
    fn transfer_mode(&self) -> ContentTransferMode {
        self.source.transfer_mode()
    }

    async fn locate_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<HashSet<ObjectHash>, ProviderError> {
        let source = self.source.clone();
        let hashes = hashes.to_vec();
        self.local_io
            .run(move || source.locate_objects(&hashes))
            .await
            .map_err(Into::into)
    }

    async fn object_manifest(
        &self,
        hash: ObjectHash,
    ) -> Result<Option<FsTreeManifest>, ProviderError> {
        let source = self.source.clone();
        self.local_io
            .run(move || source.object_manifest(hash))
            .await
            .map_err(Into::into)
    }

    async fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<HashSet<FsFileHash>, ProviderError> {
        let source = self.source.clone();
        let hashes = hashes.to_vec();
        self.local_io
            .run(move || source.locate_fs_files(&hashes))
            .await
            .map_err(Into::into)
    }

    async fn import_fs_files(
        &self,
        working: &Store,
        hashes: &[FsFileHash],
    ) -> Result<ContentProviderImport<()>, ProviderError> {
        let source = self.source.clone();
        let working = working.clone();
        let hashes = hashes.to_vec();
        self.local_io
            .run(move || source.import_fs_files(&working, &hashes))
            .await
            .map(ContentProviderImport::local)
            .map_err(Into::into)
    }

    async fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError> {
        let source = self.source.clone();
        let working = working.clone();
        self.local_io
            .run(move || source.import_object(&working, hash))
            .await
            .map(ContentProviderImport::local)
            .map_err(Into::into)
    }
}

/// One named mapping capability in configured priority order.
#[derive(Debug, Clone)]
pub struct NamedMappingProvider {
    name: String,
    provider: Arc<dyn MappingProvider>,
}

impl NamedMappingProvider {
    /// Names one mapping provider for diagnostics and priority selection.
    pub fn new(name: impl Into<String>, provider: Arc<dyn MappingProvider>) -> Self {
        Self {
            name: name.into(),
            provider,
        }
    }

    /// Returns the configured diagnostic name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One named content-source capability in configured priority order.
#[derive(Debug, Clone)]
pub struct NamedContentProvider {
    name: String,
    source: Arc<dyn ContentProvider>,
}

impl NamedContentProvider {
    /// Names one content source for diagnostics and priority selection.
    pub fn new(name: impl Into<String>, source: Arc<dyn ContentProvider>) -> Self {
        Self {
            name: name.into(),
            source,
        }
    }

    /// Returns the configured diagnostic name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One authoritative mapping answer retained for conflict diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingAnswer {
    /// Provider that supplied the answer.
    pub provider: String,
    /// Object named by the provider.
    pub object_hash: ObjectHash,
}

/// Ordered, hash-only answers for one build or reuse mapping lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingCandidates<K> {
    /// Queried build or reuse key.
    pub key: K,
    /// Every answer in provider priority order, including agreements.
    pub answers: Vec<MappingAnswer>,
    /// Distinct object hashes in first-answer order.
    pub object_hashes: Vec<ObjectHash>,
}

impl<K> MappingCandidates<K> {
    /// Returns true when mapping providers named more than one distinct hash.
    pub fn has_conflict(&self) -> bool {
        self.object_hashes.len() > 1
    }
}

/// Successfully selected and imported secondary result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSecondaryContent {
    /// Selected object hash.
    pub object_hash: ObjectHash,
    /// First mapping provider that named the selected candidate.
    pub mapping_provider: String,
    /// Content sources used for the top-level object or fs-file closure.
    ///
    /// This is empty when the complete candidate was already in the working
    /// store.
    pub content_sources: Vec<String>,
    /// Physical content transfers performed while completing the object.
    pub transfers: Vec<ContentTransferReport>,
    /// Whether the top-level object was already present or newly imported.
    pub import_outcome: ContentImportOutcome,
}

/// Content-only resolution of an object whose hash was already known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownObjectResolution {
    /// Requested object hash.
    pub object_hash: ObjectHash,
    /// Content sources used for the top-level object or fs-file closure.
    pub content_sources: Vec<String>,
    /// Physical content transfers performed while completing the object.
    pub transfers: Vec<ContentTransferReport>,
    /// Import result, or `None` when no complete content source set was found.
    pub outcome: Option<ContentImportOutcome>,
}

/// One completed physical transfer from a named content source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentTransferReport {
    /// Object whose payload or fs-tree closure required the transfer.
    pub object_hash: ObjectHash,
    /// Configured content-source name.
    pub content_source: String,
    /// Physical import transport.
    pub transfer_mode: ContentTransferMode,
    /// Number of regular payload files transferred.
    pub files: u64,
    /// Sum of transferred regular-file lengths.
    pub bytes: u64,
    /// Encoded representation bytes received over the network, when any.
    pub encoded_bytes: Option<u64>,
    /// Wall-clock duration of the synchronous transfer call.
    pub duration_ms: u64,
}

/// Lifecycle event for an actual content-source transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentTransferEvent {
    /// A selected provider is about to import content.
    Started {
        /// Object whose payload or closure is being completed.
        object_hash: ObjectHash,
        /// Configured content-source name.
        content_source: String,
        /// Physical import transport.
        transfer_mode: ContentTransferMode,
    },
    /// The selected provider completed an actual transfer.
    Finished(ContentTransferReport),
    /// Advertised content from one provider was unavailable or invalid.
    Failed {
        /// Object whose payload or closure was being completed.
        object_hash: ObjectHash,
        /// Configured content-source name.
        content_source: String,
        /// Physical import transport.
        transfer_mode: ContentTransferMode,
        /// Verified failure which may be followed by another provider.
        error: String,
    },
}

/// Mapping/content resolution report for one queried key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryResolution<K> {
    /// Queried build or reuse identity.
    pub key: K,
    /// Every answer in provider priority order.
    pub answers: Vec<MappingAnswer>,
    /// Candidate hashes that no complete set of content sources could provide.
    pub unavailable: Vec<ObjectHash>,
    /// Selected result, or `None` for a complete secondary miss.
    pub resolved: Option<ResolvedSecondaryContent>,
}

impl<K> SecondaryResolution<K> {
    /// Returns true when mapping providers named more than one distinct hash.
    pub fn has_conflict(&self) -> bool {
        self.answers
            .iter()
            .map(|answer| answer.object_hash)
            .collect::<HashSet<_>>()
            .len()
            > 1
    }
}

/// Reuse lookup together with the current build key repaired on a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReuseQuery {
    /// Current graph/build key.
    pub build_key: BuildKey,
    /// Canonical reuse key computed from realized input objects.
    pub reuse_key: ReuseKey,
}

/// Coordinator performing authoritative mapping lookup before independent content
/// acquisition and promotion into one working store.
#[derive(Debug)]
pub struct SecondaryResolver {
    working: Store,
    run_id: String,
    mapping_providers: Vec<NamedMappingProvider>,
    sources: Vec<NamedContentProvider>,
}

struct AcquiredContent {
    outcome: ContentImportOutcome,
    sources: Vec<String>,
    transfers: Vec<ContentTransferReport>,
}

struct AcquiredClosure {
    sources: Vec<String>,
    transfers: Vec<ContentTransferReport>,
}

impl SecondaryResolver {
    /// Creates a resolver and validates unique, non-empty capability names.
    ///
    /// The same name may appear once in each list because one configured local
    /// store normally contributes both independent capabilities. `run_id` is
    /// written into neutral local object records after successful acquisition;
    /// records from mapping providers are never opened or copied.
    pub fn new(
        working: Store,
        run_id: impl Into<String>,
        mapping_providers: Vec<NamedMappingProvider>,
        sources: Vec<NamedContentProvider>,
    ) -> Result<Self, StoreError> {
        validate_names(
            "mapping provider",
            mapping_providers.iter().map(|entry| entry.name.as_str()),
        )?;
        validate_names(
            "content source",
            sources.iter().map(|entry| entry.name.as_str()),
        )?;
        Ok(Self {
            working,
            run_id: run_id.into(),
            mapping_providers,
            sources,
        })
    }

    /// Returns the working store populated by successful resolutions.
    pub fn working(&self) -> &Store {
        &self.working
    }

    /// Returns whether any content source can satisfy known-object requests.
    pub fn has_content_sources(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Returns whether any mapping provider can resolve build or reuse keys.
    pub fn has_mapping_providers(&self) -> bool {
        !self.mapping_providers.is_empty()
    }

    async fn query_build_providers(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<(String, Vec<TrustedResolution<BuildKey>>)>, StoreError> {
        let mut tasks = JoinSet::new();
        for (order, entry) in self.mapping_providers.iter().cloned().enumerate() {
            let keys = keys.to_vec();
            tasks.spawn(async move {
                let result = entry.provider.resolve_builds(&keys).await;
                (order, entry.name, result)
            });
        }
        Ok(
            collect_ordered_provider_tasks(tasks, self.mapping_providers.len(), "build mapping")
                .await?
                .into_iter()
                .flatten()
                .collect(),
        )
    }

    async fn query_reuse_providers(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<(String, Vec<TrustedResolution<ReuseKey>>)>, StoreError> {
        let mut tasks = JoinSet::new();
        for (order, entry) in self.mapping_providers.iter().cloned().enumerate() {
            let keys = keys.to_vec();
            tasks.spawn(async move {
                let result = entry.provider.resolve_reuses(&keys).await;
                (order, entry.name, result)
            });
        }
        Ok(
            collect_ordered_provider_tasks(tasks, self.mapping_providers.len(), "reuse mapping")
                .await?
                .into_iter()
                .flatten()
                .collect(),
        )
    }

    async fn locate_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<Vec<HashSet<ObjectHash>>, StoreError> {
        let mut tasks = JoinSet::new();
        for (order, entry) in self.sources.iter().cloned().enumerate() {
            let hashes = hashes.to_vec();
            tasks.spawn(async move {
                let result = entry.source.locate_objects(&hashes).await;
                (order, entry.name, result)
            });
        }
        Ok(
            collect_ordered_provider_tasks(tasks, self.sources.len(), "object availability")
                .await?
                .into_iter()
                .map(|entry| entry.map_or_else(HashSet::new, |(_, hashes)| hashes))
                .collect(),
        )
    }

    async fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<Vec<HashSet<FsFileHash>>, StoreError> {
        let mut tasks = JoinSet::new();
        for (order, entry) in self.sources.iter().cloned().enumerate() {
            let hashes = hashes.to_vec();
            tasks.spawn(async move {
                let result = entry.source.locate_fs_files(&hashes).await;
                (order, entry.name, result)
            });
        }
        Ok(
            collect_ordered_provider_tasks(tasks, self.sources.len(), "fs-file availability")
                .await?
                .into_iter()
                .map(|entry| entry.map_or_else(HashSet::new, |(_, hashes)| hashes))
                .collect(),
        )
    }

    /// Ensures content for already-known object hashes without consulting or
    /// publishing trusted key mappings.
    ///
    /// Duplicate hashes are reported once in first-input order. Complete
    /// working-store objects do not query content sources. Remaining hashes are
    /// located in one batch per source before imports begin.
    pub async fn ensure_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<Vec<KnownObjectResolution>, StoreError> {
        self.ensure_objects_with_progress(hashes, |_| {}).await
    }

    /// Ensures known objects while reporting transfers selected after content
    /// discovery. Mapping lookup is deliberately not part of this callback.
    pub async fn ensure_objects_with_progress(
        &self,
        hashes: &[ObjectHash],
        mut progress: impl FnMut(ContentTransferEvent) + Send,
    ) -> Result<Vec<KnownObjectResolution>, StoreError> {
        let hashes = unique_in_order(hashes);
        let mut need_content = Vec::new();
        for hash in &hashes {
            if !self.working.object_is_complete(*hash)? {
                need_content.push(*hash);
            }
        }
        let availability = if need_content.is_empty() {
            vec![HashSet::new(); self.sources.len()]
        } else {
            self.locate_objects(&need_content).await?
        };

        let mut reports = Vec::with_capacity(hashes.len());
        for hash in hashes {
            if self.working.object_is_complete(hash)? {
                record_existing_object(&self.working, hash, &self.run_id)?;
                reports.push(KnownObjectResolution {
                    object_hash: hash,
                    content_sources: Vec::new(),
                    transfers: Vec::new(),
                    outcome: Some(ContentImportOutcome::AlreadyPresent),
                });
                continue;
            }
            let acquired = self
                .acquire_candidate(hash, &availability, &mut progress)
                .await?;
            if acquired.is_some() {
                record_existing_object(&self.working, hash, &self.run_id)?;
            }
            reports.push(match acquired {
                Some(acquired) => KnownObjectResolution {
                    object_hash: hash,
                    content_sources: acquired.sources,
                    transfers: acquired.transfers,
                    outcome: Some(acquired.outcome),
                },
                None => KnownObjectResolution {
                    object_hash: hash,
                    content_sources: Vec::new(),
                    transfers: Vec::new(),
                    outcome: None,
                },
            });
        }
        Ok(reports)
    }

    /// Resolves trusted build mappings without locating or importing content.
    pub async fn lookup_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<MappingCandidates<BuildKey>>, StoreError> {
        Ok(self
            .lookup_build_groups(keys)
            .await?
            .into_iter()
            .map(mapping_candidates)
            .collect())
    }

    /// Resolves trusted reuse mappings without locating or importing content.
    pub async fn lookup_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<MappingCandidates<ReuseKey>>, StoreError> {
        let keys = unique_in_order(keys);
        let per_index = self.query_reuse_providers(&keys).await?;
        Ok(combine_index_results("reuse", &keys, per_index)?
            .into_iter()
            .map(mapping_candidates)
            .collect())
    }

    /// Resolves exact build mappings and content in input-key order.
    ///
    /// Duplicate input keys are queried and reported once, at their first
    /// position. Every mapping provider is queried before content is imported.
    pub async fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<SecondaryResolution<BuildKey>>, StoreError> {
        let groups = self.lookup_build_groups(keys).await?;
        let availability = self.locate_candidate_objects(&groups).await?;
        let mut reports = Vec::with_capacity(groups.len());
        for group in groups {
            reports.push(
                self.resolve_group(group, &availability, |candidate| {
                    promote_build(
                        &self.working,
                        candidate.key,
                        candidate.object_hash,
                        &self.run_id,
                    )
                })
                .await?,
            );
        }
        Ok(reports)
    }

    async fn lookup_build_groups(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<CandidateGroup<BuildKey>>, StoreError> {
        let keys = unique_in_order(keys);
        let per_index = self.query_build_providers(&keys).await?;
        combine_index_results("build", &keys, per_index)
    }

    /// Resolves reuse mappings and content in input-query order.
    ///
    /// A hit publishes both the reuse mapping and the current build mapping.
    /// Duplicate `(build_key, reuse_key)` queries are reported once.
    pub async fn resolve_reuses(
        &self,
        queries: &[ReuseQuery],
    ) -> Result<Vec<SecondaryResolution<ReuseQuery>>, StoreError> {
        let queries = unique_in_order(queries);
        let reuse_keys = unique_in_order(
            &queries
                .iter()
                .map(|query| query.reuse_key)
                .collect::<Vec<_>>(),
        );
        let mut answers_by_key =
            HashMap::<ReuseKey, Vec<(String, TrustedResolution<ReuseKey>)>>::new();
        for (name, answers) in self.query_reuse_providers(&reuse_keys).await? {
            for answer in answers {
                if !reuse_keys.contains(&answer.key) {
                    return Err(unrequested_key_error("reuse", &answer.key.to_string()));
                }
                answers_by_key
                    .entry(answer.key)
                    .or_default()
                    .push((name.clone(), answer));
            }
        }

        let groups = queries
            .into_iter()
            .map(|query| {
                let answers = answers_by_key
                    .get(&query.reuse_key)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(index, answer)| {
                        (
                            index,
                            TrustedResolution {
                                key: query,
                                object_hash: answer.object_hash,
                            },
                        )
                    })
                    .collect();
                group_answers(query, answers)
            })
            .collect::<Vec<_>>();
        let availability = self.locate_candidate_objects(&groups).await?;
        let mut reports = Vec::with_capacity(groups.len());
        for group in groups {
            reports.push(
                self.resolve_group(group, &availability, |candidate| {
                    promote_reuse(
                        &self.working,
                        candidate.key.build_key,
                        candidate.key.reuse_key,
                        candidate.object_hash,
                        &self.run_id,
                    )
                })
                .await?,
            );
        }
        Ok(reports)
    }

    async fn resolve_group<K, F>(
        &self,
        group: CandidateGroup<K>,
        availability: &[HashSet<ObjectHash>],
        promote: F,
    ) -> Result<SecondaryResolution<K>, StoreError>
    where
        K: Copy,
        F: Fn(&Candidate<K>) -> Result<(), StoreError>,
    {
        let mut report = SecondaryResolution {
            key: group.key,
            answers: group.answers,
            unavailable: Vec::new(),
            resolved: None,
        };

        for candidate in &group.candidates {
            if self.working.object_is_complete(candidate.object_hash)? {
                promote(candidate)?;
                report.resolved = Some(ResolvedSecondaryContent {
                    object_hash: candidate.object_hash,
                    mapping_provider: candidate.provider.clone(),
                    content_sources: Vec::new(),
                    transfers: Vec::new(),
                    import_outcome: ContentImportOutcome::AlreadyPresent,
                });
                return Ok(report);
            }
        }

        let mut ignore_progress = |_: ContentTransferEvent| {};
        for candidate in &group.candidates {
            if let Some(acquired) = self
                .acquire_candidate(candidate.object_hash, availability, &mut ignore_progress)
                .await?
            {
                promote(candidate)?;
                report.resolved = Some(ResolvedSecondaryContent {
                    object_hash: candidate.object_hash,
                    mapping_provider: candidate.provider.clone(),
                    content_sources: acquired.sources,
                    transfers: acquired.transfers,
                    import_outcome: acquired.outcome,
                });
                return Ok(report);
            }
            report.unavailable.push(candidate.object_hash);
        }
        Ok(report)
    }

    async fn locate_candidate_objects<K>(
        &self,
        groups: &[CandidateGroup<K>],
    ) -> Result<Vec<HashSet<ObjectHash>>, StoreError> {
        let mut seen = HashSet::new();
        let mut hashes = Vec::new();
        for group in groups {
            let mut has_complete_local = false;
            for candidate in &group.candidates {
                if self.working.object_is_complete(candidate.object_hash)? {
                    has_complete_local = true;
                    break;
                }
            }
            if has_complete_local {
                continue;
            }
            for candidate in &group.candidates {
                if seen.insert(candidate.object_hash) {
                    hashes.push(candidate.object_hash);
                }
            }
        }
        if hashes.is_empty() {
            return Ok(vec![HashSet::new(); self.sources.len()]);
        }
        self.locate_objects(&hashes).await
    }

    async fn acquire_candidate(
        &self,
        hash: ObjectHash,
        availability: &[HashSet<ObjectHash>],
        progress: &mut (dyn FnMut(ContentTransferEvent) + Send),
    ) -> Result<Option<AcquiredContent>, StoreError> {
        if let Some(working_path) = self.working.object_path(hash)? {
            let Some(manifest) = read_manifest_if_marked(&working_path)? else {
                return Ok(Some(AcquiredContent {
                    outcome: ContentImportOutcome::AlreadyPresent,
                    sources: Vec::new(),
                    transfers: Vec::new(),
                }));
            };
            let Some(closure) = self
                .ensure_manifest_closure(hash, &manifest, progress)
                .await?
            else {
                return Ok(None);
            };
            return Ok(Some(AcquiredContent {
                outcome: ContentImportOutcome::AlreadyPresent,
                sources: closure.sources,
                transfers: closure.transfers,
            }));
        }

        let mut used_sources = Vec::new();
        let mut transfers = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            if !availability[index].contains(&hash) {
                continue;
            }
            let manifest = match source.source.object_manifest(hash).await {
                Ok(manifest) => manifest,
                Err(error) if content_item_failure(&error) => {
                    progress(ContentTransferEvent::Failed {
                        object_hash: hash,
                        content_source: source.name.clone(),
                        transfer_mode: source.source.transfer_mode(),
                        error: error.to_string(),
                    });
                    continue;
                }
                Err(error) => return Err(error.into_store_error()),
            };
            if let Some(manifest) = &manifest {
                let Some(closure) = self
                    .ensure_manifest_closure(hash, manifest, progress)
                    .await?
                else {
                    continue;
                };
                for name in closure.sources {
                    insert_name_once(&mut used_sources, &name);
                }
                transfers.extend(closure.transfers);
            }
            progress(ContentTransferEvent::Started {
                object_hash: hash,
                content_source: source.name.clone(),
                transfer_mode: source.source.transfer_mode(),
            });
            let started = Instant::now();
            let imported = match source.source.import_object(&self.working, hash).await {
                Ok(outcome) => outcome,
                Err(error) if content_item_failure(&error) => {
                    progress(ContentTransferEvent::Failed {
                        object_hash: hash,
                        content_source: source.name.clone(),
                        transfer_mode: source.source.transfer_mode(),
                        error: error.to_string(),
                    });
                    continue;
                }
                Err(error) => return Err(error.into_store_error()),
            };
            match imported.value {
                ContentImportOutcome::NotFound => continue,
                outcome => {
                    insert_name_once(&mut used_sources, &source.name);
                    if outcome == ContentImportOutcome::Imported {
                        let path = self.working.object_path(hash)?.ok_or_else(|| {
                            StoreError::InvalidData(format!(
                                "content source '{}' reported object '{}' imported, but it is absent",
                                source.name, hash
                            ))
                        })?;
                        let (files, bytes) = transferred_path_stats(&path)?;
                        let report = ContentTransferReport {
                            object_hash: hash,
                            content_source: source.name.clone(),
                            transfer_mode: source.source.transfer_mode(),
                            files,
                            bytes,
                            encoded_bytes: imported.encoded_bytes,
                            duration_ms: duration_ms(started),
                        };
                        progress(ContentTransferEvent::Finished(report.clone()));
                        merge_transfer(&mut transfers, report);
                    }
                    return Ok(Some(AcquiredContent {
                        outcome,
                        sources: used_sources,
                        transfers,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn ensure_manifest_closure(
        &self,
        object_hash: ObjectHash,
        manifest: &FsTreeManifest,
        progress: &mut (dyn FnMut(ContentTransferEvent) + Send),
    ) -> Result<Option<AcquiredClosure>, StoreError> {
        let hashes = manifest_fs_files(manifest);
        let mut missing = Vec::new();
        for hash in hashes {
            let path = self.working.fs_file_path(hash);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => {
                    return Err(StoreError::InvalidData(format!(
                        "working fs-file path '{}' is not a regular file",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(hash),
                Err(error) => {
                    return Err(StoreError::Io(format!(
                        "failed to inspect working fs-file '{}': {error}",
                        path.display()
                    )));
                }
            }
        }
        if missing.is_empty() {
            return Ok(Some(AcquiredClosure {
                sources: Vec::new(),
                transfers: Vec::new(),
            }));
        }

        let availability = self.locate_fs_files(&missing).await?;
        let mut unresolved = missing.iter().copied().collect::<HashSet<_>>();
        let mut used_sources = Vec::new();
        let mut transfers = Vec::new();
        for (index, available) in availability.iter().enumerate() {
            let hashes = missing
                .iter()
                .copied()
                .filter(|hash| unresolved.contains(hash) && available.contains(hash))
                .collect::<Vec<_>>();
            if hashes.is_empty() {
                continue;
            }
            let source = &self.sources[index];
            progress(ContentTransferEvent::Started {
                object_hash,
                content_source: source.name.clone(),
                transfer_mode: source.source.transfer_mode(),
            });
            let started = Instant::now();
            let imported = match source.source.import_fs_files(&self.working, &hashes).await {
                Ok(imported) => imported,
                Err(error) if content_item_failure(&error) => {
                    progress(ContentTransferEvent::Failed {
                        object_hash,
                        content_source: source.name.clone(),
                        transfer_mode: source.source.transfer_mode(),
                        error: error.to_string(),
                    });
                    for hash in &hashes {
                        if self.working.fs_file_path(*hash).is_file() {
                            unresolved.remove(hash);
                        }
                    }
                    continue;
                }
                Err(error) => return Err(error.into_store_error()),
            };
            let bytes = hashes.iter().try_fold(0_u64, |total, hash| {
                let path = self.working.fs_file_path(*hash);
                let metadata = fs::symlink_metadata(&path).map_err(|error| {
                        StoreError::Io(format!(
                            "failed to inspect imported fs-file '{}': {error}",
                            path.display()
                        ))
                    })?;
                if !metadata.file_type().is_file() {
                    return Err(StoreError::InvalidData(format!(
                        "content source '{}' reported fs-file '{}' imported, but '{}' is not a regular file",
                        source.name,
                        hash,
                        path.display()
                    )));
                }
                unresolved.remove(hash);
                Ok::<_, StoreError>(total.saturating_add(metadata.len()))
            })?;
            let report = ContentTransferReport {
                object_hash,
                content_source: source.name.clone(),
                transfer_mode: source.source.transfer_mode(),
                files: hashes.len() as u64,
                bytes,
                encoded_bytes: imported.encoded_bytes,
                duration_ms: duration_ms(started),
            };
            progress(ContentTransferEvent::Finished(report.clone()));
            merge_transfer(&mut transfers, report);
            used_sources.push(source.name.clone());
        }
        if !unresolved.is_empty() {
            return Ok(None);
        }
        Ok(Some(AcquiredClosure {
            sources: used_sources,
            transfers,
        }))
    }
}

#[derive(Debug, Clone)]
struct Candidate<K> {
    key: K,
    object_hash: ObjectHash,
    provider: String,
}

#[derive(Debug)]
struct CandidateGroup<K> {
    key: K,
    answers: Vec<MappingAnswer>,
    candidates: Vec<Candidate<K>>,
}

async fn collect_ordered_provider_tasks<T: Send + 'static>(
    mut tasks: JoinSet<(usize, String, Result<T, ProviderError>)>,
    count: usize,
    operation: &str,
) -> Result<Vec<Option<(String, T)>>, StoreError> {
    let mut ordered = (0..count).map(|_| None).collect::<Vec<_>>();
    while let Some(joined) = tasks.join_next().await {
        let (order, name, result) = joined.map_err(|error| {
            StoreError::Io(format!("secondary {operation} task panicked: {error}"))
        })?;
        ordered[order] = Some((name, result));
    }
    ordered
        .into_iter()
        .map(|entry| {
            let (name, result) = entry.expect("every provider task produces one result");
            match result {
                Ok(value) => Ok(Some((name, value))),
                Err(error) if provider_transport_failure(&error) => Ok(None),
                Err(error) => Err(error.into_store_error()),
            }
        })
        .collect()
}

fn provider_transport_failure(error: &ProviderError) -> bool {
    matches!(
        error.repository_kind(),
        Some(RepositoryErrorKind::Transport { .. })
    )
}

fn content_item_failure(error: &ProviderError) -> bool {
    match error {
        ProviderError::Repository(repository) => {
            matches!(repository.kind(), RepositoryErrorKind::Transport { .. })
        }
        ProviderError::RepositoryContent(repository) => matches!(
            repository.kind(),
            RepositoryErrorKind::Transport { .. } | RepositoryErrorKind::InvalidRepository
        ),
        ProviderError::Store(_) => false,
    }
}

fn combine_index_results<K>(
    kind: &str,
    keys: &[K],
    per_index: Vec<(String, Vec<TrustedResolution<K>>)>,
) -> Result<Vec<CandidateGroup<K>>, StoreError>
where
    K: Copy + Eq + Hash + ToString,
{
    let requested = keys.iter().copied().collect::<HashSet<_>>();
    let mut answers_by_key = HashMap::<K, Vec<(String, TrustedResolution<K>)>>::new();
    for (provider, answers) in per_index {
        for answer in answers {
            if !requested.contains(&answer.key) {
                return Err(unrequested_key_error(kind, &answer.key.to_string()));
            }
            answers_by_key
                .entry(answer.key)
                .or_default()
                .push((provider.clone(), answer));
        }
    }
    Ok(keys
        .iter()
        .copied()
        .map(|key| group_answers(key, answers_by_key.remove(&key).unwrap_or_default()))
        .collect())
}

fn mapping_candidates<K>(group: CandidateGroup<K>) -> MappingCandidates<K> {
    MappingCandidates {
        key: group.key,
        object_hashes: group
            .candidates
            .iter()
            .map(|candidate| candidate.object_hash)
            .collect(),
        answers: group.answers,
    }
}

fn group_answers<K>(key: K, answers: Vec<(String, TrustedResolution<K>)>) -> CandidateGroup<K>
where
    K: Copy,
{
    let public_answers = answers
        .iter()
        .map(|(provider, answer)| MappingAnswer {
            provider: provider.clone(),
            object_hash: answer.object_hash,
        })
        .collect();
    let mut seen = HashSet::new();
    let candidates = answers
        .into_iter()
        .filter_map(|(provider, answer)| {
            seen.insert(answer.object_hash).then_some(Candidate {
                key,
                object_hash: answer.object_hash,
                provider,
            })
        })
        .collect();
    CandidateGroup {
        key,
        answers: public_answers,
        candidates,
    }
}

fn manifest_fs_files(manifest: &FsTreeManifest) -> Vec<FsFileHash> {
    let mut seen = HashSet::new();
    manifest
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            FsTreeEntry::File { hash, .. } if seen.insert(*hash) => Some(*hash),
            _ => None,
        })
        .collect()
}

fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn merge_transfer(transfers: &mut Vec<ContentTransferReport>, report: ContentTransferReport) {
    if let Some(existing) = transfers.iter_mut().find(|existing| {
        existing.content_source == report.content_source
            && existing.transfer_mode == report.transfer_mode
    }) {
        existing.files = existing.files.saturating_add(report.files);
        existing.bytes = existing.bytes.saturating_add(report.bytes);
        existing.duration_ms = existing.duration_ms.saturating_add(report.duration_ms);
    } else {
        transfers.push(report);
    }
}

fn transferred_path_stats(path: &Path) -> Result<(u64, u64), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to inspect transferred object '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_file() {
        return Ok((1, metadata.len()));
    }
    if metadata.file_type().is_symlink() {
        return Ok((0, 0));
    }
    if !metadata.file_type().is_dir() {
        return Err(StoreError::InvalidData(format!(
            "transferred object entry '{}' has unsupported file type",
            path.display()
        )));
    }

    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read transferred object directory '{}': {error}",
            path.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            StoreError::Io(format!(
                "failed to read transferred object entry in '{}': {error}",
                path.display()
            ))
        })?;
        let (entry_files, entry_bytes) = transferred_path_stats(&entry.path())?;
        files = files.saturating_add(entry_files);
        bytes = bytes.saturating_add(entry_bytes);
    }
    Ok((files, bytes))
}

fn promote_build(
    working: &Store,
    build_key: BuildKey,
    object_hash: ObjectHash,
    run_id: &str,
) -> Result<(), StoreError> {
    ensure_promotable(working, object_hash)?;
    publish_existing_build_mapping(working, build_key, object_hash, run_id)
}

fn promote_reuse(
    working: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    object_hash: ObjectHash,
    run_id: &str,
) -> Result<(), StoreError> {
    ensure_promotable(working, object_hash)?;
    publish_existing_reuse_mapping(working, build_key, reuse_key, object_hash, run_id)
}

fn ensure_promotable(working: &Store, object_hash: ObjectHash) -> Result<(), StoreError> {
    if !working.object_is_complete(object_hash)? {
        return Err(StoreError::InvalidData(format!(
            "cannot promote mapping for incomplete working object '{}'",
            object_hash
        )));
    }
    Ok(())
}

fn unique_in_order<K>(values: &[K]) -> Vec<K>
where
    K: Copy + Eq + Hash,
{
    let mut seen = HashSet::new();
    values
        .iter()
        .copied()
        .filter(|value| seen.insert(*value))
        .collect()
}

fn validate_names<'a>(
    kind: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty() {
            return Err(StoreError::InvalidInput(format!(
                "secondary {kind} name must not be empty"
            )));
        }
        if !seen.insert(name.to_string()) {
            return Err(StoreError::InvalidInput(format!(
                "duplicate secondary {kind} name '{name}'"
            )));
        }
    }
    Ok(())
}

fn unrequested_key_error(kind: &str, key: &str) -> StoreError {
    StoreError::InvalidData(format!(
        "mapping provider returned unrequested {kind} key '{key}'"
    ))
}

fn insert_name_once(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::CancellationToken;
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::fs_tree::FsTreeEntry;
    use bobr_store::{
        LocalCopyContentSource, LocalHardlinkContentSource, LocalRepository, LocalTrustedKeyIndex,
        ReadOnlyStore, import_build, load_build_object_hash, load_reuse_object_hash,
    };
    use serde_json::Value;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::str::FromStr;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::Barrier;

    fn build_key(byte: char) -> BuildKey {
        BuildKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn reuse_key(byte: char) -> ReuseKey {
        ReuseKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn empty_store(path: &Path) -> Store {
        fs::create_dir(path).unwrap();
        Store::create(path).unwrap()
    }

    fn local_repository(path: &Path) -> LocalRepository {
        LocalRepository::new(ReadOnlyStore::open(path).unwrap())
    }

    fn object_record_path(store: &Store, hash: ObjectHash) -> PathBuf {
        store
            .root()
            .join("object-records")
            .join(format!("{}.json", hash.to_hex()))
    }

    fn local_io() -> LocalIoScheduler {
        LocalIoScheduler::new(8, CancellationToken::new()).unwrap()
    }

    #[test]
    fn provider_error_preserves_repository_category() {
        let error = ProviderError::from(RepositoryError::cancelled("cancelled by test"));
        assert_eq!(
            error.repository_kind(),
            Some(RepositoryErrorKind::Cancelled)
        );
        let metadata = ProviderError::from(RepositoryError::invalid_repository("bad list"));
        let content =
            ProviderError::repository_content(RepositoryError::invalid_repository("bad object"));
        assert!(!content_item_failure(&metadata));
        assert!(content_item_failure(&content));
    }

    fn publish_file(
        store: &Store,
        build: BuildKey,
        reuse: ReuseKey,
        bytes: &[u8],
        staged: &Path,
    ) -> ObjectHash {
        fs::write(staged, bytes).unwrap();
        import_build(
            store,
            build,
            reuse,
            Vec::new(),
            staged,
            &format!("object-{}", build),
            "test-run",
        )
        .unwrap()
    }

    fn index(name: &str, root: &Path) -> NamedMappingProvider {
        NamedMappingProvider::new(
            name,
            Arc::new(LocalMappingProvider::new(
                Arc::new(LocalTrustedKeyIndex::new(local_repository(root))),
                local_io(),
            )),
        )
    }

    fn source(name: &str, root: &Path) -> NamedContentProvider {
        NamedContentProvider::new(
            name,
            Arc::new(LocalContentProvider::new(
                Arc::new(LocalHardlinkContentSource::with_runtime(
                    local_repository(root),
                    RuntimeProvider::host(),
                )),
                local_io(),
            )),
        )
    }

    fn copy_source(name: &str, root: &Path) -> NamedContentProvider {
        NamedContentProvider::new(
            name,
            Arc::new(LocalContentProvider::new(
                Arc::new(LocalCopyContentSource::with_runtime(
                    local_repository(root),
                    RuntimeProvider::host(),
                )),
                local_io(),
            )),
        )
    }

    fn resolver(
        working: Store,
        indexes: Vec<NamedMappingProvider>,
        sources: Vec<NamedContentProvider>,
    ) -> SecondaryResolver {
        SecondaryResolver::new(working, "test-run", indexes, sources).unwrap()
    }

    #[derive(Debug)]
    struct UnexpectedContentSource;

    impl ContentSource for UnexpectedContentSource {
        fn transfer_mode(&self) -> ContentTransferMode {
            ContentTransferMode::Hardlink
        }

        fn locate_objects(
            &self,
            _hashes: &[ObjectHash],
        ) -> Result<HashSet<ObjectHash>, StoreError> {
            Err(StoreError::InvalidData(
                "content source was queried for a complete local candidate".to_string(),
            ))
        }

        fn object_manifest(&self, _hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError> {
            unreachable!()
        }

        fn locate_fs_files(
            &self,
            _hashes: &[FsFileHash],
        ) -> Result<HashSet<FsFileHash>, StoreError> {
            unreachable!()
        }

        fn import_fs_files(
            &self,
            _working: &Store,
            _hashes: &[FsFileHash],
        ) -> Result<(), StoreError> {
            unreachable!()
        }

        fn import_object(
            &self,
            _working: &Store,
            _hash: ObjectHash,
        ) -> Result<ContentImportOutcome, StoreError> {
            unreachable!()
        }
    }

    #[derive(Debug)]
    struct BarrierMappingProvider {
        barrier: Arc<Barrier>,
        object_hash: ObjectHash,
        delay: Duration,
    }

    #[derive(Debug)]
    struct TransportFailingMappingProvider;

    #[derive(Debug)]
    struct AuthenticationFailingMappingProvider;

    #[async_trait]
    impl MappingProvider for TransportFailingMappingProvider {
        async fn resolve_builds(
            &self,
            _keys: &[BuildKey],
        ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError> {
            Err(RepositoryError::transport("repository unavailable", false).into())
        }

        async fn resolve_reuses(
            &self,
            _keys: &[ReuseKey],
        ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError> {
            Err(RepositoryError::transport("repository unavailable", false).into())
        }
    }

    #[async_trait]
    impl MappingProvider for AuthenticationFailingMappingProvider {
        async fn resolve_builds(
            &self,
            _keys: &[BuildKey],
        ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError> {
            Err(RepositoryError::authentication("master signature is invalid").into())
        }

        async fn resolve_reuses(
            &self,
            _keys: &[ReuseKey],
        ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError> {
            Err(RepositoryError::authentication("master signature is invalid").into())
        }
    }

    #[derive(Debug)]
    struct BrokenRemoteContentProvider {
        advertised: ObjectHash,
    }

    #[derive(Debug)]
    struct BrokenRemoteFsContentProvider {
        advertised: FsFileHash,
    }

    #[async_trait]
    impl ContentProvider for BrokenRemoteFsContentProvider {
        fn transfer_mode(&self) -> ContentTransferMode {
            ContentTransferMode::Download
        }

        async fn locate_objects(
            &self,
            _hashes: &[ObjectHash],
        ) -> Result<HashSet<ObjectHash>, ProviderError> {
            Ok(HashSet::new())
        }

        async fn object_manifest(
            &self,
            _hash: ObjectHash,
        ) -> Result<Option<FsTreeManifest>, ProviderError> {
            unreachable!("this provider only advertises fs-files")
        }

        async fn locate_fs_files(
            &self,
            hashes: &[FsFileHash],
        ) -> Result<HashSet<FsFileHash>, ProviderError> {
            Ok(hashes
                .iter()
                .copied()
                .filter(|hash| *hash == self.advertised)
                .collect())
        }

        async fn import_fs_files(
            &self,
            _working: &Store,
            _hashes: &[FsFileHash],
        ) -> Result<ContentProviderImport<()>, ProviderError> {
            Err(ProviderError::repository_content(
                RepositoryError::invalid_repository("advertised fs-file is corrupt"),
            ))
        }

        async fn import_object(
            &self,
            _working: &Store,
            _hash: ObjectHash,
        ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError> {
            unreachable!("this provider only advertises fs-files")
        }
    }

    #[async_trait]
    impl ContentProvider for BrokenRemoteContentProvider {
        fn transfer_mode(&self) -> ContentTransferMode {
            ContentTransferMode::Download
        }

        async fn locate_objects(
            &self,
            hashes: &[ObjectHash],
        ) -> Result<HashSet<ObjectHash>, ProviderError> {
            Ok(hashes
                .iter()
                .copied()
                .filter(|hash| *hash == self.advertised)
                .collect())
        }

        async fn object_manifest(
            &self,
            _hash: ObjectHash,
        ) -> Result<Option<FsTreeManifest>, ProviderError> {
            Err(ProviderError::repository_content(
                RepositoryError::invalid_repository("advertised object is corrupt"),
            ))
        }

        async fn locate_fs_files(
            &self,
            _hashes: &[FsFileHash],
        ) -> Result<HashSet<FsFileHash>, ProviderError> {
            Ok(HashSet::new())
        }

        async fn import_fs_files(
            &self,
            _working: &Store,
            _hashes: &[FsFileHash],
        ) -> Result<ContentProviderImport<()>, ProviderError> {
            unreachable!("ordinary objects have no fs-file closure")
        }

        async fn import_object(
            &self,
            _working: &Store,
            _hash: ObjectHash,
        ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError> {
            unreachable!("manifest probe rejects the corrupt object")
        }
    }

    #[async_trait]
    impl MappingProvider for BarrierMappingProvider {
        async fn resolve_builds(
            &self,
            keys: &[BuildKey],
        ) -> Result<Vec<TrustedResolution<BuildKey>>, ProviderError> {
            self.barrier.wait().await;
            tokio::time::sleep(self.delay).await;
            Ok(keys
                .first()
                .copied()
                .map(|key| TrustedResolution {
                    key,
                    object_hash: self.object_hash,
                })
                .into_iter()
                .collect())
        }

        async fn resolve_reuses(
            &self,
            _keys: &[ReuseKey],
        ) -> Result<Vec<TrustedResolution<ReuseKey>>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug)]
    struct BarrierContentProvider {
        barrier: Arc<Barrier>,
    }

    #[async_trait]
    impl ContentProvider for BarrierContentProvider {
        fn transfer_mode(&self) -> ContentTransferMode {
            ContentTransferMode::Copy
        }

        async fn locate_objects(
            &self,
            _hashes: &[ObjectHash],
        ) -> Result<HashSet<ObjectHash>, ProviderError> {
            self.barrier.wait().await;
            Ok(HashSet::new())
        }

        async fn object_manifest(
            &self,
            _hash: ObjectHash,
        ) -> Result<Option<FsTreeManifest>, ProviderError> {
            unreachable!("unavailable objects have no manifest")
        }

        async fn locate_fs_files(
            &self,
            _hashes: &[FsFileHash],
        ) -> Result<HashSet<FsFileHash>, ProviderError> {
            unreachable!("unavailable objects have no fs-files")
        }

        async fn import_fs_files(
            &self,
            _working: &Store,
            _hashes: &[FsFileHash],
        ) -> Result<ContentProviderImport<()>, ProviderError> {
            unreachable!("unavailable fs-files are not imported")
        }

        async fn import_object(
            &self,
            _working: &Store,
            _hash: ObjectHash,
        ) -> Result<ContentProviderImport<ContentImportOutcome>, ProviderError> {
            unreachable!("unavailable objects are not imported")
        }
    }

    #[test]
    fn local_backend_registry_reuses_canonical_store_aliases() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("repository");
        empty_store(&root);
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();

        let mut registry = LocalBackendRegistry::default();
        let direct = registry.open(&root).unwrap();
        let through_alias = registry.open(&alias).unwrap();

        assert_eq!(registry.len(), 1);
        assert_eq!(direct.store().root(), through_alias.store().root());
    }

    #[tokio::test]
    async fn mapping_queries_run_concurrently_but_merge_in_provider_order() {
        let temp = tempdir().unwrap();
        let working = empty_store(&temp.path().join("working"));
        let barrier = Arc::new(Barrier::new(2));
        let first_hash = ObjectHash::from_str(&"a".repeat(64)).unwrap();
        let second_hash = ObjectHash::from_str(&"b".repeat(64)).unwrap();
        let resolver = resolver(
            working,
            vec![
                NamedMappingProvider::new(
                    "first",
                    Arc::new(BarrierMappingProvider {
                        barrier: barrier.clone(),
                        object_hash: first_hash,
                        delay: Duration::from_millis(25),
                    }),
                ),
                NamedMappingProvider::new(
                    "second",
                    Arc::new(BarrierMappingProvider {
                        barrier,
                        object_hash: second_hash,
                        delay: Duration::ZERO,
                    }),
                ),
            ],
            Vec::new(),
        );

        let reports = tokio::time::timeout(
            Duration::from_secs(1),
            resolver.lookup_builds(&[build_key('1')]),
        )
        .await
        .expect("mapping providers were queried sequentially")
        .unwrap();

        assert_eq!(
            reports[0]
                .answers
                .iter()
                .map(|answer| (answer.provider.as_str(), answer.object_hash))
                .collect::<Vec<_>>(),
            vec![("first", first_hash), ("second", second_hash)]
        );
    }

    #[tokio::test]
    async fn exhausted_mapping_transport_falls_back_to_the_next_provider() {
        let temp = tempdir().unwrap();
        let repository_root = temp.path().join("repository");
        let working = empty_store(&temp.path().join("working"));
        let repository = empty_store(&repository_root);
        let build = build_key('3');
        let hash = publish_file(
            &repository,
            build,
            reuse_key('4'),
            b"mapping fallback\n",
            &temp.path().join("staged"),
        );
        let resolver = resolver(
            working,
            vec![
                NamedMappingProvider::new("unavailable", Arc::new(TransportFailingMappingProvider)),
                index("available", &repository_root),
            ],
            Vec::new(),
        );

        let report = resolver.lookup_builds(&[build]).await.unwrap().remove(0);

        assert_eq!(report.object_hashes, [hash]);
        assert_eq!(report.answers[0].provider, "available");
    }

    #[tokio::test]
    async fn mapping_authentication_failure_is_fatal() {
        let temp = tempdir().unwrap();
        let working = empty_store(&temp.path().join("working"));
        let resolver = resolver(
            working,
            vec![NamedMappingProvider::new(
                "untrusted",
                Arc::new(AuthenticationFailingMappingProvider),
            )],
            Vec::new(),
        );

        let error = resolver.lookup_builds(&[build_key('7')]).await.unwrap_err();

        assert!(matches!(error, StoreError::InvalidData(_)));
        assert!(error.to_string().contains("master signature is invalid"));
    }

    #[tokio::test]
    async fn content_availability_queries_run_concurrently() {
        let temp = tempdir().unwrap();
        let working = empty_store(&temp.path().join("working"));
        let barrier = Arc::new(Barrier::new(2));
        let content = ["first", "second"]
            .into_iter()
            .map(|name| {
                NamedContentProvider::new(
                    name,
                    Arc::new(BarrierContentProvider {
                        barrier: barrier.clone(),
                    }),
                )
            })
            .collect();
        let resolver = resolver(working, Vec::new(), content);
        let hash = ObjectHash::from_str(&"a".repeat(64)).unwrap();

        let reports =
            tokio::time::timeout(Duration::from_secs(1), resolver.ensure_objects(&[hash]))
                .await
                .expect("content providers were queried sequentially")
                .unwrap();

        assert_eq!(reports[0].object_hash, hash);
        assert_eq!(reports[0].outcome, None);
    }

    #[tokio::test]
    async fn trusted_mapping_and_content_can_come_from_different_stores() {
        let temp = tempdir().unwrap();
        let index_root = temp.path().join("index");
        let content_root = temp.path().join("content");
        let working_root = temp.path().join("working");
        let index_store = empty_store(&index_root);
        let content_store = empty_store(&content_root);
        let working = empty_store(&working_root);
        let build = build_key('1');
        let object_hash = publish_file(
            &index_store,
            build,
            reuse_key('2'),
            b"shared content\n",
            &temp.path().join("index-staged"),
        );
        let content_hash = publish_file(
            &content_store,
            build_key('3'),
            reuse_key('4'),
            b"shared content\n",
            &temp.path().join("content-staged"),
        );
        assert_eq!(content_hash, object_hash);
        fs::remove_file(index_store.object_path(object_hash).unwrap().unwrap()).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![index("trusted-index", &index_root)],
            vec![source("content-mirror", &content_root)],
        );
        let reports = resolver.resolve_builds(&[build]).await.unwrap();

        assert_eq!(reports.len(), 1);
        let resolved = reports[0].resolved.as_ref().unwrap();
        assert_eq!(resolved.object_hash, object_hash);
        assert_eq!(resolved.mapping_provider, "trusted-index");
        assert_eq!(resolved.content_sources, ["content-mirror"]);
        assert_eq!(resolved.import_outcome, ContentImportOutcome::Imported);
        assert_eq!(resolved.transfers.len(), 1);
        assert_eq!(resolved.transfers[0].content_source, "content-mirror");
        assert_eq!(
            resolved.transfers[0].transfer_mode,
            ContentTransferMode::Hardlink
        );
        assert_eq!(resolved.transfers[0].files, 1);
        assert_eq!(
            resolved.transfers[0].bytes,
            b"shared content\n".len() as u64
        );
        assert_eq!(
            load_build_object_hash(&working, build).unwrap(),
            Some(object_hash)
        );
        let local_record: Value =
            serde_json::from_slice(&fs::read(object_record_path(&working, object_hash)).unwrap())
                .unwrap();
        assert_eq!(
            local_record["build_key"],
            BuildKey::from_object_hash(object_hash).to_string()
        );
        assert_eq!(local_record["run_id"], "test-run");
        assert_eq!(local_record["inputs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn corrupt_advertised_object_falls_back_to_the_next_content_provider() {
        let temp = tempdir().unwrap();
        let repository_root = temp.path().join("repository");
        let working = empty_store(&temp.path().join("working"));
        let repository = empty_store(&repository_root);
        let hash = publish_file(
            &repository,
            build_key('5'),
            reuse_key('6'),
            b"content fallback\n",
            &temp.path().join("staged"),
        );
        let resolver = resolver(
            working.clone(),
            Vec::new(),
            vec![
                NamedContentProvider::new(
                    "broken-remote",
                    Arc::new(BrokenRemoteContentProvider { advertised: hash }),
                ),
                source("available-local", &repository_root),
            ],
        );

        let mut events = Vec::new();
        let report = resolver
            .ensure_objects_with_progress(&[hash], |event| events.push(event))
            .await
            .unwrap()
            .remove(0);

        assert_eq!(report.outcome, Some(ContentImportOutcome::Imported));
        assert_eq!(report.content_sources, ["available-local"]);
        assert!(matches!(
            events.first(),
            Some(ContentTransferEvent::Failed { content_source, .. })
                if content_source == "broken-remote"
        ));
        assert!(working.object_is_complete(hash).unwrap());
        assert!(object_record_path(&working, hash).is_file());
    }

    #[tokio::test]
    async fn known_secondary_content_gets_a_new_neutral_working_record() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let secondary = empty_store(&secondary_root);
        let working = empty_store(&working_root);
        let object_hash = publish_file(
            &secondary,
            build_key('a'),
            reuse_key('b'),
            b"known content\n",
            &temp.path().join("staged"),
        );
        let resolver = resolver(
            working.clone(),
            Vec::new(),
            vec![source("secondary", &secondary_root)],
        );

        let resolution = resolver
            .ensure_objects(&[object_hash])
            .await
            .unwrap()
            .remove(0);

        assert_eq!(resolution.outcome, Some(ContentImportOutcome::Imported));
        let local_record: Value =
            serde_json::from_slice(&fs::read(object_record_path(&working, object_hash)).unwrap())
                .unwrap();
        assert_eq!(
            local_record["build_key"],
            BuildKey::from_object_hash(object_hash).to_string()
        );
        assert_eq!(local_record["run_id"], "test-run");
        assert_eq!(local_record["inputs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn complete_working_candidate_beats_an_earlier_nonlocal_conflict() {
        let temp = tempdir().unwrap();
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        let working_root = temp.path().join("working");
        let first = empty_store(&first_root);
        let second = empty_store(&second_root);
        let working = empty_store(&working_root);
        let build = build_key('5');
        let x = publish_file(
            &first,
            build,
            reuse_key('6'),
            b"first result\n",
            &temp.path().join("first-staged"),
        );
        let y = publish_file(
            &second,
            build,
            reuse_key('7'),
            b"second result\n",
            &temp.path().join("second-staged"),
        );
        let second_source = LocalHardlinkContentSource::with_runtime(
            local_repository(&second_root),
            RuntimeProvider::host(),
        );
        second_source.import_object(&working, y).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![
                index("first-index", &first_root),
                index("second-index", &second_root),
            ],
            vec![NamedContentProvider::new(
                "must-not-be-queried",
                Arc::new(LocalContentProvider::new(
                    Arc::new(UnexpectedContentSource),
                    local_io(),
                )),
            )],
        );
        let report = resolver.resolve_builds(&[build]).await.unwrap().remove(0);

        assert!(report.has_conflict());
        let resolved = report.resolved.unwrap();
        assert_eq!(resolved.object_hash, y);
        assert_eq!(resolved.mapping_provider, "second-index");
        assert!(resolved.content_sources.is_empty());
        assert_eq!(
            resolved.import_outcome,
            ContentImportOutcome::AlreadyPresent
        );
        assert!(working.object_path(x).unwrap().is_none());
        assert_eq!(load_build_object_hash(&working, build).unwrap(), Some(y));
    }

    #[tokio::test]
    async fn mapping_priority_selects_first_when_both_candidates_are_local() {
        let temp = tempdir().unwrap();
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        let working_root = temp.path().join("working");
        let first = empty_store(&first_root);
        let second = empty_store(&second_root);
        let working = empty_store(&working_root);
        let build = build_key('8');
        let x = publish_file(
            &first,
            build,
            reuse_key('9'),
            b"first local\n",
            &temp.path().join("first-staged"),
        );
        let y = publish_file(
            &second,
            build,
            reuse_key('a'),
            b"second local\n",
            &temp.path().join("second-staged"),
        );
        LocalHardlinkContentSource::with_runtime(
            local_repository(&first_root),
            RuntimeProvider::host(),
        )
        .import_object(&working, x)
        .unwrap();
        LocalHardlinkContentSource::with_runtime(
            local_repository(&second_root),
            RuntimeProvider::host(),
        )
        .import_object(&working, y)
        .unwrap();

        let resolver = resolver(
            working,
            vec![
                index("first-index", &first_root),
                index("second-index", &second_root),
            ],
            Vec::new(),
        );
        let report = resolver.resolve_builds(&[build]).await.unwrap().remove(0);
        assert_eq!(report.resolved.unwrap().object_hash, x);
    }

    #[tokio::test]
    async fn stale_first_mapping_falls_back_to_next_available_candidate() {
        let temp = tempdir().unwrap();
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        let working_root = temp.path().join("working");
        let first = empty_store(&first_root);
        let second = empty_store(&second_root);
        let working = empty_store(&working_root);
        let build = build_key('b');
        let x = publish_file(
            &first,
            build,
            reuse_key('c'),
            b"stale result\n",
            &temp.path().join("first-staged"),
        );
        let y = publish_file(
            &second,
            build,
            reuse_key('d'),
            b"available result\n",
            &temp.path().join("second-staged"),
        );
        fs::remove_file(first.object_path(x).unwrap().unwrap()).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![
                index("stale-index", &first_root),
                index("good-index", &second_root),
            ],
            vec![source("good-content", &second_root)],
        );
        let report = resolver.resolve_builds(&[build]).await.unwrap().remove(0);

        assert_eq!(report.unavailable, [x]);
        assert_eq!(report.resolved.unwrap().object_hash, y);
        assert_eq!(load_build_object_hash(&working, build).unwrap(), Some(y));
    }

    #[tokio::test]
    async fn unavailable_content_never_publishes_trusted_mapping() {
        let temp = tempdir().unwrap();
        let index_root = temp.path().join("index");
        let working_root = temp.path().join("working");
        let index_store = empty_store(&index_root);
        let working = empty_store(&working_root);
        let build = build_key('e');
        let object_hash = publish_file(
            &index_store,
            build,
            reuse_key('f'),
            b"missing content\n",
            &temp.path().join("staged"),
        );
        fs::remove_file(index_store.object_path(object_hash).unwrap().unwrap()).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![index("index", &index_root)],
            Vec::new(),
        );
        let report = resolver.resolve_builds(&[build]).await.unwrap().remove(0);

        assert!(report.resolved.is_none());
        assert_eq!(report.unavailable, [object_hash]);
        assert_eq!(load_build_object_hash(&working, build).unwrap(), None);
        assert!(!object_record_path(&working, object_hash).exists());
    }

    #[tokio::test]
    async fn reuse_hit_publishes_reuse_and_current_build_mappings() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let secondary = empty_store(&secondary_root);
        let working = empty_store(&working_root);
        let reuse = reuse_key('1');
        let old_build = build_key('2');
        let current_build = build_key('3');
        let object_hash = publish_file(
            &secondary,
            old_build,
            reuse,
            b"reuse result\n",
            &temp.path().join("staged"),
        );
        let resolver = resolver(
            working.clone(),
            vec![index("index", &secondary_root)],
            vec![source("content", &secondary_root)],
        );

        let report = resolver
            .resolve_reuses(&[ReuseQuery {
                build_key: current_build,
                reuse_key: reuse,
            }])
            .await
            .unwrap()
            .remove(0);

        assert_eq!(report.resolved.unwrap().object_hash, object_hash);
        assert_eq!(
            load_build_object_hash(&working, current_build).unwrap(),
            Some(object_hash)
        );
        assert_eq!(
            load_reuse_object_hash(&working, reuse).unwrap(),
            Some(object_hash)
        );
    }

    #[tokio::test]
    async fn fs_tree_manifest_and_fs_file_can_come_from_different_content_sources() {
        let temp = tempdir().unwrap();
        let manifest_root = temp.path().join("manifest-store");
        let file_root = temp.path().join("file-store");
        let working_root = temp.path().join("working");
        let manifest_store = empty_store(&manifest_root);
        let file_store = empty_store(&file_root);
        let working = empty_store(&working_root);
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"split closure\n").unwrap();
        let manifest = manifest_store.fs_tree().intern_tree(tree).unwrap();
        let fs_file_hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let staged_manifest = temp.path().join("manifest");
        manifest.write_canonical(&staged_manifest).unwrap();
        let build = build_key('4');
        let object_hash = import_build(
            &manifest_store,
            build,
            reuse_key('5'),
            Vec::new(),
            &staged_manifest,
            "manifest",
            "test-run",
        )
        .unwrap();
        let source_fs_file = manifest_store.fs_file_path(fs_file_hash);
        let destination_fs_file = file_store.fs_file_path(fs_file_hash);
        fs::create_dir(destination_fs_file.parent().unwrap()).unwrap();
        fs::hard_link(&source_fs_file, &destination_fs_file).unwrap();
        fs::remove_file(source_fs_file).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![index("manifest-index", &manifest_root)],
            vec![
                source("manifest-content", &manifest_root),
                NamedContentProvider::new(
                    "broken-file-content",
                    Arc::new(BrokenRemoteFsContentProvider {
                        advertised: fs_file_hash,
                    }),
                ),
                copy_source("file-content", &file_root),
            ],
        );
        let report = resolver.resolve_builds(&[build]).await.unwrap().remove(0);

        let resolved = report.resolved.unwrap();
        assert_eq!(resolved.object_hash, object_hash);
        assert_eq!(
            resolved.content_sources,
            ["file-content", "manifest-content"]
        );
        assert_eq!(resolved.transfers.len(), 2);
        assert_eq!(resolved.transfers[0].content_source, "file-content");
        assert_eq!(
            resolved.transfers[0].transfer_mode,
            ContentTransferMode::Copy
        );
        assert_eq!(resolved.transfers[0].files, 1);
        assert_eq!(resolved.transfers[1].content_source, "manifest-content");
        assert_eq!(
            resolved.transfers[1].transfer_mode,
            ContentTransferMode::Hardlink
        );
        assert_eq!(resolved.transfers[1].files, 1);
        let source_metadata = fs::metadata(file_store.fs_file_path(fs_file_hash)).unwrap();
        let working_metadata = fs::metadata(working.fs_file_path(fs_file_hash)).unwrap();
        assert_eq!(source_metadata.dev(), working_metadata.dev());
        assert_ne!(source_metadata.ino(), working_metadata.ino());
    }

    #[test]
    fn duplicate_or_empty_capability_names_are_rejected() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let working_root = temp.path().join("working");
        empty_store(&root);
        let working = empty_store(&working_root);
        let duplicate = SecondaryResolver::new(
            working.clone(),
            "test-run",
            vec![index("same", &root), index("same", &root)],
            Vec::new(),
        )
        .unwrap_err();
        assert!(
            duplicate
                .to_string()
                .contains("duplicate secondary mapping provider")
        );

        let empty =
            SecondaryResolver::new(working, "test-run", Vec::new(), vec![source("", &root)])
                .unwrap_err();
        assert!(empty.to_string().contains("name must not be empty"));
    }
}
