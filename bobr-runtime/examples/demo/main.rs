#![allow(missing_docs)]
mod checked_divide;
mod namespace_identity;
mod uppercase;

use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use bobr_runtime::runtime_host::HostRuntime;
use bobr_runtime::runtime_ns::{NsFunction, NsRuntime, worker_invocation_from_env};
use checked_divide::{CheckedDivide, DivideInput};
use namespace_identity::{NamespaceIdentity, NamespaceIdentityInput};
use std::process::ExitCode;
use std::time::Duration;
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
    let identity = NamespaceIdentity;

    let host = HostRuntime::new();
    let namespace = NsRuntime::new()?.with_call_timeout(Duration::from_secs(30));

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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use checked_divide::DivideOutput;

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
}
