use std::fmt;

pub mod builder;
pub mod cas;
pub mod fsutil;

pub use builder::*;
pub use cas::*;
pub use fsobj_hash::ObjectHash;

#[derive(Debug)]
pub enum BuilderError {
    InvalidRecipe(String),
    ExecutionFailed(String),
    NotImplemented(String),
}

impl fmt::Display for BuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(message)
            | Self::ExecutionFailed(message)
            | Self::NotImplemented(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BuilderError {}
