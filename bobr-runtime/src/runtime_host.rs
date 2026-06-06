use crate::runtime::{Runtime, RuntimeError, RuntimeFunction};

#[derive(Debug, Default)]
pub struct HostRuntime;

impl HostRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Runtime for HostRuntime {
    fn run<F>(&mut self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
    {
        function.call(input)
    }
}
