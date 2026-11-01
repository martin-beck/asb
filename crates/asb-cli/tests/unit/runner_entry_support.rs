//! Private per-thread observation of a real runner attempt; no factory bypass.
use super::*;
use std::cell::RefCell;
use std::rc::Rc;

struct Hook {
    root: PathBuf,
    event: Option<LifecycleEvent>,
    calls: Rc<RefCell<Vec<&'static str>>>,
    provider_finish: Option<bool>,
    native_reward: bool,
    pending: bool,
    grader_error: bool,
    cleanup_error: bool,
    capture_publication: bool,
    admission_replacement: bool,
    capture_grade: Option<bool>,
    observe_original_provider: bool,
    phase_fault: Option<PhaseFault>,
}
#[derive(Clone)]
enum PhaseFault {
    StoppedPreserved,
    RemoveCwd(PathBuf),
    ObserveLifetime,
    RuntimeError,
}
thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static GRADE_DIAGNOSTIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Fixture-only snapshot of the settled execution, before the evaluator runs.
pub(crate) fn capture_grade_diagnostic(
    root: &Path,
    evidence: &super::ExecutionEvidence,
    issues: &super::AttemptIssues,
    proxy: &crate::proxy::Evidence,
) {
    let selected = HOOK.with_borrow(|slot| {
        slot.as_ref()
            .is_some_and(|hook| hook.root == root && hook.capture_grade.is_some())
    });
    if !selected {
        return;
    }
    fn markers(bytes: &[u8]) -> Vec<&'static str> {
        let bytes = &bytes[..bytes.len().min(4096)];
        [
            ("agent-end", b"agent_end".as_slice()),
            ("fixture-replay-failed", b"fixture replay request failed"),
            ("proxy-completion-failed", b"proxy completion failed"),
            ("fetch-failed", b"fetch failed"),
            ("connection-refused", b"ECONNREFUSED"),
            ("timeout", b"timed out"),
            ("gate", b"gate"),
        ]
        .into_iter()
        .filter_map(|(label, pattern)| {
            bytes
                .windows(pattern.len())
                .any(|part| part == pattern)
                .then_some(label)
        })
        .collect()
    }
    let run_error = issues.run_error.as_deref().unwrap_or("");
    let capture_error = issues.capture_error.as_deref().unwrap_or("");
    let diagnostic = format!(
        "process_status_code={:?}; termination={:?}; capture_complete={}; stdout_bytes={}; stdout_markers={:?}; stderr_bytes={}; stderr_markers={:?}; run_error_bytes={}; run_error_markers={:?}; capture_error_bytes={}; capture_error_markers={:?}; proxy_cleanup_confirmed={}; proxy_capture_count={}",
        evidence.output.status.code(),
        evidence.observation,
        evidence.capture_complete,
        evidence.output.stdout.len(),
        markers(&evidence.output.stdout),
        evidence.output.stderr.len(),
        markers(&evidence.output.stderr),
        run_error.len(),
        markers(run_error.as_bytes()),
        capture_error.len(),
        markers(capture_error.as_bytes()),
        proxy.cleanup_confirmed,
        proxy.captures.len()
    );
    GRADE_DIAGNOSTIC.with_borrow_mut(|slot| *slot = Some(diagnostic));
}

pub(super) fn read_fixture_grade(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| {
        let detail = GRADE_DIAGNOSTIC
            .with_borrow(|slot| slot.clone())
            .unwrap_or_else(|| "settled execution diagnostics unavailable".into());
        panic!(
            "fixture grade file is unavailable: {:?}; {detail}",
            error.kind()
        );
    })
}

pub(crate) struct Scope;

pub(crate) fn assert_workload_owner_live(root: &Path) {
    HOOK.with_borrow(|slot| {
        let hook = slot.as_ref().filter(|hook| hook.root == root).unwrap();
        assert!(
            !hook.calls.borrow().contains(&"workload-drop"),
            "workload owner must survive publication and completion observer"
        );
    });
}

