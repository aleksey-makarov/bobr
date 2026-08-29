//! Mapping-first, content-second resolution across secondary-store capabilities.

use crate::fs_tree::{FsFileHash, FsTreeEntry, FsTreeManifest, read_manifest_if_marked};
use crate::{
    ContentImportOutcome, ContentSource, ContentTransferMode, Store, StoreError, TrustedKeyIndex,
    TrustedResolution,
};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hash;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// One named trusted key-index capability in configured priority order.
#[derive(Debug, Clone)]
pub struct NamedTrustedKeyIndex {
    name: String,
    index: Arc<dyn TrustedKeyIndex>,
}

impl NamedTrustedKeyIndex {
    /// Names one trusted index for diagnostics and priority selection.
    pub fn new(name: impl Into<String>, index: Arc<dyn TrustedKeyIndex>) -> Self {
        Self {
            name: name.into(),
            index,
        }
    }

    /// Returns the configured diagnostic name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One named content-source capability in configured priority order.
#[derive(Debug, Clone)]
pub struct NamedContentSource {
    name: String,
    source: Arc<dyn ContentSource>,
}

impl NamedContentSource {
    /// Names one content source for diagnostics and priority selection.
    pub fn new(name: impl Into<String>, source: Arc<dyn ContentSource>) -> Self {
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

/// One trusted index answer retained for conflict diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedAnswer {
    /// Index that supplied the answer.
    pub index: String,
    /// Object named by the index.
    pub object_hash: ObjectHash,
}

/// Ordered, hash-only answers for one trusted build or reuse lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingCandidates<K> {
    /// Queried build or reuse key.
    pub key: K,
    /// Every trusted answer in index priority order, including agreements.
    pub answers: Vec<TrustedAnswer>,
    /// Distinct object hashes in first-answer order.
    pub object_hashes: Vec<ObjectHash>,
}

impl<K> MappingCandidates<K> {
    /// Returns true when trusted indexes named more than one distinct hash.
    pub fn has_conflict(&self) -> bool {
        self.object_hashes.len() > 1
    }
}

/// Successfully selected and imported secondary result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSecondaryContent {
    /// Selected object hash.
    pub object_hash: ObjectHash,
    /// First trusted index that named the selected candidate.
    pub mapping_index: String,
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
}

/// Mapping/content resolution report for one queried key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryResolution<K> {
    /// Queried build or reuse identity.
    pub key: K,
    /// Every trusted answer in index priority order.
    pub answers: Vec<TrustedAnswer>,
    /// Candidate hashes that no complete set of content sources could provide.
    pub unavailable: Vec<ObjectHash>,
    /// Selected result, or `None` for a complete secondary miss.
    pub resolved: Option<ResolvedSecondaryContent>,
}

