//! Capture guest-mapped ELF files while the measured process tree exists.
use super::{ADAPTER, SharedArtifact, readiness};
use crate::measurements::{self, Collector};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::time::Duration;

impl measurements::Session {
    pub(super) fn start_perf_symbols(&mut self, pid: u32) -> Result<(), String> {
        let identity = measurements::ProcessIdentity::read(pid)
            .ok_or("perf symbol target is no longer available")?;
        let recorder = self
            .collectors
            .iter()
            .find(|collector| collector.name == "perf")
            .and_then(|collector| collector.producer.id())
            .and_then(measurements::ProcessIdentity::read)
            .ok_or("perf recorder identity is unavailable for symbol settlement")?;
        let directory = self
            .dir
            .canonicalize()
            .map_err(|error| format!("cannot resolve perf symbol directory: {error}"))?;
        let cache = directory.join("perf-symbol-cache");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&cache)
            .map_err(|error| format!("cannot create private perf symbol cache: {error}"))?;
        let metadata = std::fs::metadata(&cache).map_err(|error| error.to_string())?;
        let script = directory.join("perf-symbol-custody.py");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&script)
            .map_err(|error| format!("cannot create perf symbol helper: {error}"))?;
        file.write_all(include_bytes!(
            "../../../../assets/scripts/perf-symbol-custody.py"
        ))
        .map_err(|error| format!("cannot write perf symbol helper: {error}"))?;
        drop(file);
        let executable = measurements::resolve_executable(self.tool_dir.as_deref(), "perf");
        let executable = crate::tools::host_executable(&executable)
            .ok_or("perf executable is unavailable for symbol custody")?;
        let symbols = self
            .tool_dir
            .as_deref()
            .map(|p| p.join("symbols"))
            .unwrap_or_else(|| directory.join("no-project-symbols"));
        let mut ready = readiness::Readiness::new(&directory, "symbols")
            .map_err(|error| format!("cannot prepare symbol custody: {error}"))?;
        let mut launch = if self.collector_uses_sudo("perf") {
            let mut launch = crate::launch::ProcessLaunch::new("sudo");
            launch.arg("-n").arg("--").arg("python3");
            launch
        } else {
            crate::launch::ProcessLaunch::new("python3")
        };
        launch
            .arg("-I")
            .arg(&script)
            .arg(pid.to_string())
            .arg(identity.start_time_ticks.to_string())
            .arg(metadata.uid().to_string())
            .arg(metadata.gid().to_string())
            .arg(&cache)
            .arg(executable)
            .arg(symbols)
            .arg(ready.argument())
            .arg(recorder.pid.to_string())
            .arg(recorder.start_time_ticks.to_string())
            .arg(directory.join("perf.data"));
        let (command, route) = measurements::producer_command(&mut launch)?;
        let captures = [
            crate::evidence::Capture::create(&directory.join("perf-symbol-custody.stdout"))?,
            crate::evidence::Capture::create(&directory.join("perf-symbol-custody.stderr"))?,
        ];
        let mut collector =
            Collector::spawn("perf-symbol-custody", &ADAPTER, command, route, captures)?;
        if let Err(error) = ready.enable(Duration::from_secs(10), || collector.startup_exit()) {
            return Err(collector.startup_failure(error, self._directory_owner.clone()));
        }
        self.set_artifact(SharedArtifact::PerfSymbolCache, cache);
        self.collectors.push(collector);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn offline_symbol_custody_fixtures_run_in_the_rust_gate() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/perf-symbol-custody/test_custody.py");
        let status = std::process::Command::new("python3")
            .arg(fixture)
            .status()
            .unwrap();
        assert!(status.success(), "symbol custody fixtures failed");
    }
}
