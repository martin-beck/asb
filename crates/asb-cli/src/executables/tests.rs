//! Offline local-source installs and registry-owned execution policy.
use super::*;
use crate::{agents, install, tools};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

const CHILD: &str = "ASB_EXECUTABLE_POLICY_CASE";
const CONTENT: &[u8] = b"#!/bin/sh\nprintf 'explicit source override\\n'\n";
const TOOLS: &str = "custom tools/runtime";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/executable-policy-fixtures")
            .join(tools::runtime::unique_id());
        fs::create_dir_all(root.join(".asb")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            format!("tools_dir='{TOOLS}'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n"),
        )
        .unwrap();
        fs::write(root.join("fixture-source"), CONTENT).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn command(&self, case: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env(CHILD, case)
            .env("ASB_EXECUTABLE_POLICY_ROOT", &self.0)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .current_dir(&self.0)
            .args([
                "--exact",
                "executables::tests::executable_policy_fixture_child",
                "--nocapture",
            ]);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    fn run(&self, case: &str) {
        assert_child(self.command(case));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn assert_child(mut command: Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn script(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn source_override_is_ready_and_actually_executed_for_every_registered_agent_and_tool() {
    Fixture::new().run("matrix");
}

#[test]
fn broken_nonexecutable_and_symlink_overrides_fail_closed_in_all_registries() {
    Fixture::new().run("broken");
}

#[test]
fn external_execution_override_does_not_claim_a_project_installation() {
    let fixture = Fixture::new();
    let mut command = fixture.command("external");
    command.env("ASB_TOOL_DIR", fixture.0.join("external"));
    assert_child(command);
}

#[test]
fn legacy_shared_layout_is_scoped_to_configured_project_not_the_cwd() {
    let fixture = Fixture::new();
    let foreign = fixture.0.join("foreign");
    script(&foreign.join(".asb-tools/bin/codex"), CONTENT);
    let mut command = fixture.command("legacy");
    command.current_dir(foreign);
    assert_child(command);
}

#[test]
fn host_discovery_checks_permissions_but_never_claims_a_project_local_installation() {
    let fixture = Fixture::new();
    let first = fixture.0.join("host-first");
    let second = fixture.0.join("host-second");
    script(&first.join("mpstat"), CONTENT);
    fs::set_permissions(first.join("mpstat"), fs::Permissions::from_mode(0o600)).unwrap();
    script(&second.join("mpstat"), CONTENT);
    script(&second.join("pidstat"), CONTENT);
    script(&second.join("opencode"), CONTENT);
    let mut command = fixture.command("host");
    command.env("PATH", std::env::join_paths([first, second]).unwrap());
    assert_child(command);
}

#[test]
fn generic_lookup_rejects_nonfiles_missing_entries_and_unusable_first_candidates() {
    let fixture = Fixture::new();
    assert!(matches!(
        lookup([fixture.0.join("missing")]),
        Lookup::Missing
    ));
    assert!(matches!(lookup([fixture.0.clone()]), Lookup::Unusable(_)));
    let low = fixture.0.join("low");
    script(&low, CONTENT);
    let high = fixture.0.join("high");
    fs::write(&high, CONTENT).unwrap();
    assert!(matches!(lookup([high.clone(), low.clone()]), Lookup::Unusable(path) if path == high));
    fs::remove_file(&high).unwrap();
    symlink(&low, &high).unwrap();
    assert_eq!(
        lookup([high.clone()]).path(),
        Some(low.canonicalize().unwrap())
    );
    fs::remove_file(&low).unwrap();
    assert!(matches!(lookup([high]), Lookup::Unusable(_)));
}

#[test]
fn executable_policy_fixture_child() {
    let Ok(case) = std::env::var(CHILD) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("ASB_EXECUTABLE_POLICY_ROOT").unwrap());
    let tools_root = root.join(TOOLS);
    match case.as_str() {
        "matrix" => {
            // Dependency setup must reuse local perf, never perform a download.
            script(&tools_root.join("perf/bin/perf"), CONTENT);
            for adapter in agents::all() {
                let installed = tools_root.join(adapter.id());
                let old = match adapter.recipe() {
                    install::Recipe::Npm { .. } => installed
                        .join("node_modules/.bin")
                        .join(adapter.executable()),
                    install::Recipe::Python { .. } => {
                        installed.join("venv/bin").join(adapter.executable())
                    }
                    _ => installed.join("bin").join(adapter.executable()),
                };
                script(&old, b"#!/bin/sh\nprintf 'old package\\n'\n");
                for native in adapter.preferred_executables(&installed) {
                    script(&native, b"#!/bin/sh\nprintf 'old native\\n'\n");
                }
                let mut output = Vec::new();
                install::tool(
                    install::ToolInstallOptions {
                        name: adapter.id().into(),
                        source: Some(root.join("fixture-source").to_string_lossy().into_owned()),
                        dir: None,
                        dry_run: false,
                    },
                    &mut output,
                )
                .unwrap();
                let selected = installed
                    .join("bin")
                    .join(adapter.executable())
                    .canonicalize()
                    .unwrap();
                assert!(
                    install::tool_available(adapter.id(), &tools_root),
                    "{}",
                    adapter.id()
                );
                assert!(install::tool_installed_locally(adapter.id(), &tools_root));
                let resolved = agents::resolve(adapter.id(), &root, TOOLS).unwrap();
                assert_eq!(Path::new(&resolved), selected);
                let result = adapter
                    .process(&resolved)
                    .unwrap()
                    .direct()
                    .output()
                    .unwrap();
                assert!(result.status.success(), "{}", adapter.id());
                assert_eq!(
                    result.stdout,
                    b"explicit source override\n",
                    "{}",
                    adapter.id()
                );
                assert!(
                    String::from_utf8(output)
                        .unwrap()
                        .contains("Installed tool")
                );
            }
            for tool in tools::all() {
                let name = tool.executable().unwrap();
                install::tool(
                    install::ToolInstallOptions {
                        name: tool.id().into(),
                        source: Some(root.join("fixture-source").to_string_lossy().into_owned()),
                        dir: None,
                        dry_run: false,
                    },
                    &mut Vec::new(),
                )
                .unwrap();
                assert!(
                    install::tool_available(tool.id(), &tools_root),
                    "{}",
                    tool.id()
                );
                assert!(install::tool_installed_locally(tool.id(), &tools_root));
                let resolved = tools::resolve(tool.id(), &root, TOOLS).unwrap();
                assert_eq!(
                    Path::new(&resolved),
                    tools_root
                        .join(tool.id())
                        .join("bin")
                        .join(name)
                        .canonicalize()
                        .unwrap()
                );
                assert_eq!(
                    Command::new(resolved).output().unwrap().stdout,
                    b"explicit source override\n"
                );
            }
        }
        "broken" => {
            for adapter in agents::all() {
                let installation = tools_root.join(adapter.id());
                script(
                    &installation
                        .join("node_modules/.bin")
                        .join(adapter.executable()),
                    CONTENT,
                );
                script(
                    &installation.join("venv/bin").join(adapter.executable()),
                    CONTENT,
                );
                for native in adapter.preferred_executables(&installation) {
                    script(&native, CONTENT);
                }
                let primary = installation.join("bin").join(adapter.executable());
                script(&primary, CONTENT);
                fs::set_permissions(&primary, fs::Permissions::from_mode(0o600)).unwrap();
                assert!(!install::tool_available(adapter.id(), &tools_root));
                assert!(!install::tool_installed_locally(adapter.id(), &tools_root));
                assert!(agents::resolve(adapter.id(), &root, TOOLS).is_none());
                fs::remove_file(&primary).unwrap();
                symlink(root.join("missing"), &primary).unwrap();
                assert!(!install::tool_available(adapter.id(), &tools_root));
                assert!(agents::resolve(adapter.id(), &root, TOOLS).is_none());
                fs::remove_file(&primary).unwrap();
                symlink(root.join("fixture-source"), &primary).unwrap();
                fs::set_permissions(
                    root.join("fixture-source"),
                    fs::Permissions::from_mode(0o755),
                )
                .unwrap();
                assert!(install::tool_available(adapter.id(), &tools_root));
                assert_eq!(
                    agents::resolve(adapter.id(), &root, TOOLS).unwrap(),
                    root.join("fixture-source").to_string_lossy()
                );
            }
            for tool in tools::all() {
                let primary = tools_root
                    .join(tool.id())
                    .join("bin")
                    .join(tool.executable().unwrap());
                script(&primary, CONTENT);
                fs::set_permissions(&primary, fs::Permissions::from_mode(0o600)).unwrap();
                assert!(!install::tool_available(tool.id(), &tools_root));
                assert!(tools::resolve(tool.id(), &root, TOOLS).is_none());
            }
        }
        "external" => {
            let external = root.join("external/codex");
            script(&external, CONTENT);
            assert_eq!(
                agents::resolve("codex", &root, TOOLS).unwrap(),
                external.to_string_lossy()
            );
            assert!(!install::tool_available("codex", &tools_root));
            assert!(!install::tool_installed_locally("codex", &tools_root));
            let local = tools_root.join("codex/bin/codex");
            script(&local, CONTENT);
            assert!(install::tool_installed_locally("codex", &tools_root));
            assert_eq!(
                agents::resolve("codex", &root, TOOLS).unwrap(),
                external.to_string_lossy()
            );
            fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(agents::resolve("codex", &root, TOOLS).is_none());
            assert!(install::tool_available("codex", &tools_root));
            fs::remove_file(&external).unwrap();
            assert_eq!(
                agents::resolve("codex", &root, TOOLS).unwrap(),
                local.to_string_lossy()
            );
            symlink(root.join("missing"), &external).unwrap();
            assert!(agents::resolve("codex", &root, TOOLS).is_none());
        }
        "legacy" => {
            assert!(agents::resolve("codex", &root, TOOLS).is_none());
            assert!(!install::tool_available("codex", &tools_root));
            let local = tools_root.join("bin/codex");
            script(&local, CONTENT);
            assert_eq!(
                agents::resolve("codex", &root, TOOLS).unwrap(),
                local.to_string_lossy()
            );
            assert!(install::tool_installed_locally("codex", &tools_root));
            let perf = tools_root.join("bin/perf");
            script(&perf, CONTENT);
            assert_eq!(
                tools::resolve("perf", &root, TOOLS).unwrap(),
                perf.to_string_lossy()
            );
            assert!(install::tool_installed_locally("perf", &tools_root));
            // Absolute/custom directory values use the same project policy.
            assert_eq!(
                agents::resolve("codex", &root, tools_root.to_str().unwrap()).unwrap(),
                local.to_string_lossy()
            );
        }
        "host" => {
            let expected = root.join("host-second/mpstat");
            assert_eq!(
                tools::host_executable(Path::new("mpstat")).unwrap(),
                expected
            );
            assert_eq!(install::host_executable("mpstat").unwrap(), expected);
            assert!(install::tool_available("mpstat", &tools_root));
            fs::remove_file(root.join("host-second/pidstat")).unwrap();
            assert!(!install::tool_available("mpstat", &tools_root));
            assert!(!install::tool_installed_locally("mpstat", &tools_root));
            assert!(tools::resolve("mpstat", &root, TOOLS).is_none());
            assert!(!install::tool_available("opencode", &tools_root));
            assert!(agents::resolve("opencode", &root, TOOLS).is_none());
        }
        other => panic!("unknown fixture case {other}"),
    }
}
