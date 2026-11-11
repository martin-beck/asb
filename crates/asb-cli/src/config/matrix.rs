//! Selection expansion and bounded, non-overwriting sweep publication.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod prepared;
mod publication;
pub(crate) use publication::scalar_if_absent;
mod selections;
pub(crate) use prepared::Population;
pub(crate) use publication::scalar as publish_atomic;

pub(super) fn setup_at(
    mut options: SetupOptions,
    base: &Path,
    output: &mut dyn Write,
) -> Result<(), Error> {
    for values in [
        &options.agent,
        &options.provider,
        &options.model,
        &options.workload,
    ] {
        required(values.first().cloned())?;
    }
    for value in options
        .agent
        .iter()
        .chain(&options.provider)
        .chain(&options.model)
        .chain(&options.workload)
        .chain(&options.trial)
        .chain(&options.trial_count)
        .chain(&options.trial_set)
        .chain(&options.measure)
    {
        required(Some(value.clone()))?;
    }
    for value in &options.trial_count {
        let count = value
            .split_once('=')
            .map_or(value.as_str(), |(_, count)| count);
        if count
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .is_none()
        {
            return Err(Error::Input(format!(
                "--trial-count `{value}` must specify a positive integer"
            )));
        }
    }
    let requested_repeats = repeated(&options);
    // An explicit incompatible pair is a user input error. An `all` expansion,
    // however, denotes every runnable combination and therefore omits static
    // endpoint mismatches rather than making the whole generated sweep invalid.
    let filter_incompatible = [&options.agent, &options.provider, &options.model]
        .into_iter()
        .any(|values| values.iter().any(|value| value == "all"));
    selections::expand(&mut options, base)?;
    for workload in &options.workload {
        if !scoped(&options.trial_count, workload).is_empty()
            && !scoped(&options.trial, workload).is_empty()
        {
            return Err(Error::Input(format!(
                "--trial-count and --trial cannot both select workload `{workload}`; scope them to different workloads"
            )));
        }
    }
    crate::workloads::ensure_native_architecture(
        options.workload.iter().map(String::as_str),
        std::env::consts::ARCH,
    )?;
    validate_concurrency_before_install(options.concurrency)?;
    if !filter_incompatible {
        validate_before_install(&options)?;
    }
    prepare_missing_installables(&options, base, output)?;
    let repeated = requested_repeats || repeated(&options);
    for measure in &options.measure {
        if crate::collectors::adapter_for_selection(measure).is_none() {
            return Err(Error::Input(format!(
                "unknown measurement collector or option `{measure}`; use `asb measure` to see collectors and their available options"
            )));
        }
    }
    for owner in options
        .measure
        .iter()
        .filter_map(|measure| {
            crate::collectors::adapter_for_selection(measure).map(|adapter| adapter.id().to_owned())
        })
        .collect::<BTreeSet<_>>()
    {
        if !options.measure.contains(&owner) {
            options.measure.push(owner);
        }
    }
    let (root, settings) = project::load(base)?;
    let directory = project::environment_path("ASB_WORKLOAD_DIR", base)
        .unwrap_or_else(|| project::resolve(&root, &settings.workloads_dir));
    let mut sets = BTreeMap::new();
    for filename in unique(&options.trial_set) {
        let path = Path::new(filename);
        if path.is_absolute()
            || path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return Err(Error::Input(format!(
                "trial-set path `{filename}` must be relative to the workloads directory"
            )));
        }
        let set = crate::workloads::read_trial_set(&directory.join(path)).map_err(Error::Input)?;
        if !options.workload.contains(&set.workload) {
            return Err(Error::Input(format!(
                "trial set `{filename}` belongs to unselected workload `{}`",
                set.workload
            )));
        }
        if sets
            .insert(set.workload.clone(), (filename.clone(), set))
            .is_some()
        {
            return Err(Error::Input(
                "select only one trial set per workload".into(),
            ));
        }
    }
    for (values, selected, flag) in [
        (&options.model, &options.provider, "model"),
        (&options.trial, &options.workload, "trial"),
        (&options.trial_count, &options.workload, "trial-count"),
    ] {
        for value in values {
            if let Some((scope, _)) = value.split_once('=')
                && !selected.iter().any(|name| name == scope)
            {
                return Err(Error::Input(format!(
                    "--{flag} `{value}` refers to an unselected scope `{scope}`"
                )));
            }
        }
    }
    let mut sampled_trials = Vec::new();
    let mut configs = Population::default();
    for workload in unique(&options.workload) {
        let catalog =
            crate::workloads::planning_catalog(workload, &directory).map_err(Error::Input)?;
        let set = sets.get(workload);
        let enabled_trials = set.map(|(_, set)| {
            set.trials
                .iter()
                .map(|trial| trial.id.clone())
                .collect::<BTreeSet<_>>()
        });
        let mut validated_trials = BTreeSet::new();
        let counts = scoped(&options.trial_count, workload);
        if counts.len() > 1 {
            return Err(Error::Input(format!(
                "select one --trial-count for workload `{workload}`"
            )));
        }
        let trials: Vec<Option<String>> = if let Some(value) = counts.first() {
            use std::hash::BuildHasher;
            let count = value
                .parse::<usize>()
                .ok()
                .filter(|count| *count > 0)
                .ok_or_else(|| {
                    Error::Input(format!(
                        "--trial-count `{value}` must be a positive integer"
                    ))
                })?;
            let available = enabled_trials
                .as_ref()
                .or_else(|| catalog.as_ref().map(|catalog| &catalog.task_ids))
                .ok_or_else(|| {
                    Error::Input(format!(
                        "workload `{workload}` has no installed trial catalog to sample"
                    ))
                })?;
            let ids: Vec<_> = available.iter().cloned().collect();
            let identity = crate::workloads::TaskIdentity::local("trial-sample", Some(workload));
            let seed = std::collections::hash_map::RandomState::new().hash_one(identity.attempt_id);
            let order = crate::cassette::TrialOrder::new(&ids, count, seed).map_err(|error| {
                Error::Input(format!("cannot sample workload `{workload}`: {error}"))
            })?;
            let trials = order.initial().iter().cloned().map(Some).collect();
            sampled_trials.push(crate::sweep::SampledTrials {
                workload: workload.clone(),
                count,
                seed: format!("{seed:016x}"),
                candidates: order.candidates().to_vec(),
            });
            trials
        } else if options.trial.is_empty()
            || (!options.trial_count.is_empty() && scoped(&options.trial, workload).is_empty())
        {
            set.map_or_else(
                || vec![None],
                |(_, set)| {
                    set.trials
                        .iter()
                        .map(|trial| Some(trial.id.clone()))
                        .collect()
                },
            )
        } else {
            selections::trials(
                &scoped(&options.trial, workload),
                catalog.as_ref(),
                enabled_trials.as_ref(),
                workload,
            )?
        };
        if trials.is_empty() {
            return Err(Error::Input(format!(
                "no trials selected for workload `{workload}`"
            )));
        }
        for provider in unique(&options.provider) {
            let models = scoped(&options.model, provider);
            if models.is_empty() {
                return Err(Error::Input(format!(
                    "no models selected for provider `{provider}`"
                )));
            }
            for agent in unique(&options.agent) {
                for model in &models {
                    let adapter = agents::find(agent).expect("expanded agent is registered");
                    let endpoint = if filter_incompatible {
                        let endpoint = crate::endpoint::choose_from(
                            adapter.id(),
                            adapter.supported_endpoints(),
                            provider,
                            model,
                            &providers::offered_endpoints(provider, model),
                        );
                        let Ok(endpoint) = endpoint else {
                            continue;
                        };
                        Some(endpoint)
                    } else {
                        None
                    };
                    for effort in unique_efforts(&options.thinking_effort) {
                        if endpoint.as_ref().is_some_and(|endpoint| {
                            effort.is_some()
                                && !adapter
                                    .supports_thinking_effort(endpoint.interface, effort.unwrap())
                        }) {
                            continue;
                        }
                        for trial in &trials {
                            if configs.len() >= crate::sweep::MAX_CONFIGS {
                                return Err(Error::Input(format!(
                                    "setup selection exceeds the {}-configuration sweep limit; select a smaller subset",
                                    crate::sweep::MAX_CONFIGS
                                )));
                            }
                            let selection = SetupOptions {
                                agent: vec![agent.clone()],
                                provider: vec![provider.clone()],
                                model: vec![model.clone()],
                                thinking_effort: vec![effort],
                                native_snapshot_record: options.native_snapshot_record,
                                workload: vec![workload.clone()],
                                trial: trial.iter().cloned().collect(),
                                trial_count: Vec::new(),
                                trial_set: set
                                    .map(|(filename, _)| filename.clone())
                                    .into_iter()
                                    .collect(),
                                measure: options.measure.clone(),
                                concurrency: options.concurrency,
                                workspace: options.workspace.clone(),
                                output: options.output.clone(),
                                retention: options.retention.clone(),
                                repeat: None,
                            };
                            let config = configuration_at(selection, base)?;
                            if validated_trials.insert(config.trial.clone()) {
                                validate_selection(
                                    &config,
                                    &directory,
                                    catalog.as_ref(),
                                    enabled_trials.as_ref(),
                                )?;
                            }
                            configs.push(config)?;
                        }
                    }
                }
            }
        }
    }
    let manifest_repeat = options.repeat;
    let configs = configs.into_configs();
    if repeated || configs.len() > 1 || !sampled_trials.is_empty() {
        let path = options
            .output
            .map(|path| base.join(path))
            .unwrap_or_else(|| root.join(format!("asb-sweep-{}.toml", stamp())));
        publish_sweep_with_progress_and_repeat(
            &path,
            configs,
            manifest_repeat,
            sampled_trials,
            output,
            &mut |_, _| {},
        )?;
    } else {
        let config = &configs[0];
        let text = render(config)?;
        if let Some(path) = options.output {
            let target = base.join(&path);
            create_parent(&target, output)?;
            publication::scalar(&target, text.as_bytes())?;
            let shown = if std::env::current_dir().is_ok_and(|cwd| cwd == base) {
                PathBuf::from(path)
            } else {
                target.clone()
            };
            writeln!(
                output,
                "Wrote {}\n{}{}",
                shown.display(),
                run_command_message(&shown.to_string_lossy()),
                if config.native_snapshot_record {
                    " --record"
                } else {
                    ""
                }
            )
            .map_err(|error| publication::confirmation(&target, error))?;
        } else {
            output.write_all(text.as_bytes()).map_err(crate::io_error)?;
        }
    }
    Ok(())
}

