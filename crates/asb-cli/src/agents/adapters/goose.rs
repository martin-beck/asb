use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{
    Error,
    install::{InstallResult, OperationContext, Recipe},
};
use std::path::{Path, PathBuf};

const X86_64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-x86_64-unknown-linux-gnu.tar.gz";
const X86_64_SHA256: &str = "ab396c95aebbd28b4d38c175234be8d4a3ea0c96e7ccb24c307e4618c9be57e1";
const AARCH64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-aarch64-unknown-linux-gnu.tar.gz";
const AARCH64_SHA256: &str = "0f174b5118a53888cd4e65b7ca9a4b72dd0b887e89142c8ce41e2409205e40d5";
const SOURCE_URL: &str =
    "https://codeload.github.com/aaif-goose/goose/tar.gz/cd643d2126fffab4c6a6186905211ba868d6a657";
const SOURCE_SHA256: &str = "34aac2c8c8efdd0e1423faecee5d905a0a32500e640c80d40c6a9666621f0ab1";

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
.with_native_tool_transport()
.with_guest_service(start_native_service)
.with_deferred_native_recording(finalize_native_recording)
.with_tape_wire_extension(include_str!(
    "../../../assets/scripts/goose-native-wire.mjs"
))
.with_collection(collect_native)
.with_step_count(step_count)
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "goose",
        filename: "native-tool-pending.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
])
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
    if std::env::var_os("ASB_GOOSE_NATIVE_BUILD").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return install_native(context, root, progress);
    }
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

