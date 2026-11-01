//! Sweep-owned campaign preparation/publication; execution stays runner-owned.
use super::{Options, execution, plan};
use crate::{Error, runner, tape};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct Prepared {
    entries: Vec<(runner::repeated::Prepared, PathBuf)>,
}

pub(super) struct Pool {
    entries: Vec<Entry>,
}
struct Entry {
    published: Mutex<Option<Worker>>,
    settlement: Settlement,
    stop: AtomicBool,
}

enum Settlement {
    Retained,
    Attempt(PathBuf),
}

enum Worker {
    Fresh(Box<runner::repeated::PublishedCampaign>),
    Continued(Box<ContinuedWorker>),
}
struct ContinuedWorker {
    admitted: super::checkpoint::campaigns::AdmittedCampaign,
    json: bool,
    show_output: bool,
    invocation_dir: Option<PathBuf>,
    policy: Option<crate::planning::ResolvedPlan>,
}

fn error(reason: &str) -> Error {
    Error::Input(format!("sweep campaign: {reason}"))
}

/// Resolve the immutable ZIP publication layouts accepted for an explicit
/// replay root.  Fresh sweep recording publishes at
/// `ROOT/ENTRY/recording.asb.zip`; the older campaign layout placed the same
/// archive below `ROOT/ENTRY/cassettes/`.  Check the publisher's current
/// layout first so an explicit root never silently falls through to a legacy
/// JSONL name when a valid ZIP was supplied.
fn replay_bundle(root: &Path, entry: &str) -> Result<Option<PathBuf>, Error> {
    for candidate in [
        root.join(entry).join("recording.asb.zip"),
        root.join(entry).join("cassettes/recording.asb.zip"),
    ] {
        if crate::cassette::Source::is_present(&candidate)
            .map_err(|failure| error(&failure.to_string()))?
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Resume consumes original private inputs; an explicit source-root argument
/// may confirm, but never redirect, their persisted source identity. This does
/// not reopen the archive or require the original public source to still exist.
pub(super) fn validate_resume_options(
    results: &Path,
    plan: &plan::SweepPlan,
    options: Options<'_>,
) -> Result<(), Error> {
    let Some(intent) = &plan.execution else {
        if options.repeat.is_some() || options.track.is_some() || options.seed.is_some() {
            return Err(error(
                "legacy sweep has no saved campaign selection; start a fresh sweep to change repeat/track/seed",
            ));
        }
        return Ok(());
    };
    if options.repeat.is_some_and(|repeat| repeat != intent.repeat)
        || options
            .track
            .is_some_and(|track| Some(track) != intent.selector)
        || options.seed.is_some_and(|seed| Some(seed) != intent.seed)
    {
        return Err(error(
            "repeat, track or seed differs from the saved campaign selection; omit these options to reuse the original plan",
        ));
    }
    let Some(requested) = options.cassette_root else {
        return Ok(());
    };
    for (index, (entry, layout)) in plan
        .entries
        .iter()
        .zip(plan.read_campaigns(results)?)
        .enumerate()
    {
        let original = layout
            .plan
            .recording
            .as_ref()
            .or(layout.plan.replay_source.as_ref())
            .or_else(|| layout.plan.replay_inputs.first().map(|input| &input.source))
            .ok_or_else(|| {
                error("saved campaign has no cassette source; omit --cassette-root when resuming")
            })?;
        let expected = std::path::absolute(
            requested
                .join(execution::result_name(index, Path::new(&entry.reference)))
                .join("cassettes"),
        )
        .map_err(crate::io_error)?;
        if original.parent() != Some(expected.as_path()) {
            return Err(error(
                "--cassette-root differs from the saved campaign source; omit it to use the original private inputs",
            ));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;

pub(super) fn prepare(
    parent: &Path,
    results: &Path,
    preserved: &Path,
    plan: &mut plan::SweepPlan,
    options: Options<'_>,
) -> Result<Option<Prepared>, Error> {
    if options.cassette_root.is_some() && options.mode.is_none() {
        return Err(error("cassette root requires record or replay"));
    }
    if (options.track.is_some() || options.seed.is_some())
        && options.mode != Some(tape::Mode::Replay)
    {
        return Err(error("track and seed require replay"));
    }
    let previous = if options.mode == Some(tape::Mode::Replay) && options.cassette_root.is_none() {
        plan::SweepPlan::source_snapshot(results, parent)?
    } else {
        None
    };
    let previous_layouts = previous
        .as_ref()
        .filter(|saved| saved.execution.is_some())
        .map(|saved| saved.read_campaigns(results))
        .transpose()?;
    let previous_catalog = previous
        .as_ref()
        .zip(previous_layouts.as_ref())
        .map(|(saved, layouts)| saved.generation_catalog(results, layouts))
        .transpose()?
        .flatten();
    let sources = plan.entries.iter().enumerate().map(|(index, entry)| {
        let name = execution::result_name(index, Path::new(&entry.reference));
        if options.mode == Some(tape::Mode::Record) {
            // Default bundles outlive results rotation; immutable recording
            // associations must not point into a subsequently moved result tree.
            return Ok(Some(options.cassette_root.map(|root| root.join(&name).join("cassettes/recording.asb.zip"))
                .unwrap_or_else(|| parent.join("cassettes").join(format!("sweep-{}", plan.run_id)).join(name).join("recording.asb.zip"))));
        }
        if options.mode.is_none() { return Ok(None); }
        if let Some(old) = previous.as_ref().and_then(|saved| saved.entries.get(index))
            && let Some(reference) = &old.campaign
        {
            if old.reference != entry.reference { return Err(error("default replay source configuration order changed; select an explicit cassette root")); }
            let layout = runner::campaign::reading::Layout::read_reference(results, reference)?;
            let current = previous_catalog.as_ref().map(|catalog| runner::recording::union::current(&layout.root, catalog, index)).transpose()?.flatten();
            let source = current.or(layout.plan.recording).or(layout.plan.replay_source).or_else(|| layout.plan.replay_inputs.first().map(|input| input.source.clone()))
                .ok_or_else(|| error("previous campaign has no recording/replay source"))?;
            return Ok(Some(source));
        }
        if let Some(root) = options.cassette_root
            && let Some(bundle) = replay_bundle(root, &name)?
        {
            return Ok(Some(bundle));
        }
        let root = options.cassette_root.unwrap_or(results).join(name).join("cassettes");
        let bundle = root.join("recording.asb.zip");
        Ok(Some(if crate::cassette::Source::is_present(&bundle).map_err(|failure| error(&failure.to_string()))? { bundle } else { root.join(super::cassette_filename(&entry.configuration)) }))
    }).collect::<Result<Vec<_>, Error>>()?;
    let bundles = sources
        .iter()
        .map(|source| {
            if options.mode == Some(tape::Mode::Replay) {
                crate::cassette::Source::is_bundle(source.as_deref().expect("replay source"))
                    .map_err(|failure| error(&failure.to_string()))
            } else {
                Ok(false)
            }
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let modern = options.repeat.is_some()
        || options.track.is_some()
        || options.seed.is_some()
        || options.mode == Some(tape::Mode::Record)
        || bundles.iter().any(|bundle| *bundle)
        || previous
            .as_ref()
            .is_some_and(|saved| saved.execution.is_some());
    if !modern {
        return Ok(None);
    }
    if options.resume || options.reconcile_completed {
        return Err(error(
            "campaign continuation needs saved-slot validation; existing results were not changed",
        ));
    }
    let repeat = options.repeat.unwrap_or(1);
    plan.entries
        .iter()
        .try_fold(0u64, |total, entry| {
            repeat
                .checked_mul(entry.configuration.concurrency as u64)
                .and_then(|count| total.checked_add(count))
        })
        .filter(|total| repeat > 0 && *total <= 65_533)
        .ok_or_else(|| error("invalid or excessive total repeat/concurrency population"))?;
    let seed = options.seed.or_else(|| {
        bundles.iter().any(|bundle| *bundle).then(|| {
            let id = crate::workloads::TaskIdentity::local("sweep-replay-seed", None);
            let digest = crate::cassette::digest(id.attempt_id.as_bytes());
            u64::from_str_radix(&digest[..16], 16).expect("canonical digest prefix")
        })
    });
    let mut inspections = Vec::new();
    let mut cost = runner::repeated::InputCost::default();
    let mut recording_reservation = crate::cassette::RecordingReservation::default();
    for (index, ((entry, source), bundle)) in plan
        .entries
        .iter_mut()
        .zip(&sources)
        .zip(&bundles)
        .enumerate()
    {
        let campaign_parent = results
            .join(execution::result_name(index, Path::new(&entry.reference)))
            .join("campaigns");
        let inspected = runner::repeated::Inspection::prepare_selected(
            &parent.join(&entry.reference),
            entry.configuration.clone(),
            repeat,
            runner::RunOptions {
                invocation_dir: options.invocation_dir,
                max_steps: options.max_steps,
                json: options.json,
                show_output: options.show_output,
                quiet_output: options.quiet_output,
                mode: options.mode,
                tool_mode: options.tool_mode,
                tool_mode_explicit: options.tool_mode_explicit,
                mcp_mode: options.mcp_mode,
                cassette: source.as_deref().and_then(Path::to_str),
                track: options.track,
                replay_seed: if *bundle { seed } else { options.seed },
                respect_record_timings: options.respect_record_timings,
                // PreparedSweep warmed this fresh population before campaign
                // construction, so each repeated task must not revalidate the
                // same shared installations.
                requirements_prepared: true,
                policy: entry.policy.as_ref(),
                ..Default::default()
            },
            Some(&campaign_parent),
            Some(&entry.selection),
        )?;
        if let Some(capacity) = inspected.recording_reservation() {
            recording_reservation = recording_reservation
                .checked_add(capacity)
                .map_err(|failure| error(&failure.to_string()))?;
            recording_reservation
                .validate(crate::cassette::ProofLimits::default())
                .map_err(|failure| error(&failure.to_string()))?;
        }
        cost = cost.checked_add(inspected.input_cost()?)?;
        inspections.push(inspected);
    }
    // No legacy hash occurs until every source's metadata and the complete
    // retained-descriptor/selected-byte budgets have passed admission.
    for inspected in &inspections {
        inspected.validate_sources()?;
    }
    let mut entries = Vec::new();
    let mut plan_bytes = 0u64;
    for (entry, inspected) in plan.entries.iter_mut().zip(inspections) {
        let mut prepared = inspected.observe()?;
        prepared.preserve_source_locations(results, preserved)?;
        let reference = prepared.reference(results)?;
        plan_bytes = plan_bytes
            .checked_add(reference.bytes)
            .filter(|bytes| *bytes <= plan::MAX_PLAN_BYTES as u64)
            .ok_or_else(|| error("campaign plans exceed aggregate 64 MiB"))?;
        entries.push((prepared, results.join(&reference.root)));
        entry.campaign = Some(reference);
    }
    plan.bind_execution(plan::ExecutionIntent {
        recording_reservation: (options.mode == Some(tape::Mode::Record))
            .then_some(recording_reservation),
        repeat,
        seed,
        user_seed: options.seed.is_some(),
        selector: options.track,
    })?;
    Ok(Some(Prepared { entries }))
}

impl Prepared {
    pub fn publish(self, results: &Path) -> Result<Pool, Error> {
        // Collect ALL publications before returning execution authority. A
        // failure drops locks but retains the index and already-published roots.
        let entries = self
            .entries
            .into_iter()
            .map(|(prepared, root)| {
                runner::repeated::publish_campaign(prepared).map(|published| Entry {
                    published: Mutex::new(Some(Worker::Fresh(Box::new(published)))),
                    settlement: Settlement::Attempt(root),
                    stop: AtomicBool::new(false),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let index = runner::campaign::reading::bounded(
            &results.join("plan.json"),
            plan::MAX_PLAN_BYTES as u64,
        )?;
        crate::evidence::write_json(&results.join("campaigns-published.json"), &serde_json::json!({"schema_version":1,"scope":"published-plans-and-private-inputs-not-attempt-evidence","plan_sha256":crate::cassette::digest(&index),"configurations":entries.len()})).map_err(Error::Io)?;
        Ok(Pool { entries })
    }
}

impl Pool {
    pub(super) fn continued(
        admitted: Vec<super::checkpoint::campaigns::AdmittedCampaign>,
        plan: &plan::SweepPlan,
        options: Options<'_>,
    ) -> Self {
        Self {
            entries: admitted
                .into_iter()
                .zip(&plan.entries)
                .map(|(admitted, entry)| {
                    let settlement = if admitted.has_pending() {
                        Settlement::Attempt(admitted.settlement_root())
                    } else {
                        Settlement::Retained
                    };
                    Entry {
                        published: Mutex::new(Some(Worker::Continued(Box::new(ContinuedWorker {
                            admitted,
                            json: options.json,
                            show_output: options.show_output,
                            invocation_dir: options.invocation_dir.map(Path::to_path_buf),
                            policy: entry.policy.clone(),
                        })))),
                        settlement,
                        stop: AtomicBool::new(false),
                    }
                })
                .collect(),
        }
    }

    pub fn run(
        &self,
        index: usize,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        let entry = &self.entries[index];
        let published = {
            let mut owned = entry
                .published
                .lock()
                .map_err(|_| error("campaign execution ownership was interrupted"))?;
            owned
                .take()
                .ok_or_else(|| error("campaign was already consumed"))?
        };
        let result = match published {
            Worker::Fresh(published) => runner::repeated::execute(*published, output, warnings),
            Worker::Continued(worker) => worker.admitted.execute(
                worker.json,
                worker.show_output,
                worker.invocation_dir.as_deref(),
                worker.policy.as_ref(),
                output,
                warnings,
            ),
        };
        let settled = match &entry.settlement {
            // Reuse validates the complete retained population without a new
            // attempt; absence of a new finished.json is therefore expected.
            Settlement::Retained => true,
            Settlement::Attempt(root) => {
                runner::campaign::reading::bounded(&root.join("finished.json"), 16 * 1024)
                    .and_then(|bytes| {
                        serde_json::from_slice::<serde_json::Value>(&bytes)
                            .map_err(|_| error("malformed campaign settlement"))
                    })
                    .is_ok_and(|value| value["schema_version"] == 1 && value["stopped"] == false)
            }
        };
        let stop = matches!(result, Err(Error::Input(_) | Error::Io(_))) || !settled;
        entry.stop.store(stop, Ordering::Release);
        result
    }

    pub fn stop_requested(&self, index: usize) -> bool {
        self.entries[index].stop.load(Ordering::Acquire)
    }
}
