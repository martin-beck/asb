//! Real owned files with deterministic I/O and namespace faults; no host tools.
use super::*;
use std::cell::{Cell, RefCell};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/setup-publication-fixtures")
            .join(crate::tools::runtime::unique_id());
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn destination(&self) -> PathBuf {
        self.0.join("setup.toml")
    }
    fn prepared(&self) -> prepared::Sweep {
        let config: BenchmarkConfig = toml::from_str(
            "agent='pi'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture'\n",
        )
        .unwrap();
        prepared::Sweep::new(&self.destination(), vec![config; 2]).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

type Action = Box<dyn FnOnce(&Path) -> io::Result<()>>;
struct Fault {
    point: Boundary,
    action: RefCell<Option<Action>>,
    events: RefCell<Vec<(Boundary, PathBuf)>>,
}
impl Fault {
    fn new(point: Boundary, action: impl FnOnce(&Path) -> io::Result<()> + 'static) -> Self {
        Self {
            point,
            action: RefCell::new(Some(Box::new(action))),
            events: RefCell::new(Vec::new()),
        }
    }
    fn failure(point: Boundary) -> Self {
        Self::new(point, |_| Err(io::Error::other("fixture I/O failure")))
    }
}
impl Backend for Fault {
    fn boundary(&self, point: Boundary, path: &Path) -> io::Result<()> {
        self.events.borrow_mut().push((point, path.into()));
        if self.point == point {
            let action = self.action.borrow_mut().take();
            if let Some(action) = action {
                action(path)?;
            }
        }
        Ok(())
    }
}
fn old(path: &Path) {
    assert_eq!(fs::read(path).unwrap(), b"old valid setup");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
}
fn prior(path: &Path) {
    fs::write(path, b"old valid setup").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o751)).unwrap();
    }
}

#[test]
fn scalar_faults_never_truncate_prior_bytes_or_mode_and_success_preserves_mode() {
    for boundary in [
        Boundary::Write,
        Boundary::FileSync,
        Boundary::Detach,
        Boundary::Commit,
    ] {
        let fixture = Fixture::new();
        let path = fixture.destination();
        prior(&path);
        let error = scalar_with(&path, b"new complete setup", &Fault::failure(boundary))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not published"), "{error}");
        old(&path);
    }
    let fixture = Fixture::new();
    let path = fixture.destination();
    prior(&path);
    scalar_with(&path, b"new complete setup", &System).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new complete setup");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
}

#[test]
fn postcommit_durability_and_cleanup_faults_are_not_presented_as_rollback() {
    for boundary in [
        Boundary::AfterCommit,
        Boundary::DirectorySync,
        Boundary::Cleanup,
    ] {
        let fixture = Fixture::new();
        let path = fixture.destination();
        prior(&path);
        let error = scalar_with(&path, b"new complete setup", &Fault::failure(boundary))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("was published") && error.contains("unconfirmed"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"new complete setup");
        if boundary == Boundary::Cleanup {
            assert!(error.contains("retained staging"), "{error}");
            assert!(
                fs::read_dir(&fixture.0).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".asb-setup-")),
                "a failed explicit settlement was retried during destruction"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn final_check_replacement_is_retained_not_published_or_deleted() {
    use std::os::unix::fs::symlink;
    for replace_with_symlink in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.destination();
        prior(&path);
        let foreign = fixture.0.join("foreign");
        fs::write(&foreign, b"foreign target").unwrap();
        let remembered = std::rc::Rc::new(RefCell::new(None));
        let saved = std::rc::Rc::clone(&remembered);
        let target = foreign.clone();
        let fault = Fault::new(Boundary::Detach, move |source| {
            *saved.borrow_mut() = Some(source.to_owned());
            fs::remove_file(source)?;
            if replace_with_symlink {
                symlink(target, source)?;
            } else {
                fs::write(source, b"foreign arrival")?;
            }
            Ok(())
        });
        let error = scalar_with(&path, b"new complete setup", &fault)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no-clobber recovery") && error.contains("retained staging"),
            "{error}"
        );
        old(&path);
        let source = remembered.borrow();
        let source = source.as_ref().unwrap();
        if replace_with_symlink {
            assert!(source.is_symlink());
        } else {
            assert_eq!(fs::read(source).unwrap(), b"foreign arrival");
        }
        assert_eq!(fs::read(foreign).unwrap(), b"foreign target");
    }
}

