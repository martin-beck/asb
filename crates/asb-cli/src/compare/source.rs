//! Read-only report inputs and per-record decoding. Never publishes report files.
use super::*;
use std::io::Read;
mod campaign;

/// Open once, reject nonregular sources, and bound the actual read even if the
/// file grows after metadata inspection. Nonblocking open avoids FIFO hangs.
pub(super) fn read_text(path: &Path, kind: &str) -> Result<String, Error> {
    read_text_limit(path, kind, MAX_BYTES)
}
fn read_text_limit(path: &Path, kind: &str, limit: u64) -> Result<String, Error> {
    read_text_limit_with_policy(path, kind, limit, false)
}
fn read_text_limit_with_policy(
    path: &Path,
    kind: &str,
    limit: u64,
    nofollow: bool,
) -> Result<String, Error> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | if nofollow { libc::O_NOFOLLOW } else { 0 });
    }
    #[cfg(not(unix))]
    let _ = nofollow;
    let file = options
        .open(path)
        .map_err(|error| Error::Input(format!("cannot read {kind} {}: {error}", path.display())))?;
    let metadata = file.metadata().map_err(|error| {
        Error::Input(format!("cannot inspect {kind} {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(Error::Input(format!(
            "{kind} {} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > limit {
        return Err(Error::Input(format!(
            "{kind} {} exceeds {}",
            path.display(),
            limit_label(limit)
        )));
    }
    #[cfg(test)]
    super::pipeline_tests::after_open(&file, path);
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Input(format!("cannot read {kind} {}: {error}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(Error::Input(format!(
            "{kind} {} exceeds {}",
            path.display(),
            limit_label(limit)
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Input(format!("cannot read {kind} {}: {error}", path.display())))
}
fn limit_label(limit: u64) -> String {
    if limit.is_multiple_of(1024 * 1024) {
        format!("{} MiB", limit / (1024 * 1024))
    } else {
        format!("{} KiB", limit / 1024)
    }
}
pub(super) fn read_benchmark_text(path: &Path) -> Result<String, Error> {
    read_text_limit(path, "benchmark configuration", 64 * 1024)
}
pub(super) fn read_json(path: &Path) -> Result<Value, Error> {
    let value = serde_json::from_str(&read_text(path, "measurement file")?).map_err(|error| {
        Error::Input(format!(
            "invalid measurement JSON {}: {error}",
            path.display()
        ))
    })?;
    crate::process_termination::recorded(&value).map_err(|error| Error::Input(error.into()))?;
    Ok(value)
}
pub(super) fn read_svg(path: &Path, artifact: &str) -> Result<String, Error> {
    let svg = read_text(path, "embedded SVG artifact").map_err(|error| {
        Error::Run(format!(
            "cannot read embedded SVG artifact {artifact}: {error}"
        ))
    })?;
    let trimmed = svg.trim_start();
    if !trimmed.starts_with("<svg") && !trimmed.starts_with("<?xml") {
        return Err(Error::Run(format!(
            "embedded SVG artifact {artifact} does not contain an SVG document"
        )));
    }
    Ok(svg)
}

pub(super) struct Request {
    pub path: String,
    pub parent: PathBuf,
    pub single: bool,
    pub sweep: crate::sweep::SweepFile,
    pub sweep_results: PathBuf,
    pub comparison_dir: PathBuf,
    pub plan: Option<crate::sweep::plan::SweepPlan>,
    pub campaign: Option<Value>,
    configurations: BTreeMap<PathBuf, String>,
}
pub(super) struct LoadedReport {
    pub request: Request,
    pub rows: Vec<BenchmarkData>,
    pub previews: BTreeMap<String, String>,
}

impl Request {
    fn read(path: &str) -> Result<Self, Error> {
        let sweep_path = Path::new(path);
        let input_text = read_text(sweep_path, "TOML")?;
        let input: toml::Value = toml::from_str(&input_text)
            .map_err(|error| Error::Input(format!("cannot parse {path}: {error}")))?;
        let single = !input
            .as_table()
            .is_some_and(|table| table.contains_key("configs"));
        let sweep = if single {
            crate::sweep::SweepFile {
                resources: None,
                repeat: None,
                configs: vec![
                    sweep_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                ],
            }
        } else {
            crate::sweep::parse_manifest(sweep_path, input)?
        };
        let parent = sweep_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = sweep_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("sweep");
        let sweep_results = parent.join(format!("{stem}-results"));
        let mut configurations = BTreeMap::new();
        let mut configuration_bytes = 0usize;
        for reference in &sweep.configs {
            let config = checked_benchmark_reference(parent, reference, path)?;
            if configurations.contains_key(&config) {
                continue;
            }
            let text = if single {
                if input_text.len() > 64 * 1024 {
                    return Err(Error::Input(format!(
                        "benchmark configuration {} exceeds 64 KiB",
                        sweep_path.display()
                    )));
                }
                input_text.clone()
            } else {
                read_benchmark_text(&config)?
            };
            configuration_bytes = configuration_bytes.saturating_add(text.len());
            if configuration_bytes > 32 * 1024 * 1024 {
                return Err(Error::Input("comparison configuration inputs exceed 32 MiB; select a smaller task/model batch".into()));
            }
            configurations.insert(config, text);
        }
        let plan = if single {
            None
        } else {
            let plan_path = sweep_results.join("plan.json");
            match fs::symlink_metadata(&plan_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(Error::Input(format!(
                        "cannot inspect sweep plan {}: {error}",
                        plan_path.display()
                    )));
                }
                Ok(_) => {
                    let bytes = read_text_limit(
                        &plan_path,
                        "sweep plan",
                        crate::sweep::plan::MAX_PLAN_BYTES as u64,
                    )?;
                    Some(crate::sweep::plan::SweepPlan::decode(
                        &plan_path,
                        bytes.as_bytes(),
                        parent,
                        &sweep,
                        |path| {
                            let text = configurations.get(path).ok_or_else(|| {
                                Error::Input(
                                    "sweep plan requested an uncaptured configuration".into(),
                                )
                            })?;
                            toml::from_str(text).map_err(|error| Error::Input(error.to_string()))
                        },
                    )?)
                }
            }
        };
        let comparison_dir = parent.join("results").join(format!("{stem}-comparison"));
        Ok(Request {
            campaign: None,
            path: path.to_owned(),
            parent: parent.to_path_buf(),
            single,
            sweep,
            sweep_results,
            comparison_dir,
            plan,
            configurations,
        })
    }
}

pub(super) fn load(path: &str) -> Result<LoadedReport, Error> {
    if Path::new(path)
        .file_name()
        .is_some_and(|name| name == "plan.json")
    {
        return campaign::load(path);
    }
    let request = Request::read(path)?;
    if request
        .plan
        .as_ref()
        .is_some_and(|plan| plan.execution.is_some())
    {
        return load_campaign_sweep(request);
    }
    let mut rows = Vec::new();
    let mut proofs = Vec::new();
    for (index, reference) in request.sweep.configs.iter().enumerate() {
        let configuration = load_configuration(&request, index, reference)?;
        for replica in 1..=configuration.concurrency {
            rows.push(load_attempt(
                &request,
                &configuration,
                replica,
                &mut proofs,
                None,
            )?);
        }
    }
    apply_pricing(&request, &mut rows)?;
    let mut previews = BTreeMap::new();
    for row in &rows {
        if row.result_source == ResultSource::RecoveredOfficialGrade {
            continue;
        }
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
    }
    Ok(LoadedReport {
        request,
        rows,
        previews,
    })
}

fn load_campaign_sweep(request: Request) -> Result<LoadedReport, Error> {
    let plan = request.plan.as_ref().expect("campaign sweep plan");
    let mut layouts = plan.read_campaigns(&request.sweep_results)?;
    let catalog = plan.generation_catalog(&request.sweep_results, &layouts)?;
    let budget = std::rc::Rc::new(std::cell::Cell::new(256 * 1024 * 1024));
    for layout in &mut layouts {
        layout.share_observation_budget(budget.clone());
    }
    let mut primary_bytes = 0u64;
    for layout in &layouts {
        primary_bytes = primary_bytes
            .checked_add(layout.preflight()?)
            .filter(|bytes| *bytes <= 256 * 1024 * 1024)
            .ok_or_else(|| {
                Error::Input("sweep primary observations exceed aggregate 256 MiB limit".into())
            })?;
    }
    let mut rows = Vec::new();
    let mut previews = BTreeMap::new();
    let mut proofs = Vec::new();
    for (index, (entry, layout)) in plan.entries.iter().zip(&layouts).enumerate() {
        let config = request.parent.join(&entry.reference);
        let (mut selected, artifacts) = campaign::rows(
            layout,
            &request,
            index,
            config,
            Some(&entry.selection),
            &mut proofs,
            catalog.as_ref(),
        )?;
        rows.append(&mut selected);
        previews.extend(artifacts);
    }
    campaign::verify_proofs(&proofs)?;
    apply_pricing(&request, &mut rows)?;
    Ok(LoadedReport {
        request,
        rows,
        previews,
    })
}

/// Derive a cost estimate from the price schedule captured when this sweep
/// started. Report generation never contacts a live provider.
fn apply_pricing(request: &Request, rows: &mut [BenchmarkData]) -> Result<(), Error> {
    for provider in crate::providers::names() {
        let Some(filename) = crate::providers::pricing_filename(provider) else {
            continue;
        };
        apply_provider_pricing(&request.sweep_results, provider, filename, rows)?;
    }
    Ok(())
}

fn apply_provider_pricing(
    root: &Path,
    provider: &str,
    filename: &str,
    rows: &mut [BenchmarkData],
) -> Result<(), Error> {
    let path = root.join(filename);
    let text = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Error::Input(format!(
                "cannot inspect pricing snapshot {}: {error}",
                path.display()
            )));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(Error::Input(
                "pricing snapshot must be a regular file".into(),
            ));
        }
        Ok(_) => read_text_limit_with_policy(&path, "pricing snapshot", MAX_BYTES, true)?,
    };
    let snapshot: Value = serde_json::from_str(&text)
        .map_err(|_| Error::Input(format!("invalid {provider} pricing snapshot")))?;
    crate::providers::validate_pricing(provider, &snapshot)?;
    for row in rows.iter_mut().filter(|row| row.provider == provider) {
        if let Some(estimate) =
            crate::providers::estimate_cost(provider, &snapshot, &row.model, &row.metrics)
        {
            row.metrics.insert("estimated_cost_usd".into(), estimate);
        }
    }
    Ok(())
}
struct Configuration {
    index: usize,
    config: PathBuf,
    result_dir: PathBuf,
    agent: String,
    workload: String,
    provider: String,
    model: String,
    concurrency: usize,
    batch: Option<u64>,
    expected_execution: Option<Value>,
    measurement_snapshot: Option<Value>,
    measurement_override: Option<Option<PathBuf>>,
    standalone: Option<crate::benchmark::BenchmarkConfig>,
    selection: Option<crate::workloads::TaskSelection>,
    outcomes: BTreeMap<usize, outcomes::Recorded>,
}
fn load_configuration(
    request: &Request,
    index: usize,
    reference: &str,
) -> Result<Configuration, Error> {
    let path = request.path.as_str();
    let parent = request.parent.as_path();
    let sweep_results = &request.sweep_results;
    let single = request.single;
    let plan = &request.plan;
    let config = checked_benchmark_reference(parent, reference, path)?;
    let mut result_dir = sweep_results.join(format!("{index:03}-{}", benchmark_stem(&config)));
    let text = &request.configurations[&config];
    let (agent, workload, provider, model, concurrency) = benchmark_identity_text(&config, text)?;
    let configuration =
        toml::from_str::<crate::benchmark::BenchmarkConfig>(text).map_err(|error| {
            Error::Input(format!(
                "cannot parse benchmark {}: {error}",
                config.display()
            ))
        })?;
    let standalone = single.then(|| configuration.clone());
    if let Some(configuration) = &standalone {
        result_dir = crate::runner::resolve_log_path(&config, &configuration.log_file)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
    }
    let selection = plan
        .as_ref()
        .map(|plan| plan.entries[index].selection.clone());
    let mut outcomes = if let Some(configuration) = &standalone {
        outcomes::load_run(&config, configuration)?
    } else {
        outcomes::load(&result_dir, concurrency)?
    };
    if let Some(plan) = &plan {
        let mut imports = crate::sweep::recovered::load(
            &sweep_results.join(".checkpoint"),
            &result_dir,
            index,
            &plan.entries[index],
        )?;
        let closed = crate::sweep::closed_slot_reports(
            &sweep_results.join(".checkpoint"),
            &result_dir,
            index,
            &plan.entries[index],
        )?;
        for (slot, (_, value)) in &closed {
            if let Some((_, imported)) = imports.get(slot) {
                if imported["task"] != value["task"]
                    || imported["workload_success"] != value["workload_success"]
                {
                    return Err(Error::Input(
                        "closed slot conflicts with its imported grade; preserve both records"
                            .into(),
                    ));
                }
                imports.remove(slot);
            }
        }
        outcomes::add_imports(&mut outcomes, imports)?;
        outcomes::add_closed(&mut outcomes, closed)?;
    } else if !single
        && (result_dir.join(".asb-imported-grades").exists()
            || sweep_results.join(".checkpoint/closed-slots").exists())
    {
        return Err(Error::Input(
            "retained grades require their original saved sweep plan".into(),
        ));
    }
    if !single {
        outcomes::add_sweep_logs(&mut outcomes, &result_dir, &configuration)?;
    }
    Ok(Configuration {
        index,
        config,
        result_dir,
        agent,
        workload,
        provider,
        model,
        concurrency,
        batch: None,
        expected_execution: None,
        measurement_snapshot: None,
        measurement_override: None,
        standalone,
        selection,
        outcomes,
    })
}
fn load_attempt(
    request: &Request,
    context: &Configuration,
    replica: usize,
    proofs: &mut Vec<crate::cassette::TrackEvidence>,
    recording_owner: Option<&campaign::RecordingContext<'_>>,
) -> Result<BenchmarkData, Error> {
    let index = context.index;
    let config = &context.config;
    let result_dir = &context.result_dir;
    let (agent, workload, provider, model) = (
        context.agent.clone(),
        context.workload.clone(),
        context.provider.clone(),
        context.model.clone(),
    );
    let concurrency = context.concurrency;
    let standalone = &context.standalone;
    let selection = &context.selection;
    let outcomes = &context.outcomes;
    let parent = request.parent.as_path();
    let single = request.single;
    let sweep_results = &request.sweep_results;
    let label = if concurrency == 1 {
        format!("{index:03}-{}", benchmark_stem(config))
    } else {
        format!(
            "{index:03}-{}-concurrency-{replica}",
            benchmark_stem(config)
        )
    };
    let measurement_dir = if let Some(directory) = &context.measurement_override {
        directory.clone()
    } else if let Some(configuration) = &standalone {
        let selected = crate::runner::replica::configuration(
            configuration,
            crate::runner::replica::Identity {
                index: replica,
                total: concurrency,
            },
        )?;
        let (root, _) = crate::project::load(config)?;
        selected
            .measurements
            .as_ref()
            .filter(|value| value.enabled)
            .map(|value| crate::project::resolve(&root, &value.directory))
    } else {
        Some(result_dir.join(if concurrency == 1 {
            "measurements".to_owned()
        } else {
            format!("measurements-concurrency-{replica}")
        }))
    };
    let measurement_dir = if single && context.measurement_override.is_none() {
        outcomes
            .get(&replica)
            .and_then(|record| record.summary.get("measurement_directory"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or(measurement_dir)
    } else {
        measurement_dir
    };
    let capture_enabled = measurement_dir.is_some();
    let measurement_dir =
        measurement_dir.unwrap_or_else(|| result_dir.join(".disabled-measurements"));
    let mut run_path = measurement_dir.join("run.json");
    let artifact_sources: BTreeMap<_, _> = if capture_enabled {
        artifact_inventory(parent, &measurement_dir)
            .into_iter()
            .collect()
    } else {
        BTreeMap::new()
    };
    let artifacts = artifact_sources.keys().cloned().collect();
    let outcome = outcomes.get(&replica);
    let grade_only_closed =
        outcome.is_some_and(|record| record.source == "recovered-official-grade");
    let measured = capture_enabled
        && (if context.expected_execution.is_some() {
            context.measurement_snapshot.is_some()
        } else {
            run_path.is_file()
        })
        && !grade_only_closed;
    if !measured && outcome.is_none() {
        let workload_evaluation = selection.as_ref().map_or_else(
            || {
                crate::workloads::evaluation_kind(&workload, Path::new(""))
                    .as_str()
                    .into()
            },
            |selection| selection.evaluation_kind.clone(),
        );
        return Ok(BenchmarkData {
            execution: None,
            observation: Default::default(),
            id: RowId {
                configuration: context.index,
                batch: context.batch,
                replica,
            },
            legacy_evaluation_provenance: None,
            execution_plan_fingerprint: None,
            module: selection.as_ref().and_then(|s| s.module.clone()),
            benchmark_outcome: None,
            planning: None,
            effective_budgets: None,
            result_source: ResultSource::Missing,
            reconciled: false,
            replica_index: None,
            execution_cohort: None,
            label,
            directory: result_dir.clone(),
            agent: agent.clone(),
            workload: workload.clone(),
            task: None,
            selection: selection.clone(),
            evaluation_completed: false,
            workload_evaluation,
            provider: provider.clone(),
            model: model.clone(),
            metrics: BTreeMap::new(),
            series: BTreeMap::new(),
            host_metadata: None,
            components: None,
            collector_documents: BTreeMap::new(),
            status: Some(format!(
                "missing measurements: expected {} (measurement collection may be disabled, or the run stopped before writing results)",
                run_path.display()
            )),
            artifacts,
            artifact_sources,
        });
    }
    let mut run_value = if measured {
        let measured = if let Some(snapshot) = &context.measurement_snapshot {
            snapshot.clone()
        } else {
            read_json(&run_path)?
        };
        if let Some(outcome) = outcome {
            outcome.check_measurement(&measured)?;
        }
        measured
    } else {
        let outcome = outcome.expect("a recorded outcome is required without measurements");
        run_path = outcome.path.clone();
        outcome.summary.clone()
    };
    // Measurement agreement above binds the same task/execution/cohort before
    // borrowing optional historical policy objects. Never consult today's
    // configuration/defaults or a different/older attempt for hash inputs.
    let planning = outcomes::recorded_object(&run_value, outcome, "planning")?;
    let effective_budgets = outcomes::recorded_object(&run_value, outcome, "effective_budgets")?;
    for (field, value) in [
        ("planning", &planning),
        ("effective_budgets", &effective_budgets),
    ] {
        if let Some(value) = value {
            run_value[field] = value.clone();
        }
    }
    let DecodedRecord {
        task,
        replica_index,
        execution_cohort,
        workload_evaluation,
        identity_bound,
        run_status,
    } = decode_record(
        &run_value,
        &AttemptContext {
            configuration: context,
            replica,
            path: &run_path,
            measured,
            outcome,
        },
    )?;
    if context.expected_execution.is_some() {
        campaign::collect_summary_recording(&run_value, proofs, recording_owner)?;
    }
    let mut metrics = BTreeMap::new();
    let legacy_evaluation_provenance: Option<
        crate::workloads::outcome::LegacyEvaluationProvenance,
    > = run_value
        .get("legacy_evaluation_provenance")
        .filter(|value| !value.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|_| Error::Input("invalid retained grade provenance".into()))?;
    if let Some(provenance) = &legacy_evaluation_provenance
        && (provenance.execution_attempt.as_deref() != task.as_ref().map(|t| t.attempt_id.as_str())
            || provenance.evaluation_id.is_some()
            || provenance.supersedes.is_some()
            || provenance
                .evidence_manifest_sha256
                .as_ref()
                .is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())
                }))
    {
        return Err(Error::Input("retained grade provenance differs from original attempt or invents evaluation identity".into()));
    }
    let execution_plan_fingerprint: Option<String> = run_value
        .get("execution_plan_fingerprint")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|_| Error::Input("malformed recorded execution plan fingerprint".into()))?;
    if execution_plan_fingerprint.as_ref().is_some_and(|v| {
        v.is_empty()
            || v.len() > 256
            || !v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b":._-".contains(&b))
    }) {
        return Err(Error::Input(
            "invalid recorded execution plan fingerprint".into(),
        ));
    }
    let module: Option<crate::workloads::contracts::ModuleDescriptor> = run_value
        .get("module")
        .filter(|v| !v.is_null())
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|e| Error::Input(format!("invalid recorded module descriptor: {e}")))?;
    if module.as_ref().is_some_and(|m| {
        !m.valid() || m.id != workload || task.as_ref().is_some_and(|t| m.dataset != t.dataset)
    }) {
        return Err(Error::Input(
            "recorded module differs from task identity".into(),
        ));
    }
    if let (Some(recorded), Some(planned)) =
        (&module, selection.as_ref().and_then(|s| s.module.as_ref()))
        && recorded != planned
    {
        return Err(Error::Input(
            "recorded module differs from saved protocol selection".into(),
        ));
    }
    let module = module.or_else(|| selection.as_ref().and_then(|s| s.module.clone()));
    let benchmark_outcome =
        crate::workloads::outcome::BenchmarkOutcome::read(&run_value).map_err(Error::Input)?;
    if benchmark_outcome.is_some() && task.is_none() {
        return Err(Error::Input(
            "structured benchmark outcome has no execution task identity".into(),
        ));
    }
    if module.is_none()
        && benchmark_outcome
            .as_ref()
            .is_some_and(|o| o.policy != crate::workloads::outcome::MetricPolicy::default())
    {
        return Err(Error::Input(
            "custom metric policy has no recorded module provenance".into(),
        ));
    }
    if let (Some(module), Some(outcome)) = (&module, &benchmark_outcome)
        && !outcome.policy.matches_module(module)
    {
        return Err(Error::Input(
            "recorded metric policy differs from recorded module protocol".into(),
        ));
    }
    if let (Some(outcome), Some(task)) = (&benchmark_outcome, &task)
        && outcome.lineage.execution_attempt != task.attempt_id
    {
        return Err(Error::Input(
            "evaluation lineage differs from execution attempt".into(),
        ));
    }
    if let Some(outcome) = &benchmark_outcome
        && (outcome.execution_success != run_value["process_success"].as_bool()
            || (crate::workloads::resolution_eligible(&workload_evaluation)
                && outcome.evaluation.resolution() != run_value["workload_success"].as_bool()))
    {
        return Err(Error::Input(
            "structured outcome conflicts with legacy execution or resolution".into(),
        ));
    }
    let evaluation_completed = identity_bound
        && crate::workloads::resolution_eligible(&workload_evaluation)
        && benchmark_outcome.as_ref().map_or_else(
            || {
                run_value
                    .get("workload_success")
                    .and_then(Value::as_bool)
                    .is_some()
            },
            |outcome| outcome.evaluation.completed(),
        );
    flatten_numbers("", &run_value, &mut metrics)?;
    metrics.retain(|key, _| {
        ![
            "benchmark_outcome.",
            "module.",
            "planning.",
            "effective_budgets.",
        ]
        .iter()
        .any(|prefix| key.starts_with(prefix))
    });
    if let Some(outcome) = &benchmark_outcome
        && crate::workloads::resolution_eligible(&workload_evaluation)
        && let crate::workloads::outcome::EvaluationOutcome::Completed { rewards, .. } =
            &outcome.evaluation
    {
        for (name, value) in rewards {
            let unit = &outcome.policy.rewards[name].unit;
            metrics.insert(
                format!(
                    "benchmark_rewards.{}@{}.{}.{}",
                    path_key(&outcome.policy.id),
                    outcome.policy.version,
                    path_key(name),
                    path_key(unit)
                ),
                *value,
            );
        }
    }
    // Correct old presentations without rewriting their raw evidence.
    if !crate::workloads::resolution_eligible(&workload_evaluation)
        || benchmark_outcome
            .as_ref()
            .is_some_and(|o| o.evaluation.resolution().is_none())
    {
        metrics.remove("success_rate");
    } else {
        let resolved = evaluation_completed
            && run_value.get("workload_success").and_then(Value::as_bool) == Some(true);
        metrics.insert("success_rate".into(), if resolved { 1.0 } else { 0.0 });
    }
    if run_value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        < 2
    {
        metrics.remove("steps");
    }
    let (series, host_metadata, components, collector_documents) =
        load_collectors(&measurement_dir, measured, &mut metrics)?;
    Ok(BenchmarkData {
        execution: run_value
            .get("execution")
            .filter(|value| !value.is_null())
            .cloned(),
        observation: if grade_only_closed
            || outcome.is_some_and(|record| record.source == "imported-official-grade")
        {
            Default::default()
        } else {
            outcomes::recorded_observation(&run_value, outcome)?
        },
        id: RowId {
            configuration: index,
            batch: context.batch,
            replica,
        },
        planning,
        effective_budgets,
        legacy_evaluation_provenance,
        execution_plan_fingerprint,
        module,
        benchmark_outcome,
        result_source: if measured {
            ResultSource::Measurements
        } else {
            ResultSource::parse(outcome.expect("recorded outcome").source)?
        },
        reconciled: if grade_only_closed {
            true
        } else if single
            || outcome.is_some_and(|outcome| outcome.source == "imported-official-grade")
        {
            false
        } else {
            reconciliation::recorded(
                sweep_results,
                result_dir,
                index,
                replica,
                concurrency,
                task.as_ref(),
            )?
        },
        replica_index,
        execution_cohort,
        label,
        directory: result_dir.clone(),
        agent: agent.clone(),
        workload: workload.clone(),
        task,
        selection: selection.clone(),
        evaluation_completed,
        workload_evaluation,
        provider: provider.clone(),
        model: model.clone(),
        metrics,
        series,
        host_metadata,
        components,
        collector_documents,
        status: run_status,
        artifacts,
        artifact_sources,
    })
}
struct AttemptContext<'a> {
    configuration: &'a Configuration,
    replica: usize,
    path: &'a Path,
    measured: bool,
    outcome: Option<&'a outcomes::Recorded>,
}
struct DecodedRecord {
    task: Option<crate::workloads::TaskIdentity>,
    replica_index: Option<usize>,
    execution_cohort: Option<crate::measurements::ExecutionCohort>,
    workload_evaluation: String,
    identity_bound: bool,
    run_status: Option<String>,
}
fn decode_record(run_value: &Value, attempt: &AttemptContext<'_>) -> Result<DecodedRecord, Error> {
    let context = attempt.configuration;
    let replica = attempt.replica;
    let run_path = attempt.path;
    let measured = attempt.measured;
    let outcome = attempt.outcome;
    let Configuration {
        agent,
        workload,
        provider,
        model,
        concurrency,
        standalone,
        selection,
        ..
    } = context;
    let concurrency = *concurrency;
    if let Some(expected) = &context.expected_execution
        && run_value.get("execution") != Some(expected)
    {
        return Err(Error::Input(
            "campaign result differs from its planned execution identity".into(),
        ));
    }
    let single = standalone.is_some();
    let has_plan = selection.is_some();
    if single || has_plan || outcome.is_some() {
        for (field, expected) in [
            ("agent", &agent),
            ("workload", &workload),
            ("provider", &provider),
            ("model", &model),
        ] {
            if run_value.get(field).and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(Error::Input(format!(
                    "result {} does not match selected `{field}`; inspect the attempt instead of mixing results from another configuration",
                    run_path.display()
                )));
            }
        }
    }
    let execution_cohort = run_value
        .get("execution_cohort")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<crate::measurements::ExecutionCohort>(value.clone()))
        .transpose()
        .map_err(|error| {
            Error::Input(format!(
                "invalid execution cohort in {}: {error}",
                run_path.display()
            ))
        })?;
    let replica_index = run_value
        .get("replica_index")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    Error::Input(format!("invalid replica index in {}", run_path.display()))
                })
        })
        .transpose()?;
    if execution_cohort.is_some() != replica_index.is_some()
        || execution_cohort.as_ref().is_some_and(|cohort| {
            replica_index != Some(replica) || !cohort.admits(replica, concurrency)
        })
    {
        return Err(Error::Input(format!(
            "execution cohort does not match replica selection in {}",
            run_path.display()
        )));
    }
    if let Some(cohort) = &execution_cohort {
        crate::planning::validate_recorded_dispatch(run_value, cohort, replica, concurrency)?;
    }
    let task = run_value
        .get("task")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<crate::workloads::TaskIdentity>(value.clone()))
        .transpose()
        .map_err(|error| {
            Error::Input(format!(
                "invalid task identity in {}: {error}",
                run_path.display()
            ))
        })?;
    if context.expected_execution.is_some()
        && (task.is_none() || replica_index != Some(replica) || execution_cohort.is_none())
    {
        return Err(Error::Input(
            "campaign result lacks its actual task or replica/cohort identity".into(),
        ));
    }
    if let Some(configuration) = &standalone
        && task.as_ref().is_some_and(|task| {
            task.workload != *workload
                || configuration
                    .trial
                    .as_ref()
                    .is_some_and(|trial| trial != &task.task_id)
        })
    {
        return Err(Error::Input(format!(
            "result {} does not match the benchmark's selected task",
            run_path.display()
        )));
    }
    if let (Some(selection), Some(task)) = (&selection, &task)
        && !selection.matches(task)
    {
        return Err(Error::Input(format!(
            "measurement {} does not match the planned dataset instance",
            run_path.display()
        )));
    }
    if (single || has_plan || outcome.is_some())
        && let Some(task) = &task
        && (task.attempt_id.is_empty() || task.attempt_id.len() > 256 || task.workload != *workload)
    {
        return Err(Error::Input(format!(
            "result {} has a mismatched, missing, oversized or duplicate actual attempt identity; an attempt cannot be counted twice",
            run_path.display()
        )));
    }
    let identity_bound = selection
        .as_ref()
        .is_none_or(|selection| task.as_ref().is_some_and(|task| selection.matches(task)));
    // The legacy success flag is grade-derived, not operational disposition.
    // Keep historical category strings display-only; only actual recorded
    // failures/cleanup uncertainty override healthy pending or native rewards.
    let recorded_failure = run_value
        .get("failure_reason")
        .and_then(Value::as_str)
        .is_some()
        || run_value
            .get("failure_kind")
            .and_then(Value::as_str)
            .is_some()
        || run_value["benchmark_outcome"]["cleanup_confirmed"].as_bool() == Some(false);
    let healthy_unresolved_evaluation = !recorded_failure
        && run_value["process_success"] == true
        && ((run_value["benchmark_outcome"]["evaluation"]["status"] == "completed"
            && run_value["benchmark_outcome"]["evaluation"]["resolution"].is_null())
            || (run_value["status"] == "evaluation-pending"
                && run_value["benchmark_outcome"]["evaluation"]["status"] == "pending"));
    let run_status = if !measured {
        outcome.map(outcomes::Recorded::status)
    } else {
        match run_value.get("status").and_then(Value::as_str) {
            Some(status)
                if !recorded_failure
                    && run_value.get("workload_evaluation").and_then(Value::as_str)
                        == Some("adapter-smoke") =>
            {
                Some(format!(
                    "{status}: adapter smoke only; upstream task resolution not measured"
                ))
            }
            Some("unscored") if !recorded_failure => Some(
                "unscored: initial task was already passing or its baseline could not be verified"
                    .into(),
            ),
            Some(status)
                if recorded_failure
                    || (run_value.get("success") == Some(&Value::Bool(false))
                        && !healthy_unresolved_evaluation) =>
            {
                Some(
                    if let Some(reason) = run_value.get("failure_reason").and_then(Value::as_str) {
                        format!("benchmark run failed: {reason}")
                    } else {
                        format!(
                            "benchmark run failed (status={status}, exit_code={})",
                            run_value
                                .get("exit_code")
                                .map(Value::to_string)
                                .unwrap_or_else(|| "null".into())
                        )
                    },
                )
            }
            Some(status) => Some(status.to_owned()),
            None => Some("unknown".into()),
        }
    };
    let workload_evaluation = run_value
        .get("workload_evaluation")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let workload_evaluation = crate::workloads::scored_kind(
        &workload_evaluation,
        &crate::workloads::BaselineEvidence::from_record(run_value),
    )
    .to_owned();
    if let Some(selection) = selection
        .as_ref()
        .filter(|selection| selection.dataset.is_some())
        && crate::workloads::resolution_eligible(&workload_evaluation)
        && workload_evaluation != selection.evaluation_kind
    {
        return Err(Error::Input(format!(
            "measurement {} does not match the planned evaluator; a different task grader cannot earn this benchmark's score",
            run_path.display()
        )));
    }
    Ok(DecodedRecord {
        task,
        replica_index,
        execution_cohort,
        workload_evaluation,
        identity_bound,
        run_status,
    })
}
type CollectorRecords = (
    BTreeMap<String, TimeSeries>,
    Option<Value>,
    Option<Value>,
    BTreeMap<String, Value>,
);
fn load_collectors(
    measurement_dir: &Path,
    measured: bool,
    metrics: &mut BTreeMap<String, f64>,
) -> Result<CollectorRecords, Error> {
    let mut series = BTreeMap::new();
    let mut host_metadata = None;
    let mut components = None;
    let mut documents = BTreeMap::new();
    let collector_files = if measured {
        Some(fs::read_dir(measurement_dir).map_err(|error| Error::Input(error.to_string()))?)
    } else {
        None
    };
    for entry in collector_files.into_iter().flatten() {
        let file = entry
            .map_err(|error| Error::Input(error.to_string()))?
            .path();
        if file.extension().and_then(|value| value.to_str()) != Some("json")
            || file.file_name().and_then(|value| value.to_str()) == Some("run.json")
        {
            continue;
        }
        let collector = read_json(&file)?;
        let name = file
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("collector");
        documents.insert(name.to_owned(), collector.clone());
        if name == "system" {
            host_metadata = Some(collector.clone());
            flatten_numbers("host", &collector, metrics)?;
        }
        if name == "components" {
            components = Some(collector.clone());
            if let Some(component_values) = collector.get("components") {
                flatten_numbers("component", component_values, metrics)?;
            }
        }
        if let Some(collector_metrics) = collector.get("metrics") {
            flatten_numbers(&path_key(name), collector_metrics, metrics)?;
        }
        // Collector stdout is retained for diagnosis, but is not a
        // numerical series: scraping its last token created noisy and
        // misleading graphs for repeated mpstat/iostat samples.
        if let Some(collector_series) = collector.get("series") {
            let collector_timestamps = collector.get("series_timestamps");
            let timestamp_map = collector_timestamps.and_then(Value::as_object);
            if let Some(series_map) = collector_series.as_object() {
                if collector_timestamps.is_some_and(|value| !value.is_object()) {
                    return Err(Error::Input(format!(
                        "invalid series timestamps in {}: an object keyed by metric name is required for object-valued series",
                        file.display()
                    )));
                }
                for (metric, raw_values) in series_map {
                    insert_unique(
                        &mut series,
                        metric.clone(),
                        TimeSeries::parse(
                            raw_values,
                            timestamp_map.and_then(|map| map.get(metric)),
                            &file,
                            metric,
                        )?,
                    )?;
                }
            } else {
                insert_unique(
                    &mut series,
                    name.to_owned(),
                    TimeSeries::parse(collector_series, collector_timestamps, &file, name)?,
                )?;
            }
        }
    }
    Ok((series, host_metadata, components, documents))
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    fn pricing_request(root: &Path) -> Request {
        Request {
            path: root.join("sweep.toml").display().to_string(),
            parent: root.to_path_buf(),
            single: false,
            sweep: crate::sweep::SweepFile {
                resources: None,
                repeat: None,
                configs: Vec::new(),
            },
            sweep_results: root.to_path_buf(),
            comparison_dir: root.join("comparison"),
            plan: None,
            campaign: None,
            configurations: BTreeMap::new(),
        }
    }

    fn pricing_fixture(check: impl FnOnce(&Path)) {
        let root = std::env::temp_dir().join(format!(
            "asb-pricing-snapshot-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        check(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_dispatches_pricing_only_to_the_matching_provider() {
        pricing_fixture(|root| {
            std::fs::write(root.join("openrouter-pricing.json"), r#"{"schema_version":1,"provider":"openrouter","models":{"fixture":{"prompt":"0.000002","completion":"0.000004"}}}"#).unwrap();
            let make_row = |provider: &str| BenchmarkData {
                observation: Default::default(),
                id: crate::compare::RowId {
                    configuration: 0,
                    batch: None,
                    replica: 0,
                },
                execution: None,
                planning: None,
                effective_budgets: None,
                legacy_evaluation_provenance: None,
                execution_plan_fingerprint: None,
                module: None,
                benchmark_outcome: None,
                result_source: ResultSource::Missing,
                reconciled: false,
                replica_index: None,
                execution_cohort: None,
                label: "fixture".into(),
                directory: root.into(),
                agent: "fixture".into(),
                workload: "fixture".into(),
                task: None,
                selection: None,
                evaluation_completed: false,
                workload_evaluation: "fixture".into(),
                provider: provider.into(),
                model: "fixture".into(),
                metrics: BTreeMap::from([
                    ("token_usage.input_tokens".into(), 100.0),
                    ("token_usage.output_tokens".into(), 25.0),
                ]),
                series: BTreeMap::new(),
                host_metadata: None,
                components: None,
                collector_documents: BTreeMap::new(),
                status: None,
                artifacts: Vec::new(),
                artifact_sources: BTreeMap::new(),
            };
            let mut rows = [make_row("openrouter"), make_row("unknown")];
            apply_pricing(&pricing_request(root), &mut rows).unwrap();
            assert!((rows[0].metrics["estimated_cost_usd"] - 0.0003).abs() < 1e-12);
            assert!(!rows[1].metrics.contains_key("estimated_cost_usd"));
        });
    }

    #[test]
    fn retained_pricing_rejects_an_unrecognized_snapshot_schema() {
        pricing_fixture(|root| {
            std::fs::write(
                root.join("openrouter-pricing.json"),
                r#"{"schema_version":2,"provider":"untrusted","models":{}}"#,
            )
            .unwrap();
            let error = apply_pricing(&pricing_request(root), &mut [])
                .unwrap_err()
                .to_string();
            assert!(error.contains("invalid OpenRouter pricing snapshot schema"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn retained_pricing_rejects_a_symlinked_snapshot() {
        use std::os::unix::fs::symlink;

        pricing_fixture(|root| {
            let target = root.join("untrusted-pricing.json");
            std::fs::write(
                &target,
                r#"{"schema_version":1,"provider":"openrouter","models":{}}"#,
            )
            .unwrap();
            symlink(&target, root.join("openrouter-pricing.json")).unwrap();
            let error = apply_pricing(&pricing_request(root), &mut [])
                .unwrap_err()
                .to_string();
            assert!(error.contains("must be a regular file"));
        });
    }
}
