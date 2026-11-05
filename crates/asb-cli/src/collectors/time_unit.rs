//! Declared collector timestamp units, including adapter-owned legacy defaults.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TimeUnit {
    #[default]
    Milliseconds,
    Seconds,
}
impl TimeUnit {
    pub(crate) fn milliseconds(self, value: f64) -> Option<f64> {
        let scaled = match self {
            Self::Milliseconds => value,
            Self::Seconds => value * 1000.0,
        };
        (value >= 0.0 && scaled.is_finite()).then_some(scaled)
    }
}
pub(crate) fn resolve(name: &str, value: Option<&serde_json::Value>) -> Result<TimeUnit, String> {
    match value {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| "series_timestamp_unit must be seconds or milliseconds".into()),
        None => Ok(super::find(name)
            .map(|adapter| adapter.series_timestamp_unit())
            .unwrap_or_default()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn explicit_units_legacy_defaults_and_overflow_are_distinct() {
        assert_eq!(resolve("arm-spe", None).unwrap(), TimeUnit::Seconds);
        assert_eq!(resolve("kernel-pressure", None).unwrap(), TimeUnit::Seconds);
        assert_eq!(resolve("perf", None).unwrap(), TimeUnit::Milliseconds);
        assert_eq!(resolve("unknown", None).unwrap(), TimeUnit::Milliseconds);
        assert_eq!(
            resolve("arm-spe", Some(&json!("milliseconds"))).unwrap(),
            TimeUnit::Milliseconds
        );
        assert!((TimeUnit::Seconds.milliseconds(2.20768677).unwrap() - 2207.68677).abs() < 1e-9);
        assert_eq!(TimeUnit::Milliseconds.milliseconds(0.0), Some(0.0));
        assert!(TimeUnit::Seconds.milliseconds(f64::MAX).is_none());
        assert!(TimeUnit::Seconds.milliseconds(-1.0).is_none());
        for value in [json!("minutes"), json!(null), json!(42)] {
            assert!(resolve("arm-spe", Some(&value)).is_err());
        }
    }
}
