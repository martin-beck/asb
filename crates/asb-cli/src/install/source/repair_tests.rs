//! Publication faults use local files only; subprocess fixtures are isolated.
use super::*;
use publication_tests::Fixture;
use sha2::Digest;
use std::os::unix::fs::{PermissionsExt, symlink};

const OLD: &[u8] = b"#!/bin/sh\nprintf 'old tool\\n'\n";
const NEW: &[u8] = b"#!/bin/sh\nprintf 'new tool\\n'\n";

// Fixture orchestration only: all byte preparation, producer settlement and
// publication are the actual production path, with its same callback scope.
fn install_fixture_input(
    root: &Path,
    source: &Input,
    destination: &Path,
    options: Options<'_>,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
    publication: Publication,
) -> InstallResult<()> {
    let context = OperationContext::load(root)?;
    install_input_in(
        &context,
        source,
        destination,
        options,
        validate,
        publication,
    )
}

#[test]
fn nonexecutable_readonly_source_keeps_its_previously_supported_mode() {
    let fixture = Fixture::new();
    let source = fixture.0.join("readonly-source");
    fs::write(&source, b"validated workload").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).unwrap();
    let destination = fixture.0.join("readonly-destination");
    install_fixture_input(
        &fixture.0,
        &Input::Local(source),
        &destination,
        Options {
            executable: false,
            ..options()
        },
        |_, _| Ok(()),
        Publication::Replace,
    )
    .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"validated workload");
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

#[test]
fn quiet_commit_io_failures_preserve_prior_bytes_and_detached_name_arrivals() {
    for point in [
        FaultPoint::CommitCopy,
        FaultPoint::CommitSync,
        FaultPoint::CommitRename,
    ] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let _fault = faults::set(point, |_| Err(Error::Io("quiet commit I/O fault".into())));
        let error = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            options(),
            |_, _| Ok(()),
            Publication::Replace,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("quiet commit I/O fault"), "{error}");
        old(&destination);
        publication_tests::no_scratch(&destination);
    }
    let fixture = Fixture::new();
    let destination = old_destination(&fixture.0);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
    let saved = std::rc::Rc::clone(&seen);
    let _fault = faults::set(FaultPoint::CommitRename, move |exposed| {
        fs::write(exposed, b"late foreign arrival").map_err(crate::io_error)?;
        *saved.borrow_mut() = Some(exposed.to_owned());
        Ok(())
    });
    let error = install_fixture_input(
        &fixture.0,
        &Input::Local(fixture.0.join("new-source")),
        &destination,
        options(),
        |_, _| Ok(()),
        Publication::Replace,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("publication completed") && error.contains("cleanup unconfirmed"),
        "{error}"
    );
    assert_eq!(fs::read(&destination).unwrap(), NEW);
    assert_eq!(
        fs::read(seen.borrow().as_ref().unwrap()).unwrap(),
        b"late foreign arrival"
    );
}

#[test]
fn arrivals_after_final_check_are_not_published_or_unlinked() {
    for point in [FaultPoint::CommitTake, FaultPoint::CleanupTake] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let retained = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = std::rc::Rc::clone(&retained);
        let _fault = faults::set(point, move |path| {
            fs::remove_file(path).map_err(crate::io_error)?;
            fs::write(path, b"foreign final-check arrival").map_err(crate::io_error)?;
            *observed.borrow_mut() = Some(path.to_owned());
            Ok(())
        });
        let result = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            options(),
            |_, _| {
                if point == FaultPoint::CleanupTake {
                    Err(Error::Io("primary probe failure".into()).into())
                } else {
                    Ok(())
                }
            },
            Publication::Replace,
        );
        assert!(result.is_err(), "foreign arrival was published");
        old(&destination);
        assert_eq!(
            fs::read(retained.borrow().as_ref().unwrap()).unwrap(),
            b"foreign final-check arrival"
        );
    }
}

