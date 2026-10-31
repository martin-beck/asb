//! Explicit campaign-plan comparison: every saved slot, never newest discovery.
use super::*;
use crate::runner::campaign::reading::Layout;

pub(super) fn load(path: &str) -> Result<LoadedReport, Error> {
    let layout = Layout::read(Path::new(path))?;
    let owner = standalone_owner(&layout)?;
    let configuration = &layout.plan.configuration;
    let config = layout.root.join("benchmark.toml");
    // This is a retained configuration snapshot, not a file to create or load.
    let request = Request {
        path: path.into(),
        parent: layout.root.clone(),
        single: true,
        sweep: crate::sweep::SweepFile {
            configs: vec!["benchmark.toml".into()],
            repeat: None,
            resources: None,
        },
        sweep_results: layout.root.clone(),
        comparison_dir: layout.root.join("comparison"),
        plan: None,
        campaign: Some(
            serde_json::json!({"campaign":layout.plan.campaign,"execution_mode":layout.plan.mode,"respect_record_timings":layout.plan.respect_record_timings,"repeat":layout.plan.repeat,"concurrency":layout.plan.concurrency,"replay_selection":layout.plan.replay_selection}),
        ),
        configurations: BTreeMap::from([(
            config.clone(),
            toml::to_string(configuration).map_err(|error| Error::Input(error.to_string()))?,
        )]),
    };
    let mut proofs = Vec::new();
    let (rows, previews) = rows(
        &layout,
        &request,
        owner.as_ref().map_or(0, |(_, index)| *index),
        config,
        None,
        &mut proofs,
        owner.as_ref().map(|(catalog, _)| catalog),
    )?;
    verify_proofs(&proofs)?;
    Ok(LoadedReport {
        request,
        rows,
        previews,
    })
}

/// Standalone campaign reports can resolve their original sweep context,
/// including under preserved history. Never follow a locator from Evidence.
fn standalone_owner(
    layout: &Layout,
) -> Result<Option<(crate::runner::campaign::generation::Catalog, usize)>, Error> {
    use crate::runner::campaign::{generation, reading};
    if layout
        .root
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        != Some("campaigns")
    {
        return Ok(None);
    }
    let Some(results) = layout.root.ancestors().nth(3) else {
        return Ok(None);
    };
    if !reading::present_relative(results, &Path::new(".checkpoint").join(generation::ROOT))? {
        return Ok(None);
    }
    let plan = crate::sweep::plan::SweepPlan::source_snapshot(
        results,
        results.parent().unwrap_or(results),
    )?
    .ok_or_else(|| Error::Input("continued campaign lost its owning sweep index".into()))?;
    let layouts = plan.read_campaigns(results)?;
    let configuration = layouts
        .iter()
        .position(|candidate| candidate.root == layout.root)
        .ok_or_else(|| {
            Error::Input("continued campaign is not a member of the owning sweep".into())
        })?;
    let catalog = plan
        .generation_catalog(results, &layouts)?
        .ok_or_else(|| Error::Input("continued campaign lost its owning generation".into()))?;
    Ok(Some((catalog, configuration)))
}

pub(super) fn rows(
    layout: &Layout,
    request: &Request,
    index: usize,
    config: PathBuf,
    selection: Option<&crate::workloads::TaskSelection>,
    proofs: &mut Vec<crate::cassette::TrackEvidence>,
    catalog: Option<&crate::runner::campaign::generation::Catalog>,
) -> Result<(Vec<BenchmarkData>, BTreeMap<String, String>), Error> {
    let configuration = &layout.plan.configuration;
    let mut rows = Vec::new();
    let mut previews = BTreeMap::new();
    let mut context = Configuration {
        index,
        config: config.clone(),
        result_dir: layout.root.clone(),
        agent: configuration.agent.clone(),
        workload: configuration.workload.clone(),
        provider: configuration.provider.clone(),
        model: configuration.model.clone(),
        concurrency: configuration.concurrency,
        batch: None,
        expected_execution: None,
        measurement_snapshot: None,
        measurement_override: Some(None),
        standalone: Some(configuration.clone()),
        selection: selection.cloned(),
        outcomes: BTreeMap::new(),
    };
    for slot in &layout.plan.slots {
        let outcome = layout.outcome(slot)?;
        let log = layout
            .log(slot)?
            .map(|(path, summary)| outcomes::decode_log(&path, summary))
            .transpose()?;
        let recorded = if let Some(outcome) = &outcome {
            let (_, recorded) =
                outcomes::decode_outcome(&outcome.path, &outcome.value, layout.plan.concurrency)?;
            if let Some(log) = &log {
                recorded.check_measurement(&log.summary)?;
            }
            Some(recorded)
        } else {
            log
        };
        let measurement = slot.measurements.as_ref().map(|path| {
            layout
                .plan
                .measurement_root
                .as_ref()
                .expect("validated measurement root")
                .join(path)
        });
        if let Some(declaration) = recorded
            .as_ref()
            .and_then(|record| record.summary.get("measurement_directory"))
        {
            if !declaration.is_null() && !declaration.is_string() {
                return Err(Error::Input(
                    "campaign result has malformed measurement directory".into(),
                ));
            }
            let declared = declaration.as_str().map(PathBuf::from);
            if declared != measurement {
                return Err(Error::Input(
                    "campaign outcome measurement directory differs from its planned slot".into(),
                ));
            }
        }
        context.result_dir = layout.root.join(format!("batch-{}", slot.batch));
        context.batch = Some(slot.batch);
        context.expected_execution = Some(
            serde_json::to_value(layout.plan.execution(slot))
                .map_err(|error| Error::Input(error.to_string()))?,
        );
        context.measurement_override = Some(layout.measurement_directory(slot));
        context.measurement_snapshot = layout.measurement(slot)?;
        context.outcomes = recorded
            .map(|record| BTreeMap::from([(slot.replica, record)]))
            .unwrap_or_default();
        let owner = RecordingContext {
            layout,
            slot,
            catalog,
            configuration: index,
        };
        let mut row = load_attempt(request, &context, slot.replica, proofs, Some(&owner))?;
        if let Some(outcome) = &outcome {
            verify_recording(&outcome.value)?;
        }
        row.label = format!(
            "{index:03}-{}-batch-{}-replica-{}",
            configuration.agent, slot.batch, slot.replica
        );
        for (href, physical) in &row.artifact_sources {
            let name = href.rsplit('/').next().unwrap_or(href);
            if matches!(
                crate::collectors::display_artifact(name),
                Some(crate::collectors::ArtifactDisplay::Svg { .. })
            ) && !previews.contains_key(href)
            {
                previews.insert(href.clone(), read_svg(physical, href)?);
            }
        }
        rows.push(row);
    }
    Ok((rows, previews))
}

