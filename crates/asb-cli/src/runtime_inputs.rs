//! Component-owned runtime declarations and bounded local discovery.
use crate::{Error, project};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
#[cfg(not(unix))]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(not(unix), test))]
use std::process::Stdio;
use std::time::Duration;
#[cfg(any(not(unix), test))]
use std::time::Instant;

#[cfg(unix)]
use crate::helper_exchange as exchange;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Dependency {
    File {
        path: PathBuf,
    },
    Program {
        path: PathBuf,
        search: Option<String>,
    },
    Python {
        path: PathBuf,
        site: bool,
    },
    Process {
        pid: u32,
    },
}

pub(crate) struct Context<'a> {
    pub root: &'a Path,
    pub project: &'a project::ProjectConfig,
}

impl Context<'_> {
    pub(crate) fn tool_root(&self, id: &str) -> PathBuf {
        project::resolve(self.root, &self.project.tools_dir).join(id)
    }
}

impl Dependency {
    pub(crate) fn file(path: impl Into<PathBuf>) -> Self {
        Self::File { path: path.into() }
    }
    pub(crate) fn program(path: impl Into<PathBuf>) -> Self {
        Self::Program {
            path: path.into(),
            search: None,
        }
    }
    pub(crate) fn command(command: &Command) -> Self {
        let search = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .map(|(_, value)| value.and_then(OsStr::to_str).unwrap_or("").to_owned());
        Self::Program {
            path: PathBuf::from(command.get_program()),
            search,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Discovered {
    schema_version: u32,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    paths: Vec<PathBuf>,
}

pub(crate) fn for_plan(
    parent: &Path,
    plan: &crate::sweep::plan::SweepPlan,
    mode: Option<crate::tape::Mode>,
) -> Result<Vec<Dependency>, Error> {
    let mut inputs = vec![
        Dependency::Process {
            pid: std::process::id(),
        },
        Dependency::file("/etc/os-release"),
        Dependency::file("/proc/sys/kernel/osrelease"),
        Dependency::file("/etc/ld.so.cache"),
        Dependency::file("/etc/ld.so.conf"),
        Dependency::file("/etc/ld.so.conf.d"),
    ];
    let mut selected = std::collections::BTreeSet::new();
    for entry in &plan.entries {
        let (root, project) = project::load(&parent.join(&entry.reference))?;
        let identity = (
            root.clone(),
            entry.configuration.agent.clone(),
            entry.configuration.workload.clone(),
            entry.configuration.provider.clone(),
            serde_json::to_string(&entry.configuration.measurements).map_err(|_| unavailable())?,
        );
        if !selected.insert(identity) {
            continue;
        }
        let context = Context {
            root: &root,
            project: &project,
        };
        let agent = crate::agents::find(&entry.configuration.agent).ok_or_else(unavailable)?;
        inputs.extend(agent.runtime_inputs(&context)?);
        inputs.extend(crate::workloads::runtime_inputs(
            &entry.configuration.workload,
            &context,
        )?);
        if mode != Some(crate::tape::Mode::Replay) {
            inputs.extend(crate::providers::runtime_inputs(
                &entry.configuration.provider,
                &context,
            )?);
        }
        if let Some(settings) = entry
            .configuration
            .measurements
            .as_ref()
            .filter(|settings| settings.enabled)
        {
            let expanded = crate::collectors::expand_dependencies(&settings.collectors);
            for collector in crate::collectors::all() {
                if crate::collectors::selected_or_environment(collector.id(), &expanded) {
                    inputs.extend(collector.runtime_inputs(&context)?);
                }
            }
        }
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

pub(crate) fn discover(python: &Path, dependencies: &[Dependency]) -> Result<Vec<PathBuf>, Error> {
    discover_with_timeout(python, dependencies, Duration::from_secs(60))
}

// Keep production policy unchanged while real-child fixtures exercise the
// complete exchange and custody against a short watchdog.
fn discover_with_timeout(
    python: &Path,
    dependencies: &[Dependency],
    timeout: Duration,
) -> Result<Vec<PathBuf>, Error> {
    let mut bounded = std::collections::BTreeSet::new();
    for dependency in dependencies {
        bounded.insert(dependency);
        if bounded.len() > 4096 {
            return Err(Error::Input(
                "runtime input declaration exceeds its component bound".into(),
            ));
        }
    }
    let dependencies = bounded;
    // Load the same production ELF inspection code used by guest staging.
    // Its execution module is pure definitions; discovery starts no workload.
    let modules = [
        (
            "asb_execution",
            include_str!("../assets/benchmarks/asb_execution.py"),
        ),
        (
            "asb_runtime",
            include_str!("../assets/benchmarks/asb_runtime.py"),
        ),
    ];
    let modules = serde_json::to_string(&modules).map_err(|_| unavailable())?;
    let source = format!(
        "import sys,types,json\nfor name,source in json.loads({}):\n module=types.ModuleType(name)\n sys.modules[name]=module\n exec(source,module.__dict__)\n{}",
        serde_json::to_string(&modules).map_err(|_| unavailable())?,
        include_str!("../assets/benchmarks/asb_runtime_inputs.py")
    );
    struct Request(Vec<u8>);
    impl Write for Request {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > (1024 * 1024usize).saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("runtime request exceeds its bound"));
            }
            self.0.reserve_exact(bytes.len());
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut request = Request(Vec::new());
    serde_json::to_writer(&mut request, &dependencies).map_err(|_| unavailable())?;
    let request = request.0;
    #[cfg(unix)]
    let bytes = exchange::run(python, &source, &request, exchange::Policy {
        request_bytes: 1024 * 1024,
        response_bytes: 4 * 1024 * 1024,
        timeout,
    }).map_err(|reason| {
        Error::Input(format!("runtime inputs could not be fully discovered within their limits; no input values were logged; {reason}"))
    })?;
    // Preserve the existing non-Unix route; it is not part of the Unix bounded
    // exchange qualification. A cancellable Windows writer remains separate.
    #[cfg(not(unix))]
    let bytes = legacy_exchange(python, &source, request, timeout)?;
    let result: Discovered = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    if let Some(code) = result.error_code {
        if !code.is_empty()
            && code.len() <= 80
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            && result.paths.is_empty()
        {
            return Err(Error::Input(format!(
                "runtime inputs discovery failed safely with code `{code}`"
            )));
        }
        return Err(unavailable());
    }
    if result.schema_version != 1
        || result.paths.len() > 100000
        || result.paths.iter().any(|path| !path.is_absolute())
    {
        return Err(unavailable());
    }
    Ok(result.paths)
}

#[cfg(not(unix))]
fn legacy_exchange(
    python: &Path,
    source: &str,
    request: Vec<u8>,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let mut command = Command::new(python);
    command
        .args(["-I", "-S", "-B", "-c"])
        .arg(source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| unavailable())?;
    // Drain the bounded response concurrently so a legitimate manifest cannot
    // deadlock a pipe while the parent waits for discovery to exit.
    let output = child.stdout.take().expect("runtime discovery stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let mut input = child.stdin.take().expect("runtime discovery stdin");
    let sender = std::thread::spawn(move || input.write_all(&request));
    let deadline = Instant::now() + timeout;
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break false,
        }
    };
    if !success {
        stop_owned(&mut child);
    }
    let _ = child.wait();
    let sent = sender.join().map_err(|_| unavailable())?.is_ok();
    let bytes = reader
        .join()
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    if !success || !sent || bytes.len() > 4 * 1024 * 1024 {
        return Err(unavailable());
    }
    Ok(bytes)
}

fn unavailable() -> Error {
    Error::Input("runtime inputs could not be fully discovered within their limits; no input values were logged".into())
}

#[cfg(not(unix))]
fn stop_owned(child: &mut std::process::Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    let _ = child.kill();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("runtime-input-test", None).attempt_id);
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = crate::fixture_writer::cleanup(&self.0);
        }
    }

