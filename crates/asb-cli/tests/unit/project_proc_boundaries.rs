//! Deterministic proc inputs, with no production seams or host-setting changes.
use super::super::*;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Stdio;

const CASE: &str = "ASB_PROJECT_PROC_CASE";

#[test]
fn controlled_proc_inputs_cover_consent_and_cgroup_rejections_without_host_mutation() {
    if let Ok(case) = std::env::var(CASE) {
        child(&case);
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/project-proc-fixtures")
        .join(crate::tools::runtime::unique_id());
    fs::create_dir_all(root.join("bin")).unwrap();
    let root = root.canonicalize().unwrap();
    let shim = root.join("proc-read.so");
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-O2"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/unit/proc_read_fixture.c"))
        .arg("-o")
        .arg(&shim)
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sudo = root.join("bin/sudo");
    crate::write_test_file(
        &root,
        &sudo,
        r#"#!/bin/sh
set -eu
printf 'sudo' >> "$ASB_PROC_FIXTURE_LOG"
for value in "$@"; do printf ' [%s]' "$value" >> "$ASB_PROC_FIXTURE_LOG"; done
printf '\n' >> "$ASB_PROC_FIXTURE_LOG"
if test "$1" = -n; then
 shift
 if test "$1" = true && test "$ASB_PROC_FIXTURE_MODE" = interactive; then exit 1; fi
fi
case "$1" in
 true) exit 0 ;;
 sysctl) test "$ASB_PROC_FIXTURE_MODE" != sysctl-fails ;;
 mkdir|chown|chmod) test "$ASB_PROC_FIXTURE_MODE" != cgroup-fails ;;
 tee) while IFS= read -r line; do printf 'input [%s]\n' "$line" >> "$ASB_PROC_FIXTURE_LOG"; done ;;
 *) exit 99 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&sudo, fs::Permissions::from_mode(0o700)).unwrap();
    for (case, mode, cgroup, input) in [
        (
            "pending-success",
            "success",
            "0::/fixture-parent/session.scope\n",
            "",
        ),
        (
            "pending-interactive",
            "interactive",
            "0::/fixture-parent/session.scope\n",
            "",
        ),
        (
            "pending-sysctl-fails",
            "sysctl-fails",
            "0::/fixture-parent/session.scope\n",
            "",
        ),
        (
            "pending-declined",
            "interactive",
            "0::/fixture-parent/session.scope\n",
            "n\n",
        ),
        (
            "pending-defaulted",
            "interactive",
            "0::/fixture-parent/session.scope\n",
            "\n",
        ),
        ("missing-unified", "success", "1:memory:/fixture\n", ""),
        ("missing-name", "success", "0::/.\n", ""),
        ("not-scope", "success", "0::/parent/not-a-scope\n", ""),
        (
            "unsafe-parent",
            "success",
            "0::/../parent/session.scope\n",
            "",
        ),
        ("empty-parent", "success", "0::/session.scope\n", ""),
        (
            "cgroup-failed",
            "cgroup-fails",
            "0::/fixture-parent/session.scope\n",
            "",
        ),
        (
            "helpers-missing",
            "success",
            "0::/fixture-parent/session.scope\n",
            "",
        ),
    ] {
        let state = root.join(case);
        fs::create_dir(&state).unwrap();
        fs::write(state.join("paranoid"), "2\n").unwrap();
        fs::write(state.join("schedstats"), "0\n").unwrap();
        fs::write(state.join("cgroup"), cgroup).unwrap();
        let log = state.join("sudo.log");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_clear().env(CASE, case).env("LD_PRELOAD", &shim).env("ASB_PROC_FIXTURE_ROOT", &state)
            .env("ASB_PROC_FIXTURE_LOG", &log).env("ASB_PROC_FIXTURE_MODE", mode)
            .env("PATH", if case == "helpers-missing" { state.clone() } else { root.join("bin") })
            .current_dir(&state).args(["--exact", "project::coverage_tests::proc_boundaries::controlled_proc_inputs_cover_consent_and_cgroup_rejections_without_host_mutation", "--nocapture"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if let Ok(log) = fs::read_to_string(log) {
            assert!(
                log.lines()
                    .all(|line| line.starts_with("sudo") || line.starts_with("input"))
            );
            if case == "pending-success" {
                assert!(log.contains("[sysctl] [-w] [kernel.perf_event_paranoid=-1]"));
                assert!(log.contains("[sysctl] [-w] [kernel.sched_schedstats=1]"));
            }
            if case == "pending-interactive" {
                assert!(log.contains("sudo [sysctl] [-w]"));
            }
            if matches!(case, "pending-declined" | "pending-defaulted") {
                assert!(!log.contains("[sysctl]"));
                assert!(!log.contains("[mkdir]"));
            }
        }
        assert_eq!(fs::read_to_string(state.join("paranoid")).unwrap(), "2\n");
        assert_eq!(fs::read_to_string(state.join("schedstats")).unwrap(), "0\n");
        assert_eq!(fs::read_to_string(state.join("cgroup")).unwrap(), cgroup);
    }
    fs::remove_dir_all(root).unwrap();
}

fn child(case: &str) {
    // Failure to load the shim must fail before any capability path is called.
    assert_eq!(
        fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").unwrap(),
        "2\n"
    );
    assert_eq!(
        fs::read_to_string("/proc/sys/kernel/sched_schedstats").unwrap(),
        "0\n"
    );
    let fixture = PathBuf::from(std::env::var_os("ASB_PROC_FIXTURE_ROOT").unwrap());
    assert_eq!(
        fs::read_to_string("/proc/self/cgroup").unwrap(),
        fs::read_to_string(fixture.join("cgroup")).unwrap()
    );
    assert!(
        fixture.join("shim-active").is_file(),
        "proc remapping was not loaded; refuse capability checks"
    );
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .open("/proc/sys/kernel/perf_event_paranoid")
            .is_err()
    );
    if case == "helpers-missing" {
        assert!(!run_sudo_write("/never-written", b"fixture\n"));
        return;
    }
    let mut output = Vec::new();
    if case.starts_with("pending-") {
        let result = configure_host_capabilities(
            &mut output,
            !matches!(case, "pending-declined" | "pending-defaulted"),
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("system-wide kernel tracing settings"));
        match case {
            "pending-success" => {
                assert!(text.contains("changed kernel.perf_event_paranoid=-1"));
                assert!(result.is_some());
            }
            "pending-interactive" => {
                assert!(text.contains("sudo will now ask for your password"));
                assert!(result.is_none());
            }
            "pending-sysctl-fails" => {
                assert!(text.contains("could not change kernel.perf_event_paranoid=-1"))
            }
            "pending-declined" => assert!(text.contains("unchanged by user request")),
            "pending-defaulted" => assert!(text.contains("unchanged by user request")),
            _ => unreachable!(),
        }
    } else {
        let result = configure_cgroup_parent(&mut output, true).unwrap();
        assert!(result.is_none());
        let text = String::from_utf8(output).unwrap();
        let expected = match case {
            "missing-unified" => "unified cgroup-v2 membership was not found",
            "missing-name" => "not systemd-delegatable",
            "not-scope" => "not a systemd session scope",
            "unsafe-parent" | "empty-parent" => "unsafe to delegate",
            "cgroup-failed" => "could not create or delegate",
            _ => panic!("unknown fixture {case}"),
        };
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn proxy_empty_values_and_cgi_uppercase_removal_are_explicit() {
    let (proxy, omitted) = NetworkProxy::detect(&|name| (name == "http_proxy").then(String::new));
    assert!(proxy.http.is_none());
    assert!(!omitted);
    let proxy = NetworkProxy {
        http: Some("http://fixture.invalid:8888".into()),
        no_proxy: Some("".into()),
        ..Default::default()
    };
    let mut command = Command::new("not-executed");
    command.env("HTTP_PROXY", "must-be-removed");
    proxy.apply(&mut command, &|name| {
        (name == "REQUEST_METHOD").then(|| "GET".into())
    });
    let environment = command
        .get_envs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(environment[std::ffi::OsStr::new("HTTP_PROXY")], None);
    assert_eq!(
        environment[std::ffi::OsStr::new("http_proxy")],
        Some(std::ffi::OsStr::new("http://fixture.invalid:8888"))
    );
    assert_eq!(
        environment[std::ffi::OsStr::new("no_proxy")],
        Some(std::ffi::OsStr::new("localhost,127.0.0.1,::1"))
    );
}

#[test]
fn malformed_project_loading_reset_and_missing_configuration_are_actionable() {
    let root = std::env::temp_dir()
        .join("asb-project-boundary-fixtures")
        .join(crate::tools::runtime::unique_id());
    fs::create_dir_all(root.join(".asb")).unwrap();
    let config = root.join(".asb/project.toml");
    fs::write(&config, "not = [valid").unwrap();
    assert!(
        load(&root)
            .unwrap_err()
            .to_string()
            .contains("cannot parse")
    );
    assert!(
        reset_project(&root, &config)
            .unwrap_err()
            .to_string()
            .contains("cannot parse")
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), "not = [valid");
    fs::write(&config, [0xff, 0xfe]).unwrap();
    assert!(load(&root).unwrap_err().to_string().contains("cannot read"));
    fs::remove_file(&config).unwrap();
    assert!(
        initialized_config_path(&root)
            .unwrap_err()
            .to_string()
            .contains("no initialized project")
    );
    // Own the entire positive discovery chain. TMPDIR can sit beneath a real
    // project, which must never be loaded or passed to host configuration.
    let ancestor = root.join("owned-parent");
    fs::create_dir_all(ancestor.join(CONFIG_DIR)).unwrap();
    let ancestor_config = ancestor.join(CONFIG_DIR).join(CONFIG_FILE);
    fs::write(
        &ancestor_config,
        toml::to_string(&ProjectConfig::default()).unwrap(),
    )
    .unwrap();
    let child = ancestor.join("child");
    fs::create_dir(&child).unwrap();
    assert_eq!(load(&child).unwrap().0, ancestor);
    assert_eq!(initialized_config_path(&ancestor).unwrap(), ancestor_config);
    assert!(reject_symlink(&root.join("absent")).is_ok());
    let loop_path = root.join("loop");
    symlink(&loop_path, &loop_path).unwrap();
    assert!(
        reject_symlink(&loop_path.join("child"))
            .unwrap_err()
            .to_string()
            .contains("cannot inspect")
    );
    fs::remove_dir_all(root).unwrap();
}