#[test]
fn failed_partial_generation_and_unknown_arrivals_remain_without_manifest() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared();
    let generation = prepared.directory.clone();
    let target = generation.clone();
    let fault = Fault::new(Boundary::Write, move |path| {
        fs::write(target.join("foreign-arrival"), b"not ours")?;
        fs::write(path, b"partial owned configuration")?;
        Err(io::Error::other("fixture ENOSPC"))
    });
    let error = sweep_with(&fixture.destination(), &prepared, &fault)
        .unwrap_err()
        .to_string();
    assert!(!fixture.destination().exists());
    assert_eq!(
        fs::read(generation.join("foreign-arrival")).unwrap(),
        b"not ours"
    );
    assert_eq!(
        fs::read(generation.join("benchmark-000.toml")).unwrap(),
        b"partial owned configuration"
    );
    assert!(
        error.contains(&generation.display().to_string()) && error.contains("possibly partial"),
        "{error}"
    );
}

#[test]
fn sweep_commits_manifest_last_no_replace_after_every_configuration_sync() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared();
    let path = fixture.destination();
    let fault = Fault::new(Boundary::Commit, |destination| {
        // A real create-new competitor wins immediately before publication.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        file.write_all(b"competing manifest")?;
        Ok(())
    });
    let error = sweep_with(&path, &prepared, &fault)
        .unwrap_err()
        .to_string();
    assert_eq!(fs::read(&path).unwrap(), b"competing manifest");
    assert!(
        error.contains("not published") && error.contains("remain in"),
        "{error}"
    );
    let events = fault.events.borrow();
    let commit = events
        .iter()
        .position(|(event, _)| *event == Boundary::Commit)
        .unwrap();
    for (name, _) in &prepared.configurations {
        assert!(events[..commit].contains(&(Boundary::FileSync, prepared.directory.join(name))));
    }
    assert!(events[..commit].contains(&(Boundary::DirectorySync, prepared.directory.clone())));
}

#[test]
fn replaced_generated_configuration_is_rejected_before_any_manifest_appears() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared();
    let selected = prepared.directory.join("benchmark-000.toml");
    let fault = Fault::new(Boundary::DirectorySync, move |_| {
        fs::rename(&selected, selected.with_extension("owned-retained"))?;
        fs::write(selected, b"foreign replacement")
    });
    let error = sweep_with(&fixture.destination(), &prepared, &fault)
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed before manifest"), "{error}");
    assert!(!fixture.destination().exists());
    assert_eq!(
        fs::read(prepared.directory.join("benchmark-000.toml")).unwrap(),
        b"foreign replacement"
    );
}

#[test]
fn write_all_retries_interrupted_and_short_writes_but_not_zero_or_enospc() {
    #[derive(Clone, Copy)]
    enum Mode {
        ShortInterrupted,
        Zero,
        Enospc,
    }
    struct Writes(Mode);
    struct Writer<'a> {
        file: &'a mut File,
        calls: Cell<usize>,
        mode: Mode,
    }
    impl Write for Writer<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call == 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if call > 1 {
                match self.mode {
                    Mode::Zero => return Ok(0),
                    Mode::Enospc => return Err(io::Error::from_raw_os_error(28)),
                    Mode::ShortInterrupted => {}
                }
            }
            self.file.write(&bytes[..bytes.len().min(3)])
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    impl Backend for Writes {
        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            Writer {
                file,
                calls: Cell::new(0),
                mode: self.0,
            }
            .write_all(bytes)
        }
    }
    for mode in [Mode::ShortInterrupted, Mode::Zero, Mode::Enospc] {
        let fixture = Fixture::new();
        let path = fixture.destination();
        prior(&path);
        let result = scalar_with(&path, b"complete replacement", &Writes(mode));
        if matches!(mode, Mode::ShortInterrupted) {
            result.unwrap();
            assert_eq!(fs::read(path).unwrap(), b"complete replacement");
        } else {
            assert!(result.is_err());
            old(&path);
        }
    }
}

