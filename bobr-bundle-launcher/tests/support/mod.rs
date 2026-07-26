#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

struct SharedLauncher {
    _temp: tempfile::TempDir,
    path: PathBuf,
}

fn shared_launcher() -> &'static PathBuf {
    static LAUNCHER: OnceLock<SharedLauncher> = OnceLock::new();
    &LAUNCHER
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("bobr-bundle-launcher");
            fs::copy(env!("CARGO_BIN_EXE_bobr-bundle-launcher"), &path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            SharedLauncher { _temp: temp, path }
        })
        .path
}

pub(crate) struct BundleFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl BundleFixture {
    pub(crate) fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("bobr bundle path with spaces ")
            .tempdir()
            .unwrap();
        let root = temp.path().join("bundle");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("libexec/wrapped-bin")).unwrap();
        fs::create_dir_all(root.join("root/usr/bin")).unwrap();
        fs::create_dir_all(root.join("root/usr/lib64")).unwrap();

        let launcher = root.join("libexec/bobr-bundle-launcher");
        // The shared inode is fully written before any test can spawn it.
        // Per-fixture copies would let forked children inherit another test
        // thread's write fd and intermittently fail execve with ETXTBSY.
        fs::hard_link(shared_launcher(), &launcher).unwrap();

        Self { _temp: temp, root }
    }

    pub(crate) fn launcher(&self) -> PathBuf {
        self.root.join("libexec/bobr-bundle-launcher")
    }

    pub(crate) fn write_config(&self, tool_path: &str, argv0: &str, loader_dirs: &[&str]) {
        self.write_config_with_environment(tool_path, argv0, loader_dirs, "");
    }

    pub(crate) fn write_config_with_environment(
        &self,
        tool_path: &str,
        argv0: &str,
        loader_dirs: &[&str],
        environment: &str,
    ) {
        let library_dirs = loader_dirs
            .iter()
            .map(|path| format!("{path:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            self.root.join("bundle.toml"),
            format!(
                r#"
format = "bobr-host-bundle-v1"
payload_root = "root"
policy = "strict"
[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"
[loader]
kind = "glibc"
library_dirs = [{library_dirs}]
inhibit_cache = true
{environment}
[tools.demo]
path = "{tool_path}"
argv0 = "{argv0}"
visibility = "public"
"#
            ),
        )
        .unwrap();
    }

    pub(crate) fn write_static_exit_fixture(&self, relative: &str, exit_code: u32) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, static_exit_elf(exit_code)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub(crate) fn write_dynamic_fixture(&self, relative: &str, interpreter: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, dynamic_elf(interpreter)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub(crate) fn write_script_fixture(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub(crate) fn add_public_wrapper(&self, name: &str) -> PathBuf {
        let path = self.root.join("bin").join(name);
        symlink("../libexec/bobr-bundle-launcher", &path).unwrap();
        path
    }

    pub(crate) fn add_internal_wrapper(&self, name: &str) -> PathBuf {
        let path = self.root.join("libexec/wrapped-bin").join(name);
        symlink("../bobr-bundle-launcher", &path).unwrap();
        path
    }

    pub(crate) fn command(&self) -> Command {
        Command::new(self.launcher())
    }
}

fn static_exit_elf(exit_code: u32) -> Vec<u8> {
    const HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;
    const BASE: u64 = 0x400000;
    let code = [
        0xb8,
        60,
        0,
        0,
        0, // mov eax, SYS_exit
        0xbf,
        exit_code as u8,
        (exit_code >> 8) as u8,
        (exit_code >> 16) as u8,
        (exit_code >> 24) as u8, // mov edi, exit_code
        0x0f,
        0x05, // syscall
    ];
    let code_offset = HEADER_SIZE + PROGRAM_HEADER_SIZE;
    let file_size = code_offset + code.len();
    let mut elf = vec![0_u8; file_size];

    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&2_u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1_u32.to_le_bytes());
    elf[24..32].copy_from_slice(&(BASE + code_offset as u64).to_le_bytes());
    elf[32..40].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
    elf[52..54].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    elf[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
    elf[56..58].copy_from_slice(&1_u16.to_le_bytes());

    let ph = HEADER_SIZE;
    elf[ph..ph + 4].copy_from_slice(&1_u32.to_le_bytes());
    elf[ph + 4..ph + 8].copy_from_slice(&5_u32.to_le_bytes());
    elf[ph + 8..ph + 16].copy_from_slice(&0_u64.to_le_bytes());
    elf[ph + 16..ph + 24].copy_from_slice(&BASE.to_le_bytes());
    elf[ph + 24..ph + 32].copy_from_slice(&BASE.to_le_bytes());
    elf[ph + 32..ph + 40].copy_from_slice(&(file_size as u64).to_le_bytes());
    elf[ph + 40..ph + 48].copy_from_slice(&(file_size as u64).to_le_bytes());
    elf[ph + 48..ph + 56].copy_from_slice(&0x1000_u64.to_le_bytes());
    elf[code_offset..].copy_from_slice(&code);
    elf
}

fn dynamic_elf(interpreter: &str) -> Vec<u8> {
    const HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;
    let mut interpreter = interpreter.as_bytes().to_vec();
    interpreter.push(0);
    let content_offset = HEADER_SIZE + PROGRAM_HEADER_SIZE;
    let mut elf = vec![0_u8; content_offset + interpreter.len()];

    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&3_u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1_u32.to_le_bytes());
    elf[32..40].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
    elf[52..54].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    elf[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
    elf[56..58].copy_from_slice(&1_u16.to_le_bytes());

    let ph = HEADER_SIZE;
    elf[ph..ph + 4].copy_from_slice(&3_u32.to_le_bytes());
    elf[ph + 8..ph + 16].copy_from_slice(&(content_offset as u64).to_le_bytes());
    elf[ph + 32..ph + 40].copy_from_slice(&(interpreter.len() as u64).to_le_bytes());
    elf[content_offset..].copy_from_slice(&interpreter);
    elf
}
