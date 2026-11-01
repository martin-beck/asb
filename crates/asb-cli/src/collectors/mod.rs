//! Registry for selectable benchmark collectors.
//!
//! Runtime behavior, presentation and tool identity are dispatched through an
//! extensible catalog of collector-specific adapters.

pub(crate) mod adapters;
mod output;
#[cfg(test)]
pub(crate) use output::process_output_fixture;
pub(crate) use output::{read_complete_output, validate_processed_output};

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(crate) struct ProcessContext<'a> {
    pub name: &'a str,
    pub status: Option<std::process::ExitStatus>,
    pub capture_complete: bool,
    pub controlled_interruption: bool,
    pub stderr: &'a str,
    /// Opened admitted sources, not mutable filesystem names. Only positional
    /// readers are exposed to processing; prefixes above are diagnostics.
    sources: [&'a crate::evidence::Capture; 2],
}
impl<'a> ProcessContext<'a> {
    #[cfg(test)]
    pub fn new(
        name: &'a str,
        status: &'a std::process::ExitStatus,
        diagnostics: [&'a str; 2],
        sources: [&'a crate::evidence::Capture; 2],
    ) -> Self {
        Self {
            name,
            status: Some(*status),
            capture_complete: true,
            controlled_interruption: false,
            stderr: diagnostics[1],
            sources,
        }
    }
    pub fn from_completion(
        name: &'a str,
        completion: &crate::measurements::ProducerCompletion,
        diagnostics: [&'a str; 2],
        sources: [&'a crate::evidence::Capture; 2],
    ) -> Self {
        Self {
            name,
            status: completion.exit,
            capture_complete: completion.capture_complete,
            controlled_interruption: completion.interruption_requested
                && completion.interruption_acknowledged
                && completion.guardian_cleanup_confirmed == Some(true)
                && completion.capture_complete,
            stderr: diagnostics[1],
            sources,
        }
    }
    pub fn stdout_reader(&self) -> Result<impl std::io::Read + '_, String> {
        self.sources[0].reader()
    }
    pub fn stderr_reader(&self) -> Result<impl std::io::Read + '_, String> {
        self.sources[1].reader()
    }
}

#[derive(serde::Serialize)]
pub(crate) struct ProcessData {
    pub status: String,
    pub metrics: BTreeMap<String, f64>,
    pub series: BTreeMap<String, Vec<f64>>,
    pub timestamps: BTreeMap<String, Vec<f64>>,
    pub artifact: Option<String>,
}

impl ProcessData {
    pub fn from_output(context: &ProcessContext<'_>) -> Self {
        let completed =
            context.capture_complete && context.status.is_none_or(|status| status.success());
        Self {
            status: if completed { "completed" } else { "failed" }.into(),
            metrics: BTreeMap::new(),
            series: BTreeMap::new(),
            timestamps: BTreeMap::new(),
            artifact: None,
        }
    }
}

pub(crate) struct PreflightContext<'a> {
    pub project_root: Option<&'a Path>,
    pub measurement_dir: &'a Path,
    pub selected: &'a [String],
    pub tool_dir: Option<&'a Path>,
    pub sudo_collectors: &'a mut BTreeSet<String>,
    pub states: &'a mut CollectorStates,
}

/// Per-attempt typed data prepared before launch and retained by its session.
/// No process-wide cache or on-disk re-read bridges the preparation boundary.
#[derive(Default)]
pub(crate) struct CollectorStates(BTreeMap<std::any::TypeId, Box<dyn std::any::Any>>);

impl CollectorStates {
    pub(crate) fn state<T: Default + 'static>(&mut self) -> std::rc::Rc<std::cell::RefCell<T>> {
        self.0
            .entry(std::any::TypeId::of::<T>())
            .or_insert_with(|| Box::new(std::rc::Rc::new(std::cell::RefCell::new(T::default()))))
            .downcast_ref::<std::rc::Rc<std::cell::RefCell<T>>>()
            .expect("state is keyed by its concrete type")
            .clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SharedArtifact {
    StackSamples,
}

pub(crate) enum ArtifactDisplay {
    Svg {
        class: &'static str,
        label: &'static str,
    },
    Status {
        class: &'static str,
        label: &'static str,
        suffix: &'static str,
    },
}

pub(crate) struct GraphSection {
    pub title: &'static str,
    pub description: &'static str,
}

/// A row of immutable, validated comparison evidence made available to
/// collector-owned report contributions.  The generic comparison pipeline does
/// not interpret collector metric names or artifact formats.
pub(crate) struct ReportRow<'a> {
    pub label: &'a str,
    pub metrics: &'a BTreeMap<String, f64>,
    pub components: Option<&'a serde_json::Value>,
    pub documents: &'a BTreeMap<String, serde_json::Value>,
}

/// A collector-owned graph ready for generic publication in an offline report.
/// `filename` is intentionally explicit so adapters, not comparison, own the
/// stable artifact identity of their specialized visualizations.
pub(crate) struct ReportGraph {
    pub filename: String,
    pub svg: String,
}

/// A supplemental link for an owned collector artifact (for example, a local
/// trace opened by that collector's official viewer).
pub(crate) struct ReportArtifactLink {
    pub label: &'static str,
    pub href: String,
    pub primary: Option<ReportPrimaryLink>,
}

/// Optional prominent viewer presentation and terminal trace labels.
pub(crate) struct ReportPrimaryLink {
    pub heading: &'static str,
    pub text: &'static str,
    pub class: &'static str,
    pub download_label: &'static str,
    pub file_label: &'static str,
}

/// Non-secret benchmark lifecycle events available to every collector.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind", content = "phase", rename_all = "snake_case")]
pub(crate) enum LifecycleEvent {
    RunStarted,
    RunFinishing,
    RunFailed,
    PhaseBegin(LifecyclePhase),
    PhaseEnd(LifecyclePhase),
    PhaseFailed(LifecyclePhase),
    MeasurementsReady,
    AgentSpawn,
    AgentExecuteBegin,
    AgentStarted,
    AgentSpawnFailed,
    AgentAttachFailed,
    AgentWaitFailed,
    AgentExecuteEnd,
    AgentExited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecyclePhase {
    ProviderDiscovery,
    ProviderSetup,
    WorkloadPreparation,
    ProxyPreparation,
    AgentPreparation,
    WorkloadEvaluation,
    WorkloadCleanup,
}

impl LifecyclePhase {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ProviderDiscovery => "provider.discovery",
            Self::ProviderSetup => "provider.setup",
            Self::WorkloadPreparation => "workload.prepare",
            Self::ProxyPreparation => "proxy.prepare",
            Self::AgentPreparation => "agent.prepare",
            Self::WorkloadEvaluation => "workload.evaluate",
            Self::WorkloadCleanup => "workload.cleanup",
        }
    }
}

