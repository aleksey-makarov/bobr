use serde::Serialize;
use serde::de::DeserializeOwned;
use std::error::Error;
use std::fmt;

pub trait WireCodec {
    fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, RuntimeError>;

    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, RuntimeError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JsonCodec;

impl WireCodec for JsonCodec {
    fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, RuntimeError> {
        serde_json::to_vec(value).map_err(RuntimeError::from)
    }

    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, RuntimeError> {
        serde_json::from_slice(bytes).map_err(RuntimeError::from)
    }
}

pub trait Runtime {
    fn run_erased(
        &mut self,
        function: &dyn RuntimeFunction,
        input: Vec<u8>,
    ) -> Result<Vec<u8>, RuntimeError>;

    fn run<F>(&mut self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        Self: Sized,
        F: TypedRuntimeFunction,
    {
        let input = <JsonCodec as WireCodec>::encode(&input)
            .map_err(|error| RuntimeError::new(format!("failed to encode input: {error}")))?;
        let output = self.run_erased(function, input)?;
        <JsonCodec as WireCodec>::decode(&output)
            .map_err(|error| RuntimeError::new(format!("failed to decode output: {error}")))
    }
}

pub trait RuntimeFunction: Send + Sync {
    fn name(&self) -> &'static str;

    fn call_erased(&self, input: &[u8]) -> Result<Vec<u8>, RuntimeError>;
}

pub trait TypedRuntimeFunction: Send + Sync {
    type Input: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;

    fn name(&self) -> &'static str;

    fn call_typed(&self, input: Self::Input) -> Result<Self::Output, RuntimeError>;
}

impl<T> RuntimeFunction for T
where
    T: TypedRuntimeFunction,
{
    fn name(&self) -> &'static str {
        <T as TypedRuntimeFunction>::name(self)
    }

    fn call_erased(&self, input: &[u8]) -> Result<Vec<u8>, RuntimeError> {
        let input = <JsonCodec as WireCodec>::decode(input).map_err(|error| {
            RuntimeError::new(format!(
                "invalid input for '{}': {error}",
                <T as TypedRuntimeFunction>::name(self)
            ))
        })?;
        let output = self.call_typed(input)?;
        <JsonCodec as WireCodec>::encode(&output).map_err(|error| {
            RuntimeError::new(format!(
                "invalid output from '{}': {error}",
                <T as TypedRuntimeFunction>::name(self)
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
