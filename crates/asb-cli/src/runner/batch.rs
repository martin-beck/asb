//! Cancellable preparation rendezvous; never moves adapter sessions between threads.
use super::replica::Identity;
use crate::{Error, cassette::Compatibility};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) fn dataset_fingerprint(
    dataset: Option<&crate::workloads::session::DatasetIdentity>,
) -> Result<Option<String>, Error> {
    dataset
        .map(|dataset| serde_json::to_vec(dataset).map(|bytes| crate::cassette::digest(&bytes)))
        .transpose()
        .map_err(|error| Error::Run(error.to_string()))
}

pub(crate) fn replay_protocol_fingerprint(
    module: Option<&crate::workloads::contracts::ModuleDescriptor>,
    policy: &crate::workloads::outcome::MetricPolicy,
) -> Result<String, Error> {
    serde_json::to_vec(&serde_json::json!({
        "contract":"asb-bundled-replay-v1",
        "agent_input":crate::agents::REPLAY_INPUT_CONTRACT,
        "tape_matching":crate::tape::REPLAY_MATCHING_CONTRACT,
        "material_scope":crate::workloads::session::REPLAY_MATERIAL_CONTRACT,
        "module":module,"metric_policy":policy
    }))
    .map(|bytes| crate::cassette::digest(&bytes))
    .map_err(|error| Error::Run(error.to_string()))
}

pub(crate) struct Gate {
    state: Mutex<State>,
    changed: Condvar,
    timeout: Duration,
    observer: Option<Arc<PreparationObserver>>,
}

type PreparationObserver = dyn Fn(&Compatibility) -> Result<(), Error> + Send + Sync;

/// One actual dispatch owns both rendezvous and its immutable membership.
/// This runtime object is not deserializable and is not durable resume authority;
/// the workflow owner must publish/validate its generation before constructing it.
pub(super) struct Admission {
    claimed: std::sync::atomic::AtomicBool,
    material: Gate,
    launch: Gate,
    cohort: crate::measurements::ExecutionCohort,
}

impl Admission {
    pub(super) fn new(
        total: usize,
        participants: Vec<usize>,
        expected: Option<Compatibility>,
        policy: crate::planning::BatchAdmissionPolicy,
    ) -> Result<Self, Error> {
        Self::from_cohort(Self::fresh_cohort(total, participants)?, expected, policy)
    }

    /// Retain the workflow's frozen cohort identity. This validates runtime
    /// shape only; the owning workflow must first verify durable admission.
    pub(super) fn from_cohort(
        cohort: crate::measurements::ExecutionCohort,
        expected: Option<Compatibility>,
        policy: crate::planning::BatchAdmissionPolicy,
    ) -> Result<Self, Error> {
        Self::with_deadlines(
            cohort,
            expected,
            policy.material_timeout(),
            policy.launch_timeout(),
        )
    }

    #[cfg(test)]
    pub(super) fn for_test(
        total: usize,
        participants: Vec<usize>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        Self::with_deadlines(
            Self::fresh_cohort(total, participants)?,
            None,
            timeout,
            timeout,
        )
    }

    fn fresh_cohort(
        total: usize,
        mut participants: Vec<usize>,
    ) -> Result<crate::measurements::ExecutionCohort, Error> {
        if !(1..=64).contains(&total) || participants.is_empty() || participants.len() > total {
            return Err(Error::Input("invalid batch admission population".into()));
        }
        participants.sort_unstable();
        let mut cohort = crate::measurements::ExecutionCohort {
            id: crate::workloads::TaskIdentity::local("execution-cohort", None).attempt_id,
            selected_replicas: total,
            admitted_slots: participants,
            dispatch: None,
        };
        cohort.dispatch = Some(cohort.dispatch_for(cohort.admitted_slots[0], total)?);
        Ok(cohort)
    }

    fn with_deadlines(
        cohort: crate::measurements::ExecutionCohort,
        expected: Option<Compatibility>,
        material_timeout: Duration,
        launch_timeout: Duration,
    ) -> Result<Self, Error> {
        let total = cohort.selected_replicas;
        if cohort
            .admitted_slots
            .first()
            .is_none_or(|index| !cohort.admits(*index, total))
            || cohort.dispatch.is_none()
        {
            return Err(Error::Input(
                "batch admission requires its frozen validated dispatch cohort".into(),
            ));
        }
        let material = Gate::selected(total, &cohort.admitted_slots, expected, material_timeout)?;
        let launch = Gate::deferred_selected(total, &cohort.admitted_slots, launch_timeout)?;
        Ok(Self {
            claimed: std::sync::atomic::AtomicBool::new(false),
            material,
            launch,
            cohort,
        })
    }

