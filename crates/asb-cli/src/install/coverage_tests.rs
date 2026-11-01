//! Offline installation contracts. Package managers are fixture executables;
//! no download, provider, host package installation or credential is used.
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

const CHILD_CASE: &str = "ASB_INSTALL_FIXTURE_CASE";

struct Fixture(PathBuf, std::cell::Cell<bool>);

impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/install-fixtures")
            .join(tools::runtime::unique_id());
        fs::create_dir_all(root.join(".asb")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        Self(root.canonicalize().unwrap(), std::cell::Cell::new(true))
    }

    fn script(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join("bin").join(name);
        crate::write_test_file(&self.0, &path, format!("#!/bin/sh\nset -eu\n{contents}\n"))
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn owned_step(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        label: &str,
        configure: impl FnOnce(&mut Command),
    ) -> process::CompletedStep {
        self.1.set(false);
        let (_, mut config) = project::load(&self.0).unwrap();
        config.installation.operation_timeout_seconds = 20;
        let context = OperationContext::new(&self.0, &config).unwrap();
        struct Cancel(OperationContext);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = Cancel(context.clone());
        let owner = ProducerDirectory::open(&self.0).unwrap();
        let mut scope = owner.scope(&context);
        let mut step = scope.prepare(program, Purpose::Probe, label).unwrap();
        step.capture(process::Capture::Inspect {
            stream_bytes: 1024 * 1024,
            total_bytes: 2 * 1024 * 1024,
        });
        configure(step.command_mut());
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = scope.run(step).and_then(|output| {
                owner.consume(scope.finish()?)?;
                Ok(output)
            });
            let _ = send.send(result);
        });
        let output = receive
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|error| {
                panic!(
                    "{label}: owned fixture remains uncertain: {error}; retained {}",
                    self.0.display()
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "{label}: fixture did not settle: {error}; retained {}",
                    self.0.display()
                )
            });
        // Actual same-owner proof is consumed before interpreting child status.
        assert!(
            output.status.success(),
            "{label}: {}\n{}; retained {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.0.display()
        );
        self.1.set(true);
        output
    }

    fn run(&self, case: &str) {
        let output = self.owned_step(std::env::current_exe().unwrap(), case, |command| {
            command
                .env_clear()
                .env(CHILD_CASE, case)
                .env("PATH", self.0.join("bin"))
                .env("HOME", &self.0)
                .env("ASB_FIXTURE_ROOT", &self.0)
                .current_dir(&self.0)
                .args([
                    "--exact",
                    "install::coverage_tests::installation_fixture_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .stdin(Stdio::null());
            // Child processes must retain instrumentation, never inherited keys.
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
        });
        assert!(
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")
            }),
            "{case}: exact child did not execute its one fixture test; retained {}",
            self.0.display()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.1.get() && !std::thread::panicking() {
            crate::fixture_writer::cleanup(&self.0).unwrap();
        }
    }
}

#[test]
fn host_dependency_publication_is_no_clobber_and_removes_owned_temporaries() {
    let fixture = Fixture::new();
    fixture.script("fixture-host-tool", "printf 'fixture host tool\\n'");
    fixture.run("host");
}

#[test]
fn host_recipe_replacement_never_follows_destination_symlink() {
    let fixture = Fixture::new();
    fixture.script("fixture-host-tool", "printf 'fixture host tool\\n'");
    fixture.run("host-replacement");
}

#[test]
fn npm_recipe_uses_local_cache_manifest_and_reports_installer_failures() {
    let fixture = Fixture::new();
    fixture.script("node", "printf 'v22.20.0\\n'");
    fixture.script(
        "npm",
        r#"test "$1" = install
test "$2" = --prefix
test "$4" = --no-save
test "$NPM_CONFIG_CACHE" = "$ASB_FIXTURE_ROOT/.asb-cache/npm"
printf '%s\n' "$5" >> "$ASB_FIXTURE_ROOT/npm-invocations"
case "$5" in
 fixture-success@1) /bin/mkdir -p "$3/node_modules/.bin"; printf '#!/bin/sh\nexit 0\n' > "$3/node_modules/.bin/fixture-client"; /bin/chmod 755 "$3/node_modules/.bin/fixture-client" ;;
 fixture-missing-executable@1) /bin/mkdir -p "$3/node_modules/.bin" ;;
 fixture-fail@1) exit 41 ;;
 fixture-exit127@1) printf started > "$ASB_FIXTURE_ROOT/npm127-started"; exit 127 ;;
 *) exit 99 ;;
