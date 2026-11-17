//! Read-only whole-population continuation classification. Not launch permission.
use super::*;
use crate::runner::campaign::reading;
use crate::sweep::checkpoint::Snapshotter;

type Key = (usize, u64);
const MAX_RECORDS: usize = 65_533;
const MAX_RECORD: u64 = 1024 * 1024;
const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[path = "generation.rs"]
mod generation;
pub(in crate::sweep) use generation::{AdmittedCampaign, Guard, Prepared as PreparedContinuation};

#[path = "continuation_link_recovery.rs"]
mod link_recovery;

#[path = "continuation_collector_teardown_recovery.rs"]
mod collector_teardown_recovery;

struct OwnedCampaigns {
    origin: Origin,
    layouts: Vec<Layout>,
    owners: Vec<crate::output_lock::OutputLock>,
}

/// A classification cannot be combined with another sweep/input owner, even
/// when both happen to have the same number of configurations and slots.
#[derive(Debug, Eq, PartialEq)]
struct Origin {
    checkpoint: PathBuf,
    invocation_parent: PathBuf,
    run_id: String,
    input_digest: String,
    index_bytes: u64,
    index_sha256: String,
}

impl Origin {
    fn read(
        inputs: &Inputs,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<Self, Error> {
        let bytes = reading::bounded(&results.join("plan.json"), 64 * 1024 * 1024)?;
        let retained: SweepPlan = serde_json::from_slice(&bytes)
            .map_err(|_| recovery("original sweep index is malformed"))?;
        if serde_json::to_value(retained).map_err(|_| recovery("cannot encode original index"))?
            != serde_json::to_value(plan).map_err(|_| recovery("cannot encode original index"))?
            || std::path::absolute(results.join(".checkpoint")).map_err(io_error)?
                != std::path::absolute(&inputs.directory).map_err(io_error)?
        {
            return Err(recovery(
                "classification differs from its original index/input owner",
            ));
        }
        Ok(Self {
            checkpoint: std::path::absolute(&inputs.directory).map_err(io_error)?,
            // Path::parent("sweep.toml") is Some(""); unlike join(),
            // std::path::absolute rejects that valid current-directory parent.
            invocation_parent: std::path::absolute(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })
            .map_err(io_error)?,
            run_id: plan.run_id.clone(),
            input_digest: inputs.initial.digest.clone(),
            index_bytes: bytes.len() as u64,
            index_sha256: crate::cassette::digest(&bytes),
        })
    }
}

/// Locks remain owned until this classification is consumed. Pending slots
/// still need generation publication and exclusive create-new start admission.
pub(in crate::sweep) struct Classified {
    catalog: Option<crate::runner::campaign::generation::Catalog>,
    identities: BTreeMap<usize, crate::cassette::Compatibility>,
    origin: Origin,
    layouts: Vec<Layout>,
    completed: BTreeMap<usize, BTreeMap<u64, Value>>,
    receipt_bindings: BTreeMap<Key, (u64, String)>,
    pending: BTreeMap<usize, BTreeSet<u64>>,
    _owners: Vec<crate::output_lock::OutputLock>,
}

#[cfg(test)]
impl Classified {
    pub(in crate::sweep::checkpoint) fn pending_keys(&self) -> BTreeSet<Key> {
        self.pending
            .iter()
            .flat_map(|(configuration, slots)| {
                slots.iter().map(move |slot| (*configuration, *slot))
            })
            .collect()
    }
}

