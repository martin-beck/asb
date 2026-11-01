//! Bounded FIFO scheduling of distinct configurations, weighted by replicas.
use crate::{Error, benchmark};
use serde::{Deserialize, Serialize};
use std::sync::{Condvar, Mutex, mpsc};

pub(crate) const MAX_JOBS: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scheduling {
    pub jobs: usize,
    pub max_active_attempts: usize,
}

impl Scheduling {
    pub(super) fn new(
        jobs: usize,
        budget: Option<usize>,
        weights: &[usize],
    ) -> Result<Self, Error> {
        let maximum = weights.iter().copied().max().unwrap_or(1);
        let limits = Self {
            jobs,
            max_active_attempts: budget.unwrap_or(maximum.max(jobs)),
        };
        limits.validate(weights)?;
        Ok(limits)
    }

    pub(super) fn validate(&self, weights: &[usize]) -> Result<(), Error> {
        if !(1..=MAX_JOBS).contains(&self.jobs) {
            return Err(Error::Input(format!(
                "sweep jobs must be between 1 and {MAX_JOBS}"
            )));
        }
        if !(1..=benchmark::MAX_CONCURRENCY).contains(&self.max_active_attempts)
            || weights
                .iter()
                .any(|weight| *weight == 0 || *weight > self.max_active_attempts)
        {
            return Err(Error::Input(format!(
                "sweep active-attempt budget must be between 1 and {} and at least each configuration's replica concurrency; increase --max-active-attempts or reduce concurrency",
                benchmark::MAX_CONCURRENCY
            )));
        }
        Ok(())
    }
}

pub(super) enum Event<T = ()> {
    Started(usize),
    Finished(usize, Result<T, Error>),
}

#[derive(Default)]
struct State {
    next: usize,
    active: usize,
    stopped: bool,
    resources: crate::planning::Resources,
}

struct Queue<'a> {
    state: Mutex<State>,
    changed: Condvar,
    weights: &'a [usize],
    budget: usize,
    capacity: Option<crate::planning::Resources>,
    costs: &'a [crate::planning::Resources],
}

impl Queue<'_> {
    fn take(&self) -> Option<(usize, Permit<'_, '_>)> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if state.stopped || state.next == self.weights.len() {
                return None;
            }
            let index = state.next;
            let cost = self.weights[index];
            let resources = self.costs[index];
            let total = state.resources.checked_add(resources);
            if state.active + cost <= self.budget
                && total
                    .is_some_and(|total| self.capacity.is_none_or(|capacity| total.fits(capacity)))
            {
                state.next += 1;
                state.active += cost;
                state.resources = total.expect("checked resource addition");
                return Some((
                    index,
                    Permit {
                        queue: self,
                        cost,
                        resources,
                    },
                ));
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn stop(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopped = true;
        self.changed.notify_all();
    }
}

struct Permit<'q, 'w> {
    queue: &'q Queue<'w>,
    cost: usize,
    resources: crate::planning::Resources,
}

impl Drop for Permit<'_, '_> {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active -= self.cost;
        state.resources.subtract(self.resources);
        drop(state);
        self.queue.changed.notify_all();
    }
}

/// Output callbacks stay on the caller thread. On output/spawn failure stop
/// admission and drain already running work before returning; never detach it.
#[cfg(test)]
pub(super) fn run(
    limits: Scheduling,
    weights: &[usize],
    execute: impl Fn(usize) -> Result<(), Error> + Sync,
    observe: impl FnMut(Event) -> Result<(), Error>,
) -> Result<(), Error> {
    run_with_resources(
        limits,
        weights,
        None,
        &vec![crate::planning::Resources::default(); weights.len()],
        execute,
        observe,
    )
}

#[cfg(test)]
pub(super) fn run_with_resources(
    limits: Scheduling,
    weights: &[usize],
    capacity: Option<crate::planning::Resources>,
    costs: &[crate::planning::Resources],
    execute: impl Fn(usize) -> Result<(), Error> + Sync,
    observe: impl FnMut(Event) -> Result<(), Error>,
) -> Result<(), Error> {
    run_with_resources_stopping_on(
        limits,
        weights,
        capacity,
        costs,
        execute,
        |_| false,
        observe,
    )
}

