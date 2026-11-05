//! Shared guest service plumbing; adapters own bundle and dispatch semantics.
use crate::Error;
use std::path::Path;
use std::process::Command;

pub(super) struct Settings<'a> {
    pub agent: &'static str,
    pub bundle: &'a Path,
    pub run_dir: &'a Path,
    pub mode: crate::tape::Mode,
    pub marker: std::ffi::OsString,
    pub native_tool: Option<&'a Path>,
    /// A controller-verified SHA-256 reference or a local fixture archive path.
    pub snapshot: &'a std::ffi::OsStr,
    pub assets: &'a [(&'static str, &'static str)],
}

pub(super) fn selection(
    mode: Option<crate::tape::Mode>,
    snapshot_sha256: Option<&str>,
) -> Result<Option<(crate::tape::Mode, std::ffi::OsString, std::ffi::OsString)>, Error> {
    let Some(mode) = mode else { return Ok(None) };
    let variable = if mode == crate::tape::Mode::Record {
        "ASB_FS_MARKER_SOCKET"
    } else {
        "ASB_FS_REPLAY_SOCKET"
    };
    let Some(marker) = std::env::var_os(variable) else {
        return Ok(None);
    };
    if marker.is_empty() {
        return Err(Error::Run("native snapshot marker path is empty".into()));
    }
    let snapshot = if mode == crate::tape::Mode::Replay {
        let digest = snapshot_sha256.ok_or_else(|| {
            Error::Run("native replay requires the staged snapshot digest".into())
        })?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Run(
                "native replay snapshot digest is invalid".into(),
            ));
        }
        format!("sha256:{digest}").into()
    } else {
        std::ffi::OsString::new()
    };
    Ok(Some((mode, marker, snapshot)))
}

pub(super) fn broker(
    command: &Command,
    settings: &Settings<'_>,
) -> Result<(Command, std::path::PathBuf, std::path::PathBuf), Error> {
    if settings.marker.is_empty() {
        return Err(Error::Run("native snapshot marker path is empty".into()));
    }
    let directory = settings.run_dir.join(settings.agent);
    std::fs::create_dir_all(&directory).map_err(crate::io_error)?;
    for (name, source) in [
        (
            "native-tool-service.mjs",
            include_str!("../../assets/scripts/native-tool-service.mjs"),
        ),
        (
            "native-tool-broker.mjs",
            include_str!("../../assets/scripts/native-tool-broker.mjs"),
        ),
        (
            "native-tool-custody.mjs",
            include_str!("../../assets/scripts/native-tool-custody.mjs"),
        ),
        (
            "native-tool-sidecar.mjs",
            include_str!("../../assets/scripts/native-tool-sidecar.mjs"),
        ),
        (
            "native-tool-session.mjs",
            include_str!("../../assets/scripts/native-tool-session.mjs"),
        ),
        (
            "native-tool-dispatch.mjs",
            include_str!("../../assets/scripts/native-tool-dispatch.mjs"),
        ),
        (
            "native-snapshot-boundary.mjs",
            include_str!("../../assets/scripts/native-snapshot-boundary.mjs"),
        ),
    ]
    .iter()
    .chain(settings.assets.iter())
    {
        std::fs::write(directory.join(name), source).map_err(crate::io_error)?;
    }
    let socket = if settings.agent == "claude-code" {
        let hook_dir = command
            .get_envs()
            .find_map(|(name, value)| {
                (name == "ASB_CLAUDE_NATIVE_DIRECTORY")
                    .then_some(value)
                    .flatten()
            })
            .ok_or_else(|| Error::Run("Claude native hook directory is missing".into()))?;
        let hook_dir = Path::new(hook_dir);
        std::fs::create_dir_all(hook_dir).map_err(crate::io_error)?;
        let parent = hook_dir
            .parent()
            .ok_or_else(|| Error::Run("Claude native hook runtime is missing".into()))?;
        for name in [
            "claude-native-hook.mjs",
            "claude-native-launcher.mjs",
            "native-tool-broker.mjs",
            "native-tool-session.mjs",
            "native-snapshot-boundary.mjs",
        ] {
            std::fs::copy(directory.join(name), parent.join(name)).map_err(crate::io_error)?;
        }
        parent.join("broker.sock")
    } else {
        directory.join("broker.sock")
    };
    let mut service = Command::new("node");
    service
        .env_remove("ASB_NATIVE_SETTLEMENT_MODULE")
        .env_remove("ASB_NATIVE_SETTLEMENT_DIRECTORY")
        .env_remove("ASB_NATIVE_SETTLEMENT_MARKER")
        .env_remove("ASB_CLAUDE_WEB_PLAN")
        .env_remove("ASB_CLAUDE_WEB_EVIDENCE")
        .env_remove("ASB_CLAUDE_WEB_READY")
        .env_remove("ASB_CLAUDE_WEB_RUNTIME");
    if settings.agent == "claude-code" {
        let state = command
            .get_envs()
            .find_map(|(name, value)| {
                (name == "ASB_CLAUDE_NATIVE_DIRECTORY")
                    .then_some(value)
                    .flatten()
            })
            .ok_or_else(|| Error::Run("Claude native hook directory is missing".into()))?;
        service.env("ASB_NATIVE_HOOK_STATE", state);
    }
    if let Some((_, Some(path))) = command.get_envs().find(|(name, _)| *name == "PATH") {
        service.env("PATH", path);
    }
    service
        .env("ASB_NATIVE_TOOL_AGENT", settings.agent)
        .arg(directory.join("native-tool-service.mjs"))
        .arg(&socket)
        .arg(settings.bundle);
    if settings.mode == crate::tape::Mode::Replay {
        service
            .arg(settings.native_tool.ok_or_else(|| {
                Error::Run(format!(
                    "{} snapshot replay requires a native tool sidecar",
                    settings.agent
                ))
            })?)
            .arg(settings.snapshot)
            .arg(&settings.marker);
    } else {
        service.arg(directory.join("native-tool-pending.jsonl"));
    }
    Ok((service, directory, socket))
}