pub(crate) fn module_descriptor(
    name: &str,
    mut descriptor: Option<workloads::contracts::ModuleDescriptor>,
) -> Option<workloads::contracts::ModuleDescriptor> {
    HOOK.with_borrow(|slot| {
        if name == "repository-navigation"
            && slot.as_ref().is_some_and(|hook| hook.native_reward)
            && let Some(module) = descriptor.as_mut()
        {
            // This process-local fixture's declared protocol feeds normal
            // planning, fingerprints and runtime before any evidence is saved.
            module.protocol.score_policy = "metric:accuracy@1".into();
        }
    });
    descriptor
}

impl Scope {
    pub fn artifact(
        root: &Path,
        calls: Rc<RefCell<Vec<&'static str>>>,
        failed_measurement: bool,
    ) -> Self {
        let scope = Self::provider(root, calls, false, false);
        if failed_measurement {
            HOOK.with(|slot| {
                slot.borrow_mut().as_mut().unwrap().event = Some(LifecycleEvent::RunFinishing)
            });
        }
        scope
    }
    pub fn new(root: &Path, event: LifecycleEvent, calls: Rc<RefCell<Vec<&'static str>>>) -> Self {
        HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Hook {
                root: root.canonicalize().unwrap(),
                event: Some(event),
                calls,
                provider_finish: None,
                native_reward: false,
                pending: false,
                grader_error: false,
                cleanup_error: false,
                capture_publication: false,
                admission_replacement: false,
                capture_grade: None,
                observe_original_provider: false,
                phase_fault: None,
            });
        });
        Self
    }

    pub fn provider(
        root: &Path,
        calls: Rc<RefCell<Vec<&'static str>>>,
        fail: bool,
        native_reward: bool,
    ) -> Self {
        HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Hook {
                root: root.canonicalize().unwrap(),
                event: None,
                calls,
                provider_finish: Some(fail),
                native_reward,
                pending: false,
                grader_error: false,
                cleanup_error: false,
                capture_publication: false,
                admission_replacement: false,
                capture_grade: None,
                observe_original_provider: false,
                phase_fault: None,
            });
        });
        Self
    }

    pub fn disposition(root: &Path, calls: Rc<RefCell<Vec<&'static str>>>, case: &str) -> Self {
        HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Hook {
                root: root.canonicalize().unwrap(),
                event: case
                    .contains("collector")
                    .then_some(LifecycleEvent::AgentExecuteEnd),
                calls,
                provider_finish: Some(case.contains("provider")),
                native_reward: case.contains("native"),
                pending: case.contains("pending"),
                grader_error: case.contains("grader"),
                cleanup_error: case.contains("cleanup"),
                capture_publication: case.contains("publication-") && case.contains("capture"),
                admission_replacement: case.contains("admission"),
                capture_grade: case
                    .starts_with("capture-")
                    .then_some(!case.contains("false"))
                    .or_else(|| {
                        (case.starts_with("termination-") && !case.contains("pending"))
                            .then_some(!case.contains("false"))
                    }),
                observe_original_provider: false,
                phase_fault: None,
            });
        });
        Self
    }
    pub fn recording(root: &Path, calls: Rc<RefCell<Vec<&'static str>>>, native: bool) -> Self {
        let scope = Self::provider(root, calls, false, native);
        HOOK.with_borrow_mut(|slot| slot.as_mut().unwrap().observe_original_provider = true);
        scope
    }
    pub fn phase(root: &Path, calls: Rc<RefCell<Vec<&'static str>>>, case: &str) -> Self {
        let scope = Self::provider(
            root,
            calls,
            case.contains("provider"),
            case.contains("native"),
        );
        HOOK.with_borrow_mut(|slot| {
            let hook = slot.as_mut().unwrap();
            hook.event = if case.contains("prepare-workload-end") {
                Some(LifecycleEvent::PhaseEnd(
                    LifecyclePhase::WorkloadPreparation,
                ))
            } else {
                case.contains("measurements")
                    .then_some(LifecycleEvent::RunFinishing)
            };
            hook.phase_fault = Some(if case.contains("owner-lifetime") {
                PhaseFault::ObserveLifetime
            } else if case.contains("runtime-error") {
                PhaseFault::RuntimeError
            } else if case.ends_with("removed-cwd") {
                PhaseFault::RemoveCwd(root.join("empty-cwd"))
            } else {
                PhaseFault::StoppedPreserved
            });
        });
        scope
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
        GRADE_DIAGNOSTIC.with_borrow_mut(|slot| *slot = None);
    }
}
pub(crate) fn configure_measurements(root: &Path, session: Option<&mut measurements::Session>) {
    HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref().filter(|hook| hook.root == root)
            && let Some(session) = session
        {
            session.test_event_failure = hook.event;
            if hook.capture_publication {
                crate::measurements::publication_tests::stopped_fixture(
                    session,
                    if hook.admission_replacement {
                        "admission"
                    } else {
                        "capture-fixture"
                    },
                );
                crate::measurements::publication_tests::stopped_fixture(session, "capture-sibling");
                if hook.admission_replacement {
                    std::fs::rename(
                        session.dir.join("admission.stdout"),
                        session.dir.join("admission.saved"),
                    )
                    .unwrap();
                    std::fs::write(session.dir.join("admission.stdout"), "A=99\n").unwrap();
                }
            }
        }
    });
}

