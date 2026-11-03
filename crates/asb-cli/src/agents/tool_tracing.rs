//! Content-free local-tool observation for the collector-owned Perfetto relay.
//!
//! This module deliberately knows no MCP arguments, tool names, paths, or
//! JSON-RPC IDs.  The surrounding local-tool/MCP relay retains the matching
//! identity in memory and hands this writer only an opaque numeric cookie.

use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::PathBuf;

const FD_ENV: &str = "ASB_TOOL_PERFETTO_OBSERVER_FD";
const REPORT_ENV: &str = "ASB_TOOL_PERFETTO_REPORT";
const TRACE_ID_ENV: &str = "ASB_TOOL_PERFETTO_TRACE_ID";
const MAX_ACTIVE: usize = 1024;

pub(crate) struct Trace {
    output: Option<File>,
    report: Option<PathBuf>,
    trace_id: Option<String>,
    next_cookie: u64,
    started: u64,
    ended: u64,
    errors: u64,
    active: usize,
    limited: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    producer: &'static str,
    trace_id: &'a str,
    started: u64,
    ended: u64,
    unfinished: usize,
    complete: bool,
    limit_reached: bool,
    observation_errors: u64,
}

impl Trace {
    pub(crate) fn from_environment() -> Self {
        let output = std::env::var(FD_ENV)
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|fd| *fd >= 3)
            .map(|fd| {
                // The descriptor is a narrowly scoped ASB launch input. Its
                // owner retains tracefs and validates every line.
                #[allow(unsafe_code)]
                unsafe {
                    File::from_raw_fd(fd)
                }
            });
        Self {
            output,
            report: std::env::var_os(REPORT_ENV).map(PathBuf::from),
            trace_id: std::env::var(TRACE_ID_ENV).ok(),
            next_cookie: 0,
            started: 0,
            ended: 0,
            errors: 0,
            active: 0,
            limited: false,
        }
    }

    pub(crate) fn begin(&mut self, _operation: &'static str) -> Option<u64> {
        if self.output.is_none() || self.active >= MAX_ACTIVE {
            self.limited |= self.output.is_some();
            return None;
        }
        self.next_cookie = self.next_cookie.saturating_add(1);
        let cookie = self.next_cookie;
        if self.write("begin", cookie, "").is_ok() {
            self.started = self.started.saturating_add(1);
            self.active += 1;
            Some(cookie)
        } else {
            self.errors = self.errors.saturating_add(1);
            None
        }
    }

    pub(crate) fn finish(&mut self, cookie: Option<u64>, failed: bool) {
        let Some(cookie) = cookie else { return };
        if failed && self.write("instant", cookie, "error").is_err() {
            self.errors = self.errors.saturating_add(1);
        }
        if self.write("end", cookie, "").is_ok() {
            self.ended = self.ended.saturating_add(1);
            self.active = self.active.saturating_sub(1);
        } else {
            self.errors = self.errors.saturating_add(1);
        }
    }

    fn write(&mut self, kind: &str, cookie: u64, detail: &str) -> std::io::Result<()> {
        let Some(output) = self.output.as_mut() else {
            return Ok(());
        };
        if kind == "instant" {
            writeln!(output, "instant tool {cookie} {detail}")
        } else {
            writeln!(output, "{kind} tool {cookie}")
        }
    }

    pub(crate) fn close(&mut self) {
        let (Some(path), Some(trace_id)) = (&self.report, &self.trace_id) else {
            return;
        };
        let report = Report {
            schema_version: 1,
            producer: "asb-tool-service",
            trace_id,
            started: self.started,
            ended: self.ended,
            unfinished: self.active,
            complete: self.active == 0 && !self.limited,
            limit_reached: self.limited,
            observation_errors: self.errors,
        };
        let _ = std::fs::write(path, serde_json::to_vec(&report).unwrap_or_default());
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_only_fixed_tool_markers_and_bounded_report() {
        let root = std::env::temp_dir().join(format!("asb-tool-trace-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("marker");
        let report = root.join("perfetto-tools.json");
        let mut trace = Trace {
            output: Some(File::create(&marker).unwrap()),
            report: Some(report.clone()),
            trace_id: Some("run-1".into()),
            next_cookie: 0,
            started: 0,
            ended: 0,
            errors: 0,
            active: 0,
            limited: false,
        };
        let cookie = trace.begin("command");
        trace.finish(cookie, true);
        trace.close();
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "begin tool 1\ninstant tool 1 error\nend tool 1\n"
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        assert_eq!(value["producer"], "asb-tool-service");
        assert_eq!(value["complete"], true);
        std::fs::remove_dir_all(root).unwrap();
    }
}
