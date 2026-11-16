//! Execution of validated composition plans into independent plain objects.

use bobr_runtime::runtime::{NoRuntimeEvent, RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{
    FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, lchown, symlink,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PlainTreeComposeFunction;

/// A builder-resolved plan. Sources are immutable inputs; destinations are
/// checked again here, on the privileged side of the runtime boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlainTreeComposeInput {
    pub(crate) output_root: PathBuf,
    pub(crate) ops: Vec<PlainTreeOp>,
    pub(crate) cancellation_fifo: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "kebab-case")]
pub(crate) enum PlainTreeOp {
    CopyTree { source: PathBuf, dest: String },
    CopyFile { source: PathBuf, dest: String },
    Symlink { target: String, dest: String },
}

impl PlainTreeOp {
    fn dest(&self) -> &str {
        match self {
            Self::CopyTree { dest, .. }
            | Self::CopyFile { dest, .. }
            | Self::Symlink { dest, .. } => dest,
        }
    }
}

/// Cancellation crosses the namespace wire as an outcome, not a runtime error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PlainTreeComposeOutput {
    Completed,
    Cancelled,
}

impl PlainTreeComposeOutput {
    pub(crate) fn into_builder_result(self) -> Result<(), crate::BuilderError> {
        match self {
            Self::Completed => Ok(()),
            Self::Cancelled => Err(crate::BuilderError::Cancelled(
                "plain-tree composition cancelled".into(),
            )),
        }
    }
}

impl RuntimeFunction for PlainTreeComposeFunction {
    type Input = PlainTreeComposeInput;
    type Output = PlainTreeComposeOutput;
    type Event = NoRuntimeEvent;

    fn name(&self) -> &'static str {
        "plain-tree-compose"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        match compose(input) {
            Ok(()) => Ok(PlainTreeComposeOutput::Completed),
            Err(ComposeError::Cancelled) => Ok(PlainTreeComposeOutput::Cancelled),
            Err(ComposeError::Failed(message)) => Err(RuntimeError::new(message)),
        }
    }
}

#[derive(Debug)]
enum ComposeError {
    Cancelled,
    Failed(String),
}

impl From<String> for ComposeError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<io::Error> for ComposeError {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

type ComposeResult<T> = Result<T, ComposeError>;

/// Owns only a newly created output. Never removes a pre-existing path, and
/// never follows output symlinks during cleanup (including on unwinding).
struct StagedOutput(Option<PathBuf>);

impl Drop for StagedOutput {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

struct Composition {
    root: PathBuf,
    cancellation: File,
    uid: u32,
    gid: u32,
}

fn compose(input: PlainTreeComposeInput) -> ComposeResult<()> {
    let cancellation = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&input.cancellation_fifo)?;
    let metadata = cancellation.metadata()?;
    if !metadata.file_type().is_fifo()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(
            "plain-tree cancellation channel must be a private FIFO owned by the runtime user"
                .to_string()
                .into(),
        );
    }
    let cx = Composition {
        root: input.output_root,
        cancellation,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    cx.check_cancelled()?;
    let mut destinations = BTreeSet::new();
    for op in &input.ops {
        cx.check_cancelled()?;
        validate_destination(op.dest())?;
        if !destinations.insert(op.dest()) {
            return Err(format!("duplicate plain-tree destination '{}'", op.dest()).into());
        }
    }
    for dest in &destinations {
        cx.check_cancelled()?;
        for (index, _) in dest.match_indices('/') {
            if destinations.contains(&dest[..index]) {
                return Err(format!(
                    "overlapping plain-tree destinations '{}' and '{dest}'",
                    &dest[..index]
                )
                .into());
            }
        }
    }

    // The caller supplies a private build-workspace path. No output is
    // published here; the guard is disarmed only after the entire plan succeeds.
    fs::create_dir(&cx.root).map_err(|error| {
        format!(
            "failed to create plain-tree output '{}': {error}",
            cx.root.display()
        )
    })?;
    let mut staged = StagedOutput(Some(cx.root.clone()));
    cx.normalize(&cx.root, Some(0o755))?;
    for op in input.ops {
        cx.check_cancelled()?;
        let destination = cx.root.join(op.dest());
        cx.create_parents(&destination)?;
        match op {
            PlainTreeOp::CopyTree { source, .. } => cx.copy_tree(&source, &destination)?,
            PlainTreeOp::CopyFile { source, .. } => cx.copy_file(&source, &destination)?,
            PlainTreeOp::Symlink { target, .. } => {
                symlink(target, &destination)?;
                cx.normalize(&destination, None)?;
            }
        }
    }
    cx.check_cancelled()?;
    staged.0.take();
    Ok(())
}

fn validate_destination(relative: &str) -> ComposeResult<()> {
    if relative.is_empty()
        || Path::new(relative).is_absolute()
        || relative.contains('\0')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("unsafe plain-tree destination '{relative}': expected a non-empty relative path without empty, '.', '..', or NUL components").into());
    }
    Ok(())
}

