use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{
    Error,
    install::{InstallResult, OperationContext, Recipe},
};
use std::path::{Path, PathBuf};

const X86_64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-x86_64-unknown-linux-gnu.tar.gz";
const X86_64_SHA256: &str = "ab396c95aebbd28b4d38c175234be8d4a3ea0c96e7ccb24c307e4618c9be57e1";
const AARCH64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-aarch64-unknown-linux-gnu.tar.gz";
const AARCH64_SHA256: &str = "0f174b5118a53888cd4e65b7ca9a4b72dd0b887e89142c8ce41e2409205e40d5";

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "goose",
    "goose",
    Recipe::Custom {
        executable: "goose",
        description: "download and verify the pinned Goose binary",
        summary: "pinned Goose binary (x86_64/aarch64) -> goose",
        install,
        guest_assets: Some(guest_assets),
    },
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(1024)
.with_tool_replay(prepare_tool_replay)
.with_step_count(step_count)
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "tool-replay",
    filename: "local-tools.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}])
.with_replay_credential_vars(&["ANTHROPIC_AUTH_TOKEN"])
.with_max_steps();

// Goose JSON mode emits a pretty-printed conversation summary after the run.
fn final_response(output: &str, _: &Path) -> Option<String> {
    // Pinned Goose prints its three-line session banner before JSON even in
    // non-interactive mode. Accept only that known framing, never scan for
    // summary-shaped objects inside arbitrary diagnostics.
    let json = if output.trim_start().starts_with('{') {
        output
    } else {
        let rest = output.strip_prefix('\n')?;
        let (first, rest) = rest.split_once('\n')?;
        let (second, rest) = rest.split_once('\n')?;
        let (third, json) = rest.split_once('\n')?;
        let heading = first.strip_prefix("    __( O)>  ● ")?;
        if !(heading.starts_with("new session · ")
            || heading.starts_with("resuming · ")
            || heading.starts_with("ephemeral · "))
            || !second.starts_with("   \\____)    ")
            || third != "     L L     goose is ready"
        {
            return None;
        }
        json
    };
    let summary: serde_json::Value = serde_json::from_str(json).ok()?;
    if summary.pointer("/metadata/status").and_then(|v| v.as_str()) != Some("completed") {
        return None;
    }
    let message = summary.get("messages")?.as_array()?.last()?;
    if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?.as_array()?;
    if content.iter().any(|part| {
        matches!(
            part.get("type").and_then(|v| v.as_str()),
            Some(
                "toolRequest"
                    | "toolResponse"
                    | "toolConfirmationRequest"
                    | "actionRequired"
                    | "error"
            ) | None
        )
    }) {
        return None;
    }
    let text = content
        .iter()
        .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// Goose's JSON mode emits accumulated usage in the top-level metadata of
/// its conversation summary. Message content is arbitrary task data.
fn output_usage(stdout: &str, _: &str, _: bool) -> crate::measurements::TokenUsage {
    for (index, _) in stdout.match_indices('{') {
        let Some(Ok(document)) = serde_json::Deserializer::from_str(&stdout[index..])
            .into_iter::<serde_json::Value>()
            .next()
        else {
            continue;
        };
        if document
            .get("messages")
            .and_then(|v| v.as_array())
            .is_some()
        {
            return document
                .get("metadata")
                .filter(|v| v.is_object())
                .map(super::super::usage_fields)
                .unwrap_or_default()
                .with_derived_total();
        }
    }
    Default::default()
}

/// Goose's JSON output is preceded by its terminal banner.  Count only the
/// assistant messages that carry provider inference metadata, rather than
/// banner text, user/tool responses, or locally synthesized messages.
fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let start = stdout.find('{')?;
    let value = serde_json::Deserializer::from_str(&stdout[start..])
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let steps = value
        .get("messages")?
        .as_array()?
        .iter()
        .filter(|message| {
            message.get("role").and_then(|value| value.as_str()) == Some("assistant")
                && message.pointer("/metadata/inference").is_some()
        })
        .count()
        .try_into()
        .ok()?;
    (steps > 0).then_some(steps)
}

