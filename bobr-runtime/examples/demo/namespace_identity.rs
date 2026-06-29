use bobr_runtime::runtime::{RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;

#[derive(Debug, Clone, Copy)]
pub(crate) struct NamespaceIdentity;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamespaceIdentityInput;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamespaceIdentityOutput {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub effective_uid: u32,
    pub effective_gid: u32,
    pub uid_map: String,
    pub gid_map: String,
    pub setgroups: Option<String>,
}

impl RuntimeFunction for NamespaceIdentity {
    type Input = NamespaceIdentityInput;
    type Output = NamespaceIdentityOutput;

    fn name(&self) -> &'static str {
        "namespace-identity"
    }

    fn call(&self, _input: Self::Input) -> Result<Self::Output, RuntimeError> {
        Ok(NamespaceIdentityOutput {
            pid: std::process::id(),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            effective_uid: unsafe { libc::geteuid() },
            effective_gid: unsafe { libc::getegid() },
            uid_map: fs::read_to_string("/proc/self/uid_map")?,
            gid_map: fs::read_to_string("/proc/self/gid_map")?,
            setgroups: read_optional_to_string("/proc/self/setgroups")?,
        })
    }
}

fn read_optional_to_string(path: &str) -> Result<Option<String>, RuntimeError> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RuntimeError::from(error)),
    }
}
