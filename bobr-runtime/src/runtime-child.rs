use crate::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct ChildRuntime {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl ChildRuntime {
    pub fn spawn_current_exe() -> RuntimeResult<Self> {
        let executable = env::current_exe().map_err(|error| {
            RuntimeError::new(format!("failed to locate current executable: {error}"))
        })?;
        let mut child = Command::new(executable)
            .arg("--child-runtime-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                RuntimeError::new(format!("failed to spawn child runtime: {error}"))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RuntimeError::new("child runtime stdin pipe was not created"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RuntimeError::new("child runtime stdout pipe was not created"))?;

        Ok(Self {
            child,
            stdin: Some(BufWriter::new(stdin)),
            stdout: BufReader::new(stdout),
            next_id: 0,
        })
    }
}

impl Runtime for ChildRuntime {
    fn run_erased(
        &mut self,
        function: &dyn RuntimeFunction,
        input: Value,
    ) -> Result<Value, RuntimeError> {
        let id = self.next_id;
        self.next_id += 1;

        let request = ParentToChild::Call {
            id,
            function: function.spec().name.to_string(),
            input,
        };
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| RuntimeError::new("child runtime stdin is closed"))?;
        serde_json::to_writer(&mut *stdin, &request)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;

        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line)?;
        if bytes == 0 {
            return Err(RuntimeError::new("child runtime exited without response"));
        }

        match serde_json::from_str::<ChildToParent>(&line)? {
            ChildToParent::Ok {
                id: response_id,
                output,
            } if response_id == id => Ok(output),
            ChildToParent::Err {
                id: response_id,
                message,
            } if response_id == id => Err(RuntimeError::new(format!(
                "child failed while running '{}': {message}",
                function.spec().name
            ))),
            response => Err(RuntimeError::new(format!(
                "child returned response for the wrong call: expected id {id}, got id {}",
                response.id()
            ))),
        }
    }
}

impl Drop for ChildRuntime {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = serde_json::to_writer(&mut stdin, &ParentToChild::Shutdown);
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
        let _ = self.child.wait();
    }
}

pub fn run_worker(functions: Vec<Box<dyn RuntimeFunction>>) -> RuntimeResult<()> {
    let mut registry = BTreeMap::<String, Box<dyn RuntimeFunction>>::new();
    for function in functions {
        let name = function.spec().name.to_string();
        if registry.insert(name.clone(), function).is_some() {
            return Err(RuntimeError::new(format!(
                "duplicate child runtime function '{name}'"
            )));
        }
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        match serde_json::from_str::<ParentToChild>(&line?)? {
            ParentToChild::Call {
                id,
                function,
                input,
            } => {
                let response = match registry.get(&function) {
                    Some(function) => match function.call_erased(input) {
                        Ok(output) => ChildToParent::Ok { id, output },
                        Err(error) => ChildToParent::Err {
                            id,
                            message: error.to_string(),
                        },
                    },
                    None => ChildToParent::Err {
                        id,
                        message: format!("unknown function '{function}'"),
                    },
                };
                serde_json::to_writer(&mut stdout, &response)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            ParentToChild::Shutdown => break,
        }
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParentToChild {
    Call {
        id: u64,
        function: String,
        input: Value,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildToParent {
    Ok { id: u64, output: Value },
    Err { id: u64, message: String },
}

impl ChildToParent {
    fn id(&self) -> u64 {
        match self {
            Self::Ok { id, .. } | Self::Err { id, .. } => *id,
        }
    }
}
