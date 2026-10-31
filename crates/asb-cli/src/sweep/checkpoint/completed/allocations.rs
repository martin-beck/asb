//! Bind an allocated workload environment to its pre-existing sweep intent.
use super::*;
use crate::runner::replica::Identity;
use crate::workloads::recovery::AttemptReference;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Allocation {
    schema_version: u32,
    scope: String,
    configuration: usize,
    replica: Identity,
    input_digest: String,
    intent_id: String,
    reference: AttemptReference,
}

pub(super) fn path(inputs: &Inputs, index: usize, slot: usize) -> PathBuf {
    inputs
        .directory
        .join("allocated")
        .join(format!("{index:06}"))
        .join(format!("{slot:03}.json"))
}

pub(super) fn record(
    observer: &replicas::Observer<'_>,
    replica: Identity,
    reference: &AttemptReference,
) -> Result<(), Error> {
    if !replica.valid()
        || replica.total != observer.entry.configuration.concurrency
        || !reference.matches_selection(&observer.entry.selection)
    {
        return Err(invalid(
            "allocated workload does not match the selected replica",
        ));
    }
    let (root, _) = crate::project::load(&observer.directory.join("benchmark.toml"))?;
    reference.validate_location(&root)?;
    let intent: Value = read_private(&replicas::start_path(
        observer.inputs,
        observer.index,
        replica.index,
    ))?;
    if intent["replica"]
        != serde_json::to_value(replica).map_err(|_| invalid("cannot encode replica"))?
        || intent["configuration"].as_u64() != Some(observer.index as u64)
        || intent["input_digest"] != observer.inputs.initial.digest
    {
        return Err(invalid(
            "allocated workload has no matching pre-start intent",
        ));
    }
    let intent_id = intent["intent_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or_else(|| invalid("allocated workload has no bounded intent identity"))?;
    publish(
        &path(observer.inputs, observer.index, replica.index),
        &Allocation {
            schema_version: 1,
            scope: "allocated-workload-attempt".into(),
            configuration: observer.index,
            replica,
            input_digest: observer.inputs.initial.digest.clone(),
            intent_id: intent_id.into(),
            reference: reference.clone(),
        },
    )
}

pub(super) fn validate(
    path: &Path,
    intent_path: &Path,
    directory: &Path,
    entry: &PlannedBenchmark,
    replica: Identity,
) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error(error)),
        Ok(_) => (),
    }
    let allocation: Allocation = read_private(path)?;
    let intent: Value = read_private(intent_path)?;
    if allocation.schema_version != 1
        || allocation.scope != "allocated-workload-attempt"
        || allocation.replica != replica
        || !allocation.reference.matches_selection(&entry.selection)
        || intent["configuration"].as_u64() != Some(allocation.configuration as u64)
        || intent["replica"]
            != serde_json::to_value(replica).map_err(|_| invalid("cannot encode replica"))?
        || intent["input_digest"] != allocation.input_digest
        || intent["intent_id"] != allocation.intent_id
    {
        return Err(invalid(
            "allocated workload reference does not match its original start intent",
        ));
    }
    let (root, _) = crate::project::load(&directory.join("benchmark.toml"))?;
    allocation.reference.validate_location(&root)?;
    Ok(true)
}

pub(super) fn reference(
    inputs: &Inputs,
    index: usize,
    replica: Identity,
    directory: &Path,
    entry: &PlannedBenchmark,
) -> Result<AttemptReference, Error> {
    let path = path(inputs, index, replica.index);
    if !validate(
        &path,
        &replicas::start_path(inputs, index, replica.index),
        directory,
        entry,
        replica,
    )? {
        return Err(invalid(
            "retained outcome has no original workload allocation",
        ));
    }
    Ok(read_private::<Allocation>(&path)?.reference)
}

