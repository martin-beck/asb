//! Private stdio MCP endpoint for tape-backed local agent tools.
//!
//! The endpoint is launched only by adapter-generated, run-private
//! configuration. It records canonical calls in record mode and applies only
//! the pre-recorded effects in replay mode.

use super::{
    PreparationContext, TapeContext,
    tool_replay::{self, Effect, Event, SCHEMA_VERSION, Transcript},
};
use crate::Error;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub(crate) fn entry(args: &[OsString]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::from(2),
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Record,
    Replay,
}

struct Options {
    mode: Mode,
    workspace: PathBuf,
    transcript: PathBuf,
}

/// The adapter-facing, private stdio command for a single tape-backed run.
/// It is deliberately constructed here so every adapter gets the same sealed
/// transcript naming and replay fail-closed behavior.
pub(crate) struct Launch {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
}

pub(crate) fn launch(
    context: &PreparationContext<'_>,
    tape: TapeContext<'_>,
) -> Result<Launch, Error> {
    let transcript = match tape.mode {
        crate::tape::Mode::Record => context.run_dir.join("tool-replay/local-tools.jsonl"),
        crate::tape::Mode::Replay => replay_transcript(tape.cassette)?,
    };
    if tape.mode == crate::tape::Mode::Replay && !transcript.is_file() {
        return Err(Error::Run(
            "this replay requires its sealed local-tool transcript; record the benchmark again before replaying it".into(),
        ));
    }
    let program = std::env::current_exe().map_err(|error| {
        Error::Run(format!(
            "cannot locate the ASB local-tool replay executable: {error}"
        ))
    })?;
    Ok(Launch {
        program,
        args: vec![
            "__asb-tool-replay".into(),
            "--mode".into(),
            match tape.mode {
                crate::tape::Mode::Record => "record".into(),
                crate::tape::Mode::Replay => "replay".into(),
            },
            "--workspace".into(),
            context.workspace.as_os_str().to_owned(),
            "--transcript".into(),
            transcript.into_os_string(),
        ],
    })
}

fn replay_transcript(cassette: &Path) -> Result<PathBuf, Error> {
    let name = cassette
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Run("replay cassette has no valid private filename".into()))?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .ok_or_else(|| Error::Run("replay cassette has an unexpected private filename".into()))?;
    let parent = cassette
        .parent()
        .ok_or_else(|| Error::Run("replay cassette has no private directory".into()))?;
    Ok(parent.join(format!("local-tools{suffix}.jsonl")))
}

fn run(args: &[OsString]) -> Result<(), Error> {
    let options = options(args)?;
    let mut transcript = match options.mode {
        Mode::Record => Transcript::default(),
        Mode::Replay => Transcript::decode(
            &std::fs::read(&options.transcript)
                .map_err(|_| Error::Run("local-tool replay transcript is unavailable".into()))?,
        )?,
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = line.map_err(crate::io_error)?;
        if line.len() > 1024 * 1024 {
            return Err(Error::Run("local-tool MCP request exceeds bound".into()));
        }
        let request: Value = serde_json::from_str(&line)
            .map_err(|_| Error::Run("local-tool MCP request is malformed".into()))?;
        if let Some(response) = respond(&options, &mut transcript, &request)? {
            serde_json::to_writer(&mut output, &response)
                .map_err(|_| Error::Run("cannot encode local-tool MCP response".into()))?;
            output.write_all(b"\n").map_err(crate::io_error)?;
            output.flush().map_err(crate::io_error)?;
        }
    }
    match options.mode {
        Mode::Record => {
            let bytes = transcript.encode()?;
            let parent = options.transcript.parent().ok_or_else(|| {
                Error::Run("local-tool transcript has no private namespace".into())
            })?;
            if !parent.is_dir() {
                return Err(Error::Run(
                    "local-tool transcript namespace is unavailable".into(),
                ));
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&options.transcript)
                .map_err(|_| Error::Run("cannot publish local-tool transcript".into()))?;
            file.write_all(&bytes).map_err(crate::io_error)?;
            file.sync_all().map_err(crate::io_error)?;
        }
        Mode::Replay => transcript.finish()?,
    }
    Ok(())
}

