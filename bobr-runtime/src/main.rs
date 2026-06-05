mod checked_divide;
mod runtime;
mod runtime_ns;
mod runtime_plain;
mod uppercase;

use crate::runtime::{Runtime, RuntimeFunction, RuntimeResult};
use checked_divide::{CheckedDivide, DivideInput};
use runtime_ns::{NsFunction, NsRuntime};
use runtime_plain::PlainRuntime;
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
    if std::env::args().nth(1).as_deref() == Some("--ns-runtime-worker") {
        return runtime_ns::run_worker(example_functions());
    }

    let uppercase = Uppercase;
    let divide = CheckedDivide;

    let mut plain = PlainRuntime::new();
    let mut namespace = NsRuntime::new()?;

    run_example(
        "plain",
        &mut plain,
        &uppercase,
        UppercaseInput {
            text: "hello from runtime".to_string(),
        },
    )?;
    run_example(
        "plain",
        &mut plain,
        &divide,
        DivideInput {
            dividend: 42,
            divisor: 5,
        },
    )?;
    run_example(
        "namespace",
        &mut namespace,
        &uppercase,
        UppercaseInput {
            text: "hello from namespace runtime".to_string(),
        },
    )?;
    run_example(
        "namespace",
        &mut namespace,
        &divide,
        DivideInput {
            dividend: 42,
            divisor: 5,
        },
    )?;

    Ok(())
}

fn run_example<R, F>(
    runtime_name: &str,
    runtime: &mut R,
    function: &F,
    input: F::Input,
) -> RuntimeResult<()>
where
    R: Runtime,
    F: RuntimeFunction,
{
    let output = runtime.run(function, input)?;
    let output = serde_json::to_string_pretty(&output)
        .map_err(|error| crate::runtime::RuntimeError::new(error.to_string()))?;
    println!("{} via {runtime_name}: {}", function.name(), output);
    Ok(())
}

fn example_functions() -> Vec<NsFunction> {
    vec![NsFunction::new(Uppercase), NsFunction::new(CheckedDivide)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use checked_divide::DivideOutput;

    #[test]
    fn plain_runtime_calls_typed_function_through_runtime_trait() {
        let mut runtime = PlainRuntime::new();

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
        let mut runtime = PlainRuntime::new();

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
}
