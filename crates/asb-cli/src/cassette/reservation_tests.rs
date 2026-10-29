//! Capacity contracts against actual wire bytes, journals and ZIP publications.
use super::super::serialization::encoded_size;
use super::*;

fn shape(identity: &Compatibility) -> CompatibilityShape<'_> {
    CompatibilityShape {
        agent: &identity.agent,
        provider: &identity.provider,
        model: &identity.model,
        workload: &identity.workload,
        task: &identity.task,
    }
}

#[test]
fn shared_wire_projections_preserve_exact_existing_bytes_and_optional_fields() {
    let fixture = Fixture::new();
    for (dataset, has_protocol) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut value = manifest(1);
        value.compatibility.model = "fixture/\"quoted\\模型".into();
        value.compatibility.dataset_sha256 = dataset.then(|| digest(b"dataset"));
        value.compatibility.protocol_sha256 = has_protocol.then(|| digest(b"protocol"));
        let c = &value.compatibility;
        let encoded = |text: &str| serde_json::to_string(text).unwrap();
        let protocol = c
            .protocol_sha256
            .as_ref()
            .map(|digest| format!(",\"protocol_sha256\":{}", encoded(digest)))
            .unwrap_or_default();
        let compatibility = format!(
            "{{\"agent\":{},\"provider\":{},\"model\":{},\"workload\":{},\"task\":{},\"dataset_sha256\":{},\"prompt_sha256\":{},\"material_sha256\":{}{protocol}}}",
            encoded(&c.agent),
            encoded(&c.provider),
            encoded(&c.model),
            encoded(&c.workload),
            encoded(&c.task),
            serde_json::to_string(&c.dataset_sha256).unwrap(),
            encoded(&c.prompt_sha256),
            encoded(&c.material_sha256)
        );
        assert_eq!(serde_json::to_string(c).unwrap(), compatibility);
        let expected = format!(
            "{{\"schema_version\":{},\"recording_id\":{},\"compatibility\":{compatibility},\"concurrency\":1,\"tracks\":{}}}",
            value.schema_version,
            encoded(&value.recording_id),
            serde_json::to_string(&value.tracks).unwrap()
        );
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(serde_json::from_slice::<Manifest>(&bytes).unwrap(), value);
        let proof = TrackEvidence {
            origin_sha256: None,
            schema_version: 1,
            destination: fixture.0.join("trials.zip"),
            recording_id: value.recording_id.clone(),
            compatibility: c.clone(),
            concurrency: 1,
            track: value.tracks[0].clone(),
        };
        let expected = format!(
            "{{\"schema_version\":1,\"destination\":{},\"recording_id\":{},\"compatibility\":{compatibility},\"concurrency\":1,\"track\":{}}}",
            serde_json::to_string(&proof.destination).unwrap(),
            encoded(&proof.recording_id),
            serde_json::to_string(&proof.track).unwrap()
        );
        assert_eq!(serde_json::to_vec(&proof).unwrap(), expected.as_bytes());
        assert_eq!(
            serde_json::from_str::<TrackEvidence>(&expected).unwrap(),
            proof
        );
        let planned = encoded_size(&shape(c).wire().unwrap(), u64::MAX).unwrap();
        let actual = encoded_size(c, u64::MAX).unwrap();
        assert!(planned >= actual);
        assert_eq!(planned == actual, dataset && has_protocol);
    }
}

