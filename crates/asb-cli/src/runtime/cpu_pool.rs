//! Ordered, bounded logical-CPU pool planning and target validation.
use std::collections::BTreeSet;

// Keep both range expansion and memory use bounded for untrusted TOML input.
const MAX_CPU_ID: u32 = 1_048_575;
const MAX_POOL_CPUS: usize = 65_536;

/// An ordered list of Linux logical CPU identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuPool {
    cpus: Vec<u32>,
    per_instance: usize,
}

impl CpuPool {
    /// Parse comma-separated logical IDs and inclusive ranges, preserving order.
    /// Duplicate IDs, malformed or oversized inputs, and zero group sizes fail.
    pub fn parse(spec: &str, per_instance: usize) -> Result<Self, String> {
        if per_instance == 0 {
            return Err("num_cpus_per_instance must be greater than zero".into());
        }
        let mut cpus = Vec::new();
        let mut seen = BTreeSet::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err("CPU pool contains an empty item".into());
            }
            let parse_id = |id: &str| -> Result<u32, String> {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("invalid logical CPU identifier `{id}`"));
                }
                let id = id
                    .parse::<u32>()
                    .map_err(|_| "logical CPU identifier is too large")?;
                if id > MAX_CPU_ID {
                    return Err(format!("logical CPU identifier {id} exceeds {MAX_CPU_ID}"));
                }
                Ok(id)
            };
            let (first, last) = match part.split_once('-') {
                Some((first, last)) => (parse_id(first.trim())?, parse_id(last.trim())?),
                None => {
                    let id = parse_id(part)?;
                    (id, id)
                }
            };
            if first > last {
                return Err(format!("CPU range `{part}` is descending"));
            }
            let count = (last - first) as usize + 1;
            if count > MAX_POOL_CPUS.saturating_sub(cpus.len()) {
                return Err(format!("CPU pool exceeds {MAX_POOL_CPUS} logical CPUs"));
            }
            for cpu in first..=last {
                if !seen.insert(cpu) {
                    return Err(format!("CPU pool repeats logical CPU {cpu}"));
                }
                cpus.push(cpu);
            }
        }
        if per_instance > cpus.len() {
            return Err("CPU pool does not contain a complete instance allocation".into());
        }
        Ok(Self { cpus, per_instance })
    }

    /// The maximum number of complete, disjoint instance allocations.
    pub fn capacity(&self) -> usize {
        self.cpus.len() / self.per_instance
    }

    /// Return ordered groups for the requested concurrency; leftovers are unused.
    pub fn allocate(&self, concurrency: usize) -> Result<Vec<Vec<u32>>, String> {
        if concurrency == 0 || concurrency > self.capacity() {
            return Err(format!(
                "concurrency {concurrency} exceeds CPU pool capacity {} or is zero",
                self.capacity()
            ));
        }
        Ok(self
            .cpus
            .chunks_exact(self.per_instance)
            .take(concurrency)
            .map(<[u32]>::to_vec)
            .collect())
    }

    /// Validate all configured CPUs against the execution target's observed sets.
    /// Callers must independently verify the effective affinity after applying it.
    pub fn validate_target(
        &self,
        present: &BTreeSet<u32>,
        online: &BTreeSet<u32>,
        allowed: &BTreeSet<u32>,
    ) -> Result<(), String> {
        for cpu in &self.cpus {
            if !present.contains(cpu) {
                return Err(format!(
                    "logical CPU {cpu} does not exist on the execution target"
                ));
            }
            if !online.contains(cpu) {
                return Err(format!(
                    "logical CPU {cpu} is offline on the execution target"
                ));
            }
            if !allowed.contains(cpu) {
                return Err(format!(
                    "logical CPU {cpu} is outside the execution target's effective CPU set"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn examples_preserve_order_and_reject_over_capacity() {
        let first = CpuPool::parse("1-3,5-7", 3).unwrap();
        assert_eq!(first.capacity(), 2);
        assert_eq!(first.allocate(1).unwrap(), vec![vec![1, 2, 3]]);
        assert_eq!(
            first.allocate(2).unwrap(),
            vec![vec![1, 2, 3], vec![5, 6, 7]]
        );
        assert!(first.allocate(3).is_err());
        let second = CpuPool::parse("1,3-4,5", 2).unwrap();
        assert_eq!(second.allocate(2).unwrap(), vec![vec![1, 3], vec![4, 5]]);
        assert_eq!(
            CpuPool::parse("7,1-3", 3).unwrap().allocate(1).unwrap(),
            vec![vec![7, 1, 2]]
        );
    }
    #[test]
    fn invalid_inputs_are_bounded_and_duplicates_are_rejected() {
        for spec in [
            "",
            "1,,2",
            "1,",
            "-1",
            "1-",
            "2-1",
            "1-2-3",
            "1,1",
            "1-3,2",
            "1.0",
            "1048576",
            "0-65536",
            "4294967296",
        ] {
            assert!(CpuPool::parse(spec, 1).is_err(), "{spec}");
        }
        assert!(CpuPool::parse("1", 0).is_err());
        assert!(CpuPool::parse("1", 2).is_err());
        assert!(CpuPool::parse("1", 1).unwrap().allocate(0).is_err());
    }
    #[test]
    fn distinguishes_missing_offline_and_disallowed_cpus() {
        let pool = CpuPool::parse("1,3", 1).unwrap();
        let all = BTreeSet::from([1, 3]);
        assert!(pool.validate_target(&all, &all, &all).is_ok());
        assert!(
            pool.validate_target(&BTreeSet::from([1]), &all, &all)
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            pool.validate_target(&all, &BTreeSet::from([1]), &all)
                .unwrap_err()
                .contains("offline")
        );
        assert!(
            pool.validate_target(&all, &all, &BTreeSet::from([1]))
                .unwrap_err()
                .contains("effective CPU set")
        );
    }
}
