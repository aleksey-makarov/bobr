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
use std::time::Duration;

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

struct RuntimeProviderInner<C> {
    backend: RuntimeBackend,
    namespace_call_timeout: Option<Duration>,
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

    /// Create a namespace provider with a per-call timeout.
    ///
    /// The timeout is applied to the lazily constructed [`NsRuntime`].
    pub fn namespace_with_call_timeout(timeout: Duration) -> Self {
        Self::namespace_with_codec_call_timeout(timeout)
    }
}

impl<C> RuntimeProvider<C> {
    /// Create a provider using `backend` and codec `C`.
    ///
    /// Codec `C` is used only by namespace providers. Host providers execute
    /// functions in-process and do not encode payloads.
    pub fn new_with_codec(backend: RuntimeBackend) -> Self {
        Self::from_parts(backend, None)
    }

    /// Create a host provider using codec `C` for any future namespace calls.
    pub fn host_with_codec() -> Self {
        Self::from_parts(RuntimeBackend::Host, None)
    }

    /// Create a namespace provider using codec `C`.
    ///
    /// Namespace setup is deferred until the first [`Runtime::run`] call.
    pub fn namespace_with_codec() -> Self {
        Self::from_parts(RuntimeBackend::Namespace, None)
    }

    /// Create a namespace provider using codec `C` and a per-call timeout.
    pub fn namespace_with_codec_call_timeout(timeout: Duration) -> Self {
        Self::from_parts(RuntimeBackend::Namespace, Some(timeout))
    }

    /// Return the selected execution backend.
    pub fn backend(&self) -> RuntimeBackend {
        self.inner.backend
    }

    /// Return the configured namespace per-call timeout.
    ///
    /// `None` means namespace calls run without a runtime-enforced deadline.
    /// For host providers this value is always `None`.
    pub fn namespace_call_timeout(&self) -> Option<Duration> {
        self.inner.namespace_call_timeout
    }

    fn from_parts(backend: RuntimeBackend, namespace_call_timeout: Option<Duration>) -> Self {
        let namespace_call_timeout = match backend {
            RuntimeBackend::Host => None,
            RuntimeBackend::Namespace => namespace_call_timeout,
        };
        Self {
            inner: Arc::new(RuntimeProviderInner {
                backend,
                namespace_call_timeout,
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

        let mut runtime = NsRuntime::<C>::new_with_codec()?;
        if let Some(timeout) = self.inner.namespace_call_timeout {
            runtime = runtime.with_call_timeout(timeout);
        }
        let runtime = Arc::new(runtime);
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

#[cfg(test)]
mod tests {
    use super::{RuntimeBackend, RuntimeProvider};
    use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

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
        assert_eq!(runtime.namespace_call_timeout(), None);
    }

    #[test]
    fn namespace_provider_construction_is_lazy() {
        let runtime = RuntimeProvider::namespace();

        assert_eq!(runtime.backend(), RuntimeBackend::Namespace);
        assert_eq!(runtime.namespace_call_timeout(), None);
    }

    #[test]
    fn namespace_timeout_is_recorded() {
        let timeout = Duration::from_secs(5);
        let runtime = RuntimeProvider::namespace_with_call_timeout(timeout);

        assert_eq!(runtime.backend(), RuntimeBackend::Namespace);
        assert_eq!(runtime.namespace_call_timeout(), Some(timeout));
    }

    #[test]
    fn host_provider_ignores_namespace_timeout() {
        let runtime: RuntimeProvider = RuntimeProvider::new_with_codec(RuntimeBackend::Host);

        assert_eq!(runtime.backend(), RuntimeBackend::Host);
        assert_eq!(runtime.namespace_call_timeout(), None);
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