    pub(super) fn observe(mut self, observer: Arc<PreparationObserver>) -> Self {
        self.material = self.material.observe(observer);
        self
    }

    pub(super) fn material(&self) -> &Gate {
        &self.material
    }
    pub(super) fn launch(&self) -> &Gate {
        &self.launch
    }
    pub(super) fn cohort(&self) -> &crate::measurements::ExecutionCohort {
        &self.cohort
    }
    pub(super) fn count(&self) -> usize {
        self.cohort.admitted_slots.len()
    }

    pub(super) fn claim(&self) -> Result<(), Error> {
        self.claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| {
                Error::Input(
                    "batch admission was already consumed; do not retry its original slots".into(),
                )
            })
    }

    pub(super) fn matches(
        &self,
        total: usize,
        reused: Option<&std::collections::BTreeMap<usize, serde_json::Value>>,
    ) -> bool {
        self.cohort.selected_replicas == total
            && !reused.is_some_and(|slots| slots.keys().any(|slot| !(1..=total).contains(slot)))
            && self
                .cohort
                .admitted_slots
                .iter()
                .copied()
                .eq((1..=total)
                    .filter(|slot| !reused.is_some_and(|slots| slots.contains_key(slot))))
    }
}

struct State {
    deadline: Option<Instant>,
    participants: Vec<bool>,
    registered: Vec<bool>,
    ready: Vec<bool>,
    identity: Option<Compatibility>,
    aborted: Option<String>,
    released: bool,
}

pub(crate) struct Arrival<'a> {
    gate: &'a Gate,
    index: usize,
    settled: bool,
}

impl Gate {
    pub(crate) fn new(
        total: usize,
        expected: Option<Compatibility>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if total > 64 {
            return Err(Error::Input("invalid preparation gate population".into()));
        }
        Self::selected(total, &(1..=total).collect::<Vec<_>>(), expected, timeout)
    }

    /// Wait for actual admitted original slots, never synthetic arrivals for
    /// reused peers. Identity.total remains the original selected concurrency.
    pub(crate) fn selected(
        total: usize,
        admitted: &[usize],
        expected: Option<Compatibility>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if !(1..=64).contains(&total) || timeout.is_zero() {
            return Err(Error::Input(
                "invalid preparation gate population or timeout".into(),
            ));
        }
        let mut participants = vec![false; total];
        if admitted.is_empty() || admitted.len() > total {
            return Err(Error::Input(
                "invalid preparation participant population".into(),
            ));
        }
        for slot in admitted {
            if !(1..=total).contains(slot) || participants[*slot - 1] {
                return Err(Error::Input(
                    "invalid or duplicate preparation participant".into(),
                ));
            }
            participants[*slot - 1] = true;
        }
        if let Some(identity) = &expected {
            identity
                .validate()
                .map_err(|error| Error::Input(error.to_string()))?;
        }
        Ok(Self {
            state: Mutex::new(State {
                deadline: Some(
                    Instant::now()
                        .checked_add(timeout)
                        .ok_or_else(|| Error::Input("preparation deadline overflows".into()))?,
                ),
                registered: vec![false; total],
                participants,
                ready: vec![false; total],
                identity: expected,
                aborted: None,
                released: false,
            }),
            changed: Condvar::new(),
            timeout,
            observer: None,
        })
    }

    /// A separate launch budget starts at the first post-material registration.
    #[cfg(test)]
    pub(crate) fn deferred(total: usize, timeout: Duration) -> Result<Self, Error> {
        if total > 64 {
            return Err(Error::Input("invalid preparation gate population".into()));
        }
        Self::deferred_selected(total, &(1..=total).collect::<Vec<_>>(), timeout)
    }