/// Reject a numeric setup request before automatic installation can emit output
/// or mutate a project. Per-configuration validation retains the same checks
/// after expansion as a defensive boundary.
fn validate_concurrency_before_install(concurrency: Option<usize>) -> Result<(), Error> {
    let Some(concurrency) = concurrency else {
        return Ok(());
    };
    if concurrency == 0 {
        return Err(Error::Input("concurrency must be at least 1".into()));
    }
    if concurrency > crate::benchmark::MAX_CONCURRENCY {
        return Err(Error::Input(format!(
            "concurrency must not exceed {}; split larger selections into bounded sweeps",
            crate::benchmark::MAX_CONCURRENCY
        )));
    }
    Ok(())
}

/// Preserve input diagnostics before an automatic installer can perform side
/// effects.  `all` deliberately filters incompatible pairs later instead.
fn validate_before_install(options: &SetupOptions) -> Result<(), Error> {
    for agent in unique(&options.agent) {
        let adapter = agents::find(agent).ok_or_else(|| {
            let known = agents::all()
                .iter()
                .map(|item| item.id())
                .collect::<Vec<_>>();
            let suggestion = crate::suggestions::closest(agent, &known)
                .map(|candidate| format!("; did you mean `{candidate}`?"))
                .unwrap_or_default();
            Error::Input(format!("unknown agent `{agent}`{suggestion}"))
        })?;
        for provider in unique(&options.provider) {
            providers::model_spec(provider).ok_or_else(|| {
                let suggestion = crate::suggestions::closest(provider, providers::names())
                    .map(|candidate| format!("; did you mean `{candidate}`?"))
                    .unwrap_or_default();
                Error::Input(format!("unknown provider `{provider}`{suggestion}"))
            })?;
            for model in scoped(&options.model, provider) {
                let endpoint = crate::endpoint::choose_from(
                    adapter.id(),
                    adapter.supported_endpoints(),
                    provider,
                    &model,
                    &providers::offered_endpoints(provider, &model),
                )?;
                for effort in unique_efforts(&options.thinking_effort) {
                    crate::config::validate_thinking_effort(adapter, endpoint.interface, effort)?;
                }
            }
        }
    }
    Ok(())
}