pub(super) fn start(
    command: &mut Command,
    settings: Settings<'_>,
) -> Result<super::auxiliary::AuxiliaryPlan, Error> {
    let (service, directory, socket) = broker(command, &settings)?;
    let bootstrap = directory.join(format!("{}-native-bootstrap.mjs", settings.agent));
    let bootstrap = bootstrap
        .to_str()
        .ok_or_else(|| Error::Run("native bootstrap path is not UTF-8".into()))?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let mut options = command
        .get_envs()
        .find(|(name, _)| *name == "NODE_OPTIONS")
        .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os("NODE_OPTIONS"))
        .unwrap_or_default();
    if !options.is_empty() {
        options.push(" ");
    }
    options.push(format!("--import=\"{bootstrap}\""));
    let prefix = if settings.agent == "continue" {
        "ASB_CONTINUE"
    } else {
        "ASB_NATIVE"
    };
    command
        .env("NODE_OPTIONS", options)
        .env(format!("{prefix}_BUNDLE"), settings.bundle)
        .env(
            format!("{prefix}_MODE"),
            if settings.mode == crate::tape::Mode::Replay {
                "replay"
            } else {
                "record"
            },
        )
        .env(format!("{prefix}_SOCKET"), socket)
        .env(format!("{prefix}_MARKER"), settings.marker);
    Ok(super::auxiliary::AuxiliaryPlan::new(service))
}

pub(super) fn finalize(
    agent: &str,
    pending: &[u8],
    workspace_sha256: &str,
) -> Result<Vec<u8>, Error> {
    if workspace_sha256.len() != 64
        || !workspace_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || pending.is_empty()
        || pending.len() > 16 * 1024 * 1024 - 120
        || pending.last() != Some(&b'\n')
    {
        return Err(Error::Run(format!(
            "{agent} native recording or settled snapshot digest is invalid"
        )));
    }
    let mut completed = pending.to_vec();
    completed.extend_from_slice(
        format!("{{\"event\":\"workspace_effect\",\"sha256\":\"{workspace_sha256}\"}}\n")
            .as_bytes(),
    );
    let native = crate::cassette::NativeTrack {
        complete: true,
        events: completed.iter().filter(|byte| **byte == b'\n').count() as u64,
        bytes: completed.len() as u64,
        sha256: crate::cassette::digest(&completed),
    };
    let transport = crate::cassette::NativeToolTransport::from_sidecar(&native, &completed)
        .map_err(|_| Error::Run(format!("{agent} native recording is invalid or incomplete")))?;
    if transport.launch_session() != format!("{agent}-launch-1") {
        return Err(Error::Run(format!(
            "{agent} native recording belongs to another adapter"
        )));
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_prepares_native_client_without_node_bootstrap() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let directory = base.join(format!("native-broker-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let mut client = Command::new("goose");
        client.env("PATH", "/fixture/bin");
        let settings = Settings {
            agent: "goose",
            bundle: Path::new("/fixture/goose"),
            run_dir: &directory,
            mode: crate::tape::Mode::Replay,
            marker: "/fixture/marker".into(),
            native_tool: Some(Path::new("/fixture/sidecar")),
            snapshot: std::ffi::OsStr::new("sha256:fixture"),
            assets: &[],
        };
        let (service, service_dir, socket) = broker(&client, &settings).unwrap();
        let args = service
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args[0],
            service_dir
                .join("native-tool-service.mjs")
                .to_string_lossy()
        );
        assert_eq!(args[1], socket.to_string_lossy());
        assert_eq!(
            &args[2..],
            [
                "/fixture/goose",
                "/fixture/sidecar",
                "sha256:fixture",
                "/fixture/marker"
            ]
        );
        assert!(client.get_envs().all(|(name, _)| name != "NODE_OPTIONS"));
        assert!(service_dir.join("native-tool-broker.mjs").is_file());
        let missing = Settings {
            native_tool: None,
            ..settings
        };
        assert!(broker(&client, &missing).is_err());
        let empty_marker = Settings {
            marker: std::ffi::OsString::new(),
            native_tool: Some(Path::new("/fixture/sidecar")),
            ..missing
        };
        assert!(
            broker(&client, &empty_marker)
                .unwrap_err()
                .to_string()
                .contains("native snapshot marker path is empty")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
