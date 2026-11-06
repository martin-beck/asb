//! Single-attempt preparation and publication. Each transition consumes its
//! phase; live provider custody surrounds the whole workload pipeline.
use super::*;

struct Context<'a> {
    config_path: PathBuf,
    publication_config: PathBuf,
    config: BenchmarkConfig,
    options: RunOptions<'a>,
    project_root: PathBuf,
    project_config: project::ProjectConfig,
    policy: crate::planning::ResolvedPlan,
    measurement_path: Option<PathBuf>,
    tape_cassette: Option<(tape::Mode, PathBuf)>,
    tool_tape_cassette: Option<(tape::Mode, PathBuf)>,
    mcp_tape_cassette: Option<(tape::Mode, PathBuf)>,
    normalization_artifact: Option<Vec<u8>>,
    network: Option<project::NetworkProjectGuard>,
}

/// All fallible cwd anchoring precedes resource allocation. Logical cassette
/// and configuration spelling remains separate from physical execution paths.
pub(super) struct ConfiguredAttempt<'a> {
    context: Context<'a>,
    measurements: Option<measurements::Session>,
    selected_offer: Option<crate::endpoint::Offer>,
    discovered_tool_support: Option<bool>,
}

impl<'a> ConfiguredAttempt<'a> {
    pub fn configure(
        config_path: &Path,
        admitted: AdmittedAttempt<'a>,
        policy: crate::planning::ResolvedPlan,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<Self, Error> {
        let AdmittedAttempt {
            config,
            options,
            project_root,
            project_config,
        } = admitted;
        if options.recording.is_some() && options.mode != Some(tape::Mode::Record) {
            return Err(Error::Run(
                "recording attempt lost its selected record mode before adapter preparation".into(),
            ));
        }
        let publication_config = std::path::absolute(config_path).map_err(io_error)?;
        let runtime_anchor = project::configuration_anchor(config_path, options.invocation_dir)?;
        let measurement_path = options
            .measurement_dir
            .map(PathBuf::from)
            .or_else(|| {
                config
                    .measurements
                    .as_ref()
                    .filter(|value| value.enabled)
                    .map(|value| project::resolve(&project_root, &value.directory))
            })
            .map(std::path::absolute)
            .transpose()
            .map_err(io_error)?;
        let tape_cassette = options.mode.map(|mode| {
            (
                mode,
                options
                    .cassette
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_cassette(&runtime_anchor, &config)),
            )
        });
        let tool_mode = options.effective_tool_mode();
        let tool_tape_cassette = tool_mode.map(|mode| {
            (
                mode,
                options
                    .cassette
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_cassette(&runtime_anchor, &config)),
            )
        });
        let mcp_tape_cassette = options.mcp_mode.map(|mode| {
            (
                mode,
                options
                    .cassette
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_cassette(&runtime_anchor, &config)),
            )
        });
        let network = project::network_project_at(
            &project_root,
            options.invocation_dir.unwrap_or_else(|| Path::new(".")),
        );
        let normalization_artifact = if options.mode == Some(tape::Mode::Record) {
            options
                .custom_normalization
                .map(|custom| custom.policy.cassette_artifact().map_err(Error::Input))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let context = Context {
            config_path: config_path.into(),
            publication_config,
            config,
            options,
            project_root,
            project_config,
            policy,
            measurement_path,
            tape_cassette,
            tool_tape_cassette,
            mcp_tape_cassette,
            normalization_artifact,
            network: Some(network),
        };
        let config = &context.config;
        let options = context.options;
        let project_root = &context.project_root;
        let project_config = &context.project_config;
        let policy = &context.policy;
        let tape_cassette = &context.tape_cassette;
        let mcp_tape_mode = context.mcp_tape_cassette.as_ref().map(|(mode, _)| *mode);
        let json = options.json;
        let _initial_policy = crate::planning::persist(project_root, policy, None, None)?;
        let measurement_path = &context.measurement_path;
        let measurement_collectors: Vec<_> = policy
            .collectors
            .iter()
            .filter(|collector| collector.selected)
            .map(|collector| collector.id.clone())
            .collect();
        let active_collectors: Vec<_> = policy
            .collectors
            .iter()
            .map(|collector| collector.id.clone())
            .collect();
        if measurement_path.is_some() {
            let selected = if measurement_collectors.is_empty() {
                "none".into()
            } else {
                measurement_collectors.join(", ")
            };
            progress::status(
                options.interactive,
                &format!("starting collectors: {selected}"),
            );
        }
        let mut measurement_session = measurement_path
            .as_deref()
            .map(|dir| {
                measurements::Session::start_with_resolved_collectors_and_parent(
                    project_root,
                    dir,
                    &measurement_collectors,
                    &active_collectors,
                    Some(&project::resolve(project_root, &project_config.tools_dir)),
                    std::env::var_os("ASB_CGROUP_PARENT")
                        .is_none()
                        .then_some(project_config.cgroup_parent.as_deref())
                        .flatten()
                        .map(Path::new),
                )
            })
            .transpose()
            .map_err(Error::Run)?;
        #[cfg(all(test, unix))]
        phases::entry_test_support::configure_measurements(
            project_root,
            measurement_session.as_mut(),
        );
        if let Some(session) = measurement_session.as_mut() {
            session
                .event(LifecycleEvent::RunStarted)
                .map_err(Error::Run)?;
        }
        if measurement_path.is_some() {
            progress::status(
                options.interactive,
                "collectors ready; preparing provider and workload",
            );
        }
        let replay_recorder = if matches!(tape_cassette.as_ref(), Some((tape::Mode::Replay, _))) {
            let name = options
                .native_tool_inputs
                .and_then(|inputs| inputs.get(options.replica.index.checked_sub(1)?))
                .and_then(|input| input.recorder.as_deref())
                .unwrap_or(crate::proxy::legacy_recorder_name());
            Some(crate::proxy::recorder(name).ok_or_else(|| {
                Error::Input(format!("recording proxy `{name}` is not registered"))
            })?)
        } else {
            None
        };
        if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
            replay_recorder
                .expect("replay recorder selected")
                .validate_replay_model(cassette, &config.model)?;
        }
        validate_explicit_tool_transport(config, options)?;
        // This is deliberately retained at admission as a defence-in-depth
        // guard.  Direct runs and complete sweep populations perform the same
        // static validation before capacity, dependency, or collector work.
        validate_mcp_configuration(config, mcp_tape_mode)?;
        let mut discovered_tool_support = None;
        let selected_offer = instrument_phase(
            measurement_session.as_mut(),
            LifecyclePhase::ProviderDiscovery,
            || {
                agents::find(&config.agent)
    .map(|adapter| {
        let offered = if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
            providers::replay_endpoints(&config.provider, &config.model, replay_recorder.expect("replay recorder selected").replay_interface(cassette)?)
        } else {
            let (offers, snapshot_fallback, tool_support) = providers::discover_endpoints(&config.provider, &config.model);
            discovered_tool_support = tool_support;
            if snapshot_fallback && !json {
                emit_warning(warnings, output, format!("Warning: current endpoint metadata is unavailable for provider `{}` and model `{}`; using the built-in endpoint snapshot, which may be out of date.", config.provider, config.model))?;
            }
            offers
        };
        crate::endpoint::choose_from(
            adapter.id(),
            adapter.supported_endpoints(),
            &config.provider,
            &config.model,
            &offered,
        )
    })
    .transpose()
            },
        )?;

        if context.options.custom_normalization.is_some() {
            require_normalizer_decoder(selected_offer.as_ref().map(|offer| offer.interface))?;
        }

        Ok(Self {
            context,
            measurements: measurement_session,
            selected_offer,
            discovered_tool_support,
        })
    }

    pub fn run(
        self,
        arrival: &mut Option<batch::Arrival<'a>>,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        ProviderAttempt::start(self, arrival, output, warnings)?.run(arrival, output, warnings)
    }
}

fn require_normalizer_decoder(interface: Option<crate::endpoint::Interface>) -> Result<(), Error> {
    match interface {
        Some(crate::endpoint::Interface::OpenAiResponses
        | crate::endpoint::Interface::OpenAiChat
        | crate::endpoint::Interface::AnthropicMessages) => Ok(()),
        _ => Err(Error::Input(
            "the selected API interface has no replay normalizer tool-result decoder; choose a supported interface or remove the matching normalizer profile".into(),
        )),
    }
}

