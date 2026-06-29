//! Executes a `bobr` build request: a JSON DAG of recipe nodes producing one
//! realized object.
//!
//! A request names a content-addressed store and a table of `nodes` keyed by id,
//! with a reserved `root` node as the build target; dependencies are id
//! references in input slots. [`execute_request`] reads and runs a request file;
//! [`execute`] runs an already-parsed [`Request`]. Both return the root's
//! realized [`ObjectHash`](bobr_core::ObjectHash), building only what is missing
//! from the store and reusing the rest.
//!
//! This is the library entry point; the `bobr` binary is a thin wrapper that
//! reads the request from a file or stdin.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod builder_registry;
mod collect_graph;
mod execution;
mod planned;
mod request;
mod resolved_inputs;

pub use execution::{ExecutionError, execute, execute_request};
pub use request::Request;