#[test]
fn symlink_arrival_and_recovery_collision_retain_both_foreign_entries() {
    for collide in [false, true] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let foreign = fixture.0.join("foreign-target");
        fs::write(&foreign, b"foreign target unchanged").unwrap();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = std::rc::Rc::clone(&seen);
        let target = foreign.clone();
        let _fault = faults::set(FaultPoint::CommitTake, move |path| {
            *observed.borrow_mut() = Some(path.to_owned());
            fs::remove_file(path).map_err(crate::io_error)?;
            symlink(target, path).map_err(crate::io_error)?;
            if collide {
                faults::arm(FaultPoint::Recovery, |path| {
                    fs::write(path, b"second arrival").map_err(crate::io_error)
                });
            }
            Ok(())
        });
        let error = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            options(),
            |_, _| Ok(()),
            Publication::Replace,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no-clobber recovery"), "{error}");
        let original = seen.borrow();
        let original = original.as_ref().unwrap();
        if collide {
            assert_eq!(fs::read(original).unwrap(), b"second arrival");
        } else {
            assert!(original.is_symlink());
        }
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign target unchanged");
        let retained = fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("entry"))
            .any(|entry| entry.is_symlink() && fs::read_link(entry).unwrap() == foreign);
        assert!(retained, "foreign quarantine was deleted");
        old(&destination);
    }
}

#[test]
fn empty_foreign_directory_arriving_after_last_check_is_retained() {
    let fixture = Fixture::new();
    let destination = old_destination(&fixture.0);
    let identity = std::rc::Rc::new(std::cell::RefCell::new(None));
    let saved = std::rc::Rc::clone(&identity);
    let _fault = faults::set(FaultPoint::DirectoryTake, move |path| {
        fs::rename(path, path.with_extension("owned-retained")).map_err(crate::io_error)?;
        fs::create_dir(path).map_err(crate::io_error)?;
        *saved.borrow_mut() = Some(fs::metadata(path).map_err(crate::io_error)?);
        Ok(())
    });
    let error = install_fixture_input(
        &fixture.0,
        &Input::Local(fixture.0.join("new-source")),
        &destination,
        options(),
        |_, _| Err(Error::Io("primary failure".into()).into()),
        Publication::Replace,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("primary failure") && error.contains("directory recovery refused"),
        "{error}"
    );
    assert!(
        fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .any(|entry| {
                let path = entry.unwrap().path().join("entry");
                fs::metadata(path)
                    .is_ok_and(|now| same_object(identity.borrow().as_ref().unwrap(), &now))
            })
    );
    old(&destination);
}
fn options() -> Options<'static> {
    Options {
        kind: "recipe fixture",
        limit: 1024,
        checksum: None,
        executable: true,
    }
}
fn old(path: &Path) {
    assert_eq!(fs::read(path).unwrap(), OLD);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o751
    );
}
fn old_destination(root: &Path) -> PathBuf {
    let path = root.join("tools/perf/bin/perf");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
    path
}

#[test]
fn probe_identity_content_mode_empty_and_size_obligations_are_independent() {
    for change in [
        "identical-replacement",
        "identical-replacement-checksum",
        "content",
        "mode",
        "empty",
        "oversized",
    ] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let checksum = format!("{:x}", sha2::Sha256::digest(NEW));
        let checked = Options {
            checksum: if change == "identical-replacement-checksum" {
                Some(&checksum)
            } else {
                None
            },
            ..options()
        };
        let result = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            checked,
            |path, _| {
                (|| -> io::Result<()> {
                    match change {
                        "identical-replacement" | "identical-replacement-checksum" => {
                            fs::remove_file(path)?;
                            fs::write(path, NEW)?;
                            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
                        }
                        "content" => {
                            let mut changed = NEW.to_vec();
                            changed[12] = b'X';
                            fs::write(path, changed)?;
                        }
                        "mode" => fs::set_permissions(path, fs::Permissions::from_mode(0o644))?,
                        "empty" => fs::write(path, [])?,
                        "oversized" => fs::write(path, [b'x'; 1025])?,
                        _ => unreachable!(),
                    }
                    Ok(())
                })()
                .map_err(|error| crate::io_error(error).into())
            },
            Publication::Replace,
        );
        assert!(result.is_err(), "{change} was published");
        old(&destination);
    }
}