impl Composition {
    fn check_cancelled(&self) -> ComposeResult<()> {
        let mut cancellation = &self.cancellation;
        loop {
            match cancellation.read(&mut [0_u8; 1]) {
                Ok(0) => return Err(ComposeError::Cancelled),
                Ok(_) => {
                    return Err("plain-tree cancellation FIFO unexpectedly contained data"
                        .to_string()
                        .into());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn normalize(&self, path: &Path, mode: Option<u32>) -> ComposeResult<()> {
        // chown can clear setuid/setgid, so restore regular-file modes AFTER it.
        // lchown changes the symlink itself, never its target.
        lchown(path, Some(self.uid), Some(self.gid))?;
        if let Some(mode) = mode {
            set_mode(path, mode)?;
        }
        Ok(())
    }

    fn create_directory(&self, path: &Path) -> ComposeResult<()> {
        fs::create_dir(path)?;
        self.normalize(path, Some(0o755))
    }

    fn create_parents(&self, destination: &Path) -> ComposeResult<()> {
        let relative = destination
            .strip_prefix(&self.root)
            .expect("validated destination");
        let mut parent = self.root.clone();
        for component in relative
            .parent()
            .expect("destination has parent")
            .components()
        {
            self.check_cancelled()?;
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "plain-tree destination parent '{}' is not a real directory",
                        parent.display()
                    )
                    .into());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.create_directory(&parent)?
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn copy_tree(&self, source: &Path, destination: &Path) -> ComposeResult<()> {
        self.check_cancelled()?;
        // Path components remove trailing slashes and `/.`, which otherwise
        // make lstat follow a symlink root despite using symlink_metadata.
        let source_path: PathBuf = source.components().collect();
        let source = source_path.as_path();
        if !fs::symlink_metadata(source)?.is_dir() {
            return Err(format!(
                "plain-tree source '{}' is not a real directory",
                source.display()
            )
            .into());
        }
        self.create_directory(destination)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(source)? {
            self.check_cancelled()?;
            entries.push(entry?);
        }
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            self.check_cancelled()?;
            let source = entry.path();
            let destination = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.is_dir() {
                self.copy_tree(&source, &destination)?;
            } else if metadata.is_file() {
                self.copy_file(&source, &destination)?;
            } else if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&source)?, &destination)?;
                self.normalize(&destination, None)?;
            } else {
                return Err(format!(
                    "unsupported special file in plain-tree input: '{}'",
                    source.display()
                )
                .into());
            }
        }
        Ok(())
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> ComposeResult<()> {
        self.check_cancelled()?;
        // NOFOLLOW rejects a symlink source; NONBLOCK avoids hanging on FIFOs
        // before fstat rejects them. Inputs are immutable for the whole call.
        let mut source_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        let metadata = source_file.metadata()?;
        if !metadata.is_file() {
            return Err(format!(
                "plain-tree file source '{}' is not a regular file",
                source.display()
            )
            .into());
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            self.check_cancelled()?;
            let size = source_file.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            output.write_all(&buffer[..size])?;
        }
        self.normalize(destination, Some(metadata.mode()))
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|error| format!("failed to chmod '{}': {error}", path.display()))
}

/// Removes write, setuid, and setgid bits from a completed plain directory tree.
pub(crate) fn make_tree_read_only(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
        entries.sort();
        for entry in entries {
            make_tree_read_only(&entry)?;
        }
    }
    if !metadata.file_type().is_symlink() {
        // Input modes, including setuid/setgid, are preserved only in the
        // private staged tree so composition can inspect an accurate copy.
        // No staged path is published before this recursive finalization.
        let mode = metadata.permissions().mode() & 0o7777 & !0o6222;
        set_mode(path, mode)?;
    }
    Ok(())
}

