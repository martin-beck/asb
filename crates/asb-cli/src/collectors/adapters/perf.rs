use super::super::CollectorAdapter;
use super::super::PreflightContext;
use super::super::{ArtifactDisplay, ProcessContext, ProcessData, SharedArtifact};
use crate::measurements;
use std::process::{Command, Stdio};
use std::time::Duration;

mod kernel_symbols;
#[cfg(target_os = "linux")]
pub(super) mod readiness;
#[cfg(target_os = "linux")]
mod symbols;

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../tests/unit/perf_stop_contract.rs"]
mod stop_contract_tests;

/// Attach to the selected agent root, never the ASB runner. Inheritance follows
/// subsequently created agent threads and children without sampling siblings,
/// collector processes or unrelated host activity.
fn record_args(pid: u32) -> Vec<String> {
    crate::runtime::perf_scope::process_tree_record_args(pid)
}

/// Build the process-scoped interval counter arguments with the same
/// inheritance contract as call-stack recording.
fn stat_args(pid: u32, interval_ms: u64, hardware_events: &[String]) -> Vec<String> {
    let mut events = "task-clock,context-switches,cpu-migrations,page-faults".to_owned();
    if !hardware_events.is_empty() {
        events.push(',');
        events.push_str(&hardware_events.join(","));
    }
    vec![
        "stat".into(),
        "--no-scale".into(),
        "-I".into(),
        interval_ms.to_string(),
        "-x,".into(),
        "-e".into(),
        // These software counters remain available when a workload's cgroup
        // or hybrid-PMU policy rejects hardware cycles/instructions.  Hardware
        // counters are deliberately not estimated or silently substituted.
        events,
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
    ]
}

pub(super) struct PerfCollector;
pub(super) static ADAPTER: PerfCollector = PerfCollector;

/// A symbol tree is usable only with its completed, parseable custody marker.
pub(super) fn symfs_options(cache: &std::path::Path) -> Option<(std::path::PathBuf, bool)> {
    let marker = cache.join("symfs-ready");
    if std::fs::metadata(&marker).ok()?.len() > 4096 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(marker).ok()?).ok()?;
    if value.get("schema_version")?.as_u64()? != 1 {
        return None;
    }
    let no_id = value.get("missing_no_build_id_images")?.as_u64()?;
    let missing = value
        .get("missing_build_id_images")
        .map_or(Some(0), serde_json::Value::as_u64)?;
    let symfs = cache.join("symfs");
    symfs.is_dir().then_some((symfs, no_id > 0 || missing > 0))
}

impl CollectorAdapter for PerfCollector {
    fn private_service_entry(&self, args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
        Some(match streaming_validation_service(args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("perf validation service: {error}");
                std::process::ExitCode::from(1)
            }
        })
    }
    #[cfg(any(target_os = "linux", test))]
    fn host_settings(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        &[(
            "kernel.perf_event_paranoid",
            "-1",
            "perf process-tree counters",
        )]
    }
    fn estimated_disk_mib(&self) -> u64 {
        // Include the bounded symbol cache and one in-flight image copy.
        3072
    }
    fn description(&self) -> &'static str {
        "performance samples and process-tree counters"
    }
    fn id(&self) -> &'static str {
        "perf"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_PERF")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("perf")
    }
    fn prioritizes_artifact(&self, name: &str) -> bool {
        matches!(
            name,
            "perf-symbol-quality.json" | "perf-symbol-custody.json" | "perf-record-scope.json"
        )
    }
    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        match name {
            "perf-symbol-quality.json" => Some(ArtifactDisplay::Status {
                class: "perf-symbol-quality",
                label: "Perf symbol quality",
                suffix: "See the decoded and unknown frame counts in the quality artifact.",
            }),
            "perf-symbol-custody.json" => Some(ArtifactDisplay::Status {
                class: "perf-symbol-custody",
                label: "Perf symbol custody",
                suffix: "See the helper output for recorded images and unavailable debug files.",
            }),
            "perf-record-scope.json" => Some(ArtifactDisplay::Status {
                class: "perf-record-scope",
                label: "Perf recording scope",
                suffix: "CPU-wide samples can include unrelated host, kernel and collector activity; perf stat remains agent-process-tree scoped. CPU leases do not reserve CPUs from other host activity.",
            }),
            _ => None,
        }
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, context: &mut PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn preflight_unavailable(
        &self,
        selected: &[String],
        tools: Option<&std::path::Path>,
    ) -> Option<super::super::PreflightUnavailable> {
        if !super::super::selected_or_environment("perf", selected) {
            return None;
        }
        super::super::PreflightUnavailable::missing_executable(
            &measurements::resolve_executable(tools, "perf"),
            tools,
            "perf",
        )
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<PerfState>().borrow_mut().enabled =
            super::super::selected_or_environment("perf", selected);
        Ok(())
    }
    fn producer_policy(&self, name: &str) -> measurements::ProducerPolicy {
        measurements::ProducerPolicy {
            stream_bytes: 256 * 1024 * 1024,
            graceful: match name {
                "perf-symbol-custody" => Some(std::time::Duration::from_secs(60)),
                // Large recordings can need longer to flush their final data
                // and metadata. Symbol custody waits up to 35 seconds for the
                // recorder, so its own bounded finalization remains ordered.
                "perf" => Some(std::time::Duration::from_secs(30)),
                "perf-stat" => Some(std::time::Duration::from_secs(5)),
                _ => None,
            },
        }
    }
    fn accepts_termination(
        &self,
        name: &str,
        completion: &measurements::ProducerCompletion,
    ) -> bool {
        matches!(name, "perf" | "perf-stat" | "perf-symbol-custody")
            && accepted_exit(
                completion.exit,
                completion.interruption_requested
                    && completion.interruption_acknowledged
                    && completion.guardian_cleanup_confirmed == Some(true)
                    && completion.capture_complete,
            )
    }
    fn allows_failed_termination(&self, name: &str) -> bool {
        matches!(name, "perf" | "perf-stat")
    }
    fn retain_raw_on_processing_failure(&self) -> bool {
        true
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        session.start_perf(pid)
    }
    fn attach_isolated_pid(
        &self,
        session: &mut measurements::Session,
        pid: u32,
    ) -> Result<(), String> {
        // The target is a verified daemon-host PID, but rootless Docker may
        // still reject unprivileged perf_event_open across its user namespace.
        // Prefer an existing non-interactive sudo route before the launch gate
        // resumes; never probe the deliberately stopped target.
        let _ = session.enable_passwordless_sudo_collector("perf");
        session.start_perf(pid)
    }
}

fn process(
    session: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    if !accepted_exit(context.status, context.controlled_interruption) {
        result.status = "failed".into();
        return Ok(result);
    }
    if context.name == "perf-symbol-custody" {
        result.status = "completed".into();
        return Ok(result);
    }
    if context.name == "perf-stat" {
        let file = context
            .stderr_reader()
            .map_err(|error| format!("cannot read complete perf stat output: {error}"))?;
        let parsed = parse_perf_stat(std::io::BufReader::new(file))?;
        if parsed.metrics.is_empty() {
            return Err(
                "perf stat produced no supported numeric counters; no totals published".into(),
            );
        }
        result.metrics = parsed.metrics;
        result.series = parsed.series;
        result.timestamps = parsed.timestamps;
        measurements::write_json(
            &session.dir.join("perf-stat-quality.json"),
            &serde_json::json!({
                "schema_version": 1,
                "counter_mode": "unscaled-observed",
                "whole_run_hardware_estimates": false,
                "events": parsed.quality,
                "limitations": [
                    "Hardware values are observed unscaled counts, potentially incomplete; never estimated whole-run totals.",
                    "Unavailable intervals and multiplexing can omit observations. PMU running percentage is not CPU utilization.",
                    "Hybrid PMU identities and runtime fractions are retained separately. Rounded percentages are never used to reverse scaling."
                ]
            }),
        )?;
        result.artifact = Some("perf-stat-quality.json".into());
    } else {
        let artifact = session
            .artifact(SharedArtifact::StackSamples)
            .filter(|path| path.is_file())
            .ok_or("perf record artifact is missing; cannot validate captured samples")?;
        validate_record(
            session,
            artifact,
            std::time::Duration::from_secs(300),
            256 * 1024 * 1024,
        )?;
        result.artifact = Some(measurements::file_name(artifact));
    }
    result.status = "completed".into();
    Ok(result)
}