    pub(crate) fn deferred_selected(
        total: usize,
        admitted: &[usize],
        timeout: Duration,
    ) -> Result<Self, Error> {
        let mut gate = Self::selected(total, admitted, None, timeout)?;
        gate.state.get_mut().map_err(|_| unavailable())?.deadline = None;
        Ok(gate)
    }

    pub(crate) fn observe(mut self, observer: Arc<PreparationObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub(crate) fn confirmed_identity(&self) -> Result<Compatibility, Error> {
        let state = self.state.lock().map_err(|_| unavailable())?;
        if !state.released || state.aborted.is_some() {
            return Err(Error::Run("batch preparation was not confirmed".into()));
        }
        state.identity.clone().ok_or_else(unavailable)
    }

    pub(crate) fn register(&self, replica: Identity) -> Result<Arrival<'_>, Error> {
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        if let Some(reason) = &state.aborted {
            return Err(Error::Run(reason.clone()));
        }
        if !replica.valid()
            || replica.total != state.registered.len()
            || !state.participants[replica.index - 1]
            || state.registered[replica.index - 1]
            || state.released
        {
            state.aborted = Some("invalid or duplicate preparation slot".into());
            self.changed.notify_all();
            return Err(Error::Run("invalid or duplicate preparation slot".into()));
        }
        if state.deadline.is_none() {
            state.deadline = Some(
                Instant::now()
                    .checked_add(self.timeout)
                    .ok_or_else(unavailable)?,
            );
        }
        state.registered[replica.index - 1] = true;
        Ok(Arrival {
            gate: self,
            index: replica.index - 1,
            settled: false,
        })
    }

    pub(super) fn abort(&self, reason: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.released && state.aborted.is_none() {
            state.aborted = Some(reason.into());
        }
        self.changed.notify_all();
    }
}

impl Arrival<'_> {
    pub(crate) fn abort(&mut self, reason: &str) {
        self.gate.abort(reason);
    }

    pub(crate) fn ready(&mut self, identity: Compatibility) -> Result<(), Error> {
        let mut state = self.gate.state.lock().map_err(|_| unavailable())?;
        if let Some(reason) = &state.aborted {
            return Err(Error::Run(reason.clone()));
        }
        let deadline = state.deadline.ok_or_else(unavailable)?;
        if Instant::now() >= deadline && !state.released {
            state.aborted = Some("batch preparation timed out; no agent was admitted".into());
        } else if self.settled || state.ready[self.index] {
            state.aborted = Some("preparation slot arrived more than once".into());
        } else if let Err(error) = identity.validate() {
            state.aborted = Some(error.to_string());
        } else if state
            .identity
            .as_ref()
            .is_some_and(|expected| expected != &identity)
        {
            state.aborted =
                Some("prepared workload material or prompt differs within/across batches".into());
        } else {
            state.identity.get_or_insert(identity);
            state.ready[self.index] = true;
            if state
                .ready
                .iter()
                .zip(&state.participants)
                .all(|(ready, selected)| !selected || *ready)
                && state.aborted.is_none()
            {
                // Publication can block: do not hold the coordination mutex while
                // writing it, so peers can still time out and abort admission.
                let identity = state.identity.clone().ok_or_else(unavailable)?;
                drop(state);
                let published = self
                    .gate
                    .observer
                    .as_ref()
                    .map_or(Ok(()), |observe| observe(&identity));
                state = self.gate.state.lock().map_err(|_| unavailable())?;
                if state.aborted.is_none() {
                    if let Err(error) = published {
                        state.aborted =
                            Some(format!("cannot publish prepared batch identity: {error}"));
                    } else if Instant::now() >= deadline {
                        state.aborted =
                            Some("batch preparation timed out; no agent was admitted".into());
                    } else {
                        state.released = true;
                    }
                }
            }
        }
        self.gate.changed.notify_all();
        loop {
            if let Some(reason) = &state.aborted {
                return Err(Error::Run(reason.clone()));
            }
            if state.released {
                self.settled = true;
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.aborted = Some("batch preparation timed out; no agent was admitted".into());
                self.gate.changed.notify_all();
                continue;
            }
            let (next, _) = self
                .gate
                .changed
                .wait_timeout(state, remaining.min(Duration::from_secs(1)))
                .map_err(|_| unavailable())?;
            state = next;
        }
    }
}