/// Materialize explicitly selected local capabilities before writing a setup
/// result.  `all` is expanded first, so this treats explicit and all-selected
/// agents, external workloads, and collector dependencies identically.
fn prepare_missing_installables(
    options: &SetupOptions,
    base: &Path,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let (root, settings) = project::load(base)?;
    let tools = project::resolve(&root, &settings.tools_dir);
    let workloads = project::environment_path("ASB_WORKLOAD_DIR", base)
        .unwrap_or_else(|| project::resolve(&root, &settings.workloads_dir));
    for agent in unique(&options.agent) {
        if agents::find(agent).is_some() && !crate::install::tool_installed_locally(agent, &tools) {
            crate::install::tool_with_progress_at(
                crate::install::ToolInstallOptions {
                    name: agent.clone(),
                    source: None,
                    dir: None,
                    dry_run: false,
                },
                base,
                output,
                &mut |_, _| {},
            )?;
        }
    }
    // Some providers own a project-local runtime in addition to their endpoint
    // metadata.  Treat those runtimes exactly like selected agents: setup must
    // leave its generated configuration runnable without a second manual
    // `asb tool install` step.  Providers without a registered tool remain
    // endpoint-only and require no installation.
    for provider in unique(&options.provider) {
        if crate::tools::find(provider).is_some()
            && !crate::install::tool_installed_locally(provider, &tools)
        {
            crate::install::tool_with_progress_at(
                crate::install::ToolInstallOptions {
                    name: provider.clone(),
                    source: None,
                    dir: None,
                    dry_run: false,
                },
                base,
                output,
                &mut |_, _| {},
            )?;
        }
    }
    for workload in unique(&options.workload) {
        if crate::install::external_workload_names().any(|name| name == workload)
            && !crate::install::workload_available(workload, &workloads)
        {
            crate::install::workload_with_progress_at(
                crate::install::WorkloadInstallOptions {
                    source: workload.clone(),
                    name: None,
                    dir: None,
                },
                base,
                output,
                &mut |_, _| {},
            )?;
        }
    }
    for tool in collector_installables(&options.measure) {
        if !crate::install::tool_installed_locally(&tool, &tools) {
            crate::install::tool_with_progress_at(
                crate::install::ToolInstallOptions {
                    name: tool,
                    source: None,
                    dir: None,
                    dry_run: false,
                },
                base,
                output,
                &mut |_, _| {},
            )?;
        }
    }
    Ok(())
}

