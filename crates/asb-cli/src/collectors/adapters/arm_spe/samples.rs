//! Bounded synthesis reader. Counts are sampled observations, never estimated
//! retired instructions, total memory operations or DRAM service latency.
use super::quality::Quality;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};

const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BINS: usize = 4096;
const EVENTS: &[&str] = &[
    "l1d-access",
    "l1d-miss",
    "llc-access",
    "llc-miss",
    "tlb-access",
    "tlb-miss",
    "branch-miss",
    "remote-access",
    "memory",
    "instructions",
    "branches",
];

#[derive(Default, Serialize)]
pub(super) struct Decoded {
    pub memory_latency_weight_sum: String,
    pub memory_latency_known_samples: u64,
    pub counts: BTreeMap<String, u64>,
    pub observed_tasks: BTreeSet<(u32, u32)>,
    pub first_sample_ns: Option<u64>,
    pub last_sample_ns: Option<u64>,
    pub metrics: BTreeMap<String, f64>,
    pub series: BTreeMap<String, Vec<f64>>,
    pub timestamps: BTreeMap<String, Vec<f64>>,
}

fn timestamp(text: &str) -> Result<u64, String> {
    let (seconds, fraction) = text
        .trim_end_matches(':')
        .split_once('.')
        .ok_or("SPE sample timestamp is invalid")?;
    if fraction.len() != 6 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return Err("SPE decoder must report six-digit microsecond timestamps".into());
    }
    seconds
        .parse::<u64>()
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(fraction.parse::<u64>().ok()?.checked_mul(1000)?))
        .ok_or("SPE sample timestamp overflows".into())
}

