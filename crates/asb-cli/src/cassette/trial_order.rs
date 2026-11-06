//! Reproducible sampling without replacement for workload-trial preparation.
use super::Error;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// A reproducible candidate order, including spare trials for failed replays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialOrder {
    candidates: Vec<String>,
    target: usize,
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
}