/// Setup owns the whole selected collector capability, not just its indirect
/// tools.  Some collectors (for example Perfetto) are themselves installable
/// tools, while others only contribute dependencies such as `perf`.
fn collector_installables(selected: &[String]) -> Vec<String> {
    let selected = if selected.is_empty() {
        measurements::default_collectors()
    } else {
        selected.to_vec()
    };
    let selected = selected
        .iter()
        .filter_map(|name| {
            crate::collectors::adapter_for_selection(name).map(|adapter| adapter.id().to_owned())
        })
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    for id in crate::collectors::expand_dependencies(&selected) {
        if crate::tools::find(&id).is_some() && !result.contains(&id) {
            result.push(id);
        }
    }
    for id in crate::collectors::dependency_tools(&selected) {
        let id = id.to_owned();
        if !result.contains(&id) {
            result.push(id);
        }
    }
    result
}

fn repeated(options: &SetupOptions) -> bool {
    options.agent.len() > 1
        || options.provider.len() > 1
        || options.model.len() > 1
        || unique_efforts(&options.thinking_effort).len() > 1
        || options.workload.len() > 1
        || options.trial.len() > 1
        || options.trial_set.len() > 1
        || options.measure.len() > 1
}

fn unique(values: &[String]) -> Vec<&String> {
    let mut result = Vec::new();
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    result
}

