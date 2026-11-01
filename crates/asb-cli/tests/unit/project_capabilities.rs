//! Capability execution is intercepted by a fixture-only PATH. The fake sudo
//! logs argv/stdin and never invokes a command or writes a host control file.
use super::*;
use std::io::Write;

// Offered fixture input need not be consumed on already-configured hosts.
// An early exit must still be reaped and checked before accepting EPIPE.
#[cfg(target_os = "linux")]
fn check_fixture_input(
    output: &std::process::Output,
    input: std::io::Result<()>,
) -> Result<(), String> {
    if !output.status.success() {
        return Err(format!(
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    match input {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("fixture stdin delivery failed: {error}")),
    }
}

#[test]
#[cfg(target_os = "linux")]
fn fixture_input_settlement_requires_success_even_after_early_pipe_close() {
    use std::process::Stdio;
    for code in [0, 7] {
        let mut child = Command::new("/bin/sh")
            .env_clear()
            .args(["-c", if code == 0 { "exit 0" } else { "exit 7" }])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        // Waiting for this exact direct child makes the pipe closure certain,
        // rather than depending on a fast child winning a scheduling race.
        let output = child.wait_with_output().unwrap();
        let error = input.write_all(b"unused offered input").unwrap_err();
        drop(input);
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(check_fixture_input(&output, Err(error)).is_ok(), code == 0);
        if code == 0 {
            assert!(
                check_fixture_input(
                    &output,
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                )
                .is_err()
            );
            assert!(check_fixture_input(&output, Ok(())).is_ok());
        }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "project_proc_boundaries.rs"]
mod proc_boundaries;

#[cfg(target_os = "linux")]
#[test]
fn isolated_host_capability_consent_and_execution_diagnostics() {
    const CASE: &str = "ASB_PROJECT_CAPABILITY_CASE";
    let Ok(case) = std::env::var(CASE) else {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        // This test recursively launches itself.  Keep its fake sudo and
        // scripted-input files outside the shared Cargo target tree: a
        // previous interrupted child must not influence a later CI retry.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "asb-project-capability-fixtures-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let sudo = root.join("sudo");
        crate::write_test_file(
            &root,
            &sudo,
            r##"#!/bin/sh
printf 'sudo' >> "$ASB_PROJECT_CAPABILITY_LOG"
for arg in "$@"; do printf ' [%s]' "$arg" >> "$ASB_PROJECT_CAPABILITY_LOG"; done
printf '\n' >> "$ASB_PROJECT_CAPABILITY_LOG"
if [ "$ASB_PROJECT_CAPABILITY_MODE" = denied ]; then exit 1; fi
if [ "$ASB_PROJECT_CAPABILITY_MODE" = interactive ] && [ "$1" = '-n' ]; then exit 1; fi
case "$*" in
  *sysctl*) [ "$ASB_PROJECT_CAPABILITY_MODE" != sysctl-fails ] || exit 2;;
  *mkdir*) [ "$ASB_PROJECT_CAPABILITY_MODE" != mkdir-fails ] || exit 2;;
  *tee*)
    while IFS= read -r line; do printf 'input [%s]\n' "$line" >> "$ASB_PROJECT_CAPABILITY_LOG"; done
    [ "$ASB_PROJECT_CAPABILITY_MODE" != tee-fails ] || exit 2;;
esac
exit 0
"##,
        )
        .unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o700)).unwrap();
        let name = format!(
            "{}::isolated_host_capability_consent_and_execution_diagnostics",
            module_path!().split_once("::").unwrap().1
        );
        for (case, mode, input) in [
            ("approved", "success", ""),
            ("denied", "denied", ""),
            ("interactive", "interactive", ""),
            ("sysctl-fails", "sysctl-fails", ""),
            ("mkdir-fails", "mkdir-fails", ""),
            ("tee-fails", "tee-fails", ""),
            ("declined", "success", "n\nn\n"),
            ("invalid", "denied", "\n"),
            ("consent-yes", "success", "Y\n"),
            ("helpers", "success", ""),
            ("helpers-denied", "denied", ""),
        ] {
            let log = root.join(format!("{case}.log"));
            // The child may legitimately finish without reading optional
            // scripted input. Publish the exact bytes before spawn instead of
            // racing its stdin closure with a parent pipe write. Keep status
            // and stdout/stderr checks authoritative; never ignore EPIPE.
            let input_path = root.join(format!("{case}.stdin"));
            fs::write(&input_path, input.as_bytes()).unwrap();
            let input_file = fs::File::open(input_path).unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CASE, case)
                .env("ASB_PROJECT_CAPABILITY_MODE", mode)
                .env("ASB_PROJECT_CAPABILITY_LOG", &log)
                .env("ASB_PROJECT_CAPABILITY_ROOT", &root)
                .env("PATH", &root)
                .stdin(Stdio::from(input_file))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let output = child.wait_with_output().unwrap();
            check_fixture_input(&output, Ok(()))
                .unwrap_or_else(|error| panic!("case {case}: {error}"));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            if let Ok(log) = fs::read_to_string(log) {
                assert!(
                    log.lines()
                        .all(|line| line.starts_with("sudo") || line.starts_with("input"))
                );
                if case == "declined" || case == "invalid" {
                    assert!(!log.contains("sysctl"));
                    assert!(!log.contains("mkdir"));
                }
                if case == "helpers" {
                    assert!(log.contains("sudo [-n] [true]"));
                    assert!(
                        log.contains("sudo [-n] [sysctl] [-w] [kernel.fixture_does_not_exist=1]")
                    );
                    assert!(log.contains("sudo [sysctl] [-w] [kernel.fixture_does_not_exist=1]"));
                    assert!(log.contains("sudo [-n] [chmod] [0750] [/never-executed]"));
                    assert!(log.contains("input [fixture contents]"));
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let mut output = Vec::new();
    if case == "consent-yes" {
        assert!(request_sudo_consent(&mut output, false).unwrap());
        return;
    }
    if case == "helpers" || case == "helpers-denied" {
        let expected = case == "helpers";
        assert_eq!(passwordless_sudo_available(), expected);
        assert_eq!(
            run_sysctl("kernel.fixture_does_not_exist=1", true),
            expected
        );
        assert_eq!(
            run_sysctl("kernel.fixture_does_not_exist=1", false),
            expected
        );
        assert_eq!(
            run_sudo_status(&["chmod", "0750", "/never-executed"]),
            expected
        );
        let absent = PathBuf::from(std::env::var_os("ASB_PROJECT_CAPABILITY_ROOT").unwrap())
            .join("must-not-be-created");
        assert_eq!(
            run_sudo_write(absent.to_str().unwrap(), b"fixture contents\n"),
            expected
        );
        assert!(!absent.exists());
        return;
    }
    let yes = !matches!(case.as_str(), "declined" | "invalid");
    let result = configure_host_capabilities(&mut output, yes).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Host capability setup") || text.contains("passwordless sudo"));
    if case == "denied" || case == "interactive" || case == "invalid" {
        assert!(result.is_none());
    }
    if case == "declined" {
        assert!(text.contains("unchanged by user request") || text.contains("already enabled"));
    }
    if case == "invalid" {
        assert!(
            text.contains("no valid y/n answer")
                || text.contains("No answer received; defaulting to no.")
                || (text.contains("system-wide tracing settings unchanged by user request")
                    && text.contains("no delegated project cgroup was created"))
                || (text.matches("already enabled").count()
                    == crate::collectors::host_settings().len()
                    && text.contains("no delegated project cgroup was created"))
        );
    }
}