fn install_native(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    use crate::install::{
        ProducerDirectory, Purpose, VerifiedSource, install_executable_source_in,
        install_verified_source_bounded_in, verify_sha256_preserving,
    };
    use std::{fs, process::Stdio};

    fs::create_dir_all(root.join("cache")).map_err(crate::io_error)?;
    let archive = root.join("cache/goose-native-source.tar.gz");
    if verify_sha256_preserving(&archive, SOURCE_SHA256, "Goose source").is_err() {
        progress(10, crate::install::ProgressStage::Downloading);
        install_verified_source_bounded_in(
            context,
            SOURCE_URL,
            &archive,
            VerifiedSource {
                kind: "pinned Goose source",
                checksum: SOURCE_SHA256,
                executable: false,
                limit: 512 * 1024 * 1024,
            },
            |_, _| Ok(()),
        )?;
    }
    let source = root.join("source-v1.54.0");
    let complete = source.join(".asb-source-extracted");
    if source.exists() && !complete.is_file() {
        let retained = root.join(format!(
            "source-incomplete-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::rename(&source, retained).map_err(crate::io_error)?;
    }
    let owner = ProducerDirectory::open(root)?;
    let mut scope = owner.scope(context);
    if !complete.is_file() {
        progress(25, crate::install::ProgressStage::Extracting);
        fs::create_dir_all(&source).map_err(crate::io_error)?;
        let mut step = scope.prepare("tar", Purpose::Extract, "Goose source extraction")?;
        step.command_mut()
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("--strip-components=1")
            .stdin(Stdio::null());
        if !scope.run(step)?.status.success() {
            return Err(
                Error::Io("Goose source extraction failed; install tar and retry".into()).into(),
            );
        }
        fs::write(&complete, b"1\n").map_err(crate::io_error)?;
    }
    let support = root.join("asb-native-build");
    fs::create_dir_all(&support).map_err(crate::io_error)?;
    for (name, content) in [
        (
            "goose-native-build.py",
            include_str!("../../../assets/scripts/goose-native-build.py"),
        ),
        (
            "goose-native-prepare.py",
            include_str!("../../../assets/scripts/goose-native-prepare.py"),
        ),
        (
            "goose-native-dispatch.patch",
            include_str!("../../../assets/scripts/goose-native-dispatch.patch"),
        ),
        (
            "goose-native-hook.rs",
            include_str!("../../../assets/scripts/goose-native-hook.rs"),
        ),
    ] {
        let path = support.join(name);
        if fs::read(&path).ok().as_deref() != Some(content.as_bytes()) {
            fs::write(path, content).map_err(crate::io_error)?;
        }
    }
    let output = root.join("bin/goose");
    let candidate = root.join("native-build-output/goose");
    progress(40, crate::install::ProgressStage::Building);
    let mut step = scope.prepare("python3", Purpose::Build, "Goose native compilation")?;
    step.command_mut()
        .arg(support.join("goose-native-build.py"))
        .arg(&source)
        .arg(root.join("build"))
        .arg(&candidate)
        .stdin(Stdio::null());
    if std::env::var_os("CARGO_HOME").is_none() {
        step.command_mut()
            .env("CARGO_HOME", root.join("cargo-cache"));
    }
    if !scope.run(step)?.status.success() {
        return Err(Error::Io("Goose native build failed; install Python, Git, Rust and Goose's build dependencies, then retry `asb tool install goose`".into()).into());
    }
    owner.consume(scope.finish()?)?;
    progress(90, crate::install::ProgressStage::Validating);
    let candidate = candidate.to_str().ok_or_else(|| {
        Error::Io("Goose build output path is not valid UTF-8; use a UTF-8 project path".into())
    })?;
    install_executable_source_in(context, candidate, &output, "Goose native build")?;
    progress(100, crate::install::ProgressStage::Validating);
    Ok(output)
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
    let mut args = vec!["run".into(), "--no-session".into()];
    if let Some(max_turns) = context.max_steps {
        args.extend(["--max-turns".into(), max_turns.to_string()]);
    }
    args.extend([
        "--output-format".into(),
        "json".into(),
        "-t".into(),
        context.prompt.into(),
    ]);
    CommandSpec {
        program: "goose".into(),
        args,
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    context.command.env_remove("ASB_GOOSE_NATIVE_CAPABILITY");
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
    if (tape.mode == crate::tape::Mode::Record
        && std::env::var_os("ASB_FS_MARKER_SOCKET").is_some())
        || (tape.mode == crate::tape::Mode::Replay
            && std::env::var_os("ASB_FS_REPLAY_SOCKET").is_some())
    {
        return Ok(());
    }
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

fn collect_native(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = AgentArtifacts::default();
    if let Some(bytes) = context
        .frozen
        .bytes("native-tool-pending.jsonl")
        .filter(|bytes| !bytes.is_empty())
    {
        artifacts
            .sealed_artifacts
            .push(super::super::SealedArtifact {
                kind: "native-tool-deferred".into(),
                bytes: bytes.to_vec(),
            });
    }
    artifacts
}

fn finalize_native_recording(pending: &[u8], snapshot_sha256: &str) -> Result<Vec<u8>, Error> {
    super::super::native_guest::finalize("goose", pending, snapshot_sha256)
}

fn start_native_service(
    command: &mut std::process::Command,
    executable: &str,
    run_dir: &Path,
    mode: Option<crate::tape::Mode>,
    native_tool: Option<&Path>,
    snapshot_sha256: Option<&str>,
) -> Result<Option<super::super::auxiliary::AuxiliaryPlan>, Error> {
    let Some((mode, marker, snapshot)) =
        super::super::native_guest::selection(mode, snapshot_sha256)?
    else {
        return Ok(None);
    };
    configured_native_service(
        command,
        executable,
        run_dir,
        mode,
        marker,
        native_tool,
        &snapshot,
    )
    .map(Some)
}

fn configured_native_service(
    command: &mut std::process::Command,
    executable: &str,
    run_dir: &Path,
    mode: crate::tape::Mode,
    marker: std::ffi::OsString,
    native_tool: Option<&Path>,
    snapshot: &std::ffi::OsStr,
) -> Result<super::super::auxiliary::AuxiliaryPlan, Error> {
    let executable = std::fs::canonicalize(executable)
        .map_err(|_| Error::Run("Goose executable is unavailable for native recording".into()))?;
    if !native_hook_available(&executable) {
        return Err(Error::Run("Goose native record/replay requires an ASB-hooked Goose build; the ordinary Goose installation does not provide it".into()));
    }
    if mode == crate::tape::Mode::Replay {
        let endpoint = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == "OPENAI_HOST" || key == "ANTHROPIC_HOST")
                    .then_some(value)
                    .flatten()
                    .and_then(std::ffi::OsStr::to_str)
            })
            .ok_or_else(|| {
                Error::Run("Goose native replay needs the local LLM replay proxy".into())
            })?;
        if !["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
            .iter()
            .any(|prefix| endpoint.starts_with(prefix))
        {
            return Err(Error::Run(
                "Goose native replay needs a local LLM replay proxy".into(),
            ));
        }
        command.env(
            "ASB_GOOSE_ID_MAP_URL",
            format!("{}/.asb/goose-session-map", endpoint.trim_end_matches('/')),
        );
    }
    let settings = super::super::native_guest::Settings {
        agent: "goose",
        bundle: &executable,
        run_dir,
        mode,
        marker: marker.clone(),
        native_tool,
        snapshot,
        assets: &[],
    };
    let (service, _, socket) = super::super::native_guest::broker(command, &settings)?;
    command
        .env("ASB_GOOSE_PROBE_SOCKET", socket)
        .env("ASB_GOOSE_PROBE_MARKER", marker)
        .env(
            "ASB_GOOSE_PROBE_MODE",
            if mode == crate::tape::Mode::Replay {
                "replay"
            } else {
                "record"
            },
        );
    Ok(super::super::auxiliary::AuxiliaryPlan::new(service))
}

/// Check the installed executable's compiled adapter interface before
/// allowing it to own native recordings. Ordinary Goose builds remain valid
/// for normal, non-native runs.
fn native_hook_available(executable: &Path) -> bool {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new(executable)
        .arg("--version")
        .env("ASB_GOOSE_NATIVE_CAPABILITY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code() == Some(73),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn shell_quote(value: &std::ffi::OsStr) -> Result<String, Error> {
    let value = value
        .to_str()
        .ok_or_else(|| Error::Run("ASB local-tool replay argument is not valid UTF-8".into()))?;
    Ok(format!("'{}'", value.replace('\'', "'\\\"'\\\"'")))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn native_guest_service_records_goose_dispatch_without_replacing_tools() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::{fs::PermissionsExt, net::UnixStream};
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("goose-guest-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let executable = root.join("goose-executable");
        std::fs::write(&executable, "#!/bin/sh\nexit 73\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new(&executable);
        let plan = super::configured_native_service(
            &mut command,
            executable.to_str().unwrap(),
            &root,
            crate::tape::Mode::Record,
            "/fixture/marker".into(),
            None,
            std::ffi::OsStr::new(""),
        )
        .unwrap();
        let mut service = plan.start().unwrap();
        let socket = root.join("goose/broker.sock");
        let request = |message: serde_json::Value| {
            let mut stream = UnixStream::connect(&socket).unwrap();
            writeln!(stream, "{message}").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        let started = request(
            serde_json::json!({"op":"start","request":{"extension":"developer","tool":"write","args":{"path":"file"}}}),
        );
        assert_eq!(started["ok"], true);
        let finished = request(
            serde_json::json!({"op":"finish","token":started["token"],"outcome":{"ok":true,"value":{"Ok":{"content":[]}}}}),
        );
        assert_eq!(finished["ok"], true);
        service.settle().unwrap();
        let pending = std::fs::read(root.join("goose/native-tool-pending.jsonl")).unwrap();
        assert!(
            !super::finalize_native_recording(&pending, &"a".repeat(64))
                .unwrap()
                .is_empty()
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "ASB_GOOSE_PROBE_SOCKET" && value.is_some())
        );
        assert!(command.get_args().next().is_none());
        drop(service);
        let mut replay = Command::new(&executable);
        replay.env("OPENAI_HOST", "http://127.0.0.1:1234");
        let _plan = super::configured_native_service(
            &mut replay,
            executable.to_str().unwrap(),
            &root.join("replay"),
            crate::tape::Mode::Replay,
            "/fixture/replay-marker".into(),
            Some(Path::new("/fixture/sidecar")),
            std::ffi::OsStr::new("sha256:fixture"),
        )
        .unwrap();
        assert!(
            replay
                .get_envs()
                .any(|(key, value)| key == "ASB_GOOSE_ID_MAP_URL"
                    && value
                        == Some(std::ffi::OsStr::new(
                            "http://127.0.0.1:1234/.asb/goose-session-map"
                        )))
        );
        let mut remote = Command::new(&executable);
        remote.env("OPENAI_HOST", "https://provider.example");
        assert!(
            super::configured_native_service(
                &mut remote,
                executable.to_str().unwrap(),
                &root.join("remote"),
                crate::tape::Mode::Replay,
                "/fixture/replay-marker".into(),
                Some(Path::new("/fixture/sidecar")),
                std::ffi::OsStr::new("sha256:fixture"),
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_hook_probe_distinguishes_upstream_and_hooked_builds() {
        use std::os::unix::fs::PermissionsExt;
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("goose-capability-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("goose");
        for (body, expected) in [
            ("#!/bin/sh\nprintf 'goose 1.54.0\\n'\n", false),
            (
                "#!/bin/sh\ntest \"$ASB_GOOSE_NATIVE_CAPABILITY\" = 1 || exit 1\nexit 73\n",
                true,
            ),
            (
                "#!/bin/sh\nprintf 'asb-goose-native-probe-v1\\n'\nexit 1\n",
                false,
            ),
        ] {
            std::fs::write(&executable, body).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(super::native_hook_available(&executable), expected);
        }
        assert!(!super::native_hook_available(&root.join("missing")));
        std::fs::remove_dir_all(root).unwrap();
    }

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
    fn default_launch_does_not_impose_turn_or_tool_limits() {
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
        assert!(!spec.args.iter().any(|arg| arg == "--max-turns"));
        assert!(!spec.args.iter().any(|arg| arg == "--system"));
    }

    #[test]
    fn requested_step_budget_is_forwarded() {
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
