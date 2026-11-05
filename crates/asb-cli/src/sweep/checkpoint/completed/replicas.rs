//! Immutable evidence for a single replica, independent of unfinished siblings.
use super::*;
use crate::runner::replica::{CompletionObserver, Identity};
use std::sync::Mutex;

const REPLICA_SCOPE: &str = "completed-replica-declared-evidence";

pub(in crate::sweep) struct Observer<'a> {
    pub inputs: &'a Inputs,
    pub index: usize,
    pub directory: &'a Path,
    pub entry: &'a PlannedBenchmark,
    pub publication: Mutex<()>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    #[serde(default)]
    reconciled: bool,
    schema_version: u32,
    scope: String,
    configuration: usize,
    replica: Identity,
    input_digest: String,
    outcome: PathBuf,
    roots: Vec<Root>,
    fingerprint: Fingerprint,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    schema_version: u32,
    scope: String,
    configuration: usize,
    replica: Identity,
    input_digest: String,
    entry: Value,
    reservation_id: String,
}

impl Observer<'_> {
    /// Reserve every slot before any execution begins. Continuation never
    /// fabricates a missing reservation for an existing partial group.
    pub(in crate::sweep) fn reserve(&self) -> Result<(), Error> {
        for index in 1..=self.entry.configuration.concurrency {
            let replica = Identity {
                index,
                total: self.entry.configuration.concurrency,
            };
            publish(
                &pending_path(self.inputs, self.index, index),
                &Reservation {
                    schema_version: 1,
                    scope: "reserved-replica-slot".into(),
                    configuration: self.index,
                    replica,
                    input_digest: self.inputs.initial.digest.clone(),
                    entry: serde_json::to_value(self.entry)
                        .map_err(|_| invalid("cannot encode slot reservation"))?,
                    reservation_id: TaskIdentity::local("replica-reservation", None).attempt_id,
                },
            )?;
        }
        Ok(())
    }
}

impl CompletionObserver for Observer<'_> {
    fn evidence_destination(&self, replica: Identity) -> Option<PathBuf> {
        Some(path(self.inputs, self.index, replica.index))
    }
    fn allocated(
        &self,
        replica: Identity,
        reference: &crate::workloads::recovery::AttemptReference,
    ) -> Result<(), Error> {
        super::allocations::record(self, replica, reference)
    }
    fn starting(
        &self,
        replica: Identity,
        cohort: Option<&crate::measurements::ExecutionCohort>,
    ) -> Result<(), Error> {
        if !replica.valid() || replica.total != self.entry.configuration.concurrency {
            return Err(invalid("execution intent does not match its selected slot"));
        }
        let reservation = self.inputs.reservation(self.index, replica, self.entry)?;
        publish(
            &start_path(self.inputs, self.index, replica.index),
            &json!({
                "schema_version":1,"scope":"replica-execution-intent",
                "configuration":self.index,"replica":replica,"input_digest":self.inputs.initial.digest,
                "entry":self.entry,"owner_pid":std::process::id(),
                "intent_id":TaskIdentity::local("replica-start",None).attempt_id,
                "reservation_id":reservation.reservation_id,
                "execution_cohort":cohort,
            }),
        )?;
        let pending = pending_path(self.inputs, self.index, replica.index);
        std::fs::remove_file(&pending).map_err(io_error)?;
        std::fs::File::open(pending.parent().expect("reservation parent"))
            .and_then(|file| file.sync_all())
            .map_err(io_error)
    }
    fn completed(&self, replica: Identity, record: &Path) -> Result<(), Error> {
        // Do not launch a hasher per simultaneously exiting sibling. The
        // execution itself has already ended; only evidence publication waits.
        let _publication = self
            .publication
            .lock()
            .map_err(|_| invalid("replica evidence publication failed"))?;
        let outcome = std::path::absolute(record).map_err(io_error)?;
        let (roots, _) = state(
            self.directory,
            self.entry,
            replica,
            &outcome,
            &start_path(self.inputs, self.index, replica.index),
            &super::allocations::path(self.inputs, self.index, replica.index),
        )?;
        let fingerprint = self.inputs.snapshotter.fingerprint(
            &roots,
            &context(
                self.index,
                replica,
                self.entry,
                &self.inputs.initial.digest,
                false,
            ),
            false,
            &[],
        )?;
        publish(
            &path(self.inputs, self.index, replica.index),
            &Receipt {
                reconciled: false,
                schema_version: 1,
                scope: REPLICA_SCOPE.into(),
                configuration: self.index,
                replica,
                input_digest: self.inputs.initial.digest.clone(),
                outcome,
                roots,
                fingerprint,
            },
        )
    }
}

