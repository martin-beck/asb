//! Sample only the selected agent entry point, not every descendant or host task.
use crate::collectors::{CollectorAdapter, GraphSection, LifecycleEvent};
use crate::measurements::{self, Session};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) struct AgentProcesses<const MEMORY: bool>;

#[derive(Default)]
struct State<const MEMORY: bool> {
    selected: bool,
    memory: bool,
    workers: bool,
    identity: Option<Identity>,
    sampler: Option<Sampler>,
    totals: Totals,
    isolated: bool,
    imported: Option<NamespaceSample>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct NamespaceSample {
    schema_version: u32,
    identity_available: bool,
    distinct_processes: u64,
    peak_rss_kib: u64,
    samples: u64,
    incomplete: bool,
    sampling_interval_ms: u64,
}

#[derive(Clone)]
struct Identity {
    executable: PathBuf,
    interpreter: Option<PathBuf>,
    runtime_arguments: Option<(String, String)>,
}

#[derive(Default)]
struct Totals {
    seen: BTreeSet<(u32, u64)>,
    admitted: BTreeSet<(u32, u64)>,
    declared: BTreeSet<(u32, u64)>,
    peak_rss_kib: u64,
    samples: u64,
    incomplete: bool,
}

struct Sampler {
    control: mpsc::Sender<Control>,
    thread: Option<JoinHandle<Totals>>,
}

enum Control {
    Admit((u32, u64)),
    Stop,
}

impl Sampler {
    fn finish(mut self) -> Totals {
        let _ = self.control.send(Control::Stop);
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_else(|| Totals {
                incomplete: true,
                ..Totals::default()
            })
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl<const MEMORY: bool> CollectorAdapter for AgentProcesses<MEMORY> {
    fn description(&self) -> &'static str {
        if MEMORY {
            "selected agent process-tree memory usage"
        } else {
            "selected agent process-tree worker census"
        }
    }
    fn prefers_execution_namespace(&self) -> bool {
        true
    }

    fn execution_namespace_requested(&self, session: &mut Session) -> bool {
        let state = session.state::<State<MEMORY>>();
        let state = state.borrow();
        if MEMORY { state.memory } else { state.workers }
    }

    fn export_execution_namespace_state(
        &self,
        session: &mut Session,
    ) -> Result<serde_json::Value, String> {
        let state = session.state::<State<MEMORY>>();
        let state = state.borrow();
        serde_json::to_value(NamespaceSample {
            schema_version: 1,
            identity_available: state.identity.is_some(),
            distinct_processes: state.totals.seen.len() as u64,
            peak_rss_kib: state.totals.peak_rss_kib,
            samples: state.totals.samples,
            incomplete: state.totals.incomplete,
            sampling_interval_ms: measurements::measurement_interval_ms(),
        })
        .map_err(|_| "cannot encode agent-process observations".into())
    }

    fn import_execution_namespace_state(
        &self,
        session: &mut Session,
        value: &serde_json::Value,
    ) -> Result<(), String> {
        let sample: NamespaceSample = serde_json::from_value(value.clone())
            .map_err(|_| "invalid execution-namespace agent-process observations")?;
        if sample.schema_version != 1
            || !(100..=60000).contains(&sample.sampling_interval_ms)
            || sample.distinct_processes > 100000
            || (sample.samples == 0 && sample.distinct_processes != 0)
            || (!sample.identity_available && sample.distinct_processes != 0)
        {
            return Err("inconsistent execution-namespace agent-process observations".into());
        }
        session.state::<State<MEMORY>>().borrow_mut().imported = Some(sample);
        Ok(())
    }

    fn attach_isolated_pid(&self, session: &mut Session, _pid: u32) -> Result<(), String> {
        // The execution namespace owns entry-point resolution and proc access.
        // Do not guess container paths or escalate host /proc permissions.
        session.state::<State<MEMORY>>().borrow_mut().isolated = true;
        Ok(())
    }
    fn id(&self) -> &'static str {
        if MEMORY {
            "agent-memory"
        } else {
            "agent-workers"
        }
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some(if MEMORY {
            "ASB_MEASURE_AGENT_MEMORY"
        } else {
            "ASB_MEASURE_AGENT_WORKERS"
        })
    }
    fn requires_pid(&self) -> bool {
        true
    }

    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        if crate::collectors::selected_or_environment(self.id(), selected) {
            let state = session.state::<State<MEMORY>>();
            let mut state = state.borrow_mut();
            state.selected = true;
            if MEMORY {
                state.memory = true;
            } else {
                state.workers = true;
            }
        }
        Ok(())
    }