fn distribution() -> Result<(&'static str, &'static str), Error> {
    Ok(match std::env::consts::ARCH {
        "x86_64" => (X86_64_URL, X86_64_SHA256),
        "aarch64" => (AARCH64_URL, AARCH64_SHA256),
        architecture => {
            return Err(Error::Input(format!(
                "Goose is packaged by ASB only for Linux x86_64 and aarch64; this host is {architecture}"
            )));
        }
    })
}

fn cached_archive(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<(PathBuf, &'static str)> {
    let (url, checksum) = distribution()?;
    let archive = root.join("cache/goose.tar.gz");
    let archive =
        crate::tools::runtime::cached_archive_in(context, &archive, url, checksum, progress)?;
    Ok((archive, checksum))
}

pub(crate) fn archive_spec() -> crate::install::archive::ArchiveSpec {
    crate::install::archive::ArchiveSpec {
        name: "Goose",
        executable: "goose",
        max_bytes: 256 * 1024 * 1024,
    }
}

fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    let (archive, checksum) = cached_archive(context, root, progress)?;
    crate::install::archive::install_archive_in(
        context,
        root,
        &archive,
        checksum,
        progress,
        &archive_spec(),
    )
}

fn guest_assets(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<Vec<crate::tools::GuestAsset>> {
    let (archive, _) = cached_archive(context, root, progress)?;
    Ok(vec![crate::tools::GuestAsset {
        source: archive,
        target: PathBuf::from("cache/goose.tar.gz"),
        distribution: None,
    }])
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    let max_turns = context.max_steps.unwrap_or(16);
    CommandSpec {
        program: "goose".into(),
        args: vec![
            "run".into(),
            "--no-session".into(),
            // Goose can otherwise dispatch sibling MCP calls concurrently.
            // The ASB local-tool sidecar is a causal transcript, so make the
            // adapter's tool boundary explicit: a result must be observed
            // before the client asks its next tool.  This is adapter launch
            // policy, not a generic replay ordering relaxation.
            "--system".into(),
            "Use exactly one tool call per assistant turn. Wait for that tool result before making another tool call.".into(),
            "--max-turns".into(),
            // A single turn cannot complete even the bundled fixture: Goose
            // needs one turn to inspect and another to edit/test. Keep the
            // bound finite while allowing a normal agent trajectory.
            max_turns.to_string(),
            "--output-format".into(),
            "json".into(),
            "-t".into(),
            context.prompt.into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    // Session naming is an asynchronous, auxiliary provider request.  It has
    // no bearing on a non-session benchmark result, yet it can race the first
    // tool-bearing request and make an otherwise identical recording unreplayable.
    context.command.env("GOOSE_DISABLE_SESSION_NAMING", "true");
    match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => {
            context
                .command
                .env("GOOSE_PROVIDER", "anthropic")
                .env("GOOSE_MODEL", context.model)
                .env(
                    "ANTHROPIC_HOST",
                    connection.base_url.trim_end_matches("/v1"),
                )
                .env("ANTHROPIC_API_KEY", key)
                .env("ANTHROPIC_AUTH_TOKEN", key);
        }
        crate::endpoint::Interface::OpenAiChat => {
            context
                .command
                .env("GOOSE_PROVIDER", "openai")
                .env("GOOSE_MODEL", context.model)
                .env("OPENAI_HOST", connection.base_url.trim_end_matches("/v1"))
                .env("OPENAI_BASE_PATH", "/v1/chat/completions")
                .env("OPENAI_API_KEY", key);
        }
        _ => return Err(context.unsupported_endpoint("goose")),
    }
    Ok(())
}

fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    // Goose may submit independent MCP observations from one assistant turn
    // in a different order on replay.  The sealed transcript still requires
    // an exact request/result match, and workspace effects remain barriers;
    // only an effect-free observation run may commute.
    let launch = super::super::tool_replay_mcp::allow_commuted_observations(
        super::super::tool_replay_mcp::launch(context, tape)?,
    );
    let extension = std::iter::once(launch.program.as_os_str())
        .chain(launch.args.iter().map(std::ffi::OsString::as_os_str))
        .map(shell_quote)
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    context
        .command
        // A profile may contribute arbitrary host extensions.  Replay uses
        // only the explicit ASB server so no external tool can bypass the
        // sealed transcript.
        .arg("--no-profile")
        .arg("--with-extension")
        .arg(extension);
    Ok(())
}

fn shell_quote(value: &std::ffi::OsStr) -> Result<String, Error> {
    let value = value
        .to_str()
        .ok_or_else(|| Error::Run("ASB local-tool replay argument is not valid UTF-8".into()))?;
    Ok(format!("'{}'", value.replace('\'', "'\\\"'\\\"'")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn completed_goose_summary_supplies_final_answer() {
        use crate::agents::AgentAdapter;
        let output = "{\n  \"messages\": [{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}],\n  \"metadata\": {\"status\":\"completed\"}\n}";
        let path = std::path::Path::new(".");
        assert_eq!(
            super::ADAPTER.final_response(output, path).as_deref(),
            Some("answer")
        );
        let failed = output.replace("completed", "error");
        assert!(super::ADAPTER.final_response(&failed, path).is_none());
        let user = output.replace("assistant", "user");
        assert!(super::ADAPTER.final_response(&user, path).is_none());
        let tool = r#"{"messages":[{"role":"assistant","content":[{"type":"text","text":"intermediate"},{"type":"toolRequest"}]}],"metadata":{"status":"completed"}}"#;
        assert!(super::ADAPTER.final_response(tool, path).is_none());
        for kind in [
            "actionRequired",
            "toolConfirmationRequest",
            "toolResponse",
            "error",
        ] {
            let pending = format!(
                r#"{{"messages":[{{"role":"assistant","content":[{{"type":"text","text":"intermediate"}},{{"type":"{kind}"}}]}}],"metadata":{{"status":"completed"}}}}"#
            );
            assert!(
                super::ADAPTER.final_response(&pending, path).is_none(),
                "{kind}"
            );
        }

        assert!(
            super::ADAPTER
                .final_response(&format!("{output}\n{output}"), path)
                .is_none()
        );
        assert!(
            super::ADAPTER
                .final_response(&format!("diagnostic\n{output}"), path)
                .is_none()
        );
        let banner = "\n    __( O)>  ● new session · openai mock\n   \\____)    session · /work\n     L L     goose is ready\n";
        assert_eq!(
            super::ADAPTER
                .final_response(&format!("{banner}{output}"), path)
                .as_deref(),
            Some("answer")
        );
        assert!(
            super::ADAPTER
                .final_response(&format!("diagnostic\n{banner}{output}"), path)
                .is_none()
        );
    }

    use super::*;
    use crate::agents::AgentAdapter;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    fn environment(command: &Command) -> HashMap<String, String> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn counts_only_assistant_messages_with_provider_inference() {
        let output = r#"banner
{"messages":[
 {"role":"user"},
 {"role":"assistant","metadata":{"inference":{"provider":"openai"}}},
 {"role":"assistant","metadata":{}},
 {"role":"assistant","metadata":{"inference":{"provider":"openai"}}}
]}"#;
        assert_eq!(step_count(output, ""), Some(2));
        assert_eq!(step_count("banner\n{\"messages\":[]}", ""), None);
    }

    #[test]
    fn task_content_is_not_usage_when_goose_summary_is_missing_usage() {
        let output = r#"banner
{"messages":[{"role":"user","content":[{"type":"text","text":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}]}],"metadata":{"status":"completed","total_tokens":null}}"#;
        let usage = crate::usage::from_sources("openrouter", "goose", output, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }

    #[test]
    fn openrouter_replay_uses_tape_and_dummy_credential() {
        let mut command = Command::new("goose");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "test-model",
            connection: &connection,
            workspace: Path::new("."),
            runtime_dir: Path::new("."),
            run_dir: Path::new("."),
            mcp_servers: &[],
            mcp_tape: None,
            tape: None,
        })
        .unwrap();
        let env = environment(&command);
        assert_eq!(env["GOOSE_PROVIDER"], "openai");
        assert_eq!(env["OPENAI_HOST"], "http://127.0.0.1:4321");
        assert_eq!(env["OPENAI_BASE_PATH"], "/v1/chat/completions");
        assert_eq!(env["OPENAI_API_KEY"], "asb-replay");
        assert_eq!(env["GOOSE_DISABLE_SESSION_NAMING"], "true");
    }

    #[test]
    fn cli2key_tape_endpoint_wins_over_bridge_endpoint() {
        for mode in [crate::tape::Mode::Record, crate::tape::Mode::Replay] {
            let mut command = Command::new("goose");
            let connection = crate::endpoint::negotiate(
                "fixture-agent",
                &[crate::endpoint::MESSAGES_KEY],
                "cli2key",
                "test-model",
                Some(("fixture-key", "http://127.0.0.1:9000/v1")),
                Some((mode, "http://127.0.0.1:4321/v1")),
            )
            .unwrap();
            prepare_adapter(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: Path::new("."),
                runtime_dir: Path::new("."),
                run_dir: Path::new("."),
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            })
            .unwrap();
            let env = environment(&command);
            assert_eq!(env["GOOSE_PROVIDER"], "anthropic");
            assert_eq!(env["ANTHROPIC_HOST"], "http://127.0.0.1:4321");
            assert_eq!(
                env["ANTHROPIC_API_KEY"],
                if mode == crate::tape::Mode::Replay {
                    "asb-replay"
                } else {
                    "fixture-key"
                }
            );
        }
    }

    #[test]
    fn tape_backed_preparation_uses_only_private_mcp_extension() {
        let mut command = Command::new("goose");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let root =
            std::env::temp_dir().join(format!("asb-goose-tool-replay-{}", std::process::id()));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let replay = root.join("replay.jsonl");
        let transcript = root.join("local-tools.jsonl");
        std::fs::write(&replay, "{}\n").unwrap();
        std::fs::write(&transcript, "{\"schema_version\":1,\"events\":[]}\n").unwrap();
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(crate::agents::TapeContext {
                    mode: crate::tape::Mode::Replay,
                    cassette: &replay,
                }),
            })
            .unwrap();
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|argument| argument == "--no-profile"));
        let extension = args
            .windows(2)
            .find(|pair| pair[0] == "--with-extension")
            .map(|pair| pair[1].as_str())
            .expect("private extension");
        assert!(extension.contains("__asb-tool-replay"));
        assert!(extension.contains("--workspace"));
        assert!(extension.contains("--allow-commuted-observations"));
        assert!(!environment(&command).contains_key("ASB_TOOL_REPLAY_MODE"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_turn_budget_allows_inspect_edit_and_test() {
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "goose".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            executable: "goose",
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiChat),
            prompt: "repair",
            max_steps: None,
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(
            spec.args
                .windows(2)
                .find(|pair| pair[0] == "--max-turns")
                .map(|pair| pair[1].as_str()),
            Some("16")
        );
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "--system" && pair[1].contains("exactly one tool call"))
        );
    }

    #[test]
    fn requested_step_budget_overrides_the_default_turn_limit() {
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "goose".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            executable: "goose",
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiChat),
            prompt: "repair",
            max_steps: Some(7),
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(
            spec.args
                .windows(2)
                .find(|pair| pair[0] == "--max-turns")
                .map(|pair| pair[1].as_str()),
            Some("7")
        );
    }
}
