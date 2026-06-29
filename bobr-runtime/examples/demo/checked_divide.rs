use bobr_runtime::runtime::{RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckedDivide;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DivideInput {
    pub dividend: i64,
    pub divisor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DivideOutput {
    pub quotient: i64,
    pub remainder: i64,
    pub pid: u32,
}

impl RuntimeFunction for CheckedDivide {
    type Input = DivideInput;
    type Output = DivideOutput;

    fn name(&self) -> &'static str {
        "checked-divide"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        if input.divisor == 0 {
            return Err(RuntimeError::new("division by zero"));
        }

        Ok(DivideOutput {
            quotient: input.dividend / input.divisor,
            remainder: input.dividend % input.divisor,
            pid: std::process::id(),
        })
    }
}