impl Inputs {
    pub(super) fn reconciled_replica(&self, index: usize, slot: usize) -> Result<bool, Error> {
        let receipt: Receipt = read_private(&path(self, index, slot))?;
        Ok(receipt.reconciled)
    }

    pub(super) fn publish_reconciled(
        &self,
        index: usize,
        replica: Identity,
        outcome: PathBuf,
        roots: Vec<Root>,
        fingerprint: Fingerprint,
    ) -> Result<(), Error> {
        publish(
            &path(self, index, replica.index),
            &Receipt {
                reconciled: true,
                schema_version: 1,
                scope: REPLICA_SCOPE.into(),
                configuration: index,
                replica,
                input_digest: self.initial.digest.clone(),
                outcome,
                roots,
                fingerprint,
            },
        )
    }

    fn reservation(
        &self,
        index: usize,
        replica: Identity,
        entry: &PlannedBenchmark,
    ) -> Result<Reservation, Error> {
        let value: Reservation = read_private(&pending_path(self, index, replica.index))?;
        if value.schema_version != 1
            || value.scope != "reserved-replica-slot"
            || value.configuration != index
            || value.replica != replica
            || !replica.valid()
            || replica.total != entry.configuration.concurrency
            || value.input_digest != self.initial.digest
            || value.entry
                != serde_json::to_value(entry)
                    .map_err(|_| invalid("cannot encode selected slot"))?
            || value.reservation_id.is_empty()
            || value.reservation_id.len() > 128
        {
            return Err(invalid(
                "pending replica reservation does not match the selected slot",
            ));
        }
        Ok(value)
    }

    pub(in crate::sweep) fn partial_replicas(
        &self,
        index: usize,
        directory: &Path,
        entry: &PlannedBenchmark,
    ) -> Result<BTreeMap<usize, Value>, Error> {
        self.partial_replicas_with(index, directory, entry, |replica| {
            self.closed_slot(index, replica, directory, entry)
        })
    }

