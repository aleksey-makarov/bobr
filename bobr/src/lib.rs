//! Executes a unified `bobr` request: a JSON DAG of recipe nodes producing an
//! ordered set of complete goal objects.
//!
//! A request names a content-addressed store and a table of `nodes` keyed by id,
//! plus a non-empty ordered `goals` list; dependencies are id references in
//! input slots. [`realize`] performs acquisition, dynamic reuse, and builder
//! execution in one asynchronous run.
//!
//! This is the library entry point; the `bobr` binary is a thin wrapper that
//! reads the request from a file or stdin.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod builder_registry;
mod error;
mod realize;
mod request;

pub use error::ExecutionError;
pub use realize::{GoalResult, realize};
pub use request::{REQUEST_SCHEMA, Request};
