use super::super::{ArtifactDisplay, CollectorAdapter};
pub(super) struct FlamegraphCollector;
pub(super) static ADAPTER: FlamegraphCollector = FlamegraphCollector;

impl CollectorAdapter for FlamegraphCollector {
    fn id(&self) -> &'static str {
        "flamegraph"
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("flamegraph")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &["perf"]
    }
    fn preflight(&self, context: &mut super::super::PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn prioritizes_artifact(&self, name: &str) -> bool {
        name == "flamegraph.svg" || name == "flamegraph.json"
    }
    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        match name {
            "flamegraph.svg" => Some(ArtifactDisplay::Svg {
                class: "flamegraph",
                label: "Flamegraph SVG",
            }),
            "flamegraph.json" => Some(ArtifactDisplay::Status {
                class: "flamegraph-status",
                label: "Flamegraph collector status",
                suffix: "An SVG is embedded here when collection completed.",
            }),
            _ => None,
        }
    }
    fn finish(&self, session: &mut crate::measurements::Session) -> Result<(), String> {
        session.write_flamegraph()
    }
}

use crate::measurements::{self, write_json};
use serde::Serialize;
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// Large concurrent agent traces can take several minutes to decode. Keep the
// post-processing bounded, but do not discard valid samples merely because
// `perf script` needs longer than the old one-minute limit.
const FLAMEGRAPH_TIMEOUT: Duration = Duration::from_secs(300);

fn flamegraph_commands(
    tool_dir: Option<&Path>,
) -> (Option<std::ffi::OsString>, Option<std::ffi::OsString>) {
    let collapse = std::env::var_os("ASB_FLAMEGRAPH_STACKCOLLAPSE").or_else(|| {
        tool_dir.and_then(|dir| {
            [
                dir.join("flamegraph/bin/stackcollapse-perf.pl"),
                dir.join("bin/stackcollapse-perf.pl"),
            ]
            .into_iter()
            .find(|path| path.is_file())
            .map(|path| path.into_os_string())
        })
    });
    let render = std::env::var_os("ASB_FLAMEGRAPH_RENDER").or_else(|| {
        tool_dir.and_then(|dir| {
            [
                dir.join("flamegraph/bin/flamegraph"),
                dir.join("bin/flamegraph"),
            ]
            .into_iter()
            .find(|path| path.is_file())
            .map(|path| path.into_os_string())
        })
    });
    (collapse, render)
}

fn preflight(context: &mut super::super::PreflightContext<'_>) -> Result<(), String> {
    if !context.selected.iter().any(|item| item == "flamegraph") {
        return Ok(());
    }
    let (Some(collapse), Some(render)) = flamegraph_commands(context.tool_dir) else {
        return Err("flamegraph scripts are required for the selected collector. Install them with `asb tool install flamegraph` and retry.".into());
    };
    for executable in [collapse, render] {
        let mut child = Command::new(&executable).arg("--help").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
            .map_err(|error| format!("cannot execute flamegraph tool {}: {error}. Check directory access and reinstall with `asb tool install flamegraph`.", Path::new(&executable).display()))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break, // Some script versions return nonzero for --help.
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "flamegraph tool {} did not finish its preflight; check `asb tool install flamegraph`.",
                        Path::new(&executable).display()
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("cannot check flamegraph tool: {error}"));
                }
            }
        }
    }
    Ok(())
}

impl measurements::Session {
    pub(crate) fn write_flamegraph(&self) -> Result<(), String> {
        let Some(data) = self.artifact(super::super::SharedArtifact::StackSamples) else {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf recording did not start; check perf.json for the host error",
            );
        };
        if !data.is_file() || data.metadata().map_or(true, |metadata| metadata.len() == 0) {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf produced no samples; check perf.json and host perf permissions",
            );
        }
        let (collapse, render) = flamegraph_commands(self.tool_dir.as_deref());
        let (Some(collapse), Some(render)) = (collapse, render) else {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "install stackcollapse-perf.pl and flamegraph in the project tool directory",
            );
        };
        // `perf record` may require sudo, but the resulting perf.data is
        // pre-created and owned by the benchmark user.  Run post-processing
        // without sudo so perf's ownership check accepts that file and the
        // flamegraph never needs elevated privileges.
        let mut script = Command::new(measurements::resolve_executable(
            self.tool_dir.as_deref(),
            "perf",
        ))
        .args(["script", "-i"])
        .arg(data)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start perf script for flamegraph: {e}"))?;
        let script_stdout = script
            .stdout
            .take()
            .ok_or_else(|| "perf script stdout was not piped".to_owned())?;
        let folded_path = self.dir.join("flamegraph.folded");
        let mut collapsed = Command::new(collapse)
            .stdin(Stdio::from(script_stdout))
            .stdout(Stdio::from(File::create(&folded_path).map_err(|e| {
                format!("cannot create flamegraph folded output: {e}")
            })?))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start flamegraph stack collapse script: {e}"))?;
        if !wait_bounded(&mut script, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf script timed out after 300 seconds or exited unsuccessfully; inspect perf.stderr",
            );
        }
        if !wait_bounded(&mut collapsed, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "the stack collapse script timed out after 300 seconds or exited unsuccessfully; inspect flamegraph.folded",
            );
        }
        let mut render = Command::new(render);
        render
            .stdin(Stdio::from(File::open(&folded_path).map_err(|e| {
                format!("cannot open collapsed flamegraph output: {e}")
            })?))
            .stdout(Stdio::from(
                File::create(self.dir.join("flamegraph.svg"))
                    .map_err(|e| format!("cannot create flamegraph.svg: {e}"))?,
            ));
        let mut render = render
            .spawn()
            .map_err(|e| format!("cannot start flamegraph renderer: {e}"))?;
        if !wait_bounded(&mut render, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "the flamegraph renderer timed out after 300 seconds or exited unsuccessfully",
            );
        }
        write_flamegraph_status(&self.dir, "completed", Some("flamegraph.svg"), "")
    }
}

#[derive(Debug, Serialize)]
struct FlamegraphStatus<'a> {
    schema_version: u32,
    status: &'a str,
    artifact: Option<&'a str>,
    reason: Option<&'a str>,
}

pub(crate) fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .map_err(|error| format!("cannot poll collector process: {error}"))?
        {
            Some(status) => return Ok(status.success()),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let pid = child.id().to_string();
                let _ = Command::new("pkill").args(["-KILL", "-P", &pid]).status();
                let _ = child.wait();
                return Ok(false);
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

pub(crate) fn write_flamegraph_status(
    dir: &Path,
    status: &'static str,
    artifact: Option<&'static str>,
    reason: &'static str,
) -> Result<(), String> {
    write_json(
        &dir.join("flamegraph.json"),
        &FlamegraphStatus {
            schema_version: 1,
            status,
            artifact,
            reason: (!reason.is_empty()).then_some(reason),
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::measurements::resolve_executable;

    #[test]
    fn flamegraph_postprocessing_resolves_project_perf_without_privilege_wrapper() {
        let tools =
            std::env::temp_dir().join(format!("asb-flamegraph-test-{}", std::process::id()));
        std::fs::create_dir_all(tools.join("perf/bin")).expect("create tool directory");
        std::fs::write(tools.join("perf/bin/perf"), b"fixture").expect("write tool fixture");
        assert_eq!(
            resolve_executable(Some(&tools), "perf"),
            tools.join("perf/bin/perf")
        );
        let _ = std::fs::remove_dir_all(tools);
    }
}
