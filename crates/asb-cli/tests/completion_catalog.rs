//! End-to-end checks for the generated shell completion catalog bridge.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asb-completion-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".asb")).unwrap();
        fs::create_dir_all(root.join("workloads")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        fs::write(
            root.join("workloads/local-catalog.txt"),
            "Local completion fixture.",
        )
        .unwrap();
        fs::write(
            root.join("workloads/local-catalog.workload.toml"),
            "workload='local-catalog'\ntrials=[{id='selected-case'}]\n",
        )
        .unwrap();
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn asb() -> &'static str {
    env!("CARGO_BIN_EXE_asb")
}

fn generated(shell: &str, root: &Path) -> PathBuf {
    let output = Command::new(asb())
        .args(["--__generate-completion", shell])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = root.join(format!("asb.{shell}"));
    fs::write(&script, output.stdout).unwrap();
    script
}

#[test]
fn completion_values_cover_every_registry_and_project_local_catalog() {
    let project = Project::new();
    for (kind, required) in [
        ("agent", "opencode"),
        ("provider", "openrouter"),
        ("tool", "cli2key"),
        ("collector", "cpu-accounting"),
        ("workload", "local-catalog"),
        ("trial-set", "local-catalog"),
        ("model", "gpt-6.1-sol"),
    ] {
        let output = Command::new(asb())
            .args(["--__complete-values", kind])
            .current_dir(&project.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == required),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn generated_bash_completes_project_local_workloads_and_zsh_has_the_same_bridge() {
    let project = Project::new();
    let script = generated("bash", &project.0);
    let output = Command::new("bash")
        .args([
            "-c",
            r#"source "$1"; COMP_WORDS=("$2" setup --workload ""); COMP_CWORD=3; _asb "$2" "" --workload; printf '%s\n' "${COMPREPLY[@]}""#,
            "completion-fixture",
        ])
        .arg(&script)
        .arg(asb())
        .current_dir(&project.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == "local-catalog"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let zsh = fs::read_to_string(generated("zsh", &project.0)).unwrap();
    assert!(zsh.contains("_asb_catalog_complete"));
    assert!(zsh.contains("--__complete-values"));
    assert!(zsh.contains("compdef _asb asb"));
}

#[test]
fn generated_shells_use_the_explicit_project_catalog_in_both_flag_forms() {
    let caller = Project::new();
    let selected = Project::new();
    fs::write(selected.0.join("workloads/selected-only.txt"), "fixture").unwrap();
    fs::write(caller.0.join("workloads/caller-only.txt"), "fixture").unwrap();
    fs::write(
        selected.0.join("workloads/selected-set.workload.toml"),
        "fixture",
    )
    .unwrap();
    fs::write(
        caller.0.join("workloads/caller-set.workload.toml"),
        "fixture",
    )
    .unwrap();
    let target = selected.0.to_str().unwrap();
    for shell in ["bash", "zsh"] {
        if Command::new(shell).arg("--version").output().is_err() {
            continue;
        }
        let script = generated(shell, &caller.0);
        let mut flags = vec![
            vec!["--project-dir".to_owned(), target.to_owned()],
            vec![format!("--project-dir={target}")],
        ];
        if shell == "bash" {
            flags.push(vec!["--project-dir".into(), "=".into(), target.into()]);
        }
        for flag in flags {
            for (command, prefix, expected, unexpected) in [
                (
                    "setup",
                    vec!["setup", "--workload"],
                    "selected-only",
                    "caller-only",
                ),
                (
                    "install",
                    vec!["workload", "install"],
                    "selected-only",
                    "caller-only",
                ),
                (
                    "trial-set",
                    vec!["setup", "--trial-set"],
                    "selected-set",
                    "caller-set",
                ),
            ] {
                for before_command in [true, false] {
                    let mut words = vec![asb().to_owned()];
                    if before_command {
                        words.extend(flag.clone());
                    }
                    words.extend(prefix.iter().take(1).map(|word| (*word).to_owned()));
                    if !before_command {
                        words.extend(flag.clone());
                    }
                    words.extend(prefix.iter().skip(1).map(|word| (*word).to_owned()));
                    words.push(String::new());
                    let body = if shell == "bash" {
                        r#"source "$1"; shift; COMP_WORDS=("$@"); COMP_CWORD=$(($# - 1)); _asb; printf '%s\n' "${COMPREPLY[@]}""#
                    } else {
                        r#"compdef() { :; }; source "$1"; shift; words=("$@"); CURRENT=$#; _describe() { printf '%s\n' "${candidates[@]}"; }; _asb_catalog_complete"#
                    };
                    let output = Command::new(shell)
                        .arg("-c")
                        .arg(body)
                        .arg("fixture")
                        .arg(&script)
                        .args(&words)
                        .current_dir(&caller.0)
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{shell} {command}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let candidates = String::from_utf8(output.stdout).unwrap();
                    assert!(
                        candidates.lines().any(|line| line == expected),
                        "{shell} {command}: {candidates}"
                    );
                    assert!(
                        !candidates.lines().any(|line| line == unexpected),
                        "{shell} {command}: {candidates}"
                    );
                }
            }
        }
    }
}

fn override_catalog(project: &Path, name: &str) {
    let directory = project.join("custom-workloads");
    let source = directory.join("agentbench/source/configs/tasks");
    fs::create_dir_all(&source).unwrap();
    fs::write(directory.join(format!("{name}.txt")), "fixture").unwrap();
    fs::write(directory.join(format!("{name}.workload.toml")), "fixture").unwrap();
    fs::write(source.join(format!("{name}.yaml")), "fixture").unwrap();
    let marker = "THUDM/AgentBench@d1e4a10db08c87075c78972e48ecc182be03e2d5";
    fs::write(directory.join("agentbench/ASB-BUNDLE"), marker).unwrap();
    fs::write(
        directory.join("agentbench/catalog.json"),
        format!(r#"{{"schema_version":1,"workload":"agentbench","marker":"{marker}","tasks":[{{"id":"{name}","description":"fixture"}}]}}"#),
    ).unwrap();
}

#[test]
fn workload_override_applies_to_listing_and_generated_shell_completion() {
    let caller = Project::new();
    let selected = Project::new();
    override_catalog(&caller.0, "caller-task");
    override_catalog(&selected.0, "selected-task");
    let target = selected.0.to_str().unwrap();
    for override_dir in [
        "custom-workloads".to_owned(),
        selected
            .0
            .join("custom-workloads")
            .to_string_lossy()
            .into_owned(),
    ] {
        let output = Command::new(asb())
            .current_dir(&caller.0)
            .env("ASB_WORKLOAD_DIR", &override_dir)
            .args(["workload", "agentbench", "trials", "--project-dir", target])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let listing = String::from_utf8(output.stdout).unwrap();
        assert!(listing.contains("selected-task"), "{listing}");
        assert!(!listing.contains("caller-task"), "{listing}");
        for shell in ["bash", "zsh"] {
            if Command::new(shell).arg("--version").output().is_err() {
                continue;
            }
            let script = generated(shell, &caller.0);
            for flag in [
                vec!["--project-dir".to_owned(), target.to_owned()],
                vec![format!("--project-dir={target}")],
            ] {
                for prefix in [
                    vec!["setup", "--workload"],
                    vec!["setup", "--trial-set"],
                    vec!["setup", "--workload", "agentbench", "--trial"],
                ] {
                    let mut words = vec![asb().to_owned()];
                    words.extend(flag.clone());
                    words.extend(prefix.iter().map(|s| (*s).to_owned()));
                    words.push(String::new());
                    let body = if shell == "bash" {
                        r#"source "$1"; shift; COMP_WORDS=("$@"); COMP_CWORD=$(($# - 1)); _asb; printf '%s\n' "${COMPREPLY[@]}""#
                    } else {
                        r#"compdef() { :; }; source "$1"; shift; words=("$@"); CURRENT=$#; _describe() { printf '%s\n' "${candidates[@]}"; }; _asb_catalog_complete"#
                    };
                    let output = Command::new(shell)
                        .args(["-c", body, "fixture"])
                        .arg(&script)
                        .args(&words)
                        .current_dir(&caller.0)
                        .env("ASB_WORKLOAD_DIR", &override_dir)
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let candidates = String::from_utf8(output.stdout).unwrap();
                    assert!(
                        candidates.lines().any(|line| line == "selected-task"),
                        "{shell} {prefix:?}: {candidates}"
                    );
                    assert!(
                        !candidates.lines().any(|line| line == "caller-task"),
                        "{shell} {prefix:?}: {candidates}"
                    );
                }
            }
        }
    }
}

#[test]
fn relative_workload_override_uses_nested_invocation_directory() {
    let project = Project::new();
    let nested = project.0.join("nested");
    fs::create_dir_all(&nested).unwrap();
    override_catalog(&project.0, "root-task");
    override_catalog(&nested, "nested-task");
    for args in [
        vec!["workload", "agentbench", "trials"],
        vec!["--__complete-values", "trial", "agentbench"],
    ] {
        let output = Command::new(asb())
            .current_dir(&nested)
            .env("ASB_WORKLOAD_DIR", "custom-workloads")
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let listing = String::from_utf8(output.stdout).unwrap();
        assert!(listing.contains("nested-task"), "{args:?}: {listing}");
        assert!(!listing.contains("root-task"), "{args:?}: {listing}");
    }
}

#[test]
fn concurrent_library_calls_keep_project_trial_catalogs_isolated() {
    use std::ffi::OsString;
    use std::sync::{Arc, Barrier};

    let projects = [Project::new(), Project::new()];
    for (project, task) in projects.iter().zip(["first-task", "second-task"]) {
        override_catalog(&project.0, task);
        fs::write(
            project.0.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='custom-workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (project, expected, unexpected) in [
            (&projects[0], "first-task", "second-task"),
            (&projects[1], "second-task", "first-task"),
        ] {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                let args: Vec<OsString> = ["workload", "agentbench", "trials", "--project-dir"]
                    .into_iter()
                    .map(OsString::from)
                    .chain(std::iter::once(project.0.as_os_str().to_owned()))
                    .collect();
                barrier.wait();
                for _ in 0..50 {
                    let mut output = Vec::new();
                    let mut errors = Vec::new();
                    let code = asb_cli::run(&args, &mut output, &mut errors);
                    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&errors));
                    let listing = String::from_utf8(output).unwrap();
                    assert!(listing.contains(expected), "{listing}");
                    assert!(!listing.contains(unexpected), "{listing}");
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
}
