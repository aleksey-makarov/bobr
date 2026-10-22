//! Host runtime implementation.
//!
//! [`crate::runtime_host::HostRuntime`] executes runtime functions directly in the current process
//! and therefore in the current host namespaces. It is useful as a baseline
//! implementation and as a way to run the same typed function without any
//! process boundary or wire encoding.

use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};

/// Runtime that calls functions directly in the current process.
///
/// `HostRuntime` performs no marshalling and creates no child process.
/// [`Runtime::run_with_events`] calls
/// [`RuntimeFunction::call_with_events`] directly; [`Runtime::run`] uses the
/// same path and discards intermediate events.
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
    /// use bobr_runtime::runtime_host::HostRuntime;
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
    fn run_with_events<F, H>(
        &self,
        function: &F,
        input: F::Input,
        mut on_event: H,
    ) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
        H: FnMut(F::Event),
    {
        function.call_with_events(input, &mut |event| {
            on_event(event);
            Ok(())
        })
    }
}