#[test]
fn copy_mode_sync_and_commit_faults_preserve_old_bytes_and_permissions() {
    for point in [
        FaultPoint::Copy,
        FaultPoint::Mode,
        FaultPoint::Sync,
        FaultPoint::Commit,
    ] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let _fault = faults::set(point, |path| {
            fs::write(path, b"partial private output").map_err(crate::io_error)?;
            Err(Error::Io("fixture publication fault".into()))
        });
        let result = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            options(),
            |_, _| Ok(()),
            Publication::Replace,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fixture publication fault")
        );
        old(&destination);
        publication_tests::no_scratch(&destination);
    }
}

#[test]
fn replacement_at_commit_or_cleanup_is_retained_and_explicit_failure_is_not_retried() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    for point in [FaultPoint::Commit, FaultPoint::Cleanup] {
        let fixture = Fixture::new();
        let destination = old_destination(&fixture.0);
        let foreign = fixture.0.join("foreign");
        fs::write(&foreign, b"foreign bytes").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o640)).unwrap();
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::new(RefCell::new(None));
        let count = Rc::clone(&calls);
        let saved = Rc::clone(&observed);
        let target = foreign.clone();
        let _fault = faults::set(point, move |path| {
            count.set(count.get() + 1);
            *saved.borrow_mut() = Some(path.to_path_buf());
            fs::remove_file(path).map_err(crate::io_error)?;
            symlink(&target, path).map_err(crate::io_error)?;
            Ok(())
        });
        let result = install_fixture_input(
            &fixture.0,
            &Input::Local(fixture.0.join("new-source")),
            &destination,
            options(),
            |_, _| {
                if point == FaultPoint::Cleanup {
                    Err(Error::Io("primary probe failure".into()).into())
                } else {
                    Ok(())
                }
            },
            Publication::Replace,
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("retained staging"), "{error}");
        if point == FaultPoint::Cleanup {
            assert!(error.contains("primary probe failure"));
        }
        assert_eq!(calls.get(), 1);
        assert!(observed.borrow().as_ref().unwrap().is_symlink());
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign bytes");
        assert_eq!(
            fs::metadata(&foreign).unwrap().permissions().mode() & 0o777,
            0o640
        );
        old(&destination);
    }
    let fixture = Fixture::new();
    let destination = old_destination(&fixture.0);
    let calls = Rc::new(Cell::new(0));
    let count = Rc::clone(&calls);
    let _fault = faults::set(FaultPoint::Cleanup, move |_| {
        count.set(count.get() + 1);
        Err(Error::Io("cleanup refused".into()))
    });
    let error = install_fixture_input(
        &fixture.0,
        &Input::Local(fixture.0.join("new-source")),
        &destination,
        options(),
        |_, _| Err(Error::Io("primary failure".into()).into()),
        Publication::Replace,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("primary failure") && error.contains("cleanup refused"));
    assert_eq!(calls.get(), 1);
    assert!(
        fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".asb-install-"))
    );
    old(&destination);
}

#[test]
fn raw_local_sources_and_destination_paths_are_not_lossily_round_tripped() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let input = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
    let directory = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"tools-\xfe".to_vec()));
    fs::create_dir(&directory).unwrap();
    fs::write(&input, NEW).unwrap();
    let destination = directory.join("fixture");
    install_fixture_input(
        &fixture.0,
        &Input::Local(input.clone()),
        &destination,
        options(),
        |_, _| Ok(()),
        Publication::Replace,
    )
    .unwrap();
    assert_eq!(fs::read(destination).unwrap(), NEW);
    let checksum = format!("{:x}", sha2::Sha256::digest(NEW));
    super::super::verify_sha256_preserving(&input, &checksum, "raw path").unwrap();
}