pub(super) fn parse(reader: impl Read, quality: &Quality) -> Result<Decoded, String> {
    let mut result = Decoded::default();
    let mut bytes = 0;
    let mut bins = BTreeMap::<u64, BTreeMap<String, (u64, u128, u64)>>::new();
    let mut memory_weight = 0u128;
    for line in std::io::BufReader::new(reader.take(MAX_DECODE_BYTES + 1)).lines() {
        let line = line.map_err(|_| "SPE decoded text is unreadable")?;
        bytes += line.len() as u64 + 1;
        if bytes > MAX_DECODE_BYTES || line.len() > 4096 {
            return Err("SPE decoded output exceeds its bound".into());
        }
        if line.trim().is_empty() {
            continue;
        }
        // Task metadata is independently validated in raw perf records. It can
        // contain arbitrary comm names and must never become a numeric sample.
        if line.contains("PERF_RECORD_") {
            continue;
        }
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 4 {
            return Err("Unexpected SPE decoded sample layout".into());
        }
        let (pid, tid) = parts[0]
            .split_once('/')
            .ok_or("SPE sample has no task identity")?;
        let pid = pid
            .parse::<u32>()
            .map_err(|_| "SPE sample PID is invalid")?;
        let tid = tid
            .parse::<u32>()
            .map_err(|_| "SPE sample TID is invalid")?;
        let time = timestamp(parts[1])?;
        let task = quality
            .tasks
            .get(&tid)
            .filter(|task| task.pid == pid)
            .ok_or("SPE sample belongs to an unrelated or unresolved task")?;
        // perf script rounds down to microseconds; accept only that disclosed
        // sub-microsecond boundary uncertainty, not an arbitrary grace period.
        if time.saturating_add(999) < task.start_ns || time > task.end_ns {
            return Err("SPE sample lies outside its recorded task generation".into());
        }
        let event = parts[2]
            .strip_suffix(':')
            .filter(|e| EVENTS.contains(e))
            .ok_or("Unsupported SPE synthesized event")?;
        let weight = parts[3]
            .parse::<u64>()
            .map_err(|_| "SPE sample latency weight is invalid")?;
        let first = *result.first_sample_ns.get_or_insert(time);
        if time < first || result.last_sample_ns.is_some_and(|last| time < last) {
            return Err("SPE samples are not in timestamp order".into());
        }
        result.last_sample_ns = Some(time);
        result.observed_tasks.insert((pid, tid));
        let event = event.replace('-', "_");
        *result.counts.entry(event.clone()).or_default() += 1;
        if event == "memory" {
            memory_weight += weight as u128;
            if weight > 0 {
                result.memory_latency_known_samples += 1;
            }
        }
        let bin = (time - first) / 1_000_000_000;
        if !bins.contains_key(&bin) && bins.len() == MAX_BINS {
            return Err("SPE series exceeds its interval bound".into());
        }
        let values = bins.entry(bin).or_default().entry(event).or_default();
        values.0 += 1;
        values.1 += weight as u128;
        values.2 += u64::from(weight > 0);
    }
    if result.counts.is_empty() {
        return Err("No scoped SPE samples were decoded".into());
    }
    for (event, count) in &result.counts {
        result
            .metrics
            .insert(format!("sampled_{event}_count"), *count as f64);
    }
    if let Some(count) = result.counts.get("memory").filter(|count| **count > 0)
        && result.memory_latency_known_samples == *count
    {
        result.metrics.insert(
            "sampled_memory_latency_cycles_mean".into(),
            memory_weight as f64 / *count as f64,
        );
    }
    result.memory_latency_weight_sum = memory_weight.to_string();
    for (bin, events) in bins {
        for (event, (count, weight, known)) in events {
            let key = format!("arm-spe.sampled_{event}_count");
            result
                .series
                .entry(key.clone())
                .or_default()
                .push(count as f64);
            result
                .timestamps
                .entry(key)
                .or_default()
                .push(bin as f64 + 1.0);
            if event == "memory" && known == count {
                let key = "arm-spe.sampled_memory_latency_cycles_mean".to_owned();
                result
                    .series
                    .entry(key.clone())
                    .or_default()
                    .push(weight as f64 / count as f64);
                result
                    .timestamps
                    .entry(key)
                    .or_default()
                    .push(bin as f64 + 1.0);
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn quality() -> Quality {
        Quality {
            tasks: BTreeMap::from([(
                42,
                super::super::quality::Lifetime {
                    pid: 42,
                    start_ns: 1_000_000_000,
                    end_ns: 4_000_000_000,
                },
            )]),
            aux_records: 1,
        }
    }
    #[test]
    fn samples_keep_latency_cycles_counts_actual_gaps_and_task_scope() {
        let result=parse("0/0 0.000000: PERF_RECORD_COMM: ignore me\n42/42 1.100000: memory: 8\n42/42 1.200000: memory: 12\n42/42 3.200000: memory: 22\n".as_bytes(),&quality()).unwrap();
        assert_eq!(result.metrics["sampled_memory_count"], 3.0);
        assert_eq!(result.metrics["sampled_memory_latency_cycles_mean"], 14.0);
        assert_eq!(
            result.timestamps["arm-spe.sampled_memory_count"],
            vec![1.0, 3.0]
        );
        assert_eq!(
            result.series["arm-spe.sampled_memory_latency_cycles_mean"],
            vec![10.0, 22.0]
        );
    }
    #[test]
    fn foreign_reused_outside_window_or_malformed_samples_are_not_ignored() {
        for text in [
            "99/99 1.100000: memory: 8",
            "42/99 1.100000: memory: 8",
            "42/42 0.100000: memory: 8",
            "42/42 5.100000: memory: 8",
            "42/42 1.100000: memory: -8",
            "42/42 1.100000: unknown: 8",
            "42/42 1.100000: memory:",
            "42/42 1.1: memory: 8",
            "",
            "42/42 2.100000: memory: 8\n42/42 1.100000: memory: 8",
        ] {
            assert!(parse(text.as_bytes(), &quality()).is_err(), "{text}");
        }
    }
    #[test]
    fn missing_latency_weights_do_not_become_zero_latency() {
        let result = parse(
            "42/42 1.100000: memory: 0\n42/42 1.200000: memory: 12\n".as_bytes(),
            &quality(),
        )
        .unwrap();
        assert_eq!(result.counts["memory"], 2);
        assert_eq!(result.memory_latency_known_samples, 1);
        assert!(
            !result
                .metrics
                .contains_key("sampled_memory_latency_cycles_mean")
        );
        assert!(
            !result
                .series
                .contains_key("arm-spe.sampled_memory_latency_cycles_mean")
        );
    }
}
