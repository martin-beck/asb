//! Fake curl never opens a socket. All content, faults and delays are local.
use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::{Duration, Instant};

const CHILD: &str = "ASB_SOURCE_PUBLICATION_CHILD";
const OLD: &[u8] = b"#!/bin/sh\nprintf 'old tool\\n'\n";
const NEW: &[u8] = b"#!/bin/sh\nprintf 'new tool\\n'\n";

pub(super) struct Fixture(pub(super) PathBuf, std::cell::Cell<bool>);
impl Fixture {
    pub(super) fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/publication-fixtures")
            .join(tools::runtime::unique_id());
        fs::create_dir_all(path.join("bin")).unwrap();
        fs::create_dir(path.join(".asb")).unwrap();
        fs::write(
            path.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        let fixture = Self(path.canonicalize().unwrap(), std::cell::Cell::new(true));
        symlink("/usr/bin/sha256sum", fixture.0.join("bin/sha256sum")).unwrap();
        fixture.write_executable("tools/fixture/bin/fixture", OLD);
        fixture.write_executable("tools/perf/bin/perf", OLD);
        fs::write(fixture.0.join("new-source"), NEW).unwrap();
        fixture.write_executable("bin/curl", br##"#!/bin/sh
set -eu
test "$1" = --disable
destination=''; previous=''; source=''; bounded=0; timed=0
for value in "$@"; do
 if test "$previous" = --output; then destination="$value"; fi
 if test "$previous" = --max-filesize; then test "$value" -gt 0; bounded=1; fi
 # The owning operation enforces the frozen overall download deadline; curl's
 # connect timeout remains explicit without a second fixed --max-time cap.
 if test "$previous" = --connect-timeout; then test "$value" = 10; timed=1; fi
 previous="$value"; source="$value"
done
test "$bounded" = 1; test "$timed" = 1; test -f "$destination"
printf '%s\n' "$destination" > "$ASB_FIXTURE_ROOT/download-path"
case "$source" in
 https://fixture.invalid/failure) printf 'truncated' > "$destination"; exit 22 ;;
 https://fixture.invalid/exit127) printf started > "$ASB_FIXTURE_ROOT/curl127-started"; exit 127 ;;
 https://fixture.invalid/oversize) printf 'much too large' > "$destination" ;;
 https://fixture.invalid/empty) : > "$destination" ;;
 https://fixture.invalid/symlink)
   /bin/ln -s "$ASB_FIXTURE_ROOT/foreign" "$destination.replacement"
   /bin/mv -Tf "$destination.replacement" "$destination" ;;
 https://fixture.invalid/timeout) printf '%s\n' "$$" > "$ASB_FIXTURE_ROOT/curl-pid"; exec /bin/sleep 60 ;;
 https://fixture.invalid/blocked)
   printf '#!/bin/sh\n' > "$destination"
   printf ready > "$ASB_FIXTURE_ROOT/download-ready"
   while ! test -f "$ASB_FIXTURE_ROOT/download-release"; do /bin/sleep 0.01; done
   /bin/cp "$ASB_FIXTURE_ROOT/new-source" "$destination" ;;
 https://fixture.invalid/success) /bin/cp "$ASB_FIXTURE_ROOT/new-source" "$destination" ;;
 *) exit 99 ;;
esac
"##);
        fixture
    }

