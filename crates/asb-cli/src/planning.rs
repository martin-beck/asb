//! Provider-free execution policy resolution. Reservations are admission costs,
//! never replacements for a harness's own environment limits.
use crate::{Error, benchmark::BenchmarkConfig, collectors, workloads};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_NATIVE_EXECUTION_SECONDS: u64 = 900;
pub(crate) const DEFAULT_ENVIRONMENT_PREPARATION_SECONDS: u64 = 900;
pub(crate) const DEFAULT_EVALUATION_SECONDS: u64 = 1800;

/// Optional original policy metadata may come only from agreeing same-attempt
/// evidence. Missing historical objects remain unknown, never today's defaults.
pub(crate) fn recorded_object<'a>(
    primary: &'a serde_json::Value,
    retained: Option<&'a serde_json::Value>,
    field: &str,
) -> Result<Option<&'a serde_json::Value>, Error> {
    let primary = primary.get(field).filter(|value| !value.is_null());
    let retained = retained
        .and_then(|record| record.get(field))
        .filter(|value| !value.is_null());
    if primary.zip(retained).is_some_and(|(a, b)| a != b) {
        return Err(Error::Input(format!(
            "measurement and outcome disagree on recorded {field}"
        )));
    }
    let value = primary.or(retained);
    if value.is_some_and(|value| !value.is_object()) {
        return Err(Error::Input(format!(
            "recorded {field} metadata must be an object"
        )));
    }
    Ok(value)
}

/// Runtime dispatch population, not replay identity or observed overlap.
/// Occurrence IDs and original slot membership deliberately do not enter it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "DispatchWire")]
pub(crate) struct Dispatch {
    schema_version: u8,
    configured: usize,
    admitted: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchWire {
    schema_version: u8,
    configured: usize,
    admitted: usize,
}

impl TryFrom<DispatchWire> for Dispatch {
    type Error = &'static str;
    fn try_from(value: DispatchWire) -> Result<Self, Self::Error> {
        if value.schema_version != 1 {
            return Err("unsupported dispatch schema");
        }
        Self::new(value.configured, value.admitted)
    }
}

impl Dispatch {
    pub(crate) fn new(configured: usize, admitted: usize) -> Result<Self, &'static str> {
        if !(1..=crate::benchmark::MAX_CONCURRENCY).contains(&configured)
            || admitted == 0
            || admitted > configured
        {
            return Err("invalid configured/admitted dispatch population");
        }
        Ok(Self {
            schema_version: 1,
            configured,
            admitted,
        })
    }
    pub(crate) fn configured(self) -> usize {
        self.configured
    }
    pub(crate) fn admitted(self) -> usize {
        self.admitted
    }
}

/// Persisted execution mode shared by sweep and standalone campaign plans.
/// Legacy missing modes are unknown, never implicit permission to run live.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ExecutionMode {
    Live,
    Record,
    Replay,
    #[default]
    Unknown,
}

impl ExecutionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Record => "record",
            Self::Replay => "replay",
            Self::Unknown => "unknown",
        }
    }
}

/// Fixed shared admission policy, not a configurable per-agent phase cap.
/// Its typed constructor also supplies the actual gate deadlines.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct BatchAdmissionPolicy {
    scope: &'static str,
    material_seconds: u64,
    launch_seconds: u64,
    material_arming: &'static str,
    launch_arming: &'static str,
}

impl<'de> Deserialize<'de> for BatchAdmissionPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let observed = serde_json::Value::deserialize(deserializer)?;
        let current = Self::current();
        let expected = serde_json::to_value(current).map_err(serde::de::Error::custom)?;
        if observed != expected {
            return Err(serde::de::Error::custom(
                "saved batch admission policy does not match the supported operational contract",
            ));
        }
        Ok(current)
    }
}

impl BatchAdmissionPolicy {
    pub(crate) const fn current() -> Self {
        Self {
            scope: "verified-material-shared-launch-v1",
            material_seconds: 1800,
            launch_seconds: 1800,
            material_arming: "batch-gate-construction",
            launch_arming: "first-post-material-slot-registration",
        }
    }
    pub(crate) fn material_timeout(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.material_seconds)
    }
    pub(crate) fn launch_timeout(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.launch_seconds)
    }
}

