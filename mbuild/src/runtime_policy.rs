use mbuild_core::{RuntimeBackend, RuntimeProvider};

pub(crate) fn runtime_provider_for_current_process() -> RuntimeProvider {
    match runtime_backend_for_effective_uid(effective_uid()) {
        RuntimeBackend::Host => RuntimeProvider::host(),
        RuntimeBackend::Namespace => RuntimeProvider::namespace(),
    }
}

pub(crate) fn runtime_backend_for_effective_uid(euid: u32) -> RuntimeBackend {
    if euid == 0 {
        RuntimeBackend::Host
    } else {
        RuntimeBackend::Namespace
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

#[cfg(not(unix))]
fn effective_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_effective_uid_uses_host_runtime() {
        assert_eq!(runtime_backend_for_effective_uid(0), RuntimeBackend::Host);
    }

    #[test]
    fn non_root_effective_uid_uses_namespace_runtime() {
        assert_eq!(
            runtime_backend_for_effective_uid(1000),
            RuntimeBackend::Namespace
        );
    }
}