    fn write_executable(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::write_test_file(&self.0, &path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_observed<T>(&self, case: &str, observe: impl FnOnce(&Path) -> T) -> T {
        // A failed/uncertain fixture must not recursively remove a namespace
        // that its actual producer may still own. Only matching consumed proof
        // and a successful exact child re-enable ordinary test cleanup.
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
        let mut step = scope
            .prepare(
                std::env::current_exe().unwrap(),
                Purpose::Probe,
                "exact source publication fixture",
            )
            .unwrap();
        step.capture(super::super::process::Capture::Inspect {
            stream_bytes: 1024 * 1024,
            total_bytes: 2 * 1024 * 1024,
        });
        step.command_mut()
            .env_clear()
            .current_dir(&self.0)
            .env("PATH", self.0.join("bin"))
            .env("HOME", &self.0)
            .env(CHILD, case)
            .env("ASB_FIXTURE_ROOT", &self.0)
            .args([
                "--exact",
                "install::source::publication_tests::publication_fixture_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(Stdio::null());
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            step.command_mut().env("LLVM_PROFILE_FILE", profile);
        }
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = scope.run(step).and_then(|output| {
                owner.consume(scope.finish()?)?;
                Ok(output)
            });
            let _ = send.send(result);
        });
        let observed = observe(&self.0);
        let output = receive
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|error| {
                panic!(
                    "{case}: owned source fixture remains uncertain: {error}; retained {}",
                    self.0.display()
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "{case}: source fixture did not settle: {error}; retained {}",
                    self.0.display()
                )
            });
        // Both scope.finish and owner.consume completed before status/output
        // interpretation. No direct-child exit substitutes for tree proof.
        assert!(
            output.status.success(),
            "{case}: {}\n{}; retained {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.0.display()
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")
            }),
            "{case}: exact child did not execute its one fixture test; retained {}",
            self.0.display()
        );
        self.1.set(true);
        observed
    }

    pub(super) fn run(&self, case: &str) {
        self.run_observed(case, |_| ());
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
fn failed_http_size_checksum_empty_and_probe_keep_the_old_ready_executable() {
    Fixture::new().run("failures");
}

#[test]
fn source_symlinks_and_rename_failure_never_mutate_foreign_destinations() {
    Fixture::new().run("symlinks");
}

#[test]
fn verified_shared_download_cache_is_reused_by_another_project() {
    Fixture::new().run("shared-cache");
}

#[test]
fn timeout_reaps_the_owned_downloader_and_retains_incomplete_raw() {
    Fixture::new().run("timeout");
}

#[test]
fn staged_version_probe_failure_and_timeout_preserve_the_ready_installation() {
    Fixture::new().run("probe");
}

#[test]
fn valid_exit127_and_postfailure_checks_do_not_invent_launch_failure() {
    Fixture::new().run("probe127");
}

#[test]
fn source_installation_publishes_complete_bytes_and_modes_only_after_download() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("tools/fixture/bin/fixture");
    struct Observation {
        ready: bool,
        old_bytes: Option<Vec<u8>>,
        old_mode: Option<u32>,
        old_ready: bool,
        staged: Option<PathBuf>,
        directory_mode: Option<u32>,
        file_mode: Option<u32>,
        released: bool,
    }
    let observed = fixture.run_observed("blocked", |root| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("download-ready").is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let staged = fs::read_to_string(root.join("download-path"))
            .ok()
            .map(|name| PathBuf::from(name.trim()));
        let mode = |path: &Path| {
            fs::metadata(path)
                .ok()
                .map(|metadata| metadata.permissions().mode() & 0o777)
        };
        let mut observed = Observation {
            ready: root.join("download-ready").is_file(),
            old_bytes: fs::read(&destination).ok(),
            old_mode: mode(&destination),
            old_ready: super::super::tool_installed_locally("perf", &root.join("tools")),
            directory_mode: staged.as_deref().and_then(Path::parent).and_then(mode),
            file_mode: staged.as_deref().and_then(mode),
            staged,
            released: false,
        };
        observed.released = fs::write(root.join("download-release"), "continue").is_ok();
        observed
    });
    // Every assertion follows actual matching child-tree settlement, including
    // failed readiness/metadata observations. There is no raw kill/wait path.
    assert!(observed.ready, "download never became ready");
    assert_eq!(observed.old_bytes.as_deref(), Some(OLD));
    assert_eq!(observed.old_mode, Some(0o755));
    assert!(observed.old_ready && observed.released);
    let staged = observed.staged.unwrap();
    assert_ne!(staged, destination);
    assert!(staged.starts_with(destination.parent().unwrap()));
    assert_eq!(observed.directory_mode, Some(0o700));
    assert_eq!(observed.file_mode, Some(0o600));
    assert_eq!(fs::read(&destination).unwrap(), NEW);
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!staged.parent().unwrap().exists());
}

#[test]
fn local_copy_and_tool_source_dispatch_use_the_same_atomic_publication() {
    Fixture::new().run("local");
}

