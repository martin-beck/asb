//! Bounded Arm PMU discovery. This records capability, never measured activity.

use crate::collectors::{CollectorAdapter, TargetScope};
use crate::measurements::{self, Session};
use serde::Serialize;
use std::{collections::BTreeMap, fs, io::Read, path::Path};

pub(super) struct ArmPlatform;
pub(super) static ADAPTER: ArmPlatform = ArmPlatform;
const MAX_PMUS: usize = 256;
const MAX_EVENTS: usize = 256;
const MAX_TOTAL_FIELDS: usize = 4096;
const MAX_VALUE: u64 = 512;

impl CollectorAdapter for ArmPlatform {
    fn id(&self) -> &'static str {
        "arm-platform"
    }
    fn description(&self) -> &'static str {
        "Arm CPU, SPE and uncore PMU capabilities"
    }
    fn target_scope(&self, _: &str) -> TargetScope {
        TargetScope::HostContext
    }
    fn estimated_disk_mib(&self) -> u64 {
        8
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &["system"]
    }
    fn start(&self, session: &mut Session) -> Result<(), String> {
        let report = resolved_inventory(session);
        measurements::write_json(&session.dir.join("arm-platform.json"), &report)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Pmu {
    pub name: String,
    pub domain: &'static str,
    pub event_type: Option<u32>,
    pub cpus: Option<String>,
    pub cpumask: Option<String>,
    pub events: BTreeMap<String, String>,
    pub formats: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Inventory {
    schema_version: u32,
    architecture: String,
    status: &'static str,
    reason: Option<String>,
    scope: &'static str,
    pub pmus: Vec<Pmu>,
    limitations: &'static str,
}

#[derive(Default)]
struct State {
    inventory: Option<Inventory>,
}

/// Freeze one inventory per attempt; adapters and capability output consume the
/// same selected definitions even if host metadata changes during preparation.
pub(super) fn resolved_inventory(session: &mut Session) -> Inventory {
    let state = session.state::<State>();
    let mut state = state.borrow_mut();
    state
        .inventory
        .get_or_insert_with(|| {
            inventory(
                Path::new("/sys/bus/event_source/devices"),
                std::env::consts::ARCH,
            )
        })
        .clone()
}

fn text(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|_| "PMU metadata is unreadable")?;
    if !file
        .metadata()
        .map_err(|_| "PMU metadata cannot be inspected")?
        .is_file()
    {
        return Err("PMU metadata is not a regular file".into());
    }
    let mut value = String::new();
    file.take(MAX_VALUE + 1)
        .read_to_string(&mut value)
        .map_err(|_| "PMU metadata is not valid text")?;
    if value.len() as u64 > MAX_VALUE
        || !value
            .trim()
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err("PMU metadata is oversized or contains unsupported characters".into());
    }
    Ok(value.trim().to_owned())
}

fn names(path: &Path, maximum: usize) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for item in fs::read_dir(path)
        .map_err(|_| "PMU directory is unavailable")?
        .take(maximum + 1)
    {
        let name = item
            .map_err(|_| "PMU directory cannot be read")?
            .file_name()
            .into_string()
            .map_err(|_| "PMU entry name is not UTF-8")?;
        if name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err("PMU entry name is invalid".into());
        }
        result.push(name);
    }
    if result.len() > maximum {
        return Err("PMU directory exceeds discovery limit".into());
    }
    result.sort();
    Ok(result)
}

fn fields(path: &Path) -> Result<BTreeMap<String, String>, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(_) => Err("PMU directory cannot be inspected".into()),
        Ok(_) => names(path, MAX_EVENTS)?
            .into_iter()
            .map(|name| text(&path.join(&name)).map(|value| (name, value)))
            .collect(),
    }
}

fn domain(name: &str) -> Option<&'static str> {
    if name.starts_with("armv8_pmuv3") {
        Some("cpu")
    } else if name.starts_with("arm_spe") {
        Some("spe")
    } else if [
        "hisi_", "arm_cmn", "arm_ccn", "arm_dsu", "ampere_", "nvidia_", "smmuv3_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
    {
        Some("uncore")
    } else {
        None
    }
}

