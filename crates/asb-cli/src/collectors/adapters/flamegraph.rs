use super::super::{ArtifactDisplay, CollectorAdapter};
pub(super) struct FlamegraphCollector;
pub(super) static ADAPTER: FlamegraphCollector = FlamegraphCollector;

#[cfg(test)]
#[path = "../../../tests/unit/flamegraph.rs"]
mod coverage_tests;

#[cfg(test)]
#[path = "../../../tests/unit/flamegraph_cleanup.rs"]
mod cleanup_tests;

impl CollectorAdapter for FlamegraphCollector {
    fn estimated_disk_mib(&self) -> u64 {
        1024
    }
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
use crate::process_cleanup::OwnedReaper;
use serde::Serialize;
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// Large concurrent agent traces can take several minutes to decode. Keep the
// post-processing bounded, but do not discard valid samples merely because
// `perf script` needs longer than the old one-minute limit.
const FLAMEGRAPH_TIMEOUT: Duration = Duration::from_secs(300);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Direct children only. Dropping a `Child` alone neither stops nor reaps it.
struct PipelineChildren {
    children: Vec<(&'static str, Child)>,
    reaper: OwnedReaper,
}

impl PipelineChildren {
    fn new() -> Result<Self, String> {
        Ok(Self {
            children: Vec::new(),
            reaper: OwnedReaper::new("flamegraph")?,
        })
    }

    fn add(&mut self, name: &'static str, child: Child) -> usize {
        self.children.push((name, child));
        #[cfg(all(test, target_os = "linux"))]
        cleanup_tests::record_admission(name, self.children.last().expect("admitted child").1.id());
        self.children.len() - 1
    }

    fn child(&mut self, index: usize) -> &mut Child {
        &mut self.children[index].1
    }

