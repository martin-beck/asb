//! Paired observations and explicitly bounded visualization, never state inference.
use super::*;
pub(super) const MAX_RENDERED_POINTS: usize = 2048;
#[derive(Debug, Clone, Serialize)]
pub(super) struct TimeSeries {
    pub values: Vec<Option<f64>>,
    pub timestamps_ms: Option<Vec<f64>>,
}
impl TimeSeries {
    pub(super) fn parse(
        values: &Value,
        timestamps: Option<&Value>,
        path: &Path,
        metric: &str,
        unit: crate::collectors::TimeUnit,
    ) -> Result<Self, Error> {
        let invalid = |reason: &str| {
            Error::Input(format!(
                "invalid series `{metric}` in {}: {reason}",
                path.display()
            ))
        };
        let array = values
            .as_array()
            .ok_or_else(|| invalid("values must be an array of numbers or null gaps"))?;
        let values = array
            .iter()
            .map(|value| {
                if value.is_null() {
                    Ok(None)
                } else {
                    value
                        .as_f64()
                        .filter(|value| value.is_finite())
                        .map(Some)
                        .ok_or_else(|| invalid("values must be finite numbers or null gaps"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let timestamps_ms = timestamps
            .map(|timestamps| {
                let array = timestamps
                    .as_array()
                    .ok_or_else(|| invalid("timestamps must be an aligned array"))?;
                if array.len() != values.len() {
                    return Err(invalid("timestamp and value lengths differ"));
                }
                let timestamps = array
                    .iter()
                    .map(|value| {
                        value
                            .as_f64()
                            .and_then(|value| unit.milliseconds(value))
                            .ok_or_else(|| {
                                invalid("timestamps must be finite nonnegative values convertible to milliseconds")
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if timestamps.windows(2).any(|pair| pair[1] < pair[0]) {
                    return Err(invalid("timestamps must be nondecreasing"));
                }
                Ok(timestamps)
            })
            .transpose()?;
        Ok(Self {
            values,
            timestamps_ms,
        })
    }
    pub(super) fn render_indices(&self) -> Option<Vec<usize>> {
        let observed = self
            .values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value.map(|value| (index, value)))
            .collect::<Vec<_>>();
        if observed.len() <= MAX_RENDERED_POINTS {
            return Some(observed.into_iter().map(|(index, _)| index).collect());
        }
        let mut selected = BTreeSet::new();
        for (offset, (index, _)) in observed.iter().copied().enumerate() {
            if offset == 0 || observed[offset - 1].0 + 1 != index {
                selected.insert(index);
            }
            if offset + 1 == observed.len() || observed[offset + 1].0 != index + 1 {
                selected.insert(index);
            }
        }
        // Even when gap endpoints exhaust the bucket budget, a bounded graph
        // must not conceal a full-source extreme inside one of those segments.
        for extreme in [
            observed
                .iter()
                .min_by(|left, right| left.1.total_cmp(&right.1)),
            observed
                .iter()
                .max_by(|left, right| left.1.total_cmp(&right.1)),
        ]
        .into_iter()
        .flatten()
        {
            selected.insert(extreme.0);
        }
        if selected.len() > MAX_RENDERED_POINTS {
            return None;
        }
        let buckets = (MAX_RENDERED_POINTS - selected.len()) / 2;
        if buckets > 0 {
            let width = self.values.len().div_ceil(buckets);
            for chunk in observed.chunk_by(|left, right| left.0 / width == right.0 / width) {
                if let Some((index, _)) = chunk
                    .iter()
                    .min_by(|left, right| left.1.total_cmp(&right.1))
                {
                    selected.insert(*index);
                }
                if let Some((index, _)) = chunk
                    .iter()
                    .max_by(|left, right| left.1.total_cmp(&right.1))
                {
                    selected.insert(*index);
                }
            }
        }
        Some(selected.into_iter().collect())
    }
    pub(super) fn rendering_metadata(&self) -> Value {
        let indices = self.render_indices();
        let observed = self.values.iter().flatten().copied();
        let count = observed.clone().count();
        json!({
            "source_points":self.values.len(), "observed_points":count,
            "rendered_points":indices.as_ref().map(Vec::len),
            "axis":if self.timestamps_ms.is_some(){"milliseconds"}else{"sample-index"},
            "policy":"at most 2048 original paired samples; all segment endpoints and per-bucket minima/maxima; gaps never bridged; full aligned data retained in series/series_timestamps",
            "status":if count==0 {"unavailable: no observed values"}else if indices.is_none(){"unavailable: too many gap segments for the bounded graph"}else{"available"},
            "minimum":observed.clone().reduce(f64::min), "maximum":observed.reduce(f64::max)
        })
    }
}
