use crate::runtime::{RuntimeError, TypedRuntimeFunction};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub struct Uppercase;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UppercaseInput {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UppercaseOutput {
    pub text: String,
    pub pid: u32,
}

impl TypedRuntimeFunction for Uppercase {
    type Input = UppercaseInput;
    type Output = UppercaseOutput;

    fn name(&self) -> &'static str {
        "uppercase"
    }

    fn call_typed(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        Ok(UppercaseOutput {
            text: input.text.to_uppercase(),
            pid: std::process::id(),
        })
    }
}
