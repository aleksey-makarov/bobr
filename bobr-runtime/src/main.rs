#[path = "checked-divide.rs"]
mod checked_divide;
mod runtime;
#[path = "runtime-child.rs"]
mod runtime_child;
#[path = "runtime-plain.rs"]
mod runtime_plain;
#[path = "uppercase.rs"]
mod uppercase;

use crate::runtime::{Runtime, RuntimeFunction, RuntimeResult, TypedRuntimeFunction};
use checked_divide::{CheckedDivide, DivideInput};
use runtime_child::ChildRuntime;
use runtime_plain::PlainRuntime;
use serde::Serialize;
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
    if std::env::args().nth(1).as_deref() == Some("--child-runtime-worker") {
        return runtime_child::run_worker(example_functions());
    }

    let uppercase = Uppercase;
    let divide = CheckedDivide;

    let mut plain = PlainRuntime::new();
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

    let mut child = ChildRuntime::spawn_current_exe()?;
    run_example(
        "child",
        &mut child,
        &uppercase,
        UppercaseInput {
            text: "hello from child runtime".to_string(),
        },
    )?;
    run_example(
        "child",
        &mut child,
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
    F: TypedRuntimeFunction,
    F::Output: Serialize,
{
    let output = runtime.run(function, input)?;
    println!(
        "{} via {runtime_name}: {}",
        function.spec().name,
        serde_json::to_string_pretty(&output)?
    );
    Ok(())
}

fn example_functions() -> Vec<Box<dyn RuntimeFunction>> {
    vec![Box::new(Uppercase), Box::new(CheckedDivide)]
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
