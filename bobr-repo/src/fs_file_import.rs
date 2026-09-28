//! Batched publication of downloaded filesystem-file representations.

use crate::{RepositoryError, RepositoryErrorKind, decode_fs_file_staged};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_provider::RuntimeProvider;
use bobr_store::fs_tree::FsFileHash;
use bobr_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// One downloaded, still-untrusted encoded filesystem-file representation.
///
/// The temporary file remains alive until this value is dropped. Constructing
/// this type is private to the authenticated repository reader; callers can
/// inspect its identity and pass it to [`publish_fs_file_batch`].
pub struct FetchedFsFileRepresentation {
    hash: FsFileHash,
    encoded: tempfile::NamedTempFile,
    encoded_bytes: u64,
}

impl std::fmt::Debug for FetchedFsFileRepresentation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FetchedFsFileRepresentation")
            .field("hash", &self.hash)
            .field("encoded_bytes", &self.encoded_bytes)
            .finish_non_exhaustive()
    }
}

impl FetchedFsFileRepresentation {
    pub(crate) fn new(
        hash: FsFileHash,
        encoded: tempfile::NamedTempFile,
    ) -> Result<Self, RepositoryError> {
        let encoded_bytes = encoded.as_file().metadata()?.len();
        Ok(Self {
            hash,
            encoded,
            encoded_bytes,
        })
    }

    /// Returns the authenticated-list identity requested by the reader.
    ///
    /// The downloaded bytes have not yet been decoded or verified against this
    /// hash.
    pub fn hash(&self) -> FsFileHash {
        self.hash
    }

    /// Returns the encoded representation length downloaded from the origin.
    pub fn encoded_bytes(&self) -> u64 {
        self.encoded_bytes
    }
}

/// Summary of one local filesystem-file publication batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsFilePublicationReport {
    /// Distinct files newly published into the working store.
    pub published: usize,
    /// Distinct files already present and successfully verified.
    pub already_present: usize,
}

/// Decodes, verifies, normalizes, and publishes a downloaded batch.
///
/// Exactly one runtime call is made for a nonempty batch. The caller chooses
/// the batch boundary and therefore controls cancellation latency and local-I/O
/// concurrency without spawning one namespace operation per fs-file.
pub fn publish_fs_file_batch(
    runtime: &RuntimeProvider,
    working: &Store,
    representations: &[FetchedFsFileRepresentation],
) -> Result<FsFilePublicationReport, RepositoryError> {
    if representations.is_empty() {
        return Ok(FsFilePublicationReport::default());
    }
    let output = runtime
        .run(
            &PublishFsFilesFunction,
            PublishFsFilesInput {
                working_root: working.root().to_path_buf(),
                files: representations
                    .iter()
                    .map(|representation| PublishFsFileInput {
                        hash: representation.hash.to_hex(),
                        encoded: representation.encoded.path().to_path_buf(),
                    })
                    .collect(),
            },
        )
        .map_err(|error| {
            RepositoryError::runtime(format!(
                "filesystem-file publication runtime failed: {error}"
            ))
        })?;
    match output {
        PublishFsFilesOutput::Success(report) => Ok(report),
        PublishFsFilesOutput::Failure { kind, message } => {
            Err(RepositoryError::with_kind(kind, message))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishFsFilesInput {
    working_root: PathBuf,
    files: Vec<PublishFsFileInput>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishFsFileInput {
    hash: String,
    encoded: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishFsFilesOutput {
    Success(FsFilePublicationReport),
    Failure {
        kind: RepositoryErrorKind,
        message: String,
    },
}

/// Namespace operation publishing a complete batch of downloaded fs-files.
#[derive(Debug)]
pub(crate) struct PublishFsFilesFunction;

impl RuntimeFunction for PublishFsFilesFunction {
    type Input = PublishFsFilesInput;
    type Output = PublishFsFilesOutput;

    fn name(&self) -> &'static str {
        "bobr_repo_publish_fs_files"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        Ok(match publish_fs_files(input) {
            Ok(report) => PublishFsFilesOutput::Success(report),
            Err(error) => PublishFsFilesOutput::Failure {
                kind: error.kind(),
                message: error.to_string(),
            },
        })
    }
}

fn publish_fs_files(
    input: PublishFsFilesInput,
) -> Result<FsFilePublicationReport, RepositoryError> {
    let working = Store::create(&input.working_root).map_err(map_store_error)?;
    let mut report = FsFilePublicationReport::default();
    let mut seen = HashSet::new();
    for file in input.files {
        let hash = file.hash.parse::<FsFileHash>().map_err(|error| {
            RepositoryError::configuration(format!(
                "invalid filesystem-file hash '{}': {error}",
                file.hash
            ))
        })?;
        if !seen.insert(hash) {
            continue;
        }
        let destination = working.fs_file_path(hash);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                working.verify_fs_file(hash).map_err(map_store_error)?;
                report.already_present += 1;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(RepositoryError::from(error)),
        }
        let parent = destination.parent().ok_or_else(|| {
            RepositoryError::local_io(format!(
                "working fs-file path has no parent: '{}'",
                destination.display()
            ))
        })?;
        fs::create_dir_all(parent)?;
        let (staging, _) = decode_fs_file_staged(&file.encoded, hash, parent)?;
        match staging.persist_noclobber(&destination) {
            Ok(_) => {
                working.verify_fs_file(hash).map_err(map_store_error)?;
                report.published += 1;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                working.verify_fs_file(hash).map_err(map_store_error)?;
                report.already_present += 1;
            }
            Err(error) => return Err(RepositoryError::from(error.error)),
        }
    }
    Ok(report)
}

fn map_store_error(error: StoreError) -> RepositoryError {
    match error {
        StoreError::InvalidInput(message) => RepositoryError::configuration(message),
        StoreError::InvalidData(message) | StoreError::Hashing(message) => {
            RepositoryError::invalid_repository(message)
        }
        StoreError::Unsupported(message) | StoreError::Io(message) => {
            RepositoryError::local_io(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Compression, encode_fs_file};
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn one_runtime_call_publishes_a_batch_and_reuses_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let mut representations = Vec::new();
        for (index, bytes) in [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .enumerate()
        {
            let source = temp.path().join(format!("source-{index}"));
            fs::write(&source, bytes).unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
            let encoded = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
            let hash = encode_fs_file(&source, Compression::Identity, encoded.path()).unwrap();
            representations.push(FetchedFsFileRepresentation::new(hash, encoded).unwrap());
        }

        let runtime = RuntimeProvider::host();
        let first = publish_fs_file_batch(&runtime, &store, &representations).unwrap();
        assert_eq!(first.published, 2);
        assert_eq!(first.already_present, 0);
        for representation in &representations {
            store.verify_fs_file(representation.hash()).unwrap();
        }

        let second = publish_fs_file_batch(&runtime, &store, &representations).unwrap();
        assert_eq!(second.published, 0);
        assert_eq!(second.already_present, 2);
    }

    #[test]
    fn invalid_representation_is_not_published() {
        let temp = tempfile::tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let encoded = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
        fs::write(encoded.path(), b"not CBOR").unwrap();
        let hash = "11".repeat(32).parse().unwrap();
        let representations = [FetchedFsFileRepresentation::new(hash, encoded).unwrap()];

        let error =
            publish_fs_file_batch(&RuntimeProvider::host(), &store, &representations).unwrap_err();
        assert_eq!(error.kind(), RepositoryErrorKind::InvalidRepository);
        assert!(!store.fs_file_path(hash).exists());
    }
}