/// Report-only recovery binds original selection and start metadata, not the
/// currently running executable. It cannot create execution resume evidence.
pub(in crate::sweep) fn reporting_reference(
    checkpoint: &Path,
    index: usize,
    replica: Identity,
    directory: &Path,
    entry: &PlannedBenchmark,
) -> Result<AttemptReference, Error> {
    let relative = PathBuf::from(format!("{index:06}/{:03}.json", replica.index));
    let path = checkpoint.join("allocated").join(&relative);
    let start = checkpoint.join("started").join(relative);
    if !validate(&path, &start, directory, entry, replica)? {
        return Err(invalid("grade import has no original workload allocation"));
    }
    let allocation: Allocation = read_private(&path)?;
    let intent: Value = read_private(&start)?;
    // Start intents are durable records.  Compare their typed, canonical
    // representation rather than raw JSON so a newly introduced defaulted
    // configuration field does not invalidate an otherwise identical older
    // intent.  Deserialization remains strict and rejects unknown fields.
    let recorded_entry: PlannedBenchmark = serde_json::from_value(
        intent
            .get("entry")
            .cloned()
            .ok_or_else(|| invalid("grade import has no original configuration"))?,
    )
    .map_err(|_| invalid("grade import has invalid original configuration"))?;
    let recorded_entry = serde_json::to_value(recorded_entry)
        .map_err(|_| invalid("cannot encode original configuration"))?;
    if !replica.valid()
        || replica.total != entry.configuration.concurrency
        || allocation.configuration != index
        || allocation.input_digest.len() != 64
        || !allocation
            .input_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || allocation.intent_id.is_empty()
        || allocation.intent_id.len() > 128
        || intent["schema_version"] != 1
        || intent["scope"] != "replica-execution-intent"
        || recorded_entry
            != serde_json::to_value(entry).map_err(|_| invalid("cannot encode selection"))?
    {
        return Err(invalid(
            "grade import differs from its original configuration or start intent",
        ));
    }
    Ok(allocation.reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::replica::CompletionObserver;
    use std::sync::Mutex;

    #[test]
    fn allocated_locations_bind_exact_selection_slot_and_start_intent() {
        let fixture = super::super::tests::Fixture::new(false);
        std::fs::create_dir_all(fixture.root.join(".asb/attempts")).unwrap();
        std::fs::write(
            fixture.root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let observer = replicas::Observer {
            inputs: &fixture.inputs,
            index: 0,
            directory: &fixture.directory,
            entry: &fixture.entry,
            publication: Mutex::new(()),
        };
        let replica = Identity { index: 1, total: 2 };
        observer.reserve().unwrap();
        let reference = AttemptReference {
            schema_version: 1,
            preparation_id: "abc-123".into(),
            workload: fixture.entry.selection.workload.clone(),
            task_id: fixture.entry.selection.task_id.clone(),
            dataset: fixture.entry.selection.dataset.clone().unwrap(),
            artifacts: fixture.root.join(".asb/attempts/abc-123"),
        };
        assert!(
            observer.allocated(replica, &reference).is_err(),
            "allocation cannot precede start intent"
        );
        observer.starting(replica, None).unwrap();
        observer.allocated(replica, &reference).unwrap();
        let saved = path(&fixture.inputs, 0, 1);
        let intent = replicas::start_path(&fixture.inputs, 0, 1);
        assert!(validate(&saved, &intent, &fixture.directory, &fixture.entry, replica).unwrap());
        let original = std::fs::read(&saved).unwrap();
        assert!(
            observer.allocated(replica, &reference).is_err(),
            "allocated history is immutable"
        );
        assert_eq!(std::fs::read(&saved).unwrap(), original);
        let mut other = reference.clone();
        other.dataset.revision = "different".into();
        assert!(
            observer
                .allocated(Identity { index: 2, total: 2 }, &other)
                .is_err()
        );
        let mut record: Value = serde_json::from_slice(&original).unwrap();
        for field in ["intent_id", "input_digest"] {
            record[field] = json!("changed");
            std::fs::write(&saved, serde_json::to_vec(&record).unwrap()).unwrap();
            assert!(
                validate(&saved, &intent, &fixture.directory, &fixture.entry, replica).is_err()
            );
            record = serde_json::from_slice(&original).unwrap();
        }
        std::fs::write(&saved, original).unwrap();
        assert!(validate(&saved, &intent, &fixture.directory, &fixture.entry, replica).unwrap());
    }
}