/// Stateless collector behavior shared through the registry.
///
/// Adapters override their supported operations and keep mutable run state in
/// `Session`. Defaults leave unsupported capabilities absent or do no work.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TargetScope {
    AgentProcess,
    HostContext,
    RunObservation,
    Container,
}

/// Selectable adapter-owned values displayed by setup and CLI discovery.
pub(crate) struct SelectionOption {
    pub selection: String,
    pub label: &'static str,
    pub description: &'static str,
    pub default: bool,
}

pub(crate) struct SelectionExtension {
    pub tab: &'static str,
    pub description: &'static str,
    pub all_selection: Option<String>,
    pub all_description: &'static str,
    pub required: bool,
    pub options: Vec<SelectionOption>,
}

pub(crate) trait CollectorAdapter: Sync {
    fn private_service_aliases(&self) -> &'static [&'static str] {
        &[]
    }
    fn private_service_entry(&self, _: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
        None
    }
    /// Configure collector-owned host capabilities during project setup.
    fn configure_project(
        &self,
        _: &Path,
        _: &mut dyn std::io::Write,
        _: bool,
        _: &mut crate::project::ProjectConfig,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
    /// Hold collector admission and active-service leases throughout reset.
    fn lock_project_reset(&self, _: &Path) -> Result<Vec<std::fs::File>, crate::Error> {
        Ok(Vec::new())
    }
    /// Conservative per-attempt retained capture estimate.
    fn estimated_disk_mib(&self) -> u64 {
        128
    }
    /// Concise human-readable description for command-line discovery.
    fn description(&self) -> &'static str {
        "collector"
    }
    fn target_scope(&self, _backend: &str) -> TargetScope {
        if self.requires_pid() {
            TargetScope::AgentProcess
        } else {
            TargetScope::RunObservation
        }
    }
    fn supports_backend(&self, backend: &str) -> bool {
        matches!(backend, "native" | "local-docker")
    }
    fn id(&self) -> &'static str;
    /// Whether this adapter owns a configured selection value.  Most
    /// collectors are selected by their stable ID; adapters with typed
    /// sub-selections can extend that grammar without teaching generic
    /// planning or setup code about their syntax.
    fn accepts_selection(&self, selection: &str) -> bool {
        selection == self.id()
    }
    fn selection_extension(&self) -> Option<SelectionExtension> {
        None
    }
    fn runtime_inputs(
        &self,
        context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        self.tool_id().map_or_else(
            || Ok(Vec::new()),
            |id| {
                crate::tools::find(id)
                    .ok_or_else(|| {
                        crate::Error::Input("collector tool has no input declaration".into())
                    })?
                    .runtime_inputs(context)
            },
        )
    }
    fn environment_flag(&self) -> Option<&'static str> {
        None
    }
    fn tool_id(&self) -> Option<&'static str> {
        None
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &[]
    }
    fn requires_pid(&self) -> bool {
        false
    }
    /// Process-scoped collectors finish when execution ends, not after the
    /// workload's potentially long grading phase. Whole-run tracing can opt out.
    fn stop_at_execution_end(&self) -> bool {
        self.requires_pid()
    }
    /// Inspect the prepared entry point. Arguments can include prompts and
    /// must never be logged or persisted by this hook.
    fn agent_launch(
        &self,
        _session: &mut crate::measurements::Session,
        _program: &str,
        _args: &[String],
    ) {
    }
    fn configure(
        &self,
        _session: &mut crate::measurements::Session,
        _selected: &[String],
    ) -> Result<(), String> {
        Ok(())
    }
    /// Some collectors must resolve entry points inside the execution
    /// namespace, where its executables/interpreters are actually installed.
    fn prefers_execution_namespace(&self) -> bool {
        false
    }
    fn execution_namespace_requested(&self, _session: &mut crate::measurements::Session) -> bool {
        false
    }
    fn export_execution_namespace_state(
        &self,
        _session: &mut crate::measurements::Session,
    ) -> Result<serde_json::Value, String> {
        Err("collector does not export execution-namespace observations".into())
    }
    fn import_execution_namespace_state(
        &self,
        _session: &mut crate::measurements::Session,
        _value: &serde_json::Value,
    ) -> Result<(), String> {
        Err("collector does not import execution-namespace observations".into())
    }
    fn agent_launch_prepared(
        &self,
        session: &mut crate::measurements::Session,
        target: &crate::launch::TargetLaunch,
    ) {
        let args = target
            .args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.agent_launch(session, &target.program.to_string_lossy(), &args);
    }
    /// The direct agent child has been created.  This is a provenance-bearing
    /// identity, unlike an arbitrary matching descendant discovered later by
    /// sampling: adapters may retain it when clients rewrite their argv.
    fn agent_process_spawned(
        &self,
        _session: &mut crate::measurements::Session,
        _pid: u32,
    ) -> Result<(), String> {
        Ok(())
    }
    fn preflight(&self, _context: &mut PreflightContext<'_>) -> Result<(), String> {
        Ok(())
    }
    fn prioritizes_artifact(&self, _name: &str) -> bool {
        false
    }
    fn display_artifact(&self, _name: &str) -> Option<ArtifactDisplay> {
        None
    }
    fn graph_section(&self, _name: &str) -> Option<GraphSection> {
        None
    }
    /// Render specialized offline-report graphs from validated rows.  Generic
    /// comparison writes the returned SVGs without interpreting their metric
    /// names, state taxonomy, or presentation semantics.
    fn report_graphs(&self, _rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
        Vec::new()
    }
    /// Return extra viewer links for one retained artifact.  The owning
    /// collector validates both its name and source before exposing a link.
    fn report_artifact_links(&self, _name: &str, _source: &Path) -> Vec<ReportArtifactLink> {
        Vec::new()
    }
    fn producer_policy(&self, _name: &str) -> crate::measurements::ProducerPolicy {
        crate::measurements::ProducerPolicy {
            stream_bytes: 256 * 1024 * 1024,
            graceful: None,
        }
    }
    /// Allows complete captures to reach adapter-specific data validation; it
    /// does not itself certify any metrics or rewrite an actual exit status.
    fn accepts_termination(
        &self,
        _name: &str,
        completion: &crate::measurements::ProducerCompletion,
    ) -> bool {
        completion.exit.is_none_or(|status| status.success())
    }
    /// Optional collectors may publish a complete failed capture as unavailable.
    fn allows_failed_termination(&self, _name: &str) -> bool {
        false
    }
    /// Owning adapters can preserve raw evidence rejected by data validation.
    fn retain_raw_on_processing_failure(&self) -> bool {
        false
    }
    fn process(
        &self,
        _session: &mut crate::measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        Ok(ProcessData::from_output(context))
    }
    fn finish(&self, _session: &mut crate::measurements::Session) -> Result<(), String> {
        Ok(())
    }
    /// Start collectors that need to observe events before the child exists.
    fn start(&self, _session: &mut crate::measurements::Session) -> Result<(), String> {
        Ok(())
    }
    fn lifecycle_event(
        &self,
        _session: &mut crate::measurements::Session,
        _event: LifecycleEvent,
    ) -> Result<(), String> {
        Ok(())
    }
    /// Attach this collector to an already spawned benchmark process.
    fn attach_pid(
        &self,
        _session: &mut crate::measurements::Session,
        _pid: u32,
    ) -> Result<(), String> {
        Ok(())
    }
    /// A verified local isolated process is not ASB's direct child. Adapters
    /// that manage membership must override this instead of moving its PID.
    fn attach_isolated_pid(
        &self,
        session: &mut crate::measurements::Session,
        pid: u32,
    ) -> Result<(), String> {
        self.attach_pid(session, pid)
    }
}