fn unique_efforts(
    values: &[Option<crate::benchmark::ThinkingEffort>],
) -> Vec<Option<crate::benchmark::ThinkingEffort>> {
    let values = if values.is_empty() {
        &[None][..]
    } else {
        values
    };
    let mut result = Vec::new();
    for value in values {
        if !result.contains(value) {
            result.push(*value);
        }
    }
    result
}

fn scoped(values: &[String], scope: &str) -> Vec<String> {
    let mut result = Vec::new();
    for value in values {
        let selected = match value.split_once('=') {
            Some((owner, value)) if owner == scope => Some(value),
            Some(_) => None,
            None => Some(value.as_str()),
        };
        if let Some(value) = selected
            && !result.iter().any(|item| item == value)
        {
            result.push(value.to_owned());
        }
    }
    result
}

pub(super) fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn render(config: &BenchmarkConfig) -> Result<String, Error> {
    prepared::source_bound(config)?;
    let text = toml::to_string_pretty(config)
        .map_err(|error| Error::Input(format!("cannot render configuration: {error}")))?;
    if text.len() > crate::benchmark::MAX_CONFIG_BYTES {
        return Err(Error::Input(
            "generated benchmark configuration exceeds 64 KiB; shorten selected values; no setup files were written".into(),
        ));
    }
    Ok(text)
}