#[test]
fn sweep_sync_commit_and_postcommit_errors_disclose_the_actual_publication_state() {
    for point in [
        Boundary::FileSync,
        Boundary::DirectorySync,
        Boundary::Commit,
        Boundary::AfterCommit,
        Boundary::Cleanup,
    ] {
        let fixture = Fixture::new();
        let path = fixture.destination();
        let prepared = fixture.prepared();
        let error = sweep_with(&path, &prepared, &Fault::failure(point))
            .unwrap_err()
            .to_string();
        let committed = matches!(point, Boundary::AfterCommit | Boundary::Cleanup);
        assert_eq!(path.exists(), committed, "{point:?}: {error}");
        if committed {
            assert!(
                error.contains("Setup was published")
                    && !error.contains("Sweep manifest was not published"),
                "{error}"
            );
            let sweep = crate::sweep::read_manifest(&path).unwrap();
            for name in sweep.configs {
                let text = crate::read_config(&fixture.0.join(name)).unwrap();
                toml::from_str::<BenchmarkConfig>(&text).unwrap();
            }
        } else {
            assert!(
                error.contains("Sweep manifest was not published"),
                "{error}"
            );
        }
        assert!(
            error.contains(&prepared.directory.display().to_string()),
            "{error}"
        );
        assert!(prepared.directory.is_dir());
    }
}

#[test]
fn primary_write_error_and_unknown_scratch_are_preserved_without_drop_retry() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    prior(&path);
    let fault = Fault::new(Boundary::Write, |source| {
        fs::write(source.parent().unwrap().join("unknown"), b"foreign scratch")?;
        Err(io::Error::other("primary write failure"))
    });
    let error = scalar_with(&path, b"complete replacement", &fault)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("primary write failure") && error.contains("retained staging"),
        "{error}"
    );
    assert_eq!(
        fault
            .events
            .borrow()
            .iter()
            .filter(|(point, _)| *point == Boundary::Detach)
            .count(),
        1
    );
    assert!(fs::read_dir(&fixture.0).unwrap().any(|entry| {
        fs::read(entry.unwrap().path().join("previous-stage/unknown"))
            .is_ok_and(|bytes| bytes == b"foreign scratch")
    }));
    old(&path);
}

#[test]
fn foreign_empty_staging_directory_is_not_removed_after_the_last_check() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    prior(&path);
    let identity = std::rc::Rc::new(RefCell::new(None));
    let saved = std::rc::Rc::clone(&identity);
    let fault = Fault::new(Boundary::DirectoryDetach, move |directory| {
        fs::rename(directory, directory.with_extension("owned-retained"))?;
        fs::create_dir(directory)?;
        *saved.borrow_mut() = Some(key(&open(directory)?)?);
        Ok(())
    });
    let error = scalar_with(&path, b"complete replacement", &fault)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("automatic directory recovery refused"),
        "{error}"
    );
    assert!(fs::read_dir(&fixture.0).unwrap().any(|entry| {
        open(&entry.unwrap().path().join("previous-stage"))
            .and_then(|file| key(&file))
            .is_ok_and(|value| Some(value) == *identity.borrow())
    }));
    old(&path);
}