pub(crate) fn all() -> &'static [&'static dyn CollectorAdapter] {
    adapters::REGISTRY
}

pub(crate) fn configure_project(
    root: &Path,
    output: &mut dyn std::io::Write,
    yes: bool,
    config: &mut crate::project::ProjectConfig,
) -> Result<(), crate::Error> {
    configure_project_for(all(), root, output, yes, config)
}

fn configure_project_for(
    adapters: &[&dyn CollectorAdapter],
    root: &Path,
    output: &mut dyn std::io::Write,
    yes: bool,
    config: &mut crate::project::ProjectConfig,
) -> Result<(), crate::Error> {
    for adapter in adapters {
        adapter.configure_project(root, output, yes, config)?;
    }
    Ok(())
}

pub(crate) fn lock_project_reset(root: &Path) -> Result<Vec<std::fs::File>, crate::Error> {
    lock_project_reset_for(all(), root)
}

fn lock_project_reset_for(
    adapters: &[&dyn CollectorAdapter],
    root: &Path,
) -> Result<Vec<std::fs::File>, crate::Error> {
    let mut leases = Vec::new();
    for adapter in adapters {
        leases.extend(adapter.lock_project_reset(root)?);
    }
    Ok(leases)
}
pub(crate) fn find(id: &str) -> Option<&'static dyn CollectorAdapter> {
    all().iter().copied().find(|collector| collector.id() == id)
}

