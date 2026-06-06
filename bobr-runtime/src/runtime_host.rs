//! Host runtime implementation.
//!
//! [`crate::runtime_host::HostRuntime`] executes runtime functions directly in the current process
//! and therefore in the current host namespaces. It is useful as a baseline
//! implementation and as a way to run the same typed function without any
//! process boundary or wire encoding.

use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};

/// Runtime that calls functions directly in the current process.
///
/// `HostRuntime` performs no marshalling and creates no child process. Calling
/// [`Runtime::run`] on this runtime is equivalent to calling
/// [`RuntimeFunction::call`] on the provided function.
#[derive(Debug, Default)]
pub struct HostRuntime;

impl HostRuntime {
    /// Create a host runtime instance.
    ///
    /// `HostRuntime` has no setup work and no internal resources. The
    /// constructor exists so call sites can construct all runtime
    /// implementations through an explicit `new` method:
    ///
    /// ```
    /// use rust_test::runtime_host::HostRuntime;
    ///
    /// let runtime = HostRuntime::new();
    /// ```
    ///
    /// The returned runtime executes functions in the current process and
    /// current namespaces.
    pub fn new() -> Self {
        Self
    }
}

impl Runtime for HostRuntime {
    fn run<F>(&self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
    {
        function.call(input)
    }
}