esac"#,
    );
    fixture.run("npm");
}

#[test]
fn npm_recipe_uses_project_local_only_runtime_path() {
    let fixture = Fixture::new();
    fixture.script("node", "printf 'v22.20.0\\n'");
    let npm = fixture.0.join("tools/node/bin/npm");
    fs::create_dir_all(npm.parent().unwrap()).unwrap();
    crate::write_test_file(
        &fixture.0,
        &npm,
        b"#!/bin/sh\nset -eu\nprintf local > \"$ASB_FIXTURE_ROOT/local-npm-started\"\n/bin/mkdir -p \"$3/node_modules/.bin\"\nprintf '#!/bin/sh\\nexit 0\\n' > \"$3/node_modules/.bin/fixture-client\"\n/bin/chmod 755 \"$3/node_modules/.bin/fixture-client\"\n",
    )
    .unwrap();
    fs::set_permissions(npm, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.run("npm-local");
}

#[test]
fn python_recipe_isolated_venv_dependencies_and_failure_diagnostics() {
    let fixture = Fixture::new();
    let python = fixture.script(
        "fixture-python",
        r#"if test "$1" = -I; then printf '3.12\n'; exit 0; fi
test "$1" = -m
if test "$2" = venv; then
 case "$3" in *venv-create-fail*) exit 22 ;; esac
 /bin/mkdir -p "$3/bin"
 /bin/cp "$0" "$3/bin/python"
 exit 0
fi
test "$2" = pip; test "$3" = install
printf '%s\n' "$4" >> "$ASB_FIXTURE_ROOT/pip-invocations"
case "$4" in fixture-package-fail|fixture-dependency-fail) exit 23 ;; esac
printf '#!/bin/sh\nexit 0\n' > "$(/usr/bin/dirname "$0")/fixture-client"
/bin/chmod 755 "$(/usr/bin/dirname "$0")/fixture-client""#,
    );
    let project_python = fixture.0.join(".asb/python/venv/bin/python");
    fs::create_dir_all(project_python.parent().unwrap()).unwrap();
    fs::copy(python, project_python).unwrap();
    fixture.run("python");
}

#[test]
fn local_bundle_extract_reuse_and_corrupt_installation_are_distinguished() {
    let fixture = Fixture::new();
    symlink("/usr/bin/tar", fixture.0.join("bin/tar")).unwrap();
    // GNU tar invokes gzip by name; only this explicit local program is exposed.
    symlink("/usr/bin/gzip", fixture.0.join("bin/gzip")).unwrap();
    fs::create_dir(fixture.0.join("upstream-fixture")).unwrap();
    fs::write(
        fixture.0.join("upstream-fixture/README.md"),
        "retained upstream fixture source",
    )
    .unwrap();
    fixture.owned_step("/usr/bin/tar", "create local bundle fixture", |command| {
        command
            .args(["-czf"])
            .arg(fixture.0.join("fixture.tar.gz"))
            .arg("-C")
            .arg(&fixture.0)
            .arg("upstream-fixture")
            .stdin(Stdio::null());
    });
    fixture.run("bundle");
}

#[test]
fn source_checksum_and_command_failures_are_actionable_and_preserve_evidence() {
    let fixture = Fixture::new();
    symlink("/usr/bin/sha256sum", fixture.0.join("bin/sha256sum")).unwrap();
    fixture.run("diagnostics");
}

#[test]
fn downloader_protocol_and_remote_bounds_use_an_offline_executable_fixture() {
    let fixture = Fixture::new();
    fixture.script(
        "curl",
        r#"destination=''; previous=''; bounded=0; source=''
for value in "$@"; do
 if test "$previous" = --output; then destination="$value"; fi
 if test "$value" = --max-filesize; then bounded=1; fi
 previous="$value"; source="$value"
done
test -n "$destination"
printf '%s %s\n' "$bounded" "$source" >> "$ASB_FIXTURE_ROOT/download-invocations"
case "$source" in
 https://fixture.invalid/success) printf 'fixture data' > "$destination" ;;
 https://fixture.invalid/oversize) test "$bounded" = 1; printf 'too large' > "$destination" ;;
 https://fixture.invalid/failure|'https://fixture-user:fixture-password@fixture.invalid/failure?token=fixture-query-secret') exit 22 ;;
 *) exit 99 ;;
esac"#,
    );
    fixture.run("download");
}