#[test]
fn prepared_source_keeps_its_original_file_when_the_name_is_replaced() {
    let fixture = Fixture::new();
    let path = fixture.0.join("new-source");
    let input = Input::pin_local(path.clone()).unwrap();
    fs::rename(&path, fixture.0.join("admitted-source")).unwrap();
    fs::write(&path, b"foreign arrival").unwrap();
    let destination = old_destination(&fixture.0);
    install_fixture_input(
        &fixture.0,
        &input,
        &destination,
        options(),
        |_, _| Ok(()),
        Publication::Replace,
    )
    .unwrap();
    assert_eq!(fs::read(destination).unwrap(), NEW);
    assert_eq!(fs::read(path).unwrap(), b"foreign arrival");
}

#[test]
fn real_self_mutating_version_probe_and_healthy_multicall_probe() {
    Fixture::new().run("repair-version");
}
#[test]
fn ordinary_host_faults_and_real_concurrent_automatic_creator() {
    Fixture::new().run("repair-host");
}
#[test]
fn pinned_local_goose_archives_and_extraction_faults() {
    Fixture::new().run("repair-goose");
}
#[test]
fn raw_project_and_explicit_directory_paths_keep_their_os_identity() {
    Fixture::new().run("repair-raw-project");
}

pub(super) fn child(case: &str, root: &Path) {
    match case {
        "repair-version" => {
            let destination = old_destination(root);
            let source = root.join("self-mutation");
            fs::write(&source, b"#!/bin/sh\nprintf changed >> \"$0\"\nexit 0\n").unwrap();
            let result = install_fixture_input(
                root,
                &Input::Local(source.clone()),
                &destination,
                options(),
                |path, scope| check_version_in(scope, path, "self-mutating fixture"),
                Publication::Replace,
            );
            assert!(result.is_err());
            old(&destination);
            fs::write(
                &source,
                b"#!/bin/sh\ntest \"${0##*/}\" = perf\nprintf 'healthy version\\n'\n",
            )
            .unwrap();
            install_fixture_input(
                root,
                &Input::Local(source),
                &destination,
                options(),
                |path, scope| check_version_in(scope, path, "multicall fixture"),
                Publication::Replace,
            )
            .unwrap();
            assert!(
                fs::read_to_string(destination)
                    .unwrap()
                    .contains("healthy version")
            );
        }
        "repair-host" => host_child(root),
        "repair-goose" => goose_child(root),
        "repair-raw-project" => raw_project_child(root),
        _ => unreachable!(),
    }
}

fn host_child(root: &Path) {
    let host = root.join("bin/fixture-host");
    fs::write(&host, NEW).unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
    let directory = root.join("tools/host");
    fs::create_dir_all(directory.join("bin")).unwrap();
    let destination = directory.join("bin/fixture-host");
    fs::write(&destination, OLD).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o751)).unwrap();
    for point in [
        FaultPoint::Copy,
        FaultPoint::Mode,
        FaultPoint::Sync,
        FaultPoint::Commit,
    ] {
        let _fault = faults::set(point, |_| Err(Error::Io("fixture host failure".into())));
        assert!(
            super::super::install_recipe(
                super::super::Recipe::Host {
                    executable: "fixture-host"
                },
                &directory,
                root,
                &mut Vec::new(),
                &mut |_, _| {}
            )
            .is_err()
        );
        old(&destination);
    }
    fs::remove_file(&destination).unwrap();
    let creator = destination.clone();
    let _fault = faults::set(FaultPoint::Commit, move |_| {
        std::thread::spawn(move || {
            use std::io::Write;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&creator)
                .unwrap();
            file.write_all(b"concurrent creator").unwrap();
            file.set_permissions(fs::Permissions::from_mode(0o711))
                .unwrap();
        })
        .join()
        .unwrap();
        Ok(())
    });
    super::super::install_host_dependency("fixture-host", &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"concurrent creator");
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
        0o711
    );
    publication_tests::no_scratch(&destination);
}