fn options(args: &[OsString]) -> Result<Options, Error> {
    let mut mode = None;
    let mut workspace = None;
    let mut transcript = None;
    let mut values = args.iter();
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| Error::Input("invalid local-tool MCP arguments".into()))?;
        match flag.to_string_lossy().as_ref() {
            "--mode" => {
                mode = Some(match value.to_string_lossy().as_ref() {
                    "record" => Mode::Record,
                    "replay" => Mode::Replay,
                    _ => return Err(Error::Input("invalid local-tool replay mode".into())),
                })
            }
            "--workspace" => workspace = Some(PathBuf::from(value)),
            "--transcript" => transcript = Some(PathBuf::from(value)),
            _ => return Err(Error::Input("invalid local-tool MCP arguments".into())),
        }
    }
    Ok(Options {
        mode: mode.ok_or_else(|| Error::Input("local-tool MCP mode is required".into()))?,
        workspace: workspace
            .ok_or_else(|| Error::Input("local-tool MCP workspace is required".into()))?,
        transcript: transcript
            .ok_or_else(|| Error::Input("local-tool MCP transcript is required".into()))?,
    })
}

fn respond(
    options: &Options,
    transcript: &mut Transcript,
    request: &Value,
) -> Result<Option<Value>, Error> {
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Err(Error::Run("local-tool MCP request has no method".into()));
    };
    let Some(id) = request.get("id") else {
        return Ok(None);
    };
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"asb-local-tools","version":"1"}})
        }
        "tools/list" => json!({"tools":[
            {"name":"read_file","description":"Read a UTF-8 file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}},
            {"name":"write_file","description":"Create or replace a UTF-8 file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path","content"],"properties":{"path":{"type":"string"},"content":{"type":"string"}}}},
            {"name":"create_directory","description":"Create a directory inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}},
            {"name":"remove_file","description":"Remove a file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}}
        ]}),
        "tools/call" => call(
            options,
            transcript,
            request.get("params").cloned().unwrap_or(Value::Null),
        )?,
        _ => {
            return Ok(Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method unavailable"}}),
            ));
        }
    };
    Ok(Some(json!({"jsonrpc":"2.0","id":id,"result":result})))
}

fn call(options: &Options, transcript: &mut Transcript, params: Value) -> Result<Value, Error> {
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Run("local-tool MCP call has no name".into()))?;
    let request = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = if options.mode == Mode::Replay {
        transcript.replay(&options.workspace, tool, &request)?
    } else {
        let (result, effects) = execute(&options.workspace, tool, &request)?;
        let sequence = transcript.encode()?.lines().count() as u64 + 1;
        transcript.record(Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            tool: tool.into(),
            request: request.clone(),
            result: result.clone(),
            effects,
        })?;
        result
    };
    Ok(
        json!({"content":[{"type":"text","text":serde_json::to_string(&result).map_err(|_| Error::Run("cannot render local-tool result".into()))?}]}),
    )
}

fn execute(root: &Path, tool: &str, request: &Value) -> Result<(Value, Vec<Effect>), Error> {
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Run("local-tool request needs a path".into()))?;
    match tool {
        "read_file" => Ok((
            json!({"text":tool_replay::read_text(root, path)?}),
            Vec::new(),
        )),
        "write_file" => {
            let effects = vec![Effect::WriteText {
                path: path.into(),
                content: request
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Run("local-tool write needs content".into()))?
                    .into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        "create_directory" => {
            let effects = vec![Effect::CreateDirectory { path: path.into() }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        "remove_file" => {
            let effects = vec![Effect::RemoveFile { path: path.into() }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        _ => Err(Error::Run("local-tool is unsupported".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tools_apply_only_confined_declared_effects() {
        let root = std::env::temp_dir().join(format!("asb-mcp-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (_, effects) = execute(
            &root,
            "write_file",
            &json!({"path":"answer.txt","content":"recorded"}),
        )
        .unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("answer.txt")).unwrap(),
            "recorded"
        );
        assert!(
            execute(
                &root,
                "write_file",
                &json!({"path":"../escape","content":"no"})
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn options_refuse_unknown_or_partial_arguments() {
        assert!(options(&[OsString::from("--mode"), OsString::from("record")]).is_err());
        assert!(options(&[OsString::from("--unknown"), OsString::from("value")]).is_err());
    }

    #[test]
    fn replay_launch_refuses_an_unsealed_local_tool_track() {
        let root = std::env::temp_dir().join(format!("asb-mcp-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let replay = root.join("replay.jsonl");
        std::fs::write(&replay, "{}\n").unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("unused");
        let error = launch(
            &PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                tape: None,
            },
            TapeContext {
                mode: crate::tape::Mode::Replay,
                cassette: &replay,
            },
        )
        .err()
        .expect("unsealed cassette must not enable host tools")
        .to_string();
        assert!(error.contains("sealed local-tool transcript"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}