/// A private replay-only session double exercises generic finish obligations.
pub(crate) fn observe_provider(
    root: &Path,
    original: Option<Box<dyn providers::ProviderSession>>,
) -> Option<Box<dyn providers::ProviderSession>> {
    let shared = SHARED_PROVIDERS.lock().unwrap().get(root).cloned();
    if let Some(finish) = shared {
        assert!(
            original.is_none(),
            "shared provider fixture requires replay"
        );
        return Some(Box::new(SharedProvider { finish }));
    }
    HOOK.with(|slot| {
        let slot = slot.borrow();
        let Some(hook) = slot
            .as_ref()
            .filter(|hook| hook.root == root && hook.provider_finish.is_some())
        else {
            return original;
        };
        assert!(original.is_none() || hook.observe_original_provider,
            "provider fixture requires replay unless explicitly observing the synthetic recording provider");
        Some(Box::new(Provider {
            original,
            calls: hook.calls.clone(),
            fail: hook.provider_finish.unwrap(),
            finished: false,
        }))
    })
}
type SharedFinish = dyn Fn() -> Result<(), Error> + Send + Sync;
static SHARED_PROVIDERS: std::sync::Mutex<
    std::collections::BTreeMap<PathBuf, std::sync::Arc<SharedFinish>>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Root-scoped fixture shared with actual concurrent runner threads. No runtime