fn goose_child(root: &Path) {
    use sha2::Digest;
    use std::os::unix::ffi::OsStringExt;
    for program in ["tar", "gzip"] {
        symlink(
            Path::new("/usr/bin").join(program),
            root.join("bin").join(program),
        )
        .unwrap();
    }
    let payload = root.join("goose-payload");
    fs::create_dir(&payload).unwrap();
    let executable = payload.join("goose");
    fs::write(
        &executable,
        b"#!/bin/sh\nprintf 'goose fixture version\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let archive = root.join(std::ffi::OsString::from_vec(b"goose-\xff.tar.gz".to_vec()));
    let pack = || {
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&payload)
                .arg(".")
                .status()
                .unwrap()
                .success()
        );
    };
    pack();
    let checksum = || format!("{:x}", sha2::Sha256::digest(fs::read(&archive).unwrap()));
    let directory = root
        .join(std::ffi::OsString::from_vec(b"tools-\xfe".to_vec()))
        .join("goose");
    fs::create_dir_all(directory.join("bin")).unwrap();
    let destination = directory.join("bin/goose");
    fs::write(&destination, OLD).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o751)).unwrap();
    for script in [
        b"#!/bin/sh\nexit 7\n".as_slice(),
        b"#!/bin/sh\nprintf changed >> \"$0\"\nexit 0\n".as_slice(),
    ] {
        fs::write(&executable, script).unwrap();
        pack();
        assert!(install_goose_archive(&directory, &archive, &checksum(), &mut |_, _| {}).is_err());
        old(&destination);
    }
    fs::write(
        &executable,
        b"#!/bin/sh\ntest \"${0##*/}\" = goose\nprintf 'healthy Goose\\n'\n",
    )
    .unwrap();
    pack();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o751)).unwrap();
    for point in [
        FaultPoint::Copy,
        FaultPoint::Mode,
        FaultPoint::Sync,
        FaultPoint::Commit,
    ] {
        let _fault = faults::set(point, |_| Err(Error::Io("Goose fixture failure".into())));
        assert!(install_goose_archive(&directory, &archive, &checksum(), &mut |_, _| {}).is_err());
        old(&destination);
    }
    install_goose_archive(&directory, &archive, &checksum(), &mut |_, _| {}).unwrap();
    assert!(
        fs::read_to_string(&destination)
            .unwrap()
            .contains("healthy Goose")
    );
    for point in [
        FaultPoint::ArchiveCleanupTake,
        FaultPoint::ArchiveDirectoryTake,
    ] {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = std::rc::Rc::clone(&seen);
        let _fault = faults::set(point, move |path| {
            if point == FaultPoint::ArchiveCleanupTake {
                fs::remove_file(path).map_err(crate::io_error)?;
                fs::write(path, b"foreign extracted arrival").map_err(crate::io_error)?;
            } else {
                fs::rename(path, path.with_extension("owned-retained")).map_err(crate::io_error)?;
                fs::create_dir(path).map_err(crate::io_error)?;
            }
            *observed.borrow_mut() = Some(fs::metadata(path).map_err(crate::io_error)?);
            Ok(())
        });
        let mut stages = Vec::new();
        let error = install_goose_archive(&directory, &archive, &checksum(), &mut |_, stage| {
            stages.push(stage)
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("Goose scratch settlement failed"), "{error}");
        assert!(!stages.contains(&super::super::ProgressStage::Ready));
        assert!(
            fs::read_to_string(&destination)
                .unwrap()
                .contains("healthy Goose")
        );
        let saved = seen.borrow();
        let saved = saved.as_ref().unwrap();
        assert!(
            fs::read_dir(&directory).unwrap().any(|entry| {
                let entry = entry.unwrap().path();
                [entry.join("entry"), entry.join("goose")]
                    .iter()
                    .any(|path| fs::metadata(path).is_ok_and(|now| same_object(saved, &now)))
            }),
            "foreign extraction arrival was deleted: {error}"
        );
    }
    fs::write(&destination, OLD).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o751)).unwrap();
    assert!(install_goose_archive(&directory, &archive, "wrong", &mut |_, _| {}).is_err());
    old(&destination);
    fs::write(payload.join("unknown"), b"foreign archive entry").unwrap();
    pack();
    let error = install_goose_archive(&directory, &archive, &checksum(), &mut |_, _| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("retained Goose extraction"));
    old(&destination);
    assert!(
        fs::read_dir(&directory).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .join("unknown")
            .is_file())
    );
    fs::write(&archive, b"partial invalid archive").unwrap();
    let error = install_goose_archive(&directory, &archive, &checksum(), &mut |_, _| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("retained Goose extraction"));
    old(&destination);
}