pub(crate) fn private_service_entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    let Some((id, rest)) = args.split_first() else {
        eprintln!("asb: collector service requires a registered collector");
        return std::process::ExitCode::from(2);
    };
    let Some(adapter) = id.to_str().and_then(find) else {
        eprintln!("asb: collector service requires a registered collector");
        return std::process::ExitCode::from(2);
    };
    adapter.private_service_entry(rest).unwrap_or_else(|| {
        eprintln!("asb: selected collector has no private service");
        std::process::ExitCode::from(2)
    })
}

pub(crate) fn legacy_private_service_entry(
    args: &[std::ffi::OsString],
) -> Option<std::process::ExitCode> {
    let (name, rest) = args.split_first()?;
    let name = name.to_str()?;
    adapters::REGISTRY
        .iter()
        .find(|adapter| adapter.private_service_aliases().contains(&name))?
        .private_service_entry(rest)
}

/// Return the adapter that owns a configured collector selection, including
/// adapter-defined sub-selections such as `bpftrace:sched-latency`.
pub(crate) fn adapter_for_selection(selection: &str) -> Option<&'static dyn CollectorAdapter> {
    all()
        .iter()
        .copied()
        .find(|collector| collector.accepts_selection(selection))
}

/// Resolve transitive dependencies once, keeping user-selected order stable.
pub(crate) fn expand_dependencies(selected: &[String]) -> Vec<String> {
    let mut expanded = selected.to_vec();
    let mut index = 0;
    while index < expanded.len() {
        if let Some(adapter) = adapter_for_selection(&expanded[index]) {
            for dependency in adapter.dependencies() {
                if !expanded.iter().any(|item| item == dependency) {
                    expanded.push((*dependency).into());
                }
            }
        }
        index += 1;
    }
    expanded
}

