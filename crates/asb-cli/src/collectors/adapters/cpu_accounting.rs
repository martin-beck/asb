//! Optional accounting in an exclusive, delegated cgroup-v2 leaf. The process
//! gate is moved before it starts the agent; all descendants inherit the leaf.
use crate::collectors::adapters::process_accounting::Sampler;
use crate::collectors::{CollectorAdapter, LifecycleEvent, PreflightContext};
use crate::measurements::{self, Session};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) struct CpuAccounting;
pub(super) static ADAPTER: CpuAccounting = CpuAccounting;

#[derive(Default)]
struct State {
    leaf: Option<Leaf>,
    reason: Option<String>,
    isolated: bool,
    external: Option<ExternalCounter>,
    external_reason: Option<String>,
    fallback_sampler: Option<Sampler>,
}

struct ExternalCounter {
    path: PathBuf,
    initial: (u64, u64),
    final_delta: Option<(u64, u64)>,
}

impl ExternalCounter {
    fn finish(&mut self) -> Result<(), String> {
        self.final_delta = None;
        let final_value = read_cpu_stat(&self.path.join("cpu.stat"))?;
        self.final_delta = Some((
            final_value
                .0
                .checked_sub(self.initial.0)
                .ok_or("container CPU counter decreased")?,
            final_value
                .1
                .checked_sub(self.initial.1)
                .ok_or("container CPU counter decreased")?,
        ));
        Ok(())
    }
}

struct Leaf {
    path: PathBuf,
}

impl Drop for Leaf {
    fn drop(&mut self) {
        // Never delete recursively, kill tasks, or move a surviving process.
        let _ = fs::remove_dir(&self.path);
    }
}

impl CollectorAdapter for CpuAccounting {
    fn description(&self) -> &'static str {
        "process-tree CPU time and scheduler accounting"
    }
    fn target_scope(&self, backend: &str) -> crate::collectors::TargetScope {
        if backend == "local-docker" {
            crate::collectors::TargetScope::Container
        } else {
            crate::collectors::TargetScope::AgentProcess
        }
    }
    fn id(&self) -> &'static str {
        "cpu-accounting"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        None
    }
    fn tool_id(&self) -> Option<&'static str> {
        None
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, _: &mut PreflightContext<'_>) -> Result<(), String> {
        Ok(())
    }

    fn attach_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        match attach(pid, session.cgroup_parent()) {
            Ok(leaf) => state.leaf = Some(leaf),
            Err(reason) => state.reason = Some(reason),
        }
        start_fallback_sampler(session, &mut state, pid);
        Ok(())
    }

    fn finish(&self, session: &mut Session) -> Result<(), String> {
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        stop_fallback_sampler(session, &mut state);
        let mut values = None;
        let mut source = "proc-sampling";
        let mut cleanup = "not_created";
        if let Some(leaf) = state.leaf.take() {
            let populated = fs::read_to_string(leaf.path.join("cgroup.events"))
                .is_ok_and(|text| text.lines().any(|line| line.trim() == "populated 0"));
            match read_cpu_stat(&leaf.path.join("cpu.stat")) {
                Ok(_) if !populated => {
                    state.reason = Some("agent accounting cgroup still has live tasks or unreadable membership; final CPU totals are unavailable".into());
                }
                Ok(cpu) => {
                    session.record_cpu_accounting(cpu.0, cpu.1, "cgroup-v2");
                    values = Some(cpu);
                    source = "cgroup-v2";
                }
                Err(error) => state.reason = Some(error),
            }
            cleanup = if fs::remove_dir(&leaf.path).is_ok() {
                "removed"
            } else {
                "retained_busy_or_inaccessible"
            };
        }
        if values.is_none()
            && let Some((user_ms, system_ms)) = session.sampled_cpu_accounting()
        {
            if state.isolated && user_ms == 0.0 && system_ms == 0.0 {
                session.suppress_unobserved_sampled_cpu();
                state.reason = Some("No agent-tree CPU activity was observed at the sampling resolution; CPU totals are unavailable, not zero. Container cgroup membership was preserved.".into());
            } else {
                values = Some(((user_ms * 1_000.0) as u64, (system_ms * 1_000.0) as u64));
            }
        }
        let container_values = state
            .external
            .as_ref()
            .and_then(|counter| counter.final_delta);
        let mut metrics = serde_json::Map::new();
        if let Some((user, system)) = values {
            metrics.insert(
                "cpu_user_ms".into(),
                serde_json::json!(user as f64 / 1000.0),
            );
            metrics.insert(
                "cpu_system_ms".into(),
                serde_json::json!(system as f64 / 1000.0),
            );
        }
        if let Some((user, system)) = container_values {
            metrics.insert(
                "container_cpu_user_ms".into(),
                serde_json::json!(user as f64 / 1000.0),
            );
            metrics.insert(
                "container_cpu_system_ms".into(),
                serde_json::json!(system as f64 / 1000.0),
            );
        }
        measurements::write_json(
            &session.dir.join("cpu-accounting.json"),
            &serde_json::json!({
                "schema_version": 1, "collector": "cpu-accounting",
                "status": if metrics.is_empty() { "unavailable" } else { "completed" },
                "source": if container_values.is_some() { "read-only-container-cgroup-and-optional-agent-sampling" } else { source },
                "reason": state.reason,
                "remediation": if source == "proc-sampling" && !state.isolated { Some("Run ASB inside a delegated cgroup-v2 scope or set ASB_CGROUP_PARENT to an existing writable delegated cgroup; process-tree sampling was used as the fallback.") } else { None },
                "cleanup": cleanup,
                "metrics": if metrics.is_empty() { serde_json::Value::Null } else { serde_json::Value::Object(metrics) },
                "container_scope": if state.isolated { Some("whole-owned-container-including-runtime-relay-and-background-services; not agent-only CPU") } else { None },
                "container_accounting_reason": state.external_reason,
            }),
        )
    }

    fn attach_isolated_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        state.isolated = true;
        state.reason = Some("Container cgroup membership is preserved; agent-tree CPU accounting uses process sampling, which can miss short-lived processes. Container-wide counters must not be reported as agent-only CPU.".into());
        match external_counter(pid) {
            Ok(counter) => state.external = Some(counter),
            Err(reason) => state.external_reason = Some(reason),
        }
        start_fallback_sampler(session, &mut state, pid);
        Ok(())
    }

    fn lifecycle_event(&self, session: &mut Session, event: LifecycleEvent) -> Result<(), String> {
        if matches!(
            event,
            LifecycleEvent::AgentExecuteEnd | LifecycleEvent::RunFailed
        ) {
            let state = session.state::<State>();
            let mut state = state.borrow_mut();
            if let Some(sample) = state.fallback_sampler.take().and_then(Sampler::stop) {
                session.record_process_tree_accounting(
                    sample.user_ticks,
                    sample.system_ticks,
                    sample.io_delay_ticks,
                    sample.runqueue_ns,
                );
            }
            if let Some(counter) = state.external.as_mut()
                && let Err(reason) = counter.finish()
            {
                state.external_reason = Some(reason);
            }
        }
        Ok(())
    }
}