    fn agent_launch(&self, session: &mut Session, program: &str, args: &[String]) {
        let state = session.state::<State<MEMORY>>();
        let mut state = state.borrow_mut();
        if state.selected {
            state.identity = Identity::resolve(program, args);
        }
    }

    fn agent_launch_prepared(&self, session: &mut Session, target: &crate::launch::TargetLaunch) {
        let state = session.state::<State<MEMORY>>();
        let mut state = state.borrow_mut();
        if state.selected {
            // env_clear is intentionally opaque in std::Command. A bare
            // executable with no explicit PATH has no proven identity here.
            // Execution still uses std's real child environment unchanged.
            let args = target
                .args
                .iter()
                .map(|arg| arg.to_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>();
            state.identity = target.program.to_str().and_then(|program| {
                if Path::new(program).components().count() == 1 && target.path.is_none() {
                    return None;
                }
                Identity::resolve_in(
                    program,
                    args.as_deref()?,
                    target.path.as_deref(),
                    target.cwd.as_deref(),
                )
            });
        }
    }

    fn attach_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        let root = session
            .measurement_target()
            .or_else(|| measurements::ProcessIdentity::read(pid));
        let state = session.state::<State<MEMORY>>();
        let mut state = state.borrow_mut();
        if state.selected
            && state.sampler.is_none()
            && cfg!(target_os = "linux")
            && let Some(root) = root
            && let Some(identity) = state.identity.clone()
        {
            let (control, receiver) = mpsc::channel();
            let interval = Duration::from_millis(measurements::measurement_interval_ms());
            state.sampler = Some(Sampler {
                control,
                thread: Some(thread::spawn(move || {
                    let mut totals = Totals::default();
                    loop {
                        if !root.matches() {
                            return totals;
                        }
                        totals.sample(root, &identity, MEMORY);
                        match receiver.recv_timeout(interval) {
                            Err(mpsc::RecvTimeoutError::Timeout) => (),
                            Ok(Control::Admit(process)) => {
                                totals.declared.insert(process);
                            }
                            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                return totals;
                            }
                        }
                    }
                })),
            });
        }
        Ok(())
    }

    fn agent_process_spawned(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        let process = measurements::ProcessIdentity::read(pid)
            .ok_or_else(|| "cannot identify the direct agent process".to_owned())?;
        let state = session.state::<State<MEMORY>>();
        let state = state.borrow();
        if state.selected
            && state.identity.is_some()
            && let Some(sampler) = &state.sampler
        {
            // `pid` is returned by Command::spawn for the exact launch that
            // the collector resolved before execution.  Keep that provenance
            // through clients such as Pi that change argv after startup, but
            // never grant the same exemption to subsequently discovered tools.
            sampler
                .control
                .send(Control::Admit((process.pid, process.start_time_ticks)))
                .map_err(|_| "agent-process sampler stopped before launch".to_owned())?;
        }
        Ok(())
    }

    fn lifecycle_event(&self, session: &mut Session, event: LifecycleEvent) -> Result<(), String> {
        if matches!(
            event,
            LifecycleEvent::AgentExecuteEnd | LifecycleEvent::RunFailed
        ) {
            let state = session.state::<State<MEMORY>>();
            let mut state = state.borrow_mut();
            if let Some(sampler) = state.sampler.take() {
                state.totals = sampler.finish();
            }
        }
        Ok(())
    }

    fn finish(&self, session: &mut Session) -> Result<(), String> {
        self.lifecycle_event(session, LifecycleEvent::AgentExecuteEnd)?;
        let state = session.state::<State<MEMORY>>();
        let state = state.borrow();
        if (MEMORY && !state.memory) || (!MEMORY && !state.workers) {
            return Ok(());
        }
        let (seen, samples, peak_rss_kib, incomplete, identity_available) =
            match state.imported.as_ref() {
                Some(sample) => (
                    sample.distinct_processes,
                    sample.samples,
                    sample.peak_rss_kib,
                    sample.incomplete,
                    sample.identity_available,
                ),
                None => (
                    state.totals.seen.len() as u64,
                    state.totals.samples,
                    state.totals.peak_rss_kib,
                    state.totals.incomplete,
                    state.identity.is_some(),
                ),
            };
        let available = samples > 0 && seen > 0;
        let reason = if !cfg!(target_os = "linux") {
            Some("Agent process sampling requires Linux /proc.")
        } else if state.isolated && state.imported.is_none() {
            Some("Isolated execution did not return the requested agent-process observations.")
        } else if !identity_available {
            Some("Cannot resolve the selected agent executable.")
        } else if !available {
            Some(
                "No matching agent process was observed; the run may be shorter than the sampling interval or use a different launcher entry point.",
            )
        } else if incomplete {
            Some(
                "Agent process accounting was incomplete (unreadable process data or observation limit); metrics are omitted.",
            )
        } else {
            None
        };
        let metrics = if !available || incomplete {
            serde_json::json!({})
        } else if MEMORY {
            serde_json::json!({ "peak_rss_mib": peak_rss_kib as f64 / 1024.0 })
        } else {
            serde_json::json!({ "spawned_worker_count": seen.saturating_sub(1) })
        };
        measurements::write_json(
            &session.dir.join(format!("{}.json", self.id())),
            &serde_json::json!({
                "schema_version": 1, "collector": self.id(),
                "status": if incomplete { "incomplete" } else if available { "observed" } else { "unavailable" },
                "reason": reason,
                "source": if state.isolated { "execution-namespace-linux-proc-sampling" } else { "linux-proc-sampling" }, "scope": if MEMORY { "selected-agent-process-tree" } else { "selected-agent-entry-point-only" },
                "sampling_interval_ms": state.imported.as_ref().map_or_else(measurements::measurement_interval_ms, |sample| sample.sampling_interval_ms),
                "samples": samples,
                "limitations": if MEMORY { "Sampled lower bound over the verified selected-agent process tree: short-lived processes can be missed. RSS includes shared pages per matching process, not unique physical memory. ASB's outer gate and unrelated host tasks are excluded." } else { "Sampled lower bound: short-lived processes can be missed. New process identities require an observed exec; fork-without-exec workers are unclassified because inherited agent identity cannot distinguish them from pending tool launches. Workers are additional OS processes with the same executable or script entry point, not logical subagents within one process. Tools and the ASB gate are excluded. RSS includes shared pages per matching process, not unique physical memory. Different wrapper entry points are not inferred." },
                "metrics": metrics
            }),
        )
    }

    fn graph_section(&self, name: &str) -> Option<GraphSection> {
        name.starts_with(&format!("metric-{}.", self.id())).then_some(if MEMORY {
            GraphSection { title: "Agent memory", description: "Sampled peak aggregate RSS (MiB) of matching agent processes only. Tool subprocesses and host memory are excluded; shared pages may be counted in multiple agent processes." }
        } else {
            GraphSection { title: "Agent workers", description: "Observed additional agent OS processes per execution, excluding the primary agent and tool subprocesses. Sampling may miss short-lived workers; logical subagents inside one process are not measured." }
        })
    }
}