pub(crate) fn dependency_tools(selected: &[String]) -> Vec<&'static str> {
    let expanded = expand_dependencies(selected);
    let mut tools = Vec::new();
    for id in &expanded {
        if let Some(adapter) = find(id) {
            for dependency in adapter.dependencies() {
                if let Some(tool) = find(dependency).and_then(|item| item.tool_id())
                    && !tools.contains(&tool)
                {
                    tools.push(tool);
                }
            }
        }
    }
    tools
}
pub(crate) fn prioritizes_artifact(name: &str) -> bool {
    all()
        .iter()
        .any(|adapter| adapter.prioritizes_artifact(name))
}
pub(crate) fn display_artifact(name: &str) -> Option<ArtifactDisplay> {
    all()
        .iter()
        .find_map(|adapter| adapter.display_artifact(name))
}
pub(crate) fn graph_section(name: &str) -> Option<GraphSection> {
    all().iter().find_map(|adapter| adapter.graph_section(name))
}
pub(crate) fn report_graphs(rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
    all()
        .iter()
        .flat_map(|adapter| adapter.report_graphs(rows))
        .collect()
}
pub(crate) fn report_artifact_links(name: &str, source: &Path) -> Vec<ReportArtifactLink> {
    all()
        .iter()
        .flat_map(|adapter| adapter.report_artifact_links(name, source))
        .collect()
}
#[cfg(test)]
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    all().iter().map(|collector| collector.id())
}
pub(crate) fn selected_or_environment(id: &str, selected: &[String]) -> bool {
    find(id).is_some_and(|collector| {
        selected
            .iter()
            .any(|value| collector.accepts_selection(value))
            || collector
                .environment_flag()
                .is_some_and(super::measurements::env_true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hooks_are_dispatched_and_reset_leases_remain_held() {
        struct Fixture;
        impl CollectorAdapter for Fixture {
            fn id(&self) -> &'static str {
                "fixture"
            }
            fn configure_project(
                &self,
                _: &Path,
                output: &mut dyn std::io::Write,
                yes: bool,
                config: &mut crate::project::ProjectConfig,
            ) -> Result<(), crate::Error> {
                assert!(yes);
                writeln!(output, "fixture configured").unwrap();
                config.perfetto_instances = vec!["fixture-slot".into()];
                Ok(())
            }
            fn lock_project_reset(&self, root: &Path) -> Result<Vec<std::fs::File>, crate::Error> {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(root.join("fixture.lease"))
                    .unwrap();
                file.try_lock().unwrap();
                Ok(vec![file])
            }
        }
        let root = std::env::temp_dir().join(format!(
            "asb-collector-hooks-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let adapters: &[&dyn CollectorAdapter] = &[&Fixture];
        let mut config = crate::project::ProjectConfig::default();
        let mut output = Vec::new();
        configure_project_for(adapters, &root, &mut output, true, &mut config).unwrap();
        assert_eq!(config.perfetto_instances, ["fixture-slot"]);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("fixture configured")
        );
        let leases = lock_project_reset_for(adapters, &root).unwrap();
        let other = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("fixture.lease"))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(leases);
        other.try_lock().unwrap();
        drop(other);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_preserves_metadata_and_shutdown_policy_for_every_adapter() {
        let expected = [
            ("agent-memory", Some("ASB_MEASURE_AGENT_MEMORY"), None, true),
            (
                "agent-workers",
                Some("ASB_MEASURE_AGENT_WORKERS"),
                None,
                true,
            ),
            ("cpu-accounting", None, None, true),
            ("system", None, None, false),
            ("mpstat", Some("ASB_MEASURE_MPSTAT"), Some("mpstat"), true),
            ("iostat", Some("ASB_MEASURE_IOSTAT"), Some("iostat"), true),
            ("perf", Some("ASB_MEASURE_PERF"), Some("perf"), true),
            (
                "perfetto",
                Some("ASB_MEASURE_PERFETTO"),
                Some("perfetto"),
                true,
            ),
            ("flamegraph", None, Some("flamegraph"), true),
            (
                "bpftrace",
                Some("ASB_MEASURE_BPFTRACE"),
                Some("bpftrace"),
                true,
            ),
            ("tool-calls", None, None, false),
        ];
        assert_eq!(all().len(), expected.len());
        for (adapter, (id, environment, tool, pid)) in all().iter().zip(expected) {
            assert_eq!(adapter.id(), id);
            assert_eq!(adapter.environment_flag(), environment, "{id}");
            assert_eq!(adapter.tool_id(), tool, "{id}");
            assert_eq!(adapter.requires_pid(), pid, "{id}");
            assert_eq!(
                adapter.stop_at_execution_end(),
                pid && id != "perfetto",
                "{id}"
            );
            let dependencies: &[&str] = if id == "flamegraph" { &["perf"] } else { &[] };
            assert_eq!(adapter.dependencies(), dependencies, "{id}");
            for process in ["perf", "perf-stat", "perfetto", "bpftrace-sched", "other"] {
                let graceful = matches!(id, "perfetto" | "bpftrace")
                    || (id == "perf" && matches!(process, "perf" | "perf-stat"));
                assert_eq!(
                    adapter.producer_policy(process).graceful.is_some(),
                    graceful,
                    "{id}/{process}"
                );
            }
        }
    }

    #[test]
    fn minimal_adapter_uses_safe_defaults_through_dynamic_dispatch() {
        struct Minimal;
        impl CollectorAdapter for Minimal {
            fn id(&self) -> &'static str {
                "minimal"
            }
        }
        let adapter: &dyn CollectorAdapter = &Minimal;
        assert_eq!(adapter.environment_flag(), None);
        assert_eq!(adapter.tool_id(), None);
        assert!(adapter.dependencies().is_empty());
        assert!(!adapter.requires_pid());
        assert!(!adapter.stop_at_execution_end());
        assert!(adapter.producer_policy("process").graceful.is_none());
        assert!(!adapter.prioritizes_artifact("artifact.svg"));
        assert!(adapter.display_artifact("artifact.svg").is_none());
        assert!(adapter.graph_section("metric").is_none());
    }
    #[test]
    fn registry_has_unique_names_and_declares_tool_backed_collectors() {
        let ids = names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), all().len());
        for collector in all() {
            assert_ne!(collector.description(), "collector", "{}", collector.id());
            assert!(!collector.description().contains('\n'));
        }
        assert!(crate::measurements::collector_description("unknown").is_none());
        for id in [
            "mpstat",
            "iostat",
            "perf",
            "perfetto",
            "flamegraph",
            "bpftrace",
        ] {
            assert!(
                find(id).and_then(|collector| collector.tool_id()).is_some(),
                "{id}"
            );
        }
        assert!(find("perf").unwrap().requires_pid());
    }

    #[test]
    fn adapter_owned_subselections_resolve_without_generic_id_dispatch() {
        assert_eq!(
            adapter_for_selection("bpftrace:sched-latency").map(CollectorAdapter::id),
            Some("bpftrace")
        );
        assert!(adapter_for_selection("bpftrace:not-a-profile").is_none());
        assert!(selected_or_environment(
            "bpftrace",
            &["bpftrace:sched-latency".into()]
        ));
    }

    #[test]
    fn collector_extension_options_are_unique_and_resolve_to_their_owner() {
        let mut all_selections = std::collections::BTreeSet::new();
        for adapter in all() {
            let Some(extension) = adapter.selection_extension() else {
                continue;
            };
            assert!(!extension.options.is_empty(), "{}", adapter.id());
            if let Some(all) = extension.all_selection {
                assert_eq!(
                    adapter_for_selection(&all).map(CollectorAdapter::id),
                    Some(adapter.id())
                );
            }
            for option in extension.options {
                assert!(all_selections.insert(option.selection.clone()));
                assert_eq!(
                    adapter_for_selection(&option.selection).map(CollectorAdapter::id),
                    Some(adapter.id())
                );
                assert!(!option.label.is_empty() && !option.description.is_empty());
            }
        }
    }

    #[test]
    fn generic_runtime_does_not_dispatch_on_collector_ids() {
        fn direct_dispatch(source: &str, name: &str) -> bool {
            let code: String = source
                .split("#[cfg(test)]")
                .next()
                .unwrap_or(source)
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            let literal = format!("\"{name}\"");
            [
                "collector",
                "collector.name",
                "collector.id()",
                "collector_id",
            ]
            .iter()
            .any(|value| {
                code.contains(&format!("{value}=={literal}"))
                    || code.contains(&format!("{literal}=={value}"))
            }) || [
                "matchcollector{",
                "matchcollector.name.as_str(){",
                "matchcollector_id{",
            ]
            .iter()
            .filter_map(|value| code.split_once(value).map(|(_, tail)| tail))
            .any(|tail| {
                tail.split('}')
                    .next()
                    .unwrap_or("")
                    .contains(&format!("{literal}=>"))
            }) || code.contains(&format!(".starts_with(\"metric-{name}-\")"))
                || code.contains(&format!(".starts_with(\"series-{name}-\")"))
        }
        assert!(direct_dispatch(
            "if collector == \"perfetto\" {}",
            "perfetto"
        ));
        let source_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for filename in [
            "measurements.rs",
            "runner.rs",
            "install.rs",
            "tui.rs",
            "compare.rs",
        ] {
            let source = std::fs::read_to_string(source_dir.join(filename)).unwrap();
            for adapter in all() {
                assert!(
                    !direct_dispatch(&source, adapter.id()),
                    "collector-specific dispatch for {} in {filename}",
                    adapter.id()
                );
            }
        }
    }
}