fn install_goose_archive(
    root: &Path,
    archive: &Path,
    checksum: &str,
    progress: &mut dyn FnMut(u8, super::super::ProgressStage),
) -> Result<PathBuf, Error> {
    super::super::archive::install_archive(
        root,
        archive,
        checksum,
        progress,
        &crate::agents::goose_archive_spec(),
    )
}

fn raw_project_child(root: &Path) {
    use std::os::unix::ffi::OsStringExt;
    let project = root.join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
    fs::create_dir_all(project.join(".asb")).unwrap();
    fs::write(
        project.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    std::env::set_current_dir(&project).unwrap(); // Only this isolated fixture process.
    let tools = PathBuf::from(std::ffi::OsString::from_vec(b"tools-\xfe".to_vec()));
    super::super::tool_with_progress(
        super::super::ToolInstallOptions {
            name: "fixture-custom".into(),
            source: Some(root.join("new-source").to_str().unwrap().into()),
            dir: Some(tools.clone()),
            dry_run: false,
        },
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(
        fs::read(
            project
                .join(&tools)
                .join("fixture-custom/bin/fixture-custom")
        )
        .unwrap(),
        NEW
    );
    assert!(!project.join("tools-\u{fffd}").exists());
    let workloads = PathBuf::from(std::ffi::OsString::from_vec(b"workloads-\xfd".to_vec()));
    super::super::workload_with_progress(
        super::super::WorkloadInstallOptions {
            source: root.join("new-source").to_str().unwrap().into(),
            name: Some("raw-prompt".into()),
            dir: Some(workloads.clone()),
        },
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(
        fs::read(project.join(workloads).join("raw-prompt.txt")).unwrap(),
        NEW
    );
    // A project runtime under raw tools bytes must be the PATH prefix used by
    // the package recipe. No real npm, Node distribution or network is used.
    let node = project.join(&tools).join("node/bin/node");
    fs::create_dir_all(node.parent().unwrap()).unwrap();
    fs::write(&node, b"#!/bin/sh\nprintf 'v22.20.0\\n'\n").unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let npm = root.join("bin/npm");
    fs::write(&npm,b"#!/bin/sh\nset -eu\ntest -x \"${PATH%%:*}/node\"\ntest \"$5\" = fixture-raw@1\n/bin/mkdir -p \"$3/node_modules/.bin\" \"$NPM_CONFIG_CACHE\"\nprintf '#!/bin/sh\\nexit 0\\n' > \"$3/node_modules/.bin/fixture-client\"\nprintf seen > \"$NPM_CONFIG_CACHE/seen\"\n/bin/chmod 755 \"$3/node_modules/.bin/fixture-client\"\n").unwrap();
    fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
    let package = project.join(&tools).join("raw-package");
    let installed = super::super::install_recipe(
        super::super::Recipe::Npm {
            package: "fixture-raw@1",
            executable: "fixture-client",
            manifest: None,
        },
        &package,
        &project,
        &mut Vec::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(installed, package.join("node_modules/.bin/fixture-client"));
    assert!(crate::executables::is_executable(&installed));
    assert!(project.join(".asb-cache/npm/seen").is_file());
}
