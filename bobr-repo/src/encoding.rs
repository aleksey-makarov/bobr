//! Preferred repository representation encoding.

use crate::{Compression, RepositoryError, encode_fs_file, encode_object};
use bobr_core::ObjectHash;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_provider::RuntimeProvider;
use bobr_store::StoredFsFile;
use bobr_store::fs_tree::FsFileHash;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Encodes an ordinary object as identity and Zstandard and returns the smaller
/// complete envelope. Equal-size representations prefer identity.
pub fn encode_preferred_object(
    source: &Path,
    expected: ObjectHash,
    directory: &Path,
) -> Result<NamedTempFile, RepositoryError> {
    let identity = tempfile::Builder::new()
        .prefix("object-identity-")
        .tempfile_in(directory)?;
    let zstd = tempfile::Builder::new()
        .prefix("object-zstd-")
        .tempfile_in(directory)?;
    require_object_hash(
        encode_object(source, Compression::Identity, identity.path())?,
        expected,
    )?;
    require_object_hash(
        encode_object(source, Compression::Zstd, zstd.path())?,
        expected,
    )?;
    if zstd.as_file().metadata()?.len() < identity.as_file().metadata()?.len() {
        Ok(zstd)
    } else {
        Ok(identity)
    }
}

/// Encodes filesystem files through a runtime which observes their logical
/// uid/gid and writes one preferred envelope named by each hash.
pub fn encode_preferred_fs_files(
    runtime: &RuntimeProvider,
    files: &[StoredFsFile],
    output_directory: &Path,
) -> Result<(), RepositoryError> {
    fs::create_dir_all(output_directory)?;
    let plan = NamedTempFile::new()?;
    serde_json::to_writer(plan.as_file(), files).map_err(|error| {
        RepositoryError::new(format!(
            "failed to encode fs-file publication plan: {error}"
        ))
    })?;
    runtime
        .run(
            &EncodeFsFilesFunction,
            EncodeFsFilesInput {
                plan: plan.path().to_path_buf(),
                output_directory: output_directory.to_path_buf(),
            },
        )
        .map_err(|error| RepositoryError::new(format!("fs-file encoder runtime failed: {error}")))
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EncodeFsFilesInput {
    plan: PathBuf,
    output_directory: PathBuf,
}

pub(crate) struct EncodeFsFilesFunction;

impl RuntimeFunction for EncodeFsFilesFunction {
    type Input = EncodeFsFilesInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "bobr_repo_encode_fs_files"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        let file = fs::File::open(&input.plan)?;
        let files: Vec<StoredFsFile> =
            serde_json::from_reader(file).map_err(|error| RuntimeError::new(error.to_string()))?;
        for file in files {
            let encoded = encode_preferred_fs_file(&file.path, file.hash, &input.output_directory)
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            encoded
                .persist_noclobber(input.output_directory.join(file.hash.to_string()))
                .map_err(|error| RuntimeError::new(error.error.to_string()))?;
        }
        Ok(())
    }
}

fn encode_preferred_fs_file(
    source: &Path,
    expected: FsFileHash,
    directory: &Path,
) -> Result<NamedTempFile, RepositoryError> {
    let identity = tempfile::Builder::new()
        .prefix("file-identity-")
        .tempfile_in(directory)?;
    let zstd = tempfile::Builder::new()
        .prefix("file-zstd-")
        .tempfile_in(directory)?;
    require_fs_file_hash(
        encode_fs_file(source, Compression::Identity, identity.path())?,
        expected,
    )?;
    require_fs_file_hash(
        encode_fs_file(source, Compression::Zstd, zstd.path())?,
        expected,
    )?;
    if zstd.as_file().metadata()?.len() < identity.as_file().metadata()?.len() {
        Ok(zstd)
    } else {
        Ok(identity)
    }
}

fn require_object_hash(actual: ObjectHash, expected: ObjectHash) -> Result<(), RepositoryError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "local object changed while publishing: expected '{expected}', got '{actual}'"
        )))
    }
}

fn require_fs_file_hash(actual: FsFileHash, expected: FsFileHash) -> Result<(), RepositoryError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "local fs-file changed while publishing: expected '{expected}', got '{actual}'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::fs_tree::hash_fs_file_parts;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn identity_wins_for_incompressible_small_object() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, [1, 2, 3, 4]).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let expected = fsobj_hash::hash_path(&source).unwrap();
        let encoded = encode_preferred_object(&source, expected, directory.path()).unwrap();
        assert!(encoded.as_file().metadata().unwrap().len() > 4);
    }

    #[test]
    fn host_runtime_encodes_fs_file_plan() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output");
        let source = directory.path().join("source");
        fs::write(&source, b"hello hello hello hello").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let metadata = fs::metadata(&source).unwrap();
        use std::os::unix::fs::MetadataExt;
        let content: [u8; 32] = Sha256::digest(b"hello hello hello hello").into();
        let hash = hash_fs_file_parts(
            metadata.uid(),
            metadata.gid(),
            0o644,
            metadata.len(),
            content,
        )
        .unwrap();
        encode_preferred_fs_files(
            &RuntimeProvider::host(),
            &[StoredFsFile { hash, path: source }],
            &output,
        )
        .unwrap();
        assert!(output.join(hash.to_string()).is_file());
    }
}
