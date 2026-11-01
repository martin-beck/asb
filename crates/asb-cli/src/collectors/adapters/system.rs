use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::super::CollectorAdapter;
use crate::measurements::{self, Session};
use serde::Serialize;

pub(super) struct SystemCollector;
pub(super) static ADAPTER: SystemCollector = SystemCollector;

impl CollectorAdapter for SystemCollector {
    fn estimated_disk_mib(&self) -> u64 {
        64
    }
    fn description(&self) -> &'static str {
        "host architecture and operating-system metadata"
    }
    fn target_scope(&self, _backend: &str) -> crate::collectors::TargetScope {
        crate::collectors::TargetScope::HostContext
    }
    fn id(&self) -> &'static str {
        "system"
    }
    fn start(&self, session: &mut Session) -> Result<(), String> {
        start(session)
    }
}

const MAX_METADATA_VALUE: usize = 128;
const MAX_CPU_ENTRIES: usize = 4096;

#[derive(Default, Serialize)]
struct SystemInfo {
    schema_version: u32,
    collected_unix_ms: u128,
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    kernel_release: Option<String>,
    memory_total_kib: Option<u64>,
    cpu_model: Option<String>,
    cpu_vendor: Option<String>,
    cpu_implementer: Option<String>,
    cpu_part: Option<String>,
    physical_packages: Option<usize>,
    physical_cores: Option<usize>,
    threads_per_core: Option<usize>,
    cpu_governors: Vec<String>,
    cpu_frequency_min_khz: Option<u64>,
    cpu_frequency_max_khz: Option<u64>,
    numa_nodes: Option<usize>,
    virtualization: Option<String>,
    container_runtime: Option<String>,
    compiler_versions: BTreeMap<String, String>,
    runtime_versions: BTreeMap<String, String>,
    collector_versions: BTreeMap<String, String>,
    kernel_settings: BTreeMap<String, String>,
}

#[derive(Default)]
struct CpuIdentity {
    model: Option<String>,
    vendor: Option<String>,
    implementer: Option<String>,
    part: Option<String>,
}

#[derive(Default)]
struct Topology {
    packages: BTreeSet<String>,
    cores: BTreeSet<(String, String)>,
}

fn start(session: &mut Session) -> Result<(), String> {
    let cpuinfo = read_file("/proc/cpuinfo");
    let identity = cpuinfo.as_deref().map(parse_cpuinfo).unwrap_or_default();
    let topology = collect_topology();
    let governors = collect_governors();
    let logical_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let physical_cores = (!topology.cores.is_empty()).then_some(topology.cores.len());
    let physical_packages = (!topology.packages.is_empty()).then_some(topology.packages.len());
    let threads_per_core = physical_cores
        .filter(|cores| *cores > 0 && logical_cpus.is_multiple_of(*cores))
        .map(|cores| logical_cpus / cores);
    let virtualization = classify_virtualization(
        marker_exists("/.dockerenv") || marker_exists("/run/.containerenv"),
        read_file("/proc/1/cgroup").as_deref().unwrap_or_default(),
        read_file("/sys/class/dmi/id/product_name")
            .as_deref()
            .unwrap_or_default(),
        cpuinfo.as_deref().unwrap_or_default(),
    );
    let info = SystemInfo {
        schema_version: 2,
        collected_unix_ms: session.started_unix_ms(),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        logical_cpus,
        kernel_release: read_trimmed("/proc/sys/kernel/osrelease"),
        memory_total_kib: read_file("/proc/meminfo").and_then(|text| parse_memory_total_kib(&text)),
        cpu_model: identity.model,
        cpu_vendor: identity.vendor,
        cpu_implementer: identity.implementer,
        cpu_part: identity.part,
        physical_packages,
        physical_cores,
        threads_per_core,
        cpu_governors: governors.names,
        cpu_frequency_min_khz: governors.min_khz,
        cpu_frequency_max_khz: governors.max_khz,
        numa_nodes: read_trimmed("/sys/devices/system/node/online")
            .and_then(|value| parse_cpu_set(&value)),
        virtualization: Some(virtualization.0),
        container_runtime: virtualization.1,
        compiler_versions: command_versions([("rustc", "--version"), ("cargo", "--version")]),
        runtime_versions: runtime_versions(),
        collector_versions: collector_versions(session.tool_dir.as_deref()),
        kernel_settings: kernel_settings(),
    };
    measurements::write_json(&session.dir.join("system.json"), &info)
}