    pub(super) fn partial_replicas_with(
        &self,
        index: usize,
        directory: &Path,
        entry: &PlannedBenchmark,
        closed: impl Fn(Identity) -> Result<Option<Value>, Error>,
    ) -> Result<BTreeMap<usize, Value>, Error> {
        let directory = std::path::absolute(directory).map_err(io_error)?;
        if !std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(invalid("partial replica results are not a real directory"));
        }
        let expected = crate::sweep::isolated_config(&entry.configuration, &directory);
        let saved: crate::benchmark::BenchmarkConfig =
            toml::from_str(&crate::read_config(&directory.join("benchmark.toml"))?)
                .map_err(|_| invalid("partial replica configuration is malformed"))?;
        if saved != expected {
            return Err(invalid("partial replica configuration changed"));
        }
        let records = directory.join(".asb-completions");
        let mut claims = BTreeMap::new();
        match std::fs::symlink_metadata(&records) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(invalid("partial outcome directory is not a real directory"));
            }
            Ok(_) => {
                for file in std::fs::read_dir(&records).map_err(io_error)? {
                    let file = file.map_err(io_error)?;
                    if claims.len() >= entry.configuration.concurrency
                        || file
                            .path()
                            .extension()
                            .is_none_or(|extension| extension != "json")
                    {
                        return Err(invalid(
                            "partial replica outcome population is excessive or unfinished",
                        ));
                    }
                    let record = ValidatedOutcome::read(&file.path(), entry)?;
                    let replica = record.replica.ok_or_else(|| {
                        invalid("partial replica outcomes require explicit slot identities")
                    })?;
                    if record.schema_version != 2
                        || claims.insert(replica.index, record.summary).is_some()
                    {
                        return Err(invalid(
                            "partial replica slot identity is missing or duplicated",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_error(error)),
        }
        let mut completed = BTreeMap::new();
        for slot in 1..=entry.configuration.concurrency {
            let replica = Identity {
                index: slot,
                total: entry.configuration.concurrency,
            };
            if let Some(summary) = closed(replica)? {
                if self.has_replica(index, slot)? || claims.contains_key(&slot) {
                    return Err(invalid(
                        "closed grade-only slot conflicts with an execution outcome",
                    ));
                }
                completed.insert(slot, summary);
                continue;
            }
            if self.has_replica(index, slot)? {
                let summary = self.verify_replica(index, replica, &directory, entry)?;
                if claims.get(&slot) != Some(&summary) {
                    return Err(invalid(
                        "replica receipt and partial outcome population disagree",
                    ));
                }
                completed.insert(slot, summary);
            } else {
                if claims.contains_key(&slot) || self.replica_started(index, slot)? {
                    return Err(invalid(
                        "an unsealed or interrupted replica group exists; an already-started slot needs owned-resource recovery before retry",
                    ));
                }
                self.reservation(index, replica, entry)?;
                let config = crate::runner::replica::configuration(&expected, replica)?;
                let workspace = PathBuf::from(config.workspace);
                let mut outputs = vec![
                    PathBuf::from(config.log_file),
                    crate::runner::replica::runtime_path(&workspace),
                    workspace,
                ];
                if let Some(settings) = config
                    .measurements
                    .as_ref()
                    .filter(|settings| settings.enabled)
                {
                    outputs.push(PathBuf::from(&settings.directory));
                }
                for output in outputs {
                    match std::fs::symlink_metadata(output) {
                        Ok(_) => {
                            return Err(invalid(
                                "a reserved replica already has attempt outputs; preserve them and recover the slot before retry",
                            ));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                        Err(error) => return Err(io_error(error)),
                    }
                }
            }
        }
        Ok(completed)
    }
    pub(in crate::sweep) fn replica_started(
        &self,
        index: usize,
        replica: usize,
    ) -> Result<bool, Error> {
        match std::fs::symlink_metadata(start_path(self, index, replica)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }
    pub(in crate::sweep) fn has_replica(
        &self,
        index: usize,
        replica: usize,
    ) -> Result<bool, Error> {
        match std::fs::symlink_metadata(path(self, index, replica)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }
    pub(in crate::sweep) fn verify_replica(
        &self,
        index: usize,
        replica: Identity,
        directory: &Path,
        entry: &PlannedBenchmark,
    ) -> Result<Value, Error> {
        let receipt: Receipt = read_private(&path(self, index, replica.index))?;
        let (roots, summary) = state(
            directory,
            entry,
            replica,
            &receipt.outcome,
            &start_path(self, index, replica.index),
            &super::allocations::path(self, index, replica.index),
        )?;
        if receipt.schema_version != 1
            || receipt.scope != REPLICA_SCOPE
            || receipt.configuration != index
            || receipt.replica != replica
            || receipt.input_digest != self.initial.digest
            || !receipt.fingerprint.valid()
            || receipt.roots != roots
        {
            return Err(invalid(
                "replica receipt does not match its selected slot or evidence",
            ));
        }
        let actual = self.snapshotter.fingerprint(
            &roots,
            &context(
                index,
                replica,
                entry,
                &self.initial.digest,
                receipt.reconciled,
            ),
            false,
            &[],
        )?;
        if actual != receipt.fingerprint {
            return Err(invalid(
                "completed replica outputs or evaluator evidence changed",
            ));
        }
        Ok(summary)
    }
}

pub(super) fn path(inputs: &Inputs, index: usize, replica: usize) -> PathBuf {
    inputs
        .directory
        .join("replicas")
        .join(format!("{index:06}"))
        .join(format!("{replica:03}.json"))
}

pub(super) fn start_path(inputs: &Inputs, index: usize, replica: usize) -> PathBuf {
    inputs
        .directory
        .join("started")
        .join(format!("{index:06}"))
        .join(format!("{replica:03}.json"))
}

fn pending_path(inputs: &Inputs, index: usize, replica: usize) -> PathBuf {
    inputs
        .directory
        .join("pending")
        .join(format!("{index:06}"))
        .join(format!("{replica:03}.json"))
}

pub(super) fn context(
    index: usize,
    replica: Identity,
    entry: &PlannedBenchmark,
    digest: &str,
    reconciled: bool,
) -> Value {
    let mut value = json!({"protocol":"asb-replica-evidence-v1","scope":REPLICA_SCOPE,
        "configuration":index,"replica":replica,"entry":entry,"input_digest":digest});
    if reconciled {
        value["reconciliation"] = json!("retained-artifact-consistency");
    }
    value
}

pub(super) fn state(
    directory: &Path,
    entry: &PlannedBenchmark,
    replica: Identity,
    record: &Path,
    intent_path: &Path,
    allocation_path: &Path,
) -> Result<(Vec<Root>, Value), Error> {
    let directory = std::path::absolute(directory).map_err(io_error)?;
    if !replica.valid()
        || replica.total != entry.configuration.concurrency
        || record.parent() != Some(directory.join(".asb-completions").as_path())
    {
        return Err(invalid(
            "replica outcome is outside the selected configuration",
        ));
    }
    let outcome = ValidatedOutcome::read(record, entry)?;
    if outcome.schema_version != 2 || outcome.replica != Some(replica) || outcome.outputs.is_empty()
    {
        return Err(invalid(
            "replica outcome has no stable slot or execution outputs",
        ));
    }
    let mut roots = vec![Root::path("replica-outcome", record.to_path_buf())];
    if super::allocations::validate(allocation_path, intent_path, &directory, entry, replica)? {
        roots.push(Root::path(
            "workload-allocation",
            allocation_path.to_path_buf(),
        ));
    }
    if !outcome.summary["execution_cohort"].is_null() {
        let intent: Value = read_private(intent_path)?;
        if intent["scope"] != "replica-execution-intent"
            || intent["replica"]
                != serde_json::to_value(replica).map_err(|_| invalid("cannot encode replica"))?
            || intent["execution_cohort"] != outcome.summary["execution_cohort"]
        {
            return Err(invalid(
                "execution cohort does not match its pre-start intent",
            ));
        }
        roots.push(Root::path(
            "replica-start-intent",
            intent_path.to_path_buf(),
        ));
    }
    roots.push(Root::path(
        "configuration",
        directory.join("benchmark.toml"),
    ));
    let mut outputs = BTreeSet::new();
    for output in &outcome.outputs {
        if output == &directory
            || !output.starts_with(&directory)
            || output
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || !outputs.insert(output.clone())
        {
            return Err(invalid(
                "replica execution outputs are shared, duplicated or outside its configuration",
            ));
        }
    }
    let isolated = crate::sweep::isolated_config(&entry.configuration, &directory);
    let config = crate::runner::replica::configuration(&isolated, replica)?;
    let workspace = PathBuf::from(&config.workspace);
    let runtime = crate::runner::replica::runtime_path(&workspace);
    // The runner intentionally excludes a settled workspace from the receipt
    // before it deletes that workspace. Failed or retained attempts must still
    // bind it, even when compact retention was requested.
    let compacted = config.retention.compact_completed_attempts
        && outcome.summary["process_success"] == true
        && outcome.summary["workload_success"] != false
        && outcome.summary.get("failure_kind") == Some(&Value::Null)
        && matches!(
            outcome.summary["workload_resource_disposition"].as_str(),
            Some("not-required" | "not-created" | "removed")
        );
    let mut expected = BTreeSet::from([PathBuf::from(&config.log_file)]);
    if !compacted {
        expected.insert(workspace);
    }
    if std::fs::symlink_metadata(&runtime).is_ok() {
        expected.insert(runtime);
    }
    if let Some(settings) = config
        .measurements
        .as_ref()
        .filter(|settings| settings.enabled)
    {
        expected.insert(PathBuf::from(&settings.directory));
    }
    if outputs != expected {
        return Err(invalid(
            "replica outputs do not match its selected workspace, log, runtime and measurements",
        ));
    }
    if entry
        .configuration
        .measurements
        .as_ref()
        .is_some_and(|settings| settings.enabled)
    {
        let name = if replica.total == 1 {
            "measurements".into()
        } else {
            format!("measurements-concurrency-{}", replica.index)
        };
        let measurements = directory.join(name);
        if !outputs.contains(&measurements)
            || !measurement_matches(
                &read_private(&measurements.join("run.json"))?,
                &outcome.summary,
            )
        {
            return Err(invalid(
                "replica measurement does not match its slot's graded outcome",
            ));
        }
    }
    for (index, output) in outputs.into_iter().enumerate() {
        roots.push(Root::path(format!("replica-output-{index}"), output));
    }
    let evidence: BTreeSet<_> = outcome
        .workload_evidence
        .into_iter()
        .chain(outcome.proxy_evidence)
        .collect();
    for (index, output) in evidence.into_iter().enumerate() {
        roots.push(Root::path(format!("replica-evidence-{index}"), output));
    }
    Ok((roots, outcome.summary))
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    fn record(fixture: &Fixture, replica: Identity) -> PathBuf {
        let path = fixture.directory.join(".asb-completions/0.json");
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let isolated =
            crate::sweep::isolated_config(&fixture.entry.configuration, &fixture.directory);
        let config = crate::runner::replica::configuration(&isolated, replica).unwrap();
        let workspace = PathBuf::from(config.workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&config.log_file, "private log sentinel").unwrap();
        record["schema_version"] = 2.into();
        record["replica"] = serde_json::to_value(replica).unwrap();
        record["outputs"] = json!([workspace, config.log_file]);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        path
    }

    #[test]
    fn a_false_graded_replica_is_sealed_before_its_sibling_and_is_independently_verified() {
        let fixture = Fixture::new(false);
        let identity = Identity { index: 1, total: 2 };
        let record = record(&fixture, identity);
        std::fs::remove_file(fixture.directory.join(".asb-completions/1.json")).unwrap();
        let observer = Observer {
            inputs: &fixture.inputs,
            index: 0,
            directory: &fixture.directory,
            entry: &fixture.entry,
            publication: Mutex::new(()),
        };
        observer.completed(identity, &record).unwrap();
        assert!(!fixture.inputs.has_replica(0, 2).unwrap());
        let verified = fixture
            .inputs
            .verify_replica(0, identity, &fixture.directory, &fixture.entry)
            .unwrap();
        assert_eq!(verified["workload_success"], false);
        assert!(
            fixture
                .inputs
                .completed(0, &fixture.directory, &fixture.entry)
                .is_err(),
            "the sibling group is not complete"
        );
        let receipt = std::fs::read(path(&fixture.inputs, 0, 1)).unwrap();
        let text = String::from_utf8(receipt.clone()).unwrap();
        assert!(!text.contains("private log sentinel"));
        assert!(!text.contains("private evaluator sentinel"));
        // A running sibling can produce its own outputs without invalidating
        // the already graded replica's immutable evidence.
        std::fs::create_dir(fixture.directory.join("workspace-concurrency-2")).unwrap();
        std::fs::write(
            fixture.directory.join("run-concurrency-2.log"),
            "sibling still working",
        )
        .unwrap();
        fixture
            .inputs
            .verify_replica(0, identity, &fixture.directory, &fixture.entry)
            .unwrap();
        assert!(
            observer.completed(identity, &record).is_err(),
            "never replace a prior replica receipt"
        );
        assert_eq!(std::fs::read(path(&fixture.inputs, 0, 1)).unwrap(), receipt);
        std::fs::write(
            fixture.directory.join("run-concurrency-1.log"),
            "edited evidence",
        )
        .unwrap();
        assert!(
            fixture
                .inputs
                .verify_replica(0, identity, &fixture.directory, &fixture.entry)
                .is_err()
        );
    }

    #[test]
    fn pending_slots_require_a_positive_reservation_and_no_started_attempt() {
        let fixture = Fixture::new(false);
        let identity = Identity { index: 1, total: 2 };
        let record = record(&fixture, identity);
        std::fs::remove_file(fixture.directory.join(".asb-completions/1.json")).unwrap();
        let config =
            crate::sweep::isolated_config(&fixture.entry.configuration, &fixture.directory);
        std::fs::write(
            fixture.directory.join("benchmark.toml"),
            toml::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        let observer = Observer {
            inputs: &fixture.inputs,
            index: 0,
            directory: &fixture.directory,
            entry: &fixture.entry,
            publication: Mutex::new(()),
        };
        observer.reserve().unwrap();
        observer.starting(identity, None).unwrap();
        observer.completed(identity, &record).unwrap();
        assert!(!pending_path(&fixture.inputs, 0, 1).exists());
        assert!(fixture.inputs.replica_started(0, 1).unwrap());
        let partial = fixture
            .inputs
            .partial_replicas(0, &fixture.directory, &fixture.entry)
            .unwrap();
        assert_eq!(partial.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(
            partial[&1]["workload_success"], false,
            "a graded failure is preserved, never retried"
        );
        assert!(pending_path(&fixture.inputs, 0, 2).exists());
        let unstarted = Identity { index: 2, total: 2 };
        observer.starting(unstarted, None).unwrap();
        assert!(!pending_path(&fixture.inputs, 0, 2).exists());
        assert!(
            observer.starting(unstarted, None).is_err(),
            "intent cannot be consumed twice"
        );
        let error = fixture
            .inputs
            .partial_replicas(0, &fixture.directory, &fixture.entry)
            .unwrap_err();
        assert!(error.to_string().contains("already-started slot"));
    }

    #[test]
    fn missing_changed_or_used_reservations_and_existing_outputs_reject_admission() {
        for case in ["missing", "changed", "output", "started-deleted"] {
            let fixture = Fixture::new(true);
            let identity = Identity { index: 1, total: 2 };
            let record = record(&fixture, identity);
            std::fs::remove_file(fixture.directory.join(".asb-completions/1.json")).unwrap();
            let config =
                crate::sweep::isolated_config(&fixture.entry.configuration, &fixture.directory);
            std::fs::write(
                fixture.directory.join("benchmark.toml"),
                toml::to_string_pretty(&config).unwrap(),
            )
            .unwrap();
            let observer = Observer {
                inputs: &fixture.inputs,
                index: 0,
                directory: &fixture.directory,
                entry: &fixture.entry,
                publication: Mutex::new(()),
            };
            observer.reserve().unwrap();
            observer.starting(identity, None).unwrap();
            observer.completed(identity, &record).unwrap();
            let pending = pending_path(&fixture.inputs, 0, 2);
            match case {
                "missing" => std::fs::remove_file(&pending).unwrap(),
                "changed" => {
                    let mut data: Value = read_private(&pending).unwrap();
                    data["input_digest"] = json!("wrong input");
                    std::fs::write(&pending, serde_json::to_vec(&data).unwrap()).unwrap();
                }
                "output" => {
                    std::fs::create_dir(fixture.directory.join("workspace-concurrency-2")).unwrap()
                }
                _ => {
                    observer
                        .starting(Identity { index: 2, total: 2 }, None)
                        .unwrap();
                    std::fs::remove_file(start_path(&fixture.inputs, 0, 2)).unwrap();
                }
            }
            assert!(
                fixture
                    .inputs
                    .partial_replicas(0, &fixture.directory, &fixture.entry)
                    .is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn compacted_success_seals_without_workspace_but_failure_cannot_omit_it() {
        for success in [true, false] {
            let mut fixture = Fixture::new(success);
            fixture
                .entry
                .configuration
                .retention
                .compact_completed_attempts = true;
            let replica = Identity { index: 1, total: 2 };
            let path = record(&fixture, replica);
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["outputs"].as_array_mut().unwrap().remove(0);
            value["workload_resource_disposition"] = json!("removed");
            value["summary"]["workload_resource_disposition"] = json!("removed");
            value["summary"]["failure_kind"] = if success {
                Value::Null
            } else {
                json!("workload_check_failed")
            };
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let observer = Observer {
                inputs: &fixture.inputs,
                index: 0,
                directory: &fixture.directory,
                entry: &fixture.entry,
                publication: Mutex::new(()),
            };
            assert_eq!(observer.completed(replica, &path).is_ok(), success);
            assert_eq!(fixture.inputs.has_replica(0, 1).unwrap(), success);
            if success {
                let workspace = fixture.directory.join("workspace-concurrency-1");
                std::fs::remove_dir_all(workspace).unwrap();
                fixture
                    .inputs
                    .verify_replica(0, replica, &fixture.directory, &fixture.entry)
                    .unwrap();
            }
        }
    }

    #[test]
    fn replica_identity_output_omissions_and_cleanup_mismatches_cannot_be_sealed() {
        for case in ["slot", "total", "outputs", "cleanup", "outside"] {
            let fixture = Fixture::new(true);
            let identity = Identity { index: 1, total: 2 };
            let path = record(&fixture, identity);
            let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            match case {
                "slot" => record["replica"]["index"] = 2.into(),
                "total" => record["replica"]["total"] = 3.into(),
                "outputs" => {
                    record["outputs"].as_array_mut().unwrap().pop();
                }
                "cleanup" => record["workload_cleanup_confirmed"] = false.into(),
                _ => record["outputs"][0] = json!(fixture.root),
            }
            std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            let observer = Observer {
                inputs: &fixture.inputs,
                index: 0,
                directory: &fixture.directory,
                entry: &fixture.entry,
                publication: Mutex::new(()),
            };
            assert!(observer.completed(identity, &path).is_err(), "{case}");
            assert!(!fixture.inputs.has_replica(0, 1).unwrap());
        }
    }
}