    fn cleanup(&mut self) -> Result<(), String> {
        // Signal all still-running stages first: a downstream reader must not
        // retain a failed upstream stage, or vice versa, while we wait.
        for (_, child) in &mut self.children {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
        }
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut errors = Vec::new();
        for (name, mut child) in self.children.drain(..) {
            #[cfg(all(test, target_os = "linux"))]
            let result = cleanup_tests::cleanup_poll_override(name)
                .unwrap_or_else(|| poll_until(&mut child, deadline));
            #[cfg(not(all(test, target_os = "linux")))]
            let result = poll_until(&mut child, deadline);
            match result {
                Ok(Some(_)) => {}
                result => {
                    let reason = match result {
                        Err(error) => error,
                        _ => "child did not exit within five seconds".into(),
                    };
                    errors.push(format!("{name} cleanup was not confirmed: {reason}; an owned reaper retains the child"));
                    // A task stuck in the kernel must not make report export
                    // unbounded. Retain its Child rather than abandoning it;
                    // this rare fallback is never a successful cleanup claim.
                    self.reaper.adopt(child);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for PipelineChildren {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn poll_until(
    child: &mut Child,
    deadline: Instant,
) -> Result<Option<std::process::ExitStatus>, String> {
    loop {
        match child
            .try_wait()
            .map_err(|error| format!("cannot poll collector process: {error}"))?
        {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() >= deadline => return Ok(None),
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

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
        let mut children = PipelineChildren::new()?;
        let child = Command::new(&executable).arg("--help").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
            .map_err(|error| format!("cannot execute flamegraph tool {}: {error}. Check directory access and reinstall with `asb tool install flamegraph`.", Path::new(&executable).display()))?;
        let child = children.add("flamegraph preflight", child);
        let result = match poll_until(
            children.child(child),
            Instant::now() + Duration::from_secs(5),
        ) {
            Ok(Some(_)) => Ok(()), // Some script versions return nonzero for --help.
            Ok(None) => Err(format!(
                "flamegraph tool {} did not finish its preflight; check `asb tool install flamegraph`.",
                Path::new(&executable).display()
            )),
            Err(error) => Err(format!("cannot check flamegraph tool: {error}")),
        };
        match (result, children.cleanup()) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Err(error), Err(cleanup)) => return Err(format!("{error}; {cleanup}")),
        }
    }
    Ok(())
}

impl measurements::Session {
    pub(crate) fn write_flamegraph(&self) -> Result<(), String> {
        self.write_flamegraph_with_timeout(FLAMEGRAPH_TIMEOUT)
    }

    fn write_flamegraph_with_timeout(&self, timeout: Duration) -> Result<(), String> {
        let mut children = PipelineChildren::new()?;
        let result = self.write_flamegraph_pipeline(timeout, &mut children);
        match (result, children.cleanup()) {
            (result, Ok(())) => result,
            (result, Err(cleanup)) => {
                let mut error = match result {
                    Ok(()) => cleanup,
                    Err(error) => format!("{error}; {cleanup}"),
                };
                if let Err(publication) = write_json(
                    &self.dir.join("flamegraph.json"),
                    &FlamegraphStatus {
                        schema_version: 1,
                        status: "unavailable",
                        artifact: None,
                        reason: Some(&error),
                    },
                ) {
                    error.push_str(&format!("; cannot publish cleanup failure: {publication}"));
                }
                Err(error)
            }
        }
    }

    fn write_flamegraph_pipeline(
        &self,
        timeout: Duration,
        children: &mut PipelineChildren,
    ) -> Result<(), String> {
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
        let script = Command::new(measurements::resolve_executable(
            self.tool_dir.as_deref(),
            "perf",
        ))
        .args(["script", "-i"])
        .arg(data)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start perf script for flamegraph: {e}"))?;
        let script = children.add("perf script", script);
        let script_stdout = children
            .child(script)
            .stdout
            .take()
            .ok_or_else(|| "perf script stdout was not piped".to_owned())?;
        let folded_path = self.dir.join("flamegraph.folded");
        let collapsed = Command::new(collapse)
            .stdin(Stdio::from(script_stdout))
            .stdout(Stdio::from(File::create(&folded_path).map_err(|e| {
                format!("cannot create flamegraph folded output: {e}")
            })?))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start flamegraph stack collapse script: {e}"))?;
        let collapsed = children.add("flamegraph stack collapse", collapsed);
        if !wait_bounded(children.child(script), timeout)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf script timed out after 300 seconds or exited unsuccessfully; inspect perf.stderr",
            );
        }
        if !wait_bounded(children.child(collapsed), timeout)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "the stack collapse script timed out after 300 seconds or exited unsuccessfully; inspect flamegraph.folded",
            );
        }
        if folded_path
            .metadata()
            .is_ok_and(|metadata| metadata.len() == 0)
        {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf produced no stack samples for this execution; flamegraph rendering was skipped",
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
        let render = render
            .spawn()
            .map_err(|e| format!("cannot start flamegraph renderer: {e}"))?;
        let render = children.add("flamegraph renderer", render);
        if !wait_bounded(children.child(render), timeout)? {
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
    if let Some(status) = poll_until(child, Instant::now() + timeout)? {
        return Ok(status.success());
    }
    let _ = child.kill();
    if poll_until(child, Instant::now() + CLEANUP_TIMEOUT)?.is_none() {
        return Err("collector timed out and its owned child cleanup was not confirmed".into());
    }
    Ok(false)
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

    #[test]
    fn flamegraph_automatically_resolves_perf_once() {
        let selected = vec!["flamegraph".to_owned()];
        assert_eq!(
            crate::collectors::expand_dependencies(&selected),
            ["flamegraph", "perf"]
        );
        assert_eq!(crate::collectors::dependency_tools(&selected), ["perf"]);
        let both = vec!["perf".to_owned(), "flamegraph".to_owned()];
        assert_eq!(crate::collectors::expand_dependencies(&both), both);
        assert_eq!(crate::collectors::dependency_tools(&both), ["perf"]);
        assert!(crate::collectors::dependency_tools(&["perf".into()]).is_empty());
    }
}
