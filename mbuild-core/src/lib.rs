pub mod builder;
pub mod cas;
pub mod fsutil;

pub use builder::*;

#[derive(Debug)]
pub enum BuilderError {
    InvalidRecipe(String),
    ExecutionFailed(String),
    NotImplemented(String),
}
