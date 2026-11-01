//! Actual backend publication, no fabricated agent outcomes or paid requests.
use super::*;

fn publish(
    root: &Path,
    descriptor: &Descriptor,
    binding: &Binding,
    publication: &Publication,
) -> Result<(), Error> {
    super::publish(
        root,
        descriptor,
        binding,
        publication,
        Path::new("recording-current.json"),
    )
}

fn prepare_publication(
    root: &Path,
    descriptor: &Descriptor,
    binding: &Binding,
    publication: &Publication,
) -> Result<PublicationPlan, Error> {
    super::prepare_publication(
        root,
        descriptor,
        binding,
        publication,
        Path::new("recording-current.json"),
    )
}

#[test]
fn successor_publication_keeps_the_original_root_ledger_immutable() {
    let fixture = Fixture::new();
    let (descriptor, binding, publication) = fixture.publication();
    let original = b"original immutable publication ledger";
    fs::write(fixture.0.join("recording-current.json"), original).unwrap();
    let relative = PathBuf::from(".asb-generations/111/recording-current.json");
    super::publish(&fixture.0, &descriptor, &binding, &publication, &relative).unwrap();
    let bytes = fs::read(fixture.0.join(&relative)).unwrap();
    super::publish(&fixture.0, &descriptor, &binding, &publication, &relative).unwrap();
    assert_eq!(fs::read(fixture.0.join(&relative)).unwrap(), bytes);
    assert_eq!(
        fs::read(fixture.0.join("recording-current.json")).unwrap(),
        original
    );
    assert!(
        super::prepare_publication(
            &fixture.0,
            &descriptor,
            &binding,
            &publication,
            Path::new("foreign-current.json")
        )
        .is_err()
    );
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/union-publication-tests")
            .join(crate::workloads::TaskIdentity::local("union-test", None).attempt_id);
        fs::create_dir_all(root.join(".asb-generations/111")).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn publication(&self) -> (Descriptor, Binding, Publication) {
        let compatibility = Compatibility {
            agent: "pi".into(),
            provider: "local".into(),
            model: "fixture".into(),
            workload: "bug-fix".into(),
            task: "bug-fix".into(),
            dataset_sha256: None,
            prompt_sha256: crate::cassette::digest(b"prompt"),
            material_sha256: crate::cassette::digest(b"material"),
            protocol_sha256: None,
        };
        let origins = OriginPlan {
            schema_version: 1,
            population: 1,
            sources: Vec::new(),
            imports: Vec::new(),
        };
        let destination = self.0.join("union.zip");
        let reservation = RecordingReservation::for_origin_plan(
            crate::cassette::CompatibilityShape {
                agent: &compatibility.agent,
                provider: &compatibility.provider,
                model: &compatibility.model,
                workload: &compatibility.workload,
                task: &compatibility.task,
            },
            "111-0",
            &destination,
            1,
            &origins,
            Limits::default(),
        )
        .unwrap();
        let mut journal = RecordingJournal::create(
            &destination,
            Manifest {
                schema_version: 1,
                recording_id: "111-0".into(),
                compatibility,
                concurrency: 1,
                tracks: Vec::new(),
                provenance: Some(origins.clone()),
            },
            1,
            Limits::default(),
        )
        .unwrap();
        journal.begin(1, 1).unwrap();
        journal
            .commit(
                Track {
                    recorder: None,
                    id: 1,
                    batch: 1,
                    replica: 1,
                    complete: false,
                    process_success: None,
                    workload_success: None,
                    bytes: 3,
                    sha256: crate::cassette::digest(b"{}\n"),
                    statistics: Statistics::default(),
                    native: None,
                    artifacts: Vec::new(),
                },
                &mut &b"{}\n"[..],
            )
            .unwrap();
        let publication = journal.finalize().unwrap();
        let descriptor = Descriptor {
            physical_id: "111-0".into(),
            destination,
            origins,
            sources: Vec::new(),
            reservation,
        };
        // Synthetic passive references qualify only the publication primitive.
        // Public CLI fixtures separately prove real global/config admission.
        let binding = Binding {
            reference: crate::runner::campaign::generation::StartReference {
                generation: "111".into(),
                configuration: 0,
                manifest: crate::runner::campaign::generation::Blob::of(b"manifest"),
                admission: crate::runner::campaign::generation::Blob::of(b"admission"),
            },
            origins_sha256: descriptor.origins.commitment().unwrap(),
        };
        (descriptor, binding, publication)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        PUBLICATION_FAULT.set(None);
        PUBLICATION_POINTS.with_borrow_mut(Vec::clear);
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn linked_receipt_and_ledger_require_resync_on_explicit_recovery() {
    for target in ["recording-published.json", "recording-current.json"] {
        let fixture = Fixture::new();
        let (descriptor, binding, publication) = fixture.publication();
        PUBLICATION_FAULT.set(Some((target, "after-link")));
        assert!(publish(&fixture.0, &descriptor, &binding, &publication).is_err());
        let bytes = fs::read(&publication.destination).unwrap();
        PUBLICATION_POINTS.with_borrow_mut(Vec::clear);
        PUBLICATION_FAULT.set(Some((target, "existing-before-directory-sync")));
        assert!(publish(&fixture.0, &descriptor, &binding, &publication).is_err());
        assert!(
            PUBLICATION_POINTS.with_borrow(
                |points| points.contains(&format!("{target}:existing-before-file-sync"))
            )
        );
        publish(&fixture.0, &descriptor, &binding, &publication).unwrap();
        let ledger = fs::read(fixture.0.join("recording-current.json")).unwrap();
        publish(&fixture.0, &descriptor, &binding, &publication).unwrap();
        assert_eq!(
            fs::read(fixture.0.join("recording-current.json")).unwrap(),
            ledger
        );
        assert_eq!(fs::read(&publication.destination).unwrap(), bytes);
    }
}

#[test]
fn late_metadata_conflict_is_rejected_before_any_earlier_ledger_write() {
    let first = Fixture::new();
    let late = Fixture::new();
    let (a, a_binding, a_publication) = first.publication();
    let (b, b_binding, b_publication) = late.publication();
    fs::write(
        late.0.join("recording-current.json"),
        b"foreign retained metadata",
    )
    .unwrap();
    let plans = [
        prepare_publication(&first.0, &a, &a_binding, &a_publication).unwrap(),
        prepare_publication(&late.0, &b, &b_binding, &b_publication).unwrap(),
    ];
    assert!(
        plans
            .iter()
            .try_for_each(PublicationPlan::preflight)
            .is_err()
    );
    assert!(!first.0.join("recording-current.json").exists());
    assert!(
        !first
            .0
            .join(".asb-generations/111/recording-published.json")
            .exists()
    );
    assert_eq!(
        fs::read(late.0.join("recording-current.json")).unwrap(),
        b"foreign retained metadata"
    );
}

#[test]
fn exact_archive_reader_rejects_same_length_changes_without_decoding_payloads() {
    let fixture = Fixture::new();
    let (_, _, publication) = fixture.publication();
    let bytes = fs::read(&publication.destination).unwrap();
    let mut archive = Archive::open_verified(
        &publication.destination,
        publication.archive_bytes,
        &publication.archive_sha256,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(archive.read_track(1).unwrap(), b"{}\n");
    let mut changed = bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    fs::write(&publication.destination, changed).unwrap();
    assert!(
        Archive::open_verified(
            &publication.destination,
            publication.archive_bytes,
            &publication.archive_sha256,
            Limits::default()
        )
        .is_err()
    );
    fs::write(&publication.destination, &bytes).unwrap();
    assert!(
        Archive::open_verified(
            &publication.destination,
            publication.archive_bytes - 1,
            &publication.archive_sha256,
            Limits::default()
        )
        .is_err()
    );
}
