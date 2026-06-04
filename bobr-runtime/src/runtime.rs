use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionSpec {
    pub name: &'static str,
}

pub trait Runtime {
    fn run_erased(
        &mut self,
        function: &dyn RuntimeFunction,
        input: Value,
    ) -> Result<Value, RuntimeError>;

    fn run<F>(&mut self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        Self: Sized,
        F: TypedRuntimeFunction,
    {
        let input = serde_json::to_value(input)
            .map_err(|error| RuntimeError::new(format!("failed to encode input: {error}")))?;
        let output = self.run_erased(function, input)?;
        serde_json::from_value(output)
            .map_err(|error| RuntimeError::new(format!("failed to decode output: {error}")))
    }
}

pub trait RuntimeFunction: Send + Sync {
    fn spec(&self) -> &'static FunctionSpec;

    fn call_erased(&self, input: Value) -> Result<Value, RuntimeError>;
}

pub trait TypedRuntimeFunction: Send + Sync {
    type Input: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;

    fn spec(&self) -> &'static FunctionSpec;

    fn call_typed(&self, input: Self::Input) -> Result<Self::Output, RuntimeError>;
}

impl<T> RuntimeFunction for T
where
    T: TypedRuntimeFunction,
{
    fn spec(&self) -> &'static FunctionSpec {
        <T as TypedRuntimeFunction>::spec(self)
    }

    fn call_erased(&self, input: Value) -> Result<Value, RuntimeError> {
        let input = serde_json::from_value(input).map_err(|error| {
            RuntimeError::new(format!(
                "invalid input for '{}': {error}",
                <T as TypedRuntimeFunction>::spec(self).name
            ))
        })?;
        let output = self.call_typed(input)?;
        serde_json::to_value(output).map_err(|error| {
            RuntimeError::new(format!(
                "invalid output from '{}': {error}",
                <T as TypedRuntimeFunction>::spec(self).name
            ))
        })
    }
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

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}
