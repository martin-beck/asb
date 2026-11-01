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