pub(super) fn run_with_resources_stopping_on<T: Send>(
    limits: Scheduling,
    weights: &[usize],
    capacity: Option<crate::planning::Resources>,
    costs: &[crate::planning::Resources],
    execute: impl Fn(usize) -> Result<T, Error> + Sync,
    stop_admission: impl Fn(Result<&T, &Error>) -> bool + Sync,
    mut observe: impl FnMut(Event<T>) -> Result<(), Error>,
) -> Result<(), Error> {
    limits.validate(weights)?;
    if costs.len() != weights.len()
        || costs
            .iter()
            .any(|cost| capacity.is_some_and(|capacity| !cost.fits(capacity)))
    {
        return Err(Error::Input(
            "configuration resources exceed scheduler capacity".into(),
        ));
    }
    let queue = Queue {
        state: Mutex::new(State::default()),
        changed: Condvar::new(),
        weights,
        budget: limits.max_active_attempts,
        capacity,
        costs,
    };
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(limits.jobs * 2);
        let mut handles = Vec::new();
        let mut failure = None;
        for _ in 0..limits.jobs.min(weights.len()) {
            let sender = sender.clone();
            let queue = &queue;
            let execute = &execute;
            let stop_admission = &stop_admission;
            match std::thread::Builder::new().spawn_scoped(scope, move || {
                while let Some((index, _permit)) = queue.take() {
                    if sender.send(Event::Started(index)).is_err() {
                        break;
                    }
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(index)))
                            .unwrap_or_else(|_| {
                                Err(Error::Run(
                                    "sweep worker panicked; inspect its private result artifacts"
                                        .into(),
                                ))
                            });
                    if stop_admission(result.as_ref()) {
                        queue.stop();
                    }
                    if sender.send(Event::Finished(index, result)).is_err() {
                        break;
                    }
                }
            }) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    queue.stop();
                    failure = Some(Error::Run(format!(
                        "cannot start bounded sweep worker: {error}"
                    )));
                    break;
                }
            }
        }
        drop(sender);
        for event in receiver {
            // Even a broken terminal must not hide outcomes of admitted work.
            // Stop new admission, but deliver every finished result while
            // joining the owned workers. Never replace the first output error.
            if (failure.is_none() || matches!(event, Event::Finished(..)))
                && let Err(error) = observe(event)
            {
                queue.stop();
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        for handle in handles {
            if handle.join().is_err() && failure.is_none() {
                failure = Some(Error::Run(
                    "sweep scheduler worker failed outside execution".into(),
                ));
            }
        }
        failure.map_or(Ok(()), Err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn panicking_worker_counts_towards_stopping_before_the_next_claim() {
        let admitted = std::sync::Barrier::new(2);
        let release = (Mutex::new(false), Condvar::new());
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let mut unexpected = false;
        let mut completed = Vec::new();
        run_with_resources_stopping_on(
            Scheduling::new(2, Some(2), &[1; 4]).unwrap(),
            &[1; 4],
            None,
            &[crate::planning::Resources::default(); 4],
            |index| {
                if index < 2 {
                    admitted.wait();
                }
                if index == 0 {
                    panic!("fixture worker panic");
                }
                if index == 1 {
                    let mut done = release.0.lock().unwrap();
                    while !*done {
                        done = release.1.wait(done).unwrap();
                    }
                } else {
                    claimed_tx.send(index).unwrap();
                }
                Ok(())
            },
            |result| result.is_err(),
            |event| {
                if let Event::Finished(index, result) = event {
                    if index == 0 {
                        assert!(result.unwrap_err().to_string().contains("worker panicked"));
                        unexpected = claimed_rx.recv_timeout(Duration::from_secs(1)).is_ok();
                        *release.0.lock().unwrap() = true;
                        release.1.notify_all();
                    }
                    completed.push(index);
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(
            !unexpected,
            "panic allowed a later configuration to be claimed"
        );
        completed.sort_unstable();
        assert_eq!(completed, [0, 1]);
    }

    #[test]
    fn actual_spool_failure_stops_before_next_claim_and_joins_admitted_resource_holders() {
        use std::io::Write;
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("scheduler-spool", None).attempt_id);
        std::fs::create_dir(&root).unwrap();
        let admitted = std::sync::Barrier::new(2);
        let release = (Mutex::new(false), Condvar::new());
        let active = AtomicUsize::new(0);
        let mut starts = Vec::new();
        let mut finishes = Vec::new();
        let costs = vec![
            crate::planning::Resources {
                cpu_millis: 1,
                memory_mib: 1,
                disk_mib: 1
            };
            4
        ];
        let capacity = crate::planning::Resources {
            cpu_millis: 2,
            memory_mib: 2,
            disk_mib: 2,
        };
        run_with_resources_stopping_on(
            Scheduling::new(2, Some(2), &[1; 4]).unwrap(),
            &[1; 4],
            Some(capacity),
            &costs,
            |index| {
                assert!(
                    index < 2,
                    "a worker claimed a later configuration after locally observing output failure"
                );
                active.fetch_add(1, Ordering::SeqCst);
                admitted.wait();
                let report = if index == 0 {
                    let mut spool =
                        super::super::spool::Output::create(&root.join("output"), 1).unwrap();
                    let mut output = super::super::presentation::Presentation::new(&mut spool);
                    assert!(output.write_all(&vec![0; 5 * 1024 * 1024]).is_err());
                    super::super::execution::ConfigurationReport {
                        stop_admission: false,
                        result: Err(Error::Run("official false grade retained".into())),
                        output_error: output.error(),
                        spool_created: true,
                    }
                } else {
                    let mut done = release.0.lock().unwrap();
                    while !*done {
                        done = release.1.wait(done).unwrap();
                    }
                    super::super::execution::ConfigurationReport {
                        stop_admission: false,
                        result: Ok(()),
                        output_error: None,
                        spool_created: false,
                    }
                };
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(report)
            },
            |report| report.is_ok_and(|report| report.output_error.is_some()),
            |event| {
                match event {
                    Event::Started(index) => starts.push(index),
                    Event::Finished(index, result) => {
                        let report = result.unwrap();
                        if index == 0 {
                            assert!(
                                report
                                    .result
                                    .unwrap_err()
                                    .to_string()
                                    .contains("official false grade retained")
                            );
                            assert!(report.output_error.is_some());
                            assert_eq!(
                                active.load(Ordering::SeqCst),
                                1,
                                "the admitted sibling must still hold its execution permit"
                            );
                            *release.0.lock().unwrap() = true;
                            release.1.notify_all();
                        }
                        finishes.push(index);
                    }
                }
                Ok(())
            },
        )
        .unwrap();
        starts.sort();
        finishes.sort();
        assert_eq!(starts, [0, 1]);
        assert_eq!(finishes, [0, 1]);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_admission_limits_jobs_independently_of_replica_slots_and_releases_failed_work() {
        use crate::planning::Resources;
        let weights = vec![1; 32];
        let cost = Resources {
            cpu_millis: 1000,
            memory_mib: 4096,
            disk_mib: 2000,
        };
        let capacity = Resources {
            cpu_millis: 8000,
            memory_mib: 8192,
            disk_mib: 16000,
        };
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(2);
        let mut finished = Vec::new();
        run_with_resources(
            Scheduling::new(8, Some(8), &weights).unwrap(),
            &weights,
            Some(capacity),
            &vec![cost; weights.len()],
            |index| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                assert!(
                    now <= 2,
                    "memory admission must constrain the eight available count slots"
                );
                if index < 2 {
                    barrier.wait();
                }
                std::thread::sleep(Duration::from_millis(1));
                active.fetch_sub(1, Ordering::SeqCst);
                if index == 2 {
                    return Err(Error::Run("fixture failure".into()));
                }
                Ok(())
            },
            |event| {
                if let Event::Finished(index, _) = event {
                    finished.push(index);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        finished.sort_unstable();
        assert_eq!(finished, (0..32).collect::<Vec<_>>());
        assert!(
            run_with_resources(
                Scheduling::new(1, None, &[1]).unwrap(),
                &[1],
                Some(Resources::default()),
                &[cost],
                |_| panic!("invalid resources must not run"),
                |_| Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn parallel_tasks_overlap_without_exceeding_weighted_replica_budget() {
        let weights = vec![2; 64];
        let limits = Scheduling::new(4, Some(4), &weights).unwrap();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let begun = (Mutex::new(0), Condvar::new());
        let mut started = Vec::new();
        let mut finished = Vec::new();
        run(
            limits,
            &weights,
            |index| {
                let cost = active.fetch_add(weights[index], Ordering::SeqCst) + weights[index];
                peak.fetch_max(cost, Ordering::SeqCst);
                assert!(cost <= 4, "replicas must count against the shared budget");
                if index < 2 {
                    let mut count = begun.0.lock().unwrap();
                    *count += 1;
                    begun.1.notify_all();
                    let (_guard, timeout) = begun
                        .1
                        .wait_timeout_while(count, Duration::from_secs(5), |count| *count < 2)
                        .unwrap();
                    assert!(
                        !timeout.timed_out(),
                        "both distinct configurations must actually execute concurrently"
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
                active.fetch_sub(weights[index], Ordering::SeqCst);
                Ok(())
            },
            |event| {
                match event {
                    Event::Started(index) => started.push(index),
                    Event::Finished(index, result) => {
                        result?;
                        finished.push(index);
                    }
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        started.sort_unstable();
        finished.sort_unstable();
        assert_eq!(started, (0..weights.len()).collect::<Vec<_>>());
        assert_eq!(finished, started);
    }

    #[test]
    fn failed_and_panicked_tasks_release_slots_and_do_not_hide_other_results() {
        let weights = vec![1; 20];
        let mut completed = Vec::new();
        let mut failures = Vec::new();
        run(
            Scheduling::new(3, Some(3), &weights).unwrap(),
            &weights,
            |index| {
                if index == 1 {
                    panic!("synthetic owned-worker panic");
                }
                if index == 4 {
                    return Err(Error::Run("synthetic task failure".into()));
                }
                Ok(())
            },
            |event| {
                if let Event::Finished(index, result) = event {
                    completed.push(index);
                    if result.is_err() {
                        failures.push(index);
                    }
                }
                Ok(())
            },
        )
        .unwrap();
        completed.sort_unstable();
        failures.sort_unstable();
        assert_eq!(completed, (0..20).collect::<Vec<_>>());
        assert_eq!(failures, [1, 4]);
    }

    #[test]
    fn broken_output_stops_admission_and_joins_running_workers() {
        let weights = vec![1; 100];
        let entered = AtomicUsize::new(0);
        let exited = AtomicUsize::new(0);
        let mut started = 0;
        let mut finished = 0;
        let error = run(
            Scheduling::new(2, Some(2), &weights).unwrap(),
            &weights,
            |_| {
                entered.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                exited.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |event| {
                match event {
                    Event::Started(_) => started += 1,
                    Event::Finished(_, _) => finished += 1,
                }
                Err(Error::Io("synthetic broken terminal".into()))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("synthetic broken terminal"));
        assert_eq!(started, 1, "no presentation starts after output failure");
        assert_eq!(finished, exited.load(Ordering::SeqCst));
        assert_eq!(
            entered.load(Ordering::SeqCst),
            exited.load(Ordering::SeqCst)
        );
        assert!(entered.load(Ordering::SeqCst) < weights.len());
    }

    #[test]
    fn failed_output_still_delivers_every_admitted_failure_and_retains_first_error() {
        let admitted = (Mutex::new(0), Condvar::new());
        let mut failures = Vec::new();
        let error = run(
            Scheduling::new(2, Some(2), &[1, 1]).unwrap(),
            &[1, 1],
            |index| {
                let mut count = admitted.0.lock().unwrap();
                *count += 1;
                admitted.1.notify_all();
                let (_count, timeout) = admitted
                    .1
                    .wait_timeout_while(count, Duration::from_secs(5), |count| *count < 2)
                    .unwrap();
                assert!(!timeout.timed_out(), "both owned tasks must be admitted");
                Err(Error::Run(format!("owned failure {index}")))
            },
            |event| {
                if let Event::Finished(index, result) = event {
                    failures.push((index, result.unwrap_err().to_string()));
                    return Err(Error::Io(format!("output failure {index}")));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(failures.len(), 2);
        assert!(
            error
                .to_string()
                .contains(&format!("output failure {}", failures[0].0))
        );
        failures.sort();
        assert_eq!(
            failures,
            vec![(0, "owned failure 0".into()), (1, "owned failure 1".into())]
        );
    }

    #[test]
    fn admission_is_validated_before_spawning_and_defaults_preserve_replica_choices() {
        let defaults = Scheduling::new(1, None, &[8, 1]).unwrap();
        assert_eq!(defaults.jobs, 1);
        assert_eq!(defaults.max_active_attempts, 8);
        assert_eq!(
            Scheduling::new(4, None, &[1]).unwrap().max_active_attempts,
            4
        );
        for (jobs, budget, weights) in [
            (0, None, vec![1]),
            (33, None, vec![1]),
            (1, Some(0), vec![1]),
            (1, Some(65), vec![1]),
            (2, Some(1), vec![2]),
            (1, None, vec![0]),
        ] {
            let limits = Scheduling {
                jobs,
                max_active_attempts: budget.unwrap_or(1),
            };
            assert!(
                run(
                    limits,
                    &weights,
                    |_| panic!("must not start invalid work"),
                    |_| Ok(())
                )
                .is_err()
            );
        }
    }

    #[test]
    fn bounded_failure_budget_stops_new_claims_after_the_limit() {
        let failed = AtomicUsize::new(0);
        let mut started = Vec::new();
        run_with_resources_stopping_on(
            Scheduling::new(1, Some(1), &[1; 4]).unwrap(),
            &[1; 4],
            None,
            &[crate::planning::Resources::default(); 4],
            |index| Ok(index < 2),
            |success| {
                success.is_ok_and(|success| !success)
                    && failed.fetch_add(1, Ordering::SeqCst) + 1 > 1
            },
            |event| {
                if let Event::Started(index) = event {
                    started.push(index);
                }
                Ok(())
            },
        )
        .unwrap();
        // One failure is tolerated; the second is observed and prevents only
        // subsequent claims.
        assert_eq!(started, vec![0, 1, 2, 3]);
        assert_eq!(failed.load(Ordering::SeqCst), 2);
    }
}