/// Reject declarations that an adapter would otherwise ignore.  Ordinary MCP
/// configuration is adapter-owned, so this check deliberately stays separate
/// from ASB's semantic local-tool replay capability.
/// Validate static MCP configuration before any execution-side effects.
///
/// This is shared by direct runs and sweep population preflight.  The
/// configured-attempt call remains a defence-in-depth guard for internal
/// callers which bypass those public entry points.
pub(super) fn validate_mcp_configuration(
    config: &BenchmarkConfig,
    mcp_tape_mode: Option<tape::Mode>,
) -> Result<(), Error> {
    validate_mcp_server_capability(config)?;
    validate_mcp_relay_capability(config, mcp_tape_mode)
}

fn validate_mcp_server_capability(config: &BenchmarkConfig) -> Result<(), Error> {
    if !config.mcp_servers.is_empty()
        && let Some(adapter) = agents::find(&config.agent)
        && !adapter.supports_mcp_servers()
    {
        return Err(Error::Input(format!(
            "agent `{}` does not support declared MCP servers yet; choose an adapter with ordinary MCP configuration support",
            config.agent
        )));
    }
    Ok(())
}

fn validate_mcp_relay_capability(
    config: &BenchmarkConfig,
    mcp_tape_mode: Option<tape::Mode>,
) -> Result<(), Error> {
    if mcp_tape_mode.is_none() {
        return Ok(());
    }
    if config.mcp_servers.is_empty() {
        return Err(Error::Input(
            "MCP record/replay requires at least one [[mcp_servers]] declaration".into(),
        ));
    }
    let adapter = agents::find(&config.agent)
        .ok_or_else(|| Error::Input(format!("unknown agent `{}`", config.agent)))?;
    if !adapter.supports_mcp_relay() {
        return Err(Error::Input(format!(
            "agent `{}` does not yet provide a transparent MCP relay route; choose an adapter with tested MCP recording support",
            config.agent
        )));
    }
    Ok(())
}

/// Provider custody is not threaded through a list of unrelated arguments.
/// Its outer finalizer also covers every failed preparation/runtime transition.
struct ProviderAttempt<'a> {
    configured: ConfiguredAttempt<'a>,
    bridge: Option<Box<dyn providers::ProviderSession>>,
    _network: Option<project::NetworkProjectGuard>,
}
impl<'a> ProviderAttempt<'a> {
    fn start(
        mut configured: ConfiguredAttempt<'a>,
        arrival: &mut Option<batch::Arrival<'a>>,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<Self, Error> {
        let network = configured.context.network.take();
        let context = &configured.context;
        let config = &context.config;
        let options = context.options;
        let project_root = &context.project_root;
        let project_config = &context.project_config;
        let tape_mode = options.mode;
        let json = options.json;
        let discovered_tool_support = configured.discovered_tool_support;
        let mut measurement_session = configured.measurements.take();
        let mut bridge = None;
        let setup = (|| {
            instrument_phase(
                measurement_session.as_mut(),
                LifecyclePhase::ProviderSetup,
                || {
                    bridge = providers::start_local_bridge(
                        &config.provider,
                        project_root,
                        &project_config.tools_dir,
                        tape_mode == Some(tape::Mode::Replay),
                    )?;
                    #[cfg(all(test, unix))]
                    {
                        bridge = phases::entry_test_support::observe_provider(
                            project_root,
                            bridge.take(),
                        );
                    }
                    if tape_mode != Some(tape::Mode::Replay) {
                        let endpoint =
                            providers::local_bridge_endpoint(&config.provider, bridge.as_deref());
                        let bridge_catalog = providers::local_bridge_credentials(
                            &config.provider,
                            bridge.as_deref(),
                            endpoint,
                        )
                        .map(|(key, url)| (url, key));
                        if discovered_tool_support == Some(false) {
                            return Err(Error::Input(format!(
                                "provider metadata reports that model `{}` does not support tools",
                                config.model
                            )));
                        }
                        if !models::ensure_tool_capable(
                            &config.provider,
                            &config.model,
                            bridge_catalog,
                        )? && discovered_tool_support != Some(true)
                            && !json
                        {
                            emit_warning(
                                warnings,
                                output,
                                format!(
                                    "Warning: provider `{}` lists model `{}` but does not report whether it supports tools; the agent may fail if the model cannot use tools.",
                                    config.provider, config.model
                                ),
                            )?;
                        }
                    }
                    Ok(())
                },
            )?;

            Ok(())
        })();
        configured.measurements = measurement_session;
        if let Err(error) = setup {
            return match finish_provider_attempt(Err(error), arrival, &mut bridge) {
                Err(error) => {
                    drop(configured);
                    drop(network);
                    Err(error)
                }
                Ok(()) => Ok(Self {
                    configured,
                    bridge,
                    _network: network,
                }),
            };
        }
        Ok(Self {
            configured,
            bridge,
            _network: network,
        })
    }
    fn run(
        self,
        arrival: &mut Option<batch::Arrival<'a>>,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        let Self {
            configured,
            mut bridge,
            _network,
        } = self;
        let result = (|| {
            let ready =
                ReadyAttempt::prepare(configured, bridge.as_deref(), arrival, output, warnings)?;
            let executed = ready.execute(bridge.as_deref(), arrival, output)?;
            executed
                .finalize(&mut bridge, output, warnings)?
                .publish(output, warnings)
        })();
        finish_provider_attempt(result, arrival, &mut bridge)
    }
}

struct WorkloadMetadata {
    workspace: PathBuf,
    workload_baseline: workloads::BaselineEvidence,
    workload_baseline_passed: Option<bool>,
    effective_budgets: crate::planning::EffectiveBudgets,
    execution_plan: PathBuf,
    execution_plan_fingerprint: crate::planning::ExecutionPlanFingerprint,
    module: Option<workloads::contracts::ModuleDescriptor>,
    metric_policy: workloads::outcome::MetricPolicy,
}
struct ReadyAttempt<'a> {
    context: Context<'a>,
    metadata: WorkloadMetadata,
    prepared: PreparedAttempt,
    selected_offer: Option<crate::endpoint::Offer>,
    launch: Option<batch::LaunchAdmission<'a>>,
}

/// An unsuccessful preparation transition must notify peers before cancellation,
/// and report cancellation failure without replacing the original error.
pub(super) fn cancel_prepared(
    error: Error,
    arrival: &mut Option<batch::Arrival<'_>>,
    workload: &mut dyn workloads::WorkloadSession,
) -> Error {
    if let Some(arrival) = arrival.as_mut() {
        return abort_prepared(error, arrival, workload);
    }
    match workload.cancel_prepared() {
        Ok(()) => error,
        Err(cleanup) => preparation_cancellation_error(error, cleanup),
    }
}