/// Passive historical identity only. This never grants launch permission and
/// deliberately does not require the current operational deadline constants.
#[derive(Deserialize, Serialize)]
#[serde(try_from = "RecordedAdmissionWire")]
struct RecordedAdmission {
    scope: String,
    material_seconds: u64,
    launch_seconds: u64,
    material_arming: String,
    launch_arming: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedAdmissionWire {
    scope: String,
    material_seconds: u64,
    launch_seconds: u64,
    material_arming: String,
    launch_arming: String,
}

impl TryFrom<RecordedAdmissionWire> for RecordedAdmission {
    type Error = &'static str;
    fn try_from(value: RecordedAdmissionWire) -> Result<Self, Self::Error> {
        if [&value.scope, &value.material_arming, &value.launch_arming]
            .into_iter()
            .any(|text| text.is_empty() || text.len() > 128 || text.chars().any(char::is_control))
            || !(1..=86400).contains(&value.material_seconds)
            || !(1..=86400).contains(&value.launch_seconds)
        {
            return Err("invalid historical batch admission identity");
        }
        Ok(Self {
            scope: value.scope,
            material_seconds: value.material_seconds,
            launch_seconds: value.launch_seconds,
            material_arming: value.material_arming,
            launch_arming: value.launch_arming,
        })
    }
}

impl From<BatchAdmissionPolicy> for RecordedAdmission {
    fn from(value: BatchAdmissionPolicy) -> Self {
        Self {
            scope: value.scope.into(),
            material_seconds: value.material_seconds,
            launch_seconds: value.launch_seconds,
            material_arming: value.material_arming.into(),
            launch_arming: value.launch_arming.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Resources {
    pub cpu_millis: u64,
    pub memory_mib: u64,
    pub disk_mib: u64,
}

impl Resources {
    pub(crate) fn fits(self, capacity: Self) -> bool {
        self.cpu_millis <= capacity.cpu_millis
            && self.memory_mib <= capacity.memory_mib
            && self.disk_mib <= capacity.disk_mib
    }
    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu_millis: self.cpu_millis.checked_add(other.cpu_millis)?,
            memory_mib: self.memory_mib.checked_add(other.memory_mib)?,
            disk_mib: self.disk_mib.checked_add(other.disk_mib)?,
        })
    }
    pub(crate) fn subtract(&mut self, other: Self) {
        self.cpu_millis -= other.cpu_millis;
        self.memory_mib -= other.memory_mib;
        self.disk_mib -= other.disk_mib;
    }
    pub(crate) fn replicas(self, count: usize) -> Result<Self, Error> {
        (0..count).try_fold(Self::default(), |total, _| {
            total
                .checked_add(self)
                .ok_or_else(|| Error::Input("resource reservation overflow".into()))
        })
    }
}

/// Absent values preserve adapter/task budgets. Explicit overrides are part of
/// the experiment and are saved, including on unmeasured runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PhaseBudgets {
    pub environment_preparation_seconds: Option<u64>,
    pub agent_preparation_seconds: Option<u64>,
    pub execution_seconds: Option<u64>,
    pub evaluation_seconds: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveBudgets {
    pub environment_preparation_seconds: Option<u64>,
    pub agent_preparation_seconds: Option<u64>,
    pub execution_seconds: Option<f64>,
    pub evaluation_seconds: Option<u64>,
    /// Authoritative ready-invocation default, retained even when the operator
    /// selects a different execution cap.
    pub task_default_execution_seconds: Option<f64>,
}

/// Module-owned hints from a pinned task definition. Missing values remain
/// unknown; these are not observations of backend enforcement.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRequirements {
    pub source: String,
    pub cpu_millis: Option<u64>,
    pub memory_mib: Option<u64>,
    pub disk_mib: Option<u64>,
    pub execution_seconds: Option<f64>,
    pub evaluation_seconds: Option<f64>,
}