impl Identity {
    fn resolve(program: &str, args: &[String]) -> Option<Self> {
        Self::resolve_in(program, args, std::env::var_os("PATH").as_deref(), None)
    }

    fn resolve_in(
        program: &str,
        args: &[String],
        path: Option<&std::ffi::OsStr>,
        cwd: Option<&Path>,
    ) -> Option<Self> {
        let executable = resolve_executable_in(program, path, cwd)?;
        let name = executable.file_name()?.to_str()?;
        // A prepared adapter can invoke an interpreter rather than an agent
        // script directly. Never identify all Node/Python/shell tasks as agents.
        if name.starts_with("python")
            || matches!(
                name,
                "node"
                    | "nodejs"
                    | "bun"
                    | "deno"
                    | "sh"
                    | "bash"
                    | "dash"
                    | "zsh"
                    | "perl"
                    | "ruby"
            )
        {
            let first = args.first()?;
            if matches!(first.as_str(), "-c" | "-e" | "--eval" | "-m") {
                return Some(Self {
                    executable,
                    interpreter: None,
                    runtime_arguments: Some((first.clone(), args.get(1)?.clone())),
                });
            }
            if first.starts_with('-') {
                return None;
            }
            return Some(Self {
                executable: fs::canonicalize(if Path::new(first).is_absolute() {
                    PathBuf::from(first)
                } else {
                    cwd.unwrap_or(Path::new(".")).join(first)
                })
                .ok()?,
                interpreter: Some(executable),
                runtime_arguments: None,
            });
        }
        let prefix = read_bounded(&executable, 4096).ok()?;
        let interpreter = if prefix.starts_with(b"#!") {
            let line = std::str::from_utf8(prefix.split(|byte| *byte == b'\n').next()?).ok()?;
            let mut words = line[2..].split_whitespace();
            let first = words.next()?;
            let command = if Path::new(first).file_name()? == "env" {
                words.find(|word| !word.starts_with('-'))?
            } else {
                first
            };
            Some(resolve_executable_in(command, path, cwd)?)
        } else {
            None
        };
        Some(Self {
            executable,
            interpreter,
            runtime_arguments: None,
        })
    }

