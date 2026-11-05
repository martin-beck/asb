//! Clock-qualified offsets into the actual native principal's execution envelope.
//! No fitted clock conversion, inferred phase classification or host sampling.
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
pub(crate) struct Alignment {
    pub status: &'static str,
    pub unavailable_reason: Option<&'static str>,
    pub first_sample_offset_seconds: Option<f64>,
    last_sample_offset_seconds: Option<f64>,
    principal_phase_upper_seconds: Option<f64>,
    source: &'static str,
    clock: &'static str,
    clock_domain: Option<String>,
    evidence: [&'static str; 2],
    limitations: &'static str,
}

pub(crate) fn spe(quality: &Value, window: &Value) -> Alignment {
    let mut result = Alignment {
        status: "unavailable",
        unavailable_reason: None,
        first_sample_offset_seconds: None,
        last_sample_offset_seconds: None,
        principal_phase_upper_seconds: None,
        source: "arm-spe",
        clock: "CLOCK_MONOTONIC_RAW",
        clock_domain: None,
        evidence: ["arm-spe-quality.json", "execution-window.json"],
        limitations: "Offsets start at the observed pre-release bound of the verified native principal, not its first instruction. The enclosing phase includes observation overhead and sleeping. SPE counts retain sampling bias and microsecond timestamp precision. Descendants may outlive the principal; samples outside its envelope cannot be aligned here. These offsets do not classify LLM, tool or CPU phases, and do not convert between raw and adjusted monotonic clocks.",
    };
    match offsets(quality, window) {
        Ok((first, last, span, domain)) => {
            result.status = "observed";
            result.first_sample_offset_seconds = Some(first);
            result.last_sample_offset_seconds = Some(last);
            result.principal_phase_upper_seconds = Some(span);
            result.clock_domain = Some(domain.to_owned());
        }
        Err(reason) => result.unavailable_reason = Some(reason),
    }
    result
}

fn offsets<'a>(
    quality: &Value,
    window: &'a Value,
) -> Result<(f64, f64, f64, &'a str), &'static str> {
    if quality["schema_version"] != 1
        || quality["status"] != "observed"
        || quality["scope"] != "agent-process-tree"
        || quality["clock"] != "CLOCK_MONOTONIC_RAW"
    {
        return Err(
            "requires validated task-scoped SPE with an explicitly selected raw monotonic clock",
        );
    }
    let root = &quality["root"];
    if root["pid"]
        .as_u64()
        .is_none_or(|pid| pid == 0 || pid > u32::MAX as u64)
        || root["start_time_ticks"]
            .as_u64()
            .is_none_or(|ticks| ticks == 0)
        || root != &window["root"]
    {
        return Err("SPE and execution window must identify the same process generation");
    }
    let domain = window["clock_domain"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("execution clock domain is missing or invalid")?;
    let raw = &window["raw_clock"];
    if window["schema_version"] != 1
        || window["status"] != "observed"
        || window["scope"] != "verified-native-agent-principal-lifetime"
        || window["clock"] != "CLOCK_MONOTONIC"
        || raw["clock"] != "CLOCK_MONOTONIC_RAW"
        || raw["status"] != "observed"
    {
        return Err("requires a completed verified native execution window with raw clock bounds");
    }
    let start = raw["release_bound_ns"]
        .as_u64()
        .ok_or("raw release bound is unavailable")?;
    let end = raw["exit_bound_ns"]
        .as_u64()
        .filter(|end| *end > start)
        .ok_or("raw exit bound is invalid")?;
    let first = quality["first_sample_ns"]
        .as_u64()
        .ok_or("first sample timestamp is unavailable")?;
    let last = quality["last_sample_ns"]
        .as_u64()
        .filter(|last| *last >= first)
        .ok_or("sample timestamps are unordered")?;
    // perf script rounds down to microseconds. No guessed clock offset or
    // arbitrary grace window is allowed. Only that precision may straddle start.
    if first.saturating_add(999) < start || last > end {
        return Err("SPE samples lie outside the raw-clock principal execution envelope");
    }
    Ok((
        first.saturating_sub(start) as f64 / 1e9,
        last.saturating_sub(start) as f64 / 1e9,
        (end - start) as f64 / 1e9,
        domain,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn documents() -> (Value, Value) {
        let root = json!({"pid":42,"start_time_ticks":7});
        (
            json!({"schema_version":1,"status":"observed","scope":"agent-process-tree","clock":"CLOCK_MONOTONIC_RAW","root":root,"first_sample_ns":1_500_000_000_u64,"last_sample_ns":2_000_000_000_u64}),
            json!({"schema_version":1,"status":"observed","scope":"verified-native-agent-principal-lifetime","clock":"CLOCK_MONOTONIC","clock_domain":"a".repeat(64),"root":root,"raw_clock":{"status":"observed","clock":"CLOCK_MONOTONIC_RAW","release_bound_ns":1_000_000_000_u64,"exit_bound_ns":3_000_000_000_u64}}),
        )
    }
    #[test]
    fn offsets_use_the_explicit_clock_and_subtract_integers_before_conversion() {
        let (mut q, mut w) = documents();
        let result = spe(&q, &w);
        assert_eq!(result.status, "observed");
        assert_eq!(result.first_sample_offset_seconds, Some(0.5));
        assert_eq!(result.last_sample_offset_seconds, Some(1.0));
        assert_eq!(result.principal_phase_upper_seconds, Some(2.0));
        let base = u64::MAX - 10_000;
        w["raw_clock"]["release_bound_ns"] = json!(base);
        w["raw_clock"]["exit_bound_ns"] = json!(base + 5_000);
        q["first_sample_ns"] = json!(base + 1_000);
        q["last_sample_ns"] = json!(base + 2_000);
        assert_eq!(spe(&q, &w).first_sample_offset_seconds, Some(0.000001));
    }
    #[test]
    fn missing_foreign_failed_or_wrong_clock_evidence_never_gets_offsets() {
        for (which, pointer, value) in [
            (true, "/clock", json!("perf default recording clock")),
            (true, "/root/pid", json!(43)),
            (true, "/root/start_time_ticks", json!(8)),
            (true, "/root/pid", Value::Null),
            (true, "/scope", json!("host")),
            (true, "/status", json!("failed")),
            (true, "/first_sample_ns", json!(900_000_000)),
            (true, "/last_sample_ns", json!(3_000_000_001_u64)),
            (true, "/last_sample_ns", json!(1)),
            (true, "/first_sample_ns", Value::Null),
            (false, "/clock_domain", json!("unverified")),
            (false, "/status", json!("unavailable")),
            (false, "/scope", json!("container")),
            (false, "/raw_clock/status", json!("unavailable")),
            (false, "/raw_clock/clock", json!("CLOCK_MONOTONIC")),
            (false, "/raw_clock/release_bound_ns", Value::Null),
            (false, "/raw_clock/exit_bound_ns", json!(0)),
        ] {
            let (mut q, mut w) = documents();
            *(if which { &mut q } else { &mut w })
                .pointer_mut(pointer)
                .unwrap() = value;
            let result = spe(&q, &w);
            assert_eq!(result.status, "unavailable", "{pointer}");
            assert!(result.first_sample_offset_seconds.is_none());
            assert!(result.unavailable_reason.is_some());
        }
    }
    #[test]
    fn only_decoder_rounding_may_cross_the_pre_release_bound() {
        let (mut q, w) = documents();
        q["first_sample_ns"] = json!(999_999_001);
        assert_eq!(spe(&q, &w).first_sample_offset_seconds, Some(0.0));
        q["first_sample_ns"] = json!(999_999_000);
        assert_eq!(spe(&q, &w).status, "unavailable");
    }
}