#[test]
fn guest_bootstrap_failures_leave_host_runtime_untouched() {
    let fixture = Fixture::new();
    fixture.run("guest-bootstrap");
}

#[test]
fn installation_fixture_child() {
    let Ok(case) = std::env::var(CHILD_CASE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("ASB_FIXTURE_ROOT").unwrap());
    match case.as_str() {
        "host-replacement" => {
            let tool_root = root.join("tools/fixture-host-tool");
            let destination = tool_root.join("bin/fixture-host-tool");
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            let foreign = root.join("foreign-executable");
            fs::write(&foreign, b"keep foreign bytes and permissions").unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o640)).unwrap();
            symlink(&foreign, &destination).unwrap();
            install_recipe(
                Recipe::Host {
                    executable: "fixture-host-tool",
                },
                &tool_root,
                &root,
                &mut Vec::new(),
                &mut |_, _| {},
            )
            .unwrap();
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"keep foreign bytes and permissions"
            );
            assert_eq!(
                fs::metadata(&foreign).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert!(
                !fs::symlink_metadata(&destination)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read(destination).unwrap(),
                fs::read(root.join("bin/fixture-host-tool")).unwrap()
            );
        }
        "host" => host_contract(&root),
        "npm" => npm_contract(&root),
        "npm-local" => {
            assert!(!root.join("bin/npm").exists());
            install_recipe(
                Recipe::Npm {
                    package: "fixture-success@1",
                    executable: "fixture-client",
                    manifest: None,
                },
                &root.join("tools/npm-local"),
                &root,
                &mut Vec::new(),
                &mut |_, _| {},
            )
            .unwrap();
            assert_eq!(fs::read(root.join("local-npm-started")).unwrap(), b"local");
            assert!(crate::executables::is_executable(
                &root.join("tools/npm-local/node_modules/.bin/fixture-client")
            ));
        }
        "python" => python_contract(&root),
        "bundle" => bundle_contract(&root),
        "diagnostics" => diagnostics_contract(&root),
        "download" => download_contract(&root),
        "guest-bootstrap" => guest_bootstrap_contract(&root),
        "workload-errors" => workload_error_contract(&root),
        "host-recipe" => host_recipe_contract(&root),
        other => panic!("unknown fixture case {other}"),
    }
}

#[test]
fn custom_workload_installation_preserves_prompts_and_names_failed_paths() {
    Fixture::new().run("workload-errors");
}

fn workload_error_contract(root: &Path) {
    let source = root.join("custom.md");
    fs::write(&source, "fixture workload prompt").unwrap();
    let directory = root.join("custom-workloads");
    let options = |name: &str| WorkloadInstallOptions {
        source: source.to_string_lossy().into_owned(),
        name: Some(name.into()),
        dir: Some(directory.clone()),
    };
    let mut output = Vec::new();
    workload(options("custom"), &mut output).unwrap();
    assert_eq!(
        fs::read_to_string(directory.join("custom.txt")).unwrap(),
        "fixture workload prompt"
    );
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Installed workload custom")
    );
    fs::write(directory.join("custom.txt"), "preserved user prompt").unwrap();
    workload(options("custom"), &mut Vec::new()).unwrap();
    assert_eq!(
        fs::read_to_string(directory.join("custom.txt")).unwrap(),
        "preserved user prompt"
    );
    fs::create_dir(directory.join("blocked.txt")).unwrap();
    let error = workload(options("blocked"), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot create workload prompt"), "{error}");
    assert!(error.contains("blocked.txt"));
    let error = workload(options("bad name"), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("workload name"), "{error}");
    let error = workload(options("all"), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("reserved"), "{error}");
    let error = workload_source("https://fixture.invalid/a%20b.md")
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("simple file name"), "{error}");
    fs::write(root.join("blocked-parent"), "not a directory").unwrap();
    let error = workload(
        WorkloadInstallOptions {
            source: source.to_string_lossy().into_owned(),
            name: Some("custom".into()),
            dir: Some(root.join("blocked-parent/nested")),
        },
        &mut Vec::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("cannot create") && error.contains("blocked-parent"),
        "{error}"
    );
}