#[test]
fn planned_capacity_bounds_maximum_statistics_real_journals_and_published_proofs() {
    for (repeat, concurrency) in [(1, 1), (3, 2), (2, 16)] {
        for optional in [false, true] {
            let fixture = Fixture::new();
            let destination = fixture.0.join("trials.zip");
            let mut identity = manifest(0);
            identity.concurrency = concurrency;
            identity.compatibility.model = "escaped/\"model\\模型".into();
            identity.compatibility.dataset_sha256 = optional.then(|| digest(b"dataset"));
            identity.compatibility.protocol_sha256 = optional.then(|| digest(b"protocol"));
            let capacity = RecordingReservation::for_plan(
                shape(&identity.compatibility),
                &identity.recording_id,
                &destination,
                repeat,
                concurrency,
                Limits::default(),
            )
            .unwrap();
            let mut journal =
                RecordingJournal::create(&destination, identity, repeat, Limits::default())
                    .unwrap();
            let mut proofs = Vec::new();
            for batch in 1..=repeat {
                for replica in 1..=concurrency {
                    let id = (batch - 1) * concurrency + replica;
                    journal.begin(batch, replica).unwrap();
                    let mut track = manifest(1).tracks.remove(0);
                    track.id = id;
                    track.batch = batch;
                    track.replica = replica;
                    track.complete = false;
                    track.process_success = Some(false);
                    track.workload_success = Some(false);
                    track.statistics = Statistics {
                        tokens: Some(u64::MAX),
                        duration_ms: Some(u64::MAX),
                        requests: Some(u64::MAX),
                        steps: Some(u64::MAX),
                    };
                    journal.commit(track.clone(), &mut &b"{}\n"[..]).unwrap();
                    proofs.push(journal.track_evidence(&track).unwrap());
                }
            }
            let root = journal.partial_directory().to_path_buf();
            let mut actual_journal = fs::metadata(root.join("intent.json")).unwrap().len();
            for id in 1..=repeat * concurrency {
                for kind in ["started", "committed"] {
                    actual_journal += fs::metadata(root.join(format!("{kind}-{id}.json")))
                        .unwrap()
                        .len();
                }
            }
            assert!(actual_journal <= capacity.journal_bytes);
            journal.finalize().unwrap();
            let archive = Archive::open(&destination, Limits::default()).unwrap();
            let (directory, manifest) = archive.metadata_bytes();
            assert!(
                directory <= capacity.directory_bytes,
                "{directory} > {}",
                capacity.directory_bytes
            );
            assert!(manifest <= capacity.manifest_bytes);
            assert!(
                fs::metadata(root.join("published.json")).unwrap().len()
                    <= capacity.publication_bytes
            );
            let refs: u64 = proofs
                .iter()
                .map(|proof| encoded_size(proof, u64::MAX).unwrap())
                .sum();
            assert!(refs <= capacity.reference_bytes);
            assert_eq!(capacity.references, repeat * concurrency);
            let exact = ProofLimits {
                max_references: capacity.references as usize,
                max_reference_bytes: capacity.reference_bytes,
                max_metadata_bytes: capacity.journal_bytes
                    + capacity.manifest_bytes
                    + capacity.publication_bytes,
                max_directory_bytes: capacity.directory_bytes,
                ..ProofLimits::default()
            };
            capacity.validate(exact).unwrap();
            TrackEvidence::verify_batch(&proofs, exact).unwrap();
        }
    }
}

