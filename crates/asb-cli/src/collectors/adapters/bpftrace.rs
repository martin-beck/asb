use super::super::{
    ArtifactDisplay, CollectorAdapter, GraphSection, PreflightContext, ProcessContext, ProcessData,
    SelectionExtension, SelectionOption,
};
use crate::measurements;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
mod custom_scope;
mod snapshots;
#[cfg(test)]
mod startup;
pub(super) struct BpftraceCollector;
pub(super) static ADAPTER: BpftraceCollector = BpftraceCollector;

impl CollectorAdapter for BpftraceCollector {
    #[cfg(any(target_os = "linux", test))]
    fn host_settings(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        &[(
            "kernel.sched_schedstats",
            "1",
            "bpftrace io-wait scheduler statistics",
        )]
    }
    fn estimated_disk_mib(&self) -> u64 {
        768
    }
    fn description(&self) -> &'static str {
        "selected process-tree eBPF programs"
    }
    fn id(&self) -> &'static str {
        "bpftrace"
    }
    fn accepts_selection(&self, selection: &str) -> bool {
        selection == self.id()
            || selection.strip_prefix("bpftrace:").is_some_and(|profile| {
                profile == "all"
                    || BPFTRACE_PROFILES
                        .iter()
                        .any(|candidate| candidate.name == profile)
            })
    }
    fn selection_extension(&self) -> Option<SelectionExtension> {
        Some(SelectionExtension {
            tab: "eBPF programs",
            description: "Choose the built-in eBPF programs to run with bpftrace.",
            all_selection: Some(format!("{}:all", self.id())),
            all_description: "all built-in eBPF programs",
            required: true,
            options: BPFTRACE_PROFILES
                .iter()
                .map(|profile| SelectionOption {
                    selection: format!("{}:{}", self.id(), profile.name),
                    label: profile.name,
                    description: profile.description,
                    default: profile.default_selection,
                })
                .collect(),
        })
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_BPFTRACE")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("bpftrace")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn prioritizes_artifact(&self, name: &str) -> bool {
        name.starts_with("bpftrace-")
    }
    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        (name.starts_with("bpftrace-") && name.ends_with(".svg")).then_some(ArtifactDisplay::Svg {
            class: "bpftrace-profile",
            label: "bpftrace profile SVG",
        })
    }
    fn graph_section(&self, name: &str) -> Option<GraphSection> {
        (name.starts_with("metric-bpftrace-") || name.starts_with("series-bpftrace-"))
        .then_some(GraphSection {
            title: "bpftrace profiles",
            description: "Cross-run SVG graphs for every bpftrace aggregate emitted by the selected profiles.",
        })
    }
    fn preflight(&self, context: &mut PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        _selected: &[String],
    ) -> Result<(), String> {
        if !session.state::<BpftraceState>().borrow().prepared {
            return Err("bpftrace preparation state is missing; validate the selected programs during collector preflight before configuring the session".into());
        }
        Ok(())
    }
    fn producer_policy(&self, _: &str) -> measurements::ProducerPolicy {
        measurements::ProducerPolicy {
            stream_bytes: 256 * 1024 * 1024,
            graceful: Some(std::time::Duration::from_secs(5)),
        }
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        session.attach_bpftrace(pid)
    }
    fn lifecycle_event(
        &self,
        session: &mut measurements::Session,
        event: super::super::LifecycleEvent,
    ) -> Result<(), String> {
        use super::super::LifecycleEvent;
        let state = session.state::<BpftraceState>();
        let mut state = state.borrow_mut();
        match event {
            LifecycleEvent::AgentStarted => {
                if state.execution_start_ns.is_none() {
                    state.execution_start_ns = snapshots::monotonic_ns();
                }
            }
            LifecycleEvent::AgentExecuteEnd
            | LifecycleEvent::AgentExited
            | LifecycleEvent::RunFailed => {
                if state.execution_end_ns.is_none() {
                    state.execution_end_ns = snapshots::monotonic_ns();
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn process(
    session: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    if let Some(profile) = context.name.strip_prefix("bpftrace-") {
        let output = super::super::read_complete_output(context.stdout_reader()?)?;
        result.metrics = parse_bpftrace_metrics(profile, &output);
        let bounds = {
            let state = session.state::<BpftraceState>();
            let state = state.borrow();
            state
                .execution_start_ns
                .zip(state.execution_end_ns)
                .filter(|(start, end)| end >= start)
        };
        let samples = snapshots::parse(profile, &output, bounds)?;
        result.series = samples.values;
        result.timestamps = samples.timestamps;
        measurements::write_json(
            &session.dir.join(format!("bpftrace-{profile}-timing.json")),
            &serde_json::json!({
                "schema_version":1,"collector":"bpftrace-snapshot-timing",
                "clock":"CLOCK_MONOTONIC; execution-relative milliseconds",
            "timing_status":if bounds.is_none() {"unavailable execution clock"} else if samples.groups==0 {"no timestamped snapshots; scalar aggregates only"} else if samples.accepted_groups==0 {"no snapshots overlap execution"} else if result.series.is_empty() {"no numeric timestamped observations"} else {"timestamped snapshots"},
                "snapshot_semantics":"Timestamp marks the kernel interval callback; bpftrace prints maps asynchronously, so map reads are not atomic counter snapshots. Missing keys remain gaps, never zero padding.",
                "complete_groups":samples.groups,
            "execution_groups":samples.accepted_groups,
            }),
        )?;
        super::super::validate_processed_output(&result)?;
        write_bpftrace_profile_svg(&session.dir, profile, &result.status, &result.metrics)?;
    }
    Ok(result)
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    let prepared = prepare_state(context.selected)?;
    let enabled = prepared.enabled;
    let profiles = prepared.profiles.clone();
    let custom = !prepared.custom.is_empty();
    *context.states.state::<BpftraceState>().borrow_mut() = prepared;
    if !custom && profiles.is_empty() {
        if enabled {
            return Err("bpftrace is enabled but no program or profile is configured. Select a profile, or set `ASB_BPFTRACE_PROFILES=all` and retry.".into());
        }
        return Ok(());
    }
    let executable = measurements::resolve_executable(context.tool_dir, "bpftrace");
    let args = ["-e", "BEGIN { exit(); }"];
    match Command::new(&executable)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(_) => {
            let sudo = Command::new("sudo")
                .args(["-n", "--"])
                .arg(&executable)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if sudo.is_ok_and(|status| status.success()) {
                context.sudo_collectors.insert("bpftrace".into());
            } else {
                return Err(measurements::passwordless_sudo_error(
                    "bpftrace",
                    &executable,
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("bpftrace is required for the selected collector. Run `asb tool install bpftrace` and retry.".into()),
        Err(error) => return Err(format!("cannot execute bpftrace at {}: {error}. Check parent-directory access and reinstall with `asb tool install bpftrace`.", executable.display())),
    }
    if !profiles.is_empty() && !executable.is_file() {
        return Err("selected bpftrace profiles need a project-local bpftrace executable. What to do: run `asb tool install bpftrace` in this project, then rerun the benchmark.".into());
    }
    for profile in profiles {
        if profile.name == "io-wait" {
            check_scheduler_statistics(
                &fs::read_to_string("/proc/sys/kernel/sched_schedstats").unwrap_or_default(),
            )?;
        }
        if bpftrace_profile_probe(&executable, profile, false)? {
            continue;
        }
        if bpftrace_profile_probe(&executable, profile, true)? {
            context.sudo_collectors.insert("bpftrace".into());
            continue;
        }
        if measurements::passwordless_sudo_available() {
            return Err(format!(
                "bpftrace profile `{}` cannot attach on this kernel. It is not a sudo authorization failure; inspect the kernel tracepoint support and the profile's JSON/SVG documentation before rerunning.",
                profile.name
            ));
        }
        return Err(measurements::passwordless_sudo_error(
            "bpftrace",
            &executable,
        ));
    }
    Ok(())
}

use crate::measurements::{Collector, env_true, escape_svg, parse_float, write_unavailable};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Clone, Default)]
struct BpftraceState {
    prepared: bool,
    enabled: bool,
    profiles: Vec<BpftraceProfile>,
    custom: Vec<PreparedCustomProgram>,
    execution_start_ns: Option<u64>,
    execution_end_ns: Option<u64>,
}

#[derive(Clone)]
struct PreparedCustomProgram {
    path: String,
    source: std::sync::Arc<str>,
}

fn custom_command(mut command: Command, program: &PreparedCustomProgram, pids: &[u32]) -> Command {
    // -p activates USDT probes; the injected map, not -p, proves event scope.
    for pid in pids {
        command.args(["-p", &pid.to_string()]);
    }
    let source = format!(
        "{}\n{}",
        process_tree_prelude_from_pids(pids.iter().copied()),
        program.source
    );
    command.args(["-B", "line", "-e", &source]);
    command
}

fn prepare_state(selected: &[String]) -> Result<BpftraceState, String> {
    let enabled = super::super::selected_or_environment("bpftrace", selected);
    let mut custom = Vec::new();
    let mut total = 0usize;
    if enabled && std::env::var_os("ASB_BPFTRACE_PROGRAMS").is_some() {
        for (path, parameters) in custom_programs()? {
            let source = custom_scope::read(Path::new(&path), &parameters).map_err(|error| {
                format!("{error}. Check the program path and parameters in ASB_BPFTRACE_PROGRAMS, then retry")
            })?;
            total += source.len();
            if total > 1024 * 1024 {
                return Err("custom bpftrace sources exceed the 1 MiB combined limit".into());
            }
            custom.push(PreparedCustomProgram {
                path,
                source: source.into(),
            });
        }
        if custom.is_empty() {
            return Err("ASB_BPFTRACE_PROGRAMS is empty; name a scoped program or unset it to select built-in profiles".into());
        }
    }
    Ok(BpftraceState {
        prepared: true,
        enabled,
        profiles: selected_bpftrace_profiles(selected)?,
        custom,
        execution_start_ns: None,
        execution_end_ns: None,
    })
}

impl measurements::Session {
    pub(crate) fn attach_bpftrace(&mut self, pid: u32) -> Result<(), String> {
        if !self.state::<BpftraceState>().borrow().prepared {
            return Err("bpftrace preparation state is missing; collector attachment cannot re-read program files".into());
        }
        if self.state::<BpftraceState>().borrow().enabled {
            self.start_bpftrace(pid)?;
        }
        self.start_bpftrace_profiles(pid)
    }

    fn start_bpftrace(&mut self, pid: u32) -> Result<(), String> {
        let programs = self.state::<BpftraceState>().borrow().custom.clone();
        for (index, program) in programs.into_iter().enumerate() {
            let (command, route) = self.collector_command("bpftrace").map_err(|error| {
                format!("cannot start bpftrace program {}: {error}", program.path)
            })?;
            let command = custom_command(command, &program, &measurements::process_tree_pids(pid));
            let stdout_path = self.dir.join(format!("bpftrace-{index}.stdout"));
            let stderr_path = self.dir.join(format!("bpftrace-{index}.stderr"));
            let captures = [
                crate::evidence::Capture::create(&stdout_path)?,
                crate::evidence::Capture::create(&stderr_path)?,
            ];
            let mut collector = Collector::spawn(
                &format!("bpftrace-{index}"),
                &ADAPTER,
                command,
                route,
                captures,
            )?;
            if let Err(error) = wait_for_profile_ready(
                || collector.producer.poll(),
                &collector.captures[0],
                &collector.captures[1],
                std::time::Duration::from_secs(30),
            ) {
                return Err(collector.startup_failure(
                    format!(
                        "bpftrace program {} did not become ready: {error}",
                        program.path
                    ),
                    self._directory_owner.clone(),
                ));
            }
            self.collectors.push(collector);
        }
        Ok(())
    }

    /// Start each selected built-in profile after the agent PID is known. This
    /// keeps all probes scoped to the benchmarked agent rather than the host.
    fn start_bpftrace_profiles(&mut self, pid: u32) -> Result<(), String> {
        let profiles = self.state::<BpftraceState>().borrow().profiles.clone();
        let prelude = process_tree_prelude(pid);
        for profile in &profiles {
            let name = format!("bpftrace-{}", profile.name);
            let stdout_path = self.dir.join(format!("{name}.stdout"));
            let stderr_path = self.dir.join(format!("{name}.stderr"));
            let captures = [
                crate::evidence::Capture::create(&stdout_path)?,
                crate::evidence::Capture::create(&stderr_path)?,
            ];
            let admitted = self
                .collector_command("bpftrace")
                .and_then(|(mut command, route)| {
                    command
                        .args(["-B", "line"])
                        .args(["-e", &format!("{prelude}\n{}", profile.program)])
                        .arg(pid.to_string());
                    Collector::spawn(&name, &ADAPTER, command, route, captures)
                });
            match admitted {
                Ok(mut collector) => {
                    if let Err(error) = wait_for_profile_ready(
                        || collector.producer.poll(),
                        &collector.captures[0],
                        &collector.captures[1],
                        std::time::Duration::from_secs(30),
                    ) {
                        return Err(collector.startup_failure(
                            format!(
                                "bpftrace profile {} did not become ready: {error}",
                                profile.name
                            ),
                            self._directory_owner.clone(),
                        ));
                    }
                    self.collectors.push(collector);
                }
                Err(error) if !env_true("ASB_MEASURE_BPFTRACE") => write_unavailable(
                    &self.dir.join(format!("bpftrace-{}.json", profile.name)),
                    &format!("bpftrace-{}", profile.name),
                    format!("{}; profile: {}", error, profile.description),
                )?,
                Err(error) => {
                    return Err(format!(
                        "cannot start bpftrace profile {}: {error}",
                        profile.name
                    ));
                }
            }
        }
        Ok(())
    }
}

const MAX_CUSTOM_PROGRAM: u64 = 256 * 1024;

#[cfg(test)]
fn validate_custom_program_scope(path: &Path) -> Result<(), String> {
    custom_scope::read(path, &[]).map(|_| ())
}

#[cfg(test)]
fn scoped_custom_program(path: &Path, root: u32) -> Result<String, String> {
    let source = custom_scope::read(path, &[])?;
    Ok(format!("{}\n{}", process_tree_prelude(root), source))
}

type CustomProgram = (String, Vec<(String, String)>);

fn custom_programs() -> Result<Vec<CustomProgram>, String> {
    let specs = std::env::var("ASB_BPFTRACE_PROGRAMS")
        .map_err(|_| "ASB_MEASURE_BPFTRACE=1 requires ASB_BPFTRACE_PROGRAMS".to_owned())?;
    parse_custom_programs(&specs)
}

fn parse_custom_programs(specs: &str) -> Result<Vec<CustomProgram>, String> {
    if specs.len() > 16 * 1024 {
        return Err("ASB_BPFTRACE_PROGRAMS exceeds the 16 KiB specification limit".into());
    }
    let mut programs = Vec::new();
    for spec in specs.split(';').filter(|s| !s.trim().is_empty()) {
        let (program, params) = spec.split_once('?').map_or((spec, ""), |parts| parts);
        let mut parsed = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for parameter in params.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = parameter.split_once('=').ok_or_else(|| {
                "invalid bpftrace parameter; use PROGRAM?ASB_PARAM_LIMIT=12".to_owned()
            })?;
            custom_scope::parameter(key, value)?;
            if !names.insert(key.to_owned()) {
                return Err(format!(
                    "duplicate bpftrace parameter {key}; specify each parameter once"
                ));
            }
            parsed.push((key.into(), value.into()));
            if parsed.len() > 32 {
                return Err(
                    "a custom bpftrace program supports at most 32 numeric parameters".into(),
                );
            }
        }
        if program.trim().is_empty() {
            return Err("custom bpftrace program path is empty".into());
        }
        programs.push((program.into(), parsed));
        if programs.len() > 8 {
            return Err("ASB_BPFTRACE_PROGRAMS supports at most 8 custom programs".into());
        }
    }
    Ok(programs)
}

/// Seed the built-in probes with every process currently below the agent and
/// keep the set current as the agent forks helpers. This prevents unrelated
/// host tracepoints from entering a process-scoped measurement.
fn process_tree_prelude(root: u32) -> String {
    process_tree_prelude_from_pids(measurements::process_tree_pids(root))
}

fn process_tree_prelude_from_pids(pids: impl IntoIterator<Item = u32>) -> String {
    let mut prelude = String::from("BEGIN {\n");
    for pid in pids {
        let _ = writeln!(prelude, "  @asb_tree[{pid}] = 1;");
    }
    prelude.push_str(
        "  printf(\"ASB_BPFTRACE_READY\\n\");\n}\ntracepoint:sched:sched_process_fork /@asb_tree[args->parent_pid]/ { @asb_tree[args->child_pid] = 1; }\ntracepoint:sched:sched_process_exit /@asb_tree[tid]/ { delete(@asb_tree[tid]); }\n",
    );
    prelude
}

fn wait_for_profile_ready(
    mut poll: impl FnMut() -> Result<Option<measurements::ProducerCompletion>, String>,
    stdout: &crate::evidence::Capture,
    stderr: &crate::evidence::Capture,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(completion) = poll()? {
            let diagnostic = stderr.startup_diagnostic(4096)?;
            return Err(format!(
                "collector exited before attachment acknowledgement: {:?}; {}; {diagnostic}",
                completion.exit,
                completion.error.unwrap_or_default()
            ));
        }
        let text = stdout.startup_diagnostic(4096)?;
        if text.lines().any(|line| line == "ASB_BPFTRACE_READY") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for attachment acknowledgement".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A bounded, target-PID-scoped bpftrace program. Profiles use only stable
/// kernel tracepoint families where possible; users can still supply a
/// runtime-specific program through ASB_BPFTRACE_PROGRAMS.
#[derive(Clone, Copy)]
pub(crate) struct BpftraceProfile {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) program: &'static str,
    pub(crate) default_selection: bool,
}

const BPFTRACE_PROFILES: &[BpftraceProfile] = &[
    BpftraceProfile {
        name: "sched-latency",
        description: "wakeup-to-run latency for agent tasks",
        program: include_str!("bpftrace/sched_latency.bt"),
        default_selection: true,
    },
    BpftraceProfile {
        name: "cpu-migrations",
        description: "agent task migrations between CPUs",
        program: include_str!("bpftrace/cpu_migrations.bt"),
        default_selection: true,
    },
    BpftraceProfile {
        name: "futex-wait",
        description: "futex wait count and duration for agent threads",
        program: include_str!("bpftrace/futex_wait.bt"),
        default_selection: true,
    },
    BpftraceProfile {
        name: "syscall-rate",
        description: "aggregate system-call pressure from the agent process",
        program: include_str!("bpftrace/syscall_rate.bt"),
        default_selection: true,
    },
    BpftraceProfile {
        name: "lock-contention",
        description: "kernel lock contention for agent threads when supported",
        program: include_str!("bpftrace/lock_contention.bt"),
        default_selection: true,
    },
    BpftraceProfile {
        name: "socket-operations",
        description: "elapsed connect/recv socket syscall time, including CPU work (overlaps scheduler states)",
        program: include_str!("bpftrace/socket_operations.bt"),
        default_selection: false,
    },
    BpftraceProfile {
        name: "io-wait",
        description: "completed kernel-accounted I/O waits; requires enabled scheduler statistics",
        program: include_str!("bpftrace/io_wait.bt"),
        default_selection: false,
    },
];

/// Names accepted by `bpftrace:PROFILE` collector selections and
/// `ASB_BPFTRACE_PROFILES=PROFILE,...`.
pub fn bpftrace_profile_names() -> impl Iterator<Item = &'static str> {
    BPFTRACE_PROFILES.iter().map(|profile| profile.name)
}

pub(crate) fn selected_bpftrace_profiles(
    selected: &[String],
) -> Result<Vec<BpftraceProfile>, String> {
    let mut requested = selected
        .iter()
        .filter_map(|value| value.strip_prefix("bpftrace:"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Ok(value) = std::env::var("ASB_BPFTRACE_PROFILES") {
        requested.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
    }
    if selected.iter().any(|value| value == "bpftrace") && requested.is_empty() {
        requested.extend(
            BPFTRACE_PROFILES
                .iter()
                .filter(|profile| profile.default_selection)
                .map(|profile| profile.name.to_owned()),
        );
    }
    if requested.iter().any(|value| value == "all") {
        return Ok(BPFTRACE_PROFILES.to_vec());
    }
    requested.sort();
    requested.dedup();
    requested
        .iter()
        .map(|name| {
            BPFTRACE_PROFILES
                .iter()
                .find(|profile| profile.name == name)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "unknown bpftrace profile `{name}`; choose one of: {}",
                        bpftrace_profile_names().collect::<Vec<_>>().join(", ")
                    )
                })
        })
        .collect()
}

fn check_scheduler_statistics(value: &str) -> Result<(), String> {
    if value.trim() == "1" {
        return Ok(());
    }
    Err("bpftrace profile `io-wait` needs enabled scheduler statistics. If the host supports CONFIG_SCHEDSTATS, enable it with `sudo sysctl -w kernel.sched_schedstats=1`, then retry. Otherwise deselect `bpftrace:io-wait`; no I/O wait is inferred from generic sleep.".into())
}

/// Compile and attach a concrete profile before the agent starts. The appended
/// interval exit keeps the probe bounded while exercising the exact tracepoints,
/// predicates, and process-tree parameter form used during the run.
pub(crate) fn bpftrace_profile_probe(
    executable: &Path,
    profile: BpftraceProfile,
    sudo: bool,
) -> Result<bool, String> {
    // Compile the same process-tree prelude used by the live collector. The
    // preflight must not reject a valid profile merely because its tree map is
    // populated by the runtime attach path.
    let program = profile_probe_program(profile);
    let mut command = if sudo {
        let mut command = Command::new("sudo");
        command.args(["-n", "--"]);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command
        .args(["-e", &program, "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|error| format!("cannot probe bpftrace profile {}: {error}", profile.name))
}

pub(crate) fn write_bpftrace_profile_svg(
    dir: &Path,
    profile: &str,
    status: &str,
    metrics: &BTreeMap<String, f64>,
) -> Result<(), String> {
    let mut lines = vec![format!("status: {status}")];
    if let Some(profile) = BPFTRACE_PROFILES
        .iter()
        .find(|candidate| candidate.name == profile)
    {
        let mut line = String::new();
        for word in profile.description.split_whitespace() {
            if line.len() + word.len() + 1 > 70 {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    for (name, value) in metrics {
        lines.push(format!(
            "{}: {:.0}",
            name.rsplit('.').next().unwrap_or(name),
            value
        ));
    }
    if metrics.is_empty() {
        lines.push("no aggregate values emitted; inspect the paired JSON result".into());
    }
    let height = 76 + lines.len() * 26;
    let description = BPFTRACE_PROFILES
        .iter()
        .find(|candidate| candidate.name == profile)
        .map_or("Custom process-tree profile", |profile| profile.description);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"760\" height=\"{height}\" viewBox=\"0 0 760 {height}\"><title>bpftrace {}</title><desc>{}</desc><rect width=\"760\" height=\"{height}\" fill=\"#101827\"/><rect x=\"16\" y=\"16\" width=\"728\" height=\"{}\" rx=\"10\" fill=\"#182334\"/><text x=\"36\" y=\"48\" fill=\"#e5edf8\" font-family=\"sans-serif\" font-size=\"20\" font-weight=\"bold\">bpftrace: {}</text>",
        escape_svg(profile),
        escape_svg(description),
        height - 32,
        escape_svg(profile)
    );
    for (index, line) in lines.iter().enumerate() {
        let color = if index == 0 && status == "completed" {
            "#78dba9"
        } else if index == 0 {
            "#ffb86b"
        } else {
            "#b9c7dc"
        };
        svg.push_str(&format!(
            "<text x=\"36\" y=\"{}\" fill=\"{color}\" font-family=\"monospace\" font-size=\"15\">{}</text>",
            76 + index * 26,
            escape_svg(line)
        ));
    }
    svg.push_str("</svg>\n");
    fs::write(dir.join(format!("bpftrace-{profile}.svg")), svg)
        .map_err(|error| format!("cannot write bpftrace profile SVG: {error}"))
}

fn profile_probe_program(profile: BpftraceProfile) -> String {
    format!(
        "{}\n{}\ninterval:ms:1 {{ exit(); }}",
        // PID 0 is only a compile-time seed. Expanding its live process tree
        // would accidentally scope the preflight to the host init tree.
        process_tree_prelude_from_pids([0]),
        profile.program
    )
}

/// bpftrace prints aggregation maps as `@asb_metric: value`. Only these
/// ASB-owned map names are accepted, so arbitrary program output cannot create
/// comparison metrics accidentally.
pub(crate) fn parse_bpftrace_metrics(profile: &str, output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    for line in output.lines() {
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        let Some(name) = name.trim().strip_prefix("@asb_") else {
            continue;
        };
        // @asb_tree is the internal PID membership map used to keep forked
        // helpers in scope. It is control state, never a benchmark metric.
        if name == "tree" || name.starts_with("tree[") {
            continue;
        }
        let Some(value) = parse_float(value) else {
            continue;
        };
        metrics.insert(format!("bpftrace.{profile}.{name}"), value);
    }
    metrics
}

#[cfg(test)]
pub(crate) fn parse_bpftrace_series(profile: &str, output: &str) -> BTreeMap<String, Vec<f64>> {
    let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in output.lines() {
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        let Some(name) = name.trim().strip_prefix("@asb_") else {
            continue;
        };
        if name == "tree" || name.starts_with("tree[") {
            continue;
        }
        let Some(value) = parse_float(value) else {
            continue;
        };
        series
            .entry(format!("bpftrace.{profile}.{name}"))
            .or_default()
            .push(value);
    }
    series
}

#[cfg(test)]
#[path = "../../../tests/unit/bpftrace_capture.rs"]
mod capture_tests;

#[cfg(test)]
#[path = "bpftrace/tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::{BPFTRACE_PROFILES, profile_probe_program, validate_custom_program_scope};

    #[test]
    fn svg_renders_each_metric_and_escapes_status() {
        let dir = std::env::temp_dir().join(format!("asb-bpf-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let metrics = std::collections::BTreeMap::from([
            ("sched.total".to_owned(), 7.0),
            ("sched.max".to_owned(), 3.0),
        ]);
        super::write_bpftrace_profile_svg(&dir, "sched_latency", "failure <mock>", &metrics)
            .unwrap();
        let svg = std::fs::read_to_string(dir.join("bpftrace-sched_latency.svg")).unwrap();
        assert!(svg.contains("failure &lt;mock&gt;"));
        assert!(svg.contains("total: 7"));
        assert!(svg.contains("max: 3"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn svg_write_errors_report_the_artifact_type() {
        let absent = std::env::temp_dir().join(format!(
            "asb-no-such-bpftrace-metric-dir-{}",
            std::process::id()
        ));
        assert!(
            super::write_bpftrace_profile_svg(&absent, "sched", "error", &Default::default())
                .unwrap_err()
                .contains("bpftrace profile SVG")
        );
    }

    #[test]
    fn profiles_are_target_scoped_and_parse_only_asb_metrics() {
        assert_eq!(
            super::bpftrace_profile_names().collect::<Vec<_>>(),
            vec![
                "sched-latency",
                "cpu-migrations",
                "futex-wait",
                "syscall-rate",
                "lock-contention",
                "socket-operations",
                "io-wait"
            ]
        );
        let profiles = super::selected_bpftrace_profiles(&[
            "bpftrace:sched-latency".into(),
            "bpftrace:futex-wait".into(),
        ])
        .expect("profile selection");
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            super::selected_bpftrace_profiles(&["bpftrace".into()])
                .expect("generic bpftrace selects built-ins")
                .len(),
            super::bpftrace_profile_names().count() - 2
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("@asb_tree"))
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("interval:s:1"))
        );
        let metrics = super::parse_bpftrace_metrics(
            "sched-latency",
            "@asb_tree[123]: 1\n@asb_wakeup_latency_events: 7\n@asb_wakeup_latency_total_ns: 420\n@other: 99\n",
        );
        assert_eq!(metrics["bpftrace.sched-latency.wakeup_latency_events"], 7.0);
        assert_eq!(
            metrics["bpftrace.sched-latency.wakeup_latency_total_ns"],
            420.0
        );
        assert_eq!(metrics.len(), 2);
        let series = super::parse_bpftrace_series(
            "sched-latency",
            "@asb_wakeup_latency_events: 2\n@asb_wakeup_latency_events: 5\n",
        );
        assert_eq!(
            series["bpftrace.sched-latency.wakeup_latency_events"],
            vec![2.0, 5.0]
        );
    }

    #[test]
    fn bpf_processing_includes_final_cumulative_maps_beyond_the_diagnostic_cap() {
        let mut input = "@asb_syscalls: 2\n".to_owned();
        input.push_str(&"# ignored fixture filler\n".repeat(15000));
        input.push_str("@asb_syscalls: 900\n");
        assert!(input.len() > 256 * 1024);
        let result = crate::collectors::process_output_fixture(
            &super::ADAPTER,
            "bpftrace-syscall-rate",
            &input,
        )
        .unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.metrics["bpftrace.syscall-rate.syscalls"], 900.0);
        assert!(
            result.series.is_empty(),
            "untimed maps must not invent a clock series"
        );
        assert!(
            crate::collectors::process_output_fixture(
                &super::ADAPTER,
                "bpftrace-syscall-rate",
                "@asb_syscalls: 9"
            )
            .is_err()
        );
    }

    #[test]
    fn optional_wait_profiles_have_explicit_semantics_and_requirements() {
        let socket = BPFTRACE_PROFILES
            .iter()
            .find(|p| p.name == "socket-operations")
            .unwrap();
        assert!(!socket.default_selection);
        assert!(socket.description.contains("overlaps scheduler states"));
        let io = BPFTRACE_PROFILES
            .iter()
            .find(|p| p.name == "io-wait")
            .unwrap();
        assert!(!io.default_selection);
        assert!(super::check_scheduler_statistics("1\n").is_ok());
        for input in ["", "0", "unavailable"] {
            let error = super::check_scheduler_statistics(input).unwrap_err();
            assert!(error.contains("sudo sysctl -w kernel.sched_schedstats=1"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires ASB_TEST_BPFTRACE and host tracepoint privileges; PID-zero compile/attach probes only"]
    fn live_optional_wait_profiles_compile_and_attach() {
        let executable = std::env::var_os("ASB_TEST_BPFTRACE").expect("set ASB_TEST_BPFTRACE");
        for profile in BPFTRACE_PROFILES
            .iter()
            .filter(|profile| !profile.default_selection)
        {
            assert!(
                super::bpftrace_profile_probe(std::path::Path::new(&executable), *profile, false)
                    .unwrap()
                    || super::bpftrace_profile_probe(
                        std::path::Path::new(&executable),
                        *profile,
                        true
                    )
                    .unwrap(),
                "profile {} must compile and attach on this host",
                profile.name
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn readiness_failure_reports_startup_diagnostics_and_timeout() {
        use std::{fs, process::Command, time::Duration};
        let dir = std::env::temp_dir().join(format!("asb-bpf-ready-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let stdout = dir.join("stdout");
        let stderr = dir.join("stderr");
        fs::write(&stdout, "").unwrap();
        fs::write(&stderr, "missing kernel tracepoint").unwrap();
        let stdout_capture = crate::evidence::Capture::open(&stdout).unwrap();
        let stderr_capture = crate::evidence::Capture::open(&stderr).unwrap();
        let mut failed = Command::new("sh").args(["-c", "exit 2"]).spawn().unwrap();
        let error = super::wait_for_profile_ready(
            || {
                failed
                    .try_wait()
                    .map(|status| {
                        status.map(crate::measurements::ProducerCompletion::test_completed)
                    })
                    .map_err(|error| error.to_string())
            },
            &stdout_capture,
            &stderr_capture,
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.contains("missing kernel tracepoint"));
        let _ = failed.wait();
        let mut waiting = Command::new("sleep").arg("10").spawn().unwrap();
        assert!(
            super::wait_for_profile_ready(
                || waiting
                    .try_wait()
                    .map(|status| status
                        .map(crate::measurements::ProducerCompletion::test_completed))
                    .map_err(|error| error.to_string()),
                &stdout_capture,
                &stderr_capture,
                Duration::ZERO
            )
            .unwrap_err()
            .contains("timed out")
        );
        fs::write(&stdout, "ASB_BPFTRACE_READY\n").unwrap();
        assert!(
            super::wait_for_profile_ready(
                || waiting
                    .try_wait()
                    .map(|status| status
                        .map(crate::measurements::ProducerCompletion::test_completed))
                    .map_err(|error| error.to_string()),
                &stdout_capture,
                &stderr_capture,
                Duration::ZERO
            )
            .is_ok()
        );
        let _ = waiting.kill();
        let _ = waiting.wait();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn profile_preflight_uses_the_same_seeded_tree_program_as_runtime() {
        let profile = BPFTRACE_PROFILES
            .iter()
            .find(|profile| profile.name == "sched-latency")
            .copied()
            .expect("built-in profile");
        let program = profile_probe_program(profile);
        assert!(program.contains("@asb_tree[0] = 1;"));
        assert!(program.contains("sched_process_fork"));
        assert!(
            program.contains("sched_process_exit /@asb_tree[tid]/ { delete(@asb_tree[tid]); }")
        );
        assert!(program.contains(profile.program));
        assert!(program.contains("interval:ms:1 { exit(); }"));
        assert!(!program.contains("@asb_tree[1] = 1;"));
        assert!(!program.contains("BEGIN { exit(); }"));
    }

    #[test]
    fn custom_programs_must_declare_the_process_tree_predicate() {
        let root = std::env::temp_dir().join(format!(
            "asb-bpftrace-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(
            &root,
            "tracepoint:syscalls:sys_enter_openat { @host = count(); }\n",
        )
        .expect("write fixture");
        let error = validate_custom_program_scope(&root).expect_err("host-wide program");
        assert!(error.contains("process-tree scoped"));
        std::fs::write(
            &root,
            "tracepoint:syscalls:sys_enter_openat /@asb_tree[pid]/ { @asb_open = count(); }\n",
        )
        .expect("write scoped fixture");
        validate_custom_program_scope(&root).expect("scoped program");
        std::fs::write(
            &root,
            "// @asb_tree[pid]\ntracepoint:syscalls:sys_enter_openat { @host = count(); }\n",
        )
        .expect("write comment-only fixture");
        validate_custom_program_scope(&root).expect_err("comment must not scope a probe");
        std::fs::write(
            &root,
            "tracepoint:syscalls:sys_enter_openat /@asb_tree[pid]/ { @host = count(); }\ntracepoint:sched:sched_switch { @host = count(); }\n",
        )
        .expect("write mixed fixture");
        validate_custom_program_scope(&root).expect_err("mixed probes must fail closed");
        std::fs::remove_file(root).expect("remove fixture");
    }
}