pub(crate) fn inventory(root: &Path, architecture: &str) -> Inventory {
    let mut report = Inventory {
        schema_version: 1,
        architecture: architecture.into(),
        status: "unavailable",
        reason: None,
        scope: "host_context",
        pmus: Vec::new(),
        limitations: "Discovery proves advertised capabilities only. Event readability, process attribution, counter availability and sample quality require runtime probes. Uncore counters are shared host context.",
    };
    let discovered = || -> Result<Vec<Pmu>, String> {
        if architecture != "aarch64" {
            return Err("Arm telemetry requires an AArch64 host".into());
        }
        let mut remaining = MAX_TOTAL_FIELDS;
        names(root, MAX_PMUS)?
            .into_iter()
            .filter_map(|name| domain(&name).map(|kind| (name, kind)))
            .map(|(name, kind)| {
                let base = root.join(&name);
                let pmu = Pmu {
                    domain: kind,
                    event_type: Some(
                        text(&base.join("type"))?
                            .parse::<u32>()
                            .map_err(|_| "PMU type is not a valid integer")?,
                    ),
                    cpus: text(&base.join("cpus")).ok(),
                    cpumask: text(&base.join("cpumask")).ok(),
                    events: fields(&base.join("events"))?,
                    formats: fields(&base.join("format"))?,
                    name,
                };
                remaining = remaining
                    .checked_sub(pmu.events.len() + pmu.formats.len())
                    .ok_or("PMU metadata exceeds the shared field limit")?;
                Ok(pmu)
            })
            .collect()
    };
    match discovered() {
        Ok(pmus) if !pmus.is_empty() => {
            report.pmus = pmus;
            report.status = "discovered";
        }
        Ok(_) => report.reason = Some("No supported Arm PMU metadata is exposed".into()),
        Err(reason) => report.reason = Some(reason),
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> std::path::PathBuf {
        crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-arm-pmu-").unwrap()
    }
    #[test]
    fn inventory_separates_core_spe_and_uncore_without_executing_tools() {
        let root = fixture();
        for name in ["armv8_pmuv3_0", "arm_spe_0", "hisi_sccl1_ddrc0", "software"] {
            let dir = root.join(name);
            fs::create_dir_all(dir.join("events")).unwrap();
            fs::write(dir.join("type"), "10\n").unwrap();
            fs::write(dir.join("cpus"), "0-127\n").unwrap();
            fs::write(dir.join("events/cpu_cycles"), "event=0x11\n").unwrap();
        }
        let data = inventory(&root, "aarch64");
        assert_eq!(data.status, "discovered");
        assert_eq!(data.pmus.len(), 3);
        let core = data.pmus.iter().find(|p| p.domain == "cpu").unwrap();
        assert_eq!(core.events["cpu_cycles"], "event=0x11");
        assert_eq!(core.cpus.as_deref(), Some("0-127"));
        assert!(data.pmus.iter().any(|p| p.domain == "uncore"));
        assert_eq!(inventory(&root, "x86_64").status, "unavailable");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_or_unreadable_metadata_cannot_publish_partial_capabilities() {
        let root = fixture();
        let dir = root.join("armv8_pmuv3_0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("type"), vec![b'1'; 513]).unwrap();
        let data = inventory(&root, "aarch64");
        assert_eq!(data.status, "unavailable");
        assert!(data.pmus.is_empty());
        assert!(data.reason.unwrap().contains("oversized"));
        fs::remove_file(dir.join("type")).unwrap();
        assert_eq!(inventory(&root, "aarch64").status, "unavailable");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an AArch64 host exposing real Linux PMU sysfs metadata"]
    fn native_arm_inventory_matches_exposed_cpu_pmus() {
        assert_eq!(std::env::consts::ARCH, "aarch64");
        let data = inventory(Path::new("/sys/bus/event_source/devices"), "aarch64");
        assert_eq!(data.status, "discovered", "{:?}", data.reason);
        let cores = data
            .pmus
            .iter()
            .filter(|p| p.domain == "cpu")
            .collect::<Vec<_>>();
        assert!(!cores.is_empty());
        for core in cores {
            assert!(core.event_type.is_some());
            assert!(core.events.contains_key("cpu_cycles"));
            assert!(core.events.contains_key("inst_retired"));
        }
        if let Some(output) = std::env::var_os("ASB_TEST_ARM_PLATFORM_OUTPUT") {
            measurements::write_json(Path::new(&output), &data).unwrap();
        }
    }
}
