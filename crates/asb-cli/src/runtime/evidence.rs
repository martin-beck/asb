//! Runtime intent recorded before workload and agent launch.

use super::settings::{CpuDescription, RuntimeSettings};

/// Describe the requested allocation and observed topology without claiming
/// that a process or managed container has already applied the allocation.
pub fn planned(
    settings: &RuntimeSettings,
    allocation: Option<&[u32]>,
) -> Option<serde_json::Value> {
    settings.cpu_pool.as_ref()?;
    let actual = CpuDescription::current();
    let differences = settings
        .cpu_description
        .as_ref()
        .map(|saved| saved.differences(&actual))
        .unwrap_or_default();
    Some(serde_json::json!({
        "schema_version": 1,
        "requested": settings,
        "requested_replica_cpus": allocation,
        "target_cpu_description": actual,
        "topology_differences": differences,
        "placement_status": "pending_boundary_verification",
        "limitations": [
            "CPU leases coordinate only campaigns using the same user cache and do not reserve CPUs from the operating system",
            "independently created Docker containers may run outside the requested allocation",
            "CPU-wide sampling may include unrelated host, kernel and collector activity"
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::super::settings::CpuPoolSettings;
    use super::*;

    #[test]
    fn planned_evidence_never_claims_affinity_has_already_been_applied() {
        assert!(planned(&RuntimeSettings::default(), None).is_none());
        let settings = RuntimeSettings {
            cpu_pool: Some(CpuPoolSettings {
                cpus: "1,3".into(),
                num_cpus_per_instance: 2,
            }),
            ..Default::default()
        };
        let evidence = planned(&settings, Some(&[1, 3])).unwrap();
        assert_eq!(
            evidence["placement_status"],
            "pending_boundary_verification"
        );
        assert_eq!(
            evidence["requested_replica_cpus"],
            serde_json::json!([1, 3])
        );
        assert_eq!(
            evidence["target_cpu_description"]["architecture"],
            std::env::consts::ARCH
        );
        assert!(evidence.get("effective_replica_cpus").is_none());
    }
}