// perf v6.12 builtin-stat.c sig_atexit and builtin-record.c record__sig_exit
// restore the signal default and re-raise it AFTER flushing. A signal alone is
// not success: only an acknowledged deliberate interrupt with settled complete
// capture may reach the owning adapter's full-data validation above.
// https://github.com/torvalds/linux/blob/v6.12/tools/perf/builtin-stat.c
// https://github.com/torvalds/linux/blob/v6.12/tools/perf/builtin-record.c
pub(super) fn accepted_exit(status: Option<std::process::ExitStatus>, controlled: bool) -> bool {
    status.is_some_and(|status| {
        #[cfg(unix)]
        let interrupt = {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(libc::SIGINT) || status.code() == Some(128 + libc::SIGINT)
        };
        #[cfg(not(unix))]
        let interrupt = false;
        status.success() || (controlled && interrupt)
    })
}

/// Same built-in offline decoder used by flamegraphs, but bounded independently
/// and supervised through the producer's exact guardian/capture custody. Never
/// run a script extension, provider, live collection, sudo or network lookup.
fn validate_record(
    session: &measurements::Session,
    artifact: &std::path::Path,
    timeout: std::time::Duration,
    stream_bytes: u64,
) -> Result<(), String> {
    let artifact = artifact
        .canonicalize()
        .map_err(|error| format!("cannot resolve perf record artifact: {error}"))?;
    let executable = measurements::resolve_executable(session.tool_dir.as_deref(), "perf");
    let executable = crate::tools::host_executable(&executable)
        .ok_or("selected perf executable is unavailable for offline record validation")?;
    let runtime = crate::process_gate::executable().map_err(|error| error.to_string())?;
    let mut launch = crate::launch::ProcessLaunch::new(runtime);
    launch
        .arg("__asb-collector-service")
        .arg("perf")
        .arg("validate-v1")
        .arg(executable)
        .arg(&artifact);
    let symfs = session
        .artifact(SharedArtifact::PerfSymbolCache)
        .and_then(symfs_options);
    if let Some((path, partial)) = &symfs {
        launch.arg(path);
        if *partial {
            launch.arg("--no-inline");
        }
    }
    let (mut command, route) = measurements::producer_command(&mut launch)?;
    configure_offline_decoder_environment(&mut command, &session.dir);
    if let Some(cache) = session.artifact(SharedArtifact::PerfSymbolCache) {
        command.env("HOME", cache);
    }
    let mut captures = [
        crate::evidence::Capture::create(&session.dir.join("perf-validation.stdout"))?,
        crate::evidence::Capture::create(&session.dir.join("perf-validation.stderr"))?,
    ];
    let mut producer = measurements::Producer::spawn_with_dependency(
        &mut command,
        route,
        measurements::ProducerPolicy {
            stream_bytes,
            graceful: None,
        },
        [captures[0].writer()?, captures[1].writer()?],
        None,
    )?;
    let deadline = std::time::Instant::now() + timeout;
    let completion = loop {
        if let Some(completion) = producer.poll()? {
            break completion;
        }
        if std::time::Instant::now() >= deadline {
            let settled = producer.stop();
            return Err(format!(
                "offline perf record validation timed out; cleanup confirmed: {:?}; validation captures retained",
                settled.guardian_cleanup_confirmed
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    if !completion.capture_complete
        || completion.guardian_cleanup_confirmed != Some(true)
        || !completion.exit.is_some_and(|status| status.success())
    {
        return Err(format!(
            "offline perf record decoder failed: exit {:?}; {}; validation captures retained",
            completion.exit,
            completion.error.unwrap_or_default()
        ));
    }
    // Empty decoded output is a valid zero-sample record only when the actual
    // installed decoder successfully consumed it. Never invent a sample count.
    for capture in &mut captures {
        capture.freeze()?;
    }
    let quality = parse_validation_summary(captures[0].reader()?)?;
    let summary: serde_json::Value = serde_json::from_reader(captures[0].reader()?)
        .map_err(|error| format!("invalid perf decoder summary: {error}"))?;
    measurements::write_json(
        &session.dir.join("perf-symbol-quality.json"),
        &serde_json::json!({
            "schema_version": 1,
            "status": if quality.1 > 0 || symfs.as_ref().is_some_and(|(_, partial)| *partial) { "partial" } else { "not-verified" },
            "decoded_frames": quality.0,
            "unknown_frames": quality.1,
            "kernel_symbols": summary.get("kernel_symbols"),
            "partial_symbol_tree": symfs.as_ref().is_some_and(|(_, partial)| *partial),
            "inline_frames_omitted": symfs.as_ref().is_some_and(|(_, partial)| *partial),
        "reason": "Unknown frames can reflect missing agent, other sampled process or library symbols, JIT code, or restricted kernel addresses (kernel.kptr_restrict). Symbol custody follows the agent tree and may not include unrelated CPU-wide samples. Zero unknown frames alone does not prove complete debug information."
        }),
    )?;
    for capture in &captures {
        capture.consume()?;
    }
    Ok(())
}

fn parse_validation_summary(input: impl std::io::Read) -> Result<(u64, u64), String> {
    let summary: serde_json::Value = serde_json::from_reader(input)
        .map_err(|error| format!("invalid perf decoder summary: {error}"))?;
    let total = summary
        .get("decoded_frames")
        .and_then(serde_json::Value::as_u64)
        .ok_or("perf decoder summary lacks decoded frame count")?;
    let unknown = summary
        .get("unknown_frames")
        .and_then(serde_json::Value::as_u64)
        .filter(|unknown| *unknown <= total)
        .ok_or("perf decoder summary has invalid unknown frame count")?;
    if summary
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err("unsupported perf decoder summary version".into());
    }
    Ok((total, unknown))
}

/// Decode every record while retaining only a bounded frame-count summary.
/// The enclosing producer gate retains custody of this service and its child.
fn streaming_validation_service(args: &[std::ffi::OsString]) -> Result<(), String> {
    if !(3..=5).contains(&args.len())
        || args[0] != "validate-v1"
        || (args.len() == 5 && args[4] != "--no-inline")
    {
        return Err("invalid validation service arguments".into());
    }
    let mut command = Command::new(&args[1]);
    command.arg("script").arg("-i").arg(&args[2]);
    if let Some(symfs) = args.get(3) {
        command.arg("--symfs").arg(symfs);
    }
    if args.len() == 5 {
        command.arg("--no-inline");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start perf decoder: {error}"))?;
    let output = child
        .stdout
        .take()
        .ok_or("perf decoder stdout unavailable")?;
    let quality = match kernel_symbols::frames(output) {
        Ok(quality) => quality,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let status = child
        .wait()
        .map_err(|error| format!("cannot reap perf decoder: {error}"))?;
    if !status.success() {
        return Err(format!("perf decoder failed: {status}"));
    }
    let mut kernel = kernel_symbols::observation(&quality);
    kernel_symbols::add_build_id_evidence(&mut kernel, &args[1], &args[2]);
    println!(
        "{}",
        serde_json::json!({"schema_version": 1, "decoded_frames": quality.total, "unknown_frames": quality.unknown, "kernel_symbols": kernel})
    );
    Ok(())
}

/// Count decoded stack frames without retaining their symbols or paths.
#[cfg(test)]
fn symbol_quality(input: impl std::io::Read) -> Result<(u64, u64), String> {
    let quality = kernel_symbols::frames(input)?;
    Ok((quality.total, quality.unknown))
}

/// Keep offline decoding independent from perf configuration without clearing
/// the process environment. The supported project-local perf build can
/// dereference host-home resources during `perf script` initialization and
/// crashes if its ambient home is replaced with an empty private directory.
pub(super) fn configure_offline_decoder_environment(command: &mut Command, home: &std::path::Path) {
    command
        .env_remove("LD_PRELOAD")
        .env("DEBUGINFOD_URLS", "")
        .env("PERF_CONFIG_NOSYSTEM", "1")
        .env("PERF_CONFIG_NOGLOBAL", "1")
        .env("PERF_PAGER", "cat")
        .env("LC_ALL", "C")
        .current_dir(home);
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    if !super::super::selected_or_environment("perf", context.selected) {
        return Ok(());
    }
    let diagnostic = kernel_symbols::observation(&kernel_symbols::Frames::default());
    if diagnostic["decoder_kallsyms_addresses"] == "zeroed" {
        eprintln!(
            "perf: kernel addresses are hidden from the current user; kernel frames may be unresolved during decoding. Inspect kernel.kptr_restrict and perf-symbol-quality.json after the run; changing host settings requires administrator consent."
        );
    }
    let executable = measurements::resolve_executable(context.tool_dir, "perf");
    let probes = [
        (
            "stat",
            &[
                "stat",
                "--no-scale",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults",
                "--inherit",
                "--",
                "sleep",
                "0.01",
            ][..],
        ),
        (
            "record",
            &["record", "-g", "-F", "19", "-o", "-", "--", "sleep", "0.01"][..],
        ),
    ];
    let mut needs_sudo = false;
    for (name, args) in probes {
        let output = perf_probe(&executable, args, false);
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                if perf_access_denied(&output.stderr) {
                    needs_sudo = true;
                    continue;
                }
                return Err(perf_preflight_failure(name, &executable, output.status.code()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("perf is required for the selected collector. Run `asb tool install perf` and retry.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Err(format!("cannot execute perf at {}: {error}. Check parent-directory access and reinstall with `asb tool install perf`.", executable.display())),
            Err(error) => return Err(format!("cannot probe perf collector: {error}; check with `perf --version`")),
        }
    }
    if !needs_sudo {
        return Ok(());
    }
    if !measurements::passwordless_sudo_available() {
        return Err(perf_permission_error());
    }
    for (name, args) in probes {
        match perf_probe(&executable, args, true) {
            Ok(output) if output.status.success() => {}
            Ok(output) if perf_access_denied(&output.stderr) => {
                return Err(format!(
                    "perf is denied even through passwordless sudo during its {name} preflight. The host or container policy does not permit performance monitoring for this execution boundary. If the host policy permits it, run `sudo sysctl -w kernel.perf_event_paranoid=-1`; containers or other security policy can still deny perf. Inspect `sudo -n {} {}` and retry.",
                    executable.display(),
                    args.join(" ")
                ));
            }
            Ok(output) => {
                return Err(format!(
                    "privileged perf {name} preflight failed ({}). Inspect the host configuration with `sudo -n {} {}` and retry.",
                    output.status,
                    executable.display(),
                    args.join(" ")
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot execute privileged perf at {}: {error}",
                    executable.display()
                ));
            }
        }
    }
    context.sudo_collectors.insert("perf".into());
    Ok(())
}

fn perf_preflight_failure(name: &str, executable: &std::path::Path, code: Option<i32>) -> String {
    let status = code.map_or_else(
        || "terminated by a signal".to_owned(),
        |value| format!("exit status: {value}"),
    );
    if code == Some(127) {
        return format!(
            "perf {name} preflight failed ({status}) using {}. A runtime library or interpreter may be missing. Run `{} --version` to inspect the selected executable, then reinstall it with `asb tool install perf` and retry.",
            executable.display(),
            executable.display()
        );
    }
    format!(
        "perf {name} preflight failed ({status}) using {}. Inspect the host configuration with `{} {name} -- sleep 0.01` and retry.",
        executable.display(),
        executable.display()
    )
}

fn perf_probe(
    executable: &std::path::Path,
    args: &[&str],
    sudo: bool,
) -> std::io::Result<std::process::Output> {
    let mut command = if sudo {
        let mut command = Command::new("sudo");
        command.args(["-n", "--"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command.args(args).stdout(Stdio::null()).output()
}

fn perf_access_denied(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("permission")
        || stderr.contains("perf_event_paranoid")
        || stderr.contains("access to performance monitoring")
        || stderr.contains("operation not permitted")
}

pub(super) fn reserve_recording_artifact(path: &std::path::Path) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot reserve perf recording artifact: {error}"))?;
    Ok(())
}

use crate::measurements::{Collector, env_true, measurement_interval_ms, write_unavailable};
use std::collections::BTreeMap;
use std::io::{BufRead, Read};

#[derive(Default)]
struct PerfState {
    enabled: bool,
    cpu_wide: Option<Vec<u32>>,
}

fn selected_record_args(pid: u32, cpu_wide: Option<&[u32]>) -> Result<Vec<String>, String> {
    match cpu_wide {
        Some(cpus) => crate::runtime::perf_scope::cpu_wide_record_args(cpus),
        None => Ok(record_args(pid)),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn validate_agent_target(
    pid: u32,
    identity: Option<measurements::ProcessIdentity>,
) -> Result<(), String> {
    let root = identity
        .filter(|root| root.pid == pid && root.matches())
        .ok_or("perf requires the live, generation-bound agent measurement target")?;
    let runner = measurements::ProcessIdentity::read(std::process::id())
        .ok_or("cannot verify ASB process identity before perf attachment")?;
    if runner.in_tree(root) {
        return Err("perf target includes ASB; select the agent process root instead".into());
    }
    Ok(())
}

impl measurements::Session {
    pub(crate) fn configure_perf_cpu_scope(&mut self, cpus: &[u32]) -> Result<(), String> {
        // Validate the full allocation before retaining it for collector startup.
        crate::runtime::perf_scope::cpu_wide_record_args(cpus)?;
        self.state::<PerfState>().borrow_mut().cpu_wide = Some(cpus.to_vec());
        Ok(())
    }

    pub(crate) fn start_perf(&mut self, pid: u32) -> Result<(), String> {
        if !self.state::<PerfState>().borrow().enabled {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        validate_agent_target(pid, self.measurement_target())?;
        #[cfg(target_os = "linux")]
        let mut record_ready = readiness::Readiness::new(
            &self
                .dir
                .canonicalize()
                .map_err(|error| format!("cannot resolve perf capture directory: {error}"))?,
            "record",
        )
        .map_err(|error| format!("cannot prepare perf activation: {error}"))?;
        let data = self.dir.join("perf.data");
        // A passwordless-sudo fallback may be needed for rootless isolated
        // process trees. Pre-creating the destination keeps its ownership
        // with the benchmark user so later `perf script`/Flamegraph processing
        // can read it without expanding privileged access.
        reserve_recording_artifact(&data)?;
        let stderr_path = self.dir.join("perf.stderr");
        let stdout_path = self.dir.join("perf.stdout");
        let captures = [
            crate::evidence::Capture::create(&stdout_path)?,
            crate::evidence::Capture::create(&stderr_path)?,
        ];
        // The default recording attaches to the verified agent root and
        // inherits into descendants. The optional CPU-wide mode instead
        // samples every task running on this replica's allocated CPUs.
        let cpu_wide = self.state::<PerfState>().borrow().cpu_wide.clone();
        let record_args = selected_record_args(pid, cpu_wide.as_deref())?;
        let (mut command, route) = self.collector_command("perf")?;
        command
            // Keep perf data bounded enough for the offline flamegraph
            // renderer while retaining call samples in the selected scope.
            .args(&record_args)
            .args(["-o"])
            .arg(&data);
        #[cfg(target_os = "linux")]
        command.args(["-D", "-1", "--control", &record_ready.argument()]);
        let mut collector = match Collector::spawn("perf", &ADAPTER, command, route, captures) {
            Ok(collector) => collector,
            Err(error) if !env_true("ASB_MEASURE_PERF") => {
                write_unavailable(&self.dir.join("perf.json"), "perf", error.to_string())?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start perf (disable with ASB_MEASURE_PERF=0): {error}"
                ));
            }
        };
        #[cfg(target_os = "linux")]
        let record_failure = record_ready
            .enable(Duration::from_secs(10), || collector.startup_exit())
            .err();
        #[cfg(not(target_os = "linux"))]
        let record_failure = startup_failure(&mut collector)?;
        if let Some(reason) = record_failure {
            write_unavailable(
                &self.dir.join("perf.json"),
                "perf",
                collector.startup_failure(reason, self._directory_owner.clone()),
            )?;
            return Ok(());
        }
        self.set_artifact(SharedArtifact::StackSamples, data);
        self.collectors.push(collector);
        if let Some(cpus) = cpu_wide {
            measurements::write_json(
                &self.dir.join("perf-record-scope.json"),
                &serde_json::json!({
                    "schema_version": 1,
                    "scope": "cpu-wide",
                    "logical_cpus": cpus,
                    "perf_stat_scope": "agent-process-tree",
                    "warning": "CPU-wide samples can include unrelated host, kernel and collector activity; cooperative leases do not reserve CPUs from the operating system."
                }),
            )?;
        }
        let stat_stdout_path = self.dir.join("perf-stat.stdout");
        let stat_stderr_path = self.dir.join("perf-stat.stderr");
        let captures = [
            crate::evidence::Capture::create(&stat_stdout_path)?,
            crate::evidence::Capture::create(&stat_stderr_path)?,
        ];
        let hardware_events = super::arm_pmu::events(self)
            .unwrap_or_else(|| super::agentsysperf::telemetry_events(self));
        measurements::write_json(
            &self.dir.join("perf-stat-plan.json"),
            &serde_json::json!({
                "schema_version":1, "scope":"agent-process-tree", "counter_mode":"unscaled-observed",
                "root":self.measurement_target(), "groups":hardware_events,
                "excluded_controller_pid":std::process::id(), "excluded_guardian":self.measurement_guardian(),
                "inherit":true, "system_wide":false,
            }),
        )?;
        let stat_args = stat_args(pid, measurement_interval_ms(), &hardware_events);
        let (mut command, route) = self.collector_command("perf")?;
        command.args(&stat_args);
        #[cfg(target_os = "linux")]
        let mut stat_ready = readiness::Readiness::new(
            &self
                .dir
                .canonicalize()
                .map_err(|error| format!("cannot resolve perf capture directory: {error}"))?,
            "stat",
        )
        .map_err(|error| format!("cannot prepare perf-stat activation: {error}"))?;
        #[cfg(target_os = "linux")]
        command.args(["-D", "-1", "--control", &stat_ready.argument()]);
        let stat = Collector::spawn("perf-stat", &ADAPTER, command, route, captures);
        match stat {
            Ok(mut collector) => {
                #[cfg(target_os = "linux")]
                let stat_failure = stat_ready
                    .enable(Duration::from_secs(10), || collector.startup_exit())
                    .err();
                #[cfg(not(target_os = "linux"))]
                let stat_failure = startup_failure(&mut collector)?;
                if let Some(reason) = stat_failure {
                    write_unavailable(
                        &self.dir.join("perf-stat.json"),
                        "perf-stat",
                        collector.startup_failure(reason, self._directory_owner.clone()),
                    )?;
                } else {
                    self.collectors.push(collector);
                }
            }
            Err(error) => write_unavailable(
                &self.dir.join("perf-stat.json"),
                "perf-stat",
                error.to_string(),
            )?,
        }
        #[cfg(target_os = "linux")]
        if let Err(error) = self.start_perf_symbols(pid) {
            write_unavailable(
                &self.dir.join("perf-symbol-custody.json"),
                "perf-symbol-custody",
                error,
            )?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn startup_failure(collector: &mut Collector) -> Result<Option<String>, String> {
    // perf reports inaccessible PMUs immediately. Give its owned capture
    // worker one scheduling interval to observe that deterministic exit.
    std::thread::sleep(Duration::from_millis(25));
    collector.startup_exit()
}

// These are explicit failure bounds, never silent prefix limits. Diagnostics
// keep their separate 256 KiB bound. A measurement exceeding a bound fails
// without publishing plausible-looking partial totals or compressed timelines.
const MAX_STAT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STAT_LINE: usize = 16 * 1024;
const MAX_STAT_POINTS: usize = 100_000;

#[derive(Default)]
struct PerfStat {
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, Vec<f64>>,
    timestamps: BTreeMap<String, Vec<f64>>,
    quality: BTreeMap<String, EventQuality>,
}

#[derive(Default, serde::Serialize)]
struct EventQuality {
    observed_value: Option<f64>,
    /// Exact integer hardware counts, independent of graph rounding.
    observed_integer_count: Option<u64>,
    value_unit: &'static str,
    observed_rows: u64,
    unavailable_rows: u64,
    unknown_quality_rows: u64,
    known_event_runtime_ns: f64,
    min_running_pct: Option<f64>,
    max_running_pct: Option<f64>,
}

fn parse_perf_stat(reader: impl BufRead) -> Result<PerfStat, String> {
    parse_perf_stat_bounded(reader, MAX_STAT_BYTES)
}

fn parse_perf_stat_bounded(mut reader: impl BufRead, max_bytes: u64) -> Result<PerfStat, String> {
    let mut parsed = PerfStat::default();
    let mut line = Vec::new();
    let mut bytes = 0_u64;
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take(MAX_STAT_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("cannot read complete perf stat output: {error}"))?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if count > MAX_STAT_LINE || bytes > max_bytes {
            return Err(
                "perf stat output exceeds measurement parsing bounds; no partial totals published"
                    .into(),
            );
        }
        if !line.ends_with(b"\n") {
            return Err("unterminated perf stat output; no partial totals published".into());
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| "perf stat output is not UTF-8; no partial totals published")?;
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields
            .iter()
            .position(|field| perf_metric_name(field).is_some())
        else {
            if line.contains(',') && !line.trim_start().starts_with('#') {
                return Err("unrecognized perf stat CSV row; no partial totals published".into());
            }
            continue;
        };
        if !matches!(event_index, 2 | 3) || fields.len() < event_index + 2 {
            return Err("malformed perf stat CSV row; no partial totals published".into());
        }
        let raw_value = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .ok_or("malformed perf stat counter; no partial totals published")?;
        let quality = parsed
            .quality
            .entry(fields[event_index].into())
            .or_default();
        quality.value_unit = if fields[event_index] == "task-clock" {
            "ms"
        } else {
            "count"
        };
        if matches!(*raw_value, "<not counted>" | "<not supported>") {
            quality.unavailable_rows += 1;
            continue;
        }
        let runtime = fields
            .get(event_index + 1)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0);
        let running_pct = fields
            .get(event_index + 2)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
        quality.observed_rows += 1;
        if let Some(runtime) = runtime {
            quality.known_event_runtime_ns += runtime;
            if !quality.known_event_runtime_ns.is_finite() {
                return Err("perf stat runtime overflow; no partial totals published".into());
            }
        }
        if let Some(pct) = running_pct {
            quality.min_running_pct = Some(quality.min_running_pct.map_or(pct, |old| old.min(pct)));
            quality.max_running_pct = Some(quality.max_running_pct.map_or(pct, |old| old.max(pct)));
        }
        if runtime.is_none() || running_pct.is_none() {
            quality.unknown_quality_rows += 1;
        }
        let value: f64 = raw_value
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite())
            .ok_or("invalid perf stat counter; no partial totals published")?;
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        if value < 0.0 {
            return Err("negative perf stat counter; no partial totals published".into());
        }
        let observed_value = quality.observed_value.unwrap_or(0.0) + value;
        if !observed_value.is_finite() {
            return Err("perf stat PMU counter overflow; no partial totals published".into());
        }
        quality.observed_value = Some(observed_value);
        if exact_hardware_event(fields[event_index]) {
            let integer = raw_value
                .parse::<u64>()
                .map_err(|_| "hardware counter is not an exact integer")?;
            quality.observed_integer_count = Some(
                quality
                    .observed_integer_count
                    .unwrap_or(0)
                    .checked_add(integer)
                    .ok_or("hardware counter integer overflow")?,
            );
        }
        let name = format!("perf.{name}");
        let timestamp = if event_index == 3 {
            Some(
                fields[0]
                    .parse::<f64>()
                    .ok()
                    .map(|seconds| seconds * 1_000.0)
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .ok_or("invalid perf stat interval timestamp; no partial totals published")?,
            )
        } else {
            None
        };
        let total = parsed.metrics.entry(name.clone()).or_default();
        *total += value;
        if !total.is_finite() {
            return Err("perf stat counter overflow; no partial totals published".into());
        }
        let values = parsed.series.entry(name.clone()).or_default();
        let timestamps = parsed.timestamps.entry(name).or_default();
        if let Some(timestamp) = timestamp {
            if timestamps.len() != values.len()
                || timestamps.last().is_some_and(|last| timestamp < *last)
            {
                return Err(
                    "inconsistent perf stat interval timestamps; no partial totals published"
                        .into(),
                );
            }
            // Hybrid CPUs may emit separate core/atom hardware counters for
            // the same interval. Sum those into one observed time point.
            if timestamps.last() == Some(&timestamp) {
                *values.last_mut().expect("timestamp has paired value") += value;
                continue;
            }
            timestamps.push(timestamp);
        } else if !timestamps.is_empty() {
            return Err(
                "mixed interval and aggregate perf stat output; no partial totals published".into(),
            );
        }
        if values.len() >= MAX_STAT_POINTS {
            return Err(
                "perf stat series exceeds measurement point bound; no partial totals published"
                    .into(),
            );
        }
        values.push(value);
    }
    parsed.timestamps.retain(|_, values| !values.is_empty());
    Ok(parsed)
}

#[cfg(test)]
pub(crate) fn parse_perf_stat_series(output: &str) -> BTreeMap<String, Vec<f64>> {
    parse_perf_stat(std::io::Cursor::new(output))
        .map(|parsed| parsed.series)
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn parse_perf_stat_metrics(output: &str) -> BTreeMap<String, f64> {
    // `perf stat -I` emits deltas, not cumulative snapshots. Scalar run
    // counters must sum those same observations used by the interval graphs.
    // A single non-interval observation retains its existing value.
    parse_perf_stat(std::io::Cursor::new(output))
        .map(|parsed| parsed.metrics)
        .unwrap_or_default()
}

fn exact_hardware_event(event: &str) -> bool {
    if super::arm_pmu::metric_name(event).is_some() {
        return true;
    }
    let event = event
        .strip_prefix("cpu_core/")
        .or_else(|| event.strip_prefix("cpu_atom/"))
        .and_then(|event| event.strip_suffix('/'))
        .unwrap_or(event);
    matches!(
        event,
        "cycles" | "instructions" | "cache-misses" | "branch-misses"
    )
}

fn perf_metric_name(event: &str) -> Option<&'static str> {
    if event.starts_with("armv8_pmuv3") {
        return super::arm_pmu::metric_name(event);
    }
    // On Intel hybrid CPUs `perf stat` expands an architected event into
    // separate `cpu_core/<event>/` and `cpu_atom/<event>/` rows. They remain
    // separate raw-quality observations, while ASB's validated scalar/series
    // aggregates deliberately sum their common architectural metric.
    let event = event
        .strip_prefix("cpu_core/")
        .or_else(|| event.strip_prefix("cpu_atom/"))
        .and_then(|event| event.strip_suffix('/'))
        .unwrap_or(event);
    match event {
        "task-clock" => Some("task_clock_ms"),
        "context-switches" => Some("context_switches"),
        "cpu-migrations" => Some("cpu_migrations"),
        "page-faults" => Some("page_faults"),
        "cycles" => Some("observed_cycles_count"),
        "instructions" => Some("observed_instructions_count"),
        "cache-misses" => Some("cache_misses_count"),
        "branch-misses" => Some("branch_misses_count"),
        // `perf` adds slots to a hybrid Topdown event group. Retain it as an
        // observed counter so its presence cannot invalidate the complete
        // process-scoped capture, even when no current analyzer consumes it.
        "slots" => Some("topdown_slots_count"),
        "topdown-fe-bound" => Some("topdown_fe_bound_count"),
        "topdown-be-bound" => Some("topdown_be_bound_count"),
        "l1d_cache_refill" => Some("observed_l1d_cache_refill_count"),
        "stall_backend" => Some("observed_stall_backend_count"),
        "stall_frontend" => Some("observed_stall_frontend_count"),
        _ => None,
    }
}

pub(crate) fn perf_permission_error() -> String {
    "perf was denied access to performance events. A restrictive kernel.perf_event_paranoid setting is one possible cause.\nWhat to do: if your administrator approves system-wide perf access, run:\n  sudo sysctl -w kernel.perf_event_paranoid=-1\nTo persist the setting across reboots:\n  printf 'kernel.perf_event_paranoid=-1\\n' | sudo tee /etc/sysctl.d/99-asb-perf.conf\nThis changes access for the whole host; containers, capabilities, security policy or other restrictions may still prevent tracing. ASB does not create sudoers rules for perf.".into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn selected_record_scope_switches_without_changing_the_default() {
        assert_eq!(
            super::selected_record_args(42, None).unwrap(),
            super::record_args(42)
        );
        let args = super::selected_record_args(42, Some(&[3, 5])).unwrap();
        assert!(args.windows(2).any(|pair| pair == ["-C", "3,5"]));
        assert!(args.iter().any(|arg| arg == "-a"));
        assert!(!args.iter().any(|arg| arg == "-p"));
        assert!(super::selected_record_args(42, Some(&[])).is_err());
    }
    #[test]
    fn symfs_marker_distinguishes_partial_complete_and_invalid_trees() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(root.join("symfs")).unwrap();
        let marker = root.join("symfs-ready");
        assert!(super::symfs_options(&root).is_none());
        std::fs::write(
            &marker,
            r#"{"schema_version":1,"missing_no_build_id_images":0}"#,
        )
        .unwrap();
        assert!(!super::symfs_options(&root).unwrap().1);
        std::fs::write(
            &marker,
            r#"{"schema_version":1,"missing_no_build_id_images":0,"missing_build_id_images":2}"#,
        )
        .unwrap();
        assert!(super::symfs_options(&root).unwrap().1);
        std::fs::write(
            &marker,
            r#"{"schema_version":1,"missing_no_build_id_images":1,"missing_build_id_images":0}"#,
        )
        .unwrap();
        assert!(super::symfs_options(&root).unwrap().1);
        std::fs::write(
            &marker,
            r#"{"schema_version":1,"missing_no_build_id_images":0,"missing_build_id_images":-1}"#,
        )
        .unwrap();
        assert!(super::symfs_options(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an existing local perf.data and matching perf tool"]
    fn installed_perf_record_streaming_validation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perf-stream-validation")
            .join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let mut session =
            crate::measurements::Session::start_with_collectors(&root, &[], None).unwrap();
        session.tool_dir = Some(std::path::PathBuf::from(
            std::env::var_os("ASB_TEST_PERF_TOOLS").expect("ASB_TEST_PERF_TOOLS"),
        ));
        if let Some(cache) = std::env::var_os("ASB_TEST_PERF_CACHE") {
            session.set_artifact(super::SharedArtifact::PerfSymbolCache, cache.into());
        }
        let artifact = std::path::PathBuf::from(
            std::env::var_os("ASB_TEST_PERF_DATA").expect("ASB_TEST_PERF_DATA"),
        );
        super::validate_record(
            &session,
            &artifact,
            std::time::Duration::from_secs(300),
            1024 * 1024,
        )
        .unwrap();
        let quality = std::fs::read_to_string(root.join("perf-symbol-quality.json")).unwrap();
        println!("{quality}");
        let report: serde_json::Value = serde_json::from_str(&quality).unwrap();
        assert!(report["kernel_symbols"]["status"].as_str().is_some());
        session.set_artifact(super::SharedArtifact::StackSamples, artifact);
        session.write_flamegraph().unwrap();
        let flamegraph: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("flamegraph.json")).unwrap()).unwrap();
        assert_eq!(flamegraph["status"], "completed");
        assert!(root.join("flamegraph.svg").is_file());
        println!("flamegraph={flamegraph}");
    }

    #[test]
    fn validation_summary_rejects_incomplete_or_inconsistent_counts() {
        assert_eq!(
            super::parse_validation_summary(
                &b"{\"schema_version\":1,\"decoded_frames\":8,\"unknown_frames\":2}"[..]
            )
            .unwrap(),
            (8, 2)
        );
        for input in [
            "",
            "{}",
            "{\"schema_version\":2,\"decoded_frames\":1,\"unknown_frames\":0}",
            "{\"schema_version\":1,\"decoded_frames\":1,\"unknown_frames\":2}",
            "{\"schema_version\":1,\"decoded_frames\":1}",
        ] {
            assert!(
                super::parse_validation_summary(input.as_bytes()).is_err(),
                "{input}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn validation_service_streams_large_decode_and_reports_child_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir(&root).unwrap();
        let executable = root.join("perf-fixture");
        std::fs::write(&executable, "#!/usr/bin/python3\nimport sys\nchunk = b'\\t1 [unknown] (module)\\n' * 4096\nfor _ in range(3000): sys.stdout.buffer.write(chunk)\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = vec![
            "validate-v1".into(),
            executable.as_os_str().to_owned(),
            "fixture.data".into(),
        ];
        assert!(super::streaming_validation_service(&args).is_ok());
        std::fs::write(
            &executable,
            "#!/usr/bin/python3\nimport sys\nsys.stdout.write('partial\\n')\nsys.exit(7)\n",
        )
        .unwrap();
        assert!(
            super::streaming_validation_service(&args)
                .unwrap_err()
                .contains("perf decoder failed")
        );
        assert!(super::streaming_validation_service(&args[..2]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_perf_probe_identifies_the_actual_binary_and_missing_runtime_hint() {
        let path = std::path::Path::new("/project/tools/perf/bin/perf");
        let missing = super::perf_preflight_failure("stat", path, Some(127));
        assert!(missing.contains("/project/tools/perf/bin/perf --version"));
        assert!(missing.contains("asb tool install perf"));
        let other = super::perf_preflight_failure("record", path, Some(2));
        assert!(other.contains("/project/tools/perf/bin/perf record -- sleep 0.01"));
        assert!(!other.contains("runtime library"));
    }
    #[test]
    fn generic_and_hybrid_integer_counts_are_exact_and_never_cross_domains() {
        let data = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,9007199254740993,,cycles,100,100.00,,\n0.1,11,,cpu_core/cycles/,100,100.00,,\n0.1,17,,cpu_atom/cycles/,100,100.00,,\n"
        )).unwrap();
        for (name, count) in [
            ("cycles", 9007199254740993_u64),
            ("cpu_core/cycles/", 11),
            ("cpu_atom/cycles/", 17),
        ] {
            assert_eq!(data.quality[name].observed_integer_count, Some(count));
        }
        for value in ["1.5", "18446744073709551616"] {
            let row = format!("0.1,{value},,cycles,100,100.00,,\n");
            assert!(super::parse_perf_stat(std::io::Cursor::new(row)).is_err());
        }
        assert!(
            super::parse_perf_stat(std::io::Cursor::new(
                "0.1,18446744073709551615,,cycles,100,100.00,,\n0.2,1,,cycles,100,100.00,,\n"
            ))
            .is_err()
        );
    }
    #[test]
    fn arm_integer_counts_preserve_precision_and_pmu_quality_identity() {
        let data = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,9007199254740993,,armv8_pmuv3_0/cpu_cycles/,100000000,100.00,,\n0.1,17,,armv8_pmuv3_1/cpu_cycles/,100000000,100.00,,\n0.1,<not counted>,,armv8_pmuv3_0/inst_retired/,0,0.00,,\n"
        )).unwrap();
        assert_eq!(
            data.quality["armv8_pmuv3_0/cpu_cycles/"].observed_integer_count,
            Some(9007199254740993)
        );
        assert_eq!(
            data.quality["armv8_pmuv3_1/cpu_cycles/"].observed_integer_count,
            Some(17)
        );
        assert_eq!(
            data.quality["armv8_pmuv3_0/inst_retired/"].observed_integer_count,
            None
        );
        assert_eq!(
            data.quality["armv8_pmuv3_0/inst_retired/"].unavailable_rows,
            1
        );
        let overflow = "0.1,18446744073709551615,,armv8_pmuv3_0/cpu_cycles/,1,100.00,,\n0.2,1,,armv8_pmuv3_0/cpu_cycles/,1,100.00,,\n";
        assert!(super::parse_perf_stat(std::io::Cursor::new(overflow)).is_err());
    }
    use super::{record_args, stat_args};

    #[test]
    fn decoded_symbol_quality_counts_unknown_frames_across_chunks() {
        struct OneByte<'a>(&'a [u8]);
        impl std::io::Read for OneByte<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                output[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let text =
            b"agent 123 sample\n\t123 function+1 (agent)\n\t456 [unknown] (agent)\n\t789 [unknown]";
        assert_eq!(super::symbol_quality(OneByte(text)).unwrap(), (3, 2));
        assert_eq!(super::symbol_quality(&b""[..]).unwrap(), (0, 0));
    }

    #[test]
    fn symbol_quality_is_visible_in_reports() {
        use crate::collectors::CollectorAdapter;
        assert!(super::ADAPTER.prioritizes_artifact("perf-symbol-quality.json"));
        assert!(super::ADAPTER.prioritizes_artifact("perf-record-scope.json"));
        assert!(
            super::ADAPTER
                .display_artifact("perf-record-scope.json")
                .is_some()
        );
        assert!(
            super::ADAPTER
                .display_artifact("perf-symbol-quality.json")
                .is_some()
        );
        assert!(super::ADAPTER.prioritizes_artifact("perf-symbol-custody.json"));
        assert!(
            super::ADAPTER
                .display_artifact("perf-symbol-custody.json")
                .is_some()
        );
        assert!(
            super::ADAPTER
                .display_artifact("not-symbol-quality.json")
                .is_none()
        );
    }

    #[test]
    fn permission_error_suggests_kernel_policy_not_sudoers() {
        let message = super::perf_permission_error();
        assert!(message.contains("kernel.perf_event_paranoid=-1"));
        assert!(message.contains("sudo sysctl -w kernel.perf_event_paranoid=-1"));
        assert!(message.contains("/etc/sysctl.d/99-asb-perf.conf"));
        assert!(message.contains("does not create sudoers rules"));
        assert!(!message.contains("sudoers.d"));
    }

    #[test]
    fn access_denial_detection_covers_perf_and_kernel_diagnostics() {
        assert!(super::perf_access_denied(
            b"Failure to open event 'cpu-clock': Operation not permitted"
        ));
        assert!(super::perf_access_denied(
            b"Access to performance monitoring is limited; perf_event_paranoid setting is 2"
        ));
        assert!(!super::perf_access_denied(
            b"event is not supported by this PMU"
        ));
    }

    #[test]
    fn recording_artifact_is_reserved_once_by_the_benchmark_user() {
        let directory = std::env::temp_dir().join(format!(
            "asb-perf-reservation-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let artifact = directory.join("perf.data");
        super::reserve_recording_artifact(&artifact).unwrap();
        assert!(artifact.is_file());
        assert!(super::reserve_recording_artifact(&artifact).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_perf_stat_counters_into_stable_metrics() {
        let metrics = super::parse_perf_stat_metrics(
            "12.34,msec,task-clock,100.00,\n8,,context-switches,100.00,\n2,,cpu-migrations,100.00,\n4,,page-faults,100.00,\n1234,,cycles,100.00,\n567,,instructions,100.00,\n",
        );
        assert_eq!(metrics["perf.task_clock_ms"], 12.34);
        assert_eq!(metrics["perf.context_switches"], 8.0);
        assert_eq!(metrics["perf.cpu_migrations"], 2.0);
        assert_eq!(metrics["perf.page_faults"], 4.0);
        assert_eq!(metrics["perf.observed_cycles_count"], 1234.0);
        assert_eq!(metrics["perf.observed_instructions_count"], 567.0);
        let series = super::parse_perf_stat_series(
            "0.000,12.34,msec,task-clock,100.00,\n1.000,20.00,msec,task-clock,100.00,\n0.000,8,,context-switches,100.00,\n",
        );
        assert_eq!(series["perf.task_clock_ms"], vec![12.34, 20.0]);
        assert_eq!(series["perf.context_switches"], vec![8.0]);
    }

    #[test]
    fn complete_perf_stat_file_not_diagnostic_prefix_drives_totals_and_timestamps() {
        use std::io::Write;
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perf-complete", None).attempt_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf-stat.stderr");
        let mut file = std::fs::File::create(&path).unwrap();
        // More than the former diagnostic cap; 800 observed seconds with a
        // deliberate missing interval must stay 800s, not compress to 799s.
        for interval in 1..=8000 {
            if interval == 500 {
                continue;
            }
            writeln!(
                file,
                "{:.3},2.00,msec,task-clock,100000000,100.00,,",
                interval as f64 / 10.0
            )
            .unwrap();
        }
        drop(file);
        assert!(path.metadata().unwrap().len() > 256 * 1024);
        let mut session =
            crate::measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
        let status = std::process::Command::new("true").status().unwrap();
        let capture = crate::evidence::Capture::open(&path).unwrap();
        let data = super::process(
            &mut session,
            &crate::collectors::ProcessContext::new(
                "perf-stat",
                &status,
                ["", "diagnostic prefix only"],
                [&capture, &capture],
            ),
        )
        .unwrap();
        assert_eq!(data.status, "completed");
        assert_eq!(data.metrics["perf.task_clock_ms"], 15998.0);
        assert_eq!(data.series["perf.task_clock_ms"].len(), 7999);
        assert_eq!(
            data.timestamps["perf.task_clock_ms"].last(),
            Some(&800_000.0)
        );
        assert_eq!(data.timestamps["perf.task_clock_ms"][499], 50_100.0);
        std::fs::remove_file(&path).unwrap();
        // A removed/replaced name cannot redirect the admitted source.
        std::fs::write(&path, "0.1,999,,cycles,1,\n").unwrap();
        let retained = super::process(
            &mut session,
            &crate::collectors::ProcessContext::new(
                "perf-stat",
                &status,
                ["", "1,,cycles,1,"],
                [&capture, &capture],
            ),
        )
        .unwrap();
        assert_eq!(retained.metrics, data.metrics);
        assert_eq!(retained.timestamps, data.timestamps);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0.1,999,,cycles,1,\n"
        );
        drop(session);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn perf_stat_gaps_and_hybrid_counters_preserve_real_interval_offsets() {
        let parsed = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,4,,cpu_core/cycles/,1,\n0.1,3,,cpu_atom/cycles/,1,\n0.2,<not counted>,,cpu_core/cycles/,0,\n0.5,9,,cpu_core/cycles/,1,\n"
        )).unwrap();
        assert_eq!(parsed.metrics["perf.observed_cycles_count"], 16.0);
        assert_eq!(parsed.series["perf.observed_cycles_count"], [7.0, 9.0]);
        assert_eq!(
            parsed.timestamps["perf.observed_cycles_count"],
            [100.0, 500.0]
        );
    }

    #[test]
    fn hybrid_pmu_aliases_normalize_all_selected_architectural_events() {
        let parsed = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,2,,cpu_core/cache-misses/,1,\n0.1,3,,cpu_atom/cache-misses/,1,\n0.1,4,,cpu_core/branch-misses/,1,\n0.1,5,,cpu_atom/branch-misses/,1,\n0.1,6,,cpu_core/topdown-fe-bound/,1,\n0.1,7,,cpu_atom/topdown-fe-bound/,1,\n"
        )).unwrap();
        assert_eq!(parsed.metrics["perf.cache_misses_count"], 5.0);
        assert_eq!(parsed.metrics["perf.branch_misses_count"], 9.0);
        assert_eq!(parsed.metrics["perf.topdown_fe_bound_count"], 13.0);
    }

    #[test]
    fn raw_hybrid_counts_keep_pmu_quality_without_reversing_rounded_scaling() {
        let parsed = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,400,,cpu_core/cycles/,100000000,99.00,,\n0.1,2,,cpu_atom/cycles/,1000,0.00,,\n0.2,<not counted>,,cpu_atom/cycles/,0,0.00,,\n0.3,9,,instructions,100,,\n"
        )).unwrap();
        assert_eq!(parsed.metrics["perf.observed_cycles_count"], 402.0);
        assert!(!parsed.metrics.contains_key("perf.cycles"));
        let atom = &parsed.quality["cpu_atom/cycles/"];
        assert_eq!(atom.observed_value, Some(2.0));
        assert_eq!(atom.min_running_pct, Some(0.0));
        assert_eq!(atom.known_event_runtime_ns, 1000.0);
        assert_eq!(atom.unavailable_rows, 1);
        assert_eq!(
            parsed.quality["cpu_core/cycles/"].observed_value,
            Some(400.0)
        );
        assert_eq!(parsed.quality["instructions"].unknown_quality_rows, 1);
        let args = stat_args(1234, 100, &[]);
        assert!(args.iter().any(|arg| arg == "--no-scale"));
        assert!(!args.iter().any(|arg| arg == "--all-cpus"));
    }

    #[test]
    fn perf_stat_refuses_partial_totals_on_invalid_or_excessive_input() {
        let two_rows = "0.1,2,,cycles,1,\n0.2,3,,cycles,1,\n";
        assert!(super::parse_perf_stat_bounded(std::io::Cursor::new(two_rows), 20).is_err());
        for suffix in [
            "0.2,NaN,,cycles,1,\n",
            "0.2,inf,,cycles,1,\n",
            "0.2,-1,,cycles,1,\n",
            "0.05,3,,cycles,1,\n",
            "not-time,3,,cycles,1,\n",
            "3,,cycles,1,\n",
            "0.2,4,,cpu_core/cyc",
            "0.2,4,,cycles",
            "0.2,4,,unknown-event,1,\n",
            "cycles,1,\n",
            "0.2,4,msec,extra,task-clock,1,\n",
            "0.2,4,,cycles\n",
        ] {
            let input = format!("0.1,2,,cycles,1,\n{suffix}");
            assert!(
                super::parse_perf_stat(std::io::Cursor::new(input)).is_err(),
                "{suffix}"
            );
        }
        assert!(
            super::parse_perf_stat(std::io::Cursor::new(vec![b'x'; super::MAX_STAT_LINE + 1]))
                .is_err()
        );
        assert!(super::parse_perf_stat(std::io::Cursor::new(vec![0xff, b'\n'])).is_err());
        let input = "1,,cycles,1,\n".repeat(super::MAX_STAT_POINTS + 1);
        assert!(super::parse_perf_stat(std::io::Cursor::new(input)).is_err());
        struct Fails;
        impl std::io::Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture read failure"))
            }
        }
        assert!(super::parse_perf_stat(std::io::BufReader::new(Fails)).is_err());
    }

    #[test]
    fn scalar_perf_counters_sum_intervals_instead_of_reporting_the_final_sample() {
        let input = "0.100,12.34,msec,task-clock,100.00,\n0.200,20.00,msec,task-clock,100.00,\n0.100,8,,context-switches,100.00,\n0.200,2,,context-switches,100.00,\n0.200,<not supported>,,cycles,0,\n0.200,<not counted>,,instructions,0,\n";
        let metrics = super::parse_perf_stat_metrics(input);
        assert_eq!(metrics["perf.task_clock_ms"], 32.34);
        assert_eq!(metrics["perf.context_switches"], 10.0);
        assert!(!metrics.contains_key("perf.observed_cycles_count"));
        assert!(!metrics.contains_key("perf.observed_instructions_count"));
        for (name, values) in super::parse_perf_stat_series(input) {
            assert_eq!(metrics[&name], values.iter().sum::<f64>());
        }
        assert!(super::parse_perf_stat_metrics("unavailable\n").is_empty());
        assert!(
            super::parse_perf_stat_metrics("1e308,,cycles,100,\n1e308,,cycles,100,\n").is_empty()
        );
    }

    #[test]
    fn perf_record_is_root_attached_and_inherits_all_descendants() {
        assert_eq!(
            record_args(42),
            vec![
                "record",
                "-e",
                "cpu-clock",
                "-g",
                "-F",
                "199",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn offline_decoder_environment_preserves_home_but_removes_loader_injection() {
        let home =
            std::env::temp_dir().join(format!("asb-perf-decoder-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "test \"$HOME\" = \"$1\" && test -z \"${LD_PRELOAD+x}\"",
            "sh",
            home.to_str().unwrap(),
        ]);
        command.env("HOME", &home);
        command.env("LD_PRELOAD", "must-not-leak");
        super::configure_offline_decoder_environment(&mut command, &home);
        assert!(command.status().unwrap().success());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn perf_stat_uses_the_same_process_tree_scope() {
        assert_eq!(
            stat_args(42, 1000, &[]),
            vec![
                "stat",
                "--no-scale",
                "-I",
                "1000",
                "-x,",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }

    #[test]
    fn optional_platform_events_remain_on_the_process_tree_command() {
        let events = vec!["cycles".into(), "l1d_cache_refill".into()];
        let args = stat_args(42, 1000, &events);
        assert!(args.iter().any(|arg| arg
            == "task-clock,context-switches,cpu-migrations,page-faults,cycles,l1d_cache_refill"));
        assert!(args.windows(2).any(|pair| pair == ["-p", "42"]));
        assert!(!args.iter().any(|arg| arg == "-a"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attachment_rejects_asb_ancestors_and_unbound_process_generations() {
        use crate::measurements::ProcessIdentity;
        let runner = ProcessIdentity::read(std::process::id()).unwrap();
        assert!(
            super::validate_agent_target(runner.pid, Some(runner))
                .unwrap_err()
                .contains("includes ASB")
        );
        // Selecting ASB's parent would inherit ASB and its collectors too.
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let parent_pid = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let parent = ProcessIdentity::read(parent_pid).unwrap();
        assert!(
            super::validate_agent_target(parent.pid, Some(parent))
                .unwrap_err()
                .contains("includes ASB")
        );
        assert!(super::validate_agent_target(0, None).is_err());
        assert!(super::validate_agent_target(runner.pid, None).is_err());
        let stale = ProcessIdentity {
            start_time_ticks: runner.start_time_ticks + 1,
            ..runner
        };
        assert!(super::validate_agent_target(stale.pid, Some(stale)).is_err());
        assert!(super::validate_agent_target(runner.pid + 1, Some(runner)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attachment_accepts_a_separate_live_child_agent_root() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = crate::measurements::ProcessIdentity::read(child.id());
        let result = super::validate_agent_target(child.id(), identity);
        let _ = child.kill();
        child.wait().unwrap();
        result.unwrap();
    }

    #[test]
    fn arm_groups_never_expand_collection_to_all_cpus_or_other_processes() {
        let events = vec!["{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/}".into()];
        for args in [record_args(42), stat_args(42, 100, &events)] {
            assert_eq!(args.windows(2).filter(|pair| pair[0] == "-p").count(), 1);
            assert!(args.windows(2).any(|pair| pair == ["-p", "42"]));
            assert!(args.iter().any(|arg| arg == "--inherit"));
            assert!(!args.iter().any(|arg| matches!(
                arg.as_str(),
                "-a" | "--all-cpus" | "-C" | "--cpu" | "-G" | "--cgroup" | "--no-inherit"
            )));
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.contains("hisi_") || arg.contains("uncore"))
            );
        }
    }
}
