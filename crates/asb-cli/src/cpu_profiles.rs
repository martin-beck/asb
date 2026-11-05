//! Static, CPU-specific analysis methods, shared by acquisition and reporting.
//! Discovery reads capability registers only, never process/system activity.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub(crate) const N1: &str = "arm-neoverse-n1-cycle-accounting-v1";
const N1_SOURCE: &str = "https://documentation-service.arm.com/static/66f7154a7243ed1626433643";
const MAX_CPUS: usize = 4096;

struct Profile {
    id: &'static str,
    implementer: u64,
    part: u64,
    source: &'static str,
}
const REGISTRY: &[Profile] = &[Profile {
    id: N1,
    implementer: 0x41,
    part: 0xd0c,
    source: N1_SOURCE,
}];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CpuInventory {
    pub schema_version: u32,
    pub online_cpus: Vec<u32>,
    pub midr_by_cpu: BTreeMap<u32, u64>,
    pub unavailable_cpus: Vec<u32>,
}
#[derive(Clone, Serialize)]
pub(crate) struct Selection {
    pub id: Option<&'static str>,
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub source: Option<&'static str>,
    pub cpus: Vec<u32>,
    pub limitations: &'static str,
}

/// Bounded CPU-list parser; never expands an arbitrary numerical range first.
pub(crate) fn cpu_list(text: &str) -> Option<Vec<u32>> {
    if text.len() > 65536 || text.trim().is_empty() {
        return None;
    }
    let mut cpus = BTreeSet::new();
    for item in text.trim().split(',') {
        let (first, last) = if let Some((a, b)) = item.split_once('-') {
            (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)
        } else {
            let n = item.parse::<u32>().ok()?;
            (n, n)
        };
        if last < first || last > 1_048_575 || last - first >= MAX_CPUS as u32 {
            return None;
        }
        for cpu in first..=last {
            if !cpus.insert(cpu) || cpus.len() > MAX_CPUS {
                return None;
            }
        }
    }
    Some(cpus.into_iter().collect())
}
fn text(path: &Path, maximum: u64) -> Option<String> {
    let mut value = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(maximum + 1)
        .read_to_string(&mut value)
        .ok()?;
    (value.len() as u64 <= maximum).then_some(value)
}
pub(crate) fn discover(root: &Path) -> CpuInventory {
    let mut inventory = CpuInventory {
        schema_version: 1,
        ..Default::default()
    };
    let Some(online) = text(&root.join("online"), 65536).and_then(|s| cpu_list(&s)) else {
        return inventory;
    };
    for cpu in &online {
        let midr = text(
            &root.join(format!("cpu{cpu}/regs/identification/midr_el1")),
            128,
        )
        .and_then(|s| u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok())
        .filter(|v| *v > 0 && *v <= u32::MAX as u64);
        if let Some(midr) = midr {
            inventory.midr_by_cpu.insert(*cpu, midr);
        } else {
            inventory.unavailable_cpus.push(*cpu);
        }
    }
    inventory.online_cpus = online;
    inventory
}

/// Check collection bounds before allocating typed copies of retained metadata.
pub(crate) fn decode(value: &serde_json::Value) -> Option<CpuInventory> {
    if value.get("schema_version")?.as_u64()? != 1
        || value.get("online_cpus")?.as_array()?.len() > MAX_CPUS
        || value.get("midr_by_cpu")?.as_object()?.len() > MAX_CPUS
        || value.get("unavailable_cpus")?.as_array()?.len() > MAX_CPUS
    {
        return None;
    }
    CpuInventory::deserialize(value).ok()
}