/// environment switches or production provider registry changes are involved.
pub(crate) struct SharedProviderScope(PathBuf);
impl SharedProviderScope {
    pub fn new(root: &Path, finish: std::sync::Arc<SharedFinish>) -> Self {
        let root = root.canonicalize().unwrap();
        assert!(
            SHARED_PROVIDERS
                .lock()
                .unwrap()
                .insert(root.clone(), finish)
                .is_none()
        );
        Self(root)
    }
}
impl Drop for SharedProviderScope {
    fn drop(&mut self) {
        SHARED_PROVIDERS.lock().unwrap().remove(&self.0);
    }
}
struct SharedProvider {
    finish: std::sync::Arc<SharedFinish>,
}
impl providers::ProviderSession for SharedProvider {
    fn api_key(&self) -> &str {
        "fixture-key"
    }
    fn base_url(&self) -> &str {
        "http://127.0.0.1:1/v1"
    }
    fn finish(&mut self) -> Result<(), Error> {
        (self.finish)()
    }
}
struct Provider {
    original: Option<Box<dyn providers::ProviderSession>>,
    calls: Rc<RefCell<Vec<&'static str>>>,
    fail: bool,
    finished: bool,
}
impl providers::ProviderSession for Provider {
    fn api_key(&self) -> &str {
        self.original
            .as_ref()
            .map_or("fixture-key", |inner| inner.api_key())
    }
    fn base_url(&self) -> &str {
        self.original
            .as_ref()
            .map_or("http://127.0.0.1:1/v1", |inner| inner.base_url())
    }
    fn finish(&mut self) -> Result<(), Error> {
        assert!(!self.finished, "finish called twice");
        self.finished = true;
        self.calls.borrow_mut().push("provider-finish");
        if let Some(inner) = self.original.as_mut() {
            inner.finish()?;
        }
        if self.fail {
            Err(Error::Run("fixture provider cleanup uncertain".into()))
        } else {
            Ok(())
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        if !self.finished {
            self.calls.borrow_mut().push("provider-fallback");
        }
    }
}
pub(crate) fn observe_workload(
    root: &Path,
    workload: Box<dyn workloads::WorkloadSession>,
) -> Box<dyn workloads::WorkloadSession> {
    let calls = HOOK.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|hook| hook.root == root)
            .map(|hook| {
                (
                    hook.calls.clone(),
                    hook.native_reward,
                    hook.pending,
                    hook.grader_error,
                    hook.cleanup_error,
                    hook.capture_grade,
                    hook.phase_fault.clone(),
                )
            })
    });
    match calls {
        Some((
            calls,
            native_reward,
            pending,
            grader_error,
            cleanup_error,
            capture_grade,
            phase_fault,
        )) => Box::new(Observed {
            inner: workload,
            calls,
            native_reward,
            pending,
            grader_error,
            cleanup_error,
            capture_grade,
            phase_fault,
        }),
        None => workload,
    }
}
struct Observed {
    inner: Box<dyn workloads::WorkloadSession>,
    calls: Rc<RefCell<Vec<&'static str>>>,
    native_reward: bool,
    pending: bool,
    grader_error: bool,
    cleanup_error: bool,
    capture_grade: Option<bool>,
    phase_fault: Option<PhaseFault>,
}
impl workloads::WorkloadSession for Observed {
    fn cancel_prepared(&mut self) -> Result<(), String> {
        self.calls.borrow_mut().push("cancel-prepared");
        self.inner.cancel_prepared()
    }
    fn initial_material_sha256(&self) -> Result<String, String> {
        self.inner.initial_material_sha256()
    }
    fn invocation(&self) -> &workloads::session::TaskInvocation {
        self.inner.invocation()
    }
    fn effective_budgets(&self) -> crate::planning::EffectiveBudgets {
        self.inner.effective_budgets()
    }
    fn evaluation_kind(&self) -> workloads::KnownEvaluationKind {
        self.inner.evaluation_kind()
    }
    fn baseline_passed(&self) -> Option<bool> {
        self.inner.baseline_passed()
    }
    fn baseline_evidence(&self) -> workloads::BaselineEvidence {
        self.inner.baseline_evidence()
    }
    fn evaluate(&mut self, response: &str) -> Result<workloads::Evaluation, String> {
        self.calls.borrow_mut().push("evaluate");
        if let Some(expected) = self.capture_grade
            && !self.native_reward
        {
            let grade = read_fixture_grade(&self.invocation().workspace.join("fixture-file-grade"));
            assert_eq!(grade, if expected { "true" } else { "false" });
            return Ok(if expected {
                workloads::Evaluation::Passed
            } else {
                workloads::Evaluation::Failed("independent fixture file grade".into())
            });
        }
        self.inner.evaluate(response)
    }
    fn evaluate_outcome(&mut self, response: &str) -> workloads::outcome::EvaluationOutcome {
        if self.capture_grade.is_some() && !self.native_reward {
            return match self.evaluate(response).unwrap() {
                workloads::Evaluation::Passed => {
                    workloads::outcome::EvaluationOutcome::binary(true)
                }
                workloads::Evaluation::Failed(_) => {
                    workloads::outcome::EvaluationOutcome::binary(false)
                }
            };
        }
        self.calls.borrow_mut().push("evaluate");
        if self.pending {
            return workloads::outcome::EvaluationOutcome::Pending {
                reason: "fixture evaluation queued".into(),
            };
        }
        if self.grader_error {
            return workloads::outcome::EvaluationOutcome::Error {
                reason: "fixture verifier unavailable".into(),
            };
        }
        let evaluation = if self.native_reward {
            std::fs::write(
                self.invocation().workspace.join("native-grade.json"),
                "fixture numeric evaluator evidence",
            )
            .unwrap();
            workloads::outcome::EvaluationOutcome::Completed {
                resolution: None,
                rewards: std::collections::BTreeMap::from([("accuracy".into(), 0.5)]),
                reason: None,
            }
        } else {
            self.inner.evaluate_outcome(response)
        };
        if self.phase_fault.is_some() {
            // Retain the actual evaluator result, not a second grading pass.
            std::fs::write(
                self.invocation().workspace.join("phase-grade.json"),
                serde_json::to_vec(&evaluation).unwrap(),
            )
            .unwrap();
        }
        evaluation
    }
    fn metric_policy(&self) -> workloads::outcome::MetricPolicy {
        if !self.native_reward {
            return self.inner.metric_policy();
        }
        workloads::outcome::MetricPolicy {
            id: "accuracy".into(),
            reward: "accuracy".into(),
            rewards: std::collections::BTreeMap::from([(
                "accuracy".into(),
                workloads::outcome::RewardSpec {
                    unit: "ratio".into(),
                    range: Some([0.0, 1.0]),
                },
            )]),
            ..Default::default()
        }
    }
    fn module_descriptor(&self) -> Option<workloads::contracts::ModuleDescriptor> {
        self.inner.module_descriptor().map(|mut module| {
            if self.native_reward {
                module.protocol.score_policy = "metric:accuracy@1".into();
            }
            module
        })
    }
    fn finish(&mut self) -> Result<workloads::session::ResourceSettlement, String> {
        self.calls.borrow_mut().push("finish");
        let result = self.inner.finish();
        if let Some(fault) = &self.phase_fault {
            let mut settled = result?;
            match fault {
                PhaseFault::StoppedPreserved => {
                    settled.disposition = workloads::ResourceDisposition::StoppedPreserved;
                }
                PhaseFault::RemoveCwd(path) => {
                    assert_eq!(std::env::current_dir().unwrap(), *path);
                    // The subprocess owns this empty directory. Measurements
                    // live in its sibling; no recursive deletion or host cwd.
                    std::fs::remove_dir(path).unwrap();
                }
                PhaseFault::ObserveLifetime | PhaseFault::RuntimeError => {}
            }
            return Ok(settled);
        }
        if self.cleanup_error {
            result?;
            Err("fixture workload cleanup uncertain".into())
        } else {
            result
        }
    }
    fn checkpoint_evidence(&self) -> Vec<PathBuf> {
        if self.native_reward {
            vec![self.invocation().workspace.join("native-grade.json")]
        } else {
            self.inner.checkpoint_evidence()
        }
    }
    fn execute(
        &mut self,
        context: &mut workloads::ExecutionContext<'_>,
        output: &mut dyn Write,
    ) -> Option<Result<workloads::session::ExecutionResult, String>> {
        if matches!(self.phase_fault, Some(PhaseFault::RuntimeError)) {
            self.calls.borrow_mut().push("execute-entered");
            return Some(Err("fixture started execution failed".into()));
        }
        self.inner.execute(context, output)
    }
}

impl Drop for Observed {
    fn drop(&mut self) {
        if matches!(
            self.phase_fault,
            Some(PhaseFault::ObserveLifetime | PhaseFault::RuntimeError)
        ) {
            self.calls.borrow_mut().push("workload-drop");
        }
    }
}
