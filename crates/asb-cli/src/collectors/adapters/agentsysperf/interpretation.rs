//! External analyzer completion is distinct from diagnosis qualification.
use serde_json::{Value, json};

pub(super) fn document(findings: Value) -> Value {
    json!({
        "schema_version":1,
        "status":"unqualified",
        "execution_status":"completed",
        "interpretation_status":"unqualified",
        "interpretation_reason":"The current ASB compatibility export contains an agent execution span and scoped resource/counter observations. It does not establish complete clock-aligned inference/tool phases, cache working-set or NUMA traffic measurements, or a CPU-qualified methodology for AgentSysPerf's causal classifications and remedies. Missing phases are not zero orchestration work; aggregate RSS is not cache working-set size. These findings are retained as external diagnostics only and do not become qualified comparison graphs or benchmark scores.",
        "findings":findings,
        "qualified_findings":[],
        "qualified_measurements":"Use the shared normalized input and ASB's regenerated kernel/hardware analysis; each observation keeps its own scope and quality."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_external_patterns_are_retained_without_qualifying_causal_remedies() {
        let findings = json!([
            {"analyzer":"breakdown","verdict":"orchestration_heavy","confidence":0.75,"evidence":{"inference_calls":0,"execution_calls":0,"orchestration_pct":100}},
            {"analyzer":"memory_bandwidth","verdict":"memory_bottleneck_moderate","confidence":0.6,"recommendations":["Use Intel RDT/CAT"]}
        ]);
        let result = document(findings.clone());
        assert_eq!(result["status"], "unqualified");
        assert_eq!(result["execution_status"], "completed");
        assert_eq!(result["interpretation_status"], "unqualified");
        assert_eq!(result["findings"], findings);
        assert!(result["qualified_findings"].as_array().unwrap().is_empty());
        assert!(
            result["interpretation_reason"]
                .as_str()
                .unwrap()
                .contains("RSS is not cache working-set")
        );
    }

    #[test]
    fn empty_or_future_diagnostics_cannot_silently_acquire_eligibility() {
        for findings in [
            json!([]),
            json!([{"analyzer":"future","verdict":"certain","confidence":1.0}]),
        ] {
            assert_eq!(document(findings)["interpretation_status"], "unqualified");
        }
    }
}
