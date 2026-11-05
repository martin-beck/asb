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
        clock: "PERF_TIME_CONV",
        clock_domain: None,
        evidence: ["arm-spe-quality.json", "execution-window.json"],
        limitations: "Offsets start at the observed pre-release bound of the verified native principal, not its first instruction. The enclosing phase includes observation overhead and sleeping. SPE counts retain sampling bias and microsecond timestamp precision. Descendants may outlive the principal; samples outside its envelope cannot be aligned here. These offsets do not classify LLM, tool or CPU phases, and use the capture's recorded timer conversion rather than fitting an offset between unrelated clocks.",
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
        || quality["clock"] != "PERF_TIME_CONV"
    {
        return Err(
            "requires validated task-scoped SPE with its recorded perf hardware timer conversion",
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
    let raw = &window["arm_counter"];
    if window["schema_version"] != 1
        || window["status"] != "observed"
        || window["scope"] != "verified-native-agent-principal-lifetime"
        || window["clock"] != "CLOCK_MONOTONIC"
        || raw["clock"] != "CNTVCT_EL0"
        || raw["status"] != "observed"
    {
        return Err(
            "requires a completed verified native execution window with architectural counter bounds",
        );
    }
    let conversion: TimeConversion =
        serde_json::from_value(quality["raw_record_quality"]["time_conversion"].clone())
            .map_err(|_| "perf timer conversion metadata is missing or malformed")?;
    let start_ticks = raw["release_bound_ticks"]
        .as_u64()
        .ok_or("counter release bound is unavailable")?;
    let end_ticks = raw["exit_bound_ticks"]
        .as_u64()
        .filter(|end| *end > start_ticks)
        .ok_or("counter exit bound is invalid")?;
    let start = conversion.nanoseconds(start_ticks)?;
    let end = conversion.nanoseconds(end_ticks)?;
    if end <= start {
        return Err("converted execution envelope is not ordered");
    }
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
        return Err("SPE samples lie outside the converted principal execution envelope");
    }
    Ok((
        first.saturating_sub(start) as f64 / 1e9,
        last.saturating_sub(start) as f64 / 1e9,
        (end - start) as f64 / 1e9,
        domain,
    ))
}

/// Exact perf TIME_CONV fields. Conversion follows Linux tools/perf/util/tsc.c.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct TimeConversion {
    pub time_shift: u64,
    pub time_mult: u64,
    pub time_zero: u64,
    pub time_cycles: u64,
    pub time_mask: u64,
    pub cap_user_time_zero: u8,
    pub cap_user_time_short: u8,
}
impl TimeConversion {
    fn nanoseconds(&self, ticks: u64) -> Result<u64, &'static str> {
        if self.time_shift >= 64
            || self.time_mult == 0
            || self.cap_user_time_zero != 1
            || self.cap_user_time_short > 1
        {
            return Err("unsupported perf timer conversion capability or scale");
        }
        let ticks = if self.cap_user_time_short == 1 {
            let delta = ticks
                .checked_sub(self.time_cycles)
                .filter(|delta| {
                    self.time_mask > 0
                        && (self.time_mask & self.time_mask.wrapping_add(1)) == 0
                        && *delta <= self.time_mask
                })
                .ok_or("counter is before the conversion anchor or has ambiguous wraparound")?;
            self.time_cycles
                .checked_add(delta & self.time_mask)
                .ok_or("counter extension overflow")?
        } else {
            ticks
        };
        let quotient = ticks >> self.time_shift;
        let remainder = ticks & ((1u64 << self.time_shift) - 1);
        // Linux's unsigned time_zero may encode a negative offset. Preserve its
        // defined wrapping addition; use wide intermediate products explicitly.
        let scaled = quotient as u128 * self.time_mult as u128
            + ((remainder as u128 * self.time_mult as u128) >> self.time_shift);
        Ok(self.time_zero.wrapping_add(scaled as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn documents() -> (Value, Value) {
        let root = json!({"pid":42,"start_time_ticks":7});
        (
            json!({"schema_version":1,"status":"observed","scope":"agent-process-tree","clock":"PERF_TIME_CONV","root":root,"raw_record_quality":{"time_conversion":{"time_shift":0,"time_mult":1,"time_zero":0,"time_cycles":0,"time_mask":0,"cap_user_time_zero":1,"cap_user_time_short":0}},"first_sample_ns":1_500_000_000_u64,"last_sample_ns":2_000_000_000_u64}),
            json!({"schema_version":1,"status":"observed","scope":"verified-native-agent-principal-lifetime","clock":"CLOCK_MONOTONIC","clock_domain":"a".repeat(64),"root":root,"arm_counter":{"status":"observed","clock":"CNTVCT_EL0","release_bound_ticks":1_000_000_000_u64,"exit_bound_ticks":3_000_000_000_u64}}),
        )
    }
    #[test]
    fn timer_conversion_matches_perf_unsigned_arithmetic_and_rejects_ambiguity() {
        let mut tc = TimeConversion {
            time_shift: 21,
            time_mult: 20_971_520,
            time_zero: u64::MAX - 4,
            time_cycles: 1000,
            time_mask: 65_535,
            cap_user_time_zero: 1,
            cap_user_time_short: 0,
        };
        assert_eq!(tc.nanoseconds((1 << 21) + 3), Ok(20_971_545));
        tc.time_shift = 0;
        tc.time_mult = 1;
        tc.time_zero = 0;
        tc.cap_user_time_short = 1;
        assert_eq!(tc.nanoseconds(1010), Ok(1010));
        assert!(tc.nanoseconds(999).is_err());
        assert!(tc.nanoseconds(1000 + 65_536).is_err());
        tc.time_mask = 4;
        assert!(tc.nanoseconds(1001).is_err());
        tc.time_mask = 65_535;
        tc.time_mult = 0;
        assert!(tc.nanoseconds(1001).is_err());
        tc.time_mult = 1;
        tc.time_shift = 64;
        assert!(tc.nanoseconds(1001).is_err());
        tc.time_shift = 0;
        tc.cap_user_time_zero = 0;
        assert!(tc.nanoseconds(1001).is_err());
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
        w["arm_counter"]["release_bound_ticks"] = json!(base);
        w["arm_counter"]["exit_bound_ticks"] = json!(base + 5_000);
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
            (false, "/arm_counter/status", json!("unavailable")),
            (false, "/arm_counter/clock", json!("CLOCK_MONOTONIC")),
            (false, "/arm_counter/release_bound_ticks", Value::Null),
            (false, "/arm_counter/exit_bound_ticks", json!(0)),
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