    fn matches_sample(&self, proc: &Path, admitted: bool) -> bool {
        // Admission requires the exact launch entry point. Once this PID/start
        // lifetime was observed, argv is no longer identity: clients may rename
        // their process title. Keep checking its executable and tree membership
        // (the latter at both ends of the sample) rather than admitting every
        // process using the same interpreter.
        if admitted {
            let executable = self.interpreter.as_ref().unwrap_or(&self.executable);
            return fs::read_link(proc.join("exe")).is_ok_and(|path| path == *executable);
        }
        self.matches(proc)
    }

    fn matches(&self, proc: &Path) -> bool {
        if let Some((flag, entry)) = &self.runtime_arguments {
            if !fs::read_link(proc.join("exe")).is_ok_and(|path| path == self.executable) {
                return false;
            }
            let Ok(args) = read_bounded(&proc.join("cmdline"), 64 * 1024) else {
                return false;
            };
            let mut args = args.split(|byte| *byte == 0).skip(1);
            return args.next() == Some(flag.as_bytes()) && args.next() == Some(entry.as_bytes());
        }
        let Some(interpreter) = &self.interpreter else {
            return fs::read_link(proc.join("exe")).is_ok_and(|path| path == self.executable);
        };
        if !fs::read_link(proc.join("exe")).is_ok_and(|path| path == *interpreter) {
            return false;
        }
        // Match the interpreter's entry point, never an arbitrary later argument
        // (which might be an agent prompt mentioning its own executable).
        let Ok(args) = read_bounded(&proc.join("cmdline"), 64 * 1024) else {
            return false;
        };
        let Some(entry) = args.split(|byte| *byte == 0).nth(1) else {
            return false;
        };
        // Do not mistake `node -e SCRIPT`, `python -c SCRIPT`, or option
        // values for an entry point. Unrecognized interpreter launch forms
        // are deliberately not guessed.
        if entry.is_empty() || entry.starts_with(b"-") {
            return false;
        }
        let Ok(entry) = std::str::from_utf8(entry) else {
            return false;
        };
        let path = Path::new(entry);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            let Ok(cwd) = fs::read_link(proc.join("cwd")) else {
                return false;
            };
            cwd.join(path)
        };
        fs::canonicalize(path).is_ok_and(|path| path == self.executable)
    }
}

impl Totals {
    fn matches_process(
        &mut self,
        proc: &Path,
        generation: (u32, u64),
        identity: &Identity,
    ) -> bool {
        if identity.matches_sample(
            proc,
            self.admitted.contains(&generation) || self.declared.contains(&generation),
        ) {
            true
        } else {
            self.admitted.remove(&generation);
            false
        }
    }