fn start_fallback_sampler(session: &Session, state: &mut State, pid: u32) {
    if state.fallback_sampler.is_none() {
        state.fallback_sampler = session
            .measurement_target()
            .or_else(|| measurements::ProcessIdentity::read(pid))
            .and_then(|root| Sampler::start(root, measurements::measurement_interval_ms()));
    }
}

fn stop_fallback_sampler(session: &mut Session, state: &mut State) {
    if let Some(sample) = state.fallback_sampler.take().and_then(Sampler::stop) {
        session.record_process_tree_accounting(
            sample.user_ticks,
            sample.system_ticks,
            sample.io_delay_ticks,
            sample.runqueue_ns,
        );
    }
}

fn external_counter(pid: u32) -> Result<ExternalCounter, String> {
    // Identity/ownership was established by the execution adapter. This
    // adapter observes the existing membership, never cgroup.procs writes.
    let membership = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|error| format!("cannot read existing container cgroup: {error}"))?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("container has no readable unified cgroup membership")?;
    if !relative.starts_with('/') || relative.split('/').any(|part| matches!(part, "." | "..")) {
        return Err("invalid existing container cgroup path".into());
    }
    let root = Path::new("/sys/fs/cgroup");
    let path = fs::canonicalize(root.join(relative.trim_start_matches('/')))
        .map_err(|error| format!("cannot resolve existing container cgroup: {error}"))?;
    if !path.starts_with(root) || path == root {
        return Err("container cgroup is not a distinct cgroup-v2 scope".into());
    }
    let initial = read_cpu_stat(&path.join("cpu.stat"))?;
    Ok(ExternalCounter {
        path,
        initial,
        final_delta: None,
    })
}

