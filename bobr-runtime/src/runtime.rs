//! Shared typed runtime interface.
//!
//! A runtime executes values implementing [`crate::runtime::RuntimeFunction`]. The public
//! interface is fully typed: callers pass the function's associated input type,
//! may observe values of its event type, and receive the associated output
//! type. Individual runtime implementations decide whether that call is direct,
//! marshalled to another process, or handled by some other execution mechanism.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Executes typed runtime functions.
///
/// The trait intentionally exposes only typed calls. Runtime implementations
/// that need bytes, frames, or codecs keep those transport details behind their
/// own public API instead of leaking them into this shared interface.
pub trait Runtime {
    /// Execute `function`, delivering each typed event before the final output.
    ///
    /// Events are delivered synchronously and in emission order. The callback
    /// is never invoked after this method returns.
    fn run_with_events<F, H>(
        &self,
        function: &F,
        input: F::Input,
        on_event: H,
    ) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
        H: FnMut(F::Event);

    /// Execute `function` with the typed `input` value.
    ///
    /// Implementations may run the function directly or forward it to another
    /// execution environment, but they must preserve the function's typed input
    /// and output contract.
    fn run<F>(&self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
    {
        self.run_with_events(function, input, |_| {})
    }
}

/// Event type for runtime functions that never emit events.
///
/// Unlike `()`, this type has no value, so a function cannot accidentally emit
/// a meaningless event. It remains serializable for the uniform runtime
/// function contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoRuntimeEvent {}

/// A typed operation that can be executed by a [`Runtime`].
///
/// `RuntimeFunction` values declare the concrete input, event, and output
/// payload types used by the runtime call. The payload types must implement
/// serde serialization because transport runtimes may need to encode them when
/// sending the call across a process boundary.
pub trait RuntimeFunction: Send + Sync {
    /// Typed input accepted by the function.
    type Input: Serialize + DeserializeOwned;

    /// Typed output produced by the function.
    type Output: Serialize + DeserializeOwned;

    /// Typed intermediate event produced before the final output.
    type Event: Serialize + DeserializeOwned;

    /// Stable function name used by runtimes that dispatch through a registry.
    ///
    /// Names should be unique within a worker registry.
    fn name(&self) -> &'static str;

    /// Execute the function in the current process.
    ///
    /// Direct runtimes call this method immediately. Transport runtimes call it
    /// on the worker side after decoding the input payload.
    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError>;

    /// Execute the function and deliver intermediate events to `emit`.
    ///
    /// Functions without events inherit this implementation unchanged. An
    /// event-producing function overrides it and keeps [`Self::call`] as its
    /// no-observer entry point. Event-producing functions must propagate an
    /// error returned by `emit`, because a transport runtime may no longer be
    /// able to deliver a final result after such an error.
    fn call_with_events(
        &self,
        input: Self::Input,
        _emit: &mut dyn FnMut(Self::Event) -> Result<(), RuntimeError>,
    ) -> Result<Self::Output, RuntimeError> {
        self.call(input)
    }
}

/// Result type used by runtime APIs.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Error type returned by runtime functions and runtime implementations.
///
/// This example keeps errors as displayable messages so the focus stays on the
/// runtime boundary rather than on a detailed error taxonomy.
#[derive(Debug)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    /// Create a runtime error from a displayable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}
