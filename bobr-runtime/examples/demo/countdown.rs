use bobr_runtime::runtime::{RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Countdown;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CountdownInput {
    pub from: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum CountdownEvent {
    Tick { value: u32, pid: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CountdownOutput {
    pub ticks: u32,
    pub pid: u32,
}

impl RuntimeFunction for Countdown {
    type Input = CountdownInput;
    type Output = CountdownOutput;
    type Event = CountdownEvent;

    fn name(&self) -> &'static str {
        "countdown"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        self.call_with_events(input, &mut |_| Ok(()))
    }

    fn call_with_events(
        &self,
        input: Self::Input,
        emit: &mut dyn FnMut(Self::Event) -> Result<(), RuntimeError>,
    ) -> Result<Self::Output, RuntimeError> {
        let pid = std::process::id();
        for value in (1..=input.from).rev() {
            emit(CountdownEvent::Tick { value, pid })?;
        }
        Ok(CountdownOutput {
            ticks: input.from,
            pid,
        })
    }
}
