//! Delegation commands are intercepted, never executed against host tracefs.
use super::*;
use std::fs;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
#[test]
fn owned_tracefs_fixture_covers_delegation_policy_without_sudo_or_host_tracefs() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    fn controls(path: &Path) {
        fs::create_dir_all(path.join("events")).unwrap();
        for file in [
            "trace_marker",
            "tracing_on",
            "buffer_size_kb",
            "trace_clock",
            "events/enable",
        ] {
            fs::write(path.join(file), b"fixture control").unwrap();
        }
    }
    for case in [
        "ready",
        "repair",
        "owned-empty",
        "partial",
        "root-traversal",
        "root-traversal-fails",
        "traversal-fails",
        "metadata-fails",
        "mkdir-fails",
        "chown-fails",
        "chmod-fails",
        "incomplete",
        "symlink",
    ] {
        let root =
            std::env::temp_dir().join(format!("asb-owned-tracefs-{case}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let parent = root.join("tracefs/instances");
        let tracefs_root = root.join("tracefs");
        fs::create_dir_all(&parent).unwrap();
        let names = instance_names(&root).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        if matches!(case, "root-traversal" | "root-traversal-fails") {
            // The fixture owner can still access this directory, but its lack
            // of world execute models the root-owned 0700 tracefs topology
            // that requires an explicit traversal repair for the caller.
            fs::set_permissions(&tracefs_root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        if case == "ready" {
            for name in &names {
                controls(&parent.join(name));
            }
        }
        if case == "partial" {
            controls(&parent.join(&names[0]));
        }
        if case == "owned-empty" {
            for name in &names {
                fs::create_dir(parent.join(name)).unwrap();
            }
        }
        if case == "symlink" {
            fs::create_dir_all(root.join("foreign")).unwrap();
            for name in &names {
                symlink(root.join("foreign"), parent.join(name)).unwrap();
            }
        }
        // A decoder file may disappear after enumeration. This deterministic
        // unreadable input exercises the exact repair command and warning.
        let metadata = if case == "metadata-fails" {
            vec![root.join("tracefs/events/missing-format")]
        } else {
            Vec::new()
        };
        let mut calls = Vec::<Vec<String>>::new();
        let mut output = Vec::new();
        let identity = fs::metadata("/proc/self").unwrap();
        let expected_owner = format!("{}:{}", identity.uid(), identity.gid());
        let result = configure_at(
            &root,
            &mut output,
            true,
            ConfigurePaths {
                tracefs_root: &tracefs_root,
                instances_parent: &parent,
                metadata_paths: Some(&metadata),
            },
            |args| {
                calls.push(args.iter().map(|value| (*value).to_owned()).collect());
                let target = Path::new(args.last().unwrap());
                assert!(
                    target.starts_with(&root),
                    "fixture runner must never touch host paths"
                );
                if args.starts_with(&["chmod", "o+x", "--"]) {
                    if target == tracefs_root {
                        return case != "root-traversal-fails";
                    }
                    assert_eq!(target, parent);
                    return case != "traversal-fails";
                }
                if args.starts_with(&["chmod", "o+r", "--"]) {
                    assert_eq!(target, metadata[0]);
                    return false;
                }
                assert_eq!(target.parent(), Some(parent.as_path()));
                assert!(names.iter().any(|name| target == parent.join(name)));
                if args.starts_with(&["mkdir", "--"]) {
                    if case == "mkdir-fails" {
                        return false;
                    }
                    fs::create_dir(target).unwrap();
                    return true;
                }
                if args[0] == "chown" {
                    assert_eq!(args[1], "-R");
                    assert_eq!(args[2], expected_owner);
                    assert_eq!(args[3], "--");
                    return case != "chown-fails";
                }
                assert!(args.starts_with(&["chmod", "-R", "u+rwX", "--"]));
                if case == "chmod-fails" {
                    return false;
                }
                if case != "incomplete" {
                    controls(target);
                }
                true
            },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        match case {
            "ready" => {
                assert_eq!(result, names);
                assert!(calls.is_empty());
                assert!(text.is_empty());
            }
            "repair" | "owned-empty" | "partial" | "metadata-fails" | "root-traversal" => {
                assert_eq!(result, names);
                assert!(
                    result
                        .iter()
                        .all(|name| delegated_ready(&parent.join(name)))
                );
                assert!(text.contains("project-specific tracefs instances"));
                if case == "partial" {
                    assert!(
                        !calls.iter().any(|args| args.last()
                            == Some(&parent.join(&names[0]).display().to_string()))
                    );
                }
                if case == "metadata-fails" {
                    assert!(text.contains("format metadata could not be delegated"));
                }
            }
            "traversal-fails" => {
                assert!(result.is_empty());
                assert_eq!(calls.len(), 1);
                assert!(text.contains("parent traversal could not be enabled"));
            }
            "root-traversal-fails" => {
                assert!(result.is_empty());
                assert_eq!(calls.len(), 1);
                assert!(text.contains("tracefs root traversal could not be enabled"));
            }
            "mkdir-fails" | "symlink" => {
                assert!(result.is_empty());
                assert!(text.contains("Did not adopt or modify"));
            }
            _ => {
                assert!(result.is_empty());
                assert!(text.contains("Could not delegate"));
            }
        }
        if case != "ready" {
            assert!(
                text.contains(
                    "No global event/buffer/tracing_on values or sudoers will be changed"
                )
            );
        }
        if case == "symlink" {
            assert!(calls.iter().all(|args| args.starts_with(&[
                "chmod".into(),
                "o+x".into(),
                "--".into()
            ])));
            assert!(fs::read_dir(root.join("foreign")).unwrap().next().is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn missing_tracefs_instances_are_reported_instead_of_silently_skipped() {
    let root = std::env::temp_dir().join(format!(
        "asb-owned-tracefs-missing-instances-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let tracefs_root = root.join("tracefs");
    fs::create_dir_all(&tracefs_root).unwrap();
    let mut output = Vec::new();
    let result = configure_at(
        &root,
        &mut output,
        true,
        ConfigurePaths {
            tracefs_root: &tracefs_root,
            instances_parent: &tracefs_root.join("instances"),
            metadata_paths: Some(&[]),
        },
        |_| panic!("missing tracefs must not run a host command"),
    )
    .unwrap();
    assert!(result.is_empty());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("instances path"));
    assert!(output.contains("Mount tracefs"));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn missing_tracefs_root_is_reported_instead_of_silently_skipped() {
    let root = std::env::temp_dir().join(format!(
        "asb-owned-tracefs-missing-root-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let tracefs_root = root.join("missing-tracefs");
    let mut output = Vec::new();
    let result = configure_at(
        &root,
        &mut output,
        true,
        ConfigurePaths {
            tracefs_root: &tracefs_root,
            instances_parent: &tracefs_root.join("instances"),
            metadata_paths: Some(&[]),
        },
        |_| panic!("missing tracefs must not run a host command"),
    )
    .unwrap();
    assert!(result.is_empty());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("tracefs root"));
    assert!(output.contains("Mount tracefs"));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn restricted_tracefs_root_requires_and_honors_explicit_consent() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "asb-owned-tracefs-root-refusal-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let tracefs_root = root.join("tracefs");
    let instances = tracefs_root.join("instances");
    fs::create_dir_all(&instances).unwrap();
    fs::set_permissions(&tracefs_root, fs::Permissions::from_mode(0o700)).unwrap();
    let mut output = Vec::new();
    let result = configure_at_with_consent(
        &root,
        &mut output,
        false,
        ConfigurePaths {
            tracefs_root: &tracefs_root,
            instances_parent: &instances,
            metadata_paths: Some(&[]),
        },
        |_| panic!("declined root traversal must not run a host command"),
        |_, yes| {
            assert!(!yes);
            Ok(false)
        },
    )
    .unwrap();
    assert!(result.is_empty());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("sudo chmod o+x"));
    assert!(output.contains("was declined"));
    fs::set_permissions(&tracefs_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn production_metadata_discovery_follows_root_traversal_repair() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "asb-owned-tracefs-late-metadata-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let tracefs_root = root.join("tracefs");
    let instances = tracefs_root.join("instances");
    let events = tracefs_root.join("events");
    fs::create_dir_all(events.join("sched/sched_switch")).unwrap();
    fs::create_dir_all(&instances).unwrap();
    for path in [
        events.join("header_page"),
        events.join("header_event"),
        events.join("sched/sched_switch/format"),
        events.join("sched/sched_switch/id"),
    ] {
        fs::write(path, "fixture").unwrap();
    }
    fs::set_permissions(&tracefs_root, fs::Permissions::from_mode(0o700)).unwrap();
    let discovered =
        crate::collectors::adapters::perfetto::required_trace_metadata_at(&tracefs_root);
    assert!(discovered.contains(&events.join("header_page")));
    assert!(discovered.contains(&events.join("sched/sched_switch/format")));

    let mut output = Vec::new();
    let mut calls = Vec::new();
    let names = instance_names(&root).unwrap();
    let result = configure_at(
        &root,
        &mut output,
        true,
        ConfigurePaths {
            tracefs_root: &tracefs_root,
            instances_parent: &instances,
            // This is the production discovery path, deliberately not an
            // injected fixture list.
            metadata_paths: None,
        },
        |args| {
            calls.push(
                args.iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
            );
            let target = Path::new(args.last().unwrap());
            if args.starts_with(&["chmod", "o+x", "--"]) {
                return target == tracefs_root;
            }
            if args.starts_with(&["mkdir", "--"]) {
                fs::create_dir(target).unwrap();
                return true;
            }
            if args.starts_with(&["chown", "-R"])
                || args.starts_with(&["chmod", "-R", "u+rwX", "--"])
            {
                fs::create_dir_all(target.join("events")).unwrap();
                for file in [
                    "trace_marker",
                    "tracing_on",
                    "buffer_size_kb",
                    "trace_clock",
                    "events/enable",
                ] {
                    fs::write(target.join(file), "fixture").unwrap();
                }
                return true;
            }
            false
        },
    )
    .unwrap();
    assert_eq!(result, names);
    assert_eq!(
        calls[0],
        vec!["chmod", "o+x", "--", tracefs_root.to_str().unwrap()]
    );
    fs::set_permissions(&tracefs_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn isolated_delegation_consent_logs_scoped_commands_without_host_mutation() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    const CASE: &str = "ASB_PERFETTO_DELEGATION_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-delegation-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let name = format!(
            "{}::isolated_delegation_consent_logs_scoped_commands_without_host_mutation",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "declined",
            "approved",
            "sudo-fails",
            "metadata-fails",
            "chown-fails",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(directory.join("commands")).unwrap();
            let sudo = directory.join("commands/sudo");
            crate::write_test_file(
                &directory,
                &sudo,
                r#"#!/bin/sh
printf 'sudo' >> "$HOME/calls"
for value in "$@"; do printf ' [%s]' "$value" >> "$HOME/calls"; done
printf '\n' >> "$HOME/calls"
[ "$ASB_PERFETTO_DELEGATION_CASE" != sudo-fails ] || exit 1
if [ "$ASB_PERFETTO_DELEGATION_CASE" = metadata-fails ] && [ "$2" = o+r ]; then exit 1; fi
if [ "$ASB_PERFETTO_DELEGATION_CASE" = chown-fails ] && [ "$1" = chown ]; then exit 1; fi
exit 0
"#,
            )
            .unwrap();
            fs::set_permissions(sudo, fs::Permissions::from_mode(0o700)).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .env_clear()
                .env(CASE, case)
                .env("PATH", directory.join("commands"))
                .env("HOME", &directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
            let mut child = command.spawn().unwrap();
            child.stdin.take().unwrap().write_all(b"n\n").unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let names = instance_names(&root).unwrap();
    let parent = Path::new("/sys/kernel/tracing/instances");
    for name in &names {
        assert!(
            !parent.join(name).exists(),
            "never select an existing host tracing instance"
        );
    }
    let mut output = Vec::new();
    let result = configure(&root, &mut output, case != "declined").unwrap();
    assert!(result.is_empty());
    for name in &names {
        assert!(
            !parent.join(name).exists(),
            "fake sudo must never create an instance"
        );
    }
    if parent.is_dir() {
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("project-specific tracefs instances"));
        if case == "declined" {
            assert!(!root.join("calls").exists());
        } else {
            let calls = fs::read_to_string(root.join("calls")).unwrap();
            assert!(calls.lines().all(|line| line.starts_with("sudo [")));
            assert!(!calls.contains("sysctl"));
            assert!(!calls.contains("tracing_on"));
            assert!(!calls.contains("sudoers"));
            assert!(
                output.contains("Could not delegate")
                    || output.contains("Did not adopt")
                    || output.contains("could not be enabled")
            );
        }
    }
}
#[cfg(unix)]
#[test]
fn admission_and_reset_reject_symlinked_coordination_without_touching_targets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/perfetto-lock-path-fixtures")
        .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
    fs::create_dir_all(root.join(".asb/perfetto")).unwrap();
    let root = root.canonicalize().unwrap();
    let target = root.join("unrelated");
    fs::write(&target, "unchanged").unwrap();
    let admission = root.join(".asb-perfetto-admission.lock");
    std::os::unix::fs::symlink(&target, &admission).unwrap();
    assert!(
        lock_admission(&root)
            .unwrap_err()
            .to_string()
            .contains("cannot open Perfetto admission lock")
    );
    fs::remove_file(&admission).unwrap();
    let instance = instance_names(&root).unwrap().remove(0);
    let lease = root.join(".asb/perfetto").join(format!("{instance}.lease"));
    std::os::unix::fs::symlink(&target, &lease).unwrap();
    assert!(
        lock_for_reset(&root)
            .unwrap_err()
            .to_string()
            .contains("cannot inspect Perfetto lease before reset")
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "unchanged");
    assert!(
        instance_names(&root.join("missing"))
            .unwrap_err()
            .contains("cannot identify benchmark project")
    );
    fs::remove_dir_all(root).unwrap();
}
