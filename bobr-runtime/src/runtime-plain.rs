use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};
use serde_json::Value;

#[derive(Debug, Default)]
pub struct PlainRuntime;

impl PlainRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Runtime for PlainRuntime {
    fn run_erased(
        &mut self,
        function: &dyn RuntimeFunction,
        input: Value,
    ) -> Result<Value, RuntimeError> {
        function.call_erased(input)
    }
}