/// The runner binds verified material once; adapters cannot fabricate identity.
pub(crate) struct LaunchAdmission<'a> {
    arrival: Mutex<Arrival<'a>>,
    identity: Compatibility,
}

impl<'a> LaunchAdmission<'a> {
    pub(crate) fn new(
        gate: &'a Gate,
        replica: Identity,
        identity: Compatibility,
    ) -> Result<Self, Error> {
        Ok(Self {
            arrival: Mutex::new(gate.register(replica)?),
            identity,
        })
    }
}

impl crate::workloads::session::LaunchReadiness for LaunchAdmission<'_> {
    fn ready(&self) -> Result<(), String> {
        self.arrival
            .lock()
            .map_err(|_| unavailable().to_string())?
            .ready(self.identity.clone())
            .map_err(|error| error.to_string())
    }
    fn abort(&self) {
        self.arrival
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort("replica failed before shared launch readiness");
    }
    fn remaining(&self) -> Result<Duration, String> {
        let arrival = self.arrival.lock().map_err(|_| unavailable().to_string())?;
        let state = arrival
            .gate
            .state
            .lock()
            .map_err(|_| unavailable().to_string())?;
        if let Some(reason) = &state.aborted {
            return Err(reason.clone());
        }
        state
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "shared launch preparation deadline exceeded".into())
    }
}

impl Drop for Arrival<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.abort(&format!(
                "replica {} left before batch preparation completed",
                self.index + 1
            ));
        }
    }
}

