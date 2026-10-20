//! Concrete runtime provider.
//!
//! [`RuntimeProvider`](crate::runtime_provider::RuntimeProvider) is a
//! cloneable handle that implements the typed [`crate::runtime::Runtime`]
//! interface while hiding whether calls are executed in the host process or in
//! a namespace worker. It is intended for application contexts that need to
//! store a concrete runtime value, because [`crate::runtime::Runtime`] is not
//! object-safe.

use crate::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use crate::runtime_host::HostRuntime;
use crate::runtime_ns::{JsonCodec, NsRuntime, WireCodec};
use std::sync::{Arc, Mutex};

/// Execution backend selected for a [`RuntimeProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeBackend {
    /// Execute runtime functions directly in the current process.
    Host,

    /// Execute runtime functions in a child Linux user namespace.
    Namespace,
}

/// Cloneable concrete runtime handle.
///
/// The provider implements [`Runtime`] and can therefore run typed
/// [`RuntimeFunction`] values directly. Host providers delegate to
/// [`HostRuntime`]. Namespace providers lazily construct [`NsRuntime`] on the
/// first namespace call, so constructing the provider does not require
/// `newuidmap`, `newgidmap`, or subuid/subgid setup to be available.
///
/// The namespace runtime is shared by cloned providers. It is initialized at
/// most once after a successful setup. If initialization fails, the error is
/// returned to the caller and a later call may retry initialization.
#[derive(Clone)]
pub struct RuntimeProvider<C = JsonCodec> {
    inner: Arc<RuntimeProviderInner<C>>,
}

impl<C> std::fmt::Debug for RuntimeProvider<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeProvider")
            .field("backend", &self.inner.backend)
            .finish_non_exhaustive()
    }
}

struct RuntimeProviderInner<C> {
    backend: RuntimeBackend,
    namespace_runtime: Mutex<Option<Arc<NsRuntime<C>>>>,
}

impl RuntimeProvider<JsonCodec> {
    /// Create a provider using `backend` and the default JSON namespace codec.
    ///
    /// This constructor performs no namespace setup. For namespace providers,
    /// setup is deferred until the first [`Runtime::run`] call.
    pub fn new(backend: RuntimeBackend) -> Self {
        Self::new_with_codec(backend)
    }

    /// Create a host provider using the default JSON namespace codec.
    pub fn host() -> Self {
        Self::host_with_codec()
    }

    /// Create a namespace provider using the default JSON namespace codec.
    ///
    /// This constructor is lazy and does not validate namespace prerequisites.
    pub fn namespace() -> Self {
        Self::namespace_with_codec()
    }
}

impl<C> RuntimeProvider<C> {
    /// Create a provider using `backend` and codec `C`.
    ///
    /// Codec `C` is used only by namespace providers. Host providers execute
    /// functions in-process and do not encode payloads.
    pub fn new_with_codec(backend: RuntimeBackend) -> Self {
        Self::from_backend(backend)
    }

    /// Create a host provider using codec `C` for any future namespace calls.
    pub fn host_with_codec() -> Self {
        Self::from_backend(RuntimeBackend::Host)
    }

    /// Create a namespace provider using codec `C`.
    ///
    /// Namespace setup is deferred until the first [`Runtime::run`] call.
    pub fn namespace_with_codec() -> Self {
        Self::from_backend(RuntimeBackend::Namespace)
    }

    /// Return the selected execution backend.
    pub fn backend(&self) -> RuntimeBackend {
        self.inner.backend
    }

    fn from_backend(backend: RuntimeBackend) -> Self {
        Self {
            inner: Arc::new(RuntimeProviderInner {
                backend,
                namespace_runtime: Mutex::new(None),
            }),
        }
    }
}

impl<C> RuntimeProvider<C>
where
    C: WireCodec,
{
    fn namespace_runtime(&self) -> RuntimeResult<Arc<NsRuntime<C>>> {
        let mut guard = self
            .inner
            .namespace_runtime
            .lock()
            .map_err(|_| RuntimeError::new("namespace runtime provider lock was poisoned"))?;
        if let Some(runtime) = guard.as_ref() {
            return Ok(Arc::clone(runtime));
        }

        let runtime = Arc::new(NsRuntime::<C>::new_with_codec()?);
        *guard = Some(Arc::clone(&runtime));
        Ok(runtime)
    }
}

impl<C> Runtime for RuntimeProvider<C>
where
    C: WireCodec,
{
    fn run<F>(&self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
    {
        match self.inner.backend {
            RuntimeBackend::Host => HostRuntime::new().run(function, input),
            RuntimeBackend::Namespace => {
                let runtime = self.namespace_runtime()?;
                runtime.run(function, input)
            }
        }
    }
}

/// Returns the runtime provider for the current process, choosing the backend
/// from its effective user id.
pub fn runtime_provider_for_current_process() -> RuntimeProvider {
    match runtime_backend_for_effective_uid(effective_uid()) {
        RuntimeBackend::Host => RuntimeProvider::host(),
        RuntimeBackend::Namespace => RuntimeProvider::namespace(),
    }
}

/// Maps an effective user id to its runtime backend: root runs in the host
/// process, everyone else runs in a namespace worker.
pub fn runtime_backend_for_effective_uid(euid: u32) -> RuntimeBackend {
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
    use super::{RuntimeBackend, RuntimeProvider, runtime_backend_for_effective_uid};
    use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};
    use serde::{Deserialize, Serialize};

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

    #[derive(Debug, Serialize, Deserialize)]
    struct EchoInput {
        value: String,
    }

    struct EchoFunction;

    impl RuntimeFunction for EchoFunction {
        type Input = EchoInput;
        type Output = String;

        fn name(&self) -> &'static str {
            "test.echo"
        }

        fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
            Ok(input.value)
        }
    }

    #[test]
    fn host_provider_runs_function_directly() {
        let runtime = RuntimeProvider::host();

        let output = runtime
            .run(
                &EchoFunction,
                EchoInput {
                    value: "hello".to_string(),
                },
            )
            .unwrap();

        assert_eq!(output, "hello");
        assert_eq!(runtime.backend(), RuntimeBackend::Host);
    }

    #[test]
    fn namespace_provider_construction_is_lazy() {
        let runtime = RuntimeProvider::namespace();

        assert_eq!(runtime.backend(), RuntimeBackend::Namespace);
    }

    #[test]
    fn provider_is_clone_send_and_sync() {
        fn assert_clone_send_sync<T: Clone + Send + Sync>() {}

        assert_clone_send_sync::<RuntimeProvider>();

        let runtime = RuntimeProvider::host();
        let cloned = runtime.clone();
        assert_eq!(cloned.backend(), RuntimeBackend::Host);
    }
}
