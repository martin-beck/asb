//! Explicit installer intent, shared deadlines and non-swallowable custody errors.
use crate::project::{InstallationConfig, ProjectConfig};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod native;
#[cfg(not(target_os = "linux"))]
#[path = "process/legacy.rs"]
mod native;
#[cfg(target_os = "linux")]
mod observer;

pub(crate) type InstallResult<T> = Result<T, InstallFailure>;

/// Only the outer command boundary may render/erase this private failure type.
#[derive(Debug)]
pub(crate) enum InstallFailure {
    Ordinary(crate::Error),
    Abort(AbortInstallation),
}
impl From<crate::Error> for InstallFailure {
    fn from(error: crate::Error) -> Self {
        Self::Ordinary(error)
    }
}
impl InstallFailure {
    pub(in crate::install) fn with_prior_ordinary(mut self, prior: Option<&crate::Error>) -> Self {
        if let (Self::Abort(abort), Some(prior)) = (&mut self, prior) {
            let preceding = format!("preceding installation failure: {prior}");
            abort.detail = Some(match abort.detail.take() {
                Some(detail) => format!("{detail}; {preceding}"),
                None => preceding,
            });
        }
        self
    }

    pub(crate) fn into_public(self) -> crate::Error {
        match self {
            Self::Ordinary(error) => error,
            Self::Abort(abort) => crate::Error::Io(abort.to_string()),
        }
    }
}
impl std::fmt::Display for InstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ordinary(error) => error.fmt(f),
            Self::Abort(error) => error.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Probe,
    Version,
    Download,
    Extract,
    Configure,
    Package,
    Build,
}
impl Purpose {
    fn budget(self, config: &InstallationConfig) -> (&'static str, u64) {
        match self {
            Self::Probe => ("probe_timeout_seconds", config.probe_timeout_seconds),
            Self::Version => ("version_timeout_seconds", config.version_timeout_seconds),
            Self::Download => ("download_timeout_seconds", config.download_timeout_seconds),
            Self::Extract => ("extract_timeout_seconds", config.extract_timeout_seconds),
            Self::Configure => (
                "configure_timeout_seconds",
                config.configure_timeout_seconds,
            ),
            Self::Package => ("package_timeout_seconds", config.package_timeout_seconds),
            Self::Build => ("build_timeout_seconds", config.build_timeout_seconds),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
struct WorkerStartFault {
    purpose: Purpose,
    observed: std::sync::mpsc::SyncSender<(PathBuf, Arc<AtomicBool>)>,
}

#[derive(Clone)]
pub(crate) struct OperationContext {
    root: PathBuf,
    config_path: PathBuf,
    budgets: InstallationConfig,
    proxy: crate::project::NetworkProxy,
    project: ProjectConfig,
    diagnostics: Arc<std::sync::Mutex<Diagnostics>>,
    #[cfg(target_os = "linux")]
    log_budget: Arc<native::LogBudget>,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    destination_owners: Vec<(PathBuf, Arc<crate::output_lock::OutputLock>)>,
    #[cfg(all(test, target_os = "linux"))]
    worker_start_fault: Arc<std::sync::Mutex<Option<WorkerStartFault>>>,
}
impl OperationContext {
    /// Explicit outer entry only; nested install steps reuse this snapshot.
    pub(crate) fn load(root: &Path) -> InstallResult<Self> {
        let (root, config) = crate::project::load(root)?;
        Self::new(&root, &config)
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn with_destination(&self, destination: &Path) -> Result<Self, crate::Error> {
        let key = super::destination_key(destination)?;
        if self.destination_owners.iter().any(|(held, _)| held == &key) {
            return Ok(self.clone());
        }
        let owner = super::destination_lock(destination)?;
        let mut context = self.clone();
        context.destination_owners.push((key, Arc::new(owner)));
        Ok(context)
    }
    pub(crate) fn custody_failure(&self, label: &str, evidence: &Path) -> InstallFailure {
        authority_failure(self, label, Some(evidence.to_owned()))
    }
    pub(crate) fn check_before_admission(&self, label: &str, evidence: &Path) -> InstallResult<()> {
        let cause = self.stopped_reason();
        if let Some(cause) = cause {
            return Err(InstallFailure::Abort(AbortInstallation {
                cause,
                cleanup_confirmed: true,
                capture_complete: true,
                label: label.into(),
                config_path: self.config_path.clone(),
                limit_key: "operation_timeout_seconds",
                evidence: Some(evidence.to_owned()),
                detail: None,
            }));
        }
        Ok(())
    }
    fn stopped_reason(&self) -> Option<AbortCause> {
        if Instant::now() >= self.deadline {
            Some(AbortCause::Deadline)
        } else if self.cancel.load(Ordering::Acquire) {
            Some(AbortCause::Cancelled)
        } else {
            None
        }
    }
    pub(crate) fn new(root: &Path, config: &ProjectConfig) -> InstallResult<Self> {
        let config_path = root.join(".asb/project.toml");
        config.installation.validate().map_err(|error| {
            crate::Error::Input(format!("invalid {}: {error}", config_path.display()))
        })?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(
                config.installation.operation_timeout_seconds,
            ))
            .ok_or_else(|| {
                crate::Error::Input(format!(
                    "invalid {}: installation.operation_timeout_seconds cannot be represented",
                    config_path.display()
                ))
            })?;
        Ok(Self {
            root: root.to_owned(),
            config_path,
            budgets: config.installation.clone(),
            proxy: config.proxy.clone(),
            project: config.clone(),
            diagnostics: Arc::new(std::sync::Mutex::new(Diagnostics::default())),
            #[cfg(target_os = "linux")]
            log_budget: Arc::new(native::LogBudget::default()),
            deadline,
            cancel: Arc::new(AtomicBool::new(false)),
            destination_owners: Vec::new(),
            #[cfg(all(test, target_os = "linux"))]
            worker_start_fault: Arc::new(std::sync::Mutex::new(None)),
        })
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::install) fn fail_next_worker_for_test(
        &self,
        purpose: Purpose,
    ) -> std::sync::mpsc::Receiver<(PathBuf, Arc<AtomicBool>)> {
        let (observed, receive) = std::sync::mpsc::sync_channel(1);
        let mut slot = self.worker_start_fault.lock().unwrap();
        assert!(slot.is_none(), "one scoped worker-start fault at a time");
        *slot = Some(WorkerStartFault { purpose, observed });
        receive
    }
    #[cfg(test)]
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn project(&self) -> &ProjectConfig {
        &self.project
    }
    fn record_log(&self, report: &native::LogReport) {
        if report.fault.is_none() && !report.truncated && report.settled {
            return;
        }
        let mut diagnostics = self
            .diagnostics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if diagnostics.logs.len() < 128 {
            diagnostics.logs.push(report.clone());
        } else {
            diagnostics.omitted = diagnostics.omitted.saturating_add(1);
        }
    }
    pub(crate) fn report_diagnostics(
        &self,
        output: &mut dyn std::io::Write,
    ) -> Result<(), crate::Error> {
        let mut diagnostics = self
            .diagnostics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let logs = std::mem::take(&mut diagnostics.logs);
        let omitted = std::mem::take(&mut diagnostics.omitted);
        drop(diagnostics);
        if logs.is_empty() && omitted == 0 {
            return Ok(());
        }
        writeln!(output, "Warning: installation diagnostic log snapshots are partial, failed, or pending; the installation outcome is reported separately.")
            .map_err(crate::io_error)?;
        for log in logs {
            render_log(&log, output).map_err(InstallFailure::into_public)?;
        }
        if omitted != 0 {
            writeln!(
                output,
                "  {omitted} additional diagnostic log snapshots omitted from this bounded list."
            )
            .map_err(crate::io_error)?;
        }
        Ok(())
    }
    fn step_deadline(&self, purpose: Purpose) -> InstallResult<(Instant, &'static str)> {
        let (key, seconds) = purpose.budget(&self.budgets);
        let requested = Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .ok_or_else(|| {
                crate::Error::Input(format!(
                    "invalid {}: installation.{key} cannot be represented",
                    self.config_path.display()
                ))
            })?;
        Ok(if requested < self.deadline {
            (requested, key)
        } else {
            (self.deadline, "operation_timeout_seconds")
        })
    }
}

#[derive(Default)]
struct Diagnostics {
    logs: Vec<native::LogReport>,
    omitted: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogPhase {
    Namespace,
    Open,
    WorkerStart,
    Queue,
    Write,
    Flush,
    Budget,
}
impl std::fmt::Display for LogPhase {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::Namespace => "private log namespace preparation",
            Self::Open => "private log file creation",
            Self::WorkerStart => "log writer start",
            Self::Queue => "bounded log queue",
            Self::Write => "log write",
            Self::Flush => "log flush",
            Self::Budget => "diagnostic log budget",
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogFault {
    phase: LogPhase,
    kind: std::io::ErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbortCause {
    Cancelled,
    Deadline,
    CaptureRead,
    CaptureOverflow,
    Custody,
    Worker,
    OwnerObservation,
}

#[derive(Clone, Debug)]
pub(crate) struct AbortInstallation {
    pub(crate) cause: AbortCause,
    pub(crate) cleanup_confirmed: bool,
    pub(crate) capture_complete: bool,
    label: String,
    config_path: PathBuf,
    limit_key: &'static str,
    pub(crate) evidence: Option<PathBuf>,
    detail: Option<String>,
}
impl AbortInstallation {
    fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }
}
impl std::fmt::Display for AbortInstallation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "installation step {} aborted ({:?}); cleanup {}; required capture {}; configuration {} [installation].{}",
            self.label,
            self.cause,
            if self.cleanup_confirmed {
                "confirmed"
            } else {
                "unconfirmed; owned worker retains producer resources"
            },
            if self.capture_complete {
                "complete"
            } else {
                "incomplete"
            },
            self.config_path.display(),
            self.limit_key
        )?;
        if let Some(path) = &self.evidence {
            write!(f, "; retained evidence {}", path.display())?;
        }
        if let Some(detail) = &self.detail {
            write!(f, "; {detail}")?;
        }
        Ok(())
    }
}

/// Actual producer dependencies, held by the worker until matching quiescence.
/// The owner must move its real staging guard here, not a copy of its pathname.
pub(crate) trait ProducerOwner: Send + Sync {
    /// The immutable state belonging to this actual owner. A lease cannot
    /// substitute fresh state while retaining the same producer resources.
    fn producer_state(&self) -> &ProducerState;
    /// Disable destructive settlement before any potentially admitted producer.
    fn producer_started(&self);
    /// Owning resource policy, never executed by the cancellation supervisor.
    fn observation(self: Arc<Self>) -> Option<ObservationCheck> {
        None
    }
    #[cfg(test)]
    fn drop_completion(&self) -> Option<Arc<AtomicBool>> {
        None
    }
}
pub(crate) type ObservationCheck = Box<dyn FnMut() -> Result<(), String> + Send>;

/// Real opened directory ownership for package/build helpers. There is no
/// destructive Drop: an uncertain producer always leaves its evidence intact.
pub(crate) struct ProducerDirectory {
    path: PathBuf,
    _directory: std::fs::File,
    state: ProducerState,
}
impl ProducerOwner for ProducerDirectory {
    fn producer_state(&self) -> &ProducerState {
        &self.state
    }
    fn producer_started(&self) {}
}
impl ProducerDirectory {
    pub(crate) fn open(path: &Path) -> InstallResult<Arc<Self>> {
        let directory = crate::install::source::open_named(path).map_err(crate::io_error)?;
        if !directory.metadata().map_err(crate::io_error)?.is_dir() {
            return Err(crate::Error::Input(
                "installation producer root is not a directory".into(),
            )
            .into());
        }
        Ok(Arc::new(Self {
            path: path.to_owned(),
            _directory: directory,
            state: ProducerState::default(),
        }))
    }
    pub(crate) fn scope(self: &Arc<Self>, context: &OperationContext) -> ProcessScope {
        ProcessScope::new(context, ProducerLease::new(Arc::clone(self), &self.path))
    }
    pub(crate) fn consume(&self, permission: ProducerPermission) -> InstallResult<()> {
        permission.consume_for(&self.state)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProducerProof {
    NoNewAdmission,
    Quiescent,
    #[cfg(not(target_os = "linux"))]
    DirectChildOnly,
}

struct OwnerState {
    generation: u64,
    active: bool,
    proof: Option<ProducerProof>,
    consumed: bool,
    poisoned: bool,
    operation: Option<Arc<AtomicBool>>,
}
/// Identity plus private admission/settlement facts, never just pointer identity.
#[derive(Clone)]
pub(crate) struct ProducerState(Arc<std::sync::Mutex<OwnerState>>);
impl Default for ProducerState {
    fn default() -> Self {
        Self(Arc::new(std::sync::Mutex::new(OwnerState {
            generation: 0,
            active: false,
            proof: Some(ProducerProof::NoNewAdmission),
            consumed: false,
            poisoned: false,
            operation: None,
        })))
    }
}
impl ProducerState {
    pub(crate) fn invalidate_settlement(&self) {
        // Sticky: later actual quiescence may refine facts, but it cannot mint
        // a new cleanup/publication capability for an already retained owner.
        self.lock().poisoned = true;
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, OwnerState> {
        match self.0.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisoned = true;
                state
            }
        }
    }
    fn permission(
        &self,
        context: &OperationContext,
        evidence: Option<PathBuf>,
    ) -> Option<ProducerPermission> {
        let mut state = self.lock();
        if state.active || state.poisoned || state.consumed {
            return None;
        }
        if let Some(operation) = &state.operation {
            if !Arc::ptr_eq(operation, &context.cancel) {
                return None;
            }
        } else {
            state.operation = Some(Arc::clone(&context.cancel));
        }
        Some(ProducerPermission {
            owner: self.clone(),
            generation: state.generation,
            proof: state.proof?,
            context: context.clone(),
            evidence,
        })
    }
}

/// Non-cloneable one-use permission; identity, generation, operation and actual
/// settlement facts are all revalidated before ANY destructive owner action.
pub(crate) struct ProducerPermission {
    owner: ProducerState,
    generation: u64,
    proof: ProducerProof,
    context: OperationContext,
    evidence: Option<PathBuf>,
}
impl ProducerPermission {
    fn current(&self) -> bool {
        let state = self.owner.lock();
        self.context.stopped_reason().is_none()
            && state.generation == self.generation
            && !state.active
            && !state.poisoned
            && !state.consumed
            && state.proof == Some(self.proof)
            && state
                .operation
                .as_ref()
                .is_some_and(|operation| Arc::ptr_eq(operation, &self.context.cancel))
    }
    pub(crate) fn consume_for(self, owner: &ProducerState) -> InstallResult<()> {
        self.consume_guard_for(owner).map(drop)
    }
    pub(crate) fn consume_guard_for(
        self,
        owner: &ProducerState,
    ) -> InstallResult<ConsumedPermission> {
        let mut state = owner.lock();
        let valid = Arc::ptr_eq(&self.owner.0, &owner.0)
            && state.generation == self.generation
            && !state.active
            && !state.poisoned
            && !state.consumed
            && state.proof == Some(self.proof)
            && state
                .operation
                .as_ref()
                .is_some_and(|operation| Arc::ptr_eq(operation, &self.context.cancel));
        if !valid {
            return Err(authority_failure(
                &self.context,
                "stale or different producer permission",
                self.evidence,
            ));
        }
        if let Some(cause) = self.context.stopped_reason() {
            let failure = ended_permission(&self.context, cause, &state, self.evidence);
            state.poisoned = true;
            return Err(failure);
        }
        state.consumed = true;
        drop(state);
        Ok(ConsumedPermission(self))
    }
}

/// A consumed permission is not a receipt. The actual publication admission
/// rechecks the intrinsic owner's generation and the original operation.
pub(crate) struct ConsumedPermission(ProducerPermission);
impl ConsumedPermission {
    pub(crate) fn check(&self) -> InstallResult<()> {
        self.check_for(&self.0.owner)
    }
    pub(crate) fn check_for(&self, owner: &ProducerState) -> InstallResult<()> {
        let permission = &self.0;
        let state = owner.lock();
        if !Arc::ptr_eq(&permission.owner.0, &owner.0)
            || state.generation != permission.generation
            || state.active
            || state.poisoned
            || !state.consumed
            || state.proof != Some(permission.proof)
            || !state
                .operation
                .as_ref()
                .is_some_and(|operation| Arc::ptr_eq(operation, &permission.context.cancel))
        {
            return Err(authority_failure(
                &permission.context,
                "consumed producer permission changed before publication",
                permission.evidence.clone(),
            ));
        }
        if let Some(cause) = permission.context.stopped_reason() {
            return Err(ended_permission(
                &permission.context,
                cause,
                &state,
                permission.evidence.clone(),
            ));
        }
        Ok(())
    }
}

fn ended_permission(
    context: &OperationContext,
    cause: AbortCause,
    state: &OwnerState,
    evidence: Option<PathBuf>,
) -> InstallFailure {
    let quiet = !state.active && state.proof.is_some();
    InstallFailure::Abort(AbortInstallation { cause, cleanup_confirmed: quiet,
        capture_complete: quiet && !state.poisoned, label: "installation publication or settlement".into(),
        config_path: context.config_path.clone(), limit_key: "operation_timeout_seconds", evidence,
        detail: Some("the operation is no longer live; publication and quiet scratch settlement were refused".into()) })
}

fn authority_failure(
    context: &OperationContext,
    label: &str,
    evidence: Option<PathBuf>,
) -> InstallFailure {
    InstallFailure::Abort(AbortInstallation {
        cause: AbortCause::Custody,
        cleanup_confirmed: false,
        capture_complete: false,
        label: label.into(),
        config_path: context.config_path.clone(),
        limit_key: "operation_timeout_seconds",
        evidence,
        detail: None,
    })
}

#[derive(Clone, Default)]
pub(crate) struct ProducerLease {
    _owner: Option<Arc<dyn ProducerOwner>>,
    state: ProducerState,
    evidence: Option<PathBuf>,
}
impl ProducerLease {
    pub(crate) fn new<T: ProducerOwner + 'static>(owner: Arc<T>, evidence: &Path) -> Self {
        let state = owner.producer_state().clone();
        Self {
            _owner: Some(owner),
            state,
            evidence: Some(evidence.to_owned()),
        }
    }
    fn begin(&self, context: &OperationContext) -> InstallResult<AdmissionTicket> {
        // The actual owner disables destructive Drop before target exposure,
        // including every pre-admission failure and unwind path.
        if let Some(owner) = &self._owner {
            owner.producer_started();
        }
        let mut state = self.state.lock();
        let same_operation = state
            .operation
            .as_ref()
            .is_none_or(|operation| Arc::ptr_eq(operation, &context.cancel));
        let next = state.generation.checked_add(1);
        if state.active || state.poisoned || state.consumed || !same_operation || next.is_none() {
            state.poisoned = true;
            return Err(authority_failure(
                context,
                "producer lane is active, settled, or belongs to another operation",
                self.evidence.clone(),
            ));
        }
        state.operation = Some(Arc::clone(&context.cancel));
        state.generation = next.unwrap();
        state.active = true;
        state.proof = None; // Invalidate every previously issued permission.
        Ok(AdmissionTicket {
            state: self.state.clone(),
            generation: state.generation,
        })
    }
}

/// Private worker/parent bookkeeping, not a public settlement permission.
#[derive(Clone)]
struct AdmissionTicket {
    state: ProducerState,
    generation: u64,
}
impl AdmissionTicket {
    fn settle(&self, proof: ProducerProof) {
        let mut state = self.state.lock();
        if state.generation != self.generation || !state.active {
            state.poisoned = true;
            return;
        }
        state.active = false;
        state.proof = Some(proof);
    }
    fn poison(&self) {
        let mut state = self.state.lock();
        state.poisoned = true;
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Capture {
    Status,
    Inspect {
        stream_bytes: usize,
        total_bytes: usize,
    },
}
impl Capture {
    pub(crate) fn inspection() -> Self {
        Self::Inspect {
            stream_bytes: 1024 * 1024,
            total_bytes: 2 * 1024 * 1024,
        }
    }
}

pub(crate) struct SettledStep {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    pub(crate) log: native::LogReport,
    lease: ProducerLease,
}
#[cfg(test)]
impl SettledStep {
    pub(crate) fn status(&self) -> std::process::ExitStatus {
        self.status
    }
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }
    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// Captures/status have no publication authority; only ProcessScope owns it.
pub(crate) struct CompletedStep {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) log: native::LogReport,
}
/// Ancillary diagnostics never change required capture/status into success.
fn render_log(log: &native::LogReport, output: &mut dyn std::io::Write) -> InstallResult<()> {
    if log.fault.is_some() || log.truncated || !log.settled {
        writeln!(
            output,
            "  Log {}: {} bytes retained, {} bytes accepted, {} bytes discarded; writer {}{}.",
            log.path
                .as_ref()
                .map_or_else(|| "unavailable".into(), |path| path.display().to_string()),
            log.retained,
            log.accepted,
            log.discarded,
            if log.settled {
                "settled"
            } else {
                "pending (completion unconfirmed)"
            },
            log.fault.map_or_else(String::new, |fault| format!(
                "; {} failed ({:?})",
                fault.phase, fault.kind
            ))
        )
        .map_err(crate::io_error)?;
    }
    Ok(())
}

/// A single sequential producer lane for one actual private stage. Neither this
/// scope nor its permission can be cloned or extracted by validation callbacks.
pub(crate) struct ProcessScope {
    context: OperationContext,
    lease: ProducerLease,
    permission: Option<ProducerPermission>,
    terminal: Option<AbortInstallation>,
}
impl ProcessScope {
    pub(crate) fn new(context: &OperationContext, lease: ProducerLease) -> Self {
        // This initial token means no producer was admitted by this new scope,
        // not a manufactured process exit or guardian receipt.
        let permission = lease.state.permission(context, lease.evidence.clone());
        Self {
            context: context.clone(),
            lease,
            permission,
            terminal: None,
        }
    }
    pub(crate) fn prepare(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        purpose: Purpose,
        label: &str,
    ) -> InstallResult<PreparedStep> {
        let step = PreparedStep::new(&self.context, program, purpose, label)?;
        #[cfg(all(test, target_os = "linux"))]
        let mut step = step;
        #[cfg(all(test, target_os = "linux"))]
        {
            let fault = {
                let mut slot = self.context.worker_start_fault.lock().unwrap();
                if slot.as_ref().is_some_and(|fault| fault.purpose == purpose) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(fault) = fault {
                step.fail_worker_start = Some(std::io::ErrorKind::Other);
                let _ = fault.observed.send((
                    self.lease.evidence.clone().expect("fixture evidence owner"),
                    self.fixture_drop_completion()
                        .expect("fixture owner completion receipt"),
                ));
            }
        }
        Ok(step)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::install) fn fixture_drop_completion(&self) -> Option<Arc<AtomicBool>> {
        self.lease._owner.as_ref()?.drop_completion()
    }
    pub(crate) fn status(
        &mut self,
        program: impl AsRef<std::ffi::OsStr>,
        purpose: Purpose,
        label: &str,
        configure: impl FnOnce(&mut std::process::Command),
    ) -> InstallResult<std::process::ExitStatus> {
        let mut step = self.prepare(program, purpose, label)?;
        step.command_mut().stdin(std::process::Stdio::null());
        configure(step.command_mut());
        Ok(self.run(step)?.status)
    }
    pub(crate) fn run(&mut self, step: PreparedStep) -> InstallResult<CompletedStep> {
        if let Some(abort) = &self.terminal {
            return Err(InstallFailure::Abort(abort.clone()));
        }
        self.permission.take(); // Invalidate before EVERY attempt.
        if !Arc::ptr_eq(&self.context.cancel, &step.context.cancel) {
            let abort = AbortInstallation {
                cause: AbortCause::Custody,
                cleanup_confirmed: true,
                capture_complete: false,
                label: "producer operation mismatch".into(),
                config_path: self.context.config_path.clone(),
                limit_key: "operation_timeout_seconds",
                evidence: self.lease.evidence.clone(),
                detail: None,
            };
            self.terminal = Some(abort.clone());
            return Err(InstallFailure::Abort(abort));
        }
        match step.run(self.lease.clone()) {
            Ok(step) => {
                if !Arc::ptr_eq(&step.lease.state.0, &self.lease.state.0) {
                    let abort = AbortInstallation {
                        cause: AbortCause::Custody,
                        cleanup_confirmed: false,
                        capture_complete: false,
                        label: "producer owner mismatch".into(),
                        config_path: self.context.config_path.clone(),
                        limit_key: "operation_timeout_seconds",
                        evidence: self.lease.evidence.clone(),
                        detail: None,
                    };
                    self.terminal = Some(abort.clone());
                    return Err(InstallFailure::Abort(abort));
                }
                self.permission = step
                    .lease
                    .state
                    .permission(&self.context, self.lease.evidence.clone());
                if self.permission.is_none() {
                    let failure = authority_failure(
                        &self.context,
                        "producer proof is missing or invalidated",
                        self.lease.evidence.clone(),
                    );
                    if let InstallFailure::Abort(abort) = &failure {
                        self.terminal = Some(abort.clone());
                    }
                    return Err(failure);
                }
                Ok(CompletedStep {
                    status: step.status,
                    stdout: step.stdout,
                    stderr: step.stderr,
                    log: step.log,
                })
            }
            // The native boundary emits Ordinary only before admission. Earlier
            // settled facts remain valid when this attempt admitted no producer.
            Err(InstallFailure::Ordinary(error)) => {
                self.permission = self
                    .lease
                    .state
                    .permission(&self.context, self.lease.evidence.clone());
                Err(InstallFailure::Ordinary(error))
            }
            Err(InstallFailure::Abort(abort)) => {
                self.terminal = Some(abort.clone());
                Err(InstallFailure::Abort(abort))
            }
        }
    }
    pub(crate) fn finish(mut self) -> InstallResult<ProducerPermission> {
        if let Some(abort) = self.terminal.take() {
            return Err(InstallFailure::Abort(abort));
        }
        if let Some(cause) = self.context.stopped_reason() {
            let state = self.lease.state.lock();
            return Err(ended_permission(
                &self.context,
                cause,
                &state,
                self.lease.evidence.clone(),
            ));
        }
        match self.permission.take() {
            Some(permission) if permission.current() => Ok(permission),
            _ => Err(authority_failure(
                &self.context,
                "producer settlement is unavailable or stale",
                self.lease.evidence.clone(),
            )),
        }
    }
}

/// Materialize first; configure this original Command, then consume the route.
pub(crate) struct PreparedStep {
    command: std::process::Command,
    route: Option<crate::launch::CommandRoute>,
    context: OperationContext,
    purpose: Purpose,
    label: String,
    capture: Capture,
    /// Some configuration children legitimately emit credentials. Their exit
    /// status is enough for the installer contract, so never retain output.
    retain_diagnostics: bool,
    #[cfg(all(test, target_os = "linux"))]
    log_writer: Option<native::WriterFactory>,
    #[cfg(all(test, target_os = "linux"))]
    hold_observation: Option<Arc<AtomicBool>>,
    #[cfg(all(test, target_os = "linux"))]
    before_continue: Option<native::AdmissionPause>,
    #[cfg(all(test, target_os = "linux"))]
    observation: Option<ObservationCheck>,
    #[cfg(all(test, target_os = "linux"))]
    reader_factory: Option<native::ReaderFactory>,
    #[cfg(all(test, target_os = "linux"))]
    fail_worker_start: Option<std::io::ErrorKind>,
}
impl PreparedStep {
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::install) fn hold_observation_for_test(&mut self, held: Arc<AtomicBool>) {
        self.hold_observation = Some(held);
    }

    pub(crate) fn new(
        context: &OperationContext,
        program: impl AsRef<std::ffi::OsStr>,
        purpose: Purpose,
        label: &str,
    ) -> InstallResult<Self> {
        let launch = crate::launch::ProcessLaunch::new(program);
        Self::from_launch(context, &launch, purpose, label)
    }
    pub(crate) fn from_launch(
        context: &OperationContext,
        launch: &crate::launch::ProcessLaunch,
        purpose: Purpose,
        label: &str,
    ) -> InstallResult<Self> {
        #[cfg(target_os = "linux")]
        let (mut command, route) = launch.native(Some(&runtime().map_err(crate::io_error)?));
        #[cfg(not(target_os = "linux"))]
        let (mut command, route) = launch.native(None);
        context
            .proxy
            .apply(&mut command, &|name| std::env::var(name).ok());
        command.stdin(std::process::Stdio::null());
        Ok(Self {
            command,
            route: Some(route),
            context: context.clone(),
            purpose,
            label: label.to_owned(),
            capture: Capture::Status,
            retain_diagnostics: true,
            #[cfg(all(test, target_os = "linux"))]
            log_writer: None,
            #[cfg(all(test, target_os = "linux"))]
            hold_observation: None,
            #[cfg(all(test, target_os = "linux"))]
            before_continue: None,
            #[cfg(all(test, target_os = "linux"))]
            observation: None,
            #[cfg(all(test, target_os = "linux"))]
            reader_factory: None,
            #[cfg(all(test, target_os = "linux"))]
            fail_worker_start: None,
        })
    }
    pub(crate) fn command_mut(&mut self) -> &mut std::process::Command {
        &mut self.command
    }
    pub(crate) fn capture(&mut self, capture: Capture) {
        self.capture = capture;
    }
    /// Keep a sensitive child observable through its exit status while
    /// preventing stdout/stderr from entering private diagnostic evidence.
    pub(crate) fn suppress_diagnostics(&mut self) {
        self.retain_diagnostics = false;
    }
    pub(crate) fn run(self, lease: ProducerLease) -> InstallResult<SettledStep> {
        let ticket = lease.begin(&self.context)?;
        native::run(self, lease, ticket)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cli_tests;
#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(target_os = "linux")]
fn runtime() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    return Ok(std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| std::io::Error::other("missing actual ASB binary directory"))?
        .join("asb"));
    #[cfg(not(test))]
    crate::process_gate::executable()
}
