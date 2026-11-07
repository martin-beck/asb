//! Stable selectors and schedule-independent sampling with replacement.
use super::{Error, Limits, Manifest, Track, digest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;

/// A normalized observation usable for ordering tracks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Canonical total tokens.
    Tokens,
    /// Execution elapsed time in milliseconds.
    Duration,
    /// Inference request count.
    Requests,
}

impl Metric {
    fn value(self, track: &Track) -> Option<u64> {
        match self {
            Self::Tokens => track.statistics.tokens,
            Self::Duration => track.statistics.duration_ms,
            Self::Requests => track.statistics.requests,
        }
    }
}

/// Explicit selection. Random selection is represented by no selector.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Selector {
    /// Stable numeric ID; incomplete tracks are permitted explicitly.
    Id(u64),
    /// Lowest known metric, ties by lowest ID.
    Min(Metric),
    /// Highest known metric, ties by lowest ID.
    Max(Metric),
    /// Lower-middle entry ordered by metric then ID.
    Median(Metric),
}

impl FromStr for Selector {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        if let Ok(id) = value.parse::<u64>() {
            return if id > 0 {
                Ok(Self::Id(id))
            } else {
                Err(Error("track IDs start at 1".into()))
            };
        }
        let invalid = || {
            Error("track must be a positive ID or min|max|median:tokens|duration|requests".into())
        };
        let (operation, metric) = value.split_once(':').ok_or_else(invalid)?;
        let metric = match metric {
            "tokens" => Metric::Tokens,
            "duration" => Metric::Duration,
            "requests" => Metric::Requests,
            _ => return Err(invalid()),
        };
        match operation {
            "min" => Ok(Self::Min(metric)),
            "max" => Ok(Self::Max(metric)),
            "median" => Ok(Self::Median(metric)),
            _ => Err(invalid()),
        }
    }
}

impl Selector {
    /// Resolve once; fixed selection uses the same ID for every execution.
    pub fn resolve(self, manifest: &Manifest) -> Result<u64, Error> {
        manifest.validate(Limits::default())?;
        let (metric, maximum, median) = match self {
            Self::Id(id) => return manifest.track(id).map(|track| track.id),
            Self::Min(metric) => (metric, false, false),
            Self::Max(metric) => (metric, true, false),
            Self::Median(metric) => (metric, false, true),
        };
        let mut eligible: Vec<_> = manifest
            .tracks
            .iter()
            .filter(|track| track.automatically_selectable())
            .filter_map(|track| metric.value(track).map(|value| (value, track.id)))
            .collect();
        eligible.sort_unstable();
        if eligible.is_empty() {
            return Err(Error(
                "no complete nonempty cassette track has the requested statistic".into(),
            ));
        }
        if median {
            return Ok(eligible[(eligible.len() - 1) / 2].1);
        }
        let value = if maximum {
            eligible[eligible.len() - 1].0
        } else {
            eligible[0].0
        };
        Ok(eligible
            .iter()
            .find(|entry| entry.0 == value)
            .expect("chosen statistic exists")
            .1)
    }
}

/// Persistable choices in execution order; scheduling must not resample them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionPlan {
    /// Versioned portable algorithm identifier, not a security mechanism.
    pub algorithm: String,
    /// Disclosed seed, including when generated automatically by a caller.
    pub seed: u64,
    /// Digest of the canonical source manifest, independent of extraction paths.
    pub manifest_sha256: String,
    /// Requested fixed/statistic selector, or random sampling when absent.
    pub selector: Option<Selector>,
    /// Batch/replica-ordered track IDs, with replacement.
    pub tracks: Vec<u64>,
}

impl SelectionPlan {
    /// Precompute bounded choices before any executions start (at most 1,000,000).
    ///
    /// `sha256-counter-rejection-v1` hashes its domain, little-endian seed and
    /// counter, reads the first little-endian u64, and uses rejection sampling.
    /// A fixed algorithm avoids platform- or library-version-dependent sampling.
    pub fn build(
        manifest: &Manifest,
        repeat: u64,
        concurrency: u64,
        seed: u64,
        selector: Option<Selector>,
        max_executions: usize,
    ) -> Result<Self, Error> {
        manifest.validate(Limits::default())?;
        let count = repeat
            .checked_mul(concurrency)
            .filter(|_| {
                repeat > 0 && (1..=crate::benchmark::MAX_CONCURRENCY as u64).contains(&concurrency)
            })
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count <= max_executions.min(1_000_000))
            .ok_or_else(|| {
                Error("invalid, overflowing or excessive repeat/concurrency product".into())
            })?;
        let mut tracks = Vec::new();
        tracks
            .try_reserve_exact(count)
            .map_err(|_| Error("cannot allocate bounded cassette selection plan".into()))?;
        if let Some(selector) = selector {
            tracks.resize(count, selector.resolve(manifest)?);
        } else {
            let eligible: Vec<_> = manifest
                .tracks
                .iter()
                .filter(|track| track.automatically_selectable())
                .map(|track| track.id)
                .collect();
            if eligible.is_empty() {
                return Err(Error(
                    "cassette has no complete nonempty tracks for automatic replay".into(),
                ));
            }
            let width = eligible.len() as u64;
            let threshold = width.wrapping_neg() % width;
            let mut counter = 0u64;
            while tracks.len() < count {
                let mut hash = Sha256::new();
                hash.update(b"asb-track-sha256-counter-rejection-v1\0");
                hash.update(seed.to_le_bytes());
                hash.update(counter.to_le_bytes());
                counter = counter
                    .checked_add(1)
                    .ok_or_else(|| Error("selection counter exhausted".into()))?;
                let bytes = hash.finalize();
                let draw =
                    u64::from_le_bytes(bytes[..8].try_into().expect("SHA-256 has eight bytes"));
                if draw >= threshold {
                    tracks.push(eligible[(draw % width) as usize]);
                }
            }
        }
        Ok(Self {
            algorithm: "sha256-counter-rejection-v1".into(),
            seed,
            manifest_sha256: digest(
                &serde_json::to_vec(manifest)
                    .map_err(|error| Error(format!("cannot encode cassette identity: {error}")))?,
            ),
            selector,
            tracks,
        })
    }

    /// Validate a deserialized plan against its source and requested slot count.
    /// Rejects algorithm drift, resampling, changed manifests and invalid IDs.
    pub fn validate(
        &self,
        manifest: &Manifest,
        repeat: u64,
        concurrency: u64,
        max_executions: usize,
    ) -> Result<(), Error> {
        let expected = Self::build(
            manifest,
            repeat,
            concurrency,
            self.seed,
            self.selector,
            max_executions,
        )?;
        if self != &expected {
            return Err(Error("saved cassette selection plan differs from its source, algorithm or execution slots".into()));
        }
        Ok(())
    }
}
