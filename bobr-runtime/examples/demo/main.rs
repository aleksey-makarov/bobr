#![allow(missing_docs)]
mod checked_divide;
mod countdown;
mod namespace_identity;
mod uppercase;

use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use bobr_runtime::runtime_host::HostRuntime;
use bobr_runtime::runtime_ns::{NsFunction, NsRuntime, worker_invocation_from_env};
use checked_divide::{CheckedDivide, DivideInput};
use countdown::{Countdown, CountdownInput};
use namespace_identity::{NamespaceIdentity, NamespaceIdentityInput};
use std::process::ExitCode;
use uppercase::{Uppercase, UppercaseInput};

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> RuntimeResult<()> {
    if let Some(invocation) = worker_invocation_from_env()? {
        return bobr_runtime::runtime_ns::run_worker(invocation, example_functions());
    }

    let uppercase = Uppercase;
    let divide = CheckedDivide;
    let countdown = Countdown;
    let identity = NamespaceIdentity;

    let host = HostRuntime::new();
    let namespace = NsRuntime::new()?;

    run_example(
        "host",
        &host,
        &uppercase,
        UppercaseInput {
            text: "hello from runtime".to_string(),
        },
    )?;
    run_example(
        "host",
        &host,
        &divide,
        DivideInput {
            dividend: 42,
            divisor: 5,
        },
    )?;
    run_example("host", &host, &identity, NamespaceIdentityInput)?;
    run_event_example("host", &host, &countdown, CountdownInput { from: 3 })?;
    run_example(
        "namespace",
        &namespace,
        &uppercase,
        UppercaseInput {
            text: "hello from namespace runtime".to_string(),
        },
    )?;
    run_example(
        "namespace",
        &namespace,
        &divide,
        DivideInput {
            dividend: 42,
            divisor: 5,
        },
    )?;
    run_example("namespace", &namespace, &identity, NamespaceIdentityInput)?;
    run_event_example(
        "namespace",
        &namespace,
        &countdown,
        CountdownInput { from: 3 },
    )?;

    Ok(())
}

fn run_event_example<R, F>(
    runtime_name: &str,
    runtime: &R,
    function: &F,
    input: F::Input,
) -> RuntimeResult<()>
where
    R: Runtime,
    F: RuntimeFunction,
    F::Event: std::fmt::Debug,
{
    let output = runtime.run_with_events(function, input, |event| {
        println!("{} via {runtime_name} event: {event:?}", function.name());
    })?;
    let output = serde_json::to_string_pretty(&output)
        .map_err(|error| RuntimeError::new(error.to_string()))?;
    println!("{} via {runtime_name} output: {output}", function.name());
    Ok(())
}

fn run_example<R, F>(
    runtime_name: &str,
    runtime: &R,
    function: &F,
    input: F::Input,
) -> RuntimeResult<()>
where
    R: Runtime,
    F: RuntimeFunction,
{
    let output = runtime.run(function, input)?;
    let output = serde_json::to_string_pretty(&output)
        .map_err(|error| RuntimeError::new(error.to_string()))?;
    println!("{} via {runtime_name}: {}", function.name(), output);
    Ok(())
}

fn example_functions() -> Vec<NsFunction> {
    vec![
        NsFunction::new(Uppercase),
        NsFunction::new(CheckedDivide),
        NsFunction::new(NamespaceIdentity),
        NsFunction::new(Countdown),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use checked_divide::DivideOutput;
    use countdown::CountdownEvent;

    #[test]
    fn plain_runtime_calls_typed_function_through_runtime_trait() {
        let runtime = HostRuntime::new();

        let output = runtime
            .run(
                &Uppercase,
                UppercaseInput {
                    text: "abc".to_string(),
                },
            )
            .unwrap();

        assert_eq!(output.text, "ABC");
        assert_eq!(output.pid, std::process::id());
    }

    #[test]
    fn typed_adapter_returns_function_errors() {
        let runtime = HostRuntime::new();

        let error = runtime
            .run(
                &CheckedDivide,
                DivideInput {
                    dividend: 1,
                    divisor: 0,
                },
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "division by zero");
    }

    #[test]
    fn divide_output_shape_is_plain_data() {
        let output = DivideOutput {
            quotient: 8,
            remainder: 2,
            pid: 123,
        };

        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::json!({ "quotient": 8, "remainder": 2, "pid": 123 })
        );
    }

    #[test]
    fn namespace_identity_reports_current_process_maps() {
        let runtime = HostRuntime::new();

        let output = runtime
            .run(&NamespaceIdentity, NamespaceIdentityInput)
            .unwrap();

        assert_eq!(output.pid, std::process::id());
        assert_eq!(output.effective_uid, unsafe { libc::geteuid() });
        assert_eq!(output.effective_gid, unsafe { libc::getegid() });
        assert!(!output.uid_map.trim().is_empty());
        assert!(!output.gid_map.trim().is_empty());
    }

    #[test]
    fn eventful_function_reports_events_before_output() {
        let runtime = HostRuntime::new();
        let mut events = Vec::new();

        let output = runtime
            .run_with_events(&Countdown, CountdownInput { from: 3 }, |event| {
                events.push(event);
            })
            .unwrap();

        assert_eq!(
            events,
            vec![
                CountdownEvent::Tick {
                    value: 3,
                    pid: std::process::id(),
                },
                CountdownEvent::Tick {
                    value: 2,
                    pid: std::process::id(),
                },
                CountdownEvent::Tick {
                    value: 1,
                    pid: std::process::id(),
                },
            ]
        );
        assert_eq!(output.ticks, 3);
        assert_eq!(output.pid, std::process::id());
    }

    #[test]
    fn plain_run_discards_events_without_changing_output() {
        let runtime = HostRuntime::new();

        let through_runtime = runtime.run(&Countdown, CountdownInput { from: 2 }).unwrap();
        let direct = Countdown.call(CountdownInput { from: 2 }).unwrap();

        assert_eq!(through_runtime, direct);
    }

    #[test]
    fn eventful_function_supports_zero_and_one_event() {
        let runtime = HostRuntime::new();

        for from in [0, 1] {
            let mut events = Vec::new();
            let output = runtime
                .run_with_events(&Countdown, CountdownInput { from }, |event| {
                    events.push(event);
                })
                .unwrap();

            assert_eq!(events.len(), from as usize);
            assert_eq!(output.ticks, from);
        }
    }
}