fn read_cpu_stat(path: &Path) -> Result<(u64, u64), String> {
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|f| f.take(4097).read_to_string(&mut text))
        .map_err(|e| format!("cannot read cgroup CPU accounting: {e}"))?;
    if text.len() > 4096 {
        return Err("cgroup CPU accounting exceeds 4096 bytes".into());
    }
    parse_cpu_stat(&text)
        .ok_or_else(|| "cgroup CPU accounting lacks valid user_usec/system_usec counters".into())
}

fn parse_cpu_stat(text: &str) -> Option<(u64, u64)> {
    let mut user = None;
    let mut system = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next()? {
            "user_usec" => {
                if user.is_some() {
                    return None;
                }
                user = Some(fields.next()?.parse().ok()?);
            }
            "system_usec" => {
                if system.is_some() {
                    return None;
                }
                system = Some(fields.next()?.parse().ok()?);
            }
            _ => {}
        }
    }
    Some((user?, system?))
}

fn attach(pid: u32, configured_parent: Option<&Path>) -> Result<Leaf, String> {
    if !cfg!(target_os = "linux") {
        return Err("cgroup-v2 accounting requires Linux".into());
    }
    let root = Path::new("/sys/fs/cgroup");
    let parent = if let Some(parent) = configured_parent {
        parent.to_path_buf()
    } else if let Some(parent) = std::env::var_os("ASB_CGROUP_PARENT") {
        PathBuf::from(parent)
    } else {
        let membership = fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or("no unified cgroup-v2 membership")?;
        root.join(relative.trim_start_matches('/'))
    };
    let parent =
        fs::canonicalize(parent).map_err(|e| format!("cannot resolve delegated cgroup: {e}"))?;
    if !parent.starts_with(root) || !parent.join("cgroup.controllers").is_file() {
        return Err(
            "ASB_CGROUP_PARENT must identify a cgroup-v2 directory under /sys/fs/cgroup".into(),
        );
    }
    // An unpaused process may already have escaped descendants. Only the
    // runner's direct, stopped gate child can establish complete accounting.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status =
            fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| e.to_string())?;
        let ppid = status
            .lines()
            .find_map(|s| s.strip_prefix("PPid:"))
            .and_then(|s| s.trim().parse::<u32>().ok());
        if ppid != Some(std::process::id()) {
            return Err("CPU accounting requires ASB's direct process gate child".into());
        }
        if status
            .lines()
            .any(|s| s.starts_with("State:") && s.split_whitespace().nth(1) == Some("T"))
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("CPU accounting gate was not stopped before agent execution".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let path = parent.join(format!("asb-{}-{pid}-{nonce}", std::process::id()));
    fs::create_dir(&path)
        .map_err(|e| format!("cannot create delegated CPU accounting cgroup: {e}"))?;
    let leaf = Leaf { path };
    if read_cpu_stat(&leaf.path.join("cpu.stat"))? != (0, 0) {
        return Err("new accounting cgroup is not empty".into());
    }
    OpenOptions::new()
        .write(true)
        .open(leaf.path.join("cgroup.procs"))
        .and_then(|mut file| file.write_all(format!("{pid}\n").as_bytes()))
        .map_err(|e| {
            format!("cannot attach the stopped agent gate to its accounting cgroup: {e}")
        })?;
    Ok(leaf)
}

