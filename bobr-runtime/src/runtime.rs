use serde::Serialize;
use serde::de::DeserializeOwned;
use std::error::Error;
use std::fmt;

pub trait Runtime {
    fn run<F>(&mut self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction;
}

pub trait RuntimeFunction: Send + Sync {
    type Input: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;

    fn name(&self) -> &'static str;

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError>;
}

pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[derive(Debug)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
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
