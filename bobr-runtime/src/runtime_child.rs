use crate::runtime::{JsonCodec, Runtime, RuntimeError, RuntimeFunction, RuntimeResult, WireCodec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

pub struct ChildRuntime {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl ChildRuntime {
    pub fn new() -> RuntimeResult<Self> {
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
        input: Vec<u8>,
    ) -> Result<Vec<u8>, RuntimeError> {
        let id = self.next_id;
        self.next_id += 1;

        let request = ParentToChild::Call {
            id,
            function: function.spec().name.to_string(),
        };
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| RuntimeError::new("child runtime stdin is closed"))?;
        let request = <JsonCodec as WireCodec>::encode(&request)?;
        write_frame(stdin, &request)?;
        write_frame(stdin, &input)?;
        stdin.flush()?;

        let response = read_frame(&mut self.stdout)?
            .ok_or_else(|| RuntimeError::new("child runtime exited without response"))?;
        match <JsonCodec as WireCodec>::decode::<ChildToParent>(&response)? {
            ChildToParent::Ok { id: response_id } if response_id == id => {
                read_frame(&mut self.stdout)?
                    .ok_or_else(|| RuntimeError::new("child runtime exited without output frame"))
            }
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
            if let Ok(request) = <JsonCodec as WireCodec>::encode(&ParentToChild::Shutdown) {
                let _ = write_frame(&mut stdin, &request);
            }
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
    let mut stdin = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();

    while let Some(request) = read_frame(&mut stdin)? {
        match <JsonCodec as WireCodec>::decode::<ParentToChild>(&request)? {
            ParentToChild::Call { id, function } => {
                let input = read_frame(&mut stdin)?.ok_or_else(|| {
                    RuntimeError::new(format!("missing input frame for call id {id}"))
                })?;
                match registry.get(&function) {
                    Some(function) => match function.call_erased(&input) {
                        Ok(output) => {
                            let response =
                                <JsonCodec as WireCodec>::encode(&ChildToParent::Ok { id })?;
                            write_frame(&mut stdout, &response)?;
                            write_frame(&mut stdout, &output)?;
                        }
                        Err(error) => {
                            let response = <JsonCodec as WireCodec>::encode(&ChildToParent::Err {
                                id,
                                message: error.to_string(),
                            })?;
                            write_frame(&mut stdout, &response)?;
                        }
                    },
                    None => {
                        let response = <JsonCodec as WireCodec>::encode(&ChildToParent::Err {
                            id,
                            message: format!("unknown function '{function}'"),
                        })?;
                        write_frame(&mut stdout, &response)?;
                    }
                }
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
    Call { id: u64, function: String },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildToParent {
    Ok { id: u64 },
    Err { id: u64, message: String },
}

impl ChildToParent {
    fn id(&self) -> u64 {
        match self {
            Self::Ok { id } | Self::Err { id, .. } => *id,
        }
    }
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) -> RuntimeResult<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {} exceeds limit {}",
            payload.len(),
            MAX_FRAME_LEN
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| RuntimeError::new(format!("frame length {} exceeds u32", payload.len())))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> RuntimeResult<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    let mut read = 0;
    while read < header.len() {
        match reader.read(&mut header[read..])? {
            0 if read == 0 => return Ok(None),
            0 => {
                return Err(RuntimeError::new(
                    "unexpected EOF while reading frame header",
                ));
            }
            count => read += count,
        }
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {len} exceeds limit {MAX_FRAME_LEN}"
        )));
    }

    let mut payload = vec![0_u8; len];
    let mut read = 0;
    while read < payload.len() {
        match reader.read(&mut payload[read..])? {
            0 => return Err(RuntimeError::new("unexpected EOF while reading frame body")),
            count => read += count,
        }
    }

    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trips() {
        let mut buffer = Vec::new();

        write_frame(&mut buffer, b"hello").unwrap();

        let mut reader = Cursor::new(buffer);
        assert_eq!(read_frame(&mut reader).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn eof_before_frame_header_returns_none() {
        let mut reader = Cursor::new(Vec::new());

        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn partial_frame_header_is_rejected() {
        let mut reader = Cursor::new(vec![0, 0]);

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("frame header")
        );
    }

    #[test]
    fn partial_frame_body_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.extend_from_slice(b"he");
        let mut reader = Cursor::new(bytes);

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("frame body")
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut reader = Cursor::new(((MAX_FRAME_LEN + 1) as u32).to_be_bytes());

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("exceeds limit")
        );
    }
}