fn create_parent(path: &Path, output: &mut dyn Write) -> Result<(), Error> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty())
        && !parent.exists()
    {
        writeln!(output, "Creating directory {}", parent.display()).map_err(crate::io_error)?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Io(format!(
                "cannot create directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

/// Shared CLI/TUI publisher: all selections must be validated before calling.
pub(crate) fn publish_sweep(
    path: &Path,
    configs: Vec<BenchmarkConfig>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    publish_sweep_with_repeat(path, configs, None, output)
}

fn publish_sweep_with_repeat(
    path: &Path,
    configs: Vec<BenchmarkConfig>,
    repeat: Option<u64>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    publish_sweep_with_progress_and_repeat(
        path,
        configs,
        repeat,
        Vec::new(),
        output,
        &mut |_, _| {},
    )
}

/// Publish a sweep while reporting each durable benchmark configuration and
/// the final sweep manifest. Callers may coalesce updates, but `total` always
/// includes that manifest as the last item.
pub(crate) fn publish_sweep_with_progress(
    path: &Path,
    configs: Vec<BenchmarkConfig>,
    output: &mut dyn Write,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(), Error> {
    publish_sweep_with_progress_and_repeat(path, configs, None, Vec::new(), output, progress)
}

fn publish_sweep_with_progress_and_repeat(
    path: &Path,
    configs: Vec<BenchmarkConfig>,
    repeat: Option<u64>,
    sampled_trials: Vec<crate::sweep::SampledTrials>,
    output: &mut dyn Write,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(), Error> {
    let native_snapshot_record = configs.iter().any(|config| config.native_snapshot_record);
    let prepared = prepared::Sweep::new_with_samples(path, configs, repeat, sampled_trials)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(Error::Input(format!(
            "sweep {} already exists; choose a new --output filename",
            path.display()
        )));
    }
    create_parent(path, output)?;
    publication::sweep_with_progress(path, &prepared, progress)?;
    writeln!(
        output,
        "Wrote {} benchmarks and sweep {}\nRun it with: asb sweep '{}'{}",
        prepared.configurations.len(),
        path.display(),
        path.display(),
        if native_snapshot_record {
            " --record"
        } else {
            ""
        }
    )
    .map_err(|error| publication::confirmation(path, error))
}

#[cfg(test)]
mod tests {
    use super::{collector_installables, repeated, unique_efforts};

    #[test]
    fn sweep_publication_preserves_snapshot_recording_in_every_config() {
        let root = std::env::temp_dir().join(format!(
            "asb-snapshot-sweep-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='model'\nworkload='swe-bench'\nnative_snapshot_record=true\n",
        ).unwrap();
        let mut second = config.clone();
        second.trial = Some("another-trial".into());
        let path = root.join("sweep.toml");
        let mut output = Vec::new();
        super::publish_sweep(&path, vec![config, second], &mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("--record"));
        let sweep: crate::sweep::SweepFile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(sweep.configs.len(), 2);
        for reference in sweep.configs {
            let saved: crate::benchmark::BenchmarkConfig =
                toml::from_str(&std::fs::read_to_string(root.join(reference)).unwrap()).unwrap();
            assert!(saved.native_snapshot_record);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_collector_tools_are_installed_alongside_dependencies() {
        let selected = vec![
            "perfetto".to_owned(),
            "flamegraph".to_owned(),
            "bpftrace:sched-latency".to_owned(),
        ];
        let tools = collector_installables(&selected);
        assert!(tools.contains(&"perfetto".to_owned()));
        assert!(tools.contains(&"flamegraph".to_owned()));
        assert!(tools.contains(&"bpftrace".to_owned()));
        assert!(tools.contains(&"perf".to_owned()));
    }

    #[test]
    fn default_collectors_install_their_project_local_dependencies() {
        let tools = collector_installables(&[]);
        for tool in ["flamegraph", "iostat", "mpstat", "perf", "perfetto"] {
            assert!(tools.contains(&tool.to_owned()), "missing {tool}");
        }
        assert!(
            !tools.contains(&"bpftrace".to_owned()),
            "bpftrace is only installed when explicitly selected"
        );
    }

    #[test]
    fn thinking_effort_matrix_selection_deduplicates_and_requires_a_sweep() {
        let mut options = crate::config::SetupOptions {
            agent: vec!["pi".into()],
            workload: vec!["bug-fix".into()],
            trial: Vec::new(),
            trial_count: Vec::new(),
            trial_set: Vec::new(),
            provider: vec!["openai".into()],
            model: vec!["gpt-6-sol".into()],
            thinking_effort: vec![
                Some(crate::benchmark::ThinkingEffort::High),
                Some(crate::benchmark::ThinkingEffort::High),
                Some(crate::benchmark::ThinkingEffort::XHigh),
            ],
            native_snapshot_record: false,
            measure: Vec::new(),
            concurrency: None,
            workspace: None,
            output: None,
            retention: Default::default(),
            repeat: None,
        };
        assert_eq!(unique_efforts(&options.thinking_effort).len(), 2);
        assert!(repeated(&options));
        options.thinking_effort.clear();
        assert_eq!(unique_efforts(&options.thinking_effort), vec![None]);
        assert!(!repeated(&options));
    }
}