impl<'a> ReadyAttempt<'a> {
    fn prepare(
        configured: ConfiguredAttempt<'a>,
        bridge: Option<&dyn providers::ProviderSession>,
        arrival: &mut Option<batch::Arrival<'a>>,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<Self, Error> {
        let ConfiguredAttempt {
            context,
            measurements: mut measurement_session,
            selected_offer,
            discovered_tool_support: _,
        } = configured;
        let config = &context.config;
        let options = context.options;
        let project_root = &context.project_root;
        let project_config = &context.project_config;
        let policy = &context.policy;
        let tape_cassette = &context.tape_cassette;
        let respect_record_timings = options.respect_record_timings;
        let json = options.json;
        let workspace_name = if config.workspace == "asb-workspace" {
            &project_config.workspace_dir
        } else {
            &config.workspace
        };
        let workspace = project::resolve(project_root, workspace_name);
        let workload_directory = project::environment_path(
            "ASB_WORKLOAD_DIR",
            options.invocation_dir.unwrap_or_else(|| Path::new(".")),
        )
        .unwrap_or_else(|| project::resolve(project_root, &project_config.workloads_dir));
        let allocation = |reference: &workloads::recovery::AttemptReference| {
            options.on_completion.map_or(Ok(()), |observer| {
                observer
                    .allocated(options.replica, reference)
                    .map_err(|error| error.to_string())
            })
        };

        let mut workload_session = None;
        let preparation = instrument_phase(
            measurement_session.as_mut(),
            LifecyclePhase::WorkloadPreparation,
            || {
                workloads::preflight(
                    &config.workload,
                    &workloads::PreflightContext {
                        project_root,
                        project_config,
                        agent: &config.agent,
                        offline: options.mode == Some(tape::Mode::Replay),
                    },
                )?;
                std::fs::create_dir_all(&workspace).map_err(|error| {
                    Error::Run(format!(
                        "cannot create workspace {}: {error}",
                        workspace.display()
                    ))
                })?;
                let workload = workloads::prepare_task(&workloads::PreparationContext {
                    tool_mode: options.effective_tool_mode(),
                    native_snapshot_replay: options.native_snapshot_replay(),
                    native_snapshot_record: config.native_snapshot_record
                        && options.effective_tool_mode() == Some(tape::Mode::Record),
                    verify_initial_material: options.preparation_gate.is_some()
                        || options.verify_initial_material,
                    budgets: policy.budgets,
                    on_allocation: Some(&allocation),
                    workload: &config.workload,
                    trial: config.trial.as_deref(),
                    trial_set: config.trial_set.as_deref(),
                    workload_directory: &workload_directory,
                    workspace: &workspace,
                    project_root,
                    project_config,
                })
                .map_err(Error::Run)?;
                #[cfg(all(test, unix))]
                let workload = phases::entry_test_support::observe_workload(project_root, workload);
                workload_session = Some(workload);
                Ok(())
            },
        );

        if let Err(error) = preparation {
            return Err(match workload_session.as_mut() {
                Some(workload) => cancel_prepared(error, arrival, workload.as_mut()),
                None => error,
            });
        }
        let mut workload_session = workload_session
            .ok_or_else(|| Error::Run("workload preparation returned no owned session".into()))?;
        let prepared = (|| {
            // Capture provenance at execution time; report rendering never consults
            // today's adapter registry to label a historical record.
            workload_session
                .invocation()
                .input
                .validate_for_agent(&config.agent)
                .map_err(Error::Input)?;
            let mut effective_budgets = workload_session.effective_budgets();
            if policy.backend == "native" {
                effective_budgets.execution_seconds = Some(
                    policy
                        .budgets
                        .execution_seconds
                        .unwrap_or(MAX_AGENT_RUNTIME.as_secs()) as f64,
                );
            }
            let dispatch = options
                .execution_cohort
                .map(|cohort| {
                    if !cohort.admits(options.replica.index, options.replica.total) {
                        return Err(Error::Input(
                            "dispatch differs from actual cohort membership".into(),
                        ));
                    }
                    // Preserve historical unknown dispatch. Do not synthesize
                    // facts absent from the observer and completion summary.
                    Ok(cohort.dispatch)
                })
                .transpose()?
                .flatten();
            let execution_plan = crate::planning::persist_with_dispatch(
                project_root,
                policy,
                Some(effective_budgets),
                Some(&workload_session.invocation().identity),
                dispatch,
            )?;
            let execution_plan_fingerprint = crate::planning::protocol_fingerprint_with_dispatch(
                config,
                policy,
                effective_budgets,
                options.replica.total,
                options.batch.map(repeated::BatchIdentity::admission),
                dispatch,
            )?;
            let module = workload_session.module_descriptor();
            let metric_policy = workload_session.metric_policy();
            if let Some(custom) = options.custom_normalization {
                if options.mode.is_none() || options.preparation_gate.is_none() {
                    return Err(Error::Input(
                        "a frozen normalizer policy requires an admitted record or replay campaign"
                            .into(),
                    ));
                }
                custom.validate_attempt(
                    &config.agent,
                    &workload_session.invocation().identity,
                    module.as_ref().map(|module| module.family.as_str()),
                )?;
            }
            if !metric_policy.valid()
                || (module.is_none()
                    && metric_policy != workloads::outcome::MetricPolicy::default())
                || module
                    .as_ref()
                    .is_some_and(|m| !m.valid() || !metric_policy.matches_module(m))
            {
                return Err(Error::Run(
                    "module metric policy differs from its declared scoring protocol".into(),
                ));
            }
            let workspace = workload_session.invocation().workspace.clone();
            let (capsule_record, capsule_replay, guest_capsule_record, guest_capsule_replay) =
                if workload_session.uses_isolated_guest() {
                    // The actual workspace is owned by the remote guest. Keep
                    // selected host paths as opaque custody only; its adapter
                    // captures and applies the payload in the guest.
                    (
                        None,
                        None,
                        options.native_capsule_record.map(PathBuf::from),
                        options.native_capsule_replay.map(PathBuf::from),
                    )
                } else {
                    (
                        options
                            .native_capsule_record
                            .map(|destination| {
                                agents::capsule::Capture::start(Path::new(destination), &workspace)
                            })
                            .transpose()?,
                        // Restore only after the workload has prepared its normal
                        // workspace, retaining that session's ownership and mounts.
                        options
                            .native_capsule_replay
                            .map(|source| {
                                agents::capsule::Replay::open(Path::new(source), &workspace)
                            })
                            .transpose()?,
                        None,
                        None,
                    )
                };
            if let Some(arrival) = arrival.as_mut() {
                let identity = (|| {
                    let task = &workload_session.invocation().identity;
                    let material_sha256 = workload_session
                        .initial_material_sha256()
                        .map_err(Error::Run)?;
                    let dataset_sha256 = batch::dataset_fingerprint(task.dataset.as_ref())?;
                    Ok(crate::cassette::Compatibility {
                        agent: config.agent.clone(),
                        provider: config.provider.clone(),
                        model: config.model.clone(),
                        workload: task.workload.clone(),
                        task: task.task_id.clone(),
                        dataset_sha256,
                        prompt_sha256: crate::cassette::digest(
                            workload_session.invocation().input.instructions.as_bytes(),
                        ),
                        material_sha256,
                        protocol_sha256: Some(batch::replay_protocol_with_normalizers(
                            &batch::replay_protocol_fingerprint(module.as_ref(), &metric_policy)?,
                            options.custom_normalization.map(|custom| &custom.policy),
                        )?),
                        normalization_sha256: options
                            .custom_normalization
                            .map(|custom| custom.policy.sha256.clone()),
                    })
                })();
                let identity = identity?;
                match workload_session
                    .legacy_initial_material_sha256()
                    .map_err(Error::Run)?
                {
                    Some(legacy) => arrival.ready_with_legacy(identity, Some(legacy))?,
                    None => arrival.ready(identity)?,
                }
            }

            let launch = options
                .launch_gate
                .map(|gate| {
                    let identity = options
                        .preparation_gate
                        .ok_or_else(|| {
                            Error::Run("launch admission requires verified material".into())
                        })?
                        .confirmed_identity()?;
                    batch::LaunchAdmission::new(gate, options.replica, identity)
                })
                .transpose()?;
            let workload_baseline = workload_session.baseline_evidence();
            let workload_baseline_passed = workload_baseline.measured_passed();
            if !json && workload_baseline_passed == Some(true) {
                emit_warning(warnings, output, "The workload already passes before the agent starts. Files are preserved, but this run cannot earn task-resolution credit. Use a fresh workspace for a new benchmark attempt.".into())?;
            }
            if !json
                && !workload_baseline.supports_evaluation(workloads::SavedEvaluationKind::Known(
                    workload_session.evaluation_kind(),
                ))
                && workload_baseline_passed != Some(true)
                && workload_session.evaluation_kind().resolution_eligible()
            {
                emit_warning(warnings, output, "The initial workload baseline could not be verified. This run cannot earn task-resolution credit. Inspect the workload fixture and use a fresh workspace.".into())?;
            }
            if !json
                && workload_session.evaluation_kind()
                    == workloads::KnownEvaluationKind::AdapterSmoke
            {
                emit_warning(warnings, output, "Selected external workload runs only an adapter smoke exercise, not an upstream benchmark task. Benchmark task success will be unavailable; an official dataset and evaluator are not integrated yet.".into())?;
            }
            let perfetto_observer = tape_cassette
                .is_some()
                .then(|| {
                    measurement_session
                        .as_mut()
                        .map(|session| session.perfetto_observer_context("perfetto-requests.json"))
                        .transpose()
                })
                .transpose()
                .map_err(Error::Run)?
                .flatten()
                .flatten();
            let tape_session = instrument_phase(
                measurement_session.as_mut(),
                LifecyclePhase::ProxyPreparation,
                || {
                    if let Some((mode, cassette)) = tape_cassette.as_ref() {
                        let bridge_url = providers::local_bridge_endpoint(&config.provider, bridge);
                        let upstream = if *mode == tape::Mode::Replay {
                            None
                        } else if let Some(offer) = selected_offer.as_ref() {
                            Some(providers::selected_tape_upstream(
                                &config.provider,
                                offer.clone(),
                                bridge_url,
                            )?)
                        } else {
                            providers::tape_upstream(&config.provider, bridge_url)
                        };
                        Ok(Some(tape::Session::start_for_agent_at(
                            *mode,
                            cassette,
                            upstream.as_deref(),
                            respect_record_timings,
                            tape::AgentContext {
                                workspace: &workspace,
                                agent: Some(&config.agent),
                                workload: Some(&config.workload),
                                task: Some(&workload_session.invocation().identity.task_id),
                                custom_normalization: options.custom_normalization.cloned(),
                                recorder: if *mode == tape::Mode::Replay {
                                    Some(
                                        options
                                            .native_tool_inputs
                                            .and_then(|inputs| {
                                                inputs.get(options.replica.index.checked_sub(1)?)
                                            })
                                            .and_then(|input| input.recorder.as_deref())
                                            .unwrap_or(crate::proxy::legacy_recorder_name()),
                                    )
                                } else {
                                    None
                                },
                                invocation_dir: options
                                    .invocation_dir
                                    .unwrap_or_else(|| Path::new(".")),
                                perfetto_observer: perfetto_observer
                                    .as_ref()
                                    .map(|(file, _, _)| file.try_clone())
                                    .transpose()
                                    .map_err(|error| Error::Run(error.to_string()))?,
                                perfetto_trace_id: perfetto_observer
                                    .as_ref()
                                    .map(|(_, trace_id, _)| trace_id.clone()),
                                perfetto_report: perfetto_observer
                                    .as_ref()
                                    .map(|(_, _, report)| report.clone()),
                            },
                        )?))
                    } else {
                        Ok(None)
                    }
                },
            )?;

            Ok((
                WorkloadMetadata {
                    workspace,
                    workload_baseline,
                    workload_baseline_passed,
                    effective_budgets,
                    execution_plan,
                    execution_plan_fingerprint,
                    module,
                    metric_policy,
                },
                tape_session,
                launch,
                capsule_record,
                capsule_replay,
                guest_capsule_record,
                guest_capsule_replay,
            ))
        })();
        let (
            metadata,
            tape,
            launch,
            capsule_record,
            capsule_replay,
            guest_capsule_record,
            guest_capsule_replay,
        ) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err(cancel_prepared(error, arrival, workload_session.as_mut())),
        };
        Ok(Self {
            context,
            metadata,
            prepared: PreparedAttempt {
                workload: workload_session,
                measurements: measurement_session,
                tape,
                capsule_record,
                capsule_replay,
                guest_capsule_record,
                guest_capsule_replay,
            },
            selected_offer,
            launch,
        })
    }

    fn execute(
        self,
        bridge: Option<&dyn providers::ProviderSession>,
        arrival: &mut Option<batch::Arrival<'a>>,
        output: &mut dyn Write,
    ) -> Result<CompletedAttempt<'a>, Error> {
        let Self {
            context,
            metadata,
            mut prepared,
            selected_offer,
            launch,
        } = self;
        let config_path = context.config_path.as_path();
        let config = &context.config;
        let options = context.options;
        let effective_max_steps = options.effective_max_steps();
        let project_root = &context.project_root;
        let project_config = &context.project_config;
        let tape_cassette = &context.tape_cassette;
        let tool_tape_cassette = &context.tool_tape_cassette;
        let mcp_tape_cassette = &context.mcp_tape_cassette;
        let tape_mode = options.mode;
        let show_output = options.show_output;
        let launch = launch
            .as_ref()
            .map(|launch| launch as &dyn workloads::session::LaunchReadiness);
        let mut runtime_entered = false;
        let execution = (|| {
            let _launch_failure = workloads::session::LaunchFailureGuard(launch);
            let tape_url = prepared.tape.as_ref().map(|session| session.url());
            let provider_endpoint =
                tape_url.or_else(|| providers::local_bridge_endpoint(&config.provider, bridge));
            let provider_credentials =
                providers::local_bridge_credentials(&config.provider, bridge, provider_endpoint);
            let connection = selected_offer
                .map(|offer| {
                    providers::endpoint_connection(
                        &config.provider,
                        offer,
                        provider_credentials,
                        tape_url.zip(tape_mode).map(|(url, mode)| (mode, url)),
                    )
                })
                .transpose()?;
            let started = Instant::now();
            // Once adapter dispatch is entered, only its existing running-owner
            // cancellation/Drop contract may settle an execution error. The
            // prepared-only cancellation API cannot authorize started teardown.
            runtime_entered = true;
            let mut isolated_progress = progress::Context::new(
                options.interactive,
                &config.agent,
                &config.provider,
                options.replay_statistics(),
            )
            .display();
            #[cfg(not(test))]
            let native_tool = options
                .native_tool_inputs
                .and_then(|inputs| inputs.get(options.replica.index.checked_sub(1)?))
                .and_then(|input| input.native_tool_launch().zip(input.native_tool.as_deref()));
            #[cfg(test)]
            let native_tool: Option<(&crate::cassette::NativeToolLaunch, &Path)> = None;
            let isolated = prepared.workload.execute(
                &mut workloads::ExecutionContext {
                    launch,
                    config,
                    connection: connection.as_ref(),
                    project_root,
                    project_config,
                    replay: tape_mode == Some(tape::Mode::Replay),
                    max_steps: effective_max_steps,
                    tape_cassette: tape_cassette
                        .as_ref()
                        .map(|(_, cassette)| cassette.as_path()),
                    tool_replay: tool_tape_cassette
                        .as_ref()
                        .is_some_and(|(mode, _)| *mode == tape::Mode::Replay),
                    tool_tape_cassette: tool_tape_cassette
                        .as_ref()
                        .map(|(_, cassette)| cassette.as_path()),
                    mcp_mode: mcp_tape_cassette.as_ref().map(|(mode, _)| *mode),
                    mcp_tape_cassette: mcp_tape_cassette
                        .as_ref()
                        .map(|(_, cassette)| cassette.as_path()),
                    #[cfg(not(test))]
                    native_tool,
                    #[cfg(not(test))]
                    native_capsule: prepared
                        .guest_capsule_record
                        .as_deref()
                        .map(workloads::session::NativeCapsule::Record)
                        .or_else(|| {
                            prepared
                                .guest_capsule_replay
                                .as_deref()
                                .map(workloads::session::NativeCapsule::Replay)
                        }),
                    measurements: prepared.measurements.as_mut(),
                    show_output,
                    progress: isolated_progress
                        .as_mut()
                        .map(|display| display as &mut dyn workloads::session::ExecutionProgress),
                },
                output,
            );
            if let Some(progress) = isolated_progress.as_mut() {
                crate::workloads::session::ExecutionProgress::finish(progress);
            }
            let mut issues = AttemptIssues::default();
            let (
                child,
                run_dir,
                guest,
                execution_elapsed_ms,
                runtime_output,
                artifact_owner,
                observation,
            ) = if let Some(result) = isolated {
                let result = result.map_err(Error::Run)?;
                issues.collector_errors = result.collector_errors;
                issues.output_error = result.output_error;
                let observation = result.metadata.observation;
                (
                    result.output,
                    PathBuf::new(),
                    Some(result.metadata),
                    result
                        .elapsed_ms
                        .unwrap_or_else(|| started.elapsed().as_millis()),
                    None,
                    None,
                    observation,
                )
            } else {
                let native = native::NativePlan {
                    config_path,
                    config,
                    project_root,
                    invocation_dir: options.invocation_dir.unwrap_or_else(|| Path::new(".")),
                    project_config,
                    invocation: prepared.workload.invocation(),
                    fresh_state: options.batch.is_some(),
                    route: native::NativeRoute {
                        connection: connection.as_ref(),
                        provider_endpoint,
                        provider_credentials,
                        tape_url,
                        tape_mode,
                        max_steps: effective_max_steps,
                        tool_tape_mode: tool_tape_cassette.as_ref().map(|(mode, _)| *mode),
                        tool_tape_cassette: tool_tape_cassette
                            .as_ref()
                            .map(|(_, cassette)| cassette.as_path()),
                        mcp_tape_mode: mcp_tape_cassette.as_ref().map(|(mode, _)| *mode),
                        mcp_tape_cassette: mcp_tape_cassette
                            .as_ref()
                            .map(|(_, cassette)| cassette.as_path()),
                        native_tool: native_tool
                            .map(|(launch, path)| agents::NativeToolContext { launch, path }),
                    },
                }
                .prepare(prepared.measurements.as_mut())?;
                if prepared.capsule_record.is_some() || prepared.capsule_replay.is_some() {
                    let root_executable = native.root_executable()?;
                    if let Some(capture) = prepared.capsule_record.as_mut() {
                        capture.bind_root_executable(root_executable.clone())?;
                    }
                    if let Some(replay) = prepared.capsule_replay.as_ref() {
                        replay.verify_root_executable(&root_executable)?;
                    }
                }
                let progress = progress::Context::new(
                    options.interactive,
                    &config.agent,
                    &config.provider,
                    options.replay_statistics(),
                );
                let (evidence, native_issues) = native.execute(
                    prepared.measurements.as_mut(),
                    show_output,
                    output,
                    launch,
                    progress,
                )?;
                issues = native_issues;
                (
                    evidence.output,
                    evidence.run_dir,
                    evidence.guest,
                    evidence.elapsed_ms,
                    evidence.runtime_output,
                    evidence.artifact_owner,
                    evidence.observation,
                )
            };

            let capture_complete = ExecutionEvidence::capture_complete(&issues, guest.as_ref());
            Ok((
                ExecutionEvidence {
                    observation,
                    output: child,
                    run_dir,
                    guest,
                    elapsed_ms: execution_elapsed_ms,
                    runtime_output,
                    artifact_owner,
                    capture_complete,
                },
                issues,
                connection.as_ref().map(|connection| connection.interface),
            ))
        })();
        let (evidence, issues, interface) = match execution {
            Ok(execution) => execution,
            Err(error) if !runtime_entered => {
                return Err(cancel_prepared(error, arrival, prepared.workload.as_mut()));
            }
            Err(error) => {
                if let Some(arrival) = arrival.as_mut() {
                    arrival.abort(&error.to_string());
                }
                return Err(error);
            }
        };
        let settled = prepared
            .executed(evidence, issues)
            .settle(tape_cassette.as_ref());
        Ok(CompletedAttempt {
            context,
            metadata,
            settled,
            interface,
        })
    }
}