struct Actual {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

struct Inventory {
    observed: BTreeSet<Key>,
    recovered: BTreeSet<Key>,
    unsettled: BTreeSet<Key>,
    actual: BTreeMap<Key, Actual>,
}

struct FileDeclaration {
    configuration: usize,
    path: PathBuf,
    bytes: u64,
}

fn recovery(reason: &str) -> Error {
    resume_error(&format!(
        "campaign saved-slot validation requires explicit recovery: {reason}"
    ))
}

fn present_absolute(path: &Path) -> Result<bool, Error> {
    if !path.is_absolute() {
        return Err(recovery("slot-owned observation is not absolute"));
    }
    let root = path
        .ancestors()
        .last()
        .ok_or_else(|| recovery("observation has no root"))?;
    reading::present_relative(
        root,
        path.strip_prefix(root)
            .map_err(|_| recovery("invalid observation root"))?,
    )
}

impl Inventory {
    fn read(
        layouts: &[Layout],
        checkpoint: &Path,
        input_digest: &str,
        snapshotter: &Snapshotter,
    ) -> Result<Self, Error> {
        let mut observed = BTreeSet::new();
        let mut recovered = BTreeSet::new();
        let mut recovered_observed = BTreeSet::new();
        let mut unsettled = BTreeSet::new();
        let mut files = Vec::new();
        let mut declared_bytes = 0u64;
        // Admit metadata for every configuration before reading any actual
        // completion or proving a recording payload. Never recursively search.
        for (configuration, layout) in layouts.iter().enumerate() {
            for slot in &layout.plan.slots {
                let key = (configuration, slot.id);
                let recovered_slot = collector_teardown_recovery::read_receipt(
                    checkpoint,
                    layout,
                    configuration,
                    slot,
                    input_digest,
                    snapshotter,
                )?;
                let mut slot_observed = false;
                let mut leaves = vec![
                    slot.workspace.clone(),
                    crate::runner::replica::runtime_path(&slot.workspace),
                    slot.log.clone(),
                    PathBuf::from(format!("batch-{}/starts/{}.json", slot.batch, slot.replica)),
                    PathBuf::from(format!(
                        "batch-{}/allocations/{}.json",
                        slot.batch, slot.replica
                    )),
                    PathBuf::from(format!(
                        "batch-{}/outcomes/{}.json",
                        slot.batch, slot.replica
                    )),
                ];
                let producer = crate::runner::recording::producer(
                    &layout.root,
                    slot.batch,
                    slot.replica,
                    layout.plan.concurrency,
                );
                leaves.push(
                    producer
                        .strip_prefix(&layout.root)
                        .map_err(|_| recovery("producer escapes campaign"))?
                        .to_path_buf(),
                );
                for leaf in leaves {
                    if reading::present_relative(&layout.root, &leaf)? {
                        slot_observed = true;
                    }
                }
                if let Some(directory) = layout.measurement_directory(slot)
                    && present_absolute(&directory)?
                {
                    slot_observed = true;
                }
                if let Some(destination) = &layout.plan.recording
                    && present_absolute(&crate::runner::recording::association_for(
                        destination,
                        slot.id,
                    )?)?
                {
                    slot_observed = true;
                }
                if reading::present_relative(
                    checkpoint,
                    &PathBuf::from(format!(
                        "campaign-completed/{configuration:06}/{:06}.json",
                        slot.id
                    )),
                )? {
                    slot_observed = true;
                }
                if reading::present_relative(
                    checkpoint,
                    &PathBuf::from(format!(
                        "campaign-unsettled/{configuration:06}/{:06}.json",
                        slot.id
                    )),
                )? {
                    slot_observed = true;
                    unsettled.insert(key);
                }
                if recovered_slot {
                    if slot_observed {
                        recovered_observed.insert(key);
                    }
                    recovered.insert(key);
                } else if slot_observed {
                    observed.insert(key);
                }
            }
            for batch in 1..=layout.plan.repeat {
                let directory = PathBuf::from(format!("batch-{batch}/.asb-completions"));
                for name in
                    reading::list_relative(&layout.root, &directory, MAX_RECORDS - files.len())?
                {
                    let path = directory.join(name);
                    if path.extension().is_none_or(|extension| extension != "json") {
                        return Err(recovery(
                            "unfinished or unknown actual completion record remains",
                        ));
                    }
                    let bytes =
                        reading::inspect_relative(&layout.root, &path)?.ok_or_else(|| {
                            recovery("actual completion disappeared during inventory")
                        })?;
                    if bytes > MAX_RECORD {
                        return Err(recovery("actual completion exceeds 1 MiB"));
                    }
                    declared_bytes = declared_bytes
                        .checked_add(bytes)
                        .filter(|bytes| *bytes <= MAX_BYTES)
                        .ok_or_else(|| recovery("actual completion inventory exceeds 256 MiB"))?;
                    files.push(FileDeclaration {
                        configuration,
                        path,
                        bytes,
                    });
                }
            }
        }
        let mut actual = BTreeMap::new();
        for file in files {
            let layout = &layouts[file.configuration];
            // Share the same actual-read budget with later strict declaration
            // and proof inspection, including growth after metadata admission.
            let bytes = layout.declaration_bytes(&file.path, MAX_RECORD)?;
            if bytes.len() as u64 != file.bytes {
                return Err(recovery("actual completion length changed after inventory"));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| recovery("actual completion is malformed"))?;
            let id = value["summary"]["execution"]["slot"]
                .as_u64()
                .ok_or_else(|| recovery("actual completion has no original slot"))?;
            let slot = id
                .checked_sub(1)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| layout.plan.slots.get(index))
                .ok_or_else(|| recovery("actual completion is outside original population"))?;
            if value["summary"]["execution"]
                != serde_json::to_value(layout.plan.execution(slot))
                    .map_err(|_| recovery("cannot encode original execution"))?
                || file.path.parent()
                    != Some(Path::new(&format!("batch-{}/.asb-completions", slot.batch)))
            {
                return Err(recovery(
                    "actual completion differs from original campaign/batch/selection",
                ));
            }
            let key = (file.configuration, id);
            observed.insert(key);
            if actual
                .insert(
                    key,
                    Actual {
                        path: layout.root.join(file.path),
                        bytes: file.bytes,
                        sha256: crate::cassette::digest(&bytes),
                    },
                )
                .is_some()
            {
                return Err(recovery(
                    "multiple actual completions claim one original slot",
                ));
            }
        }
        for key in recovered_observed {
            if !actual.contains_key(&key) {
                return Err(recovery(
                    "collector-teardown recovery leaves original slot evidence in place",
                ));
            }
        }
        Ok(Self {
            observed,
            recovered,
            unsettled,
            actual,
        })
    }
}