    fn sample(
        &mut self,
        root: measurements::ProcessIdentity,
        identity: &Identity,
        process_tree_memory: bool,
    ) {
        if !root.matches() {
            self.admitted.clear();
            return;
        }
        let mut rss = 0u64;
        let mut observed = BTreeSet::new();
        for pid in measurements::process_tree_pids(root.pid) {
            let Some(generation) = measurements::ProcessIdentity::read(pid) else {
                continue;
            };
            if !generation.in_tree(root) {
                continue;
            }
            let proc = PathBuf::from(format!("/proc/{pid}"));
            let key = (pid, generation.start_time_ticks);
            if !process_tree_memory && !self.matches_process(&proc, key, identity) {
                continue;
            }
            let Some((start, resident)) = process_memory(&proc) else {
                // A task can exit (or become a short-lived zombie) between
                // identity and RSS reads.  That race is normal for sampled
                // process accounting; treat it as a missed lower-bound sample
                // rather than making an otherwise valid run incomplete.  The
                // explicit observation cap below remains the hard incomplete
                // condition.
                continue;
            };
            if generation.start_time_ticks != start
                || !generation.in_tree(root)
                || (!process_tree_memory && !self.matches_process(&proc, key, identity))
            {
                continue;
            }
            // Bound memory usage even for pathological long runs.
            if self.seen.len() + observed.len() >= 100_000 && !self.seen.contains(&(pid, start)) {
                self.incomplete = true;
                continue;
            }
            observed.insert((pid, start));
            rss = rss.saturating_add(resident);
        }
        // Admission is current observation state, not the historical worker
        // census. Missing, reparented, reused or changed-executable tasks must
        // qualify their entry point again if they later reappear.
        self.admitted = observed.clone();
        if !observed.is_empty() && root.matches() {
            self.seen.extend(observed);
            self.samples += 1;
            self.peak_rss_kib = self.peak_rss_kib.max(rss);
        }
    }
}

fn resolve_executable_in(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let path = if program.contains('/') {
        if Path::new(program).is_absolute() {
            PathBuf::from(program)
        } else {
            cwd.unwrap_or(Path::new(".")).join(program)
        }
    } else {
        std::env::split_paths(path?)
            .map(|dir| {
                if dir.is_absolute() {
                    dir.join(program)
                } else {
                    cwd.unwrap_or(Path::new(".")).join(dir).join(program)
                }
            })
            .find(|path| path.is_file())?
    };
    fs::canonicalize(path).ok()
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    fs::File::open(path)?.take(limit).read_to_end(&mut data)?;
    Ok(data)
}

