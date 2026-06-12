#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use flate2::Compression;
use flate2::write::GzEncoder;
use mbuild_core::{
    BuildContext, BuilderInputObject, BuilderInputs, FsTreeEntry, TypedBuilder,
    load_fs_tree_object,
    oci::{self, OciDescriptor},
};
use mbuild_image::{OciExtractBuilder, OciExtractConfig};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use tar::EntryType;
use tempfile::tempdir;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn oci_extract_materializes_runtime_ownership() -> TestResult<()> {
    let temp = tempdir()?;
    let oci = create_oci_layout(temp.path())?;
    let mut cx = build_context(temp.path())?;
    let mut inputs = BuilderInputs::empty();
    inputs.insert("image", BuilderInputObject { path: oci });

    let result = OciExtractBuilder.build_typed(OciExtractConfig {}, inputs, &mut cx)?;
    assert!(result.staged_path.join("manifest.jsonl").is_file());
    assert!(result.staged_path.join("oci-config.json").is_file());
    assert_eq!(
        fs::read(result.staged_path.join("root/bin/tool"))?,
        b"tool\n"
    );
    let loaded = load_fs_tree_object(&result.staged_path)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let oci_config: serde_json::Value =
        serde_json::from_slice(&fs::read(result.staged_path.join("oci-config.json"))?)?;

    assert_eq!(loaded.paths.root_dir, result.staged_path.join("root"));
    assert_eq!(oci_config["architecture"], "amd64");

    let manifest = loaded.manifest;
    assert!(manifest.entries().iter().any(|entry| {
        matches!(entry, FsTreeEntry::File { path, uid: 1, gid: 1, mode: 0o755, .. } if path == "bin/tool")
    }));
    Ok(())
}

fn build_context(root: &Path) -> TestResult<BuildContext> {
    let temp_dir = root.join("tmp");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)?;
    Ok(BuildContext::with_noop_logger(temp_dir))
}

fn create_oci_layout(root: &Path) -> TestResult<PathBuf> {
    let oci_dir = root.join("oci");
    fs::create_dir_all(oci_dir.join("blobs").join("sha256"))?;
    fs::write(
        oci_dir.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    let config_bytes = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{}}"#;
    let config_desc = write_blob(&oci_dir, config_bytes, oci::MEDIA_TYPE_OCI_CONFIG)?;
    let layer = gzip(&make_tar()?)?;
    let layer_desc = write_blob(&oci_dir, &layer, oci::MEDIA_TYPE_OCI_LAYER)?;
    let manifest = oci::OciManifest {
        schema_version: 2,
        config: config_desc,
        layers: vec![layer_desc],
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_desc = write_blob(&oci_dir, &manifest_bytes, oci::MEDIA_TYPE_OCI_MANIFEST)?;
    oci::write_index(&oci_dir, manifest_desc, None)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(oci_dir)
}

fn write_blob(oci_dir: &Path, bytes: &[u8], media_type: &str) -> TestResult<OciDescriptor> {
    let hex = format!("{:x}", Sha256::digest(bytes));
    fs::write(oci_dir.join("blobs").join("sha256").join(&hex), bytes)?;
    Ok(OciDescriptor {
        media_type: media_type.to_string(),
        digest: format!("sha256:{hex}"),
        size: bytes.len() as u64,
        platform: None,
        annotations: None,
    })
}

fn make_tar() -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        append_dir(&mut builder, "bin", 1, 1, 0o755)?;
        append_file(&mut builder, "bin/tool", b"tool\n", 1, 1, 0o755)?;
        builder.finish()?;
    }
    Ok(bytes)
}

fn gzip(bytes: &[u8]) -> TestResult<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn append_dir(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    path: &str,
    uid: u64,
    gid: u64,
    mode: u32,
) -> TestResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, path, io::empty())?;
    Ok(())
}

fn append_file(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    path: &str,
    bytes: &[u8],
    uid: u64,
    gid: u64,
    mode: u32,
) -> TestResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, path, Cursor::new(bytes))?;
    Ok(())
}