#[derive(Default)]
struct GovernorInfo {
    names: Vec<String>,
    min_khz: Option<u64>,
    max_khz: Option<u64>,
}

fn collect_governors() -> GovernorInfo {
    let mut names = BTreeSet::new();
    let mut min_khz = None;
    let mut max_khz = None;
    for cpu in cpu_directories() {
        let base = format!("/sys/devices/system/cpu/{cpu}/cpufreq");
        if let Some(value) = read_trimmed(&format!("{base}/scaling_governor")) {
            names.insert(value);
        }
        update_min(
            &mut min_khz,
            read_trimmed(&format!("{base}/cpuinfo_min_freq")),
        );
        update_max(
            &mut max_khz,
            read_trimmed(&format!("{base}/cpuinfo_max_freq")),
        );
    }
    GovernorInfo {
        names: names.into_iter().collect(),
        min_khz,
        max_khz,
    }
}

fn update_min(target: &mut Option<u64>, value: Option<String>) {
    if let Some(value) = value.and_then(|value| value.parse().ok()) {
        *target = Some(target.map_or(value, |current| current.min(value)));
    }
}

fn update_max(target: &mut Option<u64>, value: Option<String>) {
    if let Some(value) = value.and_then(|value| value.parse().ok()) {
        *target = Some(target.map_or(value, |current| current.max(value)));
    }
}

fn collect_topology() -> Topology {
    let mut result = Topology::default();
    for cpu in cpu_directories() {
        let package = read_trimmed(&format!(
            "/sys/devices/system/cpu/{cpu}/topology/physical_package_id"
        ));
        let core = read_trimmed(&format!("/sys/devices/system/cpu/{cpu}/topology/core_id"));
        if let (Some(package), Some(core)) = (package, core) {
            result.packages.insert(package.clone());
            result.cores.insert((package, core));
        }
    }
    result
}

fn cpu_directories() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return Vec::new();
    };
    let mut cpus = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix("cpu").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
            })
        })
        .take(MAX_CPU_ENTRIES)
        .collect::<Vec<_>>();
    cpus.sort_by_key(|name| name[3..].parse::<usize>().unwrap_or(usize::MAX));
    cpus
}

fn parse_cpuinfo(text: &str) -> CpuIdentity {
    let mut result = CpuIdentity::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "model name" if result.model.is_none() => result.model = sanitize(value),
            "processor" | "hardware" if result.model.is_none() => result.model = sanitize(value),
            "vendor_id" if result.vendor.is_none() => result.vendor = sanitize(value),
            "cpu implementer" if result.implementer.is_none() => {
                result.implementer = sanitize(value)
            }
            "cpu part" if result.part.is_none() => result.part = sanitize(value),
            _ => {}
        }
    }
    if result.implementer.is_none() {
        result.implementer = result.vendor.clone();
    }
    result
}