#[test]
fn metadata_capacity_is_checked_before_any_files_and_has_exact_aggregate_boundaries() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("absent/quoted-\"\\模型.zip");
    let identity = manifest(0);
    let capacity = RecordingReservation::for_plan(
        shape(&identity.compatibility),
        &identity.recording_id,
        &destination,
        3,
        2,
        Limits::default(),
    )
    .unwrap();
    assert!(!destination.parent().unwrap().exists());
    let exact = ProofLimits {
        max_references: capacity.references as usize,
        max_reference_bytes: capacity.reference_bytes,
        max_metadata_bytes: capacity.journal_bytes
            + capacity.manifest_bytes
            + capacity.publication_bytes,
        max_directory_bytes: capacity.directory_bytes,
        ..ProofLimits::default()
    };
    capacity.validate(exact).unwrap();
    for limits in [
        ProofLimits {
            max_references: exact.max_references - 1,
            ..exact
        },
        ProofLimits {
            max_reference_bytes: exact.max_reference_bytes - 1,
            ..exact
        },
        ProofLimits {
            max_metadata_bytes: exact.max_metadata_bytes - 1,
            ..exact
        },
        ProofLimits {
            max_directory_bytes: exact.max_directory_bytes - 1,
            ..exact
        },
    ] {
        assert!(capacity.validate(limits).is_err());
    }
    assert!(
        capacity
            .checked_add(&capacity)
            .unwrap()
            .validate(exact)
            .is_err()
    );
    assert_eq!(
        RecordingReservation::default()
            .checked_add(&capacity)
            .unwrap(),
        capacity
    );
    for (repeat, concurrency) in [(0, 1), (1, 0), (1, 65), (u64::MAX, 2), (65_534, 1)] {
        assert!(
            RecordingReservation::for_plan(
                shape(&identity.compatibility),
                &identity.recording_id,
                &destination,
                repeat,
                concurrency,
                Limits::default()
            )
            .is_err()
        );
    }
    assert!(!destination.parent().unwrap().exists());
    assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
}

#[test]
fn directory_capacity_covers_digit_width_transitions_and_the_full_zip32_population() {
    for count in [9, 10, 99, 100] {
        let fixture = Fixture::new();
        let value = manifest(count);
        let names: Vec<_> = value.tracks.iter().map(Track::entry_name).collect();
        let payloads: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), &b"{}\n"[..]))
            .collect();
        let path = fixture.zip(&value, &payloads);
        let actual = Archive::open(&path, Limits::default())
            .unwrap()
            .metadata_bytes()
            .0;
        let reserved = super::super::metadata::recording_directory_size(count).unwrap();
        assert!(actual <= reserved, "count={count}, {actual} > {reserved}");
    }
    let fixture = Fixture::new();
    let value = manifest(0);
    let limits = Limits {
        max_manifest_bytes: 128 * 1024 * 1024,
        ..Limits::default()
    };
    let capacity = RecordingReservation::for_plan(
        shape(&value.compatibility),
        &value.recording_id,
        &fixture.0.join("max.zip"),
        65_533,
        1,
        limits,
    )
    .unwrap();
    assert_eq!(capacity.references, 65_533);
    capacity
        .validate(ProofLimits {
            archive: limits,
            ..ProofLimits::default()
        })
        .unwrap();
    assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
}

#[test]
fn capacity_rejects_overflow_unknown_schema_and_invalid_shapes_without_io() {
    let fixture = Fixture::new();
    let value = manifest(0);
    let one = RecordingReservation {
        reference_bytes: 1,
        ..RecordingReservation::default()
    };
    let overflow = RecordingReservation {
        reference_bytes: u64::MAX,
        ..RecordingReservation::default()
    };
    assert!(overflow.checked_add(&one).is_err());
    let unknown = RecordingReservation {
        schema_version: 2,
        ..RecordingReservation::default()
    };
    assert!(unknown.validate(ProofLimits::default()).is_err());
    assert!(one.checked_add(&unknown).is_err());
    for id in ["", "invalid id", "invalid/id"] {
        assert!(
            RecordingReservation::for_plan(
                shape(&value.compatibility),
                id,
                &fixture.0.join("a.zip"),
                1,
                1,
                Limits::default()
            )
            .is_err()
        );
    }
    assert!(
        RecordingReservation::for_plan(
            shape(&value.compatibility),
            &value.recording_id,
            PathBuf::from("relative.zip").as_path(),
            1,
            1,
            Limits::default()
        )
        .is_err()
    );
    for model in [String::new(), "control\nvalue".into(), "x".repeat(4097)] {
        let mut invalid = shape(&value.compatibility);
        invalid.model = &model;
        assert!(
            RecordingReservation::for_plan(
                invalid,
                &value.recording_id,
                &fixture.0.join("a.zip"),
                1,
                1,
                Limits::default()
            )
            .is_err()
        );
    }
    assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
}