impl Inputs {
    pub(in crate::sweep) fn classify_campaigns(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<Classified, Error> {
        let owned = self.own_campaigns(parent, results, plan)?;
        let catalog = self.read_generation_catalog(results, plan, &owned.layouts)?;
        self.classify_owned(parent, results, plan, owned, catalog)
    }

    fn own_campaigns(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<OwnedCampaigns, Error> {
        let origin = Origin::read(self, parent, results, plan)?;
        self.verify_campaign_reservations(results, plan)?;
        let layouts = shared_layouts(results, plan)?;
        let mut owners = Vec::with_capacity(layouts.len());
        for layout in &layouts {
            owners.push(
                crate::output_lock::OutputLock::acquire_existing(
                    &layout.root.join(".owner.lock"),
                    "execution campaign already has an owner",
                )
                .map_err(Error::Run)?,
            );
        }
        Ok(OwnedCampaigns {
            origin,
            layouts,
            owners,
        })
    }

    fn classify_owned(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
        owned: OwnedCampaigns,
        catalog: Option<Catalog>,
    ) -> Result<Classified, Error> {
        let OwnedCampaigns {
            origin,
            layouts,
            owners,
        } = owned;
        let inventory = Inventory::read(
            &layouts,
            &self.directory,
            &self.initial.digest,
            &self.snapshotter,
        )?;
        let declarations = self.declare_campaign_layouts_with_catalog(
            parent,
            results,
            plan,
            layouts,
            crate::workloads::recovery::verify_retained,
            catalog.as_ref(),
        )?;
        if !declarations.unsettled.is_empty() || !inventory.unsettled.is_empty() {
            return Err(recovery(
                "one or more slots have incomplete or recovery-only outcomes",
            ));
        }
        let records: BTreeMap<Key, &Record> = declarations
            .records
            .iter()
            .map(|record| {
                let slot = &declarations.layouts[record.configuration].plan.slots[record.slot];
                ((record.configuration, slot.id), record)
            })
            .collect();
        if inventory.actual.len() != records.len() {
            return Err(recovery(
                "orphan actual completion or missing outcome reference",
            ));
        }
        for (key, actual) in &inventory.actual {
            let record = records
                .get(key)
                .ok_or_else(|| recovery("actual completion has no matching reference"))?;
            if actual.path != record.path
                || actual.bytes != record.bytes
                || actual.sha256 != record.sha256
            {
                return Err(recovery(
                    "actual completion and immutable reference disagree",
                ));
            }
        }
        let candidate_keys: BTreeSet<_> = declarations
            .candidates
            .iter()
            .map(|candidate| (candidate.configuration, candidate.slot))
            .collect();
        let mut pending: BTreeMap<usize, BTreeSet<u64>> = BTreeMap::new();
        for (configuration, layout) in declarations.layouts.iter().enumerate() {
            for slot in &layout.plan.slots {
                if candidate_keys.contains(&(configuration, slot.id)) {
                    continue;
                }
                if inventory.recovered.contains(&(configuration, slot.id)) {
                    pending.entry(configuration).or_default().insert(slot.id);
                    continue;
                }
                if inventory.observed.contains(&(configuration, slot.id)) {
                    return Err(recovery(
                        "started, allocated or otherwise observed slot is not reusable",
                    ));
                }
                pending.entry(configuration).or_default().insert(slot.id);
            }
        }
        let mut remaining = MAX_BYTES;
        let receipts = declarations
            .candidates
            .iter()
            .map(|candidate| self.read_campaign_receipt(candidate, &mut remaining))
            .collect::<Result<Vec<_>, _>>()?;
        // Full-population metadata, actual/reference bijection, untouched
        // observations and receipt identities pass before one grouped proof.
        let inspection = declarations.prove()?;
        let mut completed: BTreeMap<usize, BTreeMap<u64, Value>> = BTreeMap::new();
        let mut receipt_bindings = BTreeMap::new();
        for (candidate, (receipt, identity)) in inspection.candidates.into_iter().zip(receipts) {
            self.verify_campaign_candidate(&candidate, &receipt)?;
            receipt_bindings.insert((candidate.configuration, candidate.slot), identity);
            completed
                .entry(candidate.configuration)
                .or_default()
                .insert(candidate.slot, candidate.summary);
        }
        Ok(Classified {
            catalog,
            identities: inspection.identities,
            origin,
            layouts: inspection.layouts,
            completed,
            receipt_bindings,
            pending,
            _owners: owners,
        })
    }

    #[cfg(test)]
    pub(in crate::sweep) fn resume_campaign_outcomes(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<crate::sweep::resume::State, Error> {
        let classification = self.classify_campaigns(parent, results, plan)?;
        self.consume_completed_campaigns(classification, parent, results, plan)
    }

    fn consume_completed_campaigns(
        &self,
        classification: Classified,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<crate::sweep::resume::State, Error> {
        if classification.origin != Origin::read(self, parent, results, plan)? {
            return Err(recovery("classification origin changed before consumption"));
        }
        if !classification.pending.is_empty() {
            return Err(resume_error(
                "campaign continuation requires saved-slot validation and generation admission for positively untouched slots",
            ));
        }
        let mut state = crate::sweep::resume::State::default();
        for (configuration, layout) in classification.layouts.iter().enumerate() {
            let slots = classification
                .completed
                .get(&configuration)
                .filter(|slots| slots.len() == layout.plan.slots.len())
                .ok_or_else(|| recovery("classification does not cover original population"))?;
            state
                .groups
                .insert(configuration, slots.values().cloned().collect());
        }
        state.campaign_owners = classification._owners;
        Ok(state)
    }

    pub(in crate::sweep) fn prepare_campaign_continuation(
        self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<CampaignContinuation, Error> {
        let classified = self.classify_campaigns(parent, results, plan)?;
        if classified.pending.is_empty() {
            let reused = self.consume_completed_campaigns(classified, parent, results, plan)?;
            Ok(CampaignContinuation::Completed(Box::new(self), reused))
        } else {
            Ok(CampaignContinuation::Pending(Box::new(
                PreparedContinuation::plan(self, classified, parent, results, plan)?,
            )))
        }
    }
}

pub(in crate::sweep) enum CampaignContinuation {
    Completed(Box<Inputs>, crate::sweep::resume::State),
    Pending(Box<PreparedContinuation>),
}