#[test]
fn successful_probe_cannot_publish_bytes_changed_after_checksum_validation() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("tools/perf/bin/perf");
    let checksum = format!("{:x}", Sha256::digest(NEW));
    let result = install_fixture(
        &fixture.0,
        fixture.0.join("new-source").to_str().unwrap(),
        &destination,
        options("fixture", Some(&checksum), 1024),
        |staged, _| {
            // A successful external version probe can modify its own image.
            // Its success cannot authorize different, unchecked bytes.
            fs::write(staged, b"changed after checksum")
                .map_err(|error| crate::io_error(error).into())
        },
    );
    assert!(result.is_err(), "changed staged bytes were published");
    assert_old_ready(&fixture.0, &destination);
}

#[test]
fn successful_probe_cannot_publish_a_replacement_symlink() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("tools/perf/bin/perf");
    let foreign = fixture.0.join("foreign");
    fs::write(&foreign, b"foreign bytes").unwrap();
    let result = install_fixture(
        &fixture.0,
        fixture.0.join("new-source").to_str().unwrap(),
        &destination,
        options("fixture", None, 1024),
        |staged, _| {
            fs::remove_file(staged).map_err(crate::io_error)?;
            symlink(&foreign, staged).map_err(|error| crate::io_error(error).into())
        },
    );
    assert!(result.is_err(), "replacement symlink was published");
    assert_old_ready(&fixture.0, &destination);
    assert_eq!(fs::read(foreign).unwrap(), b"foreign bytes");
}

fn options<'a>(kind: &'a str, checksum: Option<&'a str>, limit: u64) -> Options<'a> {
    Options {
        kind,
        limit,
        checksum,
        executable: true,
    }
}

fn install_fixture(
    root: &Path,
    source: &str,
    destination: &Path,
    options: Options<'_>,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
) -> InstallResult<()> {
    install_fixture_with_budget(root, source, destination, options, validate, None)
}

fn install_fixture_with_budget(
    root: &Path,
    source: &str,
    destination: &Path,
    options: Options<'_>,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
    seconds: Option<u64>,
) -> InstallResult<()> {
    let (_, mut config) = project::load(root)?;
    if let Some(seconds) = seconds {
        config.installation.operation_timeout_seconds = seconds;
    }
    let context = OperationContext::new(root, &config)?;
    install_input_in(
        &context,
        &Input::from_os(source.as_ref()),
        destination,
        options,
        validate,
        Publication::Replace,
    )
}

pub(super) fn assert_old_ready(root: &Path, destination: &Path) {
    assert_eq!(fs::read(destination).unwrap(), OLD);
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(super::super::tool_installed_locally(
        "perf",
        &root.join("tools")
    ));
}

pub(super) fn no_scratch(destination: &Path) {
    assert!(scratch(destination).is_empty());
}

fn scratch(destination: &Path) -> std::collections::BTreeSet<PathBuf> {
    fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with(".asb-install-") && !name.starts_with(".asb-install-step-")
        })
        .collect()
}