struct CompletedAttempt<'a> {
    context: Context<'a>,
    metadata: WorkloadMetadata,
    settled: phases::SettledAttempt,
    interface: Option<crate::endpoint::Interface>,
}
struct FinalResult<'a> {
    context: Context<'a>,
    facts: ResultFacts,
}
struct ResultFacts {
    metadata: WorkloadMetadata,
    finalized: FinalizedAttempt,
    adapter_artifacts: agents::AgentArtifacts,
    process_success: bool,
    agent_failure: Option<String>,
    provider_cleanup_error: Option<String>,
    artifact_evidence: Vec<String>,
}

fn sealed_step_count(guest: Option<u64>, adapter: Option<u64>) -> Option<u64> {
    guest.or(adapter)
}

impl<'a> CompletedAttempt<'a> {
    fn finalize(
        self,
        bridge: &mut Option<Box<dyn providers::ProviderSession>>,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<FinalResult<'a>, Error> {
        let Self {
            context,
            metadata,
            mut settled,
            interface,
        } = self;
        // `settle` has ended agent execution, proxy activity, and the
        // execution-bound collector work. Do not begin adapter collection,
        // official grading, or later post-processing while another admitted
        // peer is still executing or measuring.
        if let Some(barrier) = context.options.settlement_barrier {
            barrier.arrive(context.options.replica)?;
        }
        let config = &context.config;
        let workspace = &metadata.workspace;
        let mut artifact_owner = settled.evidence.artifact_owner.take();
        let frozen = artifact_owner
            .as_mut()
            .map(|owner| owner.freeze())
            .unwrap_or_default();
        if frozen.error {
            settled.issues.run_error(
                frozen
                    .diagnostic()
                    .unwrap_or_else(|| "agent artifact snapshot unavailable".into()),
            );
        }
        // Generic lifecycle transports only the recorder-owned handle. It
        // does not read or interpret protocol bytes; the adapter owns that
        // bounded correlation. The tape route is the direct-record fallback
        // for legacy activation copies that did not retain the private field.
        // Adapter-local tool recording is selected through the tool tape,
        // not the provider tape. Preserve that opaque selected cassette for
        // collection; the adapter alone reads and correlates its wire data.
        let provider_capture = context
            .tool_tape_cassette
            .as_ref()
            .and_then(|(mode, path)| (*mode == tape::Mode::Record).then_some(path.as_path()));
        if context.options.effective_tool_mode() == Some(tape::Mode::Record)
            && provider_capture.is_none()
        {
            settled.issues.run_error(
                "record collection lost the admitted tool-tape capture handle before adapter collection"
                    .into(),
            );
        }
        // Remote guests return adapter-owned workspace effects through a
        // receipt-backed opaque artifact before adapter collection or
        // evaluator cleanup can discard their private custody.
        if let Err(error) = settled.retain_guest_capsule() {
            settled.native_capsule_failure(error);
        }
        // Isolated guests return their frozen adapter state through the same
        // receipt-backed artifact route used for sidecars.  This lets the
        // host collect only after `settle` has closed and validated the outer
        // provider tape.  The envelope is generic custody; adapters alone
        // decode their own snapshots and provider wire data.
        let deferred_frozen = settled
            .evidence
            .guest
            .as_ref()
            .and_then(|guest| guest.deferred_collection.as_deref())
            .map(agents::FrozenArtifacts::from_deferred_collection_bytes)
            .transpose();
        if let Err(error) = deferred_frozen.as_ref() {
            settled.issues.run_error(error.clone());
        }
        let collection_frozen = deferred_frozen
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .unwrap_or(&frozen);
        let mut adapter_artifacts = agents::find(&config.agent)
            .map(|adapter| {
                adapter.collect(&agents::CollectionContext {
                    frozen: collection_frozen,
                    provider_capture,
                    run_dir: &settled.evidence.run_dir,
                    interface,
                    anthropic_input_includes_cache:
                        crate::providers::anthropic_input_includes_cache(&config.provider),
                    mcp_servers: &config.mcp_servers,
                    mcp_tape: context.mcp_tape_cassette.as_ref().map(|(mode, cassette)| {
                        agents::McpTapeContext {
                            mode: *mode,
                            cassette: Some(cassette),
                        }
                    }),
                })
            })
            .unwrap_or_default();
        if let Some(guest) = settled.evidence.guest.as_ref() {
            adapter_artifacts
                .sealed_artifacts
                .extend(guest.sealed_artifacts.clone());
            adapter_artifacts
                .errors
                .extend(guest.adapter_errors.clone());
        }
        for error in &adapter_artifacts.errors {
            settled.issues.run_error(error.clone());
        }
        let agent_outcome = settled.evidence.agent_outcome_with_artifacts(
            &config.agent,
            workspace,
            adapter_artifacts.final_response.as_deref(),
            collection_frozen,
        );
        let process_success = agent_outcome.failure.is_none();
        let agent_failure = agent_outcome.failure;
        let final_response = agent_outcome.final_response;
        if let Err(error) = settled.finish_native_capsule(workspace) {
            settled.native_capsule_failure(error);
        }
        // A usable submission can survive a failed client or proxy teardown.
        // Evaluator errors and process failures are recorded independently.

        #[cfg(test)]
        phases::entry_test_support::capture_grade_diagnostic(
            &context.project_root,
            &settled.evidence,
            &settled.issues,
            &settled.proxy_evidence,
        );
        let mut finalized = settled
            .evaluate_with_policy(
                final_response.as_deref().unwrap_or_default(),
                &metadata.metric_policy,
            )
            .finish();
        if finalized.resource_disposition == Some(workloads::ResourceDisposition::StoppedPreserved)
            && !context.options.json
            && finalized.issues.output_error.is_none()
        {
            let artifacts = finalized.workload.checkpoint_evidence();
            if let Err(error) = emit_warning(
                warnings,
                output,
                format!(
                    "Warning: workload resources were stopped and preserved; inspect {}; do not retry the same attempt implicitly",
                    artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ) {
                finalized.issues.output_error = Some(error.to_string());
            }
        }
        let issues = &mut finalized.issues;
        let guest = &finalized.evidence.guest;
        let provider_cleanup_error = providers::finish_session(bridge)
            .err()
            .map(|error| error.to_string());
        let mut artifact_evidence = Vec::new();
        if let Some(owner) = artifact_owner {
            let report = owner.handoff_and_settle();
            artifact_evidence = report.evidence;
            if let Some(error) = report.error {
                issues.run_error(error);
            }
        }
        if let Some(error) = guest
            .as_ref()
            .and_then(|metadata| metadata.artifact_error.as_ref())
        {
            issues.run_error(error.clone());
        }
        if context.config.native_snapshot_record
            && context.options.effective_tool_mode() == Some(tape::Mode::Record)
        {
            let has_snapshot = guest
                .as_ref()
                .and_then(|metadata| metadata.snapshot_artifact.as_ref())
                .is_some();
            let has_native = guest
                .as_ref()
                .and_then(|metadata| metadata.native_tool_sidecar.as_ref())
                .is_some()
                || adapter_artifacts.native_tool_sidecar.is_some()
                || adapter_artifacts
                    .sealed_artifacts
                    .iter()
                    .any(|item| item.kind == "native-tool-deferred");
            for error in missing_snapshot_evidence(has_snapshot, has_native) {
                issues.run_error(error.into());
            }
        }

        Ok(FinalResult {
            context,
            facts: ResultFacts {
                metadata,
                finalized,
                adapter_artifacts,
                process_success,
                agent_failure,
                provider_cleanup_error,
                artifact_evidence,
            },
        })
    }
}

fn missing_snapshot_evidence(has_snapshot: bool, has_native: bool) -> Vec<&'static str> {
    let mut errors = Vec::new();
    if !has_snapshot {
        errors.push("snapshot recording did not produce a workspace archive; this workload did not start or settle the selected snapshot backend");
    }
    if !has_native {
        errors.push("snapshot recording did not produce native tool boundaries; inspect the agent adapter and retained logs");
    }
    errors
}