fn unavailable() -> Error {
    Error::Run("batch preparation coordination was interrupted".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replica_spawn_failure_releases_waiting_material_and_launch_gates() {
        let material_gate = Gate::new(2, None, Duration::from_secs(30)).unwrap();
        let launch_gate = Gate::new(2, None, Duration::from_secs(30)).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut arrival = material_gate
                    .register(Identity { index: 1, total: 2 })
                    .unwrap();
                ready_tx.send(()).unwrap();
                done_tx.send(arrival.ready(material()).is_err()).unwrap();
            });
            ready_rx.recv().unwrap();
            super::super::abort_replica_gates(
                super::super::RunOptions {
                    preparation_gate: Some(&material_gate),
                    launch_gate: Some(&launch_gate),
                    ..Default::default()
                },
                "cannot start benchmark replica 2",
            );
            assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
            assert!(
                launch_gate
                    .register(Identity { index: 1, total: 2 })
                    .is_err()
            );
        });
    }

    #[test]
    fn replica_panic_before_registration_aborts_waiting_peer_and_keeps_output() {
        let gate = Gate::new(2, None, Duration::from_secs(30)).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut arrival = gate.register(Identity { index: 1, total: 2 }).unwrap();
                ready_tx.send(()).unwrap();
                done_tx.send(arrival.ready(material()).is_err()).unwrap();
            });
            ready_rx.recv().unwrap();
            let (index, result, output) = super::super::run_replica_catching(
                1,
                super::super::RunOptions {
                    preparation_gate: Some(&gate),
                    ..Default::default()
                },
                |output| {
                    output.extend_from_slice(b"prior output");
                    panic!("injected replica failure before registration");
                },
            );
            assert_eq!(index, 1);
            assert!(result.unwrap_err().to_string().contains("replica 2"));
            assert_eq!(output, b"prior output");
            assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        });
    }
    #[test]
    fn owned_admission_keeps_both_gates_on_actual_original_participants() {
        let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = observed.clone();
        let admission = Admission::for_test(4, vec![4, 2], Duration::from_secs(5))
            .unwrap()
            .observe(Arc::new(move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }));
        let reused = std::collections::BTreeMap::from([
            (1, serde_json::Value::Null),
            (3, serde_json::Value::Null),
        ]);
        assert_eq!(admission.cohort().admitted_slots, vec![2, 4]);
        assert_eq!(admission.count(), 2);
        assert!(admission.matches(4, Some(&reused)));
        assert!(!admission.matches(2, Some(&reused)));
        assert!(!admission.matches(4, None));
        for gate in [admission.material(), admission.launch()] {
            let state = gate.state.lock().unwrap();
            assert_eq!(state.participants, vec![false, true, false, true]);
            assert_eq!(state.registered, vec![false; 4]);
            assert_eq!(state.ready, vec![false; 4]);
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = [2, 4]
                .into_iter()
                .map(|index| {
                    let admission = &admission;
                    scope.spawn(move || {
                        let replica = Identity { index, total: 4 };
                        admission
                            .material()
                            .register(replica)
                            .unwrap()
                            .ready(material())
                            .unwrap();
                        admission
                            .launch()
                            .register(replica)
                            .unwrap()
                            .ready(material())
                            .unwrap();
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
        for gate in [admission.material(), admission.launch()] {
            let state = gate.state.lock().unwrap();
            assert_eq!(state.registered, vec![false, true, false, true]);
            assert_eq!(state.ready, vec![false, true, false, true]);
            assert!(state.released);
        }
        for (total, members) in [
            (0, vec![1]),
            (65, vec![1]),
            (4, vec![]),
            (4, vec![2, 2]),
            (4, vec![0, 2]),
            (4, vec![2, 5]),
        ] {
            assert!(Admission::for_test(total, members, Duration::from_secs(1)).is_err());
        }
    }

    #[test]
    fn owned_admission_arms_runtime_gates_without_reminting_frozen_cohort() {
        let original = Admission::new(
            4,
            vec![2, 4],
            None,
            crate::planning::BatchAdmissionPolicy::current(),
        )
        .unwrap();
        let frozen = original.cohort().clone();
        let admission = Admission::from_cohort(
            frozen.clone(),
            Some(material()),
            crate::planning::BatchAdmissionPolicy::current(),
        )
        .unwrap();
        assert_eq!(admission.cohort(), &frozen);
        assert!(
            admission
                .material()
                .state
                .lock()
                .unwrap()
                .deadline
                .is_some()
        );
        assert!(admission.launch().state.lock().unwrap().deadline.is_none());
        admission.claim().unwrap();
        assert!(admission.claim().is_err());
        // In-memory state belongs to this runtime activation, not the earlier
        // planner's temporary object with the same frozen metadata.
        assert!(original.claim().is_ok());
        for change in ["unknown", "population", "members", "id"] {
            let mut forged = frozen.clone();
            match change {
                "unknown" => forged.dispatch = None,
                "population" => forged.selected_replicas = 2,
                "members" => forged.admitted_slots = vec![4, 2],
                "id" => forged.id = "not/a/cohort".into(),
                _ => unreachable!(),
            }
            assert!(
                Admission::from_cohort(
                    forged,
                    None,
                    crate::planning::BatchAdmissionPolicy::current()
                )
                .is_err(),
                "{change}"
            );
        }
    }
    use std::sync::mpsc;

    fn material() -> Compatibility {
        Compatibility {
            agent: "agent".into(),
            provider: "provider".into(),
            model: "model".into(),
            workload: "workload".into(),
            task: "task".into(),
            dataset_sha256: None,
            prompt_sha256: "a".repeat(64),
            material_sha256: "b".repeat(64),
            protocol_sha256: None,
        }
    }

    #[test]
    fn noncontiguous_original_slots_release_only_after_genuine_subset_arrivals() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for deferred in [false, true] {
            let observations = Arc::new(AtomicUsize::new(0));
            let counter = observations.clone();
            let gate = if deferred {
                Gate::deferred_selected(4, &[2, 4], Duration::from_secs(3)).unwrap()
            } else {
                Gate::selected(4, &[2, 4], Some(material()), Duration::from_secs(3)).unwrap()
            }
            .observe(Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
            std::thread::scope(|scope| {
                let mut first = gate.register(Identity { index: 2, total: 4 }).unwrap();
                let (tx, rx) = mpsc::channel();
                scope.spawn(move || {
                    first.ready(material()).unwrap();
                    tx.send(()).unwrap();
                });
                assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
                assert_eq!(observations.load(Ordering::SeqCst), 0);
                gate.register(Identity { index: 4, total: 4 })
                    .unwrap()
                    .ready(material())
                    .unwrap();
                rx.recv_timeout(Duration::from_secs(1)).unwrap();
            });
            assert_eq!(gate.confirmed_identity().unwrap(), material());
            assert_eq!(observations.load(Ordering::SeqCst), 1);
            let state = gate.state.lock().unwrap();
            assert_eq!(state.registered, [false, true, false, true]);
            assert_eq!(state.ready, [false, true, false, true]);
        }
    }

    #[test]
    fn subset_gate_rejects_nonmembers_duplicate_or_renumbered_original_slots() {
        for admitted in [vec![], vec![0], vec![5], vec![2, 2]] {
            assert!(Gate::selected(4, &admitted, None, Duration::from_secs(1)).is_err());
        }
        for bad in [
            Identity { index: 1, total: 4 },
            Identity { index: 2, total: 2 },
            Identity { index: 2, total: 4 },
        ] {
            let gate = Gate::selected(4, &[2, 4], None, Duration::from_secs(1)).unwrap();
            let mut first = gate.register(Identity { index: 2, total: 4 }).unwrap();
            assert!(gate.register(bad).is_err());
            assert!(first.ready(material()).is_err());
            assert!(gate.confirmed_identity().is_err());
        }
    }

    #[test]
    fn subset_failure_or_expiry_cannot_release_waiting_original_peers() {
        for timeout in [false, true] {
            let gate =
                Gate::selected(4, &[2, 4], Some(material()), Duration::from_secs(3)).unwrap();
            let mut first = gate.register(Identity { index: 2, total: 4 }).unwrap();
            let mut second = gate.register(Identity { index: 4, total: 4 }).unwrap();
            if timeout {
                gate.state.lock().unwrap().deadline = Some(Instant::now() - Duration::from_secs(1));
            } else {
                second.abort("pending original slot failed before readiness");
            }
            assert!(first.ready(material()).is_err());
            assert!(second.ready(material()).is_err());
            assert!(gate.confirmed_identity().is_err());
        }
    }

    #[test]
    fn no_agent_admitted_until_all_material_matches() {
        let gate = Gate::new(2, None, Duration::from_secs(3)).unwrap();
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
            scope.spawn(move || {
                first.ready(material()).unwrap();
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
            gate.register(Identity { index: 2, total: 2 })
                .unwrap()
                .ready(material())
                .unwrap();
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
    }

    #[test]
    fn launch_deadline_arms_once_after_material_and_never_restarts_for_siblings() {
        use crate::workloads::session::LaunchReadiness;
        let gate = Gate::deferred(2, Duration::from_secs(1800)).unwrap();
        assert!(gate.state.lock().unwrap().deadline.is_none());
        let first =
            LaunchAdmission::new(&gate, Identity { index: 1, total: 2 }, material()).unwrap();
        let deadline = gate.state.lock().unwrap().deadline.unwrap();
        let second =
            LaunchAdmission::new(&gate, Identity { index: 2, total: 2 }, material()).unwrap();
        assert_eq!(gate.state.lock().unwrap().deadline, Some(deadline));
        assert!(first.remaining().unwrap() > Duration::from_secs(1700));
        gate.state.lock().unwrap().deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(first.remaining().is_err());
        assert!(second.ready().is_err());
        assert!(first.ready().is_err());
        assert!(gate.confirmed_identity().is_err());
    }

    #[test]
    fn late_readiness_and_duplicate_registration_preserve_first_failure() {
        let gate = Gate::deferred(2, Duration::from_secs(1800)).unwrap();
        let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
        let mut second = gate.register(Identity { index: 2, total: 2 }).unwrap();
        first.abort("original preparation failure");
        gate.state.lock().unwrap().deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(
            second
                .ready(material())
                .unwrap_err()
                .to_string()
                .contains("original preparation failure")
        );
        let error = match gate.register(Identity { index: 2, total: 2 }) {
            Ok(_) => panic!("aborted launch cannot register another slot"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("original preparation failure"));
    }

    #[test]
    fn early_return_panic_mismatch_duplicate_and_timeout_abort_waiters() {
        for fault in ["return", "panic", "mismatch", "duplicate", "timeout"] {
            let gate = Gate::new(2, None, Duration::from_millis(200)).unwrap();
            std::thread::scope(|scope| {
                let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
                let waiter = scope.spawn(move || first.ready(material()));
                if fault != "timeout" {
                    let mut second = gate.register(Identity { index: 2, total: 2 }).unwrap();
                    match fault {
                        "return" => drop(second),
                        "panic" => {
                            assert!(
                                scope
                                    .spawn(move || {
                                        let _guard = second;
                                        panic!("fixture");
                                    })
                                    .join()
                                    .is_err()
                            );
                        }
                        "mismatch" => {
                            let mut value = material();
                            value.material_sha256 = "c".repeat(64);
                            assert!(second.ready(value).is_err());
                        }
                        "duplicate" => {
                            assert!(gate.register(Identity { index: 2, total: 2 }).is_err());
                            drop(second);
                        }
                        _ => unreachable!(),
                    }
                }
                assert!(waiter.join().unwrap().is_err(), "{fault}");
            });
            assert!(gate.register(Identity { index: 2, total: 2 }).is_err());
        }
    }

    #[test]
    fn reference_from_earlier_batch_rejects_changed_material() {
        let gate = Gate::new(1, Some(material()), Duration::from_secs(1)).unwrap();
        let mut changed = material();
        changed.prompt_sha256 = "d".repeat(64);
        assert!(
            gate.register(Identity::default())
                .unwrap()
                .ready(changed)
                .is_err()
        );
        assert!(Gate::new(0, None, Duration::from_secs(1)).is_err());
        assert!(Gate::new(65, None, Duration::from_secs(1)).is_err());
        assert!(Gate::new(usize::MAX, None, Duration::from_secs(1)).is_err());
        assert!(Gate::deferred(usize::MAX, Duration::from_secs(1)).is_err());
        assert!(Gate::new(1, None, Duration::ZERO).is_err());
    }

    #[test]
    fn expired_gate_never_releases_single_or_all_late_arrivals() {
        for total in [1, 2] {
            let gate = Gate::new(total, None, Duration::from_millis(1)).unwrap();
            let mut arrivals: Vec<_> = (1..=total)
                .map(|index| gate.register(Identity { index, total }).unwrap())
                .collect();
            std::thread::sleep(Duration::from_millis(5));
            for arrival in &mut arrivals {
                assert!(
                    arrival
                        .ready(material())
                        .unwrap_err()
                        .to_string()
                        .contains("timed out")
                );
            }
        }
    }

    #[test]
    fn failed_or_slow_prepared_publication_cannot_admit_agents_or_block_peer_timeout() {
        let gate = Gate::new(1, None, Duration::from_secs(1))
            .unwrap()
            .observe(Arc::new(|_| {
                Err(Error::Io("fixture publication failure".into()))
            }));
        assert!(
            gate.register(Identity::default())
                .unwrap()
                .ready(material())
                .unwrap_err()
                .to_string()
                .contains("publication failure")
        );
        assert!(gate.confirmed_identity().is_err());
        let (begun_tx, begun_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let gate = Gate::new(2, None, Duration::from_millis(100))
            .unwrap()
            .observe(Arc::new(move |_| {
                begun_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                Ok(())
            }));
        std::thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel();
            let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
            let mut second = gate.register(Identity { index: 2, total: 2 }).unwrap();
            let first_done = done_tx.clone();
            let a = scope.spawn(move || {
                assert!(first.ready(material()).is_err());
                first_done.send(()).unwrap();
            });
            let b = scope.spawn(move || {
                assert!(second.ready(material()).is_err());
                done_tx.send(()).unwrap();
            });
            begun_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            // The waiting peer must time out while publication remains blocked.
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            release_tx.send(()).unwrap();
            a.join().unwrap();
            b.join().unwrap();
            assert!(gate.confirmed_identity().is_err());
        });
    }

    #[test]
    fn runner_abort_notifies_peers_before_slow_cancellation_and_preserves_cleanup_error() {
        struct Session(std::sync::mpsc::Receiver<()>);
        impl crate::workloads::WorkloadSession for Session {
            fn invocation(&self) -> &crate::workloads::session::TaskInvocation {
                unreachable!()
            }
            fn evaluation_kind(&self) -> crate::workloads::KnownEvaluationKind {
                crate::workloads::KnownEvaluationKind::Ungraded
            }
            fn baseline_passed(&self) -> Option<bool> {
                None
            }
            fn evaluate(&mut self, _: &str) -> Result<crate::workloads::Evaluation, String> {
                unreachable!()
            }
            fn cancel_prepared(&mut self) -> Result<(), String> {
                self.0
                    .recv_timeout(Duration::from_secs(1))
                    .expect("peer must wake before cleanup completes");
                Err("fixture preservation failure".into())
            }
        }
        let gate = Gate::new(2, None, Duration::from_secs(3)).unwrap();
        std::thread::scope(|scope| {
            let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
            let mut second = gate.register(Identity { index: 2, total: 2 }).unwrap();
            let (tx, rx) = mpsc::channel();
            let peer = scope.spawn(move || {
                assert!(first.ready(material()).is_err());
                tx.send(()).unwrap();
            });
            let error = super::super::abort_prepared(
                Error::Run("identity unavailable".into()),
                &mut second,
                &mut Session(rx),
            );
            assert!(error.to_string().contains("identity unavailable"));
            assert!(error.to_string().contains("fixture preservation failure"));
            peer.join().unwrap();
        });
    }

    #[test]
    fn abort_during_publication_keeps_first_failure_after_error_or_expiry() {
        for publication_error in [false, true] {
            let (begun_tx, begun_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let gate = Gate::new(2, None, Duration::from_secs(1))
                .unwrap()
                .observe(Arc::new(move |_| {
                    begun_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap();
                    if publication_error {
                        Err(Error::Io("later publication error".into()))
                    } else {
                        Ok(())
                    }
                }));
            std::thread::scope(|scope| {
                let mut first = gate.register(Identity { index: 1, total: 2 }).unwrap();
                let mut second = gate.register(Identity { index: 2, total: 2 }).unwrap();
                let a = scope.spawn(move || first.ready(material()));
                let b = scope.spawn(move || second.ready(material()));
                begun_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                gate.abort("original sibling failure during publication");
                if !publication_error {
                    std::thread::sleep(Duration::from_millis(1100));
                }
                release_tx.send(()).unwrap();
                for result in [a.join().unwrap(), b.join().unwrap()] {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("original sibling failure during publication")
                    );
                }
            });
            assert!(gate.confirmed_identity().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn runner_single_unsupported_and_multi_mismatch_stop_before_proxy_or_agent_launch() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let _disk = crate::disk_space::test_available_mib(100_000);
        for concurrency in [1, 2] {
            let root = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("batch-runner", None).attempt_id);
            fs::create_dir_all(root.join(".asb")).unwrap();
            fs::write(
                root.join(".asb/project.toml"),
                toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
            )
            .unwrap();
            fs::create_dir(root.join("workloads")).unwrap();
            crate::workloads::populate(&root.join("workloads")).unwrap();
            let workspace = root.join(if concurrency == 1 {
                "workspace"
            } else {
                "workspace-concurrency-1"
            });
            fs::create_dir(&workspace).unwrap();
            if concurrency == 1 {
                symlink("/does/not/exist", workspace.join("unsupported")).unwrap();
            } else {
                fs::write(
                    workspace.join("different-initial-content"),
                    "preserve user file",
                )
                .unwrap();
            }
            let config = root.join("benchmark.toml");
            fs::write(&config, format!("agent='/must-not-launch-fixture-agent'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nworkspace='workspace'\nlog_file='run.log'\nconcurrency={concurrency}\n")).unwrap();
            let cassette = root.join("input.jsonl");
            // Satisfies preflight; intentionally not a valid proxy trace. If the
            // gate is bypassed the error changes, so an unrelated proxy failure
            // cannot make this no-launch regression pass accidentally.
            fs::write(
                &cassette,
                "{\"request\":{\"body\":{\"model\":\"fixture\",\"tools\":[{}]}}}\n",
            )
            .unwrap();
            let error = super::super::run_with_tape(
                config.to_str().unwrap(),
                super::super::RunOptions {
                    verify_initial_material: true,
                    mode: Some(crate::tape::Mode::Replay),
                    cassette: cassette.to_str(),
                    json: true,
                    ..Default::default()
                },
                &mut Vec::new(),
            )
            .unwrap_err()
            .to_string();
            let expected = if concurrency == 1 {
                "unsupported symlink"
            } else {
                "differs within/across batches"
            };
            assert!(error.contains(expected), "{concurrency}: {error}");
            assert!(!root.join("run.log").exists());
            assert!(!workspace.with_extension("asb-agent-runtime").exists());
            if concurrency == 2 {
                assert_eq!(
                    fs::read_to_string(workspace.join("different-initial-content")).unwrap(),
                    "preserve user file"
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }
}