    #[test]
    fn inherited_stdout_after_successful_helper_exit_is_bounded() {
        bounded_exchange_case("held-stdout");
    }

    #[test]
    fn descendant_held_stdin_after_successful_nonreader_exit_is_bounded() {
        bounded_exchange_case("held-stdin");
    }

    #[test]
    fn nested_probe_inherited_writer_settles_under_outer_gate() {
        bounded_exchange_case("nested-probe");
    }

    fn bounded_exchange_case(case: &str) {
        use crate::test_support::IsolatedEnvironment;
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        const CASE: &str = "ASB_RUNTIME_DISCOVERY_RED_CASE";
        const ROOT: &str = "ASB_RUNTIME_DISCOVERY_RED_ROOT";
        let test = match case {
            "held-stdout" => {
                "runtime_inputs::tests::inherited_stdout_after_successful_helper_exit_is_bounded"
            }
            "held-stdin" => {
                "runtime_inputs::tests::descendant_held_stdin_after_successful_nonreader_exit_is_bounded"
            }
            "nested-probe" => {
                "runtime_inputs::tests::nested_probe_inherited_writer_settles_under_outer_gate"
            }
            _ => unreachable!("private test case"),
        };
        if std::env::var(CASE).as_deref() == Ok(case) {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let dependencies = if case == "held-stdin" {
                (0..512)
                    .map(|index| {
                        Dependency::file(format!(
                            "/PRIVATE_RUNTIME_COMPONENT/{index}/{}",
                            vec!["x".repeat(128); 8].join("/")
                        ))
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![Dependency::file("/fixture/absent-file")]
            };
            // The nested probe has its own 50ms deadline. Give the enclosing
            // real-child exchange enough scheduling headroom to observe its
            // response and perform owned descendant settlement on a busy CI
            // worker; this fixture's independent outer watchdog remains 2s.
            let timeout = if case == "nested-probe" {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(150)
            };
            let result = discover_with_timeout(&root.join("fake-python"), &dependencies, timeout);
            if case == "held-stdin" {
                let error = result.unwrap_err().to_string();
                assert!(!error.contains("PRIVATE_RUNTIME_COMPONENT"));
            } else {
                assert!(
                    result.is_ok(),
                    "a complete response may succeed after owned descendants settle"
                );
            }
            std::fs::write(root.join("returned"), "exchange returned").unwrap();
            return;
        }
        let root = Directory::new();
        let script = format!(
            "#!/usr/bin/python3\nimport os,pathlib,sys,time\nroot=pathlib.Path({})\ncase={}\nif case=='held-stdout': sys.stdin.buffer.read()\npid=os.fork()\nif pid==0:\n os.setsid()\n if case=='held-stdin': os.close(1)\n else: os.close(0)\n time.sleep(30)\n os._exit(0)\n(root/'descendant.pid').write_text(str(pid))\nos.write(1,b'{{\"schema_version\":1,\"paths\":[]}}\\n')\n(root/'helper-parent-exited').write_text('zero exit')\nos._exit(0)\n",
            serde_json::to_string(&root.0).unwrap(),
            serde_json::to_string(case).unwrap()
        );
        let script = if case == "nested-probe" {
            let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks");
            let nested = format!(
                "import os,pathlib,time\npid=os.fork()\nif pid==0:\n os.setsid()\n time.sleep(30)\n os._exit(0)\npathlib.Path({}).write_text(str(pid))\nos._exit(0)\n",
                serde_json::to_string(&root.0.join("descendant.pid")).unwrap()
            );
            format!(
                "#!/usr/bin/python3\nimport sys,pathlib,time\nsys.path.insert(0,{})\nimport asb_runtime\nsys.stdin.buffer.read()\ntry:\n asb_runtime.bounded_probe([sys.executable,'-I','-S','-B','-c',{}],deadline=time.monotonic()+.05)\nexcept asb_runtime.ExecutionError as error:\n assert 'deadline' in str(error)\n# The outer owned gate can settle the inherited descendant before this probe deadline.\npathlib.Path({}).write_text('zero exit')\nprint('{{\"schema_version\":1,\"paths\":[]}}')\n",
                serde_json::to_string(&assets).unwrap(),
                serde_json::to_string(&nested).unwrap(),
                serde_json::to_string(&root.0.join("helper-parent-exited")).unwrap()
            )
        } else {
            script
        };
        let helper = root.0.join("fake-python");
        crate::write_test_file(&root.0, &helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("asb");
        assert!(binary.is_file(), "build the actual ASB binary first");
        let spawn = |program: &std::ffi::OsStr, arguments: &[&str], fixture: bool| {
            let mut launch = crate::launch::ProcessLaunch::new(program);
            for argument in arguments {
                launch.arg(argument);
            }
            let (mut command, route) = launch.native(Some(&binary));
            command
                .isolated_env()
                .env("PATH", "/usr/bin:/bin")
                .env("ASB_RUNTIME_EXECUTABLE", &binary)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if fixture {
                command.env(CASE, case).env(ROOT, &root.0);
                // Keep child assertion failures distinct from an outer timeout.
                // These files belong to this isolated fixture, not a terminal
                // reader or a live provider; read only after owned settlement.
                for (name, stdout) in [("child.stdout", true), ("child.stderr", false)] {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(root.0.join(name))
                        .unwrap();
                    if stdout {
                        command.stdout(file);
                    } else {
                        command.stderr(file);
                    }
                }
            }
            let mut gate =
                crate::process_gate::Admission::new(route.validate_gate(&mut command).unwrap())
                    .unwrap()
                    .spawn()
                    .unwrap();
            gate.resume().unwrap();
            gate
        };
        let mut sentinel = spawn(OsStr::new("/bin/sleep"), &["30"], false);
        let mut fixture = spawn(
            executable.as_os_str(),
            &["--exact", test, "--nocapture"],
            true,
        );
        // Still well below the fixture's 30-second held descendant. Allow the
        // inner success budget and bounded supervisor settlement, not a retry.
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = fixture.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // Red regressions must fail only after the real supervisor confirms
        // complete fixture-tree quiescence; no raw PGID/PID cleanup is used.
        fixture
            .stop()
            .expect("red watchdog must quiesce its owned fixture tree");
        assert!(
            sentinel.try_wait().unwrap().is_none(),
            "unrelated sentinel was affected"
        );
        sentinel.stop().unwrap();
        let helper_exited = root.0.join("helper-parent-exited").is_file();
        let descendant = std::fs::read_to_string(root.0.join("descendant.pid")).and_then(|pid| {
            pid.parse::<u32>()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        });
        let descendant_exited = descendant
            .as_ref()
            .is_ok_and(|pid| !Path::new(&format!("/proc/{pid}")).exists());
        let returned = root.0.join("returned").exists();
        let observation = std::fs::File::open(root.0.join("exchange-observation.json"))
            .ok()
            .and_then(|file| {
                use std::io::Read;
                let mut bytes = Vec::new();
                file.take(256).read_to_end(&mut bytes).ok()?;
                let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                let code = value["code"].as_str()?;
                if !matches!(
                    code,
                    "ok" | "exchange-deadline" | "helper-failed" | "other-error"
                ) {
                    return None;
                }
                Some(format!("{code}, {} ms", value["elapsed_ms"].as_u64()?))
            })
            .unwrap_or_else(|| "no completed fixture observation".into());
        if !helper_exited
            || !descendant_exited
            || !status.is_some_and(|status| status.success())
            || !returned
        {
            let retained = root.0.clone();
            std::mem::forget(root); // Preserve before even diagnostic I/O can fail.
            let excerpt = |name: &str| -> std::io::Result<String> {
                let mut bytes = Vec::new();
                std::fs::File::open(retained.join(name))?
                    .take(16 * 1024)
                    .read_to_end(&mut bytes)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            };
            let stdout = excerpt("child.stdout");
            let stderr = excerpt("child.stderr");
            panic!(
                "runtime discovery fixture {case}: status={status:?}, returned={returned}, helper_exited={helper_exited}, descendant={descendant:?}, descendant_exited={descendant_exited}; owned tree reaped; {observation}; inspect {}\nchild stdout (first16KiB): {stdout:?}\nchild stderr (first16KiB): {stderr:?}",
                retained.display()
            );
        }
    }

    #[test]
    fn runtime_metadata_discovery_accepts_large_native_executable_without_staging_it() {
        use crate::test_support::IsolatedEnvironment;
        const CREATE: &str = "ASB_RUNTIME_LARGE_FIXTURE_CREATE";
        if let Some(destination) = std::env::var_os(CREATE) {
            let original = std::env::current_exe().unwrap();
            std::fs::copy(&original, &destination).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&destination)
                .unwrap()
                .set_len(
                    std::fs::metadata(original)
                        .unwrap()
                        .len()
                        .max(256 * 1024 * 1024 + 1),
                )
                .unwrap();
            return;
        }
        let root = Directory::new();
        let original = std::env::current_exe().unwrap();
        let runtime = original.parent().unwrap().parent().unwrap().join("asb");
        let executable = root.0.join("large-runtime-fixture");
        // A sparse nonloaded tail models debug information beyond the guest
        // bundle's 256 MiB payload bound, without allocating that much storage.
        // Only a dedicated execed creator opens the copy writable. Reaping it
        // before launch excludes shared-parent fork inheritance / ETXTBSY.
        let created = std::process::Command::new(&original)
            .args(["--exact", "runtime_inputs::tests::runtime_metadata_discovery_accepts_large_native_executable_without_staging_it", "--nocapture"])
            .isolated_env().env(CREATE, &executable).output().unwrap();
        assert!(
            created.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&created.stdout),
            String::from_utf8_lossy(&created.stderr)
        );
        assert!(String::from_utf8_lossy(&created.stdout).contains("1 passed"));
        let child = std::process::Command::new(&executable)
            .args(["--exact", "runtime_inputs::tests::discovery_binds_real_interpreters_libraries_and_missing_files_without_running_apps", "--nocapture"])
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("ASB_RUNTIME_EXECUTABLE", runtime)
            .output().unwrap();
        assert!(
            child.status.success(),
            "large metadata-only executable fixture failed: {}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
    }

    #[test]
    fn discovery_binds_real_interpreters_libraries_and_missing_files_without_running_apps() {
        use std::os::unix::fs::PermissionsExt;
        let root = Directory::new();
        let script = root.0.join("fixture-agent");
        let marker = root.0.join("must-not-run");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let missing = root.0.join("missing-configuration");
        let paths = discover(
            Path::new("python3"),
            &[
                Dependency::Process {
                    pid: std::process::id(),
                },
                Dependency::Python {
                    path: "python3".into(),
                    site: false,
                },
                Dependency::program(&script),
                Dependency::file(&missing),
            ],
        )
        .unwrap();
        assert!(paths.contains(&script));
        assert!(paths.contains(&missing));
        assert!(
            paths
                .iter()
                .any(|path| path.file_name().is_some_and(|name| name == "libc.so.6"))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.file_name().is_some_and(|name| name == "os.py"))
        );
        assert!(!marker.exists());
    }

    #[test]
    fn runtime_errors_never_echo_paths_or_interpreter_output() {
        let marker = "PRIVATE_RUNTIME_VALUE";
        let result = discover(
            Path::new("python3"),
            &[Dependency::program(format!("/missing/{marker}"))],
        );
        let error = result.unwrap_err().to_string();
        assert!(!error.contains(marker));
        assert!(error.contains("runtime inputs"));
    }

    fn helper(root: &Directory, source: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.0.join("helper");
        crate::write_test_file(&root.0, &path, format!("#!/usr/bin/python3\n{source}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn exact_response_byte_bound_accepts_limit_and_rejects_one_byte_over() {
        for (source, accepted) in [
            (
                "import sys\nsys.stdin.buffer.read()\nsys.stdout.write('{\"schema_version\":1,\"paths\":[]}'+ ' '*(4*1024*1024-31))\n",
                true,
            ),
            (
                "import sys\nsys.stdin.buffer.read()\nsys.stdout.write('x'*(4*1024*1024+1))\n",
                false,
            ),
        ] {
            let root = Directory::new();
            let python = helper(&root, source);
            let receipts = crate::process_cleanup::test_support::Scope::new();
            let result = discover_with_timeout(&python, &[], Duration::from_secs(2));
            receipts.wait_finished();
            if accepted {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("response exceeds its byte bound")
                );
            }
        }
    }

    #[test]
    fn duplex_response_bounds_and_silent_active_deadlines_are_enforced() {
        for source in [
            "import sys\nsys.stdin.buffer.read()\nsys.stdout.write('x'*(4*1024*1024+1))\n",
            "import time\ntime.sleep(30)\n",
            "import os\nwhile True: os.write(1,b'x'*16384)\n",
        ] {
            let root = Directory::new();
            let python = helper(&root, source);
            let started = Instant::now();
            let result = discover_with_timeout(&python, &[], Duration::from_millis(300));
            assert!(result.is_err(), "{result:?}");
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn continuous_small_responses_do_not_reset_the_absolute_deadline() {
        let root = Directory::new();
        let marker = root.0.join("output-progress");
        let python = helper(
            &root,
            &format!(
                "import os,pathlib,sys,time\nsys.stdin.buffer.read()\nfor index in range(300):\n os.write(1,b'x')\n with pathlib.Path({}).open('ab',buffering=0) as progress: progress.write(b'x')\n time.sleep(.01)\n",
                serde_json::to_string(&marker).unwrap()
            ),
        );
        let receipts = crate::process_cleanup::test_support::Scope::new();
        let started = Instant::now();
        let error = discover_with_timeout(&python, &[], Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        let elapsed = started.elapsed();
        receipts.wait_finished();
        let bytes = std::fs::read(&marker).unwrap().len();
        assert!(
            (2..=300).contains(&bytes),
            "actual output progress: {bytes}"
        );
        assert!(error.contains("exchange deadline exceeded"), "{error}");
        assert!(!error.contains("response exceeds its byte bound"));
        assert!(elapsed < Duration::from_secs(4));
    }

    #[test]
    fn exact_request_limit_is_delivered_and_one_byte_over_never_spawns() {
        let root = Directory::new();
        let marker = root.0.join("received");
        let python = helper(
            &root,
            &format!(
                "import pathlib,sys\nsys.stdout.write('{{\"schema_version\":1,\"paths\":[]}}'+' '*(4*1024*1024-31))\nsys.stdout.flush()\nrequest=sys.stdin.buffer.read()\nassert len(request)==1024*1024\npathlib.Path({}).write_text('received')\n",
                serde_json::to_string(&marker).unwrap()
            ),
        );
        let empty = serde_json::to_vec(&[Dependency::file("/")]).unwrap().len();
        for excess in [0, 1] {
            let dependencies = [Dependency::file(format!(
                "/{}",
                "a".repeat(1024 * 1024 - empty + excess)
            ))];
            assert_eq!(
                serde_json::to_vec(&dependencies).unwrap().len(),
                1024 * 1024 + excess
            );
            let result = discover_with_timeout(&python, &dependencies, Duration::from_secs(2));
            assert_eq!(result.is_ok(), excess == 0, "{result:?}");
            if excess == 0 {
                std::fs::remove_file(&marker).unwrap();
            } else {
                assert!(!marker.exists());
            }
        }
    }

    #[test]
    fn faults_unwind_and_keeper_exhaustion_keep_custody_and_redact_inputs() {
        let root = Directory::new();
        let marker = root.0.join("must-not-start");
        let python = helper(
            &root,
            &format!(
                "import pathlib,sys,time\npathlib.Path({}).write_text('started')\nsys.stdout.write('{{\"schema_version\":1,\"paths\":[]}}');sys.stdout.flush()\ntime.sleep(30)\n",
                serde_json::to_string(&marker).unwrap()
            ),
        );
        let receipts = crate::process_cleanup::test_support::Scope::new();
        crate::process_cleanup::test_support::set_creation_failure(true);
        assert!(discover_with_timeout(&python, &[], Duration::from_millis(200)).is_err());
        crate::process_cleanup::test_support::set_creation_failure(false);
        assert!(!marker.exists());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                exchange::faults::set(0);
            }
        }
        for fault in [1, 2, 3] {
            let reset = Reset;
            exchange::faults::set(fault);
            let result = std::panic::catch_unwind(|| {
                discover_with_timeout(&python, &[], Duration::from_millis(200))
            });
            if fault == 2 {
                assert!(result.is_err());
            } else {
                let error = result.unwrap().unwrap_err().to_string();
                assert!(error.contains(if fault == 3 {
                    "cleanup remains unconfirmed"
                } else {
                    "read failed"
                }));
                assert!(!error.contains(root.0.to_str().unwrap()));
            }
            drop(reset);
            receipts.wait_finished();
        }
    }
}
