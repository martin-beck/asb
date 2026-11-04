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

pub(super) fn start(
    command: &mut Command,
    settings: Settings<'_>,
) -> Result<super::auxiliary::AuxiliaryPlan, Error> {
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
    let socket = directory.join("broker.sock");
    let mut service = Command::new("node");
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