#[cfg(test)]
#[path = "../../../tests/unit/cpu_accounting.rs"]
mod boundary_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_stat_rejects_missing_duplicate_and_invalid_counters() {
        assert_eq!(
            parse_cpu_stat("usage_usec 15\nuser_usec 12\nsystem_usec 3\n"),
            Some((12, 3))
        );
        assert_eq!(parse_cpu_stat("user_usec 0\nsystem_usec 0\n"), Some((0, 0)));
        for text in [
            "user_usec 10",
            "user_usec x\nsystem_usec 1",
            "user_usec 1\nuser_usec 2\nsystem_usec 3",
        ] {
            assert_eq!(parse_cpu_stat(text), None);
        }
    }

    #[test]
    fn unavailable_accounting_keeps_sampled_values() {
        let dir = std::env::temp_dir().join(format!("asb-cgroup-fallback-{}", std::process::id()));
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        session.state::<State>().borrow_mut().reason = Some("fixture: no delegation".into());
        ADAPTER.finish(&mut session).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("cpu-accounting.json")).unwrap()).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert!(json["metrics"].is_null());
        assert_eq!(json["cleanup"], "not_created");
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn lifecycle_stops_fallback_sampler_without_waiting_for_its_interval() {
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("cpu-fallback-lifecycle", None).attempt_id);
        let mut session =
            Session::start_with_collectors(&dir, &["cpu-accounting".into()], None).unwrap();
        session.attach_pid(std::process::id()).unwrap();
        assert!(session.state::<State>().borrow().fallback_sampler.is_some());
        session.event(LifecycleEvent::AgentExecuteBegin).unwrap();
        let started = Instant::now();
        session.event(LifecycleEvent::AgentExecuteEnd).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(session.state::<State>().borrow().fallback_sampler.is_none());
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn isolated_attachment_never_creates_or_moves_a_cgroup_leaf() {
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("isolated-cpu", None).attempt_id);
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        // An invalid/unowned PID is intentional: this method must not move it
        // or create a cgroup. Missing read-only counters remain unavailable.
        ADAPTER.attach_isolated_pid(&mut session, u32::MAX).unwrap();
        assert!(session.state::<State>().borrow().leaf.is_none());
        ADAPTER.finish(&mut session).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("cpu-accounting.json")).unwrap()).unwrap();
        assert_eq!(value["cleanup"], "not_created");
        assert!(value["remediation"].is_null());
        assert!(
            value["reason"]
                .as_str()
                .unwrap()
                .contains("Container cgroup membership is preserved")
        );
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn container_counters_are_read_only_separate_and_frozen_before_grading() {
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("container-cpu", None).attempt_id);
        fs::create_dir_all(dir.join("existing-cgroup")).unwrap();
        let path = dir.join("existing-cgroup");
        fs::write(path.join("cpu.stat"), "user_usec 100\nsystem_usec 20\n").unwrap();
        let mut session =
            Session::start_with_collectors(&dir.join("measurements"), &[], None).unwrap();
        {
            let state = session.state::<State>();
            let mut state = state.borrow_mut();
            state.isolated = true;
            state.external = Some(ExternalCounter {
                path: path.clone(),
                initial: (100, 20),
                final_delta: None,
            });
        }
        fs::write(path.join("cpu.stat"), "user_usec 300\nsystem_usec 90\n").unwrap();
        ADAPTER
            .lifecycle_event(&mut session, LifecycleEvent::AgentExecuteEnd)
            .unwrap();
        // Grading/environment cleanup happens later and must not inflate the
        // recorded agent-execution interval's whole-container counters.
        fs::write(
            path.join("cpu.stat"),
            "user_usec 99999\nsystem_usec 88888\n",
        )
        .unwrap();
        ADAPTER.finish(&mut session).unwrap();
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(dir.join("measurements/cpu-accounting.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(value["metrics"]["container_cpu_user_ms"], 0.2);
        assert_eq!(value["metrics"]["container_cpu_system_ms"], 0.07);
        assert!(value["metrics"].get("cpu_user_ms").is_none());
        assert!(!path.join("cgroup.procs").exists());
        let mut counter = ExternalCounter {
            path: path.clone(),
            initial: (100000, 100000),
            final_delta: Some((1, 1)),
        };
        assert!(counter.finish().is_err());
        assert!(counter.final_delta.is_none());
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_never_removes_files_in_a_populated_directory() {
        let dir = std::env::temp_dir().join(format!("asb-cgroup-cleanup-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("fixture"), "retained").unwrap();
        drop(Leaf { path: dir.clone() });
        assert_eq!(fs::read_to_string(dir.join("fixture")).unwrap(), "retained");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a delegated cgroup-v2 scope and python3; no provider or host configuration changes"]
    fn live_cgroup_captures_short_lived_children_and_cleans_up() {
        use std::process::{Child, Command};
        struct Guard(Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Guard(Command::new("sh").args(["-c", "kill -STOP $$; for i in 1 2 3; do python3 -c 'import time; end=time.process_time()+0.05; exec(\"while time.process_time() < end: pass\")' || exit 1; done"]).spawn().unwrap());
        let leaf = attach(child.0.id(), None)
            .expect("run this test under systemd-run --user --scope -p Delegate=yes");
        let path = leaf.path.clone();
        assert_eq!(read_cpu_stat(&path.join("cpu.stat")).unwrap(), (0, 0));
        assert!(
            Command::new("kill")
                .args(["-CONT", &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        assert!(child.0.wait().unwrap().success());
        let (user, system) = read_cpu_stat(&path.join("cpu.stat")).unwrap();
        assert!(
            user + system >= 100_000,
            "CPU accounting must retain exited children: {user} + {system}"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::remove_dir(&path).is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!path.exists(), "accounting leaf must be cleaned up");
        drop(leaf);
    }
}
