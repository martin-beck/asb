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
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_COMMAND_ARGUMENTS: usize = 32;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 4096;
const MAX_COMMAND_OUTPUT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);

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
        crate::tape::Mode::Record => {
            let transcript = context.run_dir.join("tool-replay/local-tools.jsonl");
            let parent = transcript
                .parent()
                .expect("local-tool transcript has a parent");
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Run(format!(
                    "cannot create local-tool transcript directory: {error}"
                ))
            })?;
            // Some clients launch a configured MCP server lazily. Publish a
            // valid empty transcript before the client starts so an otherwise
            // tool-free recording has sealed replay evidence too.
            std::fs::write(&transcript, Transcript::default().encode()?).map_err(|error| {
                Error::Run(format!("cannot initialize local-tool transcript: {error}"))
            })?;
            transcript
        }
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
            publish_record_transcript(&options.transcript, &bytes)?;
        }
        Mode::Replay => transcript.finish()?,
    }
    Ok(())
}

fn publish_record_transcript(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Run("local-tool transcript has no private namespace".into()))?;
    if !parent.is_dir() {
        return Err(Error::Run(
            "local-tool transcript namespace is unavailable".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::Run("local-tool transcript was not initialized".into()))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Run(
            "local-tool transcript is not a regular file".into(),
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|_| Error::Run("cannot publish local-tool transcript".into()))?;
    file.write_all(bytes).map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)?;
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
            {"name":"list_directory","description":"List direct non-symlink entries inside the benchmark workspace.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}},
            {"name":"search_text","description":"Search bounded UTF-8 source text inside the benchmark workspace.","inputSchema":{"type":"object","required":["query"],"properties":{"path":{"type":"string"},"query":{"type":"string"}}}},
            {"name":"run_command","description":"Run an allowlisted read-only workspace command without a shell. Results are sealed for replay.","inputSchema":{"type":"object","required":["argv"],"properties":{"argv":{"type":"array","items":{"type":"string"}}}}},
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
    match tool {
        "list_directory" => {
            let path = request.get("path").and_then(Value::as_str).unwrap_or(".");
            let entries = tool_replay::list_directory(root, path)?
                .into_iter()
                .map(|(name, directory)| json!({"name":name,"directory":directory}))
                .collect::<Vec<_>>();
            Ok((json!({"entries":entries}), Vec::new()))
        }
        "search_text" => {
            let path = request.get("path").and_then(Value::as_str).unwrap_or(".");
            let query = request
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Run("local-tool search needs a query".into()))?;
            let matches = tool_replay::search_text(root, path, query)?
                .into_iter()
                .map(|(path, line, text)| json!({"path":path,"line":line,"text":text}))
                .collect::<Vec<_>>();
            Ok((json!({"matches":matches}), Vec::new()))
        }
        "run_command" => Ok((run_command(root, request)?, Vec::new())),
        "read_file" => Ok((
            json!({"text":tool_replay::read_text(root, required_path(request)?)?}),
            Vec::new(),
        )),
        "write_file" => {
            let effects = vec![Effect::WriteText {
                path: required_path(request)?.into(),
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
            let effects = vec![Effect::CreateDirectory {
                path: required_path(request)?.into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        "remove_file" => {
            let effects = vec![Effect::RemoveFile {
                path: required_path(request)?.into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        _ => Err(Error::Run("local-tool is unsupported".into())),
    }
}

fn run_command(root: &Path, request: &Value) -> Result<Value, Error> {
    let argv = request
        .get("argv")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Run("local-tool command needs argv".into()))?;
    if argv.is_empty() || argv.len() > MAX_COMMAND_ARGUMENTS {
        return Err(Error::Run(
            "local-tool command argument count is invalid".into(),
        ));
    }
    let argv = argv
        .iter()
        .map(|value| {
            value.as_str().filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_COMMAND_ARGUMENT_BYTES
                    && !value.as_bytes().contains(&0)
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::Run("local-tool command arguments are invalid".into()))?;
    validate_command(&argv)?;
    let workspace = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let mut command = Command::new(argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .env("CARGO_NET_OFFLINE", "true");
    let mut child = command
        .spawn()
        .map_err(|_| Error::Run("cannot start local-tool command".into()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read = |mut reader: std::process::ChildStdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take((MAX_COMMAND_OUTPUT + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })
    };
    let stdout_reader = read(stdout);
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        BufReader::new(stderr)
            .by_ref()
            .take((MAX_COMMAND_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Run(
                    "local-tool command exceeded its time bound".into(),
                ));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| Error::Run("cannot capture local-tool command output".into()))?
        .map_err(crate::io_error)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| Error::Run("cannot capture local-tool command output".into()))?
        .map_err(crate::io_error)?;
    if stdout.len() > MAX_COMMAND_OUTPUT || stderr.len() > MAX_COMMAND_OUTPUT {
        return Err(Error::Run("local-tool command output exceeds bound".into()));
    }
    Ok(json!({
        "exit_code": status.code(),
        "success": status.success(),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    }))
}

fn validate_command(argv: &[&str]) -> Result<(), Error> {
    if argv.iter().skip(1).any(|argument| {
        Path::new(argument).is_absolute()
            || Path::new(argument)
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
    }) {
        return Err(Error::Run(
            "local-tool command argument escapes the workspace".into(),
        ));
    }
    let first = argv.get(1).copied().unwrap_or_default();
    let allowed = match argv[0] {
        "git" => matches!(
            first,
            "status" | "diff" | "show" | "log" | "grep" | "ls-files" | "rev-parse"
        ),
        "rg" | "grep" | "find" | "ls" | "cat" | "sed" | "head" | "tail" | "pwd" => true,
        "cargo" => matches!(first, "test" | "check" | "fmt"),
        "pytest" => true,
        "python" | "python3" => argv.get(1) == Some(&"-m") && argv.get(2) == Some(&"pytest"),
        _ => false,
    };
    allowed
        .then_some(())
        .ok_or_else(|| Error::Run("local-tool command is not allowlisted".into()))
}

fn required_path(request: &Value) -> Result<&str, Error> {
    request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Run("local-tool request needs a path".into()))
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
    fn directory_listing_is_recorded_as_an_observation() {
        let root = std::env::temp_dir().join(format!("asb-mcp-directory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("answer.txt"), "answer").unwrap();
        let (result, effects) = execute(&root, "list_directory", &json!({})).unwrap();
        assert_eq!(effects, Vec::<Effect>::new());
        assert_eq!(result["entries"][0]["name"], "answer.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn text_search_is_recorded_as_an_observation() {
        let root = std::env::temp_dir().join(format!("asb-mcp-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("answer.txt"), "answer\n").unwrap();
        let (result, effects) = execute(&root, "search_text", &json!({"query":"answer"})).unwrap();
        assert_eq!(effects, Vec::<Effect>::new());
        assert_eq!(result["matches"][0]["path"], "answer.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_commands_are_bounded_and_reject_escape_or_mutation() {
        let root = std::env::temp_dir().join(format!("asb-mcp-command-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let result = run_command(&root, &json!({"argv":["pwd"]})).unwrap();
        assert!(result["success"].as_bool().unwrap());
        assert!(
            result["stdout"]
                .as_str()
                .unwrap()
                .contains(root.to_str().unwrap())
        );
        assert!(run_command(&root, &json!({"argv":["cat","/etc/passwd"]})).is_err());
        assert!(run_command(&root, &json!({"argv":["git","commit"]})).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_returns_a_recorded_command_result_without_starting_a_process() {
        let root =
            std::env::temp_dir().join(format!("asb-mcp-command-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let request = json!({"name":"run_command","arguments":{"argv":["pwd"]}});
        let record = Options {
            mode: Mode::Record,
            workspace: root.clone(),
            transcript: root.join("record.jsonl"),
        };
        let mut transcript = Transcript::default();
        let recorded = call(&record, &mut transcript, request.clone()).unwrap();
        let mut replay = Transcript::decode(&transcript.encode().unwrap()).unwrap();
        let replay_options = Options {
            mode: Mode::Replay,
            workspace: root.join("removed-workspace"),
            transcript: root.join("replay.jsonl"),
        };
        let replayed = call(&replay_options, &mut replay, request).unwrap();
        assert_eq!(recorded, replayed);
        replay.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_launch_publishes_a_valid_empty_local_tool_transcript() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-empty-transcript-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let cassette = root.join("recording.jsonl");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("unused");
        let launch = launch(
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
                mode: crate::tape::Mode::Record,
                cassette: &cassette,
            },
        )
        .unwrap();
        let transcript = root.join("tool-replay/local-tools.jsonl");
        assert!(transcript.is_file());
        assert!(std::fs::read(&transcript).unwrap().is_empty());
        Transcript::decode(&std::fs::read(&transcript).unwrap()).unwrap();
        assert!(launch.args.iter().any(|argument| argument == "record"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_publication_replaces_the_initialized_local_tool_transcript() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-record-publication-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let transcript = root.join("local-tools.jsonl");
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let mut expected_transcript = Transcript::default();
        expected_transcript
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 1,
                tool: "read_file".into(),
                request: json!({"name":"read_file","arguments":{"path":"answer.txt"}}),
                result: json!({"content":"recorded"}),
                effects: Vec::new(),
            })
            .unwrap();
        let expected = expected_transcript.encode().unwrap();
        publish_record_transcript(&transcript, &expected).unwrap();
        assert_eq!(std::fs::read(&transcript).unwrap(), expected);
        let link = root.join("link.jsonl");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&transcript, &link).unwrap();
        #[cfg(unix)]
        assert!(publish_record_transcript(&link, b"unexpected").is_err());
        let _ = std::fs::remove_dir_all(root);
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
