//! Verify isolated daemon-host PIDs against local kernel/process evidence.
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    boot_id: String,
    namespace_pid: u32,
    start_time_ticks: u64,
}

#[derive(Deserialize)]
struct Target {
    container_id: String,
    exec_id: String,
    daemon_pid: u32,
    namespace_pid: u32,
    pid_scope: String,
    process_identity: Identity,
}

#[derive(Clone, Debug)]
pub(crate) struct LocalTarget {
    pub pid: u32,
    pub namespace_pid: u32,
    pub start_time_ticks: u64,
    pub cgroup: PathBuf,
}

fn read(path: &Path) -> Result<String, String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(65537).read_to_string(&mut text))
        .map_err(|_| "cannot read isolated process identity; check local /proc access")?;
    if text.len() > 65536 {
        return Err("isolated process identity exceeds its bound".into());
    }
    Ok(text)
}

fn start_time(stat: &str) -> Result<u64, String> {
    // comm may contain parentheses/spaces. After the final ')', state is
    // field 3 and starttime is field 22.
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid isolated process start time".into())
}

fn snapshot(proc: &Path, pid: u32) -> Result<Identity, String> {
    if pid <= 1 {
        return Err("isolated process must be distinct from init".into());
    }
    Ok(Identity {
        boot_id: read(&proc.join("sys/kernel/random/boot_id"))?.trim().into(),
        namespace_pid: pid,
        start_time_ticks: start_time(&read(&proc.join(pid.to_string()).join("stat"))?)?,
    })
}

fn verify_at(value: &serde_json::Value, proc: &Path) -> Result<LocalTarget, String> {
    let target: Target = serde_json::from_value(value.clone())
        .map_err(|_| "isolated measurement target has invalid identity fields")?;
    let identity = &target.process_identity;
    let hexadecimal =
        |text: &str| text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit());
    let uuid = |text: &str| {
        text.len() == 36
            && text.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
    };
    if target.pid_scope != "docker-daemon-host"
        || !hexadecimal(&target.container_id)
        || !hexadecimal(&target.exec_id)
        || target.daemon_pid <= 1
        || target.namespace_pid <= 1
        || identity.namespace_pid != target.namespace_pid
        || !uuid(&identity.boot_id)
        || read(&proc.join("sys/kernel/random/boot_id"))?.trim() != identity.boot_id
    {
        return Err("isolated target is not proven to belong to this host; remote Docker measurement is not supported".into());
    }
    let process = proc.join(target.daemon_pid.to_string());
    let before = start_time(&read(&process.join("stat"))?)?;
    let status = read(&process.join("status"))?;
    let pids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or("isolated target lacks namespace PID evidence")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "isolated target has invalid namespace PID evidence")?;
    if before != identity.start_time_ticks
        || pids.len() < 2
        || pids.first() != Some(&target.daemon_pid)
        || pids.last() != Some(&target.namespace_pid)
        || !status
            .lines()
            .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T"))
    {
        return Err(
            "isolated target is stale, not stopped, or does not match this host PID namespace"
                .into(),
        );
    }
    let membership = read(&process.join("cgroup"))?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("isolated measurement requires identifiable cgroup-v2 membership")?;
    if !relative.starts_with('/')
        || relative.split('/').any(|part| matches!(part, "." | ".."))
        || !relative.split('/').any(|part| {
            part == target.container_id || part == format!("docker-{}.scope", target.container_id)
        })
        || start_time(&read(&process.join("stat"))?)? != before
    {
        return Err("isolated target does not match its owned container cgroup or changed during verification".into());
    }
    Ok(LocalTarget {
        pid: target.daemon_pid,
        namespace_pid: target.namespace_pid,
        start_time_ticks: before,
        cgroup: Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')),
    })
}

pub(crate) fn verify(value: &serde_json::Value) -> Result<LocalTarget, String> {
    verify_descendant_at(value, Path::new("/proc"))
}

/// Whether a failed local observation can settle while Docker publishes a
/// newly-stopped process tree.  Identity/schema failures are deliberately
/// excluded: retrying malformed controller data must not weaken admission.
pub(crate) fn retryable_verification_error(error: &str) -> bool {
    matches!(
        error,
        "cannot read isolated process identity; check local /proc access"
            | "invalid isolated process start time"
            | "isolated target lacks namespace PID evidence"
            | "isolated target has invalid namespace PID evidence"
            | "isolated target is stale, not stopped, or does not match this host PID namespace"
            | "isolated measurement requires identifiable cgroup-v2 membership"
            | "isolated target does not match its owned container cgroup or changed during verification"
            | "isolated target candidate scan exceeded its deadline"
            | "isolated driver changed during target resolution"
            | "cannot identify the stopped agent under its owned driver"
    )
}