fn verify_recording(raw: &Value) -> Result<(), Error> {
    if raw["recording_track"] != raw["summary"]["recording"] {
        return Err(Error::Input(
            "campaign outcome and summary recording associations differ".into(),
        ));
    }
    let captures: Vec<crate::proxy::capture::CaptureEvidence> = raw
        .get("capture_evidence")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| Error::Input("malformed campaign capture evidence".into()))?
        .unwrap_or_default();
    for capture in &captures {
        capture.validate()?;
    }
    if let Some(value) = raw.get("recording_track").filter(|value| !value.is_null()) {
        let evidence: crate::runner::recording::Evidence = serde_json::from_value(value.clone())
            .map_err(|_| Error::Input("malformed campaign recording association".into()))?;
        evidence.validate_summary_metadata(&raw["summary"], &captures)?;
    } else if !captures.is_empty() && raw["summary"]["execution"]["replay_track"].is_null() {
        return Err(Error::Input(
            "campaign capture has no immutable recording association".into(),
        ));
    }
    Ok(())
}

pub(super) struct RecordingContext<'a> {
    layout: &'a Layout,
    slot: &'a crate::runner::campaign::Slot,
    catalog: Option<&'a crate::runner::campaign::generation::Catalog>,
    configuration: usize,
}

impl RecordingContext<'_> {
    fn validate(&self, evidence: &crate::runner::recording::Evidence) -> Result<(), Error> {
        if let Some(binding) = &evidence.generation {
            let start: crate::runner::campaign::StartIntent =
                serde_json::from_value(self.layout.declaration(
                    &PathBuf::from(format!(
                        "batch-{}/starts/{}.json",
                        self.slot.batch, self.slot.replica
                    )),
                    64 * 1024,
                )?)
                .map_err(|_| Error::Input("recording has malformed original start".into()))?;
            self.catalog
                .ok_or_else(|| {
                    Error::Input("continued recording requires its owning sweep report".into())
                })?
                .recording(
                    self.configuration,
                    binding,
                    evidence,
                    &start,
                    &self.layout.plan,
                    self.slot,
                )
        } else if Some(&evidence.track.destination) != self.layout.plan.recording.as_ref() {
            Err(Error::Input(
                "recording differs from original campaign destination".into(),
            ))
        } else {
            Ok(())
        }
    }
}

pub(super) fn collect_summary_recording(
    summary: &Value,
    proofs: &mut Vec<crate::cassette::TrackEvidence>,
    owner: Option<&RecordingContext<'_>>,
) -> Result<(), Error> {
    if let Some(value) = summary.get("recording").filter(|value| !value.is_null()) {
        let evidence: crate::runner::recording::Evidence = serde_json::from_value(value.clone())
            .map_err(|_| Error::Input("malformed campaign summary recording association".into()))?;
        evidence.validate_report_metadata(summary)?;
        match owner {
            Some(owner) => owner.validate(&evidence)?,
            None if evidence.generation.is_some() => {
                return Err(Error::Input(
                    "recording has no owning campaign context".into(),
                ));
            }
            None => (),
        }
        proofs.push(evidence.track);
    }
    Ok(())
}

pub(super) fn verify_proofs(proofs: &[crate::cassette::TrackEvidence]) -> Result<(), Error> {
    crate::cassette::TrackEvidence::verify_batch(proofs, crate::cassette::ProofLimits::default())
        .map_err(|error| Error::Input(format!("campaign recording proof: {error}")))
}