/// A domain is matched in full. Never infer a model from the first CPU or name.
pub(crate) fn select(cpus: Option<&str>, inventory: &CpuInventory) -> Selection {
    let mut selection = Selection {
        id: None,
        status: "unavailable",
        reason: Some("requires a complete advertised PMU CPU domain and bounded MIDR identities"),
        source: None,
        cpus: Vec::new(),
        limitations: "Neoverse N1 Cycle Accounting reports separate frontend/backend stalled-cycle fractions, not a four-category slot decomposition or additive wall-time model. Unknown/mixed CPUs and missing event quality cannot inherit another processor's methodology. Hardware errata and sampling overhead remain separate concerns.",
    };
    let Some(cpus) = cpus.and_then(cpu_list) else {
        return selection;
    };
    if inventory.schema_version != 1
        || inventory.online_cpus.len() > MAX_CPUS
        || inventory.midr_by_cpu.len() > MAX_CPUS
        || inventory.unavailable_cpus.len() > MAX_CPUS
    {
        return selection;
    }
    if inventory
        .online_cpus
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
        || inventory
            .unavailable_cpus
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return selection;
    }
    let mut models = BTreeSet::new();
    for cpu in &cpus {
        if inventory.online_cpus.binary_search(cpu).is_err()
            || inventory.unavailable_cpus.binary_search(cpu).is_ok()
        {
            return selection;
        }
        let Some(midr) = inventory
            .midr_by_cpu
            .get(cpu)
            .filter(|m| **m > 0 && **m <= u32::MAX as u64)
        else {
            return selection;
        };
        models.insert(((midr >> 24) & 0xff, (midr >> 4) & 0xfff));
    }
    selection.cpus = cpus;
    // MIDR implementer Arm (0x41), part Neoverse N1 (0xd0c). Variant and
    // revision do not change the documented formula; no errata are bypassed.
    if let Some(profile) = REGISTRY
        .iter()
        .find(|profile| models == BTreeSet::from([(profile.implementer, profile.part)]))
    {
        selection.id = Some(profile.id);
        selection.status = "supported";
        selection.reason = None;
        selection.source = Some(profile.source);
    } else {
        selection.status = "unsupported";
        selection.reason = Some("no registered CPU methodology for every CPU in this PMU domain");
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inventory() -> CpuInventory {
        CpuInventory {
            schema_version: 1,
            online_cpus: vec![0, 1],
            midr_by_cpu: [(0, 0x410fd0c0), (1, 0x411fd0c1)].into(),
            unavailable_cpus: vec![],
        }
    }
    #[test]
    fn only_full_matching_domains_select_the_n1_methodology() {
        let mut i = inventory();
        assert_eq!(select(Some("0-1"), &i).id, Some(N1));
        i.midr_by_cpu.insert(1, 0x481fd010);
        assert_eq!(select(Some("0-1"), &i).status, "unsupported");
        assert_eq!(select(Some("0"), &i).id, Some(N1));
        assert!(select(None, &i).id.is_none());
        i.midr_by_cpu.remove(&1);
        assert_eq!(select(Some("0-1"), &i).status, "unavailable");
        i = inventory();
        i.unavailable_cpus.push(1);
        assert_eq!(select(Some("0-1"), &i).status, "unavailable");
        i = inventory();
        i.online_cpus.retain(|cpu| *cpu == 0);
        assert_eq!(select(Some("0-1"), &i).status, "unavailable");
    }
    #[test]
    fn retained_identity_shape_bounds_and_unknown_fields_are_checked_before_selection() {
        let mut value = serde_json::to_value(inventory()).unwrap();
        assert!(decode(&value).is_some());
        value["unexpected"] = serde_json::json!(true);
        assert!(decode(&value).is_none());
        value.as_object_mut().unwrap().remove("unexpected");
        value["online_cpus"] = serde_json::json!(vec![0; MAX_CPUS + 1]);
        assert!(decode(&value).is_none());
        let mut i = inventory();
        i.online_cpus = vec![1, 0];
        assert!(select(Some("0-1"), &i).id.is_none());
        i.online_cpus = vec![0, 0, 1];
        assert!(select(Some("0-1"), &i).id.is_none());
    }

    #[test]
    fn cpu_lists_reject_expansion_duplicates_traversal_and_overflow() {
        assert_eq!(cpu_list("0-2,4"), Some(vec![0, 1, 2, 4]));
        for text in [
            "",
            "0-999999999",
            "0-4096",
            "2-1",
            "1,1",
            "0-2,2",
            "../1",
            "1,,2",
            "-1",
            "0-1-2",
            "4294967296",
        ] {
            assert!(cpu_list(text).is_none(), "{text}");
        }
    }
    #[test]
    fn discovery_is_bounded_capability_metadata_and_missing_registers_are_explicit() {
        let root =
            crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-cpu-profile-").unwrap();
        std::fs::create_dir_all(root.join("cpu0/regs/identification")).unwrap();
        std::fs::write(root.join("online"), "0-1\n").unwrap();
        std::fs::write(
            root.join("cpu0/regs/identification/midr_el1"),
            "0x00000000410fd0c0\n",
        )
        .unwrap();
        let i = discover(&root);
        assert_eq!(i.midr_by_cpu[&0], 0x410fd0c0);
        assert_eq!(i.unavailable_cpus, vec![1]);
        assert_eq!(select(Some("0-1"), &i).status, "unavailable");
        std::fs::write(
            root.join("cpu0/regs/identification/midr_el1"),
            "x".repeat(129),
        )
        .unwrap();
        assert_eq!(discover(&root).unavailable_cpus, vec![0, 1]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