fn process_memory(proc: &Path) -> Option<(u64, u64)> {
    let stat = String::from_utf8(read_bounded(&proc.join("stat"), 4096).ok()?).ok()?;
    // Field 22 is start time; comm can contain spaces and closing parentheses.
    let fields = stat.rsplit_once(')')?.1.split_whitespace();
    let flags: u64 = fields.clone().nth(6)?.parse().ok()?;
    // Linux PF_FORKNOEXEC: a newly forked tool still exposes its parent's
    // executable/argv until exec. Never admit that inherited identity. The
    // flag clears at exec; the caller then rechecks identity and generation.
    // Fork-only workers remain an explicitly unclassified lower bound.
    if flags & 0x40 != 0 {
        return None;
    }
    let start = fields.clone().nth(19)?.parse().ok()?;
    let status = String::from_utf8(read_bounded(&proc.join("status"), 65536).ok()?).ok()?;
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((start, rss))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)] // Bounded, async-signal-safe owned-child pre-exec fixture.
    fn inherited_pre_exec_identity_never_enters_the_worker_census() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::{net::UnixStream, process::CommandExt};
        let (mut pid_reader, pid_writer) = UnixStream::pair().unwrap();
        let (mut release_writer, release_reader) = UnixStream::pair().unwrap();
        pid_reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        release_reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let inherited =
            Identity::resolve(std::env::current_exe().unwrap().to_str().unwrap(), &[]).unwrap();
        let mut command = std::process::Command::new("/bin/cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Only async-signal-safe syscalls in the owned child's pre-exec hook.
        unsafe {
            command.pre_exec(move || {
                let pid = libc::getpid();
                if libc::write(
                    pid_writer.as_raw_fd(),
                    (&pid as *const libc::pid_t).cast(),
                    std::mem::size_of_val(&pid),
                ) != std::mem::size_of_val(&pid) as isize
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                let mut release = 0u8;
                if libc::read(
                    release_reader.as_raw_fd(),
                    (&mut release as *mut u8).cast(),
                    1,
                ) != 1
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            });
        }
        let spawning = std::thread::spawn(move || command.spawn());
        let mut bytes = [0u8; std::mem::size_of::<libc::pid_t>()];
        pid_reader.read_exact(&mut bytes).unwrap();
        let pid = libc::pid_t::from_ne_bytes(bytes) as u32;
        let root = measurements::ProcessIdentity::read(pid).unwrap();
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let mut totals = Totals::default();
        let before = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                inherited.matches_sample(&proc, false),
                "fixture must expose matching inherited identity"
            );
            for _ in 0..5 {
                totals.sample(root, &inherited, false);
            }
            assert!(totals.seen.is_empty());
            assert!(totals.admitted.is_empty());
            assert_eq!(totals.peak_rss_kib, 0);
        }));
        release_writer.write_all(&[1]).unwrap();
        let mut child = spawning.join().unwrap().unwrap();
        let after = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            totals.sample(root, &inherited, false);
            assert!(
                totals.seen.is_empty(),
                "tool exec must not retain inherited admission"
            );
            let actual = Identity::resolve("/bin/cat", &[]).unwrap();
            totals.sample(root, &actual, false);
            assert_eq!(
                totals.seen.len(),
                1,
                "post-exec selected entry point must still qualify"
            );
        }));
        let _ = child.kill();
        child.wait().unwrap();
        before.unwrap();
        after.unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn memory_scope_accounts_for_the_verified_execution_tree() {
        let root = measurements::ProcessIdentity::read(std::process::id()).unwrap();
        let unrelated = Identity::resolve("/bin/false", &[]).unwrap();
        let mut totals = Totals::default();

        totals.sample(root, &unrelated, true);

        assert_eq!(totals.samples, 1);
        assert_eq!(totals.seen.len(), 1);
        assert!(totals.peak_rss_kib > 0);
    }

    #[test]
    fn prepared_launch_uses_child_path_and_working_directory_for_entry_point_resolution() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("prepared-entry", None).attempt_id);
        fs::create_dir_all(root.join("runtime")).unwrap();
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::write(root.join("runtime/node"), "fixture runtime").unwrap();
        fs::write(root.join("workspace/client.js"), "#!/usr/bin/env node\n").unwrap();
        let path = root.join("runtime");
        let script = Identity::resolve_in(
            "client.js",
            &[],
            Some(path.as_os_str()),
            Some(&root.join("workspace")),
        );
        assert!(
            script.is_none(),
            "bare executable names must be resolved through PATH, not cwd"
        );
        let script = Identity::resolve_in(
            "./client.js",
            &[],
            Some(path.as_os_str()),
            Some(&root.join("workspace")),
        )
        .unwrap();
        assert_eq!(
            script.interpreter.unwrap(),
            fs::canonicalize(path.join("node")).unwrap()
        );
        let direct = Identity::resolve_in(
            "node",
            &["client.js".into(), "private-prompt".into()],
            Some(path.as_os_str()),
            Some(&root.join("workspace")),
        )
        .unwrap();
        assert_eq!(
            direct.executable,
            fs::canonicalize(root.join("workspace/client.js")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn prepared_target_does_not_guess_host_path_or_identify_the_gate_as_the_agent() {
        use std::os::unix::fs::PermissionsExt;
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/agent-identity-fixtures")
            .join(crate::workloads::TaskIdentity::local("target-view", None).attempt_id);
        fs::create_dir_all(root.join("bin")).unwrap();
        let client = root.join("bin/fixture-client");
        fs::write(&client, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
        let mut session =
            Session::start_in_execution_namespace(&root.join("capture"), &["agent-workers".into()])
                .unwrap();
        let launch = crate::launch::ProcessLaunch::new("fixture-client");
        let (mut command, route) = launch.native(Some(Path::new("/not-the-client-asb-gate")));
        command.env_clear();
        session.agent_launch_prepared(&route.target(&command));
        assert!(session.state::<State<false>>().borrow().identity.is_none());
        command.env("PATH", root.join("bin"));
        session.agent_launch_prepared(&route.target(&command));
        assert_eq!(
            session
                .state::<State<false>>()
                .borrow()
                .identity
                .as_ref()
                .unwrap()
                .executable,
            fs::canonicalize(client).unwrap()
        );
        drop(session);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn namespace_capture_exports_only_bounded_aggregates_and_imports_through_owner() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("namespace-capture", None).attempt_id);
        let selected = vec!["agent-workers".into()];
        let mut capture =
            Session::start_in_execution_namespace(&root.join("guest"), &selected).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("private-prompt-fixture")
            .env("PRIVATE_KEY", "private-key-fixture");
        capture.agent_launch_prepared(&crate::launch::TargetLaunch::direct(&command));
        capture.attach_pid(std::process::id()).unwrap();
        let exported = capture.export_execution_namespace_state().unwrap();
        assert_eq!(exported.len(), 1);
        assert!(exported.contains_key("agent-workers"));
        let serialized = serde_json::to_string(&exported).unwrap();
        assert!(!serialized.contains("private-prompt"));
        assert!(!serialized.contains("private-key"));
        assert!(!serialized.contains("executable"));
        assert!(serialized.len() < 1024);
        let mut host = Session::start_with_collectors(&root.join("host"), &selected, None).unwrap();
        let adapter = AgentProcesses::<false>;
        adapter
            .attach_isolated_pid(&mut host, std::process::id())
            .unwrap();
        host.import_execution_namespace_state(&exported).unwrap();
        adapter.finish(&mut host).unwrap();
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("host/agent-workers.json")).unwrap())
                .unwrap();
        assert_eq!(result["source"], "execution-namespace-linux-proc-sampling");
        assert_eq!(result["status"], "observed");
        assert_eq!(
            result["sampling_interval_ms"],
            exported["agent-workers"]["sampling_interval_ms"]
        );
        assert!(
            !root.join("guest/system.json").exists(),
            "guest capture must not run host collectors"
        );
        assert!(!root.join("guest/cpu-accounting.json").exists());
        drop(host);
        drop(capture);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn namespace_capture_rejects_host_only_collectors_duplicates_and_malformed_counts() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("namespace-invalid", None).attempt_id);
        assert!(Session::start_in_execution_namespace(&root, &["perf".into()]).is_err());
        assert!(
            Session::start_in_execution_namespace(
                &root,
                &["agent-workers".into(), "agent-workers".into()]
            )
            .is_err()
        );
        let mut host =
            Session::start_with_collectors(&root, &["agent-workers".into()], None).unwrap();
        let adapter = AgentProcesses::<false>;
        let valid = serde_json::json!({"schema_version":1,"identity_available":true,"sampling_interval_ms":1000,
            "distinct_processes":1,"peak_rss_kib":1024,"samples":2,"incomplete":false});
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("identity_available", serde_json::json!(false)),
            ("distinct_processes", serde_json::json!(100001)),
            ("samples", serde_json::json!(0)),
            ("sampling_interval_ms", serde_json::json!(0)),
            ("sampling_interval_ms", serde_json::json!(90000)),
            ("private_prompt", serde_json::json!("must-not-import")),
        ] {
            let mut changed = valid.clone();
            changed[key] = value;
            assert!(
                adapter
                    .import_execution_namespace_state(&mut host, &changed)
                    .is_err()
            );
        }
        drop(host);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_identity_does_not_match_an_unrelated_process() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let identity = Identity {
            executable: PathBuf::from("/not/the/selected/agent"),
            interpreter: None,
            runtime_arguments: None,
        };
        assert!(!identity.matches(Path::new("/proc/self")));
        let identity =
            Identity::resolve(std::env::current_exe().unwrap().to_str().unwrap(), &[]).unwrap();
        assert!(identity.matches(Path::new("/proc/self")));
        assert!(process_memory(Path::new("/proc/self")).unwrap().1 > 0);
    }

    #[cfg(unix)]
    #[test]
    fn script_match_requires_both_interpreter_and_exact_entry_point() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("asb-process-identity-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("agent.js");
        fs::write(&script, "fixture").unwrap();
        let executable = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let identity = Identity {
            executable: fs::canonicalize(&script).unwrap(),
            interpreter: Some(executable.clone()),
            runtime_arguments: None,
        };
        symlink(&executable, root.join("exe")).unwrap();
        fs::write(
            root.join("cmdline"),
            format!("runtime\0{}\0--prompt\0", script.display()),
        )
        .unwrap();
        assert!(identity.matches(&root));
        fs::write(
            root.join("cmdline"),
            format!("runtime\0-e\0{}\0", script.display()),
        )
        .unwrap();
        assert!(!identity.matches(&root));
        fs::write(
            root.join("cmdline"),
            format!("runtime\0tool.js\0{}\0", script.display()),
        )
        .unwrap();
        assert!(!identity.matches(&root));
        fs::remove_file(root.join("exe")).unwrap();
        symlink("/not/the/interpreter", root.join("exe")).unwrap();
        fs::write(root.join("cmdline"), format!("cat\0{}\0", script.display())).unwrap();
        assert!(!identity.matches(&root));
        assert!(
            !identity.matches_sample(&root, true),
            "an admitted PID must still use its selected interpreter"
        );
        let mut totals = Totals::default();
        let generation = (123, 456);
        totals.seen.insert(generation);
        totals.admitted.insert(generation);
        assert!(!totals.matches_process(&root, generation, &identity));
        assert!(!totals.admitted.contains(&generation));
        assert!(totals.seen.contains(&generation));
        fs::remove_file(root.join("exe")).unwrap();
        symlink(&executable, root.join("exe")).unwrap();
        fs::write(root.join("cmdline"), b"runtime\0unrelated-script.js\0").unwrap();
        assert!(identity.matches_sample(&root, true));
        assert!(
            !totals.matches_process(&root, generation, &identity),
            "returning to the same interpreter cannot inherit historical admission"
        );
        let inline = Identity {
            executable,
            interpreter: None,
            runtime_arguments: Some(("-c".into(), "agent_launcher()".into())),
        };
        fs::write(
            root.join("cmdline"),
            b"python\0-c\0agent_launcher()\0prompt\0",
        )
        .unwrap();
        assert!(inline.matches(&root));
        fs::write(root.join("cmdline"), b"python\0-c\0tool_call()\0").unwrap();
        assert!(!inline.matches(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn admitted_agent_remains_sampled_after_title_change_without_admitting_other_node_tasks() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Child, Command, Stdio};
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("renamed-agent", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let script = root.join("agent.js");
        fs::write(&script, "setTimeout(() => process.exit(3), 10000); console.log('ready'); process.stdin.on('data', () => { process.title = 'renamed-agent'; console.log('renamed'); });").unwrap();
        let identity = Identity::resolve("node", &[script.to_string_lossy().into_owned()])
            .expect("Node is required by the offline replay tests");
        let mut agent = OwnedChild(
            Command::new("node")
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut output = BufReader::new(agent.0.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        let generation = measurements::ProcessIdentity::read(agent.0.id()).unwrap();
        let mut totals = Totals::default();
        agent
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"rename\n")
            .unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "renamed");
        assert!(!identity.matches(&PathBuf::from(format!("/proc/{}", agent.0.id()))));
        // Pi can rewrite argv before the one-second sampler sees it. The PID
        // returned by the exact direct launch is the only permitted admission
        // path in that case; a later matching Node process is still excluded.
        totals
            .declared
            .insert((generation.pid, generation.start_time_ticks));
        totals.sample(generation, &identity, false);
        assert_eq!(totals.samples, 1);
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            totals.sample(generation, &identity, false);
        }
        assert_eq!(
            totals.samples, 21,
            "sampling must continue beyond the initial title change"
        );
        assert_eq!(totals.seen.len(), 1);
        let other = OwnedChild(
            Command::new("node")
                .args(["-e", "setTimeout(() => {}, 10000)"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let other_generation = measurements::ProcessIdentity::read(other.0.id()).unwrap();
        totals.sample(other_generation, &identity, false);
        assert_eq!(
            totals.samples, 21,
            "same interpreter is not sufficient for initial admission"
        );
        assert_eq!(totals.seen.len(), 1);
        agent.0.kill().unwrap();
        agent.0.wait().unwrap();
        totals.sample(generation, &identity, false);
        assert_eq!(
            totals.samples, 21,
            "an exited PID lifetime cannot add observations"
        );
        drop(other);
        drop(agent);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stopping_sampler_does_not_wait_for_a_long_sampling_interval() {
        let (control, receiver) = mpsc::channel();
        let sampler = Sampler {
            control,
            thread: Some(thread::spawn(move || {
                let _ = receiver.recv_timeout(Duration::from_secs(60));
                Totals::default()
            })),
        };
        let start = std::time::Instant::now();
        assert!(sampler.finish().seen.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