fn parse_memory_total_kib(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix("MemTotal:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn parse_cpu_set(value: &str) -> Option<usize> {
    let count = value.split(',').try_fold(0usize, |count, part| {
        let part = part.trim();
        if part.is_empty() {
            return Some(count);
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<usize>().ok()?;
            let end = end.parse::<usize>().ok()?;
            return Some(count.saturating_add(end.checked_sub(start)?.saturating_add(1)));
        }
        part.parse::<usize>().ok().map(|_| count.saturating_add(1))
    })?;
    (count > 0).then_some(count)
}

fn classify_virtualization(
    marker: bool,
    cgroup: &str,
    dmi_product: &str,
    cpuinfo: &str,
) -> (String, Option<String>) {
    let lower_cgroup = cgroup.to_ascii_lowercase();
    let lower_dmi = dmi_product.to_ascii_lowercase();
    let container = marker
        || ["docker", "podman", "containerd", "kubepods", "libpod"]
            .iter()
            .any(|needle| lower_cgroup.contains(needle));
    let vm = cpuinfo.lines().any(|line| {
        line.trim_start().starts_with("flags")
            && line.split_whitespace().any(|value| value == "hypervisor")
    }) || ["kvm", "qemu", "vmware", "virtualbox", "xen", "hyper-v"]
        .iter()
        .any(|needle| lower_dmi.contains(needle));
    let runtime = [
        ("docker", "docker"),
        ("podman", "podman"),
        ("containerd", "containerd"),
        ("kubepods", "kubernetes"),
        ("libpod", "podman"),
    ]
    .iter()
    .find(|(needle, _)| lower_cgroup.contains(needle))
    .map(|(_, name)| (*name).to_owned());
    let class = match (container, vm) {
        (true, true) => "container_in_vm",
        (true, false) => "container",
        (false, true) => "vm",
        (false, false) => "bare_metal",
    };
    (class.to_owned(), runtime)
}

fn command_versions<const N: usize>(commands: [(&str, &str); N]) -> BTreeMap<String, String> {
    commands
        .into_iter()
        .filter_map(|(name, argument)| {
            command_version(name, argument).map(|version| (name.to_owned(), version))
        })
        .collect()
}

/// Return versions for runtimes used by the benchmark.
///
/// ASB is the process collecting this metadata, so resolving `asb` through
/// PATH can report an unrelated installation when a project-local binary is
/// being exercised.  Record the compiled package version instead; the other
/// runtimes remain externally resolved as before.
fn runtime_versions() -> BTreeMap<String, String> {
    let mut versions = command_versions([("node", "--version"), ("python3", "--version")]);
    versions.insert(
        "asb".to_owned(),
        format!("asb {}", env!("CARGO_PKG_VERSION")),
    );
    versions
}

fn command_version(name: &str, argument: &str) -> Option<String> {
    command_version_with_deadline(name, argument, std::time::Duration::from_secs(2))
}

fn command_version_with_deadline(
    name: &str,
    argument: &str,
    budget: std::time::Duration,
) -> Option<String> {
    let (stdout, stderr) = measurements::bounded_version(Path::new(name), argument, budget)?;
    let text = String::from_utf8_lossy(if stdout.is_empty() { &stderr } else { &stdout });
    text.lines().find_map(sanitize)
}

fn collector_versions(tool_dir: Option<&Path>) -> BTreeMap<String, String> {
    [
        ("perf", "--version"),
        ("bpftrace", "--version"),
        ("perfetto", "--version"),
        ("mpstat", "--version"),
        ("iostat", "--version"),
    ]
    .into_iter()
    .filter_map(|(name, argument)| {
        let executable = measurements::resolve_executable(tool_dir, name);
        command_version(executable.to_str()?, argument).map(|version| (name.to_owned(), version))
    })
    .collect()
}

fn kernel_settings() -> BTreeMap<String, String> {
    [
        (
            "perf_event_paranoid",
            "/proc/sys/kernel/perf_event_paranoid",
        ),
        ("kptr_restrict", "/proc/sys/kernel/kptr_restrict"),
        ("numa_balancing", "/proc/sys/kernel/numa_balancing"),
    ]
    .into_iter()
    .filter_map(|(name, path)| read_trimmed(path).map(|value| (name.to_owned(), value)))
    .collect()
}

fn marker_exists(path: &str) -> bool {
    Path::new(path).exists()
}

fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_trimmed(path: &str) -> Option<String> {
    read_file(path).and_then(|value| sanitize(&value))
}

fn sanitize(value: &str) -> Option<String> {
    let value = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_METADATA_VALUE)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{
        classify_virtualization, parse_cpu_set, parse_cpuinfo, parse_memory_total_kib,
        runtime_versions, sanitize,
    };

    #[cfg(unix)]
    #[test]
    fn version_probes_are_bounded_offline_and_keep_successful_versions() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!(
            "asb-version-probe-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = |name: &str, body: &str| {
            let path = root.join(name);
            std::fs::write(&path, format!("#!/usr/bin/python3\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        let valid = fixture(
            "valid",
            "import os; assert os.environ['RUSTUP_AUTO_INSTALL'] == '0'; print('fixture 1.2')",
        );
        let failing = fixture("failing", "import sys; print('not a version'); sys.exit(3)");
        let large = fixture("large", "print('x'*100000)");
        let hanging = fixture("hanging", "import time; time.sleep(30)");
        let check = |path: &std::path::Path| {
            super::command_version_with_deadline(
                path.to_str().unwrap(),
                "--version",
                Duration::from_millis(300),
            )
        };
        assert_eq!(check(&valid).as_deref(), Some("fixture 1.2"));
        assert!(check(&failing).is_none());
        assert!(check(&large).is_none());
        let started = Instant::now();
        assert!(check(&hanging).is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn version_probe_quiesces_pipe_holders_on_success_and_failure() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!(
            "asb-probe-descendants-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for code in [0, 7] {
            let marker = root.join(format!("pid-{code}"));
            let script = root.join(format!("fixture-{code}"));
            let body = format!(
                "#!/usr/bin/python3\nimport os,time\npid=os.fork()\nif pid==0:\n open({:?},'w').write(str(os.getpid()))\n time.sleep(30)\n os._exit(0)\nwhile not os.path.exists({:?}): time.sleep(.005)\nprint('fixture 1.0',flush=True)\nos._exit({code})\n",
                marker.to_str().unwrap(),
                marker.to_str().unwrap()
            );
            std::fs::write(&script, body).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = super::command_version_with_deadline(
                script.to_str().unwrap(),
                "--version",
                Duration::from_secs(2),
            );
            if code == 0 {
                assert_eq!(result.as_deref(), Some("fixture 1.0"));
            } else {
                assert!(result.is_none());
            }
            let pid = std::fs::read_to_string(&marker).unwrap();
            let proc_path = format!("/proc/{}", pid.trim());
            let until = Instant::now() + Duration::from_secs(3);
            while std::path::Path::new(&proc_path).exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !std::path::Path::new(&proc_path).exists(),
                "probe descendant remained alive"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_x86_and_arm_cpu_identity_without_raw_proc_data() {
        let x86 = parse_cpuinfo(
            "vendor_id : GenuineIntel\nmodel name : Intel(R) Xeon\nflags : fpu hypervisor\n",
        );
        assert_eq!(x86.vendor.as_deref(), Some("GenuineIntel"));
        assert_eq!(x86.model.as_deref(), Some("Intel(R) Xeon"));

        let arm =
            parse_cpuinfo("CPU implementer : 0x41\nCPU part : 0xd0c\nProcessor : Neoverse-N1\n");
        assert_eq!(arm.implementer.as_deref(), Some("0x41"));
        assert_eq!(arm.part.as_deref(), Some("0xd0c"));
        assert_eq!(arm.model.as_deref(), Some("Neoverse-N1"));
    }

    #[test]
    fn parses_bounded_cpu_sets_and_memory() {
        assert_eq!(parse_cpu_set("0-3,8,10-11"), Some(7));
        assert_eq!(parse_cpu_set(""), None);
        assert_eq!(
            parse_memory_total_kib("MemTotal:       123 kB\n"),
            Some(123)
        );
    }

    #[test]
    fn classifies_container_and_virtual_machine_without_exposing_markers() {
        assert_eq!(
            classify_virtualization(
                false,
                "/kubepods.slice",
                "QEMU Virtual Machine",
                "flags : hypervisor"
            ),
            ("container_in_vm".to_owned(), Some("kubernetes".to_owned()))
        );
        assert_eq!(
            classify_virtualization(true, "", "", ""),
            ("container".to_owned(), None)
        );
    }

    #[test]
    fn sanitizes_control_characters_and_bounds_values() {
        let value = sanitize(&format!("ok\n{}", "x".repeat(200))).expect("value");
        assert!(!value.contains('\n'));
        assert_eq!(value.len(), 128);
        assert_eq!(sanitize(" \n\t"), None);
    }

    #[test]
    fn records_the_running_asb_package_version_not_a_path_binary() {
        let versions = runtime_versions();
        assert_eq!(
            versions.get("asb").map(String::as_str),
            Some(concat!("asb ", env!("CARGO_PKG_VERSION")))
        );
    }
}
