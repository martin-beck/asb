//! Reproducible sampling without replacement for workload-trial preparation.
use super::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// A reproducible candidate order, including spare trials for failed replays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialOrder {
    candidates: Vec<String>,
    target: usize,
}

/// A bounded, resumable per-combination qualification ledger.
///
/// The owner must verify and durably publish the referenced cassette before
/// marking a replay successful; this ledger does not itself verify archives.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialPreparation {
    target: usize,
    candidates: Vec<String>,
    outcomes: Vec<TrialOutcome>,
    in_flight: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TrialOutcome {
    trial: String,
    /// A content digest of an independently verified cassette, when qualified.
    cassette_sha256: Option<String>,
}

impl TrialPreparation {
    /// Begin qualification with a precomputed complete candidate order.
    pub fn new(order: TrialOrder) -> Self {
        Self {
            target: order.target,
            candidates: order.candidates,
            outcomes: Vec::new(),
            in_flight: None,
        }
    }

    /// Validate loaded state before it can select or reuse any recording.
    pub fn validate(&self) -> Result<(), Error> {
        if self.target == 0
            || self.target > self.candidates.len()
            || self.candidates.len() > 65_536
            || self.outcomes.len() > self.candidates.len()
        {
            return Err(Error(
                "invalid trial preparation population or progress".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut bytes = 0usize;
        for id in &self.candidates {
            bytes = bytes
                .checked_add(id.len())
                .filter(|n| *n <= 16 * 1024 * 1024)
                .ok_or_else(|| Error("trial preparation identifiers exceed 16 MiB".into()))?;
            if id.is_empty()
                || id.len() > 4096
                || id.chars().any(char::is_control)
                || !seen.insert(id)
            {
                return Err(Error(
                    "invalid or duplicate trial preparation candidate".into(),
                ));
            }
        }
        for (index, outcome) in self.outcomes.iter().enumerate() {
            if outcome.trial != self.candidates[index]
                || outcome.cassette_sha256.as_ref().is_some_and(|digest| {
                    digest.len() != 64
                        || !digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            {
                return Err(Error(
                    "invalid trial preparation outcome or cassette digest".into(),
                ));
            }
        }
        let successes = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.cassette_sha256.is_some())
            .count();
        if successes > self.target
            || (successes == self.target
                && self
                    .outcomes
                    .last()
                    .is_some_and(|outcome| outcome.cassette_sha256.is_none()))
        {
            return Err(Error(
                "trial preparation contains outcomes after completion".into(),
            ));
        }
        if let Some(trial) = &self.in_flight
            && (successes == self.target || self.candidates.get(self.outcomes.len()) != Some(trial))
        {
            return Err(Error(
                "trial preparation has invalid in-flight ownership".into(),
            ));
        }
        Ok(())
    }

    /// Next candidate, or none once the target is met or the pool is exhausted.
    pub fn next(&self) -> Result<Option<&str>, Error> {
        self.validate()?;
        if self.in_flight.is_some() {
            return Err(Error("trial preparation has an unsettled attempt; inspect its retained evidence before continuing".into()));
        }
        if self.successful().len() == self.target {
            return Ok(None);
        }
        Ok(self.candidates.get(self.outcomes.len()).map(String::as_str))
    }

    /// Return a previously started candidate that needs explicit settlement.
    pub fn in_flight(&self) -> Result<Option<&str>, Error> {
        self.validate()?;
        Ok(self.in_flight.as_deref())
    }

    /// Mark the next candidate as in-flight before starting its recording.
    /// The owner must persist this transition before starting any process.
    pub fn start(&mut self) -> Result<Option<String>, Error> {
        let next = self.next()?.map(str::to_owned);
        self.in_flight = next.clone();
        Ok(next)
    }

    /// Record one outcome after the in-flight attempt has settled.
    /// A digest means recording and offline replay both succeeded; `None`
    /// marks a failed candidate and advances to a different trial.
    pub fn settle(&mut self, trial: &str, cassette_sha256: Option<String>) -> Result<(), Error> {
        self.validate()?;
        if self.in_flight.as_deref() != Some(trial) {
            return Err(Error(
                "trial outcome is not the next preparation candidate".into(),
            ));
        }
        let outcome = TrialOutcome {
            trial: trial.into(),
            cassette_sha256,
        };
        self.in_flight = None;
        self.outcomes.push(outcome);
        if let Err(error) = self.validate() {
            self.outcomes.pop();
            self.in_flight = Some(trial.into());
            return Err(error);
        }
        Ok(())
    }

    /// Verified-cassette identities in their original sampled order.
    pub fn successful(&self) -> Vec<(&str, &str)> {
        self.outcomes
            .iter()
            .filter_map(|outcome| {
                outcome
                    .cassette_sha256
                    .as_deref()
                    .map(|digest| (outcome.trial.as_str(), digest))
            })
            .collect()
    }

    /// Whether the requested number of successful trials was reached.
    pub fn complete(&self) -> Result<bool, Error> {
        self.validate()?;
        Ok(self.successful().len() == self.target)
    }
}

impl TrialOrder {
    /// Shuffle a bounded catalog without replacement using the supplied seed.
    ///
    /// Catalog ordering does not affect the result. Duplicate or malformed IDs
    /// and impossible targets are rejected rather than silently narrowed.
    pub fn new(ids: &[String], target: usize, seed: u64) -> Result<Self, Error> {
        if ids.is_empty() || ids.len() > 65_536 || target == 0 || target > ids.len() {
            return Err(Error(
                "trial sample must be between one and the bounded catalog size".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        let mut bytes = 0usize;
        for id in ids {
            bytes = bytes
                .checked_add(id.len())
                .filter(|total| *total <= 16 * 1024 * 1024)
                .ok_or_else(|| Error("trial catalog identifiers exceed 16 MiB".into()))?;
            if id.is_empty()
                || id.len() > 4096
                || id.chars().any(char::is_control)
                || !unique.insert(id.clone())
            {
                return Err(Error(
                    "trial catalog contains a malformed or duplicate identifier".into(),
                ));
            }
        }
        let mut candidates: Vec<_> = unique.into_iter().collect();
        let mut counter = 0u64;
        for end in (1..candidates.len()).rev() {
            let width = (end + 1) as u64;
            let threshold = width.wrapping_neg() % width;
            let index = loop {
                let mut hash = Sha256::new();
                hash.update(b"asb-trial-shuffle-sha256-v1\0");
                hash.update(seed.to_le_bytes());
                hash.update(counter.to_le_bytes());
                counter = counter
                    .checked_add(1)
                    .ok_or_else(|| Error("trial selection counter exhausted".into()))?;
                let digest = hash.finalize();
                let draw =
                    u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes"));
                if draw >= threshold {
                    break (draw % width) as usize;
                }
            };
            candidates.swap(end, index);
        }
        Ok(Self { candidates, target })
    }

    /// First candidates to prepare initially.
    pub fn initial(&self) -> &[String] {
        &self.candidates[..self.target]
    }

    /// Complete ordered candidate pool, including fallback trials.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_order_has_unique_initial_and_replacement_candidates() {
        let ids: Vec<_> = (0..100).map(|n| format!("trial-{n:03}")).collect();
        let order = TrialOrder::new(&ids, 7, 42).unwrap();
        assert_eq!(
            order.initial(),
            &[
                "trial-057",
                "trial-095",
                "trial-094",
                "trial-065",
                "trial-031",
                "trial-093",
                "trial-070"
            ]
        );
        let mut reversed = ids.clone();
        reversed.reverse();
        assert_eq!(order, TrialOrder::new(&reversed, 7, 42).unwrap());
        assert_eq!(order.initial().len(), 7);
        assert_eq!(
            order.candidates().iter().collect::<BTreeSet<_>>().len(),
            100
        );
        assert_ne!(order, TrialOrder::new(&ids, 7, 43).unwrap());
    }

    #[test]
    fn rejects_impossible_and_invalid_catalogs() {
        let ids = vec!["one".into(), "two".into()];
        assert!(TrialOrder::new(&ids, 0, 1).is_err());
        assert!(TrialOrder::new(&ids, 3, 1).is_err());
        assert!(TrialOrder::new(&["one".into(), "one".into()], 1, 1).is_err());
        assert!(TrialOrder::new(&["bad\nid".into()], 1, 1).is_err());
    }

    #[test]
    fn failed_replay_uses_a_distinct_replacement_and_recovers_successes() {
        let ids = (0..4).map(|n| format!("trial-{n}")).collect::<Vec<_>>();
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 2, 4).unwrap());
        let first = state.start().unwrap().unwrap();
        state.settle(&first, None).unwrap();
        let second = state.start().unwrap().unwrap();
        assert_ne!(first, second);
        state.settle(&second, Some("a".repeat(64))).unwrap();
        let serialized = serde_json::to_string(&state).unwrap();
        let mut restored: TrialPreparation = serde_json::from_str(&serialized).unwrap();
        let third = restored.start().unwrap().unwrap();
        assert_ne!(first, third);
        assert_ne!(second, third);
        restored.settle(&third, Some("b".repeat(64))).unwrap();
        assert!(restored.complete().unwrap());
        assert_eq!(restored.next().unwrap(), None);
        assert_eq!(
            restored.successful(),
            vec![
                (second.as_str(), "a".repeat(64).as_str()),
                (third.as_str(), "b".repeat(64).as_str())
            ]
        );
        assert!(restored.settle(&third, None).is_err());
    }

    #[test]
    fn exhausted_pool_and_tampered_receipts_do_not_qualify() {
        let ids = vec!["a".into(), "b".into()];
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 2, 9).unwrap());
        let first = state.start().unwrap().unwrap();
        assert!(state.settle(&first, Some("bad-digest".into())).is_err());
        assert!(state.next().is_err());
        state.settle(&first, None).unwrap();
        let second = state.start().unwrap().unwrap();
        state.settle(&second, None).unwrap();
        assert_eq!(state.next().unwrap(), None);
        assert!(!state.complete().unwrap());
        let mut bad = state.clone();
        bad.outcomes[0].trial = "unrelated".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn interrupted_attempt_cannot_silently_start_another_trial() {
        let ids = vec!["a".into(), "b".into()];
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 1, 3).unwrap());
        let started = state.start().unwrap().unwrap();
        let encoded = serde_json::to_vec(&state).unwrap();
        let mut restored: TrialPreparation = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.start().is_err());
        assert_eq!(restored.in_flight().unwrap(), Some(started.as_str()));
        assert!(!restored.complete().unwrap());
        restored.settle(&started, None).unwrap();
        assert_ne!(restored.start().unwrap().unwrap(), started);
    }
}