/// Prove that the exact stopped target previously admitted to the cohort has
/// actually resumed.  Docker accepting a control archive only proves delivery
/// to the daemon; it does not prove that the stopped process consumed it.
pub(crate) fn verify_resumed(target: &LocalTarget) -> Result<(), String> {
    verify_resumed_at(target, Path::new("/proc"))
}

fn verify_resumed_at(target: &LocalTarget, proc: &Path) -> Result<(), String> {
    let process = proc.join(target.pid.to_string());
    let before = start_time(&read(&process.join("stat"))?)?;
    let status = read(&process.join("status"))?;
    let state = status
        .lines()
        .find(|line| line.starts_with("State:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("isolated target lacks process-state evidence")?;
    let pids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or("isolated target lacks namespace PID evidence")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "isolated target has invalid namespace PID evidence")?;
    let membership = read(&process.join("cgroup"))?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("isolated measurement requires identifiable cgroup-v2 membership")?;
    let cgroup = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    if before != target.start_time_ticks
        || pids.len() < 2
        || pids.first() != Some(&target.pid)
        || pids.last() != Some(&target.namespace_pid)
        || cgroup != target.cgroup
        || !matches!(state, "R" | "S" | "D" | "I")
        || start_time(&read(&process.join("stat"))?)? != before
    {
        return Err("isolated target did not effectively resume with its verified PID generation and cgroup".into());
    }
    Ok(())
}

/// Resolve the guest's stopped agent beneath the owned Docker exec driver.
/// The daemon PID is a search root only; every candidate must separately pass
/// the existing namespace, generation, stopped-state and cgroup checks.
fn verify_descendant_at(value: &serde_json::Value, proc: &Path) -> Result<LocalTarget, String> {
    if let Ok(direct) = verify_at(value, proc) {
        return Ok(direct);
    }
    let target: Target = serde_json::from_value(value.clone())
        .map_err(|_| "isolated measurement target has invalid identity fields")?;
    if target.pid_scope != "docker-daemon-host" || target.daemon_pid <= 1 {
        return Err("isolated target driver identity is invalid".into());
    }
    let driver = proc.join(target.daemon_pid.to_string());
    let driver_start = start_time(&read(&driver.join("stat"))?)?;
    let cgroup = read(&driver.join("cgroup"))?;
    let container_scope = format!("docker-{}.scope", target.container_id);
    if !cgroup
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|path| {
            path.split('/')
                .any(|part| part == target.container_id || part == container_scope)
        })
    {
        return Err("isolated driver does not belong to its owned container".into());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let candidates = descendant_candidates(proc, target.daemon_pid, deadline)?;
    let mut selected = None;
    for pid in candidates.into_iter().skip(1) {
        if std::time::Instant::now() >= deadline {
            return Err("isolated target candidate scan exceeded its deadline".into());
        }
        let mut candidate = value.clone();
        candidate["daemon_pid"] = serde_json::json!(pid);
        if let Ok(found) = verify_at(&candidate, proc) {
            if selected.is_some() {
                return Err("isolated stopped target identity is ambiguous".into());
            }
            selected = Some(found);
        }
    }
    if start_time(&read(&driver.join("stat"))?)? != driver_start
        || read(&driver.join("cgroup"))? != cgroup
    {
        return Err("isolated driver changed during target resolution".into());
    }
    let selected = selected.ok_or("cannot identify the stopped agent under its owned driver")?;
    let mut final_candidate = value.clone();
    final_candidate["daemon_pid"] = serde_json::json!(selected.pid);
    verify_at(&final_candidate, proc)
}

/// Prefer the kernel's per-thread child lists.  Under a dense concurrent
/// sweep this avoids having every verification walk the entire host `/proc`
/// tree.  `children` belongs to each thread, rather than only the process
/// leader: a Docker driver may spawn the owned target from a non-leader
/// thread. Some fixture roots and constrained procfs mounts omit this Linux
/// interface or prevent a complete thread traversal, so retain the bounded
/// stat-scan fallback for those environments.
fn descendant_candidates(
    proc: &Path,
    root: u32,
    deadline: std::time::Instant,
) -> Result<Vec<u32>, String> {
    match descendants_from_children(proc, root, deadline)? {
        Some(candidates) => Ok(candidates),
        None => descendants_from_stat_scan(proc, root, deadline),
    }
}

const TARGET_PROCESS_TREE_LIMIT: usize = 4096;

fn descendants_from_children(
    proc: &Path,
    root: u32,
    deadline: std::time::Instant,
) -> Result<Option<Vec<u32>>, String> {
    let mut candidates = vec![root];
    let mut seen = std::collections::BTreeSet::from([root]);
    let mut index = 0;
    while index < candidates.len() {
        if std::time::Instant::now() >= deadline {
            return Err("isolated target candidate scan exceeded its deadline".into());
        }
        let pid = candidates[index];
        let task_root = proc.join(pid.to_string()).join("task");
        let tasks = match std::fs::read_dir(&task_root) {
            Ok(tasks) => tasks,
            // We cannot prove that a per-thread traversal covers every
            // launcher thread, so use the existing bounded stat fallback.
            Err(_) => return Ok(None),
        };
        for (task_index, task) in tasks.enumerate() {
            if std::time::Instant::now() >= deadline {
                return Err("isolated target candidate scan exceeded its deadline".into());
            }
            if task_index >= TARGET_PROCESS_TREE_LIMIT {
                // More task entries than the bounded candidate traversal can
                // prove complete coverage for. Retain the pre-existing
                // bounded stat fallback rather than silently omitting a
                // launcher thread.
                return Ok(None);
            }
            let task = match task {
                Ok(task) => task,
                Err(_) => return Ok(None),
            };
            let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|tid| tid.parse::<u32>().ok())
            else {
                continue;
            };
            if tid == 0 {
                return Err("isolated target task list is invalid".into());
            }
            let children = match read(&task.path().join("children")) {
                Ok(children) => children,
                Err(_) => return Ok(None),
            };
            for child in children.split_ascii_whitespace() {
                let pid = child
                    .parse::<u32>()
                    .map_err(|_| "isolated target children list is invalid")?;
                if pid == 0 {
                    return Err("isolated target children list is invalid".into());
                }
                if seen.insert(pid) {
                    candidates.push(pid);
                }
                if candidates.len() > TARGET_PROCESS_TREE_LIMIT {
                    return Err("isolated target process tree exceeds its bound".into());
                }
            }
        }
        index += 1;
    }
    Ok(Some(candidates))
}