#[test]
fn foreign_file_recovery_does_not_overwrite_a_second_creator() {
    struct ReplaceThenCreate(RefCell<Option<PathBuf>>);
    impl Backend for ReplaceThenCreate {
        fn boundary(&self, point: Boundary, path: &Path) -> io::Result<()> {
            if point == Boundary::Detach {
                fs::remove_file(path)?;
                fs::write(path, b"first foreign arrival")?;
                *self.0.borrow_mut() = Some(path.into());
            } else if point == Boundary::Recovery {
                fs::write(path, b"second foreign arrival")?;
            }
            Ok(())
        }
    }
    let fixture = Fixture::new();
    let path = fixture.destination();
    prior(&path);
    let backend = ReplaceThenCreate(RefCell::new(None));
    let error = scalar_with(&path, b"complete replacement", &backend)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no-clobber recovery"), "{error}");
    assert_eq!(
        fs::read(backend.0.borrow().as_ref().unwrap()).unwrap(),
        b"second foreign arrival"
    );
    assert!(fs::read_dir(&fixture.0).unwrap().any(|entry| {
        fs::read(entry.unwrap().path().join("prepared"))
            .is_ok_and(|bytes| bytes == b"first foreign arrival")
    }));
    old(&path);
}

#[test]
fn shared_cli_tui_publisher_reports_confirmation_failure_after_complete_publication() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let fixture = Fixture::new();
    let config: BenchmarkConfig =
        toml::from_str("agent='pi'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture'\n")
            .unwrap();
    let error = publish_sweep(&fixture.destination(), vec![config; 2], &mut Broken)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("was published") && error.contains("confirmation output failed"),
        "{error}"
    );
    let manifest = crate::sweep::read_manifest(&fixture.destination()).unwrap();
    for reference in manifest.configs {
        let text = crate::read_config(&fixture.0.join(reference)).unwrap();
        toml::from_str::<BenchmarkConfig>(&text).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn scalar_replaces_destination_symlinks_without_changing_targets_and_native_rename_failure_preserves_entries()
 {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let path = fixture.destination();
    let target = fixture.0.join("foreign-target");
    prior(&target);
    symlink(&target, &path).unwrap();
    scalar(&path, b"complete new configuration").unwrap();
    assert!(!path.is_symlink());
    old(&target);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("foreign"), b"unchanged directory entry").unwrap();
    assert!(
        scalar(&path, b"not a directory")
            .unwrap_err()
            .to_string()
            .contains("publication outcome is unconfirmed")
    );
    assert_eq!(
        fs::read(path.join("foreign")).unwrap(),
        b"unchanged directory entry"
    );
}

#[test]
fn a_partial_manifest_is_never_visible_while_its_private_writer_is_blocked() {
    use std::sync::mpsc;
    struct Paused {
        manifest: Cell<bool>,
        ready: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    }
    impl Backend for Paused {
        fn boundary(&self, point: Boundary, path: &Path) -> io::Result<()> {
            if point == Boundary::Write {
                self.manifest
                    .set(path.file_name().is_some_and(|name| name == "prepared"));
            }
            Ok(())
        }
        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            if self.manifest.get() {
                file.write_all(&bytes[..3])?;
                self.ready.send(()).map_err(io::Error::other)?;
                self.resume
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .map_err(io::Error::other)?;
                file.write_all(&bytes[3..])
            } else {
                file.write_all(bytes)
            }
        }
    }
    let fixture = Fixture::new();
    let path = fixture.destination();
    let prepared = fixture.prepared();
    let (ready, observe) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let destination = path.clone();
    let worker = std::thread::spawn(move || {
        sweep_with(
            &destination,
            &prepared,
            &Paused {
                manifest: Cell::new(false),
                ready,
                resume: wait,
            },
        )
    });
    observe
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let absent = !path.exists();
    resume.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(absent, "partial manifest escaped private staging");
    assert_eq!(crate::sweep::read_manifest(&path).unwrap().configs.len(), 2);
}

