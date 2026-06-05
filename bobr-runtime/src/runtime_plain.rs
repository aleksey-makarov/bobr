use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};

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
        input: Vec<u8>,
    ) -> Result<Vec<u8>, RuntimeError> {
        function.call_erased(&input)
    }
}
