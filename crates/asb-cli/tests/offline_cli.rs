//! CLI contract tests using isolated local projects and no provider credentials.

#[path = "support/fixture_namespace.rs"]
mod fixture_namespace;
#[cfg(unix)]
#[path = "support/fixture_writer.rs"]
mod fixture_writer;
#[cfg(unix)]
#[path = "support/python_prerequisite.rs"]
mod python_prerequisite;
#[path = "support/setup_installables.rs"]
mod setup_installables;
mod support;
use std::path::PathBuf;
use std::process::{Command, Output};
#[cfg(unix)]
use std::sync::atomic::Ordering;
use support::IsolatedEnvironment;

#[derive(Clone, Copy)]
#[cfg(unix)]
enum PythonPreparation {
    Ordinary,
    IntentionalBootstrap,
}

struct Project(PathBuf, #[cfg(unix)] PythonPreparation);

impl Project {
    fn new() -> Self {
        let root = fixture_namespace::allocate(&std::env::temp_dir(), "asb-offline-cli-").unwrap();
        #[cfg(unix)]
        return Self(root, PythonPreparation::Ordinary);
        #[cfg(not(unix))]
        Self(root)
    }

    #[cfg(unix)]
    fn intentional_python_bootstrap() -> Self {
        let mut project = Self::new();
        project.1 = PythonPreparation::IntentionalBootstrap;
        project
    }

    fn command(&self, args: &[&str]) -> Output {
        self.command_with_env(args, &[])
    }

    fn command_with_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.command_in_with_env(&self.0, args, extra)
    }

    fn command_in_with_env(
        &self,
        dir: &std::path::Path,
        args: &[&str],
        extra: &[(&str, &str)],
    ) -> Output {
        self.invocation(dir, args, extra)
            .output()
            .expect("ASB starts")
    }

    fn invocation(&self, dir: &std::path::Path, args: &[&str], extra: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
        command
            .current_dir(dir)
            .args(args)
            .isolated_env()
            .env("HOME", &self.0)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        // Preserve instrumentation for coverage runs, without inheriting keys.
        command.envs(extra.iter().copied());
        #[cfg(unix)]
        if args.first() == Some(&"init") && matches!(self.1, PythonPreparation::Ordinary) {
            let search = command
                .get_envs()
                .find(|(name, _)| *name == "PATH")
                .and_then(|(_, value)| value)
                .unwrap_or_default();
            python_prerequisite::require(dir, search).unwrap_or_else(|error| panic!("{error}"));
        }
        command
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn command_with_reserved_bridge(
        &self,
        args: &[&str],
        extra: &[(&str, &str)],
        listener: std::net::TcpListener,
    ) -> Output {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = listener.as_raw_fd();
        // SAFETY: this fixture exclusively owns this live listener descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        let mut command = self.invocation(&self.0, args, extra);
        command
            .env("ASB_TEST_LISTENER_FD", fd.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // SAFETY: only async-signal-safe fcntl calls and errno retrieval occur
        // after fork. CLOEXEC belongs to the child's descriptor table, not the
        // shared socket description; parent/unrelated-child flags never change.
        unsafe {
            command.pre_exec(move || {
                let owned = listener.as_raw_fd();
                let flags = libc::fcntl(owned, libc::F_GETFD);
                if flags < 0 || libc::fcntl(owned, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .expect("ASB starts with its reserved listener");
        // Capture the parent-flag check, but assert only after owned teardown.
        let parent_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        drop(command); // Closes the parent's listener before waiting for ASB.
        let output = child.wait_with_output().expect("ASB finishes");
        assert_eq!(parent_flags, flags, "parent listener flags changed");
        output
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        #[cfg(unix)]
        if contents.starts_with("#!") {
            fixture_writer::write_file(&self.0, &path, contents).unwrap();
            return;
        }
        std::fs::write(path, contents).unwrap();
    }

    fn init(&self) {
        expect_success(&self.command(&["init"]));
        // The routing fixtures use a completed bundled task so these tests
        // exercise provider/tape behavior without masking workload grading.
        self.write(
            "workspace/src/parser.py",
            "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
        );
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = fixture_writer::cleanup(&self.0);
        #[cfg(not(unix))]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
#[test]
fn ordinary_init_missing_python_fails_before_asb_or_bootstrap_admission() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.write("fake-bin/python3.12", "#!/bin/sh\nprintf '3.14\\n'\n");
    project.write(
        "fake-bin/curl",
        "#!/bin/sh\nprintf admitted > curl-admitted\nexit 55\n",
    );
    for name in ["python3.12", "curl"] {
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let path = project.0.join("fake-bin");
    let rejected = std::panic::catch_unwind(|| {
        project.command_with_env(&["init"], &[("PATH", path.to_str().unwrap())])
    });
    let panic = rejected.expect_err("ordinary init must reject unsupported test prerequisite");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("offline test prerequisite"), "{message}");
    assert!(message.contains("Python 3.12 or 3.13"), "{message}");
    assert!(message.contains("ASB init was not invoked"), "{message}");
    assert!(!project.0.join(".asb").exists());
    assert!(!project.0.join(".asb-tools").exists());
    assert!(!project.0.join("curl-admitted").exists());
}
fn expect_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(output),
        stderr(output)
    );
}

#[cfg(unix)]
#[test]
fn unusable_selected_agent_never_runs_a_different_path_installation() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let project = Project::new();
    project.init();
    project.write("benchmark.toml", "agent='codex'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture/model'\nworkspace='workspace'\nlog_file='run.log'\n");
    let marker = project.0.join("host-agent-started");
    project.write(
        "host-bin/codex",
        &format!(
            "#!/bin/sh\nprintf started > '{}'\nexit 7\n",
            marker.display()
        ),
    );
    project.write(
        "host-bin/npm",
        "#!/bin/sh\nprintf invoked > \"$HOME/npm-invoked\"\nexit 91\n",
    );
    std::fs::set_permissions(
        project.0.join("host-bin/npm"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        project.0.join("host-bin/codex"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let path = std::env::join_paths(std::iter::once(project.0.join("host-bin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let body = serde_json::json!({"model":"fixture/model", "input":"fixture", "tools":[{"type":"function", "name":"fixture"}]}).to_string();
    let interaction = serde_json::json!({"request":{"method":"POST","url":"/v1/responses","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
    project.write("input.jsonl", &format!("{interaction}\n"));
    project.write("suite.toml", "configs=['benchmark.toml']\n");
    project.write(
        "tapes/000-benchmark/cassettes/benchmark-codex-bug-fix-fixture-model.jsonl",
        &format!("{interaction}\n"),
    );
    let selected = project.0.join(".asb-tools/codex/bin/codex");
    project.write(".asb-tools/codex/bin/codex", "not executable");
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o600)).unwrap();
    for broken_symlink in [false, true] {
        if broken_symlink {
            std::fs::remove_file(&selected).unwrap();
            symlink(project.0.join("absent-native-package"), &selected).unwrap();
        }
        for args in [
            vec!["run", "benchmark.toml"],
            vec![
                "run",
                "benchmark.toml",
                "--llm-replay",
                "--cassette",
                "input.jsonl",
            ],
            vec!["sweep", "suite.toml"],
            vec![
                "sweep",
                "suite.toml",
                "--llm-replay",
                "--cassette-root",
                "tapes",
            ],
        ] {
            let output = project.command_with_env(&args, &[("PATH", path.to_str().unwrap())]);
            assert!(!output.status.success());
            assert!(
                stderr(&output).contains("not a usable executable"),
                "{}",
                stderr(&output)
            );
            assert!(stderr(&output).contains("asb tool install codex"));
            assert!(
                !marker.exists(),
                "an unusable selection must never start the host agent"
            );
            assert!(
                !project.0.join("npm-invoked").exists(),
                "an unusable selection must not be reinstalled implicitly"
            );
        }
    }
    // Truly missing tools remain eligible for automatic installation.
    std::fs::remove_file(&selected).unwrap();
    let output = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--llm-replay",
            "--cassette",
            "input.jsonl",
        ],
        &[("PATH", path.to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(
        project.0.join("npm-invoked").exists(),
        "missing agent did not attempt automatic installation"
    );
}

#[cfg(unix)]
fn fake_perf_distribution(project: &Project, bin: &str) {
    use std::os::unix::fs::PermissionsExt;
    for (name, script) in [
        (
            "apt-cache",
            "#!/bin/sh\necho '  Depends: linux-test-tools-1'\n".to_owned(),
        ),
        ("apt-get", "#!/bin/sh\ntouch package.deb\n".to_owned()),
        (
            "dpkg-deb",
            format!(
                "#!/bin/sh\nmkdir -p \"$3/usr/bin\"\ncp '{}' \"$3/usr/bin/perf\"\nchmod +x \"$3/usr/bin/perf\"\n",
                project.0.join(bin).join("perf").display()
            ),
        ),
    ] {
        let file = format!("{bin}/{name}");
        project.write(&file, &script);
        std::fs::set_permissions(project.0.join(file), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn tui_selection_installs_locally_before_enabling_agent_or_workload() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [
        "success",
        "failure",
        "no-executable",
        "not-runnable",
        "workload",
        "collector",
    ] {
        let project = Project::new();
        project.init();
        project.write("fake-bin/npm", "#!/bin/sh\ncase \"$ASB_TEST_INSTALL_MODE\" in failure) exit 1;; no-executable) exit 0;; esac\nprefix=\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --prefix ]; then shift; prefix=$1; fi\n  shift\ndone\n[ -n \"$prefix\" ] || exit 2\nmkdir -p \"$prefix/node_modules/.bin\"\nprintf '#!/bin/sh\\nexit 0\\n' > \"$prefix/node_modules/.bin/codex\"\n[ \"$ASB_TEST_INSTALL_MODE\" = not-runnable ] || chmod +x \"$prefix/node_modules/.bin/codex\"\nexit 0\n");
        project.write("fake-bin/curl", "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; printf '#!/bin/sh\\nexit 0\\n' > \"$1\"; exit 0; fi; shift; done\nexit 1\n");
        // External workloads are pinned source archives.  Keep this terminal
        // fixture offline by materialising the minimal valid AgentBench tree
        // when the installer invokes tar, rather than handing tar the fake
        // curl response as though it were an archive.
        project.write("fake-bin/tar", "#!/bin/sh\ndest=\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --directory ]; then shift; dest=$1; fi\n  shift\ndone\n[ -n \"$dest\" ] || exit 2\nmkdir -p \"$dest/AgentBench/configs/tasks\"\nprintf 'name: fixture\\n' > \"$dest/AgentBench/configs/tasks/fixture.yaml\"\n");
        project.write("fake-bin/perf", "#!/bin/sh\nexit 0\n");
        for name in ["npm", "curl", "tar", "perf"] {
            std::fs::set_permissions(
                project.0.join("fake-bin").join(name),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        fake_perf_distribution(&project, "fake-bin");
        let path = format!(
            "{}:{}",
            project.0.join("fake-bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("python3")
            .arg("-c")
            .arg(include_str!("tui_auto_install_pty.py"))
            .arg(env!("CARGO_BIN_EXE_asb"))
            .arg(&project.0)
            .arg(mode)
            .isolated_env()
            .env("HOME", &project.0)
            .env("PATH", path)
            .env("TERM", "xterm-256color")
            .env("ASB_TEST_INSTALL_MODE", mode)
            .output()
            .expect("start PTY test");
        assert!(
            output.status.success(),
            "{mode}: stdout={} stderr={}",
            stdout(&output),
            stderr(&output)
        );
        let agent = project.0.join(".asb-tools/codex/node_modules/.bin/codex");
        assert_eq!(
            agent.is_file(),
            matches!(mode, "success" | "collector" | "not-runnable"),
            "{mode}"
        );
        if mode == "workload" {
            assert!(project.0.join("workloads/agentbench.txt").is_file());
            assert!(
                project
                    .0
                    .join("workloads/agentbench/asb-trial/ASB-ADAPTER-FIXTURE")
                    .is_file()
            );
        }
        if mode == "collector" {
            assert!(project.0.join(".asb-tools/perf/bin/perf").is_file());
            assert!(
                project
                    .0
                    .join(".asb-tools/flamegraph/bin/flamegraph")
                    .is_file()
            );
            let manifest = std::fs::read_dir(&project.0)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with("asb-sweep-") && name.ends_with(".toml")
                        })
                })
                .expect("generated sweep");
            let sweep: toml::Value =
                toml::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
            let config = project.0.join(sweep["configs"][0].as_str().unwrap());
            let text = std::fs::read_to_string(config).unwrap();
            let config: toml::Value = toml::from_str(&text).unwrap();
            let collectors = config["measurements"]["collectors"].as_array().unwrap();
            assert!(
                collectors
                    .iter()
                    .any(|value| value.as_str() == Some("flamegraph"))
            );
            assert!(
                collectors
                    .iter()
                    .any(|value| value.as_str() == Some("perf"))
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn init_uses_uv_when_host_python_is_incompatible() {
    use std::os::unix::fs::PermissionsExt;
    let base = Project::new();
    base.init();
    let compatible = base.0.join(".asb/python/venv/bin/python");
    assert!(compatible.is_file(), "test host needs a compatible Python");
    let project = Project::intentional_python_bootstrap();
    for name in ["python3", "python3.12", "python3.13"] {
        project.write(&format!("fake-bin/{name}"), "#!/bin/sh\nprintf '3.14\\n'\n");
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    project.write("fake-bin/uv", &format!(
        "#!/bin/sh\nif [ \"$1\" = python ]; then exit 1; fi\nprintf '%s\\n' \"$UV_PYTHON_INSTALL_DIR\" > '{}/uv-install-dir'\nfor arg do last=\"$arg\"; done\nexec '{}' -m venv --without-pip \"$last\"\n",
        project.0.display(), compatible.display()));
    std::fs::set_permissions(
        project.0.join("fake-bin/uv"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let fake_path = project.0.join("fake-bin");
    let result = project.command_with_env(&["init"], &[("PATH", fake_path.to_str().unwrap())]);
    expect_success(&result);
    assert!(stdout(&result).contains("using uv to download Python 3.12"));
    let install_dir = std::fs::read_to_string(project.0.join("uv-install-dir")).unwrap();
    let install_dir = std::path::Path::new(install_dir.trim());
    assert!(install_dir.ends_with(".asb/python/uv-python"));
    assert!(!install_dir.is_absolute() || install_dir.starts_with(&project.0));
    let python = project.0.join(".asb/python/venv/bin/python");
    assert!(python.is_file());
    assert!(
        Command::new(python)
            .args([
                "-c",
                "import sys; assert (3, 12) <= sys.version_info[:2] < (3, 14)"
            ])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
#[ignore = "downloads a standalone Python; set ASB_TEST_UV to the uv executable"]
fn init_downloads_real_project_local_python_with_uv() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let uv = std::env::var_os("ASB_TEST_UV").expect("set ASB_TEST_UV to an absolute uv path");
    let project = Project::intentional_python_bootstrap();
    for name in ["python3", "python3.12", "python3.13"] {
        project.write(&format!("fake-bin/{name}"), "#!/bin/sh\nprintf '3.14\\n'\n");
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    symlink(uv, project.0.join("fake-bin/uv")).unwrap();
    let fake_path = project.0.join("fake-bin");
    let empty_uv = project.0.join("empty-uv");
    let result = project.command_with_env(
        &["init"],
        &[
            ("PATH", fake_path.to_str().unwrap()),
            ("UV_PYTHON_INSTALL_DIR", empty_uv.to_str().unwrap()),
        ],
    );
    expect_success(&result);
    assert!(
        stdout(&result).contains("using uv to download Python 3.12"),
        "{}",
        stdout(&result)
    );
    assert!(
        !stdout(&result).contains("Warning: project Python"),
        "{}",
        stdout(&result)
    );
    let python = project.0.join(".asb/python/venv/bin/python");
    let version = Command::new(python).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("Python 3.12."));
    assert!(project.0.join(".asb/python/uv-python").is_dir());
}

#[cfg(unix)]
#[test]
fn init_warns_when_no_compatible_python_or_uv_is_available() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::intentional_python_bootstrap();
    for name in ["python3", "python3.12", "python3.13"] {
        project.write(&format!("fake-bin/{name}"), "#!/bin/sh\nprintf '3.14\\n'\n");
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let fake_path = project.0.join("fake-bin");
    let result = project.command_with_env(&["init"], &[("PATH", fake_path.to_str().unwrap())]);
    expect_success(&result);
    assert!(stdout(&result).contains("Warning: project Python is not ready"));
    assert!(stdout(&result).contains("Install uv, or install Python 3.12 or 3.13"));
    assert!(project.0.join(".asb/project.toml").is_file());
    assert!(!project.0.join(".asb/python/venv/bin/python").exists());
}

#[cfg(unix)]
#[test]
fn python_agent_install_replaces_incompatible_venv_without_losing_it() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    let python = project.0.join(".asb/python/venv/bin/python");
    assert!(python.is_file(), "test host needs a compatible Python");
    let wheel_builder = r#"
import pathlib, sys, zipfile
root = pathlib.Path(sys.argv[1]); root.mkdir()
for name, ver in [('openjiuwen', '0.1.19.post2'), ('filelock', '3.32.7')]:
    dist = f'{name}-{ver}.dist-info'
    with zipfile.ZipFile(root / f'{name}-{ver}-py3-none-any.whl', 'w') as wheel:
        wheel.writestr(f'{name}/__init__.py', 'def main():\n    print("fixture agent")\n')
        wheel.writestr(f'{dist}/METADATA', f'Metadata-Version: 2.1\nName: {name}\nVersion: {ver}\nProvides-Extra: cli\n')
        wheel.writestr(f'{dist}/WHEEL', 'Wheel-Version: 1.0\nGenerator: asb-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
        wheel.writestr(f'{dist}/RECORD', '')
        if name == 'openjiuwen': wheel.writestr(f'{dist}/entry_points.txt', '[console_scripts]\nopenjiuwen = openjiuwen:main\n')
"#;
    let wheels = project.0.join("wheels");
    assert!(
        Command::new(&python)
            .args(["-c", wheel_builder])
            .arg(&wheels)
            .status()
            .unwrap()
            .success()
    );
    project.write(
        ".asb-tools/openjiuwen/venv/bin/python",
        "#!/bin/sh\nprintf '3.14\\n'\n",
    );
    std::fs::set_permissions(
        project.0.join(".asb-tools/openjiuwen/venv/bin/python"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    project.write(".asb-tools/openjiuwen/venv/keep.txt", "preserve me");
    let result = project.command_with_env(
        &["tool", "install", "openjiuwen"],
        &[
            ("PIP_NO_INDEX", "1"),
            ("PIP_FIND_LINKS", wheels.to_str().unwrap()),
        ],
    );
    expect_success(&result);
    assert!(stdout(&result).contains("Preserved the previous tool Python environment"));
    assert_eq!(
        std::fs::read_to_string(
            project
                .0
                .join(".asb-tools/openjiuwen/venv.asb-backup-1/venv/keep.txt")
        )
        .unwrap(),
        "preserve me"
    );
    let executable = project.0.join(".asb-tools/openjiuwen/venv/bin/openjiuwen");
    let output = Command::new(executable).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("fixture agent"));
}
fn expect_failure(output: &Output, message: &str) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        stdout(output)
    );
    assert!(
        stderr(output).contains(message),
        "expected {message:?}; stderr={}",
        stderr(output)
    );
}

#[test]
fn help_is_successful_for_each_noninteractive_command() {
    let project = Project::new();
    for command in [
        vec!["--help"],
        vec!["init", "--help"],
        vec!["setup", "--help"],
        vec!["run", "--help"],
        vec!["sweep", "--help"],
        vec!["compare", "--help"],
        vec!["provider", "--help"],
        vec!["provider", "models", "--help"],
        vec!["tool", "--help"],
        vec!["tool", "list", "--help"],
        vec!["tool", "install", "--help"],
        vec!["workload", "--help"],
        vec!["workload", "list", "--help"],
        vec!["workload", "install", "--help"],
    ] {
        let result = project.command(&command);
        expect_success(&result);
        assert!(stdout(&result).contains("Usage:"), "{command:?}");
        assert!(
            stderr(&result).is_empty(),
            "{command:?}: {}",
            stderr(&result)
        );
    }
}

#[test]
fn unknown_commands_suggest_the_intended_command() {
    let project = Project::new();
    for (args, suggestion) in [
        (vec!["sweeep"], "sweep"),
        (vec!["tool", "instal"], "install"),
        (vec!["workload", "swe-bench", "triels"], "trials"),
    ] {
        expect_failure(&project.command(&args), suggestion);
    }
}

#[test]
fn invalid_run_and_sweep_flags_fail_before_execution() {
    let project = Project::new();
    project.init();
    let workspace = project.0.join("asb-workspace");
    assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
    for (args, message) in [
        (
            vec!["run", "missing.toml", "--cassette", "x"],
            "--cassette requires",
        ),
        (
            vec!["run", "missing.toml", "--respect-record-timings"],
            "requires --replay",
        ),
        (
            vec!["sweep", "missing.toml", "--respect-record-timings"],
            "requires --replay",
        ),
        (
            vec!["run", "missing.toml", "--record", "--replay"],
            "cannot be used with",
        ),
        (
            vec!["sweep", "missing.toml", "--record", "--replay"],
            "cannot be used with",
        ),
    ] {
        expect_failure(&project.command(&args), message);
    }
    assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
}

#[test]
fn setup_writes_a_parseable_user_selected_configuration() {
    let project = Project::new();
    project.init();
    setup_installables::seed(&project.0, ".asb-tools");
    let result = project.command(&[
        "setup",
        "--agent",
        "pi",
        "--workload",
        "bug-fix",
        "--provider",
        "cli2key",
        "--model",
        "gpt-6.1-sol",
        "--concurrency",
        "3",
        "--workspace",
        "my workspace",
        "--output",
        "selected.toml",
    ]);
    expect_success(&result);
    assert!(stdout(&result).contains("asb run selected.toml"));
    let text = std::fs::read_to_string(project.0.join("selected.toml")).unwrap();
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(config["agent"].as_str(), Some("pi"));
    assert_eq!(config["provider"].as_str(), Some("cli2key"));
    assert_eq!(config["model"].as_str(), Some("gpt-6.1-sol"));
    assert_eq!(config["concurrency"].as_integer(), Some(3));
    assert_eq!(config["workspace"].as_str(), Some("my workspace"));
    assert_eq!(config["log_file"].as_str(), Some(".asb-logs/selected.log"));
    assert!(!text.to_ascii_lowercase().contains("api_key"));
}

#[test]
fn setup_rejects_missing_empty_or_zero_inputs_without_a_config_file() {
    let project = Project::new();
    project.init();
    let base = [
        "setup",
        "--agent",
        "pi",
        "--workload",
        "bug-fix",
        "--provider",
        "cli2key",
        "--model",
        "gpt-6.1-sol",
    ];
    expect_failure(&project.command(&["setup"]), "requires agent");
    let mut zero = base.to_vec();
    zero.extend(["--concurrency", "0", "--output", "bad.toml"]);
    expect_failure(&project.command(&zero), "at least 1");
    expect_failure(
        &project.command(&[
            "setup",
            "--agent",
            "",
            "--workload",
            "bug-fix",
            "--provider",
            "cli2key",
            "--model",
            "gpt-6.1-sol",
            "--output",
            "empty.toml",
        ]),
        "cannot be empty",
    );
    assert!(!project.0.join("bad.toml").exists());
    assert!(!project.0.join("empty.toml").exists());
}

#[test]
fn initialization_refuses_to_overwrite_an_existing_project() {
    let project = Project::new();
    project.init();
    project.write("workloads/custom.txt", "keep this prompt");
    let result = project.command(&["init"]);
    expect_failure(&result, "already initialized");
    assert_eq!(
        std::fs::read_to_string(project.0.join("workloads/custom.txt")).unwrap(),
        "keep this prompt"
    );
}

#[cfg(unix)]
#[test]
fn local_tool_install_copies_and_marks_an_executable_without_downloading() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    project.write("fixture-tool", "#!/bin/sh\nexit 0\n");
    expect_success(&project.command(&["tool", "install", "mytool", "--source", "fixture-tool"]));
    let installed = project.0.join(".asb-tools/mytool/bin/mytool");
    assert_eq!(
        std::fs::read_to_string(&installed).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
    assert_ne!(
        std::fs::metadata(installed).unwrap().permissions().mode() & 0o111,
        0
    );
}

#[test]
fn local_workload_install_and_list_use_the_project_registry() {
    let project = Project::new();
    project.init();
    project.write("prompt-source.txt", "Offline example workload\n");
    expect_success(&project.command(&[
        "workload",
        "install",
        "prompt-source.txt",
        "--name",
        "example",
    ]));
    assert_eq!(
        std::fs::read_to_string(project.0.join("workloads/example.txt")).unwrap(),
        "Offline example workload\n"
    );
    let result = project.command(&["workload", "list"]);
    expect_success(&result);
    assert!(stdout(&result).contains("example"));
}

#[test]
fn missing_local_install_sources_fail_without_provider_access() {
    let project = Project::new();
    project.init();
    let tool = project.command(&["tool", "install", "mytool", "--source", "missing-tool"]);
    assert!(!tool.status.success());
    assert!(!project.0.join(".asb-tools/mytool/bin/mytool").exists());
    let workload = project.command(&[
        "workload",
        "install",
        "missing-prompt.txt",
        "--name",
        "example",
    ]);
    assert!(!workload.status.success());
    assert!(!project.0.join("workloads/example.txt").exists());
}

#[test]
fn sweep_rejects_invalid_manifests_and_unsafe_references() {
    let project = Project::new();
    project.init();
    for (name, content, message) in [
        ("empty.toml", "configs = []\n", "contains no benchmark"),
        (
            "unknown.toml",
            "configs = []\nunknown = true\n",
            "unknown field",
        ),
        ("normal.toml", "agent = 'pi'\n", "not a sweep TOML"),
        (
            "missing.toml",
            "configs = ['missing-target.toml']\n",
            "references missing benchmark",
        ),
        (
            "traversal.toml",
            "configs = ['../outside.toml']\n",
            "unsafe benchmark path",
        ),
        (
            "extension.toml",
            "configs = ['bad.txt']\n",
            "not a .toml benchmark",
        ),
        (
            "absolute.toml",
            "configs = ['/tmp/other.toml']\n",
            "unsafe benchmark path",
        ),
    ] {
        project.write(name, content);
        expect_failure(&project.command(&["sweep", name]), message);
    }
}

#[test]
fn sweep_admission_errors_preserve_existing_outputs_and_reject_unbounded_replicas() {
    let project = Project::new();
    project.init();
    project.write("sweep.toml", "configs=['benchmark.toml']\n");
    project.write(
        "benchmark.toml",
        "agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='fixture'\nconcurrency=2\n",
    );
    project.write("sweep-results/previous", "retain result");
    project.write("results/sweep-comparison/report.html", "retain report");
    for options in [
        ["--jobs", "0"],
        ["--jobs", "33"],
        ["--max-active-attempts", "0"],
        ["--max-active-attempts", "65"],
        ["--max-active-attempts", "1"],
    ] {
        let result = project.command(&["sweep", "sweep.toml", options[0], options[1]]);
        assert!(!result.status.success(), "{options:?}");
        assert_eq!(
            std::fs::read_to_string(project.0.join("sweep-results/previous")).unwrap(),
            "retain result"
        );
        assert_eq!(
            std::fs::read_to_string(project.0.join("results/sweep-comparison/report.html"))
                .unwrap(),
            "retain report"
        );
        assert!(!project.0.join("sweep-history").exists());
        assert!(!project.0.join("sweep-results/plan.json").exists());
    }
    project.write(
        "benchmark.toml",
        "agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='fixture'\nconcurrency=65\n",
    );
    for arguments in [
        vec!["run", "benchmark.toml"],
        vec!["sweep", "sweep.toml", "--jobs", "2"],
    ] {
        expect_failure(
            &project.command(&arguments),
            "concurrency must not exceed 64",
        );
    }
    expect_failure(
        &project.command(&[
            "setup",
            "--agent",
            "pi",
            "--provider",
            "cli2key",
            "--model",
            "fixture",
            "--workload",
            "bug-fix",
            "--concurrency",
            "65",
            "--output",
            "invalid.toml",
        ]),
        "concurrency must not exceed 64",
    );
    assert!(!project.0.join("invalid.toml").exists());
    let help = project.command(&["sweep", "--help"]);
    expect_success(&help);
    assert!(stdout(&help).contains("--jobs"));
    assert!(stdout(&help).contains("--max-active-attempts"));
    for command in ["run", "sweep"] {
        let output = project.command(&[command, "benchmark.toml", "--json", "--show-output"]);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("cannot be used with"));
    }
}

#[test]
fn sweep_rejects_nested_sweeps_before_execution_or_comparison() {
    let project = Project::new();
    project.init();
    project.write("outer.toml", "configs = ['inner.toml']\n");
    project.write("inner.toml", "configs = ['irrelevant.toml']\n");
    let result = project.command(&["sweep", "outer.toml"]);
    expect_failure(&result, "nested sweep");
    assert!(!project.0.join("outer-results").exists());
    assert!(!project.0.join("results/outer-comparison").exists());
}

#[test]
fn competing_sweep_leaves_existing_results_and_reports_untouched() {
    let project = Project::new();
    project.init();
    project.write("sweep.toml", "configs=['benchmark.toml']\n");
    // A valid referenced path, deliberately malformed configuration: output
    // exclusion must happen before planning, archival or any runtime work.
    project.write("benchmark.toml", "not a valid benchmark");
    project.write("sweep-results/previous", "retain result");
    project.write("results/sweep-comparison/report.html", "retain report");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(project.0.join(".sweep-asb-sweep.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    expect_failure(
        &project.command(&["sweep", "sweep.toml"]),
        "another sweep owns",
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join("sweep-results/previous")).unwrap(),
        "retain result"
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join("results/sweep-comparison/report.html")).unwrap(),
        "retain report"
    );
    assert!(!project.0.join("sweep-history").exists());
    assert!(!project.0.join("sweep-results/plan.json").exists());
    // Other tests can fork this multi-threaded test process while the raw
    // descriptor is open. Unlock explicitly, just like OutputLock::drop,
    // rather than depending on every inherited copy reaching exec first.
    lock.unlock().unwrap();
    drop(lock);
    let next = project.command(&["sweep", "sweep.toml"]);
    assert!(!next.status.success());
    assert!(!stderr(&next).contains("another sweep owns"));
}

#[test]
fn missing_compare_input_fails_without_creating_a_report() {
    let project = Project::new();
    let result = project.command(&["compare", "missing.toml"]);
    assert!(!result.status.success());
    assert!(!project.0.join("results").exists());
}

#[cfg(unix)]
#[test]
fn ollama_discovery_and_codex_run_use_the_selected_local_endpoint() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let worker_stop = Arc::clone(&stop);
    let worker_requests = Arc::clone(&requests);
    let worker = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fixture server: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..header_end]);
                    let size = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + size {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let path = request.lines().next().unwrap_or_default().to_owned();
            worker_requests.lock().unwrap().push(path.clone());
            let body = if path.starts_with("GET /api/tags ") {
                r#"{"models":[{"name":"qwen3:0.6b"},{"name":"unknown"},{"name":"no-tools","capabilities":["completion"]},{"name":"modern","capabilities":["completion","tools"]}]}"#
            } else if path.starts_with("POST /api/show ") {
                if request.contains("qwen3:0.6b") {
                    r#"{"capabilities":["completion","tools","thinking"]}"#
                } else {
                    assert!(request.contains("unknown"));
                    r#"{"capabilities":["completion"]}"#
                }
            } else if path.starts_with("POST /v1/responses ") {
                r#"{"id":"resp_fixture","object":"response","output":[]}"#
            } else {
                panic!("unexpected request: {path}")
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });

    let project = Project::new();
    project.init();
    let endpoint = format!("http://{address}");
    let host = address.to_string();
    let catalog =
        project.command_with_env(&["provider", "models", "ollama"], &[("OLLAMA_HOST", &host)]);
    expect_success(&catalog);
    assert!(stdout(&catalog).contains("qwen3:0.6b"));
    assert!(stdout(&catalog).contains("modern"));
    assert!(!stdout(&catalog).contains("no-tools"));
    assert!(!stdout(&catalog).contains("unknown"));
    project.write("benchmark.toml", "agent='codex'\nworkload='bug-fix'\nprovider='ollama'\nmodel='qwen3:0.6b'\nworkspace='workspace'\nlog_file='run.log'\n");
    let agent = ".asb-tools/codex/bin/codex";
    project.write(agent, r##"#!/usr/bin/env node
const fs = require('fs');
const cfg = fs.readFileSync(process.env.CODEX_HOME + '/config.toml', 'utf8');
const endpoint = cfg.match(/base_url = "([^"]+)"/)[1];
if (!cfg.includes('model_provider = "asb_provider"') || !cfg.includes('web_search = "disabled"')) process.exit(11);
fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
const body = JSON.stringify({model:'qwen3:0.6b',input:'offline test',tools:[{type:'function',name:'read_file'}]});
fetch(endpoint + '/responses', {method:'POST',headers:{'content-type':'application/json'},body}).then(r => { if (!r.ok) process.exit(12); console.log('local-ollama-agent'); }).catch(() => process.exit(13));
"##);
    std::fs::set_permissions(
        project.0.join(agent),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let run = project.command_with_env(&["run", "benchmark.toml"], &[("OLLAMA_HOST", &endpoint)]);
    let record = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--record",
            "--cassette",
            "tape.jsonl",
        ],
        &[("OLLAMA_HOST", &endpoint)],
    );
    let replay = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
        &[("OLLAMA_HOST", "http://127.0.0.1:1")],
    );
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    expect_success(&run);
    expect_success(&record);
    expect_success(&replay);
    assert!(
        std::fs::read_to_string(project.0.join("run.log"))
            .unwrap()
            .contains("local-ollama-agent")
    );
    let seen = requests.lock().unwrap();
    assert!(seen.iter().any(|path| path.starts_with("POST /api/show ")));
    assert!(
        seen.iter()
            .any(|path| path.starts_with("POST /v1/responses "))
    );
}

#[cfg(unix)]
#[test]
fn codex_uses_selected_openrouter_endpoint_for_live_record_and_replay() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let worker_stop = Arc::clone(&stop);
    let worker_requests = Arc::clone(&requests);
    let worker = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fixture server: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let path = request.lines().next().unwrap_or_default().to_owned();
            worker_requests.lock().unwrap().push(request);
            let body = if path.starts_with("GET /api/v1/models ") {
                r#"{"data":[{"id":"fixture/model","supported_parameters":["tools"]}]}"#
            } else if path.starts_with("POST /api/v1/responses ") {
                r#"{"id":"resp_fixture","object":"response","output":[]}"#
            } else {
                panic!("unexpected request: {path}")
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let project = Project::new();
    project.init();
    project.write("benchmark.toml", "agent='codex'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture/model'\nworkspace='workspace'\nlog_file='run.log'\n");
    let agent = ".asb-tools/codex/bin/codex";
    project.write(agent, r##"#!/usr/bin/env node
const fs = require('fs');
const cfg = fs.readFileSync(process.env.CODEX_HOME + '/config.toml', 'utf8');
const endpoint = cfg.match(/base_url = "([^"]+)"/)[1];
if (!cfg.includes('model_provider = "asb_provider"') || !cfg.includes('wire_api = "responses"') || !cfg.includes('env_key = "ASB_AGENT_API_KEY"')) process.exit(11);
if (process.env.OPENROUTER_API_KEY === 'live-secret' && process.env.ASB_AGENT_API_KEY !== 'live-secret') process.exit(12);
if (process.env.OPENROUTER_API_KEY === 'asb-replay' && process.env.ASB_AGENT_API_KEY !== 'asb-replay') process.exit(13);
fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
const body = JSON.stringify({model:'fixture/model',input:'offline test',tools:[{type:'function',name:'read_file'}]});
fetch(endpoint + '/responses', {method:'POST',headers:{'content-type':'application/json','authorization':'Bearer '+process.env.ASB_AGENT_API_KEY},body}).then(r => {if (!r.ok) process.exit(14); console.log('provider-neutral-agent');}).catch(() => process.exit(15));
"##);
    std::fs::set_permissions(
        project.0.join(agent),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let base = format!("http://{address}/api/v1");
    let env = [
        ("OPENROUTER_BASE_URL", base.as_str()),
        ("OPENROUTER_API_KEY", "live-secret"),
    ];
    let run = project.command_with_env(&["run", "benchmark.toml"], &env);
    let record = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--record",
            "--cassette",
            "tape.jsonl",
        ],
        &env,
    );
    let before_replay = requests.lock().unwrap().len();
    let replay = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
        &env,
    );
    let after_replay = requests.lock().unwrap().len();
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    expect_success(&run);
    assert!(
        record.status.success(),
        "record stdout={} stderr={} log={}",
        String::from_utf8_lossy(&record.stdout),
        String::from_utf8_lossy(&record.stderr),
        std::fs::read_to_string(project.0.join("run.log")).unwrap_or_default()
    );
    expect_success(&replay);
    assert_eq!(
        before_replay, after_replay,
        "replay contacted the live provider"
    );
    let seen = requests.lock().unwrap();
    let posts: Vec<_> = seen
        .iter()
        .filter(|request| request.starts_with("POST /api/v1/responses "))
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(
        posts[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer live-secret")
    );
}

#[cfg(unix)]
fn replay_project(concurrency: usize) -> Project {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    project.write(
        "workspace/src/parser.py",
        "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n",
    );
    for index in 1..=concurrency {
        project.write(
            &format!("workspace-concurrency-{index}/src/parser.py"),
            "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n",
        );
    }
    project.write("benchmark.toml", &format!(
        "agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nconcurrency={concurrency}\nworkspace='workspace'\nlog_file='run.log'\n"
    ));
    let body =
        serde_json::json!({"model":"test-model","input":"offline","tools":[{"type":"function"}]})
            .to_string();
    let cassette = serde_json::json!({"request":{"method":"POST","url":"/v1/responses","body":body},
        "response":{"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}});
    project.write("tape.jsonl", &format!("{cassette}\n"));
    let agent = ".asb-tools/pi/bin/pi";
    project.write(agent, r##"#!/usr/bin/env node
const fs = require('fs');
const config = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json'));
const endpoint = config.providers.asb.baseUrl;
if (!/^http:\/\/127\.0\.0\.1:\d+\/v1$/.test(endpoint) || config.providers.asb.api !== 'openai-responses' || process.env.ASB_AGENT_API_KEY !== 'asb-replay') {
  console.error('unexpected replay configuration'); process.exit(11);
}
if (process.argv.includes('--fixture-agent-worker')) {
  const memory = Buffer.alloc(8 * 1024 * 1024, 1);
  setTimeout(() => process.exit(memory[0] === 1 ? 0 : 1), 1600);
  return;
}
const behavior = process.env.ASB_FIXTURE_BEHAVIOR || 'success';
async function main() {
  if (behavior === 'skip') { console.log('fixture-no-request'); return; }
  const body = {model:'test-model',input:behavior === 'mismatch' ? 'changed' : 'offline',tools:[{type:'function'}]};
  const send = () => fetch(endpoint + '/responses', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
  let response = await send();
  if (behavior === 'extra') response = await send();
  if (!response.ok) { console.error('fixture-rejected', response.status); process.exitCode = 8; return; }
  if (behavior === 'noop') { console.log('No task edits were made.'); return; }
  fs.mkdirSync('src', {recursive:true});
  fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
  if (behavior === 'delete') { fs.rmSync('src/parser.py'); console.log('fixture-deleted'); return; }
  if (behavior === 'mismatch-solution') fs.writeFileSync('src/parser.py', 'def parse_path(value): return []\n');
  if (behavior === 'invalid-utf8') fs.writeFileSync('src/parser.py', Buffer.from([255]));
  if (behavior === 'not-a-file') { fs.rmSync('src/parser.py'); fs.mkdirSync('src/parser.py'); }
  if (behavior === 'cpu-children') {
    for (let i=0;i<3;i++) {
      const result=require('child_process').spawnSync(process.execPath,['-e','const end=performance.now()+60; while(performance.now()<end) {}']);
      if (result.status!==0) throw new Error('offline CPU child failed');
    }
  }
  if (behavior === 'bpf-clock') await new Promise(resolve => setTimeout(resolve, 3600));
  if (behavior === 'replica-barrier') {
    const root = process.env.ASB_FIXTURE_BARRIER;
    fs.mkdirSync(root, {recursive:true});
    fs.writeFileSync(require('path').join(root, String(process.pid)), 'started');
    const deadline = Date.now() + 5000;
    while (fs.readdirSync(root).length < 2) {
      if (Date.now() > deadline) throw new Error('configured replicas were not started concurrently');
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  }
  if (behavior === 'agent-workers' || behavior === 'only-tools') {
    const {spawn} = require('child_process');
    const children = [];
    if (behavior === 'agent-workers') {
      for (let i=0;i<2;i++) children.push(spawn(process.execPath,[__filename,'--fixture-agent-worker']));
    }
    // These share the runtime with the agent, but are not agent workers.
    for (let i=0;i<3;i++) children.push(spawn(process.execPath,['-e','const memory=Buffer.alloc(32*1024*1024,1);setTimeout(()=>process.exit(memory[0]===1?0:1),1600)']));
    children.push(spawn('/bin/sh',['-c','sleep 1.6']));
    await Promise.all(children.map(child=>new Promise((resolve,reject)=>{
      child.on('error',reject); child.on('exit',code=>code===0?resolve():reject(new Error('fixture child failed')));
    })));
  }
  console.log('fixture-agent-output');
  if (behavior === 'api-error-event') console.log(JSON.stringify({type:'agent_end',messages:[{role:'assistant',stopReason:'error',errorMessage:'400 fixture-secret'}]}));
  if (behavior === 'terminal-refusal') {
    const final_answer = "I can't run tests because the test tool is unavailable.";
    fs.writeFileSync(process.env.ASB_PI_SUMMARY_PATH, JSON.stringify({schema_version:1,completed:true,steps:1,usage_complete:false,final_answer}), {mode:0o600});
    console.log(JSON.stringify({type:'message',role:'tool',content:'Tool output is not a refusal.'}));
  }
  if (behavior === 'fatal') console.error('Authentication required. Please use /login command to sign in to your account fixture-secret');
  const unresolvedFailure = behavior === 'exit-unresolved' ||
      (behavior === 'fail-second' && process.cwd().endsWith('concurrency-2')) ||
      (behavior === 'fail-sweep-second' && process.cwd().includes('/001-bad/'));
  if (unresolvedFailure) fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n");
  if (behavior === 'exit' || unresolvedFailure) process.exitCode = 7;
}
main().catch(e => { console.error(e); process.exitCode = 9; });
"##);
    std::fs::set_permissions(
        project.0.join(agent),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    project
}

#[cfg(unix)]
fn replay(project: &Project, behavior: &str, additional: &[&str]) -> Output {
    let mut args = vec![
        "run",
        "benchmark.toml",
        "--llm-replay",
        "--cassette",
        "tape.jsonl",
    ];
    args.extend_from_slice(additional);
    project.command_with_env(&args, &[("ASB_FIXTURE_BEHAVIOR", behavior)])
}

#[cfg(target_os = "linux")]
#[test]
fn running_binary_replacement_does_not_break_measured_replay_gate() {
    let project = replay_project(1);
    let executable = project.0.join("running-asb");
    std::fs::copy(env!("CARGO_BIN_EXE_asb"), &executable).unwrap();
    let child = spawn_copied_fixture(
        Command::new(&executable)
            .current_dir(&project.0)
            .args([
                "run",
                "benchmark.toml",
                "--replay",
                "--cassette",
                "tape.jsonl",
                "--measurements",
                "measurements",
            ])
            .isolated_env()
            .env("HOME", &project.0)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    );
    // Remove only the test-created copy, after exec has loaded it. The kernel
    // keeps its running inode alive while the old pathname no longer exists.
    std::fs::remove_file(executable).unwrap();
    expect_success(&child.wait_with_output().unwrap());
}

#[cfg(target_os = "linux")]
fn spawn_copied_fixture(command: &mut Command) -> std::process::Child {
    // Another test can fork while fs::copy owns its output descriptor. The
    // inherited writer exists briefly until exec closes it, producing ETXTBSY
    // even after our copy returned. Retry only that transient exec failure.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("copied ASB fixture could not start: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn configured_replicas_overlap_instead_of_spawning_on_join() {
    let project = replay_project(2);
    let barrier = project.0.join("replica-barrier");
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
        &[
            ("ASB_FIXTURE_BEHAVIOR", "replica-barrier"),
            ("ASB_FIXTURE_BARRIER", barrier.to_str().unwrap()),
        ],
    );
    expect_success(&result);
    assert_eq!(std::fs::read_dir(barrier).unwrap().count(), 2);
}

#[cfg(target_os = "linux")]
#[test]
fn replay_agent_process_collectors_exclude_tools_and_isolate_concurrent_runs() {
    for (behavior, expected) in [("agent-workers", 2), ("only-tools", 0)] {
        let project = replay_project(2);
        let result = project.command_with_env(
            &[
                "run",
                "benchmark.toml",
                "--replay",
                "--cassette",
                "tape.jsonl",
                "--measurements",
                "measurements",
            ],
            &[
                ("ASB_FIXTURE_BEHAVIOR", behavior),
                ("ASB_MEASURE_AGENT_MEMORY", "1"),
                ("ASB_MEASURE_AGENT_WORKERS", "1"),
                ("ASB_MEASURE_INTERVAL_MS", "100"),
            ],
        );
        expect_success(&result);
        for replica in 1..=2 {
            let dir = project
                .0
                .join(format!("measurements-concurrency-{replica}"));
            let workers: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("agent-workers.json")).unwrap())
                    .unwrap();
            assert_eq!(workers["status"], "observed", "{workers}");
            assert_eq!(
                workers["metrics"]["spawned_worker_count"], expected,
                "{workers}"
            );
            let memory: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("agent-memory.json")).unwrap())
                    .unwrap();
            assert_eq!(memory["status"], "observed", "{memory}");
            assert!(memory["metrics"]["peak_rss_mib"].as_f64().unwrap() > 0.0);
            assert!(!memory.to_string().contains("--fixture-agent-worker"));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "run inside systemd-run --user --scope -p Delegate=yes; offline loopback replay only"]
fn live_delegated_cgroup_replay_keeps_exited_child_cpu() {
    let project = replay_project(2);
    let result = replay(
        &project,
        "cpu-children",
        &["--json", "--measurements", "measurements"],
    );
    expect_success(&result);
    for replica in 1..=2 {
        let directory = project
            .0
            .join(format!("measurements-concurrency-{replica}"));
        let accounting: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("cpu-accounting.json")).unwrap())
                .unwrap();
        assert_eq!(accounting["status"], "completed", "{accounting}");
        assert_eq!(accounting["cleanup"], "removed", "{accounting}");
        assert!(
            accounting["metrics"]["cpu_user_ms"].as_f64().unwrap() > 100.0,
            "exited CPU children must be retained: {accounting}"
        );
        let components: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("components.json")).unwrap())
                .unwrap();
        assert_eq!(
            components["components"]["agent.execute"]["cpu_accounting_source"],
            "cgroup-v2"
        );
        assert_eq!(
            components["components"]["agent.execute"]["cpu_user_ms"],
            accounting["metrics"]["cpu_user_ms"]
        );
    }
    let failed = replay(&project, "exit", &["--json", "--measurements", "failed"]);
    assert!(!failed.status.success());
    for replica in 1..=2 {
        let accounting: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                project
                    .0
                    .join(format!("failed-concurrency-{replica}/cpu-accounting.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(accounting["status"], "completed", "{accounting}");
        assert_eq!(accounting["cleanup"], "removed", "{accounting}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cgroup_invalid_parent_falls_back_without_changing_the_directory() {
    let project = replay_project(1);
    project.write("unrelated/keep.txt", "retained");
    let parent = project.0.join("unrelated");
    let output = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
            "--measurements",
            "measurements",
            "--json",
        ],
        &[("ASB_CGROUP_PARENT", parent.to_str().unwrap())],
    );
    expect_success(&output);
    let accounting: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/cpu-accounting.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(accounting["status"], "completed");
    assert_eq!(accounting["source"], "proc-sampling");
    assert!(accounting["metrics"]["cpu_user_ms"].is_number());
    assert!(
        accounting["reason"]
            .as_str()
            .unwrap()
            .contains("cgroup-v2 directory")
    );
    assert_eq!(
        std::fs::read_to_string(parent.join("keep.txt")).unwrap(),
        "retained"
    );
    assert_eq!(std::fs::read_dir(parent).unwrap().count(), 1);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires ASB_TEST_BPFTRACE, kernel syscall tracepoints and existing passwordless sudo; offline loopback replay only"]
fn live_bpftrace_socket_profile_captures_replay_and_generates_graphs() {
    let project = replay_project(1);
    let executable = std::env::var_os("ASB_TEST_BPFTRACE")
        .expect("set ASB_TEST_BPFTRACE to a bpftrace executable");
    std::fs::create_dir_all(project.0.join(".asb-tools/bpftrace/bin")).unwrap();
    std::os::unix::fs::symlink(
        executable,
        project.0.join(".asb-tools/bpftrace/bin/bpftrace"),
    )
    .unwrap();
    let mut config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap();
    config.push_str("\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['bpftrace:socket-operations']\n");
    project.write("benchmark.toml", &config);
    project.write("suite.toml", "configs=['benchmark.toml']\n");
    project.write("suite-results/000-benchmark/workspace/src/parser.py", "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
    project.write(
        "tapes/000-benchmark/cassettes/benchmark-pi-bug-fix-test-model.jsonl",
        &std::fs::read_to_string(project.0.join("tape.jsonl")).unwrap(),
    );
    let result = project.command_with_env(
        &[
            "sweep",
            "suite.toml",
            "--replay",
            "--cassette-root",
            "tapes",
        ],
        &[("ASB_FIXTURE_BEHAVIOR", "bpf-clock")],
    );
    expect_success(&result);
    let measurements = project.0.join("suite-results/000-benchmark/measurements");
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(measurements.join("bpftrace-socket-operations.json")).unwrap(),
    )
    .unwrap();
    assert!(
        json["metrics"]["bpftrace.socket-operations.socket_calls"]
            .as_f64()
            .unwrap_or(0.0)
            > 0.0,
        "{json}"
    );
    let metric = "bpftrace.socket-operations.socket_calls";
    let values = json["series"][metric]
        .as_array()
        .expect("timestamped native samples");
    let timestamps = json["series_timestamps"][metric].as_array().unwrap();
    assert!(values.len() >= 2, "{json}");
    assert_eq!(values.len(), timestamps.len());
    assert!(
        timestamps
            .windows(2)
            .all(|pair| pair[1].as_f64().unwrap() > pair[0].as_f64().unwrap())
    );
    assert!(timestamps[0].as_f64().unwrap() >= 0.0);
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(measurements.join("run.json")).unwrap()).unwrap();
    assert!(
        timestamps.last().unwrap().as_f64().unwrap() <= run["elapsed_ms"].as_f64().unwrap() + 100.0
    );
    println!(
        "{}",
        serde_json::json!({"native_bpf_clock_series":metric,"observations":values.len(),"timestamps_ms":timestamps,"final_count":json["metrics"][metric]})
    );
    let comparison = project.0.join("results/suite-comparison");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(comparison.join("comparison.json")).unwrap())
            .unwrap();
    assert!(report["graphs"].as_array().unwrap().iter().any(|graph| {
        let svg = std::fs::read_to_string(comparison.join(graph.as_str().unwrap())).unwrap();
        svg.contains(
            r"<title>bpftrace-socket-operations.bpftrace\.socket-operations\.socket_calls</title>",
        )
    }));
    let html = std::fs::read_to_string(comparison.join("report.html")).unwrap();
    assert!(html.contains("bpftrace-socket-operations.svg"));
    assert!(html.contains("overlaps scheduler states"));
}

#[cfg(unix)]
#[test]
fn qwen_and_openjiuwen_replay_report_agent_usage_in_json_and_measurements() {
    use std::os::unix::fs::PermissionsExt;
    for agent in ["qwen-code", "openjiuwen"] {
        let project = replay_project(1);
        project.write("benchmark.toml", &format!("agent='{agent}'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n"));
        let (binary, path) = if agent == "qwen-code" {
            ("qwen", "/v1/chat/completions")
        } else {
            ("openjiuwen", "/v1/messages")
        };
        let body =
            serde_json::json!({"model":"test-model","messages":[],"tools":[{"type":"function"}],"temperature":0})
                .to_string();
        let cassette = serde_json::json!({"request":{"method":"POST","url":path,"body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
        project.write("tape.jsonl", &format!("{cassette}\n"));
        let executable = format!(".asb-tools/{agent}/bin/{binary}");
        project.write(&executable, r##"#!/usr/bin/env node
const isQwen = process.argv[1].endsWith('/qwen');
const endpoint = isQwen ? process.env.OPENAI_BASE_URL : process.argv[process.argv.indexOf('--api-base') + 1];
const target = endpoint.replace(/\/$/, '') + (isQwen ? '/chat/completions' : '/messages');
fetch(target, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'test-model',messages:[],tools:[{type:'function'}],temperature:0})}).then(async response => {
  if (!response.ok) { process.exitCode = 1; return; }
  require('fs').writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
  if (isQwen) console.log(JSON.stringify([{type:'message',usage:{input_tokens:0,output_tokens:0}},{type:'result',result:'done',usage:{input_tokens:100,output_tokens:5,total_tokens:105}}]));
  else { console.log(JSON.stringify({type:'llm_output',payload:{content:'done'}})); console.log(JSON.stringify({type:'llm_usage',payload:{usage_metadata:{input_tokens:40,output_tokens:3}}})); console.log(JSON.stringify({type:'llm_usage',payload:{usage_metadata:{input_tokens:60,output_tokens:2}}})); }
}).catch(() => { process.exitCode = 1; });
"##);
        std::fs::set_permissions(
            project.0.join(&executable),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let result = replay(
            &project,
            "success",
            &["--json", "--measurements", "measurements"],
        );
        expect_success(&result);
        let json: serde_json::Value = serde_json::from_str(stdout(&result).trim()).unwrap();
        assert_eq!(json["token_usage"]["input_tokens"], 100, "{agent}");
        assert_eq!(json["token_usage"]["output_tokens"], 5, "{agent}");
        assert_eq!(json["token_usage"]["total_tokens"], 105, "{agent}");
        let measurement: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("measurements/run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(measurement["token_usage"]["total_tokens"], 105, "{agent}");
    }
}

#[cfg(unix)]
#[test]
fn hermes_replay_routes_responses_and_collects_usage_ledger() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let project = replay_project(1);
    project.write(
        "benchmark.toml",
        "agent='hermes'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n",
    );
    let body = serde_json::json!({
        "model": "test-model",
        "input": "offline",
        "tools": [{"type": "function"}]
    })
    .to_string();
    let response = serde_json::json!({
        "id": "resp_fixture",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]
    })
    .to_string();
    let cassette = serde_json::json!({
        "request": {"method": "POST", "url": "/v1/responses", "body": body},
        "response": {"status": 200, "headers": {"content-type": "application/json"}, "body": response}
    });
    project.write("tape.jsonl", &format!("{cassette}\n"));
    let executable = ".asb-tools/hermes/bin/hermes";
    project.write(
        executable,
        r##"#!/usr/bin/env node
const fs = require('fs');
const usage = process.argv[process.argv.indexOf('--usage-file') + 1];
const body = {model:'test-model',input:'offline',tools:[{type:'function'}]};
fetch(process.env.OPENAI_BASE_URL.replace(/\/$/, '') + '/responses', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
  .then(response => {
    if (!response.ok) { process.exitCode = 1; return; }
    fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
    fs.mkdirSync(require('path').dirname(usage), {recursive:true});
    fs.writeFileSync(usage, JSON.stringify({input_tokens:12,output_tokens:4,cache_read_tokens:2,total_tokens:18}));
    process.stdout.write('done\n');
  }).catch(() => { process.exitCode = 1; });
"##,
    );
    std::fs::set_permissions(
        project.0.join(executable),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Hermes is a Python adapter. Keep the explicit `bin` launcher used by
    // this fixture, while supplying the recipe's required venv interpreter
    // for execution-plan runtime-input validation.
    let python = project.0.join(".asb-tools/hermes/venv/bin/python");
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    symlink("../../../../.asb/python/venv/bin/python", &python).unwrap();
    let result = replay(
        &project,
        "success",
        &["--json", "--measurements", "measurements"],
    );
    expect_success(&result);
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["token_usage"]["input_tokens"], 12);
    assert_eq!(json["token_usage"]["output_tokens"], 4);
    assert_eq!(json["token_usage"]["cached_tokens"], 2);
    assert_eq!(json["token_usage"]["total_tokens"], 16);
    let measurement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("measurements/run.json")).unwrap())
            .unwrap();
    assert_eq!(measurement["token_usage"], json["token_usage"]);
}

#[cfg(unix)]
#[test]
fn pi_mock_replay_counts_cumulative_snapshots_once_in_every_output() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(1);
    let executable = ".asb-tools/pi/bin/pi";
    project.write(executable, r##"#!/usr/bin/env node
const fs = require('fs');
const endpoint = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json')).providers.asb.baseUrl;
const body = {model:'test-model',input:'offline',tools:[{type:'function'}]};
fetch(endpoint + '/responses', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
  .then(response => {
    if (!response.ok) { process.exitCode = 1; return; }
    fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
    fs.mkdirSync(require('path').dirname(process.env.ASB_PI_SUMMARY_PATH), {recursive:true});
    fs.writeFileSync(process.env.ASB_PI_SUMMARY_PATH, JSON.stringify({schema_version:1,completed:true,usage_complete:true,steps:1,input_tokens:10,output_tokens:3,cached_tokens:0}));
    for (const type of ['message_update', 'message_end', 'turn_end', 'agent_end'])
      console.log(JSON.stringify({type, usage:{input:10,output:3,cacheRead:0,totalTokens:13}}));
  }).catch(() => { process.exitCode = 1; });
"##);
    std::fs::set_permissions(
        project.0.join(executable),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let result = replay(
        &project,
        "success",
        &["--json", "--measurements", "measurements"],
    );
    expect_success(&result);
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
    let measurement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("measurements/run.json")).unwrap())
            .unwrap();
    assert_eq!(json["token_usage"], log["token_usage"]);
    assert_eq!(json["token_usage"], measurement["token_usage"]);
    assert_eq!(json["token_usage"]["input_tokens"], 10);
    assert_eq!(json["token_usage"]["output_tokens"], 3);
    assert_eq!(json["token_usage"]["cached_tokens"], 0);
    assert_eq!(json["token_usage"]["total_tokens"], 13);
}

#[cfg(unix)]
#[test]
fn incomplete_replay_does_not_report_unconsumed_recording_tokens() {
    for (behavior, native_usage) in [
        ("skip", false),
        ("success", false),
        ("mismatch", false),
        ("success", true),
        ("extra", true),
    ] {
        let project = replay_project(1);
        let complete = behavior == "extra";
        if native_usage {
            let agent = ".asb-tools/pi/bin/pi";
            let script = std::fs::read_to_string(project.0.join(agent)).unwrap();
            project.write(
                agent,
                &script.replace(
                    "console.log('fixture-agent-output');",
                    "require('fs').mkdirSync(require('path').dirname(process.env.ASB_PI_SUMMARY_PATH),{recursive:true}); require('fs').writeFileSync(process.env.ASB_PI_SUMMARY_PATH,JSON.stringify({schema_version:1,completed:true,usage_complete:true,steps:1,input_tokens:7,output_tokens:1,cached_tokens:2}));",
                ),
            );
        }
        let body = serde_json::json!({
            "model":"test-model", "input":"offline", "tools":[{"type":"function"}]
        })
        .to_string();
        let interactions = [(10, 2), (30, 4)]
            .into_iter()
            .map(|(input, output)| {
                let response = serde_json::json!({"usage":{
                    "input_tokens":input,"output_tokens":output
                }})
                .to_string();
                serde_json::json!({
                    "request":{"method":"POST","url":"/v1/responses","body":body},
                    "response":{"status":200,"headers":{"content-type":"application/json"},"body":response}
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        project.write("tape.jsonl", &format!("{interactions}\n"));
        let result = replay(
            &project,
            behavior,
            &["--json", "--measurements", "measurements"],
        );
        assert_eq!(result.status.success(), complete, "{}", stderr(&result));
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
        let measurement: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("measurements/run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(log["token_usage"], measurement["token_usage"]);
        if complete {
            assert_eq!(log["token_usage"]["input_tokens"], 40);
            assert_eq!(log["token_usage"]["output_tokens"], 6);
            assert_eq!(log["token_usage"]["total_tokens"], 46);
            assert_eq!(log["token_usage"]["source"], "cassette");
        } else {
            assert!(stderr(&result).contains(if behavior == "mismatch" {
                "replay rejected"
            } else {
                "refusing partial replay"
            }));
            if native_usage {
                assert_eq!(log["token_usage"]["input_tokens"], 7);
                assert_eq!(log["token_usage"]["output_tokens"], 1);
                assert_eq!(log["token_usage"]["cached_tokens"], 2);
                assert_eq!(log["token_usage"]["total_tokens"], 8);
                assert_eq!(log["token_usage"]["source"], "agent-artifact");
                continue;
            }
            // This fake agent emits no native usage. Unknown is honest;
            // counting the recording's unconsumed calls is not.
            for field in [
                "input_tokens",
                "output_tokens",
                "cached_tokens",
                "total_tokens",
            ] {
                assert!(log["token_usage"][field].is_null(), "{behavior}: {log}");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn continue_mock_replay_uses_cassette_usage_in_every_output() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(1);
    project.write("benchmark.toml", "agent='continue'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n");
    let body =
        serde_json::json!({"model":"test-model","messages":[],"tools":[{"type":"function"}]})
            .to_string();
    let response = serde_json::json!({"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}).to_string();
    let cassette = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":response}});
    project.write("tape.jsonl", &format!("{cassette}\n"));
    let executable = ".asb-tools/continue/bin/cn";
    project.write(executable, r##"#!/usr/bin/env node
const body = {model:'test-model',messages:[],tools:[{type:'function'}]};
fetch(process.env.CONTINUE_API_BASE.replace(/\/$/, '') + '/chat/completions', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
  .then(response => { if (!response.ok) { process.exitCode = 1; return; } require('fs').writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n"); console.log(JSON.stringify({response:'done',status:'success'})); })
  .catch(() => { process.exitCode = 1; });
"##);
    std::fs::set_permissions(
        project.0.join(executable),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let result = replay(
        &project,
        "success",
        &["--json", "--measurements", "measurements"],
    );
    expect_success(&result);
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
    let measurement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("measurements/run.json")).unwrap())
            .unwrap();
    assert_eq!(json["token_usage"], log["token_usage"]);
    assert_eq!(json["token_usage"], measurement["token_usage"]);
    assert_eq!(json["token_usage"]["input_tokens"], 10);
    assert_eq!(json["token_usage"]["output_tokens"], 3);
    assert_eq!(json["token_usage"]["cached_tokens"], 4);
    assert_eq!(json["token_usage"]["total_tokens"], 13);
}

#[cfg(unix)]
#[test]
fn continue_and_openhands_collect_isolated_mock_usage_artifacts() {
    use std::os::unix::fs::PermissionsExt;
    for agent in ["continue", "openhands"] {
        let project = replay_project(2);
        // The current main preflight still requires a bridge in replay; seed a
        // local fixture so this offline test never installs from the network.
        let bridge = project
            .0
            .join(".asb-tools/cli2key/node_modules/.bin/codex-bridge");
        std::fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        std::fs::write(&bridge, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&bridge, std::fs::Permissions::from_mode(0o755)).unwrap();
        project.write("benchmark.toml", &format!("agent='{agent}'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n"));
        let body =
            serde_json::json!({"model":"test-model","messages":[],"tools":[{"type":"function"}]})
                .to_string();
        let cassette = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
        project.write("tape.jsonl", &format!("{cassette}\n"));
        let binary = if agent == "continue" {
            "cn"
        } else {
            "openhands"
        };
        let executable = format!(".asb-tools/{agent}/bin/{binary}");
        project.write(&executable, r##"#!/usr/bin/env node
const fs = require('fs'), path = require('path');
const n = Number(process.env.ASB_FIXTURE_INPUT || '10'), isContinue = process.argv[1].endsWith('/cn');
const endpoint = isContinue ? process.env.CONTINUE_API_BASE : process.env.LLM_BASE_URL;
const body = {model:'test-model',messages:[],tools:[{type:'function'}]};
fetch(endpoint.replace(/\/$/, '') + (isContinue ? '/chat/completions' : '/v1/chat/completions'), {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)}).then(response => {
  if (!response.ok) { process.exitCode = 1; return; }
  fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
  if (isContinue) {
    const dir = path.join(process.env.CONTINUE_GLOBAL_DIR, 'sessions'); fs.mkdirSync(dir, {recursive:true});
    const sessionId = '00112233-4455-6677-8899-aabbccddeeff';
    fs.writeFileSync(path.join(dir,sessionId+'.json'), JSON.stringify({sessionId,history:[{message:{role:'assistant',content:'complete',usage:{prompt_tokens:n,completion_tokens:3,prompt_tokens_details:{cache_read_tokens:4}}}}]}));
  } else {
    const dir = path.join(process.env.OPENHANDS_CONVERSATIONS_DIR, 'fixture'); fs.mkdirSync(dir, {recursive:true});
    // LiteLLM adds the separate cache count even when the bridge's wire input already includes it.
    fs.writeFileSync(path.join(dir,'base_state.json'), JSON.stringify({stats:{usage_to_metrics:{agent:{response_latencies:[{}],token_usages:[{prompt_tokens:n+4,completion_tokens:3,cache_read_tokens:4}]}}}}));
  }
  console.log(JSON.stringify({usage:{input_tokens:999,output_tokens:999}}));
}).catch(() => { process.exitCode = 1; });
"##);
        std::fs::set_permissions(
            project.0.join(&executable),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for input in ["10", "20"] {
            let output = project.command_with_env(
                &[
                    "run",
                    "benchmark.toml",
                    "--llm-replay",
                    "--cassette",
                    "tape.jsonl",
                    "--json",
                    "--measurements",
                    "measurements",
                ],
                &[("ASB_FIXTURE_INPUT", input)],
            );
            assert!(
                output.status.success(),
                "{agent}: stdout={} stderr={}",
                stdout(&output),
                stderr(&output)
            );
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let measurement: serde_json::Value = serde_json::from_slice(
                &std::fs::read(project.0.join("measurements/run.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(json["token_usage"], measurement["token_usage"], "{agent}");
            assert_eq!(
                json["token_usage"]["input_tokens"],
                input.parse::<u64>().unwrap(),
                "{agent}"
            );
            assert_eq!(json["token_usage"]["output_tokens"], 3, "{agent}");
            assert_eq!(json["token_usage"]["cached_tokens"], 4, "{agent}");
            assert_eq!(json["token_usage"]["source"], "agent-artifact", "{agent}");
        }
        let recorded = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":8}}}"}});
        project.write("tape.jsonl", &format!("{recorded}\n"));
        let output = replay(&project, "success", &["--json"]);
        assert!(output.status.success(), "{agent}: {}", stderr(&output));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["token_usage"]["input_tokens"], 100, "{agent}");
        assert_eq!(json["token_usage"]["output_tokens"], 7, "{agent}");
        assert_eq!(json["token_usage"]["cached_tokens"], 8, "{agent}");
        assert_eq!(json["token_usage"]["source"], "cassette", "{agent}");
        project.write("tape.jsonl", &format!("{cassette}\n"));
        project.write("benchmark.toml", &format!("agent='{agent}'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nconcurrency=2\nworkspace='workspace'\nlog_file='run.log'\n"));
        let output = replay(&project, "success", &["--json"]);
        assert!(output.status.success(), "{agent}: {}", stderr(&output));
        for index in 1..=2 {
            let log: serde_json::Value = serde_json::from_slice(
                &std::fs::read(project.0.join(format!("run-concurrency-{index}.log"))).unwrap(),
            )
            .unwrap();
            assert_eq!(
                log["token_usage"]["input_tokens"], 10,
                "{agent} replica {index}"
            );
            assert_eq!(
                log["token_usage"]["total_tokens"], 13,
                "{agent} replica {index}"
            );
        }
    }
}

#[cfg(unix)]
fn assert_tape_scratch_cleaned(project: &Project) {
    let scratch = std::fs::read_dir(&project.0)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".asb-tape-")
        })
        .count();
    assert_eq!(scratch, 0, "tape scratch directories must be removed");
}

#[cfg(unix)]
fn assert_tape_rejection_paths_bounded(project: &Project) {
    let evidence = std::fs::read_dir(&project.0)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry.file_type().is_ok_and(|kind| kind.is_dir()) && name.starts_with(".asb-tape-")
                || entry.file_type().is_ok_and(|kind| kind.is_file())
                    && name.starts_with(".asb-tape-")
                    && name.ends_with(".mismatch.json")
        })
        .count();
    assert!(
        evidence <= 1,
        "replay rejection must leave no more than one owned private evidence path"
    );
}

#[cfg(unix)]
#[test]
fn replay_returns_json_and_writes_completed_log_offline() {
    let project = replay_project(1);
    let result = replay(&project, "success", &["--json"]);
    expect_success(&result);
    let summary: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(summary["success_rate"], 1.0);
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
    assert_eq!(log["status"], "completed");
    assert_eq!(summary["task"], log["task"]);
    assert_eq!(summary["task"]["workload"], "bug-fix");
    assert_eq!(summary["task"]["task_id"], "bug-fix");
    assert!(!summary["task"]["attempt_id"].as_str().unwrap().is_empty());
    assert!(
        log["stdout"]
            .as_str()
            .unwrap()
            .contains("fixture-agent-output")
    );
    assert_tape_scratch_cleaned(&project);
}

#[cfg(unix)]
#[test]
fn replay_rejects_duplicate_recorders_before_agent_execution() {
    let project = replay_project(1);
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--llm-replay",
            "--cassette",
            "tape.jsonl",
        ],
        &[
            ("ASB_FIXTURE_BEHAVIOR", "success"),
            ("ASB_PROXY_CHAIN", "llmtape,llmtape"),
        ],
    );
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("exactly once"),
        "{}",
        stderr(&result)
    );
    let source = std::fs::read_to_string(project.0.join("workspace/src/parser.py")).unwrap();
    assert!(
        !source.contains("segment for segment"),
        "agent must not run"
    );
}

#[test]
fn standalone_compare_reports_actual_replay_logs_and_capture_overrides() {
    for concurrency in [1, 2] {
        for measured in [false, true] {
            let project = replay_project(concurrency);
            let options: &[&str] = if measured {
                &["--json", "--measurements", "custom capture"]
            } else {
                &["--json"]
            };
            let output = replay(&project, "success", options);
            assert!(
                output.status.success(),
                "replay fixture failed: {}; {}",
                replay_failure_diagnostic(&output.stdout, &output.stderr, output.status.code()),
                replay_gate_log_diagnostic(&project.0)
            );
            expect_success(&project.command(&["compare", "benchmark.toml"]));
            let directory = project.0.join("results/benchmark-comparison");
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(directory.join("comparison.json")).unwrap())
                    .unwrap();
            let rows = report["benchmarks"].as_array().unwrap();
            assert_eq!(rows.len(), concurrency);
            for row in rows {
                assert_eq!(
                    row["result_source"],
                    if measured { "measurements" } else { "run-log" }
                );
                assert_eq!(row["metrics"]["success_rate"], 1.0);
                assert!(row["metrics"]["elapsed_ms"].as_f64().unwrap() >= 0.0);
                if measured {
                    assert!(
                        row["artifacts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|value| value.as_str().unwrap().contains("lifecycle.json"))
                    );
                }
            }
            assert!(directory.join("metric-success_rate.svg").is_file());
            assert!(directory.join("report.html").is_file());
        }
    }
}

#[cfg(unix)]
fn replay_failure_diagnostic(stdout: &[u8], stderr: &[u8], status: Option<i32>) -> String {
    let stdout = &stdout[..stdout.len().min(65_536)];
    let stderr = &stderr[..stderr.len().min(65_536)];
    let value: Option<serde_json::Value> = serde_json::from_slice(stdout).ok();
    let termination = value
        .as_ref()
        .and_then(|value| value.get("process_termination_diagnostic"))
        .and_then(|value| value.as_str())
        .filter(|value| matches!(*value, "malformed" | "unavailable"))
        .unwrap_or("other-or-absent");
    let reason = value
        .as_ref()
        .and_then(|value| value.get("failure_reason"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let unconsumed = reason.contains("replay consumed 0 of 1 cassette requests");
    format!(
        "exit_code={status:?}; termination={termination}; unconsumed_cassette={unconsumed}; stdout_prefix_bytes={}; stderr_prefix_bytes={}",
        stdout.len(),
        stderr.len()
    )
}

#[cfg(unix)]
fn replay_gate_log_diagnostic(root: &std::path::Path) -> String {
    use std::io::Read;
    let directory = root.join("results/benchmark-runs/run-001");
    let mut entries = Vec::new();
    for name in ["run.log", "run-concurrency-1.log", "run-concurrency-2.log"] {
        let mut bytes = Vec::new();
        if std::fs::File::open(directory.join(name))
            .and_then(|file| file.take(65_536).read_to_end(&mut bytes))
            .is_ok()
        {
            entries.push(replay_gate_log_entry(&bytes));
        }
    }
    format!("gate_logs={entries:?}")
}

#[cfg(unix)]
fn replay_gate_log_entry(bytes: &[u8]) -> String {
    let record = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    let code = record
        .as_ref()
        .and_then(|record| record.get("exit_code"))
        .and_then(|value| value.as_i64())
        .filter(|code| (0..=255).contains(code));
    let stderr = record
        .as_ref()
        .and_then(|record| record.get("stderr"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let gate_error = stderr.contains("asb process gate:");
    let missing = gate_error
        && (stderr.contains("No such file or directory") || stderr.contains("os error 2"));
    let permission =
        gate_error && (stderr.contains("Permission denied") || stderr.contains("os error 13"));
    format!(
        "carrier_code={code:?},gate_error={gate_error},missing={missing},permission={permission}"
    )
}

#[cfg(unix)]
#[test]
fn replay_failure_diagnostic_is_bounded_and_does_not_expose_contents() {
    let stdout = br#"{"process_termination_diagnostic":"malformed","failure_reason":"replay consumed 0 of 1 cassette requests secret-token /private/path"}"#;
    let diagnostic = replay_failure_diagnostic(stdout, &[b'x'; 70_000], Some(1));
    assert!(diagnostic.contains("termination=malformed"));
    assert!(diagnostic.contains("unconsumed_cassette=true"));
    assert!(diagnostic.contains("stderr_prefix_bytes=65536"));
    assert!(!diagnostic.contains("secret-token") && !diagnostic.contains("/private/path"));
    let unknown = replay_failure_diagnostic(
        br#"{"process_termination_diagnostic":"secret-token"}"#,
        b"",
        None,
    );
    assert!(unknown.contains("termination=other-or-absent"));
    assert!(!unknown.contains("secret-token"));
    let entry = replay_gate_log_entry(br#"{"exit_code":127,"stderr":"asb process gate: No such file or directory /private/secret"}"#);
    assert!(entry.contains("carrier_code=Some(127)") && entry.contains("missing=true"));
    assert!(!entry.contains("/private/secret"));
}

#[cfg(unix)]
#[test]
fn offline_replay_writes_ordered_lifecycle_events_for_all_executed_phases() {
    let project = replay_project(1);
    let result = replay(&project, "success", &["--measurements", "measurements"]);
    expect_success(&result);
    let records: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/lifecycle.json")).unwrap(),
    )
    .unwrap();
    let events = records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["event"].clone())
        .collect::<Vec<_>>();
    let expected = vec![
        serde_json::json!({"kind":"run_started"}),
        serde_json::json!({"kind":"phase_begin","phase":"provider_discovery"}),
        serde_json::json!({"kind":"phase_end","phase":"provider_discovery"}),
        serde_json::json!({"kind":"phase_begin","phase":"provider_setup"}),
        serde_json::json!({"kind":"phase_end","phase":"provider_setup"}),
        serde_json::json!({"kind":"phase_begin","phase":"workload_preparation"}),
        serde_json::json!({"kind":"phase_end","phase":"workload_preparation"}),
        serde_json::json!({"kind":"phase_begin","phase":"proxy_preparation"}),
        serde_json::json!({"kind":"phase_end","phase":"proxy_preparation"}),
        serde_json::json!({"kind":"phase_begin","phase":"agent_preparation"}),
        serde_json::json!({"kind":"phase_end","phase":"agent_preparation"}),
        serde_json::json!({"kind":"measurements_ready"}),
        serde_json::json!({"kind":"agent_spawn"}),
        serde_json::json!({"kind":"agent_execute_begin"}),
        serde_json::json!({"kind":"agent_started"}),
        serde_json::json!({"kind":"agent_execute_end"}),
        serde_json::json!({"kind":"agent_exited"}),
        serde_json::json!({"kind":"phase_begin","phase":"workload_evaluation"}),
        serde_json::json!({"kind":"phase_end","phase":"workload_evaluation"}),
        serde_json::json!({"kind":"phase_begin","phase":"workload_cleanup"}),
        serde_json::json!({"kind":"phase_end","phase":"workload_cleanup"}),
        serde_json::json!({"kind":"run_finishing"}),
    ];
    assert_eq!(events, expected);
    for (index, record) in records.as_array().unwrap().iter().enumerate() {
        assert_eq!(record["sequence"].as_u64(), Some(index as u64));
    }
}

#[cfg(unix)]
#[test]
fn failed_agent_spawn_persists_failure_lifecycle_events() {
    let project = replay_project(1);
    project.write(
        ".asb-tools/pi/bin/pi",
        "#!/nonexistent/asb-fixture-interpreter\n",
    );
    let result = replay(&project, "success", &["--measurements", "measurements"]);
    assert!(!result.status.success());
    let records: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/lifecycle.json")).unwrap(),
    )
    .unwrap();
    let kinds = records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["event"]["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        kinds.ends_with(&[
            "agent_spawn",
            "agent_execute_begin",
            "agent_execute_end",
            "agent_spawn_failed",
            "run_failed"
        ]),
        "{kinds:?}"
    );
}

#[cfg(unix)]
#[test]
fn failed_workload_preparation_closes_its_span_and_records_failure() {
    let project = replay_project(1);
    project.write("benchmark.toml", "agent='pi'\nworkload='missing-workload'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n");
    let result = replay(&project, "success", &["--measurements", "measurements"]);
    assert!(!result.status.success());
    let records: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/lifecycle.json")).unwrap(),
    )
    .unwrap();
    let events = records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["event"].clone())
        .collect::<Vec<_>>();
    assert!(
        events.ends_with(&[
            serde_json::json!({"kind":"phase_begin","phase":"workload_preparation"}),
            serde_json::json!({"kind":"phase_end","phase":"workload_preparation"}),
            serde_json::json!({"kind":"phase_failed","phase":"workload_preparation"}),
            serde_json::json!({"kind":"run_failed"}),
        ]),
        "{events:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires project-local Perfetto and host tracing privileges"]
fn offline_replay_emits_every_success_lifecycle_phase_into_a_real_perfetto_trace() {
    let tools = PathBuf::from(std::env::var_os("ASB_TEST_PERFETTO_TOOLS").unwrap());
    let project = replay_project(1);
    std::fs::create_dir_all(project.0.join(".asb-tools")).unwrap();
    std::os::unix::fs::symlink(
        tools.join("perfetto"),
        project.0.join(".asb-tools/perfetto"),
    )
    .unwrap();
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
            "--measurements",
            "measurements",
        ],
        &[
            ("ASB_FIXTURE_BEHAVIOR", "success"),
            ("ASB_MEASURE_PERFETTO", "1"),
        ],
    );
    expect_success(&result);
    let trace = project
        .0
        .join("measurements/perfetto-scheduling.perfetto-trace");
    let output = Command::new(tools.join("perfetto/bin/trace_processor_shell"))
        .arg("query")
        .arg(trace)
        .arg("SELECT name, dur FROM slice WHERE name GLOB 'asb.*' ORDER BY ts")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let rows = stdout(&output);
    let names = [
        "asb.run.started.",
        "asb.provider.discovery.",
        "asb.provider.setup.",
        "asb.workload.prepare.",
        "asb.proxy.prepare.",
        "asb.agent.prepare.",
        "asb.measurements.ready.",
        "asb.agent.spawn.",
        "asb.agent.execute.",
        "asb.agent.started.",
        "asb.agent.exited.",
        "asb.workload.evaluate.",
        "asb.run.finishing.",
    ];
    let mut cursor = 0;
    for name in names {
        let offset = rows[cursor..]
            .find(name)
            .unwrap_or_else(|| panic!("missing {name}: {rows}"));
        cursor += offset + name.len();
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires project-local Perfetto and host tracing privileges"]
fn live_perfetto_preserves_failure_markers_and_closes_the_collector_on_early_error() {
    let tools = PathBuf::from(std::env::var_os("ASB_TEST_PERFETTO_TOOLS").unwrap());
    let project = replay_project(1);
    project.write("benchmark.toml", "agent='pi'\nworkload='missing-workload'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n");
    std::fs::create_dir_all(project.0.join(".asb-tools")).unwrap();
    std::os::unix::fs::symlink(
        tools.join("perfetto"),
        project.0.join(".asb-tools/perfetto"),
    )
    .unwrap();
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
            "--measurements",
            "measurements",
        ],
        &[
            ("ASB_FIXTURE_BEHAVIOR", "success"),
            ("ASB_MEASURE_PERFETTO", "1"),
        ],
    );
    assert!(!result.status.success());
    let trace = project
        .0
        .join("measurements/perfetto-scheduling.perfetto-trace");
    let output = Command::new(tools.join("perfetto/bin/trace_processor_shell"))
        .arg("query")
        .arg(trace)
        .arg("SELECT name, dur FROM slice WHERE name GLOB 'asb.*' ORDER BY ts")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let rows = stdout(&output);
    let begun = rows
        .find("asb.workload.prepare.")
        .expect("missing workload span");
    let failed = rows
        .find("asb.workload.prepare.failed.")
        .expect("missing workload failure marker");
    let run_failed = rows
        .find("asb.run.failed.")
        .expect("missing run failure marker");
    assert!(begun < failed && failed < run_failed, "{rows}");
}

#[cfg(unix)]
#[test]
fn show_output_streams_mock_agent_output_on_replay() {
    let project = replay_project(1);
    let result = replay(&project, "success", &["--show-output"]);
    expect_success(&result);
    assert!(stdout(&result).contains("fixture-agent-output"));
    assert!(stdout(&result).contains("success_rate=1.000"));
}

#[cfg(unix)]
#[test]
fn replay_rejects_changed_and_extra_requests_and_cleans_up() {
    for behavior in ["mismatch", "extra"] {
        let project = replay_project(1);
        let result = replay(&project, behavior, &[]);
        expect_failure(&result, "replay rejected");
        assert_tape_rejection_paths_bounded(&project);
    }
}

#[cfg(unix)]
#[test]
fn replay_rejects_partially_consumed_cassettes() {
    let project = replay_project(1);
    let result = replay(&project, "skip", &[]);
    expect_failure(&result, "replay consumed 0 of 1");
    assert_tape_scratch_cleaned(&project);
}

#[cfg(unix)]
fn install_inert_replay_bridge(project: &Project) {
    use std::os::unix::fs::PermissionsExt;
    let bridge = ".asb-tools/cli2key/node_modules/.bin/codex-bridge";
    project.write(bridge, "#!/bin/sh\nexit 99\n");
    std::fs::set_permissions(
        project.0.join(bridge),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn pi_tool_message_is_not_a_terminal_refusal() {
    let project = replay_project(1);
    install_inert_replay_bridge(&project);
    let agent = project.0.join(".asb-tools/pi/bin/pi");
    let script = std::fs::read_to_string(&agent).unwrap().replace(
        "console.log('fixture-agent-output');",
        "require('fs').writeFileSync(process.env.ASB_PI_SUMMARY_PATH,JSON.stringify({schema_version:1,completed:true,steps:1,usage_complete:false,final_answer:'Finished the work.'})); console.log(JSON.stringify({type:'message',role:'tool',content:\"I can't run tests is fixture output\"}));",
    );
    std::fs::write(&agent, script).unwrap();
    let result = replay(&project, "success", &[]);
    expect_success(&result);
}

#[cfg(unix)]
#[test]
fn pi_intermediate_assistant_text_is_not_a_terminal_refusal() {
    let project = replay_project(1);
    install_inert_replay_bridge(&project);
    let agent = project.0.join(".asb-tools/pi/bin/pi");
    let script = std::fs::read_to_string(&agent).unwrap().replace(
        "console.log('fixture-agent-output');",
        r#"require('fs').writeFileSync(process.env.ASB_PI_SUMMARY_PATH,JSON.stringify({schema_version:1,completed:true,steps:1,usage_complete:false,final_answer:'Finished the work.'})); console.log(JSON.stringify({type:'message_update',assistantMessageEvent:{type:'text',text:"I can't run tests yet"}}));"#,
    );
    std::fs::write(&agent, script).unwrap();
    let result = replay(&project, "success", &[]);
    expect_success(&result);
}

#[cfg(unix)]
#[test]
fn codex_grading_uses_owned_final_file_not_intermediate_jsonl() {
    use std::os::unix::fs::PermissionsExt;
    for write_final in [false, true] {
        let project = replay_project(1);
        project.write("benchmark.toml", "agent='codex'\nworkload='repository-navigation'\nprovider='openrouter'\nmodel='test-model'\nconcurrency=1\nworkspace='workspace'\nlog_file='run.log'\n");
        let agent = ".asb-tools/codex/bin/codex";
        project.write(agent, r##"#!/usr/bin/env node
const fs = require('fs');
process.stdin.resume();
process.stdin.on('end', async () => {
  const flag = process.argv.indexOf('--output-last-message');
  if (flag < 0 || !process.argv[flag + 1]) throw new Error('missing final-message path');
  const base = process.env.ASB_PROVIDER_BASE_URL.replace(/\/$/, '').replace(/\/v1$/, '');
  const body = {model:'test-model', input:'offline', tools:[{type:'function'}]};
  const response = await fetch(base + '/v1/responses', {method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify(body)});
  if (!response.ok) throw new Error('fixture proxy failed');
  await response.text();
  console.log(JSON.stringify({type:'item.completed',item:{type:'agent_message',text:'src/cli.py src/registry.py lookup'}}));
  console.log(JSON.stringify({type:'turn.completed',usage:{input_tokens:1,output_tokens:1}}));
  if (process.env.ASB_FIXTURE_BEHAVIOR === 'write-final') fs.writeFileSync(process.argv[flag + 1], 'src/cli.py src/registry.py lookup');
});
"##);
        std::fs::set_permissions(
            project.0.join(agent),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let result = replay(
            &project,
            if write_final {
                "write-final"
            } else {
                "no-final"
            },
            &[],
        );
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
        assert_eq!(
            log["workload_success"],
            write_final,
            "stdout={} stderr={}",
            stdout(&result),
            stderr(&result)
        );
        assert_eq!(
            result.status.success(),
            write_final,
            "stdout={} stderr={}",
            stdout(&result),
            stderr(&result)
        );
    }
}

#[cfg(unix)]
#[test]
fn pi_terminal_refusal_is_reported_without_using_tool_output() {
    let project = replay_project(1);
    install_inert_replay_bridge(&project);
    let result = replay(&project, "terminal-refusal", &[]);
    expect_failure(&result, "agent reported it could not run tests");
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
    assert_eq!(log["process_success"], false);
    assert_eq!(log["workload_success"], true);
}

#[cfg(unix)]
#[test]
fn replay_reports_agent_exit_and_zero_exit_fatal_output() {
    for behavior in ["exit", "fatal"] {
        let project = replay_project(1);
        let result = replay(&project, behavior, &[]);
        let expected = if behavior == "exit" {
            "agent exited unsuccessfully (exit status: 7)"
        } else {
            "agent reported an authentication failure"
        };
        expect_failure(&result, expected);
        assert!(!stderr(&result).contains("fixture-secret"));
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
        assert_eq!(log["status"], "failed");
        assert_eq!(log["failure_kind"], "agent_failure");
        // Both fixtures leave the correct patch before failing as processes.
        // Task resolution must survive the independent operational failure.
        assert_eq!(log["process_success"], false);
        assert_eq!(log["workload_success"], true);
        assert_eq!(log["success_rate"], 1.0);
        assert_eq!(log["benchmark_outcome"]["execution_success"], false);
        assert_eq!(log["module"]["id"], log["workload"]);
        assert!(log["module"]["protocol"].is_object());
        assert_eq!(
            log["benchmark_outcome"]["evaluation"]["status"],
            "completed"
        );
        assert_eq!(log["benchmark_outcome"]["evaluation"]["resolution"], true);
        assert_eq!(
            log["benchmark_outcome"]["evaluation"]["rewards"]["resolution"],
            1.0
        );
        assert_eq!(
            log["benchmark_outcome"]["lineage"]["execution_attempt"],
            log["task"]["attempt_id"]
        );
        assert_tape_scratch_cleaned(&project);
    }
}

#[cfg(unix)]
#[test]
fn replay_does_not_classify_pi_text_as_an_error_event() {
    let project = replay_project(1);
    let result = replay(&project, "api-error-event", &[]);
    expect_success(&result);
}

#[cfg(unix)]
#[test]
fn zero_exit_without_workload_change_is_not_success() {
    let project = replay_project(1);
    let result = replay(&project, "delete", &[]);
    expect_failure(&result, "required output file `src/parser.py` is missing");
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
    assert_eq!(log["process_success"], true);
    assert_eq!(log["workload_success"], false);
    assert_eq!(log["workload_evaluation"], "bundled-fixture");
    assert_eq!(log["failure_kind"], "workload_check_failed");
    assert!(
        log["failure_reason"]
            .as_str()
            .unwrap()
            .contains("src/parser.py")
    );
    assert_eq!(log["success_rate"], 0.0);
}

#[cfg(unix)]
#[test]
fn no_op_zero_exit_cannot_resolve_a_failing_task() {
    let project = replay_project(1);
    let result = replay(&project, "noop", &["--json"]);
    assert!(!result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["process_success"], true);
    assert_eq!(json["workload_baseline_passed"], false);
    assert_eq!(json["workload_success"], false);
    assert_eq!(json["success_rate"], 0.0);
}

#[cfg(unix)]
#[test]
fn pre_solved_workspace_is_preserved_but_never_earns_resolution_credit() {
    let project = replay_project(1);
    project.write(
        "workspace/src/parser.py",
        "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
    );
    let before = std::fs::read(project.0.join("workspace/src/parser.py")).unwrap();
    let result = replay(
        &project,
        "noop",
        &["--json", "--measurements", "measurements"],
    );
    expect_success(&result); // operation completed, not a resolved benchmark
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["workload_baseline_passed"], true);
    assert_eq!(json["workload_evaluation"], "invalid-baseline");
    assert!(json["workload_success"].is_null());
    assert!(json["success_rate"].is_null());
    assert_eq!(
        std::fs::read(project.0.join("workspace/src/parser.py")).unwrap(),
        before
    );
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["task_count"], 0);
    assert_eq!(summary["unscored_count"], 1);
    let human = replay(&project, "noop", &[]);
    assert!(stdout(&human).contains("already passes before the agent starts"));
    assert!(stdout(&human).contains("Use a fresh workspace"));
}

#[cfg(unix)]
#[test]
fn external_smoke_cannot_report_upstream_task_success_or_invent_steps() {
    use std::os::unix::fs::PermissionsExt;

    for passed in [false, true] {
        let project = replay_project(1);
        // This is sealed replay coverage.  The final human-output invocation
        // must not fall through to a networked npm install of cli2key before
        // it reaches the cassette-backed provider path.
        let bridge = project
            .0
            .join(".asb-tools/cli2key/node_modules/.bin/codex-bridge");
        std::fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        std::fs::write(&bridge, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bridge, std::fs::Permissions::from_mode(0o755)).unwrap();
        project.write("workloads/agentbench.txt", "Local adapter smoke only.\n");
        project.write("benchmark.toml", "agent='pi'\nworkload='agentbench'\nprovider='cli2key'\nmodel='test-model'\nworkspace='workspace'\nlog_file='run.log'\n");
        project.write("workspace/external-workload/asb-trial/src/task.py", if passed {
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace(' ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n"
        } else {
            "def normalize(value: str) -> str:\n    return value\n\ndef suite_label() -> str:\n    return \"agentbench\"\n"
        });
        let result = replay(
            &project,
            "success",
            &["--json", "--measurements", "measurements"],
        );
        assert_eq!(result.status.success(), passed, "{}", stderr(&result));
        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(json["process_success"], true);
        assert_eq!(json["workload_evaluation"], "adapter-smoke");
        assert_eq!(json["adapter_smoke_success"], passed);
        assert!(json["workload_success"].is_null());
        assert!(json["success_rate"].is_null());
        assert!(json["steps"].is_null());
        let measured: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("measurements/run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(measured["success"], false);
        assert_eq!(measured["adapter_smoke_success"], passed);
        let summary: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("measurements/summary.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(summary["task_count"], 0);
        assert_eq!(summary["unscored_count"], 1);
        assert!(summary["success_rate"].is_null());
        let human = replay(&project, "success", &[]);
        assert!(stdout(&human).contains("not an upstream benchmark task"));
        assert!(stdout(&human).contains("steps=unknown"));
    }
}

#[cfg(unix)]
#[test]
fn resolution_summary_counts_failed_attempts_without_survivorship_bias() {
    let project = replay_project(1);
    expect_success(&replay(
        &project,
        "success",
        &["--measurements", "measurements"],
    ));
    project.write(
        "workspace/src/parser.py",
        "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n",
    );
    let failed = replay(
        &project,
        "exit-unresolved",
        &["--measurements", "measurements"],
    );
    assert!(!failed.status.success());
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["task_count"], 2);
    assert_eq!(summary["success_count"], 1);
    assert_eq!(summary["failure_count"], 1);
    assert_eq!(summary["success_rate"], 0.5);
}

#[cfg(unix)]
#[test]
fn failed_client_can_resolve_task_without_hiding_its_process_failure() {
    let project = replay_project(1);
    let result = replay(
        &project,
        "exit",
        &["--json", "--measurements", "measurements"],
    );
    expect_failure(&result, "agent exited unsuccessfully");
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["process_success"], false);
    assert_eq!(json["workload_success"], true);
    assert_eq!(json["success_rate"], 1.0);
    assert_eq!(
        json["workload_baseline"],
        serde_json::json!({"kind":"measured","passed":false})
    );
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["success_count"], 1);
    assert_eq!(summary["runs"][0]["status"], "failed");
    assert_eq!(summary["runs"][0]["process_success"], false);
}

#[cfg(unix)]
#[test]
fn failed_client_and_failed_grading_retain_both_diagnostics() {
    let project = replay_project(1);
    let result = replay(
        &project,
        "exit-unresolved",
        &["--json", "--measurements", "measurements"],
    );
    expect_failure(&result, "agent exited unsuccessfully");
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["process_success"], false);
    assert_eq!(json["workload_success"], false);
    assert_eq!(json["failure_kind"], "agent_failure");
    assert_eq!(json["workload_failure"]["kind"], "workload_check_failed");
    assert!(
        json["workload_failure"]["reason"]
            .as_str()
            .unwrap()
            .contains("src/parser.py")
    );
    assert_eq!(json["success_rate"], 0.0);
}

#[cfg(unix)]
#[test]
fn grader_failures_and_errors_are_specific_in_human_and_json_output() {
    for (behavior, kind, success, expected) in [
        (
            "mismatch-solution",
            "workload_check_failed",
            Some(false),
            "does not match an accepted solution",
        ),
        (
            "not-a-file",
            "workload_check_failed",
            Some(false),
            "is not a file",
        ),
        (
            "invalid-utf8",
            "grader_error",
            None,
            "cannot read workload implementation src/parser.py",
        ),
    ] {
        let project = replay_project(1);
        let result = replay(&project, behavior, &["--json"]);
        expect_failure(&result, expected);
        let json: serde_json::Value = serde_json::from_str(stdout(&result).trim()).unwrap();
        assert_eq!(json["process_success"], true);
        assert_eq!(json["workload_success"], serde_json::json!(success));
        assert_eq!(json["failure_kind"], kind);
        assert!(json["failure_reason"].as_str().unwrap().contains(expected));
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.0.join("run.log")).unwrap()).unwrap();
        assert_eq!(log["failure_kind"], kind);
        assert!(log["failure_reason"].as_str().unwrap().contains(expected));
    }
}

#[cfg(unix)]
#[test]
fn concurrent_grading_failures_are_deduplicated_and_recorded_per_replica() {
    let project = replay_project(4);
    let result = replay(&project, "delete", &[]);
    expect_failure(&result, "replicas 1, 2, 3, 4");
    let error = stderr(&result);
    assert_eq!(
        error
            .matches("required output file `src/parser.py` is missing")
            .count(),
        1,
        "{error}"
    );
    for index in 1..=4 {
        let log: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join(format!("run-concurrency-{index}.log"))).unwrap(),
        )
        .unwrap();
        assert_eq!(log["process_success"], true);
        assert_eq!(log["workload_success"], false);
        assert_eq!(log["failure_kind"], "workload_check_failed");
    }
}

#[cfg(unix)]
#[test]
fn grading_failure_is_reported_in_measurements_and_lifecycle() {
    let project = replay_project(1);
    let result = replay(&project, "delete", &["--measurements", "measurements"]);
    expect_failure(&result, "required output file `src/parser.py` is missing");
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.0.join("measurements/run.json")).unwrap())
            .unwrap();
    assert_eq!(run["failure_kind"], "workload_check_failed");
    assert!(
        run["failure_reason"]
            .as_str()
            .unwrap()
            .contains("src/parser.py")
    );
    let events: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("measurements/lifecycle.json")).unwrap(),
    )
    .unwrap();
    let events = events
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["event"].clone())
        .collect::<Vec<_>>();
    let position =
        |event: serde_json::Value| events.iter().position(|actual| actual == &event).unwrap();
    let begin = position(serde_json::json!({"kind":"phase_begin","phase":"workload_evaluation"}));
    let end = position(serde_json::json!({"kind":"phase_end","phase":"workload_evaluation"}));
    let failed = position(serde_json::json!({"kind":"phase_failed","phase":"workload_evaluation"}));
    let run_failed = position(serde_json::json!({"kind":"run_failed"}));
    assert!(
        begin < end && end < failed && failed < run_failed,
        "{events:?}"
    );
}

#[cfg(unix)]
#[test]
fn concurrent_replay_isolates_replica_outputs_and_reports_partial_failure() {
    let project = replay_project(2);
    let result = replay(&project, "fail-second", &[]);
    expect_failure(&result, "1 concurrent benchmark replica(s) failed");
    assert!(
        stderr(&result).contains("replica 2:"),
        "{}",
        stderr(&result)
    );
    for (index, status) in [(1, "completed"), (2, "failed")] {
        let file = project.0.join(format!("run-concurrency-{index}.log"));
        let log: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(log["status"], status);
        assert!(
            project
                .0
                .join(format!("workspace-concurrency-{index}"))
                .is_dir()
        );
    }
    assert_tape_scratch_cleaned(&project);
}

#[cfg(unix)]
#[test]
fn concurrent_perf_permission_failure_reports_remediation_only_once() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(3);
    let perf = project.0.join(".asb-tools/perf/bin/perf");
    project.write(
        ".asb-tools/perf/bin/perf",
        "#!/bin/sh\necho 'Permission denied' >&2\nexit 1\n",
    );
    std::fs::set_permissions(perf, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['perf']\n";
    project.write("benchmark.toml", &config);
    for json in [false, true] {
        let mut args = vec!["--measurements", "measurements"];
        if json {
            args.push("--json");
        }
        let result = replay(&project, "success", &args);
        let error = stderr(&result);
        expect_failure(&result, "3 concurrent benchmark replica(s) failed");
        assert_eq!(
            error
                .matches("sudo sysctl -w kernel.perf_event_paranoid=-1")
                .count(),
            1,
            "{error}"
        );
        assert!(error.contains("replicas 1, 2, 3:"), "{error}");
        if json {
            assert!(stdout(&result).is_empty());
        }
    }
}

#[cfg(unix)]
#[test]
fn concurrent_bpftrace_sudo_help_and_sweep_failures_are_reported_once() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(3);
    for name in [".asb-tools/bpftrace/bin/bpftrace", "bin/sudo"] {
        project.write(name, "#!/bin/sh\nexit 1\n");
        std::fs::set_permissions(project.0.join(name), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let path = format!(
        "{}:{}",
        project.0.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['bpftrace']\n";
    project.write("first.toml", &config);
    project.write("second.toml", &config);
    project.write("suite.toml", "configs=['first.toml','second.toml']\n");
    // A missing replay cassette is validated before collector admission. This
    // fixture exercises the collector preflight, so execute its normal path.
    let result =
        project.command_with_env(&["sweep", "suite.toml", "--verbose"], &[("PATH", &path)]);
    let error = stderr(&result);
    expect_failure(&result, "2 failed benchmark(s)");
    assert_eq!(
        error.matches("passwordless sudo is unavailable").count(),
        1,
        "{error}"
    );
    assert_eq!(error.matches("What to do:").count(), 1, "{error}");
    assert!(
        error.contains("first.toml") && error.contains("second.toml"),
        "{error}"
    );
    assert!(error.contains("replicas 1, 2, 3:"), "{error}");
}

#[cfg(unix)]
#[test]
fn concurrent_sysstat_permission_preflight_is_deduplicated_before_agent_start() {
    use std::os::unix::fs::PermissionsExt;
    for collector in ["mpstat", "iostat"] {
        let project = replay_project(4);
        let pidstat = ".asb-tools/mpstat/bin/pidstat";
        project.write(pidstat, "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(
            project.0.join(pidstat),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
            + &format!(
                "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['{collector}']\n"
            );
        project.write("benchmark.toml", &config);
        let result = replay(&project, "success", &[]);
        let error = stderr(&result);
        expect_failure(&result, "4 concurrent benchmark replica(s) failed");
        assert_eq!(
            error.matches("cannot execute pidstat").count(),
            1,
            "{error}"
        );
        assert_eq!(error.matches("chmod u+x").count(), 1, "{error}");
        assert!(error.contains("replicas 1, 2, 3, 4"), "{error}");
        assert!(!project.0.join("run-concurrency-1.log").exists());
    }
}

#[cfg(unix)]
#[test]
fn cpu_and_io_collectors_use_project_local_pidstat_in_preflight_and_run() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(1);
    let calls = project.0.join("pidstat-calls");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
        calls.display()
    );
    project.write(".asb-tools/mpstat/bin/pidstat", &script);
    std::fs::set_permissions(
        project.0.join(".asb-tools/mpstat/bin/pidstat"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['mpstat','iostat']\n";
    project.write("benchmark.toml", &config);
    let result = replay(&project, "success", &[]);
    expect_success(&result);
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(
        calls.lines().filter(|line| line.starts_with("-u ")).count() >= 2,
        "{calls}"
    );
    assert!(
        calls.lines().filter(|line| line.starts_with("-d ")).count() >= 2,
        "{calls}"
    );
}

#[test]
fn host_reconfiguration_preserves_project_data_and_rejects_reset_combination() {
    let project = Project::new();
    project.init();
    project.write("benchmark.toml", "retained benchmark configuration\n");
    project.write(".asb-tools/user-tool", "retained installed tool\n");
    project.write("results/user-result.json", "{\"retained\":true}\n");
    let before: toml::Value =
        toml::from_str(&std::fs::read_to_string(project.0.join(".asb/project.toml")).unwrap())
            .unwrap();
    // The ordinary command helper supplies EOF, so all privileged changes
    // are declined. This is a configuration-preservation test, not host setup.
    let result = project.command(&["init", "--configure-host"]);
    expect_success(&result);
    assert!(
        stdout(&result)
            .contains("existing tools, workloads, configurations and results were preserved")
    );
    let after: toml::Value =
        toml::from_str(&std::fs::read_to_string(project.0.join(".asb/project.toml")).unwrap())
            .unwrap();
    for field in ["tools_dir", "workloads_dir", "workspace_dir", "proxy"] {
        assert_eq!(before.get(field), after.get(field));
    }
    assert_eq!(
        std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap(),
        "retained benchmark configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join(".asb-tools/user-tool")).unwrap(),
        "retained installed tool\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join("results/user-result.json")).unwrap(),
        "{\"retained\":true}\n"
    );
    expect_failure(
        &project.command(&["init", "--configure-host", "--reset"]),
        "cannot be used with",
    );
}

#[cfg(unix)]
#[test]
fn concurrent_perf_record_preflight_reports_permission_once() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(4);
    let perf = ".asb-tools/perf/bin/perf";
    project.write(
        perf,
        "#!/bin/sh\nif [ \"$1\" = stat ]; then exit 0; fi\necho 'Permission denied' >&2\nexit 1\n",
    );
    std::fs::set_permissions(project.0.join(perf), std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['perf']\n";
    project.write("benchmark.toml", &config);
    let result = replay(&project, "success", &[]);
    let error = stderr(&result);
    expect_failure(&result, "4 concurrent benchmark replica(s) failed");
    assert_eq!(
        error
            .matches("sudo sysctl -w kernel.perf_event_paranoid=-1")
            .count(),
        1,
        "{error}"
    );
    assert!(!project.0.join("run-concurrency-1.log").exists());
}

#[cfg(unix)]
#[test]
fn installing_flamegraph_installs_perf_dependency_without_replacing_existing_perf() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    project.write("host-bin/perf", "#!/bin/sh\necho host-perf\n");
    project.write("flamegraph-source", "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(
        project.0.join("host-bin/perf"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fake_perf_distribution(&project, "host-bin");
    let path = format!(
        "{}:{}",
        project.0.join("host-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let source = project.0.join("flamegraph-source");
    let source = source.to_str().unwrap();
    let dry = project.command_with_env(
        &[
            "tool",
            "install",
            "flamegraph",
            "--source",
            source,
            "--dry-run",
        ],
        &[("PATH", &path)],
    );
    expect_success(&dry);
    assert!(stdout(&dry).contains("Required dependency perf"));
    let perf = project.0.join(".asb-tools/perf/bin/perf");
    assert!(!perf.exists());
    let installed = project.command_with_env(
        &["tool", "install", "flamegraph", "--source", source],
        &[("PATH", &path)],
    );
    expect_success(&installed);
    assert_eq!(
        std::fs::read_to_string(&perf).unwrap(),
        "#!/bin/sh\necho host-perf\n"
    );
    std::fs::write(&perf, "#!/bin/sh\necho preserved\n").unwrap();
    let repeat = project.command_with_env(
        &["tool", "install", "flamegraph", "--source", source],
        &[("PATH", &path)],
    );
    expect_success(&repeat);
    assert_eq!(
        std::fs::read_to_string(&perf).unwrap(),
        "#!/bin/sh\necho preserved\n"
    );
}

#[cfg(unix)]
#[test]
fn perf_installs_from_distribution_when_host_perf_is_unusable() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    for (name, script) in [
        ("perf", "#!/bin/sh\nexit 1\n"),
        (
            "apt-cache",
            "#!/bin/sh\necho '  Depends: linux-test-tools-1'\n",
        ),
        ("apt-get", "#!/bin/sh\ntouch package.deb\n"),
        (
            "dpkg-deb",
            "#!/bin/sh\nmkdir -p \"$3/usr/lib/linux-tools/test\"\nprintf '#!/bin/sh\\necho perf version fixture\\n' > \"$3/usr/lib/linux-tools/test/perf\"\nchmod +x \"$3/usr/lib/linux-tools/test/perf\"\n",
        ),
    ] {
        project.write(&format!("fake-bin/{name}"), script);
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let path = format!(
        "{}:{}",
        project.0.join("fake-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let result = project.command_with_env(&["tool", "install", "perf"], &[("PATH", &path)]);
    expect_success(&result);
    let installed = project.0.join(".asb-tools/perf/bin/perf");
    assert!(installed.is_file());
    assert!(
        Command::new(installed)
            .arg("--version")
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn failed_perf_install_preserves_existing_project_binary() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    for name in ["perf", "apt-get", "curl"] {
        project.write(&format!("fake-bin/{name}"), "#!/bin/sh\nexit 1\n");
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    project.write(".asb-tools/perf/bin/perf", "existing perf");
    let path = format!(
        "{}:{}",
        project.0.join("fake-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let result = project.command_with_env(&["tool", "install", "perf"], &[("PATH", &path)]);
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("Could not build upstream perf"),
        "{}",
        stderr(&result)
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join(".asb-tools/perf/bin/perf")).unwrap(),
        "existing perf"
    );
}

#[cfg(unix)]
#[test]
fn perf_rejects_forged_checksum_output_before_building_unverified_source() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    for (name, script) in [
        ("apt-get", "#!/bin/sh\nexit 1\n"),
        (
            "curl",
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; printf fixture > \"$1\"; exit 0; fi; shift; done\nexit 1\n",
        ),
        (
            "sha256sum",
            "#!/bin/sh\necho 'e6c5bbaa1d7a80d8bb4ebd43aa15f2027b308a4cd2bdac9ba303e0e65f17081a  fixture'\n",
        ),
        (
            "tar",
            "#!/bin/sh\nprintf reached > \"$ASB_PERF_BUILD_MARKER\"\nexit 1\n",
        ),
        (
            "make",
            "#!/bin/sh\nprintf reached > \"$ASB_PERF_BUILD_MARKER\"\nexit 1\n",
        ),
    ] {
        project.write(&format!("fake-bin/{name}"), script);
        std::fs::set_permissions(
            project.0.join("fake-bin").join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let path = format!(
        "{}:{}",
        project.0.join("fake-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let marker = project.0.join("source-build-called");
    let marker_text = marker.to_str().unwrap();
    let result = project.command_with_env(
        &["tool", "install", "perf"],
        &[("PATH", &path), ("ASB_PERF_BUILD_MARKER", marker_text)],
    );
    assert!(!result.status.success());
    let error = stderr(&result);
    assert!(error.contains("checksum mismatch"), "{error}");
    assert!(
        error.contains("f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d"),
        "{error}"
    );
    assert!(
        !marker.exists(),
        "unverified source reached extraction/build"
    );
    let installed = project.0.join(".asb-tools/perf/bin/perf");
    assert!(!installed.exists(), "unverified executable was installed");
}

#[cfg(unix)]
#[test]
fn flamegraph_only_configuration_installs_and_starts_perf_for_concurrent_replay() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(2);
    let calls = project.0.join("perf-calls");
    project.write("host-bin/perf", &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\nrecord) while [ $# -gt 0 ]; do if [ \"$1\" = -o ]; then shift; [ \"$1\" = - ] || printf sample > \"$1\"; break; fi; shift; done ;;\nscript) printf 'sample stack\\n' ;;\nesac\nexit 0\n", calls.display()));
    project.write(
        ".asb-tools/flamegraph/bin/stackcollapse-perf.pl",
        "#!/bin/sh\ncat\n",
    );
    project.write(
        ".asb-tools/flamegraph/bin/flamegraph",
        "#!/bin/sh\nprintf '<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>\\n'\n",
    );
    for name in [
        "host-bin/perf",
        ".asb-tools/flamegraph/bin/stackcollapse-perf.pl",
        ".asb-tools/flamegraph/bin/flamegraph",
    ] {
        std::fs::set_permissions(project.0.join(name), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    fake_perf_distribution(&project, "host-bin");
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['flamegraph']\n";
    project.write("benchmark.toml", &config);
    let path = format!(
        "{}:{}",
        project.0.join("host-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
            "--json",
        ],
        &[("PATH", &path)],
    );
    expect_success(&result);
    for line in stdout(&result).lines() {
        let _: serde_json::Value = serde_json::from_str(line).expect(
            "each concurrent JSON result remains machine readable during automatic installation",
        );
    }
    assert!(project.0.join(".asb-tools/perf/bin/perf").is_file());
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(
        calls
            .lines()
            .any(|line| line.starts_with("record ") && line.contains(" -p ")),
        "{calls}"
    );
    for index in 1..=2 {
        let dir = project
            .0
            .join("results/benchmark-runs/run-001")
            .join(format!("measurements-concurrency-{index}"));
        assert!(
            dir.join("flamegraph.svg").is_file(),
            "no flamegraph in {}",
            dir.display()
        );
        let status = std::fs::read_to_string(dir.join("flamegraph.json")).unwrap();
        assert!(status.contains("completed"), "{status}");
    }
    assert!(!stderr(&result).contains("select 'perf'"));
}

#[cfg(unix)]
#[test]
fn concurrent_flamegraph_tool_permission_is_checked_before_agent() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(4);
    for (path, mode) in [
        (".asb-tools/perf/bin/perf", 0o755),
        (".asb-tools/flamegraph/bin/stackcollapse-perf.pl", 0o644),
        (".asb-tools/flamegraph/bin/flamegraph", 0o755),
    ] {
        project.write(path, "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(project.0.join(path), std::fs::Permissions::from_mode(mode))
            .unwrap();
    }
    let config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap()
        + "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['perf','flamegraph']\n";
    project.write("benchmark.toml", &config);
    let result = replay(&project, "success", &[]);
    let error = stderr(&result);
    expect_failure(&result, "4 concurrent benchmark replica(s) failed");
    assert_eq!(
        error.matches("cannot execute flamegraph tool").count(),
        1,
        "{error}"
    );
    assert_eq!(
        error.matches("asb tool install flamegraph").count(),
        1,
        "{error}"
    );
    assert!(!project.0.join("run-concurrency-1.log").exists());
}

#[cfg(unix)]
#[test]
fn concurrent_custom_bpftrace_program_access_is_checked_before_agent() {
    let project = replay_project(4);
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
            "--measurements",
            "measurements",
        ],
        &[
            ("ASB_MEASURE_BPFTRACE", "1"),
            ("ASB_BPFTRACE_PROGRAMS", "missing.bt"),
        ],
    );
    let error = stderr(&result);
    expect_failure(&result, "4 concurrent benchmark replica(s) failed");
    assert_eq!(
        error
            .matches("cannot read bpftrace program missing.bt")
            .count(),
        1,
        "{error}"
    );
    assert!(error.contains("ASB_BPFTRACE_PROGRAMS"));
    assert!(!project.0.join("run-concurrency-1.log").exists());
}

#[cfg(unix)]
#[test]
fn concurrent_tape_directory_permission_failure_uses_stable_parent() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(4);
    let cassette = std::fs::read_to_string(project.0.join("tape.jsonl")).unwrap();
    project.write("blocked/tape.jsonl", &cassette);
    let directory = project.0.join("blocked");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::create_dir(directory.join("permission-check")).is_err() {
        let result = project.command(&[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "blocked/tape.jsonl",
        ]);
        let error = stderr(&result);
        expect_failure(&result, "4 concurrent benchmark replica(s) failed");
        assert_eq!(
            error
                .matches("cannot create a tape session directory in")
                .count(),
            1,
            "{error}"
        );
        assert_eq!(error.matches("chmod u+rwx").count(), 1, "{error}");
        assert!(!project.0.join("run-concurrency-1.log").exists());
    } else {
        std::fs::remove_dir(directory.join("permission-check")).unwrap();
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn concurrent_provider_warning_is_deduplicated_and_json_stays_clean() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = [0; 4096];
                    let _ = stream.read(&mut bytes);
                    let body = r#"{"data":[{"id":"fixture/model"}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture server: {error}"),
            }
        }
    });
    let project = Project::new();
    project.init();
    let config = "agent='codex'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture/model'\nconcurrency=3\n";
    project.write("first.toml", config);
    project.write("second.toml", config);
    project.write("suite.toml", "configs=['first.toml','second.toml']\n");
    project.write(".asb-tools/codex/bin/codex", "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(
        project.0.join(".asb-tools/codex/bin/codex"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let env = [
        ("OPENROUTER_BASE_URL", base.as_str()),
        ("OPENROUTER_API_KEY", "fixture-key"),
    ];
    let run = project.command_with_env(&["run", "first.toml"], &env);
    let sweep = project.command_with_env(&["sweep", "suite.toml"], &env);
    let json = project.command_with_env(&["run", "first.toml", "--json"], &env);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    for output in [&run, &sweep] {
        let text = stdout(output);
        assert_eq!(
            text.matches("does not report whether it supports tools")
                .count(),
            1,
            "{text}"
        );
    }
    assert_eq!(stdout(&run).matches("success_rate=").count(), 3);
    assert_eq!(stdout(&sweep).matches("success_rate=").count(), 6);
    assert!(!stdout(&json).contains("Warning:"));
    assert_eq!(stdout(&json).lines().count(), 3);
    for line in stdout(&json).lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("JSON output contains only JSON records");
    }
}

#[cfg(unix)]
#[test]
fn partial_replay_sweep_preserves_both_results_and_writes_report() {
    let project = replay_project(1);
    let mut config = std::fs::read_to_string(project.0.join("benchmark.toml")).unwrap();
    config.push_str("[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n");
    project.write("good.toml", &config);
    project.write("bad.toml", &config);
    project.write("suite.toml", "configs=['good.toml','bad.toml']\n");
    for id in ["000-good", "001-bad"] {
        project.write(
            &format!("suite-results/{id}/workspace/src/parser.py"),
            "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n",
        );
    }
    let tape = std::fs::read_to_string(project.0.join("tape.jsonl")).unwrap();
    for id in ["000-good", "001-bad"] {
        project.write(
            &format!("tapes/{id}/cassettes/benchmark-pi-bug-fix-test-model.jsonl"),
            &tape,
        );
    }
    let result = project.command_with_env(
        &[
            "sweep",
            "suite.toml",
            "--replay",
            "--cassette-root",
            "tapes",
        ],
        &[("ASB_FIXTURE_BEHAVIOR", "fail-sweep-second")],
    );
    expect_failure(&result, "1 failed benchmark(s)");
    let text = stdout(&result);
    let first = text
        .find("Starting benchmark (1/2; 50% reached): good.toml")
        .unwrap();
    let second = text
        .find("Starting benchmark (2/2; 100% reached): bad.toml")
        .unwrap();
    assert!(first < second, "{text}");
    assert_eq!(text.matches("Starting benchmark (").count(), 2, "{text}");
    for (id, status) in [("000-good", "completed"), ("001-bad", "failed")] {
        let log = project.0.join(format!("suite-results/{id}/run.log"));
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(log).unwrap()).unwrap();
        assert_eq!(value["status"], status);
    }
    assert!(
        project
            .0
            .join("results/suite-comparison/comparison.json")
            .is_file()
    );
    assert!(
        project
            .0
            .join("results/suite-comparison/report.html")
            .is_file()
    );
    let report =
        std::fs::read_to_string(project.0.join("results/suite-comparison/report.html")).unwrap();
    assert!(
        report.contains("agent exited unsuccessfully (exit status: 7)"),
        "{report}"
    );
    let json = project.command_with_env(
        &[
            "sweep",
            "suite.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--json",
        ],
        &[("ASB_FIXTURE_BEHAVIOR", "fail-sweep-second")],
    );
    expect_failure(&json, "1 failed benchmark(s)");
    let text = stdout(&json);
    assert!(!text.contains("Starting benchmark"), "{text}");
    assert!(!text.is_empty());
    for line in text.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("JSON stream contains only JSON records");
    }
}

#[cfg(unix)]
#[test]
fn replay_must_not_expose_a_provider_credential_to_the_agent() {
    use std::os::unix::fs::PermissionsExt;
    let project = replay_project(1);
    project.write("benchmark.toml", "agent='pi'\nworkload='bug-fix'\nprovider='openai'\nmodel='test-model'\nworkspace='workspace'\n");
    let agent = ".asb-tools/pi/bin/pi";
    project.write(agent, r##"#!/usr/bin/env node
const fs=require('fs');
const keys = ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'CODEBUDDY_API_KEY', 'CODEBUDDY_AUTH_TOKEN', 'LLM_API_KEY', 'CLINE_API_KEY', 'CONTINUE_API_KEY', 'OPENJIUWEN_API_KEY'];
fs.writeFileSync('credential-observation', process.env.OPENAI_API_KEY === 'asb-replay' && keys.every(key => process.env[key] === undefined) ? 'not-exposed' : 'exposed');
const body={model:'test-model',input:'offline',tools:[{type:'function'}]};
if (!/^http:\/\/127\.0\.0\.1:\d+\/v1$/.test(process.env.ASB_PROVIDER_BASE_URL || '')) process.exit(12);
fetch(process.env.ASB_PROVIDER_BASE_URL + '/responses', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
  .then(r => { if (!r.ok) process.exitCode=1; else fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n"); })
  .catch(() => { process.exitCode=1; });
"##);
    std::fs::set_permissions(
        project.0.join(agent),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let result = project.command_with_env(
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
        &[
            ("OPENAI_API_KEY", "fixture-secret"),
            ("ANTHROPIC_API_KEY", "another-fixture-secret"),
            ("ANTHROPIC_AUTH_TOKEN", "fixture-auth"),
            ("CODEBUDDY_API_KEY", "fixture-auth"),
            ("CODEBUDDY_AUTH_TOKEN", "fixture-auth"),
            ("LLM_API_KEY", "fixture-auth"),
            ("CLINE_API_KEY", "fixture-auth"),
            ("CONTINUE_API_KEY", "fixture-auth"),
            ("OPENJIUWEN_API_KEY", "fixture-auth"),
        ],
    );
    expect_success(&result);
    assert_eq!(
        std::fs::read_to_string(
            project
                .0
                .join("results/benchmark-runs/run-001/workspace/credential-observation"),
        )
        .unwrap(),
        "not-exposed"
    );
}

#[cfg(unix)]
#[test]
fn reset_must_not_delete_an_external_directory_through_a_symlink() {
    use std::os::unix::fs::symlink;
    let project = Project::new();
    let project_root = project.0.join("project");
    let outside = project.0.join("outside");
    std::fs::create_dir_all(outside.join("victim")).unwrap();
    std::fs::write(outside.join("victim/sentinel"), "must survive").unwrap();
    expect_success(&project.command(&["init", project_root.to_str().unwrap()]));
    symlink(&outside, project_root.join("link")).unwrap();
    std::fs::write(
        project_root.join(".asb/project.toml"),
        "tools_dir='link/victim'\nworkloads_dir='workloads'\nworkspace_dir='asb-workspace'\n",
    )
    .unwrap();
    let result = project.command(&["init", project_root.to_str().unwrap(), "--reset"]);
    expect_failure(&result, "reset requires a direct child");
    assert!(project_root.join(".asb/project.toml").is_file());
    assert!(
        outside.join("victim/sentinel").is_file(),
        "reset deleted data outside the project"
    );
}

#[cfg(unix)]
#[test]
fn reset_rejects_symlinked_default_directories_before_changing_state() {
    use std::os::unix::fs::symlink;
    let project = Project::new();
    let root = project.0.join("project");
    let outside = project.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    expect_success(&project.command(&["init", root.to_str().unwrap()]));
    // Only remove this newly created empty fixture directory.
    std::fs::remove_dir(root.join(".asb-tools")).unwrap();
    symlink(&outside, root.join(".asb-tools")).unwrap();
    let result = project.command(&["init", root.to_str().unwrap(), "--reset"]);
    expect_failure(&result, "is a symbolic link");
    assert!(root.join(".asb/project.toml").is_file());
    assert!(outside.is_dir());
}

#[cfg(target_os = "linux")]
fn install_fake_bridge(project: &Project, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    let name = ".asb-tools/cli2key/node_modules/.bin/codex-bridge";
    let body = source.strip_prefix("#!/usr/bin/env node\n").unwrap();
    // Only these inert fixture scripts retain stderr. Production intentionally
    // does not print bridge output, which may contain provider credentials.
    let diagnostic = serde_json::to_string(&project.0.join("bridge.stderr")).unwrap();
    let prelude = format!(
        r#"#!/usr/bin/env node
const __asbFixtureFs = require('fs');
const __asbFixtureErrorFd = __asbFixtureFs.openSync({diagnostic}, 'w', 0o600);
let __asbFixtureErrorRemaining = 8192;
function __asbFixtureRetain(chunk) {{
  if (__asbFixtureErrorRemaining <= 0) return;
  const bytes = (Buffer.isBuffer(chunk) ? chunk.subarray(0, __asbFixtureErrorRemaining)
    : Buffer.from(String(chunk).slice(0, __asbFixtureErrorRemaining)))
    .subarray(0, __asbFixtureErrorRemaining);
  __asbFixtureFs.writeSync(__asbFixtureErrorFd, bytes);
  __asbFixtureErrorRemaining -= bytes.length;
}}
const __asbFixtureStderrWrite = process.stderr.write.bind(process.stderr);
process.stderr.write = (...args) => {{ __asbFixtureRetain(args[0]); return __asbFixtureStderrWrite(...args); }};
process.on('uncaughtExceptionMonitor', error => {{
  __asbFixtureRetain('uncaught ' + String(error.code).slice(0,128) + ': ' + String(error.message).slice(0,2048) + '\n');
}});
"#
    );
    project.write(name, &format!("{prelude}{body}"));
    std::fs::set_permissions(project.0.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
fn fixture_stderr(project: &Project) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(project.0.join("bridge.stderr")) {
        file.take(8192).read_to_end(&mut bytes).unwrap();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
fn assert_fixture_listener_closed(port: &str) {
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let error =
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(100))
            .expect_err("fixture teardown left a listening endpoint");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[cfg(target_os = "linux")]
#[test]
fn cli2key_mock_keeps_selected_listener_reserved_through_startup() {
    let project = Project::new();
    project.init();
    install_fake_bridge(
        &project,
        r##"#!/usr/bin/env node
const http = require('http'), fs = require('fs');
fs.writeFileSync(process.env.ASB_TEST_PIDFILE, String(process.pid));
http.createServer((req,res) => {
  res.writeHead(200, {'content-type':'application/json'});
  res.end(JSON.stringify({data:[{id:'fixture-model'}]}));
}).listen(process.env.ASB_TEST_LISTENER_FD ? {fd:Number(process.env.ASB_TEST_LISTENER_FD)}
  : {port:Number(process.env.CODEX_BRIDGE_PORT),host:'127.0.0.1'}, () => console.log('Codex Bridge API key: fixture-key'));
"##,
    );
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port().to_string();
    let pidfile = project.0.join("bridge.pid");
    let result = project.command_with_env(
        &["provider", "models", "cli2key"],
        &[
            ("CODEX_BRIDGE_PORT", &port),
            ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
        ],
    );
    assert_eq!(reserved.local_addr().unwrap().port().to_string(), port);
    let pid: u32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
    let _wait = wait_for_fixture(&project, pid);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    // stdout EOF can be observed just before Node's exit status is collectable.
    expect_failure(&result, "closed its startup output");
    assert!(fixture_stderr(&project).contains("EADDRINUSE"));
    let result = project.command_with_reserved_bridge(
        &["provider", "models", "cli2key"],
        &[
            ("CODEX_BRIDGE_PORT", &port),
            ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
        ],
        reserved,
    );
    let pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    let _wait = wait_for_fixture(&project, pid);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    assert_fixture_listener_closed(&port);
    assert!(
        result.status.success(),
        "stderr={} fixture_stderr={}",
        stderr(&result),
        fixture_stderr(&project)
    );
    assert!(stdout(&result).contains("fixture-model"));
}

#[cfg(target_os = "linux")]
#[test]
fn cli2key_fixture_stderr_is_bounded_without_swallowing_failure() {
    let project = Project::new();
    project.init();
    install_fake_bridge(
        &project,
        r##"#!/usr/bin/env node
require('fs').writeFileSync(process.env.ASB_TEST_PIDFILE, String(process.pid));
process.stderr.write('x'.repeat(16384));
throw Object.assign(new Error('fixture failure'), {code:'FIXTURE_FAILURE'});
"##,
    );
    let pidfile = project.0.join("bridge.pid");
    let result = project.command_with_env(
        &["provider", "models", "cli2key"],
        &[("ASB_TEST_PIDFILE", pidfile.to_str().unwrap())],
    );
    let pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    let _wait = wait_for_fixture(&project, pid);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    expect_failure(&result, "closed its startup output");
    assert_eq!(
        std::fs::metadata(project.0.join("bridge.stderr"))
            .unwrap()
            .len(),
        8192
    );
    assert_eq!(fixture_stderr(&project), "x".repeat(8192));
}

#[cfg(target_os = "linux")]
struct WaitForFixtureExpiry {
    pid: u32,
    executable: PathBuf,
}

#[cfg(target_os = "linux")]
impl Drop for WaitForFixtureExpiry {
    fn drop(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let cmdline = std::fs::read(format!("/proc/{}/cmdline", self.pid)).unwrap_or_default();
            if !cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == self.executable.as_os_str().as_bytes())
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_for_fixture(project: &Project, pid: u32) -> WaitForFixtureExpiry {
    WaitForFixtureExpiry {
        pid,
        executable: project
            .0
            .join(".asb-tools/cli2key/node_modules/.bin/codex-bridge"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn provider_models_fetches_all_authenticated_catalog_pages() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    for (provider, env_key, env_url, first, second, expected_query, expected_header) in [
        (
            "anthropic",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"a/b"}"#,
            r#"{"data":[{"id":"second"}],"has_more":false}"#,
            "after_id=a%2Fb",
            "x-api-key: fixture-key",
        ),
        (
            "gemini",
            "GEMINI_API_KEY",
            "GOOGLE_GEMINI_BASE_URL",
            r#"{"models":[{"name":"models/first"}],"nextPageToken":"a/b"}"#,
            r#"{"models":[{"name":"models/second"}]}"#,
            "pageToken=a%2Fb",
            "x-goog-api-key: fixture-key",
        ),
        (
            "fireworks",
            "FIREWORKS_API_KEY",
            "FIREWORKS_MODELS_URL",
            r#"{"models":[{"name":"first","supportsTools":true}],"nextPageToken":"a/b"}"#,
            r#"{"models":[{"name":"second","supportsTools":true}]}"#,
            "pageToken=a%2Fb",
            "authorization: bearer fixture-key",
        ),
    ] {
        let project = Project::new();
        project.init();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            while requests.len() < 2 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buf = [0; 8192];
                let len = stream.read(&mut buf).unwrap();
                requests.push(String::from_utf8_lossy(&buf[..len]).to_string());
                let body = if requests.len() == 1 { first } else { second };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let result = project.command_with_env(
            &["provider", "models", provider, "--json"],
            &[(env_key, "fixture-key"), (env_url, &base)],
        );
        let requests = server.join().unwrap();
        expect_success(&result);
        assert_eq!(requests.len(), 2);
        assert!(requests[1].lines().next().unwrap().contains(expected_query));
        assert!(
            requests
                .iter()
                .all(|request| request.to_ascii_lowercase().contains(expected_header))
        );
        let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(data["models"], serde_json::json!(["first", "second"]));
    }
}

fn serve_catalog(
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("catalog fixture did not receive a connection: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = [0; 8192];
        let _ = stream.read(&mut request);
        let _ = write!(
            stream,
            "HTTP/1.1 {status} fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(&body);
    });
    (base, thread)
}

#[test]
fn catalog_diagnostics_distinguish_http_html_and_truncated_json_without_secrets() {
    for (status, content_type, body, expected) in [
        (
            401,
            "application/json",
            "{\"error\":\"secret-response\"}",
            "HTTP 401",
        ),
        (
            429,
            "application/json",
            "{\"error\":\"secret-response\"}",
            "HTTP 429",
        ),
        (
            200,
            "text/html",
            "<html>secret-response</html>",
            "invalid JSON",
        ),
        (
            200,
            "application/json",
            "{\"data\":[",
            "incomplete or truncated JSON",
        ),
    ] {
        let project = Project::new();
        project.init();
        let (base, server) = serve_catalog(status, content_type, body.as_bytes().to_vec());
        let output = project.command_with_env(
            &["provider", "models", "openrouter"],
            &[
                ("OPENROUTER_BASE_URL", &base),
                ("OPENROUTER_API_KEY", "fixture-secret"),
            ],
        );
        server.join().unwrap();
        assert!(!output.status.success());
        let error = stderr(&output);
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("openrouter"), "{error}");
        assert!(
            !error.contains("fixture-secret") && !error.contains("secret-response"),
            "{error}"
        );
        if content_type == "text/html" {
            assert!(
                error.contains("HTTP 200")
                    && error.contains("text/html")
                    && error.contains("proxy"),
                "{error}"
            );
        }
    }
}

#[test]
fn catalog_above_one_mib_is_fully_enumerated() {
    let project = Project::new();
    project.init();
    let padding = "a".repeat(1_100_000);
    let body = format!(
        "{{\"data\":[{{\"id\":\"first\",\"description\":\"{padding}\"}},{{\"id\":\"second\"}}]}}"
    );
    let (base, server) = serve_catalog(200, "application/json", body.into_bytes());
    let output = project.command_with_env(
        &["provider", "models", "openrouter", "--json"],
        &[
            ("OPENROUTER_BASE_URL", &base),
            ("OPENROUTER_API_KEY", "fixture-key"),
        ],
    );
    server.join().unwrap();
    expect_success(&output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["models"], serde_json::json!(["first", "second"]));
}

#[test]
fn oversized_catalog_reports_limit_instead_of_invalid_json() {
    let project = Project::new();
    project.init();
    let (base, server) = serve_catalog(200, "application/json", vec![b'x'; 16 * 1024 * 1024 + 1]);
    let output = project.command_with_env(
        &["provider", "models", "openrouter"],
        &[
            ("OPENROUTER_BASE_URL", &base),
            ("OPENROUTER_API_KEY", "fixture-key"),
        ],
    );
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("exceeds the 16 MiB"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn chunked_catalog_is_bounded_even_without_content_length() {
    use std::io::{Read, Write};
    let project = Project::new();
    project.init();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => {
                    panic!("chunked catalog fixture did not receive a connection: {error}")
                }
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..257 {
            if write!(stream, "{:X}\r\n", chunk.len())
                .and_then(|_| stream.write_all(&chunk))
                .and_then(|_| stream.write_all(b"\r\n"))
                .is_err()
            {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let output = project.command_with_env(
        &["provider", "models", "openrouter"],
        &[
            ("OPENROUTER_BASE_URL", &base),
            ("OPENROUTER_API_KEY", "fixture-key"),
        ],
    );
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("exceeds the 16 MiB"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn run_reports_catalog_http_failure_before_agent_starts() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    project.write("benchmark.toml", "agent='codex'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture/model'\nworkspace='workspace'\nlog_file='run.log'\n");
    project.write(
        ".asb-tools/codex/bin/codex",
        "#!/bin/sh\ntouch agent-started\n",
    );
    std::fs::set_permissions(
        project.0.join(".asb-tools/codex/bin/codex"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (base, server) = serve_catalog(502, "text/html", b"secret-proxy-response".to_vec());
    let output = project.command_with_env(
        &["run", "benchmark.toml"],
        &[
            ("OPENROUTER_BASE_URL", &base),
            ("OPENROUTER_API_KEY", "fixture-secret"),
        ],
    );
    server.join().unwrap();
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("HTTP 502") && error.contains("openrouter"),
        "{error}"
    );
    assert!(!error.contains("fixture-secret") && !error.contains("secret-proxy-response"));
    assert!(!project.0.join("agent-started").exists());
}

#[test]
fn catalog_connection_failure_reports_curl_code_without_proxy_credentials() {
    let project = Project::new();
    project.init();
    let output = project.command_with_env(
        &["provider", "models", "openrouter"],
        &[
            ("OPENROUTER_BASE_URL", "https://provider.invalid/v1"),
            ("OPENROUTER_API_KEY", "fixture-secret"),
            ("https_proxy", "http://proxy-user:proxy-secret@127.0.0.1:1"),
        ],
    );
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("curl exit 7") && error.contains("proxy"),
        "{error}"
    );
    assert!(
        !error.contains("fixture-secret")
            && !error.contains("proxy-secret")
            && !error.contains("proxy-user"),
        "{error}"
    );
}

#[test]
fn together_provider_models_accepts_documented_array_response() {
    use std::io::{Read, Write};
    let project = Project::new();
    project.init();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 8192];
        let len = stream.read(&mut request).unwrap();
        let body = r#"[{"id":"example/chat","type":"chat"}]"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        String::from_utf8_lossy(&request[..len]).into_owned()
    });
    let result = project.command_with_env(
        &["provider", "models", "together", "--json"],
        &[
            ("TOGETHER_API_KEY", "fixture-key"),
            ("TOGETHER_BASE_URL", &base),
        ],
    );
    let request = server.join().unwrap();
    expect_success(&result);
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(data["models"], serde_json::json!(["example/chat"]));
}

#[cfg(target_os = "linux")]
#[test]
fn cli2key_run_validates_current_bridge_catalog_and_reaps_bridge() {
    use std::os::unix::fs::PermissionsExt;
    for (model, expect_ok) in [("current-model", true), ("removed-model", false)] {
        let project = Project::new();
        project.init();
        project.write("benchmark.toml", &format!("agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='{model}'\nworkspace='workspace'\nlog_file='run.log'\n"));
        project.write(
            ".asb-tools/pi/bin/pi",
            "#!/bin/sh\nprintf 'fixture agent complete\\n'\n",
        );
        std::fs::set_permissions(
            project.0.join(".asb-tools/pi/bin/pi"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        install_fake_bridge(
            &project,
            r##"#!/usr/bin/env node
const http = require('http'), fs = require('fs');
fs.writeFileSync(process.env.ASB_TEST_PIDFILE, String(process.pid));
http.createServer((req,res) => {
  fs.writeFileSync(process.env.ASB_TEST_OBSERVED, req.url + ' ' + req.headers.authorization);
  res.writeHead(200, {'content-type':'application/json'});
  res.end(JSON.stringify({data:[{id:'current-model'}]}));
}).listen({fd:Number(process.env.ASB_TEST_LISTENER_FD)}, () => console.log('Codex Bridge API key: fixture-key'));
"##,
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let pidfile = project.0.join("bridge.pid");
        let observed = project.0.join("observed");
        let result = project.command_with_reserved_bridge(
            &["run", "benchmark.toml"],
            &[
                ("CODEX_BRIDGE_PORT", &port),
                ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
                ("ASB_TEST_OBSERVED", observed.to_str().unwrap()),
            ],
            listener,
        );
        let pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
        let _wait = wait_for_fixture(&project, pid);
        if expect_ok {
            assert!(
                result.status.success(),
                "stderr={} fixture_stderr={}",
                stderr(&result),
                fixture_stderr(&project)
            );
        } else {
            expect_failure(&result, "was not returned by provider");
        }
        assert_eq!(
            std::fs::read_to_string(observed).unwrap(),
            "/v1/models Bearer fixture-key"
        );
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "bridge was not reaped"
        );
        assert_fixture_listener_closed(&port);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cli2key_success_stops_its_mock_bridge_after_model_discovery() {
    let project = Project::new();
    project.init();
    install_fake_bridge(
        &project,
        r##"#!/usr/bin/env node
const http = require('http'), fs = require('fs');
fs.writeFileSync(process.env.ASB_TEST_PIDFILE, String(process.pid));
// Safety net, not the cleanup mechanism. Outlive the compatibility proxy's
// five-second readiness budget under parallel test load; otherwise this fixture
// can exit before a healthy ASB reaches it. The assertions below still require
// ASB to reap the bridge before this timer fires.
process.stderr.write('fixture diagnostic\n');
setTimeout(() => { fs.writeFileSync(process.env.ASB_TEST_EXPIRED, 'expired'); process.exit(0); }, 10000).unref();
http.createServer((req,res) => {
  if (req.url !== '/v1/models') { res.writeHead(404); return res.end(); }
  res.writeHead(200, {'content-type':'application/json'});
  res.end(JSON.stringify({data:[{id:'fixture-model'}]}));
}).listen({fd:Number(process.env.ASB_TEST_LISTENER_FD)}, () => {
  console.log('Codex Bridge API key: fixture-key');
});
"##,
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let pidfile = project.0.join("bridge.pid");
    let expired = project.0.join("expired");
    let result = project.command_with_reserved_bridge(
        &["provider", "models", "cli2key"],
        &[
            ("CODEX_BRIDGE_PORT", &port),
            ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
            ("ASB_TEST_EXPIRED", expired.to_str().unwrap()),
        ],
        listener,
    );
    let pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    let _wait = wait_for_fixture(&project, pid);
    assert!(
        result.status.success(),
        "stdout={} stderr={} fixture_stderr={}",
        stdout(&result),
        stderr(&result),
        fixture_stderr(&project)
    );
    assert!(!expired.exists(), "mock timed out before ASB completed");
    assert_eq!(fixture_stderr(&project), "fixture diagnostic\n");
    assert!(stdout(&result).contains("fixture-model"));
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "bridge was not reaped"
    );
    assert_fixture_listener_closed(&port);
}

/// Repaired contract: the old numeric proposal is irrelevant to a pinned FD.
#[cfg(target_os = "linux")]
#[test]
fn cli2key_compatibility_hop_owns_its_listener_without_a_rebind_window() {
    use std::os::unix::fs::PermissionsExt;
    let real_node = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("node"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    for contend in [false, true] {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cli2key-startup-diagnostic")
            .join(format!("{}-{}", std::process::id(), contend));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Retain exact owned evidence, including on assertion failure.
        let project = std::mem::ManuallyDrop::new(Project(
            root.canonicalize().unwrap(),
            PythonPreparation::Ordinary,
        ));
        project.init();
        install_fake_bridge(
            &project,
            r##"#!/usr/bin/env node
const http=require('http'),fs=require('fs');
fs.writeFileSync(process.env.ASB_TEST_PIDFILE,String(process.pid));
setTimeout(()=>{fs.writeFileSync(process.env.ASB_TEST_EXPIRED,'expired');process.exit(0)},10000).unref();
http.createServer((req,res)=>{res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({data:[{id:'fixture-model'}]}))})
.listen({fd:Number(process.env.ASB_TEST_LISTENER_FD)},()=>console.log('Codex Bridge API key: fixture-key'));
"##,
        );
        let wrapper = format!(
            r#"#!/usr/bin/python3
import json,os,socket,sys
real_node={real_node}
if os.path.basename(sys.argv[1]) == 'cli2key-proxy.mjs':
    root=os.environ['HOME']
    fd=os.open(root+'/compatibility.stderr',os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
    os.dup2(fd,2)
    os.close(fd)
    inherited=int(sys.argv[sys.argv.index('--listen-fd')+1])
    listener=socket.fromfd(inherited,socket.AF_INET,socket.SOCK_STREAM)
    port=listener.getsockname()[1]
    listener.close()
    with open(root+'/compatibility.pid','x') as output: output.write(str(os.getpid()))
    if os.environ['ASB_DIAGNOSTIC_CONTEND']=='yes':
        owned=socket.socket()
        # Unlike the historical diagnostic, there IS NO released --port
        # proposal to claim. A separate occupied port must not disrupt startup.
        owned.bind(('127.0.0.1',0))
        owned.listen(1)
        owned.set_inheritable(True)
        # Only this admitted child holds the competing socket; exec retains
        # its identity. Node exit closes it. No helper or outside process kill.
        with open(root+'/claimed-port','x') as output: output.write(str(owned.getsockname()[1]))
        contender=socket.socket()
        try:
            contender.bind(('127.0.0.1',port))
            raise RuntimeError('ASB released its listener before Node adoption')
        except OSError as error:
            if error.errno!=98: raise
        finally: contender.close()
os.execv(real_node,[real_node]+sys.argv[1:])
"#,
            real_node = serde_json::to_string(&real_node).unwrap()
        );
        project.write("diagnostic-bin/node", &wrapper);
        std::fs::set_permissions(
            project.0.join("diagnostic-bin/node"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let path = std::env::join_paths(
            std::iter::once(project.0.join("diagnostic-bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let pidfile = project.0.join("bridge.pid");
        let expired = project.0.join("expired");
        let started = std::time::Instant::now();
        let result = project.command_with_reserved_bridge(
            &["provider", "models", "cli2key"],
            &[
                ("CODEX_BRIDGE_PORT", &port),
                ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
                ("ASB_TEST_EXPIRED", expired.to_str().unwrap()),
                ("PATH", path.to_str().unwrap()),
                ("ASB_DIAGNOSTIC_CONTEND", if contend { "yes" } else { "no" }),
            ],
            listener,
        );
        std::fs::write(project.0.join("asb.stdout"), &result.stdout).unwrap();
        std::fs::write(project.0.join("asb.stderr"), &result.stderr).unwrap();
        let bridge_pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
        let _wait = wait_for_fixture(&project, bridge_pid);
        let proxy_pid: u32 = std::fs::read_to_string(project.0.join("compatibility.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{bridge_pid}")).exists());
        assert!(!std::path::Path::new(&format!("/proc/{proxy_pid}")).exists());
        assert_fixture_listener_closed(&port);
        assert!(!expired.exists());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let diagnostic = std::fs::read_to_string(project.0.join("compatibility.stderr")).unwrap();
        if contend {
            let claimed = std::fs::read_to_string(project.0.join("claimed-port")).unwrap();
            assert_fixture_listener_closed(&claimed);
        }
        expect_success(&result);
        assert!(stdout(&result).contains("fixture-model"));
        assert!(diagnostic.is_empty(), "{diagnostic}");
        println!(
            "diagnostic contend={contend}: both owned children gone, listeners closed, evidence={}",
            project.0.display()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cli2key_failed_startup_must_stop_its_mock_bridge() {
    let project = Project::new();
    project.init();
    install_fake_bridge(
        &project,
        "#!/usr/bin/env node\nconst fs=require('fs'); fs.writeFileSync(process.env.ASB_TEST_PIDFILE, String(process.pid)); fs.closeSync(1); setTimeout(() => fs.writeFileSync(process.env.ASB_TEST_EXPIRED, 'expired'), 2000);\n",
    );
    let pidfile = project.0.join("bridge.pid");
    let expired = project.0.join("expired");
    let result = project.command_with_env(
        &["provider", "models", "cli2key"],
        &[
            ("ASB_TEST_PIDFILE", pidfile.to_str().unwrap()),
            ("ASB_TEST_EXPIRED", expired.to_str().unwrap()),
        ],
    );
    let pid: u32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    let _wait = wait_for_fixture(&project, pid);
    expect_failure(&result, "closed its startup output");
    assert!(!expired.exists(), "mock timed out before ASB completed");
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "bridge survived failed startup"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn concurrent_cli2key_replicas_use_distinct_default_bridges_and_reap_both() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.init();
    project.write("benchmark.toml", "agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='fixture-model'\nconcurrency=2\nworkspace='workspace'\nlog_file='run.log'\n");
    project.write(
        ".asb-tools/pi/bin/pi",
        r##"#!/usr/bin/env node
fetch(process.env.OPENAI_BASE_URL+'/probe', {
  headers:{authorization:'Bearer '+process.env.OPENAI_API_KEY},
  signal:AbortSignal.timeout(5000)
}).then(async response => {
  if (!response.ok) process.exitCode=1;
  else console.log('fixture complete');
}).catch(() => { process.exitCode=1; });
"##,
    );
    std::fs::set_permissions(
        project.0.join(".asb-tools/pi/bin/pi"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    install_fake_bridge(
        &project,
        r##"#!/usr/bin/env node
const http = require('http'), fs = require('fs');
setTimeout(() => process.exit(3), 10000).unref();
http.createServer((req,res) => {
  if (req.url === '/v1/probe') {
    if (req.headers.authorization !== 'Bearer fixture-key') {res.writeHead(401); return res.end();}
    fs.appendFileSync(process.env.ASB_TEST_CALLS, JSON.stringify({pid:process.pid})+'\n');
    const deadline=Date.now()+3000;
    const timer=setInterval(() => {
      if (fs.readFileSync(process.env.ASB_TEST_CALLS,'utf8').trim().split('\n').length >= 2) {
        clearInterval(timer); res.writeHead(200); res.end('ok');
      } else if (Date.now()>deadline) {clearInterval(timer); res.writeHead(500); res.end();}
    },10);
    return;
  }
  res.writeHead(200, {'content-type':'application/json'});
  res.end(JSON.stringify({data:[{id:'fixture-model'}]}));
}).listen(Number(process.env.CODEX_BRIDGE_PORT), '127.0.0.1', function () {
  fs.appendFileSync(process.env.ASB_TEST_BRIDGES, JSON.stringify({pid:process.pid,port:this.address().port})+'\n');
  console.log('Codex Bridge API key: fixture-key');
});
"##,
    );
    let records = project.0.join("bridges.jsonl");
    let calls = project.0.join("calls.jsonl");
    let node_errors = project.0.join("node-errors");
    project.write("observe-node.cjs", "const fs=require('fs');let retained=0;const write=process.stderr.write.bind(process.stderr);process.stderr.write=(chunk,...args)=>{const bytes=Buffer.from(chunk);const keep=bytes.subarray(0,Math.max(0,16384-retained));if(keep.length){fs.appendFileSync(process.env.ASB_TEST_NODE_ERRORS,keep);retained+=keep.length;}return write(chunk,...args);};\n");
    let node_options = format!("--require {}", project.0.join("observe-node.cjs").display());
    let result = project.command_with_env(
        &["run", "benchmark.toml", "--json"],
        &[
            ("ASB_TEST_BRIDGES", records.to_str().unwrap()),
            ("ASB_TEST_CALLS", calls.to_str().unwrap()),
            ("NODE_OPTIONS", &node_options),
            ("ASB_TEST_NODE_ERRORS", node_errors.to_str().unwrap()),
        ],
    );
    let summaries = stdout(&result)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        summaries.len(),
        2,
        "{}; fixture Node stderr: {}",
        stderr(&result),
        std::fs::read_to_string(&node_errors).unwrap_or_default()
    );
    assert!(
        summaries.iter().all(|row| row["process_success"] == true),
        "{}",
        stderr(&result)
    );
    let bridges = std::fs::read_to_string(records)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bridges.len(), 2);
    assert_ne!(bridges[0]["port"], bridges[1]["port"]);
    let calls = std::fs::read_to_string(calls)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["pid"]
                .as_u64()
                .unwrap()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        calls.len(),
        2,
        "both concurrent client requests must reach different owned bridges"
    );
    for bridge in bridges {
        let pid = bridge["pid"].as_u64().unwrap() as u32;
        let _wait = wait_for_fixture(&project, pid);
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "owned bridge survived completion"
        );
    }
}

#[test]
fn init_detects_network_proxies_without_persisting_credentials() {
    let project = Project::new();
    let output = project.command_with_env(
        &["init"],
        &[
            ("http_proxy", "http://proxy.example:8080"),
            ("HTTPS_PROXY", "http://user:secret@proxy.example:8080"),
            ("SOCKS_PROXY", "socks5h://proxy.example:1080"),
            ("NO_PROXY", "internal.example"),
        ],
    );
    expect_success(&output);
    let config = std::fs::read_to_string(project.0.join(".asb/project.toml")).unwrap();
    assert!(config.contains("http://proxy.example:8080"));
    assert!(config.contains("socks5h://proxy.example:1080"));
    assert!(config.contains("internal.example"));
    assert!(!config.contains("secret"));
    assert!(!stdout(&output).contains("secret"));
    assert!(stdout(&output).contains("not saved"));
}

#[test]
fn saved_http_and_socks_proxies_route_model_discovery_without_live_network() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    for socks in [false, true] {
        let project = Project::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!(
            "{}://{}",
            if socks { "socks5h" } else { "http" },
            listener.local_addr().unwrap()
        );
        let variable = if socks { "all_proxy" } else { "http_proxy" };
        expect_success(&project.command_with_env(&["init"], &[(variable, &proxy)]));
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("proxy did not receive a connection: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            if socks {
                let mut greeting = [0; 2];
                stream.read_exact(&mut greeting).unwrap();
                assert_eq!(greeting[0], 5);
                let mut methods = vec![0; greeting[1] as usize];
                stream.read_exact(&mut methods).unwrap();
                stream.write_all(&[5, 0]).unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).unwrap();
                assert_eq!(&request[..3], &[5, 1, 0]);
                assert_eq!(
                    request[3], 3,
                    "socks5h must send the domain name to the proxy"
                );
                let mut len = [0; 1];
                stream.read_exact(&mut len).unwrap();
                let mut host_and_port = vec![0; len[0] as usize + 2];
                stream.read_exact(&mut host_and_port).unwrap();
                assert_eq!(&host_and_port[..len[0] as usize], b"provider.invalid");
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                    .unwrap();
            }
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET "));
            if !socks {
                assert!(request.contains("http://provider.invalid/v1/models"));
            }
            let body = r#"{"data":[{"id":"offline-proxy-test"}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let nested = project.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let result = project.command_in_with_env(
            &nested,
            &["provider", "models", "opencode-zen", "--json"],
            &[("OPENCODE_ZEN_BASE_URL", "http://provider.invalid/v1")],
        );
        server.join().unwrap();
        expect_success(&result);
        assert!(stdout(&result).contains("offline-proxy-test"));
    }
}

#[test]
fn configured_proxy_does_not_intercept_loopback_provider() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let project = Project::new();
    expect_success(&project.command_with_env(
        &["init"],
        &[
            ("http_proxy", "http://127.0.0.1:1"),
            ("no_proxy", "unrelated.invalid"),
        ],
    ));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("loopback provider did not receive a connection: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let body = r#"{"data":[{"id":"local-model"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let result = project.command_with_env(
        &["provider", "models", "opencode-zen", "--json"],
        &[("OPENCODE_ZEN_BASE_URL", &base)],
    );
    server.join().unwrap();
    expect_success(&result);
    assert!(stdout(&result).contains("local-model"));
}

#[test]
fn saved_https_proxy_receives_tunnel_requests() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let project = Project::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    expect_success(&project.command_with_env(&["init"], &[("https_proxy", &proxy)]));
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("HTTPS proxy was not reached: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("CONNECT provider.invalid:443 ")
        );
        stream
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });
    let output = project.command_with_env(
        &["provider", "models", "opencode-zen"],
        &[("OPENCODE_ZEN_BASE_URL", "https://provider.invalid/v1")],
    );
    server.join().unwrap();
    assert!(
        !output.status.success(),
        "the fixture intentionally rejects the HTTPS tunnel"
    );
    assert!(
        stderr(&output).contains("HTTP proxy returned 502"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn record_uses_benchmark_project_proxy_even_when_invoked_elsewhere() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    for socks in [false, true] {
        let project = Project::new();
        let outside = Project::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!(
            "{}://{}",
            if socks { "socks5h" } else { "http" },
            listener.local_addr().unwrap()
        );
        expect_success(&project.command_with_env(
            &["init"],
            &[(if socks { "all_proxy" } else { "http_proxy" }, &proxy)],
        ));
        project.write("workspace/src/parser.py", "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
        project.write("benchmark.toml", "agent='codex'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture/model'\nworkspace='workspace'\nlog_file='run.log'\n");
        let agent = ".asb-tools/codex/bin/codex";
        project.write(agent, r##"#!/usr/bin/env node
const fs = require('fs');
if (!(process.env.http_proxy || process.env.all_proxy) || !process.env.no_proxy.includes('127.0.0.1')) process.exit(16);
fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
const cfg = fs.readFileSync(process.env.CODEX_HOME + '/config.toml', 'utf8');
const endpoint = cfg.match(/base_url = "([^"]+)"/)[1];
const body = JSON.stringify({model:'fixture/model',input:'offline test',tools:[{type:'function',name:'read_file'}]});
fetch(endpoint + '/responses', {method:'POST',headers:{'content-type':'application/json','authorization':'Bearer '+process.env.ASB_AGENT_API_KEY},body}).then(r => {if (!r.ok) process.exit(14); console.log('offline-proxy-agent');}).catch(() => process.exit(15));
"##);
        std::fs::set_permissions(
            project.0.join(agent),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut requests = Vec::new();
            while requests.len() < 2 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture proxy: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                if socks {
                    let mut greeting = [0; 2];
                    stream.read_exact(&mut greeting).unwrap();
                    assert_eq!(greeting[0], 5);
                    let mut methods = vec![0; greeting[1] as usize];
                    stream.read_exact(&mut methods).unwrap();
                    stream.write_all(&[5, 0]).unwrap();
                    let mut request = [0; 4];
                    stream.read_exact(&mut request).unwrap();
                    assert_eq!(request, [5, 1, 0, 3]);
                    let mut len = [0; 1];
                    stream.read_exact(&mut len).unwrap();
                    let mut host_and_port = vec![0; len[0] as usize + 2];
                    stream.read_exact(&mut host_and_port).unwrap();
                    assert_eq!(&host_and_port[..len[0] as usize], b"provider.invalid");
                    stream
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                        .unwrap();
                }
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let count = stream.read(&mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let first = request.lines().next().unwrap_or_default();
                let request_path = if socks { "" } else { "http://provider.invalid" };
                let response = if first.starts_with(&format!("GET {request_path}/api/v1/models ")) {
                    r#"{"data":[{"id":"fixture/model","supported_parameters":["tools"]}]}"#
                } else if first.starts_with(&format!("POST {request_path}/api/v1/responses ")) {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer fixture-secret")
                    );
                    assert!(request.contains("offline test"));
                    r#"{"id":"resp_fixture","object":"response","output":[]}"#
                } else {
                    panic!("unexpected proxy request: {first}")
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                requests.push(request);
            }
            requests
        });
        let benchmark = project.0.join("benchmark.toml");
        let cassette = project.0.join("tape.jsonl");
        let output = project.command_in_with_env(
            &outside.0,
            &[
                "--project-dir",
                project.0.to_str().unwrap(),
                "run",
                benchmark.to_str().unwrap(),
                "--record",
                "--cassette",
                cassette.to_str().unwrap(),
            ],
            &[
                ("OPENROUTER_BASE_URL", "http://provider.invalid/api/v1"),
                ("OPENROUTER_API_KEY", "fixture-secret"),
            ],
        );
        let requests = server.join().unwrap();
        expect_success(&output);
        assert_eq!(
            requests.len(),
            2,
            "model discovery and recorded request must both use the proxy"
        );
        let mut archive =
            asb_cli::cassette::Archive::open(&cassette, asb_cli::cassette::Limits::default())
                .unwrap();
        assert_eq!(archive.manifest().tracks.len(), 1);
        assert!(archive.manifest().tracks[0].complete);
        let tape = String::from_utf8(archive.read_track(1).unwrap()).unwrap();
        assert!(tape.contains("resp_fixture"));
        assert!(!tape.contains("fixture-secret"));
    }
}

#[test]
fn tool_and_workload_downloads_use_the_project_proxy() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let project = Project::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    expect_success(&project.command_with_env(&["init"], &[("http_proxy", &proxy)]));
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut paths = Vec::new();
        // Each direct transfer first probes the remote size, then downloads
        // the bounded payload through the same configured proxy.
        while paths.len() < 4 && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fixture proxy: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            paths.push(String::from_utf8(request).unwrap());
            let body = "fixture-download";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        paths
    });
    let tool = project.command(&[
        "tool",
        "install",
        "fixture-tool",
        "--source",
        "http://downloads.invalid/tool",
    ]);
    let workload = project.command(&["workload", "install", "http://downloads.invalid/task.txt"]);
    let requests = server.join().unwrap();
    expect_success(&tool);
    expect_success(&workload);
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET http://downloads.invalid/tool "))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET http://downloads.invalid/task.txt "))
    );
}

#[test]
fn node_sidecar_uses_https_proxy_and_redacts_proxy_failure() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("sidecar HTTPS proxy was not reached: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("CONNECT provider.invalid:443 ")
        );
        stream
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/scripts/network-fetch.mjs");
    let output = Command::new("node")
        .args(["--input-type=module", "-e", "const {networkFetch}=await import(process.argv[1]); try {await networkFetch('https://provider.invalid/v1', {headers:{authorization:'Bearer secret-for-test'}}); process.exit(1)} catch(e) { console.log(e.message) }"])
        .arg(module)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("https_proxy", proxy)
        .output().unwrap();
    server.join().unwrap();
    expect_success(&output);
    assert!(stdout(&output).contains("outbound proxy request failed"));
    assert!(!stdout(&output).contains("secret-for-test"));
    assert!(!stderr(&output).contains("secret-for-test"));
}

#[test]
fn sidecar_cancels_active_proxy_request_and_removes_private_scratch() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let project = Project::new();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/scripts/network-fetch.mjs");
    let module = project.0.join("network-fetch.mjs");
    std::fs::copy(source, &module).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("proxy was not reached: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 4096];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => panic!("request was not cancelled: {error}"),
            }
        }
    });
    let output = Command::new("node")
        .args(["--input-type=module", "-e", "const {networkFetch,cancelNetworkRequests}=await import(process.argv[1]); const request=networkFetch('http://provider.invalid/v1', {method:'POST',body:'sensitive test body'}); setTimeout(cancelNetworkRequests,300); try {await request;process.exit(1)} catch(e) {console.log(e.message)}"])
        .arg(module)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("http_proxy", proxy)
        .output().unwrap();
    server.join().unwrap();
    expect_success(&output);
    assert!(!stdout(&output).contains("sensitive test body"));
    assert!(!std::fs::read_dir(&project.0).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".network-request-")
    }));
}

#[test]
fn sidecar_follows_same_origin_redirects_but_refuses_cross_origin_credentials() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    for cross_origin in [false, true] {
        let project = Project::new();
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/scripts/network-fetch.mjs");
        let module = project.0.join("network-fetch.mjs");
        std::fs::copy(source, &module).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut count = 0;
            while count < if cross_origin { 1 } else { 2 } && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture proxy: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fixture-secret")
                );
                if count == 0 {
                    let location = if cross_origin {
                        "http://other.invalid/leak"
                    } else {
                        "/second"
                    };
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert!(request.starts_with("GET http://provider.invalid/second "));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
                count += 1;
            }
            count
        });
        let output = Command::new("node")
            .args(["--input-type=module", "-e", "const {networkFetch}=await import(process.argv[1]); try {const r=await networkFetch('http://provider.invalid/first',{headers:{authorization:'Bearer fixture-secret'}}); let b='';for await(const c of r.body)b+=Buffer.from(c).toString();console.log('ok:'+b)}catch(e){console.log('error:'+e.message)}"])
            .arg(module).isolated_env().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("http_proxy", proxy).output().unwrap();
        let count = server.join().unwrap();
        expect_success(&output);
        if cross_origin {
            assert_eq!(count, 1);
            assert!(stdout(&output).contains("refused a cross-origin redirect"));
        } else {
            assert_eq!(count, 2);
            assert!(stdout(&output).contains("ok:ok"));
        }
    }
}