impl PhaseBudgets {
    fn validate(self) -> Result<(), Error> {
        if self.agent_preparation_seconds.is_some() {
            return Err(Error::Input("agent preparation has no configurable end-to-end deadline; agent_preparation_seconds is unsupported".into()));
        }
        if [
            self.environment_preparation_seconds,
            self.execution_seconds,
            self.evaluation_seconds,
        ]
        .into_iter()
        .flatten()
        .any(|seconds| !(1..=86400).contains(&seconds))
        {
            return Err(Error::Input(
                "phase budgets must be between 1 and 86400 seconds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PlanningConfig {
    /// Auto chooses only the backend declared by the owning module.
    pub backend: Option<String>,
    /// Required enforced policies. Unsupported policies fail before startup.
    pub required_policies: Vec<String>,
    pub budgets: PhaseBudgets,
    /// Operator-declared per-replica host admission reservation. Unknown unless
    /// explicitly supplied; not a measurement or a task runtime limit.
    pub reservation: Option<Resources>,
    pub repetition: Repetition,
}

/// Identity of a repeated configuration; independent of its concurrent replicas.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repetition {
    pub index: usize,
    pub total: usize,
}
impl Default for Repetition {
    fn default() -> Self {
        Self { index: 1, total: 1 }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolvedPlan {
    pub schema_version: u32,
    pub backend: String,
    pub input_modalities: Vec<String>,
    pub enforced_policies: Vec<String>,
    pub budgets: PhaseBudgets,
    #[serde(default)]
    pub default_budgets: PhaseBudgets,
    pub reservation: Option<Resources>,
    pub collectors: Vec<CollectorPlan>,
    pub repetition: Repetition,
    pub task_requirements: Option<TaskRequirements>,
    pub host_os: String,
    pub host_architecture: String,
    pub same_architecture_guest: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectorPlan {
    pub id: String,
    pub target: collectors::TargetScope,
    pub phase: String,
    /// Capability declaration only; permission probes and actual attachment
    /// still run later. Never presented as an available observation.
    pub requires_verified_local_target: bool,
    /// Automatic report helpers can run without selecting their dependencies.
    pub selected: bool,
}

/// Constructed only from validated planning and runtime caps. Run callers
/// cannot supply a free-form cohort identifier.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct ExecutionPlanFingerprint(String);

impl ExecutionPlanFingerprint {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
pub(crate) fn protocol_fingerprint(
    config: &BenchmarkConfig,
    policy: &ResolvedPlan,
    effective: EffectiveBudgets,
    replica_concurrency: usize,
) -> Result<ExecutionPlanFingerprint, Error> {
    protocol_fingerprint_with_admission(config, policy, effective, replica_concurrency, None)
}

#[cfg(test)]
pub(crate) fn protocol_fingerprint_with_admission(
    config: &BenchmarkConfig,
    policy: &ResolvedPlan,
    effective: EffectiveBudgets,
    replica_concurrency: usize,
    admission: Option<BatchAdmissionPolicy>,
) -> Result<ExecutionPlanFingerprint, Error> {
    protocol_fingerprint_with_dispatch(
        config,
        policy,
        effective,
        replica_concurrency,
        admission,
        None,
    )
}

pub(crate) fn protocol_fingerprint_with_dispatch(
    config: &BenchmarkConfig,
    policy: &ResolvedPlan,
    effective: EffectiveBudgets,
    replica_concurrency: usize,
    admission: Option<BatchAdmissionPolicy>,
    dispatch: Option<Dispatch>,
) -> Result<ExecutionPlanFingerprint, Error> {
    resolve_snapshot(config, !policy.collectors.is_empty(), Some(policy))?;
    let admission = admission.map(RecordedAdmission::from);
    protocol_fingerprint_core(
        workloads::descriptor(&config.workload).as_ref(),
        policy,
        effective,
        replica_concurrency,
        admission.as_ref(),
        dispatch,
    )
}

/// Pure historical verification must not discover Docker, collectors or host
/// capabilities. Their saved policy is checked here, never re-resolved.
fn protocol_fingerprint_core(
    module: Option<&workloads::contracts::ModuleDescriptor>,
    policy: &ResolvedPlan,
    effective: EffectiveBudgets,
    replica_concurrency: usize,
    admission: Option<&RecordedAdmission>,
    dispatch: Option<Dispatch>,
) -> Result<ExecutionPlanFingerprint, Error> {
    use sha2::{Digest, Sha256};
    let task_default = effective.task_default_execution_seconds;
    let selected_execution = policy
        .budgets
        .execution_seconds
        .or(policy.default_budgets.execution_seconds)
        .map(|n| n as f64)
        .or(task_default);
    if !(1..=crate::benchmark::MAX_CONCURRENCY).contains(&replica_concurrency)
        || dispatch.is_some_and(|value| value.configured() != replica_concurrency)
        || effective
            .execution_seconds
            .is_none_or(|n| !n.is_finite() || !(0.001..=86400.0).contains(&n))
        || [
            effective.environment_preparation_seconds,
            effective.evaluation_seconds,
        ]
        .into_iter()
        .flatten()
        .any(|n| !(1..=86400).contains(&n))
        || effective.agent_preparation_seconds.is_some()
        || effective.execution_seconds != selected_execution
        || effective.environment_preparation_seconds
            != policy
                .budgets
                .environment_preparation_seconds
                .or(policy.default_budgets.environment_preparation_seconds)
        || effective.evaluation_seconds
            != policy
                .budgets
                .evaluation_seconds
                .or(policy.default_budgets.evaluation_seconds)
        || (policy.backend == "local-docker"
            && task_default.is_none_or(|n| !n.is_finite() || !(0.001..=86400.0).contains(&n)))
        || (policy.backend == "native" && task_default.is_some())
        || policy
            .task_requirements
            .as_ref()
            .and_then(|requirements| requirements.execution_seconds)
            .is_some_and(|n| task_default != Some(n))
    {
        return Err(Error::Input(
            "effective controller caps do not match validated execution policy".into(),
        ));
    }
    // Occurrence identity belongs in the attempt record. Cohort compatibility
    // retains the declared repetition total, without splitting occurrences.
    let mut cohort = serde_json::to_value(policy).map_err(|error| Error::Io(error.to_string()))?;
    // A pinned suite defines different requirements/defaults for each task.
    // Their realized values are provenance, not a different scoring protocol.
    cohort
        .as_object_mut()
        .expect("serialized policy object")
        .remove("task_requirements");
    cohort["repetition"]
        .as_object_mut()
        .expect("serialized repetition object")
        .remove("index");
    let mut identity = serde_json::json!({
        "schema_version":1,
        "module": module,
        "policy":cohort,
        "execution_default_rule":if policy.backend == "local-docker" { "pinned-module-ready-invocation" } else { "native-controller-default" },
        "replica_concurrency":replica_concurrency,
    });
    if let Some(admission) = admission {
        identity["batch_admission"] =
            serde_json::to_value(admission).map_err(|error| Error::Io(error.to_string()))?;
    }
    // Missing historical metadata retains its exact prior hash. New dispatch
    // evidence separates partial admission from a full configured population.
    if let Some(dispatch) = dispatch {
        identity["dispatch"] =
            serde_json::to_value(dispatch).map_err(|error| Error::Io(error.to_string()))?;
    }
    let bytes = serde_json::to_vec(&identity).map_err(|error| Error::Io(error.to_string()))?;
    Ok(ExecutionPlanFingerprint(format!(
        "asb-protocol-v1:{:x}",
        Sha256::digest(bytes)
    )))
}

/// Validate new dispatch-bearing records through the same runtime identity
/// projection. Missing historical dispatch stays unknown and is not rehashed.
pub(crate) fn validate_recorded_dispatch(
    record: &serde_json::Value,
    cohort: &crate::measurements::ExecutionCohort,
    index: usize,
    total: usize,
) -> Result<(), Error> {
    let invalid =
        || Error::Input("recorded dispatch does not match its runtime protocol fingerprint".into());
    if !cohort.admits(index, total) {
        return Err(invalid());
    }
    let Some(dispatch) = cohort.dispatch else {
        return Ok(());
    };
    let module: Option<workloads::contracts::ModuleDescriptor> =
        serde_json::from_value(record["module"].clone()).map_err(|_| invalid())?;
    if module.as_ref().is_some_and(|module| {
        !module.valid() || record["workload"].as_str() != Some(module.id.as_str())
    }) {
        return Err(invalid());
    }
    let policy: ResolvedPlan =
        serde_json::from_value(record["planning"].clone()).map_err(|_| invalid())?;
    let effective: EffectiveBudgets =
        serde_json::from_value(record["effective_budgets"].clone()).map_err(|_| invalid())?;
    let admission = if record["execution"].is_null() {
        None
    } else {
        Some(
            serde_json::from_value(record["execution"]["admission"].clone())
                .map_err(|_| invalid())?,
        )
    };
    let expected = protocol_fingerprint_core(
        module.as_ref(),
        &policy,
        effective,
        total,
        admission.as_ref(),
        Some(dispatch),
    )?;
    if record["execution_plan_fingerprint"].as_str() != Some(expected.as_str()) {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn resolve(config: &BenchmarkConfig) -> Result<ResolvedPlan, Error> {
    resolve_for_run(config, false)
}

pub(crate) fn resolve_snapshot(
    config: &BenchmarkConfig,
    measured: bool,
    saved: Option<&ResolvedPlan>,
) -> Result<ResolvedPlan, Error> {
    let mut current = resolve_for_run(config, measured)?;
    if let Some(saved) = saved {
        // The sweep's cached catalog already bound these module-owned facts.
        // Recheck operational choices before any per-attempt side effects.
        current.task_requirements = saved.task_requirements.clone();
        if &current != saved {
            return Err(Error::Input(
                "execution capabilities or collector selection changed after planning".into(),
            ));
        }
    }
    Ok(current)
}

/// Create-new, private policy evidence; never put instructions or payloads in
/// this record. The first record precedes providers/collectors/environments.
pub(crate) fn persist(
    root: &std::path::Path,
    policy: &ResolvedPlan,
    effective: Option<EffectiveBudgets>,
    task: Option<&workloads::TaskIdentity>,
) -> Result<std::path::PathBuf, Error> {
    persist_with_dispatch(root, policy, effective, task, None)
}

pub(crate) fn persist_with_dispatch(
    root: &std::path::Path,
    policy: &ResolvedPlan,
    effective: Option<EffectiveBudgets>,
    task: Option<&workloads::TaskIdentity>,
    dispatch: Option<Dispatch>,
) -> Result<std::path::PathBuf, Error> {
    use std::io::Write;
    let state = root.join(".asb");
    let directory = state.join("execution-plans");
    for path in [&state, &directory] {
        if !path.exists() {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(error) = builder.create(path)
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(crate::io_error(error));
            }
        }
        let metadata = std::fs::symlink_metadata(path).map_err(crate::io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Input(
                "execution plan directory must be a real project directory".into(),
            ));
        }
    }
    let id = workloads::TaskIdentity::local("execution-plan", None).attempt_id;
    let path = directory.join(format!("{id}.json"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(crate::io_error)?;
    let mut record = serde_json::json!({"schema_version":1,"policy":policy,"effective_controller_caps":effective,"task":task});
    if let Some(dispatch) = dispatch {
        record["dispatch"] =
            serde_json::to_value(dispatch).map_err(|error| Error::Io(error.to_string()))?;
    }
    let bytes = serde_json::to_vec(&record).map_err(|error| Error::Io(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(crate::io_error)?;
    Ok(path)
}

/// Deliberately stricter than Docker's context/host precedence: even an explicit
/// local override does not excuse an unvalidated persisted remote context. The
/// CLI and guest SDK must both remain on the built-in local default context.
/// This is metadata-only and must not require a daemon or Docker executable.
fn validate_local_docker() -> Result<(), Error> {
    if std::env::var_os("DOCKER_HOST").is_some_and(|value| {
        value
            .to_str()
            .is_none_or(|host| !host.starts_with("unix:///") || host.len() <= 8)
    }) || std::env::var_os("DOCKER_CONTEXT")
        .is_some_and(|value| value != "default" && !value.is_empty())
    {
        return Err(Error::Input(
            "remote Docker endpoints/contexts are unsupported; select a local Unix daemon endpoint"
                .into(),
        ));
    }
    let directory = std::env::var_os("DOCKER_CONFIG")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".docker"))
        })
        .ok_or_else(|| {
            Error::Input("cannot resolve local Docker configuration directory".into())
        })?;
    validate_docker_config(&directory.join("config.json"))
}

fn validate_docker_config(path: &std::path::Path) -> Result<(), Error> {
    use std::io::Read;
    const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
    let invalid = || {
        Error::Input("cannot validate local Docker config.json; require a readable bounded regular JSON file with the default currentContext".into())
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never block opening a FIFO, follow a file symlink, or spawn a process.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(invalid()),
    };
    let metadata = file.metadata().map_err(|_| invalid())?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES
        || bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{')
    {
        return Err(invalid());
    }
    #[derive(Deserialize)]
    struct DockerConfig {
        #[serde(default, rename = "currentContext")]
        current_context: String,
    }
    // Typed deserialization also rejects duplicate currentContext fields; never
    // copy config content or credentials into an error or persisted policy.
    let config: DockerConfig = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !config.current_context.is_empty() && config.current_context != "default" {
        return Err(Error::Input(
            "remote Docker endpoints/contexts are unsupported; persisted currentContext must be default".into(),
        ));
    }
    Ok(())
}

pub(crate) fn resolve_for_run(
    config: &BenchmarkConfig,
    measurements_requested: bool,
) -> Result<ResolvedPlan, Error> {
    let module = workloads::descriptor(&config.workload);
    let backend = match module.as_ref().map(|m| &m.protocol.environment) {
        Some(workloads::contracts::EnvironmentKind::LocalDocker) => "local-docker",
        _ => "native",
    };
    let modalities = module.map_or_else(|| vec!["text".into()], |m| m.protocol.input_modalities);
    if backend == "local-docker"
        && !cfg!(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))
    {
        return Err(Error::Input("local Docker guest execution requires a supported same-architecture Linux host (x86_64 or aarch64)".into()));
    }
    if backend == "local-docker" {
        validate_local_docker()?;
    }
    // Current invocation and agent interfaces carry text only. A module adding
    // another modality must first supply a qualified input transport.
    let agent_modalities =
        crate::agents::find(&config.agent).map_or(&["text"][..], |agent| agent.input_modalities());
    if modalities
        .iter()
        .any(|modality| !agent_modalities.contains(&modality.as_str()))
    {
        return Err(Error::Input(
            "selected task requires unsupported input modalities".into(),
        ));
    }
    let policy = config.planning.clone().unwrap_or_default();
    policy.budgets.validate()?;
    if policy.repetition.index == 0
        || policy.repetition.index > policy.repetition.total
        || policy.repetition.total > crate::sweep::MAX_CONFIGS
    {
        return Err(Error::Input(
            "repetition index must be within 1..=total, with total at most 65536".into(),
        ));
    }
    if policy
        .backend
        .as_deref()
        .is_some_and(|requested| requested != backend)
    {
        return Err(Error::Input(format!(
            "selected module requires {backend}; requested backend is unsupported"
        )));
    }
    let measured =
        measurements_requested || config.measurements.as_ref().is_some_and(|m| m.enabled);
    let mut enforced: Vec<String> = if backend == "local-docker" {
        vec!["owned-environment".into()]
    } else {
        vec!["owned-process".into()]
    };
    if measured && backend == "local-docker" {
        enforced.push("verified-local-target".into());
    }
    if policy
        .required_policies
        .iter()
        .any(|p| !enforced.contains(p))
    {
        return Err(Error::Input(
            "requested execution policy cannot be enforced by the selected backend".into(),
        ));
    }
    if backend == "native"
        && (policy.budgets.environment_preparation_seconds.is_some()
            || policy.budgets.evaluation_seconds.is_some())
    {
        return Err(Error::Input("native preparation/evaluation do not support interruptible phase budgets; only execution_seconds is supported".into()));
    }
    let selected = collectors::expand_dependencies(
        config
            .measurements
            .as_ref()
            .map_or(&[][..], |m| m.collectors.as_slice()),
    );
    let mut plans = Vec::new();
    if measured {
        for id in &selected {
            if collectors::adapter_for_selection(id).is_none() {
                return Err(Error::Input(format!(
                    "unknown measurement collector `{id}`"
                )));
            }
        }
        for collector in collectors::all() {
            if collector.environment_flag().is_none()
                || collectors::selected_or_environment(collector.id(), &selected)
            {
                let target = collector.target_scope(backend);
                if !collector.supports_backend(backend) {
                    return Err(Error::Input(format!(
                        "collector {} does not support backend {backend}",
                        collector.id()
                    )));
                }
                plans.push(CollectorPlan {
                    id: collector.id().into(),
                    target,
                    phase: if collector.stop_at_execution_end() {
                        "execution"
                    } else {
                        "run"
                    }
                    .into(),
                    requires_verified_local_target: collector.requires_pid()
                        && backend == "local-docker",
                    selected: collectors::selected_or_environment(collector.id(), &selected),
                });
            }
        }
    }
    Ok(ResolvedPlan {
        schema_version: 1,
        backend: backend.into(),
        input_modalities: modalities,
        enforced_policies: enforced,
        budgets: policy.budgets,
        default_budgets: if backend == "native" {
            PhaseBudgets {
                execution_seconds: Some(DEFAULT_NATIVE_EXECUTION_SECONDS),
                ..Default::default()
            }
        } else {
            PhaseBudgets {
                environment_preparation_seconds: Some(DEFAULT_ENVIRONMENT_PREPARATION_SECONDS),
                evaluation_seconds: Some(DEFAULT_EVALUATION_SECONDS),
                ..Default::default()
            }
        },
        reservation: policy.reservation,
        collectors: plans,
        repetition: policy.repetition,
        task_requirements: None,
        host_os: std::env::consts::OS.into(),
        host_architecture: std::env::consts::ARCH.into(),
        same_architecture_guest: backend == "local-docker",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_admission_hash_is_passive_and_does_not_authorize_old_launch_policy() {
        let config = config("");
        let policy = resolve(&config).unwrap();
        let caps = EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let module = workloads::descriptor(&config.workload);
        let dispatch = Dispatch::new(4, 2).unwrap();
        let cohort = crate::measurements::ExecutionCohort {
            id: "abc".into(),
            selected_replicas: 4,
            admitted_slots: vec![2, 4],
            dispatch: Some(dispatch),
        };
        let mut historical = serde_json::to_value(BatchAdmissionPolicy::current()).unwrap();
        historical["material_seconds"] = serde_json::json!(2400);
        historical["launch_seconds"] = serde_json::json!(2700);
        assert!(serde_json::from_value::<BatchAdmissionPolicy>(historical.clone()).is_err());
        let passive: RecordedAdmission = serde_json::from_value(historical.clone()).unwrap();
        assert_eq!(serde_json::to_value(&passive).unwrap(), historical);
        let fingerprint = protocol_fingerprint_core(
            module.as_ref(),
            &policy,
            caps,
            4,
            Some(&passive),
            Some(dispatch),
        )
        .unwrap();
        let mut record = serde_json::json!({"workload":config.workload,"module":module,"planning":policy,"effective_budgets":caps,"execution":{"admission":historical},"execution_plan_fingerprint":fingerprint});
        validate_recorded_dispatch(&record, &cohort, 2, 4).unwrap();
        record["execution"]["admission"]["launch_seconds"] = serde_json::json!(2701);
        assert!(validate_recorded_dispatch(&record, &cohort, 2, 4).is_err());
        for (field, value) in [
            ("launch_seconds", serde_json::json!(0)),
            ("material_seconds", serde_json::json!(86401)),
            ("scope", serde_json::json!("x".repeat(129))),
            ("launch_arming", serde_json::json!("bad\ncontrol")),
            ("material_arming", serde_json::json!("")),
        ] {
            let mut bad = historical.clone();
            bad[field] = value;
            assert!(serde_json::from_value::<RecordedAdmission>(bad).is_err());
        }
        let mut extra = historical;
        extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RecordedAdmission>(extra).is_err());
    }

    #[test]
    fn recorded_dispatch_verification_uses_saved_module_not_current_registry() {
        let config = config("");
        let policy = resolve(&config).unwrap();
        let caps = EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let dispatch = Dispatch::new(4, 2).unwrap();
        let cohort = crate::measurements::ExecutionCohort {
            id: "abc".into(),
            selected_replicas: 4,
            admitted_slots: vec![2, 4],
            dispatch: Some(dispatch),
        };
        let mut module = workloads::contracts::ModuleDescriptor::local(
            "retired-fixture",
            workloads::KnownEvaluationKind::BundledFixture,
        );
        module.suite_version = "historical-v1".into();
        assert!(workloads::descriptor(&module.id).is_none());
        let fingerprint =
            protocol_fingerprint_core(Some(&module), &policy, caps, 4, None, Some(dispatch))
                .unwrap();
        let mut record = serde_json::json!({"workload":module.id,"module":module,"planning":policy,"effective_budgets":caps,"execution_plan_fingerprint":fingerprint});
        validate_recorded_dispatch(&record, &cohort, 2, 4).unwrap();
        record["module"]["suite_version"] = serde_json::json!("new-v2");
        assert!(validate_recorded_dispatch(&record, &cohort, 2, 4).is_err());
    }

    #[test]
    fn dispatch_schema_rejects_missing_unknown_wrong_type_and_invalid_populations() {
        let value = serde_json::json!({"schema_version":1,"configured":4,"admitted":2});
        let dispatch: Dispatch = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(dispatch).unwrap(), value);
        for field in ["schema_version", "configured", "admitted"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Dispatch>(missing).is_err());
            for bad in [
                serde_json::Value::Null,
                serde_json::json!(true),
                serde_json::json!("2"),
                serde_json::json!(2.0),
            ] {
                let mut changed = value.clone();
                changed[field] = bad;
                assert!(
                    serde_json::from_value::<Dispatch>(changed).is_err(),
                    "{field}"
                );
            }
        }
        for (field, bad) in [
            ("schema_version", 0),
            ("schema_version", 2),
            ("configured", 0),
            ("configured", 65),
            ("admitted", 0),
            ("admitted", 5),
        ] {
            let mut changed = value.clone();
            changed[field] = serde_json::json!(bad);
            assert!(serde_json::from_value::<Dispatch>(changed).is_err());
        }
        let mut extra = value;
        extra["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<Dispatch>(extra).is_err());
    }

    #[test]
    fn runtime_dispatch_binds_population_not_occurrence_and_preserves_historical_hash() {
        let mut config = config("[planning.repetition]\nindex=1\ntotal=3");
        let caps = EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let policy = resolve(&config).unwrap();
        let admission = Some(BatchAdmissionPolicy::current());
        let hash = |config: &BenchmarkConfig, dispatch| {
            protocol_fingerprint_with_dispatch(
                config,
                &resolve(config).unwrap(),
                caps,
                4,
                admission,
                dispatch,
            )
            .unwrap()
        };
        let historical =
            protocol_fingerprint_with_admission(&config, &policy, caps, 4, admission).unwrap();
        assert_eq!(historical, hash(&config, None));
        // Freeze the pre-dispatch wire projection independently of the new
        // wrapper/core, including omission rather than a new null field.
        let mut old_policy = serde_json::to_value(&policy).unwrap();
        old_policy
            .as_object_mut()
            .unwrap()
            .remove("task_requirements");
        old_policy["repetition"]
            .as_object_mut()
            .unwrap()
            .remove("index");
        let old_projection = serde_json::json!({"schema_version":1,"module":workloads::descriptor("bug-fix"),"policy":old_policy,"execution_default_rule":"native-controller-default","replica_concurrency":4,"batch_admission":admission});
        assert_eq!(
            historical.as_str(),
            format!(
                "asb-protocol-v1:{}",
                crate::cassette::digest(&serde_json::to_vec(&old_projection).unwrap())
            )
        );
        let mut cohort = crate::measurements::ExecutionCohort {
            id: "abc".into(),
            selected_replicas: 4,
            admitted_slots: vec![2, 4],
            dispatch: None,
        };
        let partial = cohort.dispatch_for(2, 4).unwrap();
        let partial_hash = hash(&config, Some(partial));
        assert_ne!(historical, partial_hash);
        assert_ne!(
            partial_hash,
            hash(&config, Some(Dispatch::new(4, 4).unwrap()))
        );
        cohort.id = "def".into();
        cohort.admitted_slots = vec![1, 3];
        config.planning.as_mut().unwrap().repetition.index = 2;
        assert_eq!(
            partial_hash,
            hash(&config, Some(cohort.dispatch_for(1, 4).unwrap()))
        );
        assert!(
            protocol_fingerprint_with_dispatch(
                &config,
                &resolve(&config).unwrap(),
                caps,
                4,
                admission,
                Some(Dispatch::new(2, 2).unwrap())
            )
            .is_err()
        );
        let mut record = serde_json::json!({"workload":"bug-fix","module":workloads::descriptor("bug-fix"),"planning":resolve(&config).unwrap(),"effective_budgets":caps,"execution":{"admission":admission},"execution_plan_fingerprint":partial_hash.as_str()});
        cohort.dispatch = Some(partial);
        validate_recorded_dispatch(&record, &cohort, 1, 4).unwrap();
        record["execution_plan_fingerprint"] = serde_json::json!(historical.as_str());
        assert!(validate_recorded_dispatch(&record, &cohort, 1, 4).is_err());
        cohort.dispatch = None;
        validate_recorded_dispatch(&record, &cohort, 1, 4).unwrap();
        cohort.dispatch = Some(partial);
        for field in [
            "planning",
            "effective_budgets",
            "execution_plan_fingerprint",
        ] {
            let mut missing = record.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(validate_recorded_dispatch(&missing, &cohort, 1, 4).is_err());
        }
    }
    #[test]
    fn saved_admission_round_trips_only_the_exact_operational_contract() {
        let current = BatchAdmissionPolicy::current();
        let expected = serde_json::to_value(current).unwrap();
        assert_eq!(
            serde_json::from_value::<BatchAdmissionPolicy>(expected.clone()).unwrap(),
            current
        );
        for key in expected.as_object().unwrap().keys() {
            let mut missing = expected.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<BatchAdmissionPolicy>(missing).is_err());
            for replacement in [
                serde_json::Value::Null,
                serde_json::json!(false),
                serde_json::json!(0),
                serde_json::json!("changed"),
                serde_json::json!(1800.0),
            ] {
                let mut changed = expected.clone();
                changed[key] = replacement;
                assert!(
                    serde_json::from_value::<BatchAdmissionPolicy>(changed).is_err(),
                    "{key}"
                );
            }
        }
        let mut extra = expected.clone();
        extra["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<BatchAdmissionPolicy>(extra).is_err());
        for non_object in [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!("contract"),
        ] {
            assert!(serde_json::from_value::<BatchAdmissionPolicy>(non_object).is_err());
        }
    }

    fn config(extra: &str) -> BenchmarkConfig {
        toml::from_str(&format!(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\n{extra}"
        ))
        .unwrap()
    }
    #[test]
    fn rejects_unenforceable_policies_and_backends_without_execution() {
        for extra in [
            "[planning]\nbackend='remote'",
            "[planning]\nrequired_policies=['network-denied']",
            "[planning.budgets]\nexecution_seconds=0",
            "[planning.budgets]\nevaluation_seconds=10",
        ] {
            assert!(resolve(&config(extra)).is_err(), "{extra}");
        }
        let plan = resolve(&config("[planning.budgets]\nexecution_seconds=42")).unwrap();
        assert_eq!(plan.backend, "native");
        assert_eq!(plan.budgets.execution_seconds, Some(42));
        assert!(plan.collectors.is_empty());
    }
    #[test]
    fn shared_admission_policy_has_its_own_cohort_and_matches_actual_gate_caps() {
        let mut config = config("[planning.repetition]\nindex=1\ntotal=3");
        let caps = EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let admission = BatchAdmissionPolicy::current();
        assert_eq!(admission.material_timeout().as_secs(), 1800);
        assert_eq!(admission.launch_timeout().as_secs(), 1800);
        let manual = protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 2).unwrap();
        let first = protocol_fingerprint_with_admission(
            &config,
            &resolve(&config).unwrap(),
            caps,
            2,
            Some(admission),
        )
        .unwrap();
        assert_ne!(manual, first);
        config.planning.as_mut().unwrap().repetition.index = 2;
        assert_eq!(
            first,
            protocol_fingerprint_with_admission(
                &config,
                &resolve(&config).unwrap(),
                caps,
                2,
                Some(admission)
            )
            .unwrap()
        );
        config.planning.as_mut().unwrap().repetition.total = 4;
        assert_ne!(
            first,
            protocol_fingerprint_with_admission(
                &config,
                &resolve(&config).unwrap(),
                caps,
                2,
                Some(admission)
            )
            .unwrap()
        );
        config.planning.as_mut().unwrap().repetition.total = 3;
        let changed = BatchAdmissionPolicy {
            launch_seconds: 1801,
            ..admission
        };
        assert_ne!(
            first,
            protocol_fingerprint_with_admission(
                &config,
                &resolve(&config).unwrap(),
                caps,
                2,
                Some(changed)
            )
            .unwrap()
        );
    }

    #[test]
    fn protocol_cohort_omits_occurrence_but_binds_repeat_total_caps_and_replica_concurrency() {
        let mut config = config("[planning.repetition]\nindex=1\ntotal=3");
        let caps = EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let first = protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 2).unwrap();
        config.planning.as_mut().unwrap().repetition.index = 2;
        let second = protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 2).unwrap();
        assert_eq!(
            first, second,
            "repetition occurrence is not a protocol difference"
        );
        assert!(first.as_str().starts_with("asb-protocol-v1:"));
        assert_ne!(
            first,
            protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 3).unwrap()
        );
        let changed = EffectiveBudgets {
            execution_seconds: Some(901.0),
            ..caps
        };
        assert!(protocol_fingerprint(&config, &resolve(&config).unwrap(), changed, 2).is_err());
        config.planning.as_mut().unwrap().budgets.execution_seconds = Some(901);
        assert_ne!(
            first,
            protocol_fingerprint(&config, &resolve(&config).unwrap(), changed, 2).unwrap()
        );
        config.planning.as_mut().unwrap().budgets.execution_seconds = None;
        config.planning.as_mut().unwrap().repetition.total = 4;
        assert_ne!(
            first,
            protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 2).unwrap()
        );
        assert!(
            protocol_fingerprint(
                &config,
                &resolve(&config).unwrap(),
                EffectiveBudgets {
                    execution_seconds: Some(f64::NAN),
                    ..caps
                },
                2
            )
            .is_err()
        );
        config.planning.as_mut().unwrap().budgets.execution_seconds = Some(1000);
        assert!(protocol_fingerprint(&config, &resolve(&config).unwrap(), caps, 2).is_err());
    }
    #[test]
    fn snapshot_rejects_changed_collectors_and_repeat_identity_is_independent_of_concurrency() {
        let mut config = config(
            "concurrency=3\n[planning.repetition]\nindex=2\ntotal=5\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['agent-memory']",
        );
        let saved = resolve(&config).unwrap();
        assert_eq!(saved.repetition, Repetition { index: 2, total: 5 });
        assert_eq!(config.concurrency, 3);
        assert!(
            saved
                .collectors
                .iter()
                .find(|collector| collector.id == "flamegraph")
                .is_some_and(|collector| !collector.selected)
        );
        assert!(resolve_snapshot(&config, false, Some(&saved)).is_ok());
        config
            .measurements
            .as_mut()
            .unwrap()
            .collectors
            .push("mpstat".into());
        assert!(resolve_snapshot(&config, false, Some(&saved)).is_err());
        assert!(
            Resources {
                cpu_millis: u64::MAX,
                ..Default::default()
            }
            .replicas(2)
            .is_err()
        );
    }

    #[test]
    fn policy_records_are_private_create_new_and_preserve_fractional_effective_timeouts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/policy-record-fixtures")
            .join(workloads::TaskIdentity::local("policy", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let policy = resolve(&config("")).unwrap();
        let caps = EffectiveBudgets {
            execution_seconds: Some(0.5),
            ..Default::default()
        };
        let first = persist(&root, &policy, None, None).unwrap();
        let original = std::fs::read(&first).unwrap();
        let second = persist(&root, &policy, Some(caps), None).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), original);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&second).unwrap()).unwrap();
        assert_eq!(value["effective_controller_caps"]["execution_seconds"], 0.5);
        assert!(value["effective_controller_caps"]["agent_preparation_seconds"].is_null());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&second).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn docker_scope_requires_verified_targets_and_does_not_claim_remote_support() {
        let mut config = config(
            "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['agent-memory']",
        );
        config.workload = "terminal-bench-2.1".into();
        let plan = resolve(&config).unwrap();
        assert_eq!(plan.backend, "local-docker");
        let sampler = plan
            .collectors
            .iter()
            .find(|p| p.id == "agent-memory")
            .unwrap();
        assert!(sampler.requires_verified_local_target);
        assert_eq!(sampler.target, collectors::TargetScope::AgentProcess);
        assert_eq!(
            serde_json::from_value::<ResolvedPlan>(serde_json::to_value(&plan).unwrap()).unwrap(),
            plan
        );
        config
            .measurements
            .as_mut()
            .unwrap()
            .collectors
            .push("invented".into());
        assert!(resolve(&config).is_err());
    }

    #[test]
    fn adapter_owned_bpftrace_profile_is_admitted_as_its_collector() {
        let config = config(
            "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['bpftrace', 'bpftrace:sched-latency']",
        );
        let plan = resolve(&config).expect("valid bpftrace profile should resolve");
        assert_eq!(
            plan.collectors
                .iter()
                .filter(|collector| collector.id == "bpftrace")
                .count(),
            1
        );
        assert!(
            plan.collectors
                .iter()
                .any(|collector| collector.id == "bpftrace" && collector.selected)
        );
    }
}
