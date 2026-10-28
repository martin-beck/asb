//! Fake source/build commands never download or run a host collector.
use super::*;
use std::process::Command;
#[cfg(unix)]
#[test]
fn isolated_bpftrace_build_and_install_contracts() {
    use std::os::unix::fs::PermissionsExt;
    const CASE: &str = "ASB_BPFTRACE_INSTALL_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/bpftrace-installer-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let name = format!(
            "{}::isolated_bpftrace_build_and_install_contracts",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "build",
            "held",
            "cached",
            "cached-no-download-tools",
            "relative-path",
            "curl-fails",
            "curl-127",
            "tar-fails",
            "tar-127",
            "configure-fails",
            "configure-127",
            "compile-fails",
            "compile-127",
            "no-output",
            "missing-curl",
            "cancelled-missing-curl",
            "missing-tar",
            "missing-cmake",
            "source-blocked",
            "install",
            "version-fails",
            "kernel-fails",
            "arch-fails",
            "bin-blocked",
            "copy-blocked",
            "metadata-blocked",
        ] {
            let path = root.join(case);
            fs::create_dir_all(path.join("commands")).unwrap();
            if case == "held" && !cfg!(target_os = "linux") {
                continue;
            }
            #[cfg(target_os = "linux")]
            if case == "held" {
                // Execute the private builder entry through the real native
                // gate. The public installer legitimately prefers a host
                // bpftrace, so it cannot force this source-only fixture route.
                let current = std::env::current_exe().unwrap();
                let binary = current.parent().unwrap().parent().unwrap().join("asb");
                let mut launch = crate::launch::ProcessLaunch::new(current);
                launch.arg("--exact").arg(&name).arg("--nocapture");
                let (mut command, route) = launch.native(Some(&binary));
                command
                    .current_dir(&path)
                    .env(CASE, case)
                    .env("ASB_INSTALL_FIXTURE_ROOT", &path)
                    // The shared download cache is deliberately user scoped.
                    // Keep each nested installer fixture independent of any
                    // host cache entry (and of sibling fixture cases).
                    .env("HOME", &path)
                    .env("PATH", path.join("commands"))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let admission =
                    crate::process_gate::Admission::new(route.validate_gate(&mut command).unwrap())
                        .unwrap();
                let mut gate = admission.spawn().unwrap();
                gate.resume().unwrap();
                for (phase, expected) in [("download", "10 downloading"), ("build", "60 building")]
                {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !path.join(format!("{phase}-held")).exists() {
                        assert!(
                            gate.try_wait().unwrap().is_none(),
                            "fixture exited before {phase}"
                        );
                        assert!(std::time::Instant::now() < deadline, "{phase} never held");
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    assert_eq!(fs::read_to_string(path.join("phase")).unwrap(), expected);
                    assert!(
                        gate.try_wait().unwrap().is_none(),
                        "stage must be visible before the command finishes"
                    );
                    fs::write(
                        path.join(format!("{phase}-release")),
                        "release after observing phase",
                    )
                    .unwrap();
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if let Some(status) = gate.try_wait().unwrap() {
                        assert!(status.success());
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                continue;
            }
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&path)
                .env(CASE, case)
                .env("ASB_INSTALL_FIXTURE_ROOT", &path)
                .env("HOME", &path)
                .env(
                    "PATH",
                    if case == "relative-path" {
                        PathBuf::from("commands")
                    } else {
                        path.join("commands")
                    },
                )
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(std::env::var_os("ASB_INSTALL_FIXTURE_ROOT").unwrap());
    let context = OperationContext::new(&root, &crate::project::ProjectConfig::default()).unwrap();
    let commands = root.join("commands");
    let tool = root.join("tool");
    let executable = |name: &str, body: &str| {
        let path = commands.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    };
    executable(
        "curl",
        r#"printf 'curl %s\n' "$*" >> "$ASB_INSTALL_FIXTURE_ROOT/calls"
printf '%s\n' "$0" >> "$ASB_INSTALL_FIXTURE_ROOT/programs"
if [ "$ASB_BPFTRACE_INSTALL_CASE" = held ]; then
 printf ready > "$ASB_INSTALL_FIXTURE_ROOT/download-held"
 while [ ! -f "$ASB_INSTALL_FIXTURE_ROOT/download-release" ]; do /bin/sleep .005; done
fi
[ "$ASB_BPFTRACE_INSTALL_CASE" != curl-fails ] || exit 2
[ "$ASB_BPFTRACE_INSTALL_CASE" != curl-127 ] || exit 127
while [ "$#" -gt 0 ]; do
 if [ "$1" = --output ]; then shift; printf 'archive fixture' > "$1"; fi
 shift
done"#,
    );
    executable(
        "tar",
        r#"printf 'tar %s\n' "$*" >> "$ASB_INSTALL_FIXTURE_ROOT/calls"
printf '%s\n' "$0" >> "$ASB_INSTALL_FIXTURE_ROOT/programs"
[ "$ASB_BPFTRACE_INSTALL_CASE" != tar-127 ] || exit 127
[ "$ASB_BPFTRACE_INSTALL_CASE" != tar-fails ]"#,
    );
    executable(
        "cmake",
        r#"printf 'cmake %s\n' "$*" >> "$ASB_INSTALL_FIXTURE_ROOT/calls"
printf '%s\n' "$0" >> "$ASB_INSTALL_FIXTURE_ROOT/programs"
if [ "$1" = --build ]; then
 if [ "$ASB_BPFTRACE_INSTALL_CASE" = held ]; then
  printf ready > "$ASB_INSTALL_FIXTURE_ROOT/build-held"
  while [ ! -f "$ASB_INSTALL_FIXTURE_ROOT/build-release" ]; do /bin/sleep .005; done
 fi
 [ "$ASB_BPFTRACE_INSTALL_CASE" != compile-fails ] || exit 2
 [ "$ASB_BPFTRACE_INSTALL_CASE" != compile-127 ] || exit 127
 [ "$ASB_BPFTRACE_INSTALL_CASE" != no-output ] || exit 0
 /bin/mkdir -p "$2/src"
 printf 'fixture binary' > "$2/src/bpftrace"
else
 [ "$ASB_BPFTRACE_INSTALL_CASE" != configure-127 ] || exit 127
 [ "$ASB_BPFTRACE_INSTALL_CASE" != configure-fails ] || exit 2
fi"#,
    );
    let mut progress = Vec::new();
    if matches!(
        case.as_str(),
        "install"
            | "version-fails"
            | "kernel-fails"
            | "arch-fails"
            | "bin-blocked"
            | "copy-blocked"
            | "metadata-blocked"
    ) {
        executable(
            "bpftrace",
            r#"[ "$1" = --version ] || exit 3
[ "$ASB_BPFTRACE_INSTALL_CASE" != version-fails ] || exit 2
printf 'fixture bpftrace "quoted"\n'"#,
        );
        executable(
            "uname",
            r#"case "$1" in
-r) [ "$ASB_BPFTRACE_INSTALL_CASE" != kernel-fails ] || exit 2; printf 'fixture-kernel\n';;
-m) [ "$ASB_BPFTRACE_INSTALL_CASE" != arch-fails ] || exit 2; printf 'fixture-arch\n';;
*) exit 3;;
esac"#,
        );
        fs::create_dir_all(&tool).unwrap();
        match case.as_str() {
            "bin-blocked" => fs::write(tool.join("bin"), "blocked").unwrap(),
            "copy-blocked" => fs::create_dir_all(tool.join("bin/bpftrace")).unwrap(),
            "metadata-blocked" => {
                fs::create_dir_all(tool.join("bpftrace-compatibility.json")).unwrap()
            }
            _ => {}
        }
        let result = install_bpftrace(&context, &tool, &mut |percent, _| progress.push(percent));
        if case == "install" {
            let binary = result.unwrap();
            assert_eq!(binary, tool.join("bin/bpftrace"));
            assert_eq!(
                fs::read(&binary).unwrap(),
                fs::read(commands.join("bpftrace")).unwrap()
            );
            assert_ne!(
                fs::metadata(binary).unwrap().permissions().mode() & 0o111,
                0
            );
            let json: serde_json::Value = serde_json::from_slice(
                &fs::read(tool.join("bpftrace-compatibility.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(json["schema_version"], 1);
            assert_eq!(json["tool"], "bpftrace");
            assert_eq!(json["version"], "fixture bpftrace \"quoted\"");
            assert_eq!(json["kernel_release"], "fixture-kernel");
            assert_eq!(json["kernel_architecture"], "fixture-arch");
            assert_eq!(progress, [10, 95, 100]);
        } else {
            let error = result.unwrap_err().to_string();
            let expected = match case.as_str() {
                "version-fails" => "cannot inspect bpftrace version",
                "kernel-fails" => "cannot inspect kernel release",
                "arch-fails" => "cannot inspect kernel architecture",
                "bin-blocked" => "cannot create",
                "copy-blocked" => "cannot copy host bpftrace",
                "metadata-blocked" => "cannot write bpftrace compatibility metadata",
                _ => unreachable!(),
            };
            assert!(error.contains(expected), "{error}");
            if case == "bin-blocked" {
                assert!(error.contains("cannot create bpftrace binary directory"));
                assert!(error.contains(&tool.join("bin").display().to_string()));
            }
            assert!(!progress.contains(&100));
        }
        return;
    }
    if case == "cached" || case == "cached-no-download-tools" {
        fs::create_dir_all(tool.join("source")).unwrap();
    }
    if case == "cached-no-download-tools" {
        fs::remove_file(commands.join("curl")).unwrap();
        fs::remove_file(commands.join("tar")).unwrap();
    }
    if case == "source-blocked" {
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("source"), "blocked").unwrap();
    }
    for (name, command) in [
        ("missing-curl", "curl"),
        ("missing-tar", "tar"),
        ("missing-cmake", "cmake"),
    ] {
        if case == name {
            fs::remove_file(commands.join(command)).unwrap();
        }
    }
    if case == "cancelled-missing-curl" {
        fs::remove_file(commands.join("curl")).unwrap();
        context.cancel();
    }
    let result = build_bpftrace(&context, &tool, &mut |percent, stage| {
        progress.push(percent);
        if case == "held" {
            fs::write(root.join("phase"), format!("{percent} {}", stage.label())).unwrap();
        }
    });
    if case == "cancelled-missing-curl" {
        assert!(matches!(result, Err(InstallFailure::Abort(_))));
        assert_eq!(progress, [10]);
        assert!(!root.join("calls").exists());
        assert!(!tool.join("build/src/bpftrace").exists());
        return;
    }
    if matches!(
        case.as_str(),
        "build" | "cached" | "cached-no-download-tools" | "relative-path" | "held"
    ) {
        assert_eq!(result.unwrap(), tool.join("build/src/bpftrace"));
        assert_eq!(
            progress,
            if case != "cached" && case != "cached-no-download-tools" {
                vec![10, 25, 40, 60, 90]
            } else {
                vec![40, 60, 90]
            }
        );
        let log = fs::read_to_string(root.join("calls")).unwrap();
        assert!(log.contains("-DBUILD_TESTING=OFF -DBPFTRACE_BUILD_TESTING=OFF"));
        assert!(log.contains("--target bpftrace --parallel"));
        if case != "cached" && case != "cached-no-download-tools" {
            assert!(log.contains(BPFTRACE_SOURCE));
            assert!(log.contains("--strip-components=1"));
        } else {
            assert!(!log.lines().any(|line| line.starts_with("curl ")));
            assert!(!log.lines().any(|line| line.starts_with("tar ")));
        }
        #[cfg(target_os = "linux")]
        for selected in fs::read_to_string(root.join("programs")).unwrap().lines() {
            let selected = Path::new(selected);
            assert!(selected.is_absolute(), "{selected:?}");
            assert_eq!(selected.parent(), Some(commands.as_path()));
        }
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match case.as_str() {
            "curl-fails" | "curl-127" => "could not download pinned bpftrace source",
            "tar-fails" | "tar-127" => "could not extract",
            "configure-fails" | "configure-127" => "cmake could not configure",
            "compile-fails" | "compile-127" => "source compilation failed",
            "no-output" => "without a discoverable executable",
            // The shared bounded downloader owns its curl prerequisite;
            // extraction and compilation prerequisites remain bpftrace-build
            // responsibilities.
            "missing-curl" => "pinned bpftrace source needs",
            "missing-tar" | "missing-cmake" => "source build needs",
            "source-blocked" => "cannot create bpftrace source directory",
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{error}");
        if let Some(dependency) = case.strip_prefix("missing-") {
            assert!(error.contains(&format!("`{dependency}`")), "{error}");
            if dependency == "curl" {
                // The reusable bounded downloader owns curl discovery, so it
                // reports its generic, actionable curl installation remedy.
                assert!(error.contains("sudo apt-get install -y curl"), "{error}");
            } else {
                assert!(error.contains(bpftrace_dependency_commands(dependency)));
            }
        }
        if case == "source-blocked" {
            assert!(error.contains(&tool.join("source").display().to_string()));
        }
        let expected_steps = match case.as_str() {
            "missing-curl" => Some((vec![10], 0)),
            "curl-127" => Some((vec![10], 1)),
            "missing-tar" => Some((vec![10, 25], 1)),
            "tar-127" => Some((vec![10, 25], 2)),
            "missing-cmake" => Some((vec![10, 25, 40], 2)),
            "configure-127" => Some((vec![10, 25, 40], 3)),
            "compile-127" => Some((vec![10, 25, 40, 60], 4)),
            _ => None,
        };
        if let Some((expected_progress, expected_calls)) = expected_steps {
            assert_eq!(progress, expected_progress);
            let calls = fs::read_to_string(root.join("calls")).unwrap_or_default();
            assert_eq!(calls.lines().count(), expected_calls, "{calls}");
            assert!(!tool.join("build/src/bpftrace").exists());
            assert!(!tool.join("bin/bpftrace").exists());
            assert!(!tool.join("bpftrace-compatibility.json").exists());
            if case.ends_with("-127") {
                assert!(!error.contains("source build needs"), "{error}");
            }
        }
    }
}