fn descendants_from_stat_scan(
    proc: &Path,
    root: u32,
    deadline: std::time::Instant,
) -> Result<Vec<u32>, String> {
    let mut children = std::collections::BTreeMap::<u32, Vec<u32>>::new();
    let entries =
        std::fs::read_dir(proc).map_err(|_| "cannot enumerate isolated target candidates")?;
    for (index, entry) in entries.flatten().enumerate() {
        if index >= 65536 || std::time::Instant::now() >= deadline {
            return Err("isolated target candidate scan exceeded its bound".into());
        }
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = read(&entry.path().join("stat")) else {
            continue;
        };
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        children.entry(parent).or_default().push(pid);
    }
    let mut candidates = vec![root];
    let mut seen = std::collections::BTreeSet::from([root]);
    let mut index = 0;
    while index < candidates.len() {
        if let Some(items) = children.get(&candidates[index]) {
            for &pid in items {
                if seen.insert(pid) {
                    candidates.push(pid);
                }
            }
        }
        if candidates.len() > 4096 {
            return Err("isolated target process tree exceeds its bound".into());
        }
        index += 1;
    }
    Ok(candidates)
}

/// Candidate daemon PIDs are hints, never authority or a substitute bridge PID.
/// Reuse exactly the collector verifier and reject ambiguous/partial resolution.
fn resolve_at(value: &serde_json::Value, proc: &Path) -> Result<serde_json::Value, String> {
    let invalid = || "cannot uniquely resolve the stopped owned client on this host".to_owned();
    let mut target = value.as_object().cloned().ok_or_else(invalid)?;
    if target.len() != 5
        || ![
            "container_id",
            "exec_id",
            "namespace_pid",
            "process_identity",
            "candidates",
        ]
        .iter()
        .all(|key| target.contains_key(*key))
    {
        return Err(invalid());
    }
    let candidates = target.remove("candidates").ok_or_else(invalid)?;
    let candidates = candidates
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 4096)
        .ok_or_else(invalid)?;
    let mut unique = std::collections::BTreeSet::new();
    for candidate in candidates {
        let pid = candidate
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 1)
            .ok_or_else(invalid)?;
        if !unique.insert(pid) {
            return Err(invalid());
        }
    }
    target.insert("pid_scope".into(), serde_json::json!("docker-daemon-host"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut selected = None;
    for pid in unique {
        if std::time::Instant::now() >= deadline {
            return Err(invalid());
        }
        target.insert("daemon_pid".into(), serde_json::json!(pid));
        let candidate = serde_json::Value::Object(target.clone());
        if verify_at(&candidate, proc).is_ok() {
            if selected.is_some() {
                return Err(invalid());
            }
            selected = Some(candidate);
        }
    }
    let selected = selected.ok_or_else(invalid)?;
    verify_at(&selected, proc)?;
    Ok(selected)
}

fn resolve_input(reader: impl Read, proc: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read target resolution request")?;
    if bytes.len() > 65536 {
        return Err("target resolution request exceeds its bound".into());
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| "invalid target resolution request")?;
    serde_json::to_string(&resolve_at(&value, proc)?)
        .map_err(|_| "cannot encode resolved target".into())
}

pub(crate) fn entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    if args.len() == 1 && args[0] == "--resolve" {
        return match resolve_input(std::io::stdin().lock(), Path::new("/proc")) {
            Ok(value) => {
                println!("{value}");
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("cannot resolve stopped owned process identity");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let result = args
        .first()
        .filter(|_| args.len() == 1)
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "invalid isolated process identity request".into())
        .and_then(|pid| snapshot(Path::new("/proc"), pid));
    match result
        .and_then(|identity| serde_json::to_string(&identity).map_err(|error| error.to_string()))
    {
        Ok(value) => {
            println!("{value}");
            std::process::ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("cannot read isolated process identity");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn resolves_only_the_verified_stopped_child_of_owned_driver() {
        let root = std::env::temp_dir()
            .join(super::super::TaskIdentity::local("target-child", None).attempt_id);
        std::fs::create_dir_all(root.join("sys/kernel/random")).unwrap();
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        std::fs::write(root.join("sys/kernel/random/boot_id"), boot).unwrap();
        for (pid, parent, generation, state, namespace) in [
            (9000, 1, 17, "S", 10),
            (9001, 9000, 23, "T", 42),
            (9002, 1, 23, "T", 42),
        ] {
            let path = root.join(pid.to_string());
            std::fs::create_dir_all(&path).unwrap();
            let mut fields = vec!["0".to_owned(); 20];
            fields[0] = state.into();
            fields[1] = parent.to_string();
            fields[19] = generation.to_string();
            std::fs::write(
                path.join("stat"),
                format!("{pid} (fixture) {}", fields.join(" ")),
            )
            .unwrap();
            std::fs::write(
                path.join("status"),
                format!("State: {state}\nNSpid: {pid} {namespace}\n"),
            )
            .unwrap();
            std::fs::write(
                path.join("cgroup"),
                format!("0::/system.slice/docker-{}.scope\n", "a".repeat(64)),
            )
            .unwrap();
        }
        let value = json!({"container_id":"a".repeat(64),"exec_id":"b".repeat(64),"daemon_pid":9000,
            "namespace_pid":42,"pid_scope":"docker-daemon-host",
            "process_identity":{"boot_id":boot,"namespace_pid":42,"start_time_ticks":23}});
        assert_eq!(verify_descendant_at(&value, &root).unwrap().pid, 9001);
        let mut changed = value.clone();
        changed["process_identity"]["start_time_ticks"] = json!(24);
        assert!(verify_descendant_at(&changed, &root).is_err());
        std::fs::write(root.join("9000/cgroup"), "0::/unrelated\n").unwrap();
        assert!(verify_descendant_at(&value, &root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_target_needs_boot_namespace_start_time_stopped_gate_and_owned_cgroup() {
        let root =
            std::env::temp_dir().join(super::super::TaskIdentity::local("target", None).attempt_id);
        std::fs::create_dir_all(root.join("sys/kernel/random")).unwrap();
        std::fs::create_dir_all(root.join("9001")).unwrap();
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        std::fs::write(root.join("sys/kernel/random/boot_id"), boot).unwrap();
        let mut fields = vec!["0"; 20];
        fields[0] = "T";
        fields[19] = "123456";
        std::fs::write(
            root.join("9001/stat"),
            format!("9001 (name with ) parentheses) {}", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(root.join("9001/status"), "State: T\nNSpid: 9001 42\n").unwrap();
        std::fs::write(
            root.join("9001/cgroup"),
            format!("0::/system.slice/docker-{}.scope\n", "a".repeat(64)),
        )
        .unwrap();
        let value = json!({"container_id":"a".repeat(64),"exec_id":"b".repeat(64),"daemon_pid":9001,
            "namespace_pid":42,"pid_scope":"docker-daemon-host",
            "process_identity":{"boot_id":boot,"namespace_pid":42,"start_time_ticks":123456}});
        let target = verify_at(&value, &root).unwrap();
        assert_eq!(
            (target.pid, target.namespace_pid, target.start_time_ticks),
            (9001, 42, 123456)
        );
        assert!(target.cgroup.to_string_lossy().contains(&"a".repeat(64)));
        std::fs::write(root.join("9001/status"), "State: S\nNSpid: 9001 42\n").unwrap();
        assert!(verify_resumed_at(&target, &root).is_ok());
        std::fs::write(root.join("9001/status"), "State: T\nNSpid: 9001 42\n").unwrap();
        assert!(verify_resumed_at(&target, &root).is_err());
        std::fs::write(root.join("9001/status"), "State: Z\nNSpid: 9001 42\n").unwrap();
        assert!(verify_resumed_at(&target, &root).is_err());
        std::fs::write(root.join("9001/status"), "State: S\nNSpid: 9001 42\n").unwrap();
        std::fs::write(
            root.join("9001/cgroup"),
            "0::/system.slice/unrelated.scope\n",
        )
        .unwrap();
        assert!(verify_resumed_at(&target, &root).is_err());
        std::fs::write(
            root.join("9001/cgroup"),
            format!("0::/system.slice/docker-{}.scope\n", "a".repeat(64)),
        )
        .unwrap();
        std::fs::write(root.join("9001/status"), "State: T\nNSpid: 9001 42\n").unwrap();
        let mut request = value.clone();
        request.as_object_mut().unwrap().remove("daemon_pid");
        request.as_object_mut().unwrap().remove("pid_scope");
        request["candidates"] = json!([9002, 9001]);
        assert_eq!(resolve_at(&request, &root).unwrap(), value);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &resolve_input(request.to_string().as_bytes(), &root).unwrap()
            )
            .unwrap(),
            value
        );
        for candidates in [
            json!([]),
            json!([9002]),
            json!([9001, 9001]),
            json!([0, 9001]),
            json!([true, 9001]),
            json!([4294967296u64]),
            json!(vec![9001; 4097]),
        ] {
            let mut invalid = request.clone();
            invalid["candidates"] = candidates;
            assert!(resolve_at(&invalid, &root).is_err());
        }
        let mut extra = request.clone();
        extra["daemon_pid"] = json!(9001);
        assert!(resolve_at(&extra, &root).is_err());
        assert!(resolve_input(&b"not json"[..], &root).is_err());
        assert!(resolve_input(&vec![b' '; 65537][..], &root).is_err());
        std::fs::create_dir_all(root.join("9002")).unwrap();
        for name in ["stat", "cgroup"] {
            std::fs::copy(root.join("9001").join(name), root.join("9002").join(name)).unwrap();
        }
        std::fs::write(root.join("9002/status"), "State: T\nNSpid: 9002 42\n").unwrap();
        assert!(
            resolve_at(&request, &root).is_err(),
            "ambiguous hints cannot choose the first PID"
        );
        std::fs::write(root.join("9002/status"), "State: T\nNSpid: 9002 43\n").unwrap();
        assert_eq!(resolve_at(&request, &root).unwrap(), value);
        for (field, replacement) in [
            ("daemon_pid", json!(0)),
            ("namespace_pid", json!(43)),
            ("pid_scope", json!("local")),
            ("container_id", json!("../escape")),
            ("exec_id", json!("wrong")),
        ] {
            let mut changed = value.clone();
            changed[field] = replacement;
            assert!(verify_at(&changed, &root).is_err(), "{field}");
        }
        for (field, replacement) in [
            ("boot_id", json!("11111111-1111-1111-1111-111111111111")),
            ("namespace_pid", json!(43)),
            ("start_time_ticks", json!(123455)),
        ] {
            let mut changed = value.clone();
            changed["process_identity"][field] = replacement;
            assert!(verify_at(&changed, &root).is_err(), "{field}");
        }
        for status in [
            "State: R\nNSpid: 9001 42\n",
            "State: T\nNSpid: 9002 42\n",
            "State: T\nNSpid: 9001 43\n",
            "State: T\nNSpid: 42\n",
            "State: T\n",
        ] {
            std::fs::write(root.join("9001/status"), status).unwrap();
            assert!(verify_at(&value, &root).is_err());
        }
        std::fs::write(root.join("9001/status"), "State: T\nNSpid: 9001 42\n").unwrap();
        for path in ["/system.slice/unrelated.scope", "/", "/../escape"] {
            std::fs::write(root.join("9001/cgroup"), format!("0::{path}\n")).unwrap();
            assert!(verify_at(&value, &root).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_prefer_proc_children_without_scanning_unrelated_processes() {
        let root = std::env::temp_dir()
            .join(super::super::TaskIdentity::local("target-proc-children", None).attempt_id);
        for pid in [100, 101, 102] {
            std::fs::create_dir_all(
                root.join(pid.to_string())
                    .join("task")
                    .join(pid.to_string()),
            )
            .unwrap();
        }
        std::fs::write(root.join("100/task/100/children"), "101\n").unwrap();
        std::fs::write(root.join("101/task/101/children"), "102\n").unwrap();
        std::fs::write(root.join("102/task/102/children"), "").unwrap();

        assert_eq!(
            descendants_from_children(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap()
            .unwrap(),
            vec![100, 101, 102]
        );
        assert_eq!(
            descendant_candidates(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap(),
            vec![100, 101, 102]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_include_children_of_non_leader_threads() {
        let root = std::env::temp_dir().join(
            super::super::TaskIdentity::local("target-proc-non-leader-children", None).attempt_id,
        );
        for (pid, tid) in [(100, 100), (100, 101), (102, 102)] {
            std::fs::create_dir_all(
                root.join(pid.to_string())
                    .join("task")
                    .join(tid.to_string()),
            )
            .unwrap();
        }
        std::fs::write(root.join("100/task/100/children"), "").unwrap();
        std::fs::write(root.join("100/task/101/children"), "102\n").unwrap();
        std::fs::write(root.join("102/task/102/children"), "").unwrap();

        assert_eq!(
            descendants_from_children(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap()
            .unwrap(),
            vec![100, 102]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_fall_back_to_bounded_stat_scan_when_thread_lists_are_incomplete() {
        let root = std::env::temp_dir().join(
            super::super::TaskIdentity::local("target-proc-children-fallback", None).attempt_id,
        );
        for (pid, parent) in [(100, 1), (101, 100)] {
            std::fs::create_dir_all(root.join(pid.to_string())).unwrap();
            std::fs::write(
                root.join(pid.to_string()).join("stat"),
                format!("{pid} (fixture) S {parent} {}", ["0"; 20].join(" ")),
            )
            .unwrap();
        }
        // A visible task directory without its per-thread children list is
        // incomplete coverage, not proof that there are no descendants.
        std::fs::create_dir_all(root.join("100/task/100")).unwrap();
        assert_eq!(
            descendants_from_children(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap(),
            None
        );
        assert_eq!(
            descendant_candidates(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap(),
            vec![100, 101]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_fall_back_when_thread_list_exceeds_its_bound() {
        let root = std::env::temp_dir()
            .join(super::super::TaskIdentity::local("target-proc-thread-bound", None).attempt_id);
        for tid in 100..=u32::try_from(100 + TARGET_PROCESS_TREE_LIMIT).unwrap() {
            let task = root.join("100/task").join(tid.to_string());
            std::fs::create_dir_all(&task).unwrap();
            std::fs::write(task.join("children"), "").unwrap();
        }
        assert_eq!(
            descendants_from_children(
                &root,
                100,
                std::time::Instant::now() + std::time::Duration::from_secs(5)
            )
            .unwrap(),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_stop_when_thread_traversal_deadline_has_elapsed() {
        let root = std::env::temp_dir().join(
            super::super::TaskIdentity::local("target-proc-thread-deadline", None).attempt_id,
        );
        let task = root.join("100/task/100");
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join("children"), "").unwrap();
        assert!(descendants_from_children(&root, 100, std::time::Instant::now()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stat_parser_rejects_partial_or_zero_start_times() {
        assert!(start_time("1 (partial)").is_err());
        assert!(start_time(&format!("1 (zero) {}", ["0"; 20].join(" "))).is_err());
    }
}