/// Verifies the final permission invariant without following symlinks.
pub(crate) fn verify_tree_read_only(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect '{}': {error}", path.display()))?;
    if !metadata.file_type().is_symlink() && metadata.permissions().mode() & 0o6222 != 0 {
        return Err(format!(
            "published HostBundle path '{}' retains write, setuid, or setgid bits",
            path.display()
        ));
    }
    if metadata.file_type().is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
        entries.sort();
        for entry in entries {
            verify_tree_read_only(&entry)?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::CancellationToken;
    use bobr_runtime::runtime::Runtime;
    use bobr_runtime::runtime_host::HostRuntime;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn run(output: &Path, ops: Vec<PlainTreeOp>) -> Result<PlainTreeComposeOutput, RuntimeError> {
        let cancellation = CancellationToken::new().unwrap();
        HostRuntime::new().run(
            &PlainTreeComposeFunction,
            PlainTreeComposeInput {
                output_root: output.to_path_buf(),
                ops,
                cancellation_fifo: cancellation.fifo_path().to_path_buf(),
            },
        )
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o7777
    }

    fn assert_owner(path: &Path, uid: u32, gid: u32) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (uid, gid),
            "{}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                assert_owner(&entry.unwrap().path(), uid, gid);
            }
        }
    }

    #[test]
    fn composes_multiple_trees_files_and_symlinks_with_canonical_modes() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let other = temp.path().join("other");
        fs::create_dir_all(source.join("private")).unwrap();
        fs::create_dir_all(other.join("empty")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(source.join("private"), fs::Permissions::from_mode(0o2700)).unwrap();
        fs::set_permissions(other.join("empty"), fs::Permissions::from_mode(0o711)).unwrap();
        fs::write(source.join("private/tool"), b"executable").unwrap();
        fs::set_permissions(
            source.join("private/tool"),
            fs::Permissions::from_mode(0o6751),
        )
        .unwrap();
        fs::write(other.join("data"), b"data").unwrap();
        fs::set_permissions(other.join("data"), fs::Permissions::from_mode(0o1640)).unwrap();
        symlink("/outside/absolute", source.join("absolute")).unwrap();
        symlink("../dangling", other.join("relative")).unwrap();
        let output = temp.path().join("output");
        let result = run(
            &output,
            vec![
                PlainTreeOp::CopyTree {
                    source: source.clone(),
                    dest: "payload".into(),
                },
                PlainTreeOp::CopyTree {
                    source: other.clone(),
                    dest: "overrides".into(),
                },
                PlainTreeOp::CopyFile {
                    source: source.join("private/tool"),
                    dest: "libexec/tool".into(),
                },
                PlainTreeOp::Symlink {
                    target: "../payload/private/tool".into(),
                    dest: "bin/tool".into(),
                },
            ],
        )
        .unwrap();
        assert!(matches!(result, PlainTreeComposeOutput::Completed));
        for directory in [
            "",
            "payload",
            "payload/private",
            "overrides",
            "overrides/empty",
            "libexec",
            "bin",
        ] {
            assert_eq!(mode(&output.join(directory)), 0o755, "{directory}");
        }
        for file in ["payload/private/tool", "libexec/tool"] {
            assert_eq!(mode(&output.join(file)), 0o6751);
            assert_eq!(fs::read(output.join(file)).unwrap(), b"executable");
            assert_ne!(
                fs::metadata(output.join(file)).unwrap().ino(),
                fs::metadata(source.join("private/tool")).unwrap().ino()
            );
        }
        assert_eq!(mode(&output.join("overrides/data")), 0o1640);
        assert_eq!(
            fs::read_link(output.join("payload/absolute")).unwrap(),
            Path::new("/outside/absolute")
        );
        assert_eq!(
            fs::read_link(output.join("overrides/relative")).unwrap(),
            Path::new("../dangling")
        );
        assert_eq!(
            fs::read_link(output.join("bin/tool")).unwrap(),
            Path::new("../payload/private/tool")
        );
        let owner = fs::metadata(&output).unwrap();
        assert_owner(&output, owner.uid(), owner.gid());
        make_tree_read_only(&output).unwrap();
        verify_tree_read_only(&output).unwrap();
        assert_eq!(mode(&output.join("payload/private")), 0o555);
        assert_eq!(mode(&output.join("libexec/tool")), 0o551);
    }

    #[test]
    fn rejects_all_unsafe_destinations_before_creating_output() {
        let temp = tempdir().unwrap();
        for dest in [
            "",
            "/absolute",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "nul\0path",
        ] {
            let output = temp.path().join("output");
            let error = run(
                &output,
                vec![PlainTreeOp::Symlink {
                    target: "target".into(),
                    dest: dest.into(),
                }],
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unsafe plain-tree destination"),
                "{error}"
            );
            assert!(!output.exists());
        }
    }

    #[test]
    fn rejects_duplicate_and_overlapping_destinations_in_both_orders() {
        let temp = tempdir().unwrap();
        for (first, second, expected) in [
            ("x", "x", "duplicate"),
            ("x", "x/child", "overlapping"),
            ("x/child", "x", "overlapping"),
            ("a/b", "a/b/c/d", "overlapping"),
        ] {
            let output = temp.path().join("output");
            let error = run(
                &output,
                [first, second]
                    .map(|dest| PlainTreeOp::Symlink {
                        target: "target".into(),
                        dest: dest.into(),
                    })
                    .to_vec(),
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!output.exists());
        }
    }

    #[test]
    fn a_copied_symlink_cannot_redirect_a_later_write_outside_output() {
        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let source = temp.path().join("source");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(outside.join("sentinel"), b"unchanged").unwrap();
        symlink(&outside, source.join("escape")).unwrap();
        let output = temp.path().join("output");
        let error = run(
            &output,
            vec![
                PlainTreeOp::CopyTree {
                    source,
                    dest: "tree".into(),
                },
                PlainTreeOp::CopyFile {
                    source: outside.join("sentinel"),
                    dest: "tree/escape/sentinel".into(),
                },
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("overlapping"), "{error}");
        assert!(!output.exists());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn parent_creation_refuses_symlinks_even_without_plan_validation() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("output");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let token = CancellationToken::new().unwrap();
        let cx = Composition {
            root: root.clone(),
            cancellation: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(token.fifo_path())
                .unwrap(),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        };
        let error = cx.create_parents(&root.join("link/child")).unwrap_err();
        assert!(
            matches!(error, ComposeError::Failed(message) if message.contains("not a real directory"))
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn refuses_symlink_roots_and_cleans_partial_copies_of_special_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a-regular"), b"copied before the error").unwrap();
        let linked = temp.path().join("linked");
        symlink(&source, &linked).unwrap();
        let output = temp.path().join("output");
        for source in [
            linked.clone(),
            PathBuf::from(format!("{}/", linked.display())),
            linked.join("."),
        ] {
            let error = run(
                &output,
                vec![PlainTreeOp::CopyTree {
                    source,
                    dest: "root".into(),
                }],
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("not a real directory"),
                "{error}"
            );
            assert!(!output.exists());
        }
        let error = run(
            &output,
            vec![PlainTreeOp::CopyFile {
                source: linked,
                dest: "file".into(),
            }],
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(!output.exists());
        let fifo = source.join("z-fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let socket = source.join("z-socket");
        let listener = UnixListener::bind(&socket).unwrap();
        for special in [&fifo, &socket] {
            // Test each special entry after a successful file copy, not merely
            // plan rejection before staging starts.
            let other = if special == &fifo { &socket } else { &fifo };
            let parked = temp.path().join("parked");
            fs::rename(other, &parked).unwrap();
            let error = run(
                &output,
                vec![PlainTreeOp::CopyTree {
                    source: source.clone(),
                    dest: "root".into(),
                }],
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unsupported special file"),
                "{error}"
            );
            assert!(!output.exists());
            fs::rename(parked, other).unwrap();
        }
        drop(listener);
        for source in [fifo, socket, source] {
            assert!(
                run(
                    &output,
                    vec![PlainTreeOp::CopyFile {
                        source,
                        dest: "file".into()
                    }]
                )
                .is_err()
            );
            assert!(!output.exists());
        }
    }

    #[test]
    fn never_overwrites_or_removes_existing_output_even_a_dangling_symlink() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("sentinel"), b"unchanged").unwrap();
        assert!(run(&output, vec![]).is_err());
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"unchanged");
        let link = temp.path().join("link");
        symlink("missing", &link).unwrap();
        assert!(run(&link, vec![]).is_err());
        assert_eq!(fs::read_link(link).unwrap(), Path::new("missing"));
    }

    #[test]
    fn cancellation_mid_tree_is_typed_and_removes_all_staging_state() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        for index in 0..4096 {
            fs::write(source.join(format!("file-{index:04}")), b"payload").unwrap();
        }
        let output = temp.path().join("output");
        let token = CancellationToken::new_in(temp.path()).unwrap();
        let input = PlainTreeComposeInput {
            output_root: output.clone(),
            ops: vec![PlainTreeOp::CopyTree {
                source,
                dest: "root".into(),
            }],
            cancellation_fifo: token.fifo_path().to_path_buf(),
        };
        let runtime_thread =
            thread::spawn(move || HostRuntime::new().run(&PlainTreeComposeFunction, input));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !output.join("root/file-0000").exists() {
            assert!(
                !runtime_thread.is_finished(),
                "composition ended before cancellation"
            );
            assert!(Instant::now() < deadline, "composition never started");
            thread::yield_now();
        }
        token.cancel();
        let result = runtime_thread.join().unwrap().unwrap();
        assert!(matches!(result, PlainTreeComposeOutput::Cancelled));
        let encoded = serde_json::to_vec(&result).unwrap();
        let decoded: PlainTreeComposeOutput = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(
            decoded.into_builder_result(),
            Err(crate::BuilderError::Cancelled(_))
        ));
        assert!(!output.exists());
        drop(token);
        let mut remaining: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec!["source"]);
    }

    #[test]
    fn already_cancelled_call_creates_no_output() {
        let temp = tempdir().unwrap();
        let token = CancellationToken::new().unwrap();
        token.cancel();
        let output = temp.path().join("output");
        let result = PlainTreeComposeFunction
            .call(PlainTreeComposeInput {
                output_root: output.clone(),
                ops: vec![],
                cancellation_fifo: token.fifo_path().to_path_buf(),
            })
            .unwrap();
        assert!(matches!(result, PlainTreeComposeOutput::Cancelled));
        assert!(!output.exists());
    }

    #[test]
    fn normalizes_foreign_ownership_when_privileged() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"tool").unwrap();
        symlink("tool", source.join("link")).unwrap();
        for path in [&source, &source.join("tool"), &source.join("link")] {
            lchown(path, Some(123), Some(456)).unwrap();
        }
        fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o6751)).unwrap();
        let output = temp.path().join("output");
        run(
            &output,
            vec![PlainTreeOp::CopyTree {
                source,
                dest: "root".into(),
            }],
        )
        .unwrap();
        let metadata = fs::metadata(&output).unwrap();
        assert_owner(&output, metadata.uid(), metadata.gid());
        assert_eq!(mode(&output.join("root/tool")), 0o6751);
    }
}