impl<K> SecondaryResolution<K> {
    /// Returns true when trusted indexes named more than one distinct hash.
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

/// Coordinator performing trusted mapping lookup before independent content
/// acquisition and promotion into one working store.
#[derive(Debug)]
pub struct SecondaryResolver {
    working: Store,
    run_id: String,
    indexes: Vec<NamedTrustedKeyIndex>,
    sources: Vec<NamedContentSource>,
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
    /// records from trusted indexes are never opened or copied.
    pub fn new(
        working: Store,
        run_id: impl Into<String>,
        indexes: Vec<NamedTrustedKeyIndex>,
        sources: Vec<NamedContentSource>,
    ) -> Result<Self, StoreError> {
        validate_names(
            "trusted index",
            indexes.iter().map(|entry| entry.name.as_str()),
        )?;
        validate_names(
            "content source",
            sources.iter().map(|entry| entry.name.as_str()),
        )?;
        Ok(Self {
            working,
            run_id: run_id.into(),
            indexes,
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

    /// Returns whether any trusted index can resolve build or reuse keys.
    pub fn has_trusted_indexes(&self) -> bool {
        !self.indexes.is_empty()
    }

    /// Ensures content for already-known object hashes without consulting or
    /// publishing trusted key mappings.
    ///
    /// Duplicate hashes are reported once in first-input order. Complete
    /// working-store objects do not query content sources. Remaining hashes are
    /// located in one batch per source before imports begin.
    pub fn ensure_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<Vec<KnownObjectResolution>, StoreError> {
        self.ensure_objects_with_progress(hashes, |_| {})
    }

    /// Ensures known objects while reporting transfers selected after content
    /// discovery. Mapping lookup is deliberately not part of this callback.
    pub fn ensure_objects_with_progress(
        &self,
        hashes: &[ObjectHash],
        mut progress: impl FnMut(ContentTransferEvent),
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
            self.sources
                .iter()
                .map(|source| source.source.locate_objects(&need_content))
                .collect::<Result<Vec<_>, _>>()?
        };

        let mut reports = Vec::with_capacity(hashes.len());
        for hash in hashes {
            if self.working.object_is_complete(hash)? {
                crate::record::record_existing_object(&self.working, hash, &self.run_id)?;
                reports.push(KnownObjectResolution {
                    object_hash: hash,
                    content_sources: Vec::new(),
                    transfers: Vec::new(),
                    outcome: Some(ContentImportOutcome::AlreadyPresent),
                });
                continue;
            }
            let acquired = self.acquire_candidate(hash, &availability, &mut progress)?;
            if acquired.is_some() {
                crate::record::record_existing_object(&self.working, hash, &self.run_id)?;
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
    pub fn lookup_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<MappingCandidates<BuildKey>>, StoreError> {
        Ok(self
            .lookup_build_groups(keys)?
            .into_iter()
            .map(mapping_candidates)
            .collect())
    }

    /// Resolves trusted reuse mappings without locating or importing content.
    pub fn lookup_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<MappingCandidates<ReuseKey>>, StoreError> {
        let keys = unique_in_order(keys);
        let mut per_index = Vec::with_capacity(self.indexes.len());
        for entry in &self.indexes {
            per_index.push((entry.name.clone(), entry.index.resolve_reuses(&keys)?));
        }
        Ok(combine_index_results("reuse", &keys, per_index)?
            .into_iter()
            .map(mapping_candidates)
            .collect())
    }

    /// Resolves exact build mappings and content in input-key order.
    ///
    /// Duplicate input keys are queried and reported once, at their first
    /// position. Every trusted index is queried before any content is imported.
    pub fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<SecondaryResolution<BuildKey>>, StoreError> {
        let groups = self.lookup_build_groups(keys)?;
        let availability = self.locate_candidate_objects(&groups)?;
        groups
            .into_iter()
            .map(|group| {
                self.resolve_group(group, &availability, |candidate| {
                    promote_build(
                        &self.working,
                        candidate.key,
                        candidate.object_hash,
                        &self.run_id,
                    )
                })
            })
            .collect()
    }

    fn lookup_build_groups(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<CandidateGroup<BuildKey>>, StoreError> {
        let keys = unique_in_order(keys);
        let mut per_index = Vec::with_capacity(self.indexes.len());
        for entry in &self.indexes {
            per_index.push((entry.name.clone(), entry.index.resolve_builds(&keys)?));
        }
        combine_index_results("build", &keys, per_index)
    }

    /// Resolves reuse mappings and content in input-query order.
    ///
    /// A hit publishes both the reuse mapping and the current build mapping.
    /// Duplicate `(build_key, reuse_key)` queries are reported once.
    pub fn resolve_reuses(
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
        for entry in &self.indexes {
            for answer in entry.index.resolve_reuses(&reuse_keys)? {
                if !reuse_keys.contains(&answer.key) {
                    return Err(unrequested_key_error("reuse", &answer.key.to_string()));
                }
                answers_by_key
                    .entry(answer.key)
                    .or_default()
                    .push((entry.name.clone(), answer));
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
        let availability = self.locate_candidate_objects(&groups)?;
        groups
            .into_iter()
            .map(|group| {
                self.resolve_group(group, &availability, |candidate| {
                    promote_reuse(
                        &self.working,
                        candidate.key.build_key,
                        candidate.key.reuse_key,
                        candidate.object_hash,
                        &self.run_id,
                    )
                })
            })
            .collect()
    }

    fn resolve_group<K, F>(
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
                    mapping_index: candidate.index.clone(),
                    content_sources: Vec::new(),
                    transfers: Vec::new(),
                    import_outcome: ContentImportOutcome::AlreadyPresent,
                });
                return Ok(report);
            }
        }

        let mut ignore_progress = |_: ContentTransferEvent| {};
        for candidate in &group.candidates {
            if let Some(acquired) =
                self.acquire_candidate(candidate.object_hash, availability, &mut ignore_progress)?
            {
                promote(candidate)?;
                report.resolved = Some(ResolvedSecondaryContent {
                    object_hash: candidate.object_hash,
                    mapping_index: candidate.index.clone(),
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

    fn locate_candidate_objects<K>(
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
        self.sources
            .iter()
            .map(|source| source.source.locate_objects(&hashes))
            .collect()
    }

    fn acquire_candidate(
        &self,
        hash: ObjectHash,
        availability: &[HashSet<ObjectHash>],
        progress: &mut dyn FnMut(ContentTransferEvent),
    ) -> Result<Option<AcquiredContent>, StoreError> {
        if let Some(working_path) = self.working.object_path(hash)? {
            let Some(manifest) = read_manifest_if_marked(&working_path)? else {
                return Ok(Some(AcquiredContent {
                    outcome: ContentImportOutcome::AlreadyPresent,
                    sources: Vec::new(),
                    transfers: Vec::new(),
                }));
            };
            let Some(closure) = self.ensure_manifest_closure(hash, &manifest, progress)? else {
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
            let manifest = source.source.object_manifest(hash)?;
            if let Some(manifest) = &manifest {
                let Some(closure) = self.ensure_manifest_closure(hash, manifest, progress)? else {
                    return Ok(None);
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
            match source.source.import_object(&self.working, hash)? {
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

    fn ensure_manifest_closure(
        &self,
        object_hash: ObjectHash,
        manifest: &FsTreeManifest,
        progress: &mut dyn FnMut(ContentTransferEvent),
    ) -> Result<Option<AcquiredClosure>, StoreError> {
        let hashes = manifest_fs_files(manifest);
        let mut missing = Vec::new();
        for hash in hashes {
            let path = self.working.fs_file_path_unchecked(hash);
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

        let mut by_source = vec![Vec::new(); self.sources.len()];
        let mut assigned = HashSet::new();
        for (index, source) in self.sources.iter().enumerate() {
            let available = source.source.locate_fs_files(&missing)?;
            for hash in &missing {
                if !assigned.contains(hash) && available.contains(hash) {
                    assigned.insert(*hash);
                    by_source[index].push(*hash);
                }
            }
        }
        if assigned.len() != missing.len() {
            return Ok(None);
        }

        let mut used_sources = Vec::new();
        let mut transfers = Vec::new();
        for (index, hashes) in by_source.iter().enumerate() {
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
            source.source.import_fs_files(&self.working, hashes)?;
            let bytes = hashes.iter().try_fold(0_u64, |total, hash| {
                let path = self.working.fs_file_path_unchecked(*hash);
                let length = fs::symlink_metadata(&path)
                    .map_err(|error| {
                        StoreError::Io(format!(
                            "failed to inspect imported fs-file '{}': {error}",
                            path.display()
                        ))
                    })?
                    .len();
                Ok::<_, StoreError>(total.saturating_add(length))
            })?;
            let report = ContentTransferReport {
                object_hash,
                content_source: source.name.clone(),
                transfer_mode: source.source.transfer_mode(),
                files: hashes.len() as u64,
                bytes,
                duration_ms: duration_ms(started),
            };
            progress(ContentTransferEvent::Finished(report.clone()));
            merge_transfer(&mut transfers, report);
            used_sources.push(source.name.clone());
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
    index: String,
}

#[derive(Debug)]
struct CandidateGroup<K> {
    key: K,
    answers: Vec<TrustedAnswer>,
    candidates: Vec<Candidate<K>>,
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
    for (index, answers) in per_index {
        for answer in answers {
            if !requested.contains(&answer.key) {
                return Err(unrequested_key_error(kind, &answer.key.to_string()));
            }
            answers_by_key
                .entry(answer.key)
                .or_default()
                .push((index.clone(), answer));
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
        .map(|(index, answer)| TrustedAnswer {
            index: index.clone(),
            object_hash: answer.object_hash,
        })
        .collect();
    let mut seen = HashSet::new();
    let candidates = answers
        .into_iter()
        .filter_map(|(index, answer)| {
            seen.insert(answer.object_hash).then_some(Candidate {
                key,
                object_hash: answer.object_hash,
                index,
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
    crate::record::record_existing_object(working, object_hash, run_id)?;
    crate::refs::store_build_ref(working, build_key, object_hash)
}

fn promote_reuse(
    working: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    object_hash: ObjectHash,
    run_id: &str,
) -> Result<(), StoreError> {
    ensure_promotable(working, object_hash)?;
    crate::record::record_existing_object(working, object_hash, run_id)?;
    crate::refs::store_reuse_ref(working, reuse_key, object_hash)?;
    crate::refs::store_build_ref(working, build_key, object_hash)
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
        "trusted index returned unrequested {kind} key '{key}'"
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
    use crate::fs_tree::FsTreeEntry;
    use crate::{
        LocalCopyContentSource, LocalHardlinkContentSource, LocalRepository, LocalTrustedKeyIndex,
        ReadOnlyStore, import_build, load_build_object_hash,
    };
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use serde_json::Value;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::str::FromStr;
    use tempfile::tempdir;

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

    fn index(name: &str, root: &Path) -> NamedTrustedKeyIndex {
        NamedTrustedKeyIndex::new(
            name,
            Arc::new(LocalTrustedKeyIndex::new(local_repository(root))),
        )
    }

    fn source(name: &str, root: &Path) -> NamedContentSource {
        NamedContentSource::new(
            name,
            Arc::new(LocalHardlinkContentSource::with_runtime(
                local_repository(root),
                RuntimeProvider::host(),
            )),
        )
    }

    fn copy_source(name: &str, root: &Path) -> NamedContentSource {
        NamedContentSource::new(
            name,
            Arc::new(LocalCopyContentSource::with_runtime(
                local_repository(root),
                RuntimeProvider::host(),
            )),
        )
    }

    fn resolver(
        working: Store,
        indexes: Vec<NamedTrustedKeyIndex>,
        sources: Vec<NamedContentSource>,
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

    #[test]
    fn trusted_mapping_and_content_can_come_from_different_stores() {
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
        let reports = resolver.resolve_builds(&[build]).unwrap();

        assert_eq!(reports.len(), 1);
        let resolved = reports[0].resolved.as_ref().unwrap();
        assert_eq!(resolved.object_hash, object_hash);
        assert_eq!(resolved.mapping_index, "trusted-index");
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
            serde_json::from_slice(&fs::read(working.object_record_path(object_hash)).unwrap())
                .unwrap();
        assert_eq!(
            local_record["build_key"],
            BuildKey::from_object_hash(object_hash).to_string()
        );
        assert_eq!(local_record["run_id"], "test-run");
        assert_eq!(local_record["inputs"], serde_json::json!([]));
    }

    #[test]
    fn known_secondary_content_gets_a_new_neutral_working_record() {
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

        let resolution = resolver.ensure_objects(&[object_hash]).unwrap().remove(0);

        assert_eq!(resolution.outcome, Some(ContentImportOutcome::Imported));
        let local_record: Value =
            serde_json::from_slice(&fs::read(working.object_record_path(object_hash)).unwrap())
                .unwrap();
        assert_eq!(
            local_record["build_key"],
            BuildKey::from_object_hash(object_hash).to_string()
        );
        assert_eq!(local_record["run_id"], "test-run");
        assert_eq!(local_record["inputs"], serde_json::json!([]));
    }

    #[test]
    fn complete_working_candidate_beats_an_earlier_nonlocal_conflict() {
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
            vec![NamedContentSource::new(
                "must-not-be-queried",
                Arc::new(UnexpectedContentSource),
            )],
        );
        let report = resolver.resolve_builds(&[build]).unwrap().remove(0);

        assert!(report.has_conflict());
        let resolved = report.resolved.unwrap();
        assert_eq!(resolved.object_hash, y);
        assert_eq!(resolved.mapping_index, "second-index");
        assert!(resolved.content_sources.is_empty());
        assert_eq!(
            resolved.import_outcome,
            ContentImportOutcome::AlreadyPresent
        );
        assert!(working.object_path(x).unwrap().is_none());
        assert_eq!(load_build_object_hash(&working, build).unwrap(), Some(y));
    }

    #[test]
    fn mapping_priority_selects_first_when_both_candidates_are_local() {
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
        let report = resolver.resolve_builds(&[build]).unwrap().remove(0);
        assert_eq!(report.resolved.unwrap().object_hash, x);
    }

    #[test]
    fn stale_first_mapping_falls_back_to_next_available_candidate() {
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
        let report = resolver.resolve_builds(&[build]).unwrap().remove(0);

        assert_eq!(report.unavailable, [x]);
        assert_eq!(report.resolved.unwrap().object_hash, y);
        assert_eq!(load_build_object_hash(&working, build).unwrap(), Some(y));
    }

    #[test]
    fn unavailable_content_never_publishes_trusted_mapping() {
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
        let report = resolver.resolve_builds(&[build]).unwrap().remove(0);

        assert!(report.resolved.is_none());
        assert_eq!(report.unavailable, [object_hash]);
        assert_eq!(load_build_object_hash(&working, build).unwrap(), None);
        assert!(!working.object_record_path(object_hash).exists());
    }

    #[test]
    fn reuse_hit_publishes_reuse_and_current_build_mappings() {
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
            .unwrap()
            .remove(0);

        assert_eq!(report.resolved.unwrap().object_hash, object_hash);
        assert_eq!(
            load_build_object_hash(&working, current_build).unwrap(),
            Some(object_hash)
        );
        assert!(working.reuse_ref_path(reuse).is_symlink());
    }

    #[test]
    fn fs_tree_manifest_and_fs_file_can_come_from_different_content_sources() {
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
        let source_fs_file = manifest_store.fs_file_path_unchecked(fs_file_hash);
        let destination_fs_file = file_store.fs_file_path_unchecked(fs_file_hash);
        fs::create_dir(destination_fs_file.parent().unwrap()).unwrap();
        fs::hard_link(&source_fs_file, &destination_fs_file).unwrap();
        fs::remove_file(source_fs_file).unwrap();

        let resolver = resolver(
            working.clone(),
            vec![index("manifest-index", &manifest_root)],
            vec![
                source("manifest-content", &manifest_root),
                copy_source("file-content", &file_root),
            ],
        );
        let report = resolver.resolve_builds(&[build]).unwrap().remove(0);

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
        let source_metadata =
            fs::metadata(file_store.fs_file_path_unchecked(fs_file_hash)).unwrap();
        let working_metadata = fs::metadata(working.fs_file_path_unchecked(fs_file_hash)).unwrap();
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
                .contains("duplicate secondary trusted index")
        );

        let empty =
            SecondaryResolver::new(working, "test-run", Vec::new(), vec![source("", &root)])
                .unwrap_err();
        assert!(empty.to_string().contains("name must not be empty"));
    }
}