struct PublicationResources {
    workload: Box<dyn workloads::WorkloadSession>,
    measurements: Option<measurements::Session>,
    proxy_evidence: crate::proxy::Evidence,
    runtime_output: Option<PathBuf>,
    workspace: PathBuf,
    workload_cleanup_confirmed: bool,
    capture_complete: bool,
    provider_cleanup_error: Option<String>,
    artifact_evidence: Vec<String>,
    output_error: Option<String>,
}
impl ResultFacts {
    /// No fallible OS path lookup or presentation can escape a graded result.
    fn seal<'context, 'options: 'context>(
        self,
        context: &'context Context<'options>,
    ) -> (RunResult<'context>, PublicationResources) {
        let Self {
            metadata,
            finalized,
            adapter_artifacts,
            process_success,
            agent_failure,
            provider_cleanup_error,
            artifact_evidence,
        } = self;
        let WorkloadMetadata {
            workspace,
            workload_baseline,
            workload_baseline_passed,
            effective_budgets,
            execution_plan,
            execution_plan_fingerprint,
            module,
            metric_policy,
        } = metadata;
        let config = &context.config;
        let options = context.options;
        let policy = &context.policy;
        let tape_cassette = &context.tape_cassette;
        let FinalizedAttempt {
            workload: workload_session,
            measurements: measurement_session,
            evidence:
                ExecutionEvidence {
                    observation,
                    artifact_owner: _,
                    capture_complete,
                    output: child,
                    run_dir: _,
                    guest,
                    elapsed_ms,
                    runtime_output,
                },
            proxy_evidence,
            cassette_usage_verified,
            outcome,
            evaluator_evidence,
            cleanup_confirmed: workload_cleanup_confirmed,
            issues,
            resource_disposition: workload_resource_disposition,
        } = finalized;
        let workload_artifacts = workload_session.checkpoint_evidence();
        let workload_success = outcome.success();
        let grading_failure = RunFailure::evaluation(&outcome.evaluation);
        let evaluation = outcome.evaluation;
        let collector_error = issues.collector_failure();
        let run_error = issues.run_error;
        let output_error = issues.output_error;
        let configured_kind = workload_session.evaluation_kind();
        let evaluation_kind = configured_kind.scored(&workload_baseline);
        let resolution_eligible = evaluation_kind.resolution_eligible();
        let disposition = RunDisposition::from_facts(RunFacts {
            evaluation: &evaluation,
            evaluation_kind,
            process_success,
            agent_failure: agent_failure.as_deref(),
            capture_incomplete: !capture_complete,
            run_error: run_error.as_deref(),
            collector_error: collector_error.as_deref(),
            output_error: output_error.as_deref(),
            provider_cleanup_error: provider_cleanup_error.as_deref(),
            workload_cleanup_confirmed,
            proxy_cleanup_confirmed: proxy_evidence.cleanup_confirmed,
        });
        let success = disposition.success;
        let workload_failure =
            grading_failure
                .as_ref()
                .map(|failure| workloads::EvaluationFailure {
                    kind: failure.kind.as_str(),
                    reason: failure.reason.clone(),
                });
        let recorder_name = match tape_cassette.as_ref() {
            Some((tape::Mode::Record, path)) => proxy_evidence
                .captures
                .iter()
                .find(|capture| capture.output == *path)
                .map(|capture| capture.adapter.as_str()),
            Some((tape::Mode::Replay, _)) => Some(
                options
                    .native_tool_inputs
                    .and_then(|inputs| inputs.get(options.replica.index.checked_sub(1)?))
                    .and_then(|input| input.recorder.as_deref())
                    .unwrap_or(crate::proxy::legacy_recorder_name()),
            ),
            None => None,
        };
        let recording_adapter = recorder_name.and_then(crate::proxy::recorder);
        let token_usage = crate::usage::from_sources_with_recorder(
            &config.provider,
            &config.agent,
            &if capture_complete {
                String::from_utf8_lossy(&child.stdout)
            } else {
                "".into()
            },
            &if capture_complete {
                String::from_utf8_lossy(&child.stderr)
            } else {
                "".into()
            },
            tape_cassette
                .as_ref()
                .filter(|_| cassette_usage_verified)
                .map(|(_, cassette)| cassette.as_path()),
            recording_adapter,
        );
        let token_usage =
            crate::usage::with_artifact_usage(token_usage, adapter_artifacts.token_usage);
        let token_usage = crate::usage::with_artifact_usage(
            token_usage,
            guest.as_ref().map(|metadata| metadata.token_usage.clone()),
        );
        // A remote guest can complete without exposing a parseable stdout
        // transcript. Its host-side adapter collection is sealed after the
        // guest returns, so retain the guest counter when present but fall
        // back to that adapter-owned terminal artifact when it is absent.
        let steps = sealed_step_count(
            guest.as_ref().and_then(|metadata| metadata.steps),
            adapter_artifacts.steps,
        )
        .or_else(|| {
            capture_complete
                .then(|| {
                    crate::usage::steps_from_agent(
                        &config.agent,
                        &bounded(&child.stdout),
                        &bounded(&child.stderr),
                    )
                })
                .flatten()
        });
        // Tool-call evidence is parsed from provider responses retained by the
        // llmtape proxy. Agent stdout/stderr are presentation streams only.
        let observed_tool_calls = tape_cassette
            .as_ref()
            .filter(|_| cassette_usage_verified)
            .and_then(|(_, cassette)| {
                recording_adapter.and_then(|recorder| recorder.recorded_tool_calls(cassette))
            });
        let tool_calls_available = observed_tool_calls.is_some();
        let tool_calls = observed_tool_calls.unwrap_or_default();
        let local_tool_transcript = guest
            .as_ref()
            .and_then(|metadata| metadata.local_tool_transcript.clone())
            .or(adapter_artifacts.local_tool_transcript.clone());
        let deterministic_manifest = guest
            .as_ref()
            .and_then(|metadata| metadata.deterministic_manifest.clone())
            .or(adapter_artifacts.deterministic_manifest.clone());
        let native_trace = guest
            .as_ref()
            .and_then(|metadata| metadata.native_trace.clone())
            .or(adapter_artifacts.native_trace.clone());
        let native_tool_sidecar = guest
            .as_ref()
            .and_then(|metadata| metadata.native_tool_sidecar.clone())
            .or(adapter_artifacts.native_tool_sidecar.clone());
        let mut sealed_artifacts = adapter_artifacts.sealed_artifacts.clone();
        if let Some(bytes) = &context.normalization_artifact {
            sealed_artifacts.push(agents::SealedArtifact {
                kind: "normalization-policy".into(),
                bytes: bytes.clone(),
            });
        }
        let mut stderr = bounded(&child.stderr);
        if let Some(error) = provider_cleanup_error.as_ref() {
            stderr.push_str("\nASB provider cleanup error: ");
            stderr.push_str(error);
            stderr.push('\n');
        }
        if let Some(error) = output_error.as_ref() {
            stderr.push_str("\nASB presentation error: ");
            stderr.push_str(error);
            stderr.push('\n');
        }
        for error in &issues.collector_errors {
            stderr.push_str("\nASB collector error: ");
            stderr.push_str(error);
            stderr.push('\n');
        }
        if let Some(error) = run_error.as_deref() {
            stderr.push_str("\nASB run error: ");
            stderr.push_str(error);
            stderr.push('\n');
        }
        if let Some(failure) = &disposition.failure {
            stderr.push_str("\nASB failure (");
            stderr.push_str(failure.kind.as_str());
            stderr.push_str("): ");
            stderr.push_str(&failure.reason);
            stderr.push('\n');
        }
        let result = RunResult {
            replay_selection: options.replay_selection(&config.agent),
            provider_replay_consumed: (options.mode == Some(tape::Mode::Replay))
                .then_some(cassette_usage_verified && proxy_evidence.cleanup_confirmed),
            native_replay_consumed: options
                .native_snapshot_replay()
                .then(|| {
                    guest
                        .as_ref()
                        .and_then(|value| value.native_replay_consumed)
                })
                .flatten(),
            recording: None,
            execution: options.batch.map(|batch| batch.execution(options.replica)),
            planning: policy.clone(),
            effective_budgets,
            execution_plan: Some(execution_plan),
            execution_plan_fingerprint: Some(execution_plan_fingerprint),
            module,
            benchmark_outcome: Some(workloads::outcome::BenchmarkOutcome {
                schema_version: 1,
                execution_success: Some(process_success),
                evaluation,
                cleanup_confirmed: Some(
                    workload_cleanup_confirmed
                        && proxy_evidence.cleanup_confirmed
                        && provider_cleanup_error.is_none(),
                ),
                evidence: evaluator_evidence,
                policy: metric_policy,
                lineage: workloads::outcome::EvaluationLineage {
                    execution_attempt: workload_session.invocation().identity.attempt_id.clone(),
                    evaluation_id: workloads::TaskIdentity::local("evaluation", None).attempt_id,
                    supersedes: None,
                },
            }),
            replica_index: Some(options.replica.index),
            execution_cohort: options.execution_cohort,
            schema_version: 2,
            disposition,
            agent: &config.agent,
            workload: &config.workload,
            task: workload_session.invocation().identity.clone(),
            provider: &config.provider,
            model: &config.model,
            process_success,
            workload_success: resolution_eligible.then_some(workload_success).flatten(),
            workload_resource_disposition,
            workload_artifacts,
            workload_evaluation: evaluation_kind.as_str(),
            workload_baseline_passed,
            workload_baseline,
            workload_failure,
            success_rate: resolution_eligible
                .then_some(workload_success)
                .flatten()
                .map(|resolved| if resolved { 1.0 } else { 0.0 }),
            adapter_smoke_success: (evaluation_kind
                == workloads::KnownEvaluationKind::AdapterSmoke)
                .then_some(success),
            elapsed_ms,
            exit_code: child.status.code(),
            observation,
            token_usage,
            steps,
            tool_calls,
            tool_calls_available,
            adapter_tool_call_count: adapter_artifacts.tool_call_count.or_else(|| {
                capture_complete
                    .then(|| {
                        agents::find(&config.agent).and_then(|adapter| {
                            adapter
                                .tool_call_count(&bounded(&child.stdout), &bounded(&child.stderr))
                        })
                    })
                    .flatten()
            }),
            local_tool_transcript,
            deterministic_manifest,
            native_trace,
            native_tool_sidecar,
            sealed_artifacts,
            snapshot_artifact: guest
                .as_ref()
                .and_then(|guest| guest.snapshot_artifact.clone()),
            cassette: tape_cassette
                .as_ref()
                .map(|(_, cassette)| cassette.display().to_string()),
            measurement_directory: context.measurement_path.clone(),
            stdout: bounded(&child.stdout),
            stderr,
        };

        (
            result,
            PublicationResources {
                workload: workload_session,
                measurements: measurement_session,
                proxy_evidence,
                runtime_output,
                workspace,
                workload_cleanup_confirmed,
                capture_complete,
                provider_cleanup_error,
                artifact_evidence,
                output_error,
            },
        )
    }
}
impl FinalResult<'_> {
    fn publish(
        self,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        let Self { context, facts } = self;
        let (mut result, resources) = facts.seal(&context);
        Publication {
            context: &context,
            result: &mut result,
            resources,
        }
        .finish(output, warnings)
    }
}
struct Publication<'context, 'result> {
    context: &'context Context<'context>,
    result: &'result mut RunResult<'context>,
    resources: PublicationResources,
}
impl Publication<'_, '_> {
    fn finish(
        self,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        let Self {
            context,
            result,
            resources,
        } = self;
        let PublicationResources {
            workload: _workload,
            measurements: measurement_session,
            proxy_evidence,
            runtime_output,
            workspace,
            workload_cleanup_confirmed,
            capture_complete,
            provider_cleanup_error,
            artifact_evidence,
            output_error,
        } = resources;
        let config_path = context.config_path.as_path();
        let config = &context.config;
        let options = context.options;
        let measurement_path = &context.measurement_path;
        let json = options.json;
        // A recording failure changes command completion, never the actual
        // process or official workload grade. Commit before sealing outputs.
        let recording_error = options.recording.and_then(|recorder| {
            let batch = options.batch.expect("campaign-owned recording");
            match recorder.commit(batch.number(), options.replica, result, &proxy_evidence) {
                Ok(evidence) => {
                    result.recording = Some(evidence);
                    None
                }
                Err(error) => Some(error),
            }
        });
        if let Some(error) = &recording_error {
            result.disposition.recording_failed(error.to_string());
        }
        let log_publication =
            phases::LogPublication::publish(&context.publication_config, &config.log_file, result)
                .with_artifact_evidence(&artifact_evidence);
        progress::status(
            options.interactive,
            "tearing down collectors and publishing measurements",
        );
        let measurement_completion = phases::finish_measurements(measurement_session, result)
            .with_artifact_evidence(&artifact_evidence);
        // A compact attempt may release only its ASB-created task workspace.
        // Installed adapters, downloaded workloads, cassettes, logs and
        // collector captures remain project inputs or sealed evidence.
        let compact_workspace = super::compaction::eligible(
            config.retention.compact_completed_attempts,
            result.disposition.success,
            workload_cleanup_confirmed && proxy_evidence.cleanup_confirmed,
            result.workload_resource_disposition,
        );
        // This private record preserves evaluator outcomes, including unresolved
        // tasks. Its absence never changes a benchmark grade or blocks execution;
        // a future continuation must also validate its input and evidence seals.
        let mut completion_outputs = (!compact_workspace)
            .then(|| workspace.clone())
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(path) = measurement_path.as_ref() {
            completion_outputs.push(path.clone());
        }
        if let Some(path) = runtime_output {
            completion_outputs.push(path);
        }
        let mut continuation_warnings = Vec::new();
        if !capture_complete {
            continuation_warnings.push(diagnostics::Warning::new(
                diagnostics::Stage::Outcome,
                config_path,
                &completion::directory(config_path, &config.log_file),
                &Error::Input(
                    "agent output capture is incomplete; continuation evidence cannot be published"
                        .into(),
                ),
            ));
        }
        if log_publication.confirmed()
            && measurement_completion.confirmed()
            && provider_cleanup_error.is_none()
            && recording_error.is_none()
            && capture_complete
        {
            match completion::persist(
                &context.publication_config,
                &config.log_file,
                result,
                workload_cleanup_confirmed,
                &result.workload_artifacts,
                &proxy_evidence,
                &completion::Context {
                    replica: options.replica,
                    outputs: &completion_outputs,
                },
            ) {
                Ok(record) => {
                    let mut completion_sealed = true;
                    if let Some(observer) = options.on_completion
                        && let Err(error) = observer.completed(options.replica, &record)
                    {
                        completion_sealed = false;
                        let destination = observer.evidence_destination(options.replica);
                        let warning = diagnostics::Warning::new(
                            diagnostics::Stage::Replica,
                            config_path,
                            destination.as_deref().unwrap_or(&record),
                            &error,
                        );
                        continuation_warnings.push(if destination.is_some() {
                            warning
                        } else {
                            warning.source_outcome()
                        });
                    }
                    // A failed checkpoint publication is intentionally enough
                    // to preserve the workspace: resume/reconciliation may
                    // still require it.  Direct runs have their completion
                    // record as the durable publication boundary.
                    if compact_workspace && completion_sealed {
                        let completion_directory =
                            completion::directory(&context.publication_config, &config.log_file);
                        if let Err(error) = super::compaction::remove_workspace(
                            &workspace,
                            &completion_directory,
                            options.replica,
                        ) {
                            continuation_warnings.push(diagnostics::Warning::new(
                                diagnostics::Stage::Outcome,
                                config_path,
                                &completion_directory,
                                &error,
                            ));
                        }
                    }
                }
                Err(error) => continuation_warnings.push(diagnostics::Warning::new(
                    diagnostics::Stage::Outcome,
                    config_path,
                    &completion::directory(config_path, &config.log_file),
                    &error,
                )),
            }
        }
        progress::clear(options.interactive);
        let presentation = (|| {
            if json {
                render_json(result, output)?;
            } else {
                render_human(result, output)?;
            }
            for warning in continuation_warnings {
                warning.emit(json, output, warnings)?;
            }
            Ok(())
        })();
        let outcome = phases::final_outcome(
            result.disposition.failure.clone(),
            log_publication,
            measurement_completion,
        );
        // Output loss is not an ordinary completed benchmark failure: repeated
        // execution must stop even when a separate task failure has higher priority.
        let outcome = match output_error {
            Some(error) => Err(Error::Io(match outcome {
                Ok(()) => error,
                Err(primary)
                    if result
                        .disposition
                        .failure
                        .as_ref()
                        .map(|failure| failure.kind)
                        == Some(FailureKind::OutputError) =>
                {
                    primary.to_string()
                }
                Err(primary) => format!("{primary}; presentation failed: {error}"),
            })),
            None => outcome,
        };
        let outcome = diagnostics::preserve_primary(outcome, presentation);
        let outcome = match recording_error {
            Some(error) => Err(Error::Io(match outcome {
                Ok(()) => error.to_string(),
                Err(primary) => format!("{primary}; {error}"),
            })),
            None => outcome,
        };
        if let Some(cleanup) = provider_cleanup_error {
            match outcome {
                Ok(()) => Err(Error::Run(cleanup)),
                Err(Error::Io(primary)) => Err(Error::Io(format!(
                    "{primary}; provider cleanup failed: {cleanup}"
                ))),
                Err(primary)
                    if result.disposition.failure.as_ref().map(|f| f.kind)
                        == Some(FailureKind::ProviderCleanupError) =>
                {
                    Err(primary)
                }
                Err(primary) => Err(Error::Run(format!(
                    "{primary}; provider cleanup failed: {cleanup}"
                ))),
            }
        } else {
            outcome
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_recording_requires_both_workspace_and_native_evidence() {
        assert!(super::missing_snapshot_evidence(true, true).is_empty());
        assert!(super::missing_snapshot_evidence(false, true)[0].contains("workspace archive"));
        assert!(super::missing_snapshot_evidence(true, false)[0].contains("tool boundaries"));
        assert_eq!(super::missing_snapshot_evidence(false, false).len(), 2);
    }
    #[test]
    fn normalizer_decoder_requires_a_supported_negotiated_interface() {
        use crate::endpoint::Interface;
        for interface in [
            Interface::OpenAiResponses,
            Interface::OpenAiChat,
            Interface::AnthropicMessages,
        ] {
            assert!(super::require_normalizer_decoder(Some(interface)).is_ok());
        }
        assert!(super::require_normalizer_decoder(Some(Interface::Gemini)).is_err());
        assert!(super::require_normalizer_decoder(None).is_err());
    }
    use super::{sealed_step_count, validate_mcp_relay_capability, validate_mcp_server_capability};
    use crate::benchmark::{BenchmarkConfig, McpServerConfig};

    fn config(agent: &str) -> BenchmarkConfig {
        BenchmarkConfig {
            native_snapshot_record: false,
            planning: None,
            agent: agent.into(),
            workload: "bug-fix".into(),
            provider: "openrouter".into(),
            model: "tool-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
            retention: Default::default(),
            mcp_servers: vec![McpServerConfig {
                id: "filesystem".into(),
                program: "mcp-filesystem".into(),
                args: vec!["workspace".into()],
            }],
        }
    }

    #[test]
    fn declared_mcp_servers_fail_before_an_unsupported_adapter_can_ignore_them() {
        let error = validate_mcp_server_capability(&config("pi"))
            .expect_err("Pi must not silently ignore an ordinary MCP declaration");
        assert!(
            error
                .to_string()
                .contains("agent `pi` does not support declared MCP servers yet")
        );
    }

    #[test]
    fn declared_mcp_servers_are_admitted_for_codex() {
        validate_mcp_server_capability(&config("codex"))
            .expect("Codex owns ordinary MCP declaration materialization");
    }

    #[test]
    fn mcp_recording_requires_a_relay_capable_adapter() {
        validate_mcp_relay_capability(&config("codex"), Some(crate::tape::Mode::Record))
            .expect("Codex has the transparent relay route");
        let error = validate_mcp_relay_capability(&config("pi"), Some(crate::tape::Mode::Record))
            .expect_err("Pi must not launch an unrecorded MCP server");
        assert!(error.to_string().contains("transparent MCP relay route"));
        let mut no_servers = config("codex");
        no_servers.mcp_servers.clear();
        assert!(
            validate_mcp_relay_capability(&no_servers, Some(crate::tape::Mode::Record))
                .unwrap_err()
                .to_string()
                .contains("requires at least one")
        );
        validate_mcp_relay_capability(&config("codex"), Some(crate::tape::Mode::Replay))
            .expect("guest execution stages only the selected sealed relay bundle");
    }

    #[test]
    fn sealed_adapter_steps_fill_an_absent_guest_counter_without_replacing_one() {
        assert_eq!(sealed_step_count(None, Some(3)), Some(3));
        assert_eq!(sealed_step_count(Some(2), Some(3)), Some(2));
        assert_eq!(sealed_step_count(None, None), None);
    }
}