#[test]
fn host_recipe_materializes_only_its_project_destination() {
    let fixture = Fixture::new();
    symlink("/bin/true", fixture.0.join("bin/fixture-host")).unwrap();
    fixture.run("host-recipe");
}

fn host_recipe_contract(root: &Path) {
    let destination = root.join("tools/fixture-host");
    let mut stages = Vec::new();
    let path = install_recipe(
        Recipe::Host {
            executable: "fixture-host",
        },
        &destination,
        root,
        &mut Vec::new(),
        &mut |percent, stage| stages.push((percent, stage.to_owned())),
    )
    .unwrap();
    assert_eq!(path, destination.join("bin/fixture-host"));
    assert_eq!(fs::read(&path).unwrap(), fs::read("/bin/true").unwrap());
    assert!(crate::executables::is_executable(&path));
    assert!(stages.iter().any(|(percent, _)| *percent == 55));
    let blocked = root.join("tools/blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(blocked.join("bin"), "block destination").unwrap();
    let error = install_recipe(
        Recipe::Host {
            executable: "fixture-host",
        },
        &blocked,
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cannot create"), "{error}");
    let missing = install_recipe(
        Recipe::Host {
            executable: "missing-fixture",
        },
        &root.join("tools/missing"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err()
    .to_string();
    assert!(missing.contains("missing-fixture"), "{missing}");
}

fn guest_bootstrap_contract(root: &Path) {
    let tools = root.join("tools");
    let unknown = guest_bootstrap("fixture-unregistered-agent", root, &tools)
        .err()
        .expect("unregistered agent must be rejected");
    assert!(unknown.to_string().contains("no installation adapter"));
    assert!(!tools.exists());
    let host_node = tools.join("node/bin/node");
    fs::create_dir_all(host_node.parent().unwrap()).unwrap();
    fs::write(&host_node, "preserved host runtime").unwrap();
    // curl is absent from the child PATH: prefetch must stop, not launch an
    // agent or substitute a host executable for its declared distribution.
    let error = guest_bootstrap("codex", root, &tools)
        .err()
        .expect("missing prefetch transport must fail");
    assert!(error.to_string().contains("curl"), "{error}");
    assert_eq!(fs::read(host_node).unwrap(), b"preserved host runtime");
    assert!(tools.join(".asb-guest-bootstrap.lock").is_file());
    let blocked = root.join("blocked-tools");
    fs::write(&blocked, "preserved").unwrap();
    assert!(guest_bootstrap("codex", root, &blocked).is_err());
    assert_eq!(fs::read(&blocked).unwrap(), b"preserved");
    let error = ensure_runtime_dependency(
        "fixture-unknown-runtime",
        root,
        &tools,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("runtime dependency `fixture-unknown-runtime` is not registered")
    );
}

fn download_contract(root: &Path) {
    let destination = root.join("downloaded");
    install_source(
        root,
        "https://fixture.invalid/success",
        &destination,
        "fixture",
    )
    .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"fixture data");
    let error = install_source(
        root,
        "https://fixture.invalid/failure",
        &destination,
        "fixture",
    )
    .unwrap_err();
    assert_eq!(fs::read(&destination).unwrap(), b"fixture data");
    let error = error.to_string();
    assert!(error.contains("could not download fixture"), "{error}");
    assert!(
        error.contains("curl exited with exit status: 22"),
        "{error}"
    );
    assert!(
        error.contains("previous installation was preserved"),
        "{error}"
    );
    let (_, log) = error
        .split_once("; inspect ")
        .expect("owned diagnostic path");
    assert!(Path::new(log).is_file(), "missing diagnostic log: {log}");
    let private_source =
        "https://fixture-user:fixture-password@fixture.invalid/failure?token=fixture-query-secret";
    let private_error = install_source(root, private_source, &destination, "fixture")
        .unwrap_err()
        .to_string();
    assert_eq!(fs::read(&destination).unwrap(), b"fixture data");
    assert!(private_error.contains("curl exited with exit status: 22"));
    for private in [
        "fixture-user",
        "fixture-password",
        "fixture-query-secret",
        "token=",
    ] {
        assert!(
            !private_error.contains(private),
            "source detail leaked: {private_error}"
        );
    }
    let error = install_source_with_limit(
        root,
        "https://fixture.invalid/oversize",
        &root.join("oversize"),
        "fixture",
        Some(3),
    )
    .unwrap_err();
    assert!(error.to_string().contains("installation size limit"));
    assert_eq!(
        fs::read_to_string(root.join("download-invocations")).unwrap(),
        format!(
            "0 https://fixture.invalid/success\n1 https://fixture.invalid/success\n1 https://fixture.invalid/success\n0 https://fixture.invalid/failure\n1 https://fixture.invalid/failure\n1 https://fixture.invalid/failure\n0 {private_source}\n1 {private_source}\n1 {private_source}\n0 https://fixture.invalid/oversize\n1 https://fixture.invalid/oversize\n1 https://fixture.invalid/oversize\n"
        )
    );
    fs::remove_file(root.join("bin/curl")).unwrap();
    install_source(
        root,
        "https://fixture.invalid/success",
        &destination,
        "fixture",
    )
    .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"fixture data");
}

fn host_contract(root: &Path) {
    let destination = root.join("tools/host/bin/tool");
    assert_eq!(
        install_host_dependency("fixture-host-tool", &destination).unwrap(),
        destination
    );
    assert!(crate::executables::is_executable(&destination));
    let original = fs::read(&destination).unwrap();
    fs::write(root.join("bin/fixture-host-tool"), "replacement").unwrap();
    install_host_dependency("fixture-host-tool", &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), original);
    assert_eq!(
        fs::read_dir(destination.parent().unwrap()).unwrap().count(),
        1
    );
    let blocked = root.join("tools/host/bin/directory");
    fs::create_dir(&blocked).unwrap();
    assert!(
        install_host_dependency("fixture-host-tool", &blocked)
            .unwrap_err()
            .to_string()
            .contains("cannot install fixture-host-tool")
    );
    assert_eq!(
        fs::read_dir(destination.parent().unwrap()).unwrap().count(),
        2
    );
    assert!(
        host_executable("fixture-deliberately-absent")
            .unwrap_err()
            .to_string()
            .contains("asb tool install fixture-deliberately-absent")
    );
    let recipe = Recipe::Host {
        executable: "fixture-host-tool",
    };
    let installed = install_recipe(
        recipe,
        &root.join("tools/explicit"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(crate::executables::is_executable(&installed));
    assert_eq!(fs::read(installed).unwrap(), b"replacement");
}

fn npm_contract(root: &Path) {
    let package = |package| Recipe::Npm {
        package,
        executable: "fixture-client",
        manifest: None,
    };
    let directory = root.join("tools/npm-success");
    let mut progress = Vec::new();
    let executable = install_recipe(
        package("fixture-success@1"),
        &directory,
        root,
        &mut Vec::new(),
        &mut |percent, stage| progress.push((percent, stage.to_owned())),
    )
    .unwrap();
    assert_eq!(
        executable,
        directory.join("node_modules/.bin/fixture-client")
    );
    assert!(executable.is_file());
    assert!(!directory.join("package.json").exists());
    assert!(progress.iter().any(|(percent, stage)| *percent == 15
        && *stage == crate::install::ProgressStage::InstallingPackage));
    // Existing user manifests must never be replaced by an adapter manifest.
    fs::write(
        directory.join("package.json"),
        "{\"private\":true,\"name\":\"preserved\"}",
    )
    .unwrap();
    install_recipe(
        package("fixture-success@1"),
        &directory,
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(
        fs::read_to_string(directory.join("package.json"))
            .unwrap()
            .contains("preserved")
    );
    let absent = install_recipe(
        package("fixture-missing-executable@1"),
        &root.join("tools/npm-missing-executable"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(
        absent.to_string().contains("required executable"),
        "missing package executable must not be reported as a successful installation: {absent}"
    );
    let failure = install_recipe(
        package("fixture-fail@1"),
        &root.join("tools/npm-fail"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("agent package installation failed")
    );
    let exit127 = install_recipe(
        package("fixture-exit127@1"),
        &root.join("tools/npm-exit127"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert_eq!(fs::read(root.join("npm127-started")).unwrap(), b"started");
    let exit127 = exit127.to_string();
    assert!(exit127.contains("exit status: 127"), "{exit127}");
    assert!(!exit127.contains("needs `npm`"), "{exit127}");
    assert!(!exit127.contains("fnm install --lts"), "{exit127}");
    fs::remove_file(root.join("bin/npm")).unwrap();
    let missing = install_recipe(
        package("fixture-success@1"),
        &root.join("tools/npm-missing"),
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert_eq!(
        fs::read_to_string(root.join("npm-invocations"))
            .unwrap()
            .lines()
            .count(),
        5
    );
    assert!(
        missing.to_string().contains("npm"),
        "actual missing npm failure: {missing}"
    );
    assert!(
        missing.to_string().contains("fnm install --lts"),
        "actual missing npm repair: {missing}"
    );
}

fn python_contract(root: &Path) {
    let recipe = |package, dependencies| Recipe::Python {
        package,
        dependencies,
        executable: "fixture-client",
    };
    let directory = root.join("tools/python-success");
    let mut progress = Vec::new();
    let executable = install_recipe(
        recipe("fixture-package", &["fixture-dependency"]),
        &directory,
        root,
        &mut Vec::new(),
        &mut |percent, stage| progress.push((percent, stage.to_owned())),
    )
    .unwrap();
    assert_eq!(executable, directory.join("venv/bin/fixture-client"));
    assert!(progress.iter().any(|(percent, _)| *percent == 35));
    assert!(progress.iter().any(|(percent, _)| *percent == 45));
    assert_eq!(
        fs::read_to_string(root.join("pip-invocations")).unwrap(),
        "fixture-package\nfixture-dependency\n"
    );
    // Compatible environments are reused without manufacturing backups.
    install_recipe(
        recipe("fixture-package", &[]),
        &directory,
        root,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(!directory.join("venv.asb-backup-1").exists());
    fs::write(
        directory.join("venv/bin/python"),
        "#!/bin/sh\nprintf '3.14\\n'\n",
    )
    .unwrap();
    fs::write(directory.join("venv/manual-work"), "preserved").unwrap();
    let mut output = Vec::new();
    install_recipe(
        recipe("fixture-package", &[]),
        &directory,
        root,
        &mut output,
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(
        fs::read(directory.join("venv.asb-backup-1/venv/manual-work")).unwrap(),
        b"preserved"
    );
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Preserved the previous tool Python environment")
    );
    for (name, package, dependencies, message) in [
        (
            "venv-create-fail",
            "fixture-package",
            &[][..],
            "could not create the agent Python environment",
        ),
        (
            "python-package-fail",
            "fixture-package-fail",
            &[][..],
            "Python package installation failed for fixture-package-fail",
        ),
        (
            "python-dependency-fail",
            "fixture-package",
            &["fixture-dependency-fail"][..],
            "Python runtime dependency installation failed for fixture-dependency-fail",
        ),
    ] {
        let error = install_recipe(
            recipe(package, dependencies),
            &root.join("tools").join(name),
            root,
            &mut Vec::new(),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
    assert_eq!(
        recipe("fixture-package", &[])
            .runtime_inputs(&directory)
            .len(),
        1
    );
}

fn bundle_contract(root: &Path) {
    let directory = root.join("workloads");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join(".fixture-external.extract")).unwrap();
    fs::write(directory.join(".fixture-external.extract/stale"), "stale").unwrap();
    let source: &'static str = Box::leak(
        root.join("fixture.tar.gz")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );
    let spec = || workloads::SourceBundle {
        archive: source,
        marker: "fixture-revision",
        prompt: "fixture task prompt",
    };
    let error = install_bundle(
        "fixture-external",
        spec(),
        &directory,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("was preserved"));
    assert_eq!(
        fs::read(directory.join(".fixture-external.extract/stale")).unwrap(),
        b"stale"
    );
    fs::remove_dir_all(directory.join(".fixture-external.extract")).unwrap();
    let mut output = Vec::new();
    let mut progress = Vec::new();
    install_bundle(
        "fixture-external",
        spec(),
        &directory,
        &mut output,
        &mut |percent, stage| progress.push((percent, stage.to_owned())),
    )
    .unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Installed external workload fixture-external")
    );
    assert_eq!(
        fs::read(directory.join("fixture-external/source/README.md")).unwrap(),
        b"retained upstream fixture source"
    );
    let generated_prompt = fs::read_to_string(directory.join("fixture-external.txt")).unwrap();
    assert_eq!(generated_prompt, "fixture task prompt");
    assert_eq!(progress.last().unwrap().0, 100);
    assert!(!directory.join(".fixture-external.extract").exists());
    assert!(!directory.join(".fixture-external.tar.gz").exists());
    // Reuse restores a missing generated prompt, not modified source files.
    fs::remove_file(directory.join("fixture-external.txt")).unwrap();
    install_bundle(
        "fixture-external",
        spec(),
        &directory,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(directory.join("fixture-external.txt").is_file());
    fs::write(
        directory.join("fixture-external/ASB-BUNDLE"),
        "wrong-revision",
    )
    .unwrap();
    let error = install_bundle(
        "fixture-external",
        spec(),
        &directory,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("different pinned revision"));
    assert!(
        directory
            .join("fixture-external/source/README.md")
            .is_file()
    );
    fs::create_dir(directory.join("incomplete")).unwrap();
    let error = install_bundle(
        "incomplete",
        spec(),
        &directory,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("incomplete local bundle"));
    let mut listed = Vec::new();
    workload_list(
        WorkloadListOptions {
            dir: Some(directory),
        },
        &mut listed,
    )
    .unwrap();
    assert!(
        String::from_utf8(listed)
            .unwrap()
            .contains("fixture-external [trial set]")
    );
}

fn diagnostics_contract(root: &Path) {
    let source = root.join("source");
    fs::write(&source, "hello").unwrap();
    let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    verify_sha256_preserving(&source, expected, "fixture").unwrap();
    let error = verify_sha256_preserving(&source, "wrong", "fixture").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("checksum mismatch: expected wrong")
    );
    assert_eq!(fs::read(&source).unwrap(), b"hello");
    let context = OperationContext::load(root).unwrap();
    let owner = ProducerDirectory::open(root).unwrap();
    let mut scope = owner.scope(&context);
    for (args, expected) in [
        (["-c", "exit 7"], "command exited"),
        (["-c", "exit 0"], "command returned no output"),
    ] {
        assert!(
            command_output_in(&mut scope, Path::new("/bin/sh"), &args, "fixture")
                .unwrap_err()
                .into_public()
                .to_string()
                .contains(expected)
        );
    }
    owner.consume(scope.finish().unwrap()).unwrap();
    let destination = root.join("destination");
    let error = install_source(root, root.to_str().unwrap(), &destination, "fixture").unwrap_err();
    assert!(error.to_string().contains("source is not a file"));
    fs::write(&source, "fixture").unwrap();
    fs::create_dir(&destination).unwrap();
    let error =
        install_source(root, source.to_str().unwrap(), &destination, "fixture").unwrap_err();
    assert!(error.to_string().contains("cannot install fixture at"));
    assert!(make_executable(&root.join("missing")).is_err());
    assert_eq!(json_string("quoted\"\nvalue"), "\"quoted\\\"\\nvalue\"");
    let mut output = Vec::new();
    tool(
        ToolInstallOptions {
            name: "opencode".into(),
            source: None,
            dir: None,
            dry_run: true,
        },
        &mut output,
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&output).contains("npm install --prefix"));
    let error = tool(
        ToolInstallOptions {
            name: "../bad".into(),
            source: None,
            dir: None,
            dry_run: false,
        },
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid tool name"));
    let error = workload_source("terminal-benc").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("did you mean external workload `terminal-bench-1.0`")
    );
    let directory = root.join("empty-workloads");
    fs::create_dir(&directory).unwrap();
    workload_list(
        WorkloadListOptions {
            dir: Some(directory),
        },
        &mut output,
    )
    .unwrap();
    assert!(String::from_utf8(output).unwrap().contains("(none)"));
}