#[test]
fn replacing_tui_state_never_exposes_partial_toml_to_concurrent_readers() {
    use std::sync::mpsc;
    struct Paused {
        ready: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    }
    impl Backend for Paused {
        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            file.write_all(&bytes[..3])?;
            self.ready.send(()).map_err(io::Error::other)?;
            self.resume
                .recv_timeout(std::time::Duration::from_secs(3))
                .map_err(io::Error::other)?;
            file.write_all(&bytes[3..])
        }
    }
    let fixture = Fixture::new();
    for name in ["last-setup-tui.toml", "swe-bench.workload.toml"] {
        let path = fixture.0.join(name);
        let old = b"config = 'previous.toml'\n";
        let new = b"config = 'replacement.toml'\n";
        fs::write(&path, old).unwrap();
        let (ready, observe) = mpsc::sync_channel(1);
        let (resume, wait) = mpsc::sync_channel(1);
        let destination = path.clone();
        let worker = std::thread::spawn(move || {
            scalar_with(
                &destination,
                new,
                &Paused {
                    ready,
                    resume: wait,
                },
            )
        });
        observe
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), old);
        resume.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), new);
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_sweep_parent_and_stem_refuse_before_any_publication_mutation() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let parent = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
    fs::create_dir(&parent).unwrap();
    let prior = parent.join("prior.toml");
    fs::write(&prior, b"prior configuration unchanged").unwrap();
    let config: BenchmarkConfig = toml::from_str("agent='pi'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n").unwrap();
    for path in [
        parent.join("not-created/selection.toml"),
        fixture.0.join(std::ffi::OsString::from_vec(
            b"selection-\xfe.toml".to_vec(),
        )),
    ] {
        let error = publish_sweep(&path, vec![config.clone()], &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("UTF-8") && error.contains("no setup files were written"),
            "{error}"
        );
        assert!(!path.exists());
        assert!(!parent.join("not-created").exists());
        assert_eq!(fs::read(&prior).unwrap(), b"prior configuration unchanged");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}

#[test]
fn ambiguous_commit_retains_evidence_and_never_claims_not_published_or_retries() {
    struct Uncertain {
        perform: bool,
        interrupted: bool,
        commits: Cell<usize>,
        cleanups: Cell<usize>,
    }
    impl Backend for Uncertain {
        fn boundary(&self, point: Boundary, _: &Path) -> io::Result<()> {
            if point == Boundary::Cleanup {
                self.cleanups.set(self.cleanups.get() + 1);
            }
            Ok(())
        }
        fn commit(&self, source: &Path, destination: &Path, no_replace: bool) -> io::Result<()> {
            self.commits.set(self.commits.get() + 1);
            if self.perform {
                System.commit(source, destination, no_replace)?;
            }
            if self.interrupted {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Err(io::Error::from_raw_os_error(5))
            } // EIO, after a real commit.
        }
    }
    for no_replace in [false, true] {
        for (perform, interrupted) in [(true, false), (true, true), (false, false)] {
            let fixture = Fixture::new();
            let path = fixture.destination();
            if !no_replace {
                prior(&path);
            }
            let backend = Uncertain {
                perform,
                interrupted,
                commits: Cell::new(0),
                cleanups: Cell::new(0),
            };
            let prepared = fixture.prepared();
            let result = if no_replace {
                sweep_with(&path, &prepared, &backend)
            } else {
                scalar_with(&path, b"complete new configuration", &backend)
            };
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("publication outcome is unconfirmed")
                    && error.contains(&path.display().to_string())
                    && error.contains("retained staging")
                    && !error.contains("not published"),
                "{error}"
            );
            assert_eq!(backend.commits.get(), 1, "commit was retried");
            assert_eq!(backend.cleanups.get(), 0, "uncertain evidence was cleaned");
            assert!(
                fs::read_dir(&fixture.0).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".asb-setup-")),
                "quarantine was lost"
            );
            if perform {
                if no_replace {
                    let manifest = crate::sweep::read_manifest(&path).unwrap();
                    for name in manifest.configs {
                        toml::from_str::<BenchmarkConfig>(
                            &crate::read_config(&fixture.0.join(name)).unwrap(),
                        )
                        .unwrap();
                    }
                    assert!(prepared.directory.is_dir());
                } else {
                    assert_eq!(fs::read(&path).unwrap(), b"complete new configuration");
                }
            } else if no_replace {
                assert!(!path.exists());
                assert!(prepared.directory.is_dir());
            } else {
                old(&path);
            }
        }
    }
}
