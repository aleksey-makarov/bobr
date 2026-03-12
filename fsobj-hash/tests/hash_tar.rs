use fsobj_hash::{Error, InvalidPathReason, TarEntryKind, hash_tar_reader};
use std::io::Cursor;
use tar::{Builder, EntryType, Header};

#[test]
fn empty_tar_hashes_as_empty_directory() {
    let bytes = tar_bytes(|_| {});
    hash_tar_reader(Cursor::new(bytes)).unwrap();
}

#[test]
fn implicit_and_explicit_dirs_match() {
    let implicit = tar_bytes(|builder| {
        append_file(builder, "a/b/c.txt", 0o644, b"hello");
    });
    let explicit = tar_bytes(|builder| {
        append_dir(builder, "a", 0o755);
        append_dir(builder, "a/b", 0o755);
        append_file(builder, "a/b/c.txt", 0o644, b"hello");
    });
    assert_eq!(
        hash_tar_reader(Cursor::new(implicit)).unwrap(),
        hash_tar_reader(Cursor::new(explicit)).unwrap()
    );
}

#[test]
fn duplicate_file_entry_is_rejected() {
    let bytes = tar_bytes(|builder| {
        append_file(builder, "a.txt", 0o644, b"one");
        append_file(builder, "a.txt", 0o644, b"two");
    });
    let error = hash_tar_reader(Cursor::new(bytes)).unwrap_err();
    assert!(matches!(error, Error::DuplicateEntry { .. }));
}

#[test]
fn duplicate_directory_entry_is_allowed() {
    let bytes = tar_bytes(|builder| {
        append_dir(builder, "a", 0o755);
        append_dir(builder, "a", 0o755);
    });
    hash_tar_reader(Cursor::new(bytes)).unwrap();
}

#[test]
fn absolute_path_is_rejected() {
    let bytes = raw_tar_with_single_file(b"/a.txt", b"x");
    let error = hash_tar_reader(Cursor::new(bytes)).unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidArchivePath {
            reason: InvalidPathReason::AbsolutePath,
            ..
        }
    ));
}

#[test]
fn parent_traversal_is_rejected() {
    let bytes = raw_tar_with_single_file(b"a/../b.txt", b"x");
    let error = hash_tar_reader(Cursor::new(bytes)).unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidArchivePath {
            reason: InvalidPathReason::ParentTraversal,
            ..
        }
    ));
}

#[test]
fn hardlink_is_rejected() {
    let bytes = tar_bytes(|builder| {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_link(&mut header, "link", "target").unwrap();
    });
    let error = hash_tar_reader(Cursor::new(bytes)).unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedTarEntry {
            kind: TarEntryKind::HardLink,
            ..
        }
    ));
}

#[test]
fn symlink_duplicate_is_rejected() {
    let bytes = tar_bytes(|builder| {
        append_symlink(builder, "lib", "lib64");
        append_symlink(builder, "lib", "usr/lib64");
    });
    let error = hash_tar_reader(Cursor::new(bytes)).unwrap_err();
    assert!(matches!(error, Error::DuplicateEntry { .. }));
}

fn tar_bytes<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut Builder<Vec<u8>>),
{
    let mut builder = Builder::new(Vec::new());
    f(&mut builder);
    builder.finish().unwrap();
    builder.into_inner().unwrap()
}

fn append_file(builder: &mut Builder<Vec<u8>>, path: &str, mode: u32, bytes: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn append_dir(builder: &mut Builder<Vec<u8>>, path: &str, mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(mode);
    header.set_entry_type(EntryType::Directory);
    header.set_cksum();
    builder.append_data(&mut header, path, std::io::empty()).unwrap();
}

fn append_symlink(builder: &mut Builder<Vec<u8>>, path: &str, target: &str) {
    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o777);
    header.set_entry_type(EntryType::Symlink);
    header.set_link_name(target).unwrap();
    header.set_cksum();
    builder.append_data(&mut header, path, std::io::empty()).unwrap();
}

fn raw_tar_with_single_file(path: &[u8], contents: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 512];

    out[..path.len()].copy_from_slice(path);
    write_octal(&mut out[100..108], 0o644);
    write_octal(&mut out[108..116], 0);
    write_octal(&mut out[116..124], 0);
    write_octal(&mut out[124..136], contents.len() as u64);
    write_octal(&mut out[136..148], 0);
    out[148..156].fill(b' ');
    out[156] = b'0';
    out[257..263].copy_from_slice(b"ustar\0");
    out[263..265].copy_from_slice(b"00");

    let checksum: u32 = out.iter().map(|byte| *byte as u32).sum();
    write_checksum(&mut out[148..156], checksum);

    out.extend_from_slice(contents);
    let padding = (512 - (contents.len() % 512)) % 512;
    out.resize(out.len() + padding, 0);
    out.resize(out.len() + 1024, 0);
    out
}

fn write_octal(field: &mut [u8], value: u64) {
    let width = field.len();
    let encoded = format!("{value:0width$o}\0", width = width - 1);
    field.copy_from_slice(encoded.as_bytes());
}

fn write_checksum(field: &mut [u8], value: u32) {
    let encoded = format!("{value:06o}\0 ",);
    field.copy_from_slice(encoded.as_bytes());
}
