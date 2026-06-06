//! Typed runtime abstraction and example runtime implementations.
//!
//! The crate exposes a small typed [`runtime::Runtime`] interface, a host
//! implementation that calls functions in the current process, and a namespace
//! implementation that marshals each call to a fresh child process running in a
//! Linux user namespace.
//!
//! Application crates define their own [`runtime::RuntimeFunction`] values and
//! can execute the same typed function through either runtime implementation.

/// Shared typed runtime traits and error type.
pub mod runtime;

/// Runtime implementation that executes functions in the current host process.
pub mod runtime_host;

/// Runtime implementation that executes functions in a child user namespace.
pub mod runtime_ns;
