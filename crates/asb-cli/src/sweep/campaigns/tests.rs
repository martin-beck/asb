use super::*;
use std::fs;

struct Fixture(PathBuf);

impl Fixture {
    fn new(configurations: usize, concurrency: usize, bytes: u64) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sweep-input-admission")
            .join(crate::workloads::TaskIdentity::local("metadata", None).attempt_id);
        fs::create_dir_all(root.join(".asb")).unwrap();
        let root = root.canonicalize().unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let configuration: crate::benchmark::BenchmarkConfig = toml::from_str(&format!("agent='pi'\nprovider='local'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency={concurrency}\n")).unwrap();
        let mut references = Vec::new();
        for index in 0..configurations {
            let reference = format!("benchmark-{index}.toml");
            fs::write(
                root.join(&reference),
                toml::to_string(&configuration).unwrap(),
            )
            .unwrap();
            let directory = root
                .join("cassettes")
                .join(execution::result_name(index, Path::new(&reference)))
                .join("cassettes");
            fs::create_dir_all(&directory).unwrap();
            let base = directory.join(super::super::cassette_filename(&configuration));
            for replica in 1..=concurrency {
                let path = if concurrency == 1 {
                    base.clone()
                } else {
                    crate::benchmark_paths::replica_path(&base, replica)
                };
                fs::File::create(path).unwrap().set_len(bytes).unwrap();
            }
            references.push(reference);
        }
        fs::write(
            root.join("sweep.toml"),
            toml::to_string(&super::super::SweepFile {
                configs: references,
                resources: None,
            })
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("sweep-results")).unwrap();
        fs::create_dir_all(root.join("results/sweep-comparison")).unwrap();
        fs::write(root.join("sweep-results/plan.json"), b"original index").unwrap();
        fs::write(
            root.join("results/sweep-comparison/report.html"),
            b"original report",
        )
        .unwrap();
        Self(root)
    }

    fn refuse(&self, selector: bool, message: &str) {
        crate::cassette::take_observation_count();
        runner::repeated::take_legacy_observation_count();
        let cassettes = self.0.join("cassettes");
        let mut output = Vec::new();
        let failure = super::super::run_with_options(
            self.0.join("sweep.toml").to_str().unwrap(),
            Options {
                mode: Some(tape::Mode::Replay),
                repeat: Some(2),
                cassette_root: Some(&cassettes),
                track: selector.then_some(crate::cassette::Selector::Id(1)),
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(failure.to_string().contains(message), "{failure}");
        assert_eq!(crate::cassette::take_observation_count(), 0);
        assert_eq!(runner::repeated::take_legacy_observation_count(), 0);
        assert_eq!(
            fs::read(self.0.join("sweep-results/plan.json")).unwrap(),
            b"original index"
        );
        assert_eq!(
            fs::read(self.0.join("results/sweep-comparison/report.html")).unwrap(),
            b"original report"
        );
        assert!(!self.0.join("sweep-history").exists());
        assert!(output.is_empty());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn explicit_replay_root_prefers_fresh_published_zip_layout() {
    let fixture = Fixture::new(1, 1, 1);
    let entry = execution::result_name(0, Path::new("benchmark-0.toml"));
    let root = fixture.0.join("cassettes/sweep-fresh-recording");
    let bundle = root.join(&entry).join("recording.asb.zip");
    fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    // Presence is deliberately separate from archive validation.  A later
    // admission step validates this selected ZIP; this test only locks the
    // record/replay layout resolution contract.
    fs::write(&bundle, b"PK").unwrap();

    assert_eq!(super::replay_bundle(&root, &entry).unwrap(), Some(bundle));
}

#[test]
fn whole_sweep_admission_bounds_legacy_hash_work_and_real_descriptors() {
    for selector in [false, true] {
        // Sparse files consume metadata only: a regression would actually hash
        // the first configuration before discovering the second exceeds budget.
        Fixture::new(2, 17, 64 * 1024 * 1024).refuse(selector, "aggregate 2 GiB");
        // Numeric selection chooses one track but retains all 64 source files;
        // the descriptor budget must not count just the five chosen tracks.
        Fixture::new(5, 64, 1).refuse(selector, "descriptors exceed 256");
    }
}

#[test]
fn late_invalid_source_metadata_precedes_any_earlier_payload_hash() {
    for selector in [false, true] {
        let fixture = Fixture::new(2, 1, 3);
        let source = fixture
            .0
            .join("cassettes/001-benchmark-1/cassettes/benchmark-pi-bug-fix-fixture.jsonl");
        fs::File::options()
            .write(true)
            .open(source)
            .unwrap()
            .set_len(256 * 1024 * 1024 + 1)
            .unwrap();
        fixture.refuse(selector, if selector { "byte limit" } else { "64 MiB" });
    }
}