#[test]
fn publication_fixture_child() {
    let Ok(case) = std::env::var(CHILD) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("ASB_FIXTURE_ROOT").unwrap());
    let destination = root.join("tools/perf/bin/perf");
    if case.starts_with("repair-") {
        super::repair_tests::child(&case, &root);
        return;
    }
    #[cfg(target_os = "linux")]
    if case.starts_with("lifecycle-") {
        super::lifecycle_tests::run_case(&root, &destination, &case);
        return;
    }
    match case.as_str() {
        "failures" => {
            for (source, limit, checksum, expected) in [
                ("failure", 1024, None, "curl exited"),
                ("exit127", 1024, None, "curl exited with exit status: 127"),
                ("oversize", 3, None, "size limit"),
                ("empty", 1024, None, "is empty"),
                ("success", 1024, Some("wrong"), "checksum mismatch"),
            ] {
                let before = scratch(&destination);
                let error = install_fixture(
                    &root,
                    &format!("https://fixture.invalid/{source}"),
                    &destination,
                    options("fixture", checksum, limit),
                    |_, _| Ok(()),
                )
                .unwrap_err();
                assert!(error.to_string().contains(expected), "{error}");
                assert_old_ready(&root, &destination);
                if source == "exit127" {
                    assert!(root.join("curl127-started").is_file());
                    assert!(matches!(&error, InstallFailure::Ordinary(_)));
                    assert!(!error.to_string().contains("needs `curl`"));
                }
                if source == "oversize" {
                    assert!(matches!(error, InstallFailure::Abort(_)));
                    assert!(
                        Path::new(
                            fs::read_to_string(root.join("download-path"))
                                .unwrap()
                                .trim()
                        )
                        .is_file()
                    );
                } else {
                    assert_eq!(scratch(&destination), before);
                }
            }
            let retained = scratch(&destination);
            let digest = format!("{:x}", Sha256::digest(NEW));
            let error = install_fixture(
                &root,
                "https://fixture.invalid/success",
                &destination,
                options("fixture", Some(&digest), 1024),
                |path, _| {
                    assert_eq!(path.file_name(), destination.file_name());
                    assert_eq!(fs::read(path).unwrap(), NEW);
                    assert_eq!(
                        fs::metadata(path).unwrap().permissions().mode() & 0o777,
                        0o755
                    );
                    assert_old_ready(&root, &destination);
                    Err(Error::Io("fixture version probe failed".into()).into())
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("version probe failed"));
            assert_old_ready(&root, &destination);
            assert_eq!(scratch(&destination), retained);
            fs::remove_file(root.join("bin/curl")).unwrap();
            install_fixture(
                &root,
                "https://fixture.invalid/success",
                &destination,
                options("fixture", None, MAX_SOURCE_BYTES),
                |_, _| Ok(()),
            )
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), NEW);
            assert_eq!(scratch(&destination), retained);
        }
        "shared-cache" => {
            let digest = format!("{:x}", Sha256::digest(NEW));
            install_fixture(
                &root,
                "https://fixture.invalid/success",
                &destination,
                options("fixture", Some(&digest), 1024),
                |_, _| Ok(()),
            )
            .unwrap();
            let cache = root.join(".cache/asb/downloads");
            assert_eq!(
                fs::read_dir(&cache)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "blob"))
                    .count(),
                1
            );
            let other = root.join("second-project");
            fs::create_dir_all(other.join(".asb")).unwrap();
            fs::write(
                other.join(".asb/project.toml"),
                "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
            )
            .unwrap();
            fs::remove_file(root.join("bin/curl")).unwrap();
            let other_destination = other.join("tools/perf/bin/perf");
            fs::create_dir_all(other_destination.parent().unwrap()).unwrap();
            install_fixture(
                &other,
                "https://fixture.invalid/success",
                &other_destination,
                options("fixture", Some(&digest), 1024),
                |_, _| Ok(()),
            )
            .unwrap();
            assert_eq!(fs::read(other_destination).unwrap(), NEW);
        }
        "symlinks" => {
            let foreign = root.join("foreign");
            fs::write(&foreign, "do not change").unwrap();
            let error = install_fixture(
                &root,
                "https://fixture.invalid/symlink",
                &destination,
                options("fixture", None, 1024),
                |_, _| Ok(()),
            )
            .unwrap_err();
            assert_eq!(fs::read(&foreign).unwrap(), b"do not change");
            assert_old_ready(&root, &destination);
            // The downloader replaced our owned file with a foreign symlink.
            // Explicit settlement must retain that entry, not silently unlink
            // it as the old best-effort Drop implementation did.
            let retained = fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".asb-install-")
                })
                .unwrap();
            assert_eq!(fs::read_link(retained.join("perf")).unwrap(), foreign);
            let InstallFailure::Abort(abort) = &error else {
                panic!("identity loss became an ordinary retry: {error}");
            };
            assert_eq!(
                abort.cause,
                super::super::process::AbortCause::OwnerObservation
            );
            assert!(abort.cleanup_confirmed && !abort.capture_complete);
            assert_eq!(abort.evidence.as_deref(), Some(retained.as_path()));
            assert!(
                error
                    .to_string()
                    .contains("private installation source identity changed"),
                "{error}"
            );
            fs::remove_file(&destination).unwrap();
            symlink(&foreign, &destination).unwrap();
            let error = install_fixture(
                &root,
                "https://fixture.invalid/failure",
                &destination,
                options("fixture", None, 1024),
                |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(error.to_string().contains("curl exited"));
            assert!(destination.is_symlink());
            assert_eq!(fs::read(&foreign).unwrap(), b"do not change");
            install_fixture(
                &root,
                "https://fixture.invalid/success",
                &destination,
                options("fixture", None, MAX_SOURCE_BYTES),
                |_, _| Ok(()),
            )
            .unwrap();
            assert!(!destination.is_symlink());
            assert_eq!(fs::read(&destination).unwrap(), NEW);
            assert_eq!(fs::read(&foreign).unwrap(), b"do not change");
            let blocked = root.join("blocked");
            fs::create_dir(&blocked).unwrap();
            fs::write(blocked.join("keep"), "untouched").unwrap();
            assert!(
                install_fixture(
                    &root,
                    "https://fixture.invalid/success",
                    &blocked,
                    options("fixture", None, 1024),
                    |_, _| Ok(())
                )
                .unwrap_err()
                .to_string()
                .contains("cannot install fixture at")
            );
            assert_eq!(fs::read(blocked.join("keep")).unwrap(), b"untouched");
            no_scratch(&blocked);
        }
        "timeout" => {
            let error = install_fixture_with_budget(
                &root,
                "https://fixture.invalid/timeout",
                &destination,
                options("fixture", None, 1024),
                |_, _| Ok(()),
                Some(1),
            )
            .unwrap_err();
            let InstallFailure::Abort(abort) = error else {
                panic!("download deadline was reduced to an ordinary error");
            };
            assert_eq!(abort.cause, super::super::process::AbortCause::Deadline);
            assert!(abort.cleanup_confirmed);
            let pid = fs::read_to_string(root.join("curl-pid")).unwrap();
            assert!(
                !Path::new("/proc").join(pid.trim()).exists(),
                "owned downloader was not reaped"
            );
            assert_old_ready(&root, &destination);
            let raw = fs::read_to_string(root.join("download-path")).unwrap();
            assert!(Path::new(raw.trim()).is_file());
        }
        "probe127" => {
            let source = root.join("valid-exit127");
            crate::write_test_file(
                &root,
                &source,
                b"#!/bin/sh\nprintf started > \"$ASB_FIXTURE_ROOT/probe127-started\"\nexit 127\n",
            )
            .unwrap();
            let mut raw = None;
            let error = install_fixture(
                &root,
                source.to_str().unwrap(),
                &destination,
                options("valid exit127 fixture", None, 4096),
                |path, scope| {
                    raw = Some(path.to_owned());
                    check_version_in(scope, path, "valid exit127 fixture")
                },
            )
            .unwrap_err();
            assert_old_ready(&root, &destination);
            assert!(!raw.unwrap().exists());
            assert!(root.join("probe127-started").is_file());
            assert!(matches!(&error, InstallFailure::Ordinary(_)));
            let text = error.to_string();
            assert!(text.contains("exit status: 127"), "{text}");
            assert!(
                !text.contains("cannot start") && !text.contains("executable missing"),
                "{text}"
            );

            // This real program DID start, then removed itself. Its advisory
            // may describe only the later check, never a fabricated launch fact.
            let removing = root.join("self-removing-probe");
            crate::write_test_file(
                &root,
                &removing,
                b"#!/bin/sh\nprintf started > \"$ASB_FIXTURE_ROOT/removing-probe-started\"\n/bin/rm -- \"$0\"\nexit 127\n",
            ).unwrap();
            fs::set_permissions(&removing, fs::Permissions::from_mode(0o755)).unwrap();
            let context = OperationContext::load(&root).unwrap();
            let owner = ProducerDirectory::open(&root).unwrap();
            let mut scope = owner.scope(&context);
            let error =
                check_version_in(&mut scope, &removing, "self-removing fixture").unwrap_err();
            owner.consume(scope.finish().unwrap()).unwrap();
            assert!(root.join("removing-probe-started").is_file() && !removing.exists());
            let text = error.to_string();
            assert!(
                text.contains("exit status: 127") && text.contains("post-failure check"),
                "{text}"
            );
            assert!(!text.contains("never started"), "{text}");
            assert_old_ready(&root, &destination);
        }
        "probe" => {
            let source = root.join("version-source");
            for body in [
                "#!/bin/sh\nexit 7\n",
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$ASB_FIXTURE_ROOT/probe-pid\"\nexec /bin/sleep 60\n",
            ] {
                crate::write_test_file(&root, &source, body).unwrap();
                let mut raw = None;
                let error = install_fixture_with_budget(
                    &root,
                    source.to_str().unwrap(),
                    &destination,
                    options("fixture", None, 1024),
                    |path, scope| {
                        raw = Some(path.to_owned());
                        check_version_in(scope, path, "fixture version")
                    },
                    Some(1),
                )
                .unwrap_err();
                assert!(error.to_string().contains("version"), "{error}");
                assert_old_ready(&root, &destination);
                if let InstallFailure::Abort(abort) = &error {
                    assert_eq!(abort.cause, super::super::process::AbortCause::Deadline);
                    assert!(abort.cleanup_confirmed);
                    assert!(raw.unwrap().is_file());
                } else {
                    no_scratch(&destination);
                }
            }
            let pid = fs::read_to_string(root.join("probe-pid")).unwrap();
            assert!(!Path::new("/proc").join(pid.trim()).exists());
            let context = OperationContext::load(&root).unwrap();
            let owner = ProducerDirectory::open(&root).unwrap();
            let mut scope = owner.scope(&context);
            let error = check_version_in(&mut scope, &root.join("absent"), "fixture").unwrap_err();
            owner.consume(scope.finish().unwrap()).unwrap();
            assert!(!root.join("absent").exists());
            assert!(
                error.to_string().contains("cannot start staged fixture"),
                "{error}"
            );
        }
        "blocked" => {
            install_fixture(
                &root,
                "https://fixture.invalid/blocked",
                &root.join("tools/fixture/bin/fixture"),
                options("fixture", None, MAX_SOURCE_BYTES),
                |_, _| Ok(()),
            )
            .unwrap();
        }
        "local" => {
            let pipe = root.join("not-a-file");
            let context = OperationContext::load(&root).unwrap();
            let owner = ProducerDirectory::open(&root).unwrap();
            let mut scope = owner.scope(&context);
            let status = scope
                .status(
                    "/usr/bin/mkfifo",
                    Purpose::Probe,
                    "fixture FIFO",
                    |command| {
                        command.arg(&pipe);
                    },
                )
                .unwrap();
            owner.consume(scope.finish().unwrap()).unwrap();
            assert!(status.success());
            let error = install_fixture(
                &root,
                pipe.to_str().unwrap(),
                &destination,
                Options {
                    executable: false,
                    ..options("fixture", None, MAX_SOURCE_BYTES)
                },
                |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(error.to_string().contains("source is not a file"));
            assert_old_ready(&root, &destination);
            let local = root.join("local-source");
            fs::write(&local, NEW).unwrap();
            let digest = format!("{:x}", Sha256::digest(NEW));
            install_fixture(
                &root,
                local.to_str().unwrap(),
                &destination,
                options("fixture", Some(&digest), MAX_SOURCE_BYTES),
                |path, scope| {
                    assert_eq!(fs::read(path).unwrap(), NEW);
                    assert_old_ready(&root, &destination);
                    check_version_in(scope, path, "fixture")
                },
            )
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), NEW);
            no_scratch(&destination);
            let mut output = Vec::new();
            super::super::tool(
                super::super::ToolInstallOptions {
                    name: "perf".into(),
                    source: Some("https://fixture.invalid/failure".into()),
                    dir: None,
                    dry_run: false,
                },
                &mut output,
            )
            .unwrap_err();
            assert_eq!(fs::read(&destination).unwrap(), NEW);
            assert!(super::super::tool_installed_locally(
                "perf",
                &root.join("tools")
            ));
            super::super::tool(
                super::super::ToolInstallOptions {
                    name: "perf".into(),
                    source: Some("https://fixture.invalid/success".into()),
                    dir: None,
                    dry_run: false,
                },
                &mut output,
            )
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), NEW);
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains("Installed tool perf")
            );
            no_scratch(&destination);
        }
        other => panic!("unexpected fixture case {other}"),
    }
}
