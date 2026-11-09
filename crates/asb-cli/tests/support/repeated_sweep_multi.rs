//! Different configurations advance through different recording generations.
use super::*;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::Ordering;

/// Establish fallback custody before spawn; assertion failures cannot abandon
/// the ASB owner. Its own guardians retain descendant cleanup responsibility.
struct CampaignChild {
    child: Option<Child>,
    keeper: fixture_writer::OneSlotKeeper,
}
impl CampaignChild {
    fn spawn(command: &mut Command) -> Self {
        let keeper = fixture_writer::OneSlotKeeper::new().unwrap();
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            keeper,
        }
    }
    #[allow(unsafe_code)]
    fn finish(mut self) -> Output {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        assert!(self.child.as_ref().unwrap().stdout.is_none());
        let mut reader = self.child.as_mut().unwrap().stderr.take().unwrap();
        let fd = reader.as_raw_fd();
        // SAFETY: reader owns the live descriptor throughout; these fcntl
        // operations only query/set file status and take no pointer arguments.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut stderr = Vec::new();
        let mut eof = false;
        let mut status = None;
        loop {
            if !eof {
                let mut bytes = [0; 4096];
                match reader.read(&mut bytes) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        stderr.extend_from_slice(&bytes[..count]);
                        assert!(
                            stderr.len() <= 64 * 1024,
                            "fixture diagnostic budget exceeded"
                        );
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("fixture stderr: {error}"),
                }
            }
            if status.is_none() {
                status = self.child.as_mut().unwrap().try_wait().unwrap();
            }
            if eof && let Some(status) = status {
                // Only confirmed reaping releases the guard's actual Child.
                self.child.take();
                return Output {
                    status,
                    stdout: Vec::new(),
                    stderr,
                };
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture settlement deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
impl std::ops::Deref for CampaignChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.child.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for CampaignChild {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}
impl Drop for CampaignChild {
    fn drop(&mut self) {
        use fixture_writer::Keeper;
        if let Some(mut child) = self.child.take() {
            child.stdin.take();
            child.stdout.take();
            child.stderr.take();
            let _ = child.kill();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    _ => {
                        self.keeper.adopt(child);
                        break;
                    }
                }
            }
        }
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn freeze(paths: impl IntoIterator<Item = PathBuf>) -> Vec<(PathBuf, Vec<u8>)> {
    paths
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn unchanged(frozen: &[(PathBuf, Vec<u8>)]) {
    for (path, bytes) in frozen {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "changed {}",
            path.display()
        );
    }
}

fn held_batch(child: &mut Child, source: &Prelude, controls: &Path) {
    while !(1..=2).all(|replica| controls.join(format!("barrier-1-{replica}")).exists()) {
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= source.deadline {
            panic!("configuration clients did not reach their owned barrier");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn interrupt(child: CampaignChild, source: Prelude, controls: &Path) -> Output {
    // Actual clients already reached the barrier. Closing only presentation
    // must settle this batch, rather than implicitly retrying or dropping it.
    drop(source);
    fs::write(controls.join("release-first-batch"), "release owned batch").unwrap();
    let output = child.finish();
    assert!(!output.status.success());
    output
}

fn attempts(controls: &Path) -> usize {
    match fs::read(controls.join("attempts.jsonl")) {
        Ok(bytes) => records(&bytes).len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("attempt evidence: {error}"),
    }
}

#[test]
fn recording_continuation_resolves_latest_applicable_generation_per_configuration() {
    // Any panic retains all evidence, including while fallback/descendant
    // custody is still settling. Normal completion explicitly cleans below.
    let project = std::mem::ManuallyDrop::new(registered_project(2));
    let second = project.0.join("second-control");
    fs::create_dir(&second).unwrap();
    for controls in [&project.0, &second] {
        fs::write(controls.join("expected-concurrency"), "2").unwrap();
        fs::write(controls.join("hold-first-batch"), "bounded batch barrier").unwrap();
    }
    fs::write(project.0.join("false-grade-first"), "retain failed grade").unwrap();
    // Configure both namespaces BEFORE sealing the installed executable. The
    // agent identity remains pi; only test counters/barriers are independent.
    let executable = project.0.join("tools/pi/bin/pi");
    let script = fs::read_to_string(&executable).unwrap().replace(
        "workspace = pathlib.Path.cwd()",
        "workspace = pathlib.Path.cwd()\nif any(parent.name == '001-second' for parent in workspace.parents): root = root/'second-control'",
    );
    fixture_writer::write_file(&project.0, &executable, script).unwrap();
    fs::copy(
        project.0.join("benchmark.toml"),
        project.0.join("second.toml"),
    )
    .unwrap();
    fs::write(
        project.0.join("sweep.toml"),
        "configs=['benchmark.toml','second.toml']\n",
    )
    .unwrap();
    let provider = RecordingProvider::new();
    let mut initial = CampaignChild::spawn(&mut sweep_command(
        &project,
        Some(&provider),
        &["--record", "--repeat", "2", "--jobs", "1"],
    ));
    let mut source = Prelude::new(&mut initial);
    let event = source.try_event().unwrap();
    assert_eq!(event["type"], "execution-plan");
    held_batch(&mut initial, &source, &project.0);
    interrupt(initial, source, &project.0);
    assert_eq!((attempts(&project.0), attempts(&second)), (2, 0));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    let results = project.0.join("sweep-results");
    let checkpoint = results.join(".checkpoint");
    let index = read_json(&results.join("plan.json"));
    let roots: Vec<_> = index["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| results.join(entry["campaign"]["root"].as_str().unwrap()))
        .collect();
    assert_eq!(roots.len(), 2);
    let plans: Vec<_> = roots
        .iter()
        .map(|root| read_json(&root.join("plan.json")))
        .collect();
    let mut frozen = freeze([
        results.join("plan.json"),
        checkpoint.join("inputs.json"),
        checkpoint.join("campaign-admission.json"),
        roots[0].join("plan.json"),
        roots[1].join("plan.json"),
        PathBuf::from(plans[0]["recording"].as_str().unwrap()),
        checkpoint.join("campaign-completed/000000/000001.json"),
        checkpoint.join("campaign-completed/000000/000002.json"),
    ]);

    let mut resumed = CampaignChild::spawn(&mut sweep_command(
        &project,
        Some(&provider),
        &["--record", "--resume"],
    ));
    let mut source = Prelude::new(&mut resumed);
    let mut seen = BTreeSet::new();
    while seen.len() < 4 {
        let row = source.try_event().unwrap();
        if row["task"].is_object() && row["execution"]["campaign"] == plans[0]["campaign"] {
            let slot = row["execution"]["slot"].as_u64().unwrap();
            assert!((1..=4).contains(&slot));
            assert!(seen.insert(slot), "duplicate first-configuration summary");
        }
    }
    held_batch(&mut resumed, &source, &second);
    interrupt(resumed, source, &second);
    assert_eq!((attempts(&project.0), attempts(&second)), (4, 2));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 6);
    unchanged(&frozen);
    let first_generation = checkpoint.join("campaign-continuation");
    let manifests: Vec<_> = (0..2)
        .map(|index| read_json(&first_generation.join(format!("{index:06}.json"))))
        .collect();
    frozen.extend(freeze([
        first_generation.join("admitted.json"),
        first_generation.join("000000.json"),
        first_generation.join("000001.json"),
        roots[0].join("recording-current.json"),
        roots[1].join("recording-current.json"),
        PathBuf::from(manifests[0]["recording"]["destination"].as_str().unwrap()),
        PathBuf::from(manifests[1]["recording"]["destination"].as_str().unwrap()),
    ]));
    for (configuration, count) in [(0, 4), (1, 2)] {
        frozen.extend(freeze((1..=count).map(|slot| {
            checkpoint.join(format!(
                "campaign-completed/{configuration:06}/{slot:06}.json"
            ))
        })));
    }

    let successor = checkpoint.join("campaign-continuation/successors/000001");
    for iteration in 0..2 {
        let output = sweep(&project, Some(&provider), &["--record", "--resume"]);
        // One genuine failed grade remains failed, but all eight slots settle.
        assert!(!output.status.success());
        assert_eq!(
            (attempts(&project.0), attempts(&second)),
            (4, 4),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 8);
        unchanged(&frozen);
        if iteration == 0 {
            let completed = read_json(&successor.join("000000.json"));
            assert!(completed["recording"].is_null());
            assert!(completed["pending"].as_array().unwrap().is_empty());
            assert!(
                !roots[0]
                    .join(".asb-generations")
                    .join(completed["generation"].as_str().unwrap())
                    .exists()
            );
            let progressing = read_json(&successor.join("000001.json"));
            frozen.extend(freeze([
                successor.join("admitted.json"),
                successor.join("000000.json"),
                successor.join("000001.json"),
                PathBuf::from(progressing["recording"]["destination"].as_str().unwrap()),
                roots[1]
                    .join(".asb-generations")
                    .join(progressing["generation"].as_str().unwrap())
                    .join("recording-current.json"),
                checkpoint.join("campaign-completed/000001/000003.json"),
                checkpoint.join("campaign-completed/000001/000004.json"),
            ]));
        }
    }
    let last = read_json(&successor.join("000001.json"));
    // Config zero is completed-only: its authoritative union remains in the
    // ancestor. Config one alone acquires a new recording union.
    for (configuration, descriptor) in [(0, &manifests[0]), (1, &last)] {
        let prior_path = PathBuf::from(
            manifests[configuration]["recording"]["destination"]
                .as_str()
                .unwrap(),
        );
        let path = PathBuf::from(descriptor["recording"]["destination"].as_str().unwrap());
        let mut archive = asb_cli::cassette::Archive::open(&path, Default::default()).unwrap();
        let current = archive.manifest().clone();
        assert_eq!(current.tracks.len(), 4);
        let mut prior = asb_cli::cassette::Archive::open(&prior_path, Default::default()).unwrap();
        for track in prior.manifest().clone().tracks {
            assert_eq!(current.track(track.id).unwrap(), &track);
            assert_eq!(
                archive.read_track(track.id).unwrap(),
                prior.read_track(track.id).unwrap()
            );
        }
    }
    let report = read_json(&project.0.join("results/sweep-comparison/comparison.json"));
    let rows = report["benchmarks"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    let mut identities = BTreeSet::new();
    let mut failed = 0;
    for row in rows {
        assert!(identities.insert((
            row["execution"]["campaign"].as_str().unwrap(),
            row["execution"]["slot"].as_u64().unwrap(),
        )));
        failed += usize::from(row["metrics"]["success_rate"] == 0.0);
    }
    assert_eq!(failed, 1, "the original failed grade must remain visible");
    // Exercise the production latest-applicable resolver, not just manually
    // opening the expected archives. No provider configuration is supplied.
    let replayed = sweep(
        &project,
        None,
        &["--replay", "--repeat", "1", "--track", "max:requests"],
    );
    assert_eq!(
        (attempts(&project.0), attempts(&second)),
        (6, 6),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 8);
    let selected: Vec<_> = records(&replayed.stdout)
        .into_iter()
        .filter(|row| row["type"] == "execution-plan")
        .map(|row| read_json(Path::new(row["path"].as_str().unwrap())))
        .collect();
    assert_eq!(selected.len(), 2);
    assert_eq!(
        selected[0]["replay_source"],
        manifests[0]["recording"]["destination"]
    );
    assert_eq!(
        selected[1]["replay_source"],
        last["recording"]["destination"]
    );
    drop(std::mem::ManuallyDrop::into_inner(project));
}
