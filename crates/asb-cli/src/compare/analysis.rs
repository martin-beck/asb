//! Offline analysis regenerates from validated source documents, never saved
//! conclusions. The report publisher owns all outputs; source results stay intact.
use super::*;

pub(super) fn regenerate(
    documents: &mut BTreeMap<String, Value>,
    metrics: &mut BTreeMap<String, f64>,
) -> Result<(), Error> {
    let analysis = crate::measurements::analysis::from_documents(documents);
    for family in ["hardware", "sampling"] {
        for pmu in analysis
            .get(family)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = pmu.get("pmu").and_then(Value::as_str).unwrap_or("unknown");
            for metric in pmu
                .get("metrics")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(id), Some(value)) = (
                    metric.get("id").and_then(Value::as_str),
                    metric.get("value").and_then(Value::as_f64),
                ) {
                    insert_unique(
                        metrics,
                        format!("analysis.{}.{}", path_key(name), path_key(id)),
                        value,
                    )?;
                }
            }
        }
    }
    for diagnostic in analysis
        .get("kernel")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(rule), Some(value)) = (
            diagnostic.get("rule").and_then(Value::as_str),
            diagnostic.get("value").and_then(Value::as_f64),
        ) {
            insert_unique(metrics, format!("analysis.{rule}"), value)?;
        }
    }
    for group in analysis["counter_groups"].as_array().into_iter().flatten() {
        if let (Some(pmu), Some(ipc)) = (
            group["pmu"].as_str(),
            group["instructions_per_cycle"].as_f64(),
        ) && !pmu.starts_with("armv8_pmuv3")
        {
            insert_unique(
                metrics,
                format!("analysis.{}.instructions_per_cycle", path_key(pmu)),
                ipc,
            )?;
        }
    }
    documents.insert("bottlenecks".into(), analysis);
    Ok(())
}

pub(super) fn html(documents: &BTreeMap<String, Value>) -> String {
    let Some(analysis) = documents.get("bottlenecks") else {
        return String::new();
    };
    let mut html = String::from(
        "<details><summary>Kernel and hardware analysis</summary><p>Regenerated offline from retained collector observations. Findings describe evidence; they do not establish a cause or additive wall-time breakdown.</p><ul>",
    );
    for finding in analysis
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |name| {
            finding
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
        };
        html.push_str(&format!(
            "<li><strong>{}</strong> — {}: {}<br><small>{}</small></li>",
            escape(text("rule")),
            escape(text("assessment")),
            escape(text("summary")),
            escape(text("limitations"))
        ));
    }
    for finding in analysis
        .get("kernel")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |key| finding.get(key).and_then(Value::as_str).unwrap_or("");
        let value = finding
            .get("value")
            .and_then(Value::as_f64)
            .map_or_else(|| "unavailable".into(), |v| format!("{v:.4}"));
        html.push_str(&format!(
            "<li><strong>{}</strong> — {}: {} {}<br><small>{} {}</small></li>",
            escape(text("rule")),
            escape(text("assessment")),
            value,
            escape(text("unit")),
            escape(text("unavailable_reason")),
            escape(text("limitations"))
        ));
    }
    html.push_str("</ul>");
    for group in analysis["counter_groups"].as_array().into_iter().flatten() {
        let pmu = group["pmu"].as_str().unwrap_or("unknown");
        let status = group["status"].as_str().unwrap_or("unavailable");
        let ipc = group["instructions_per_cycle"]
            .as_f64()
            .map_or_else(|| "unavailable".into(), |v| format!("{v:.4}"));
        html.push_str(&format!("<details><summary>Grouped agent counters: {} — {} (IPC: {})</summary><p>Exact raw counts and compatible-runtime counts are distinct. Separate PMU domains are never added for ratios.</p><pre>{}</pre></details>",escape(pmu),escape(status),ipc,escape(&serde_json::to_string_pretty(group).expect("normalized group is JSON"))));
    }
    for pmu in ["hardware", "sampling"].into_iter().flat_map(|family| {
        analysis
            .get(family)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }) {
        if let Some(profile) = pmu.get("cpu_profile") {
            let id = profile
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unavailable");
            let status = profile
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unavailable");
            let reason = profile.get("reason").and_then(Value::as_str).unwrap_or("");
            html.push_str(&format!(
                "<p><strong>CPU methodology:</strong> {} — {}. {}</p>",
                escape(id),
                escape(status),
                escape(reason)
            ));
        }
        html.push_str(&format!("<h4>{}</h4><table><thead><tr><th>Metric</th><th>Value</th><th>Unit / availability</th></tr></thead><tbody>",escape(pmu.get("pmu").and_then(Value::as_str).unwrap_or("PMU"))));
        for metric in pmu
            .get("metrics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = |key| metric.get(key).and_then(Value::as_str).unwrap_or("");
            let value = metric
                .get("value")
                .and_then(Value::as_f64)
                .map_or_else(|| "unavailable".into(), |v| format!("{v:.4}"));
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{} {}</td></tr>",
                escape(text("id")),
                value,
                escape(text("unit")),
                escape(text("unavailable_reason"))
            ));
        }
        html.push_str("</tbody></table>");
        html.push_str(&format!(
            "<p class=\"muted\">{}</p>",
            escape(pmu.get("limitations").and_then(Value::as_str).unwrap_or(""))
        ));
    }
    if let Some(platform) = documents.get("arm-platform") {
        let metadata = serde_json::to_string_pretty(platform).expect("platform metadata is JSON");
        html.push_str(&format!("<details><summary>Arm CPU and PMU capabilities</summary><p>Capability inventory only. Shared uncore activity is not collected or attributed to agents.</p><pre>{}</pre></details>",escape(&metadata)));
    }
    html.push_str("</details>");
    html
}

pub(super) fn cohort_html(report: &crate::measurements::analysis::cohort::Report) -> String {
    let report = serde_json::to_value(report).expect("cohort observations contain finite values");
    let mut html = String::from(
        "<section class=\"explain\"><h2>Observed cohort execution</h2><p>Principal lifetime overlap is a lower bound and includes sleeping. It does not prove simultaneous CPU execution. Phase completion rate uses an enclosing agent-phase duration; it is not task throughput or a speedup.</p><a href=\"cohort-analysis.json\">Download cohort evidence</a><div class=\"table-wrap\"><table><thead><tr><th>Configuration</th><th>Admitted</th><th>Peak live (minimum)</th><th>Overlap (minimum ms)</th><th>Phase span (maximum ms)</th><th>Phase completion rate (minimum /s)</th><th>Availability</th></tr></thead><tbody>",
    );
    if report["cohorts"]
        .as_array()
        .is_none_or(|cohorts| cohorts.is_empty())
    {
        html.push_str("<tr><td colspan=\"7\">No retained cohort membership; observed overlap is unavailable.</td></tr>");
    }
    for c in report["cohorts"].as_array().into_iter().flatten() {
        let text = |v: &Value| escape(v.as_str().unwrap_or("unknown"));
        let number = |key: &str| {
            c[key]
                .as_f64()
                .map_or_else(|| "unavailable".into(), |v| format!("{v:.3}"))
        };
        html.push_str(&format!("<tr><td><strong>{}</strong><br>{} / {}<br>{}:{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} {}</td></tr>", text(&c["context"]["agent"]), text(&c["context"]["provider"]), text(&c["context"]["model"]), text(&c["context"]["workload"]), text(&c["context"]["trial"]), c["admitted_replicas"], number("peak_live_principals_lower_bound"), number("multiple_principals_overlap_lower_bound_ms"), number("agent_phase_span_upper_bound_ms"), number("completed_agent_phases_per_second_lower_bound"), text(&c["status"]), c["unavailable_reason"].as_str().map(escape).unwrap_or_default()));
    }
    html.push_str("</tbody></table></div></section>");
    html
}

pub(super) fn graph_section(name: &str) -> Option<crate::collectors::GraphSection> {
    if name.starts_with("cohort-lifetimes-") || name.starts_with("cohort-scaling-") {
        Some(crate::collectors::GraphSection {
            title: "Observed agent lifetime overlap",
            description: "Conservative live-principal intervals in a common clock domain. Includes sleep; does not establish simultaneous CPU execution. Each plot represents its full admitted cohort.",
        })
    } else {
        crate::collectors::graph_section(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grouped_counter_report_is_recomputed_with_quality_and_scope_guards() {
        let counter = |n| json!({"observed_integer_count":n,"observed_rows":1,"unavailable_rows":0,"unknown_quality_rows":0,"known_event_runtime_ns":1000,"min_running_pct":100,"max_running_pct":100});
        let mut documents = BTreeMap::from([
            (
                "perf-stat-plan".into(),
                json!({"schema_version":1,"scope":"agent-process-tree","root":{"pid":42,"start_time_ticks":7},"excluded_controller_pid":1,"inherit":true,"system_wide":false,"counter_mode":"unscaled-observed","groups":["{cycles,instructions}"]}),
            ),
            (
                "perf-stat-quality".into(),
                json!({"schema_version":1,"counter_mode":"unscaled-observed","events":{"cycles":counter(100),"instructions":counter(200)}}),
            ),
            (
                "perf-stat".into(),
                json!({"status":"completed","metrics":{"perf.observed_cycles_count":1,"perf.observed_instructions_count":9999}}),
            ),
        ]);
        let mut metrics = BTreeMap::new();
        regenerate(&mut documents, &mut metrics).unwrap();
        assert_eq!(metrics["analysis.generic_cpu.instructions_per_cycle"], 2.0);
        assert!(html(&documents).contains("Grouped agent counters"));
        assert!(html(&documents).contains("compatible-runtime counts"));
        documents.get_mut("perf-stat-quality").unwrap()["events"]["cycles"]["min_running_pct"] =
            json!(90);
        metrics.clear();
        regenerate(&mut documents, &mut metrics).unwrap();
        assert!(metrics.is_empty());
        assert!(documents["bottlenecks"]["counter_groups"][0]["instructions_per_cycle"].is_null());
        documents.get_mut("perf-stat-plan").unwrap()["root"]["pid"] = json!(1);
        regenerate(&mut documents, &mut metrics).unwrap();
        assert!(
            documents["bottlenecks"]["counter_groups"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn regeneration_discards_saved_findings_and_preserves_original_documents() {
        let stale = json!({"findings":[{"rule":"invented","assessment":"observed"}]});
        let mut documents = BTreeMap::from([("bottlenecks".into(), stale)]);
        let mut metrics = BTreeMap::new();
        regenerate(&mut documents, &mut metrics).unwrap();
        assert!(!html(&documents).contains("invented"));
        assert!(html(&documents).contains("insufficient_evidence"));
        assert!(metrics.is_empty());
    }
    #[test]
    fn hardware_labels_and_diagnostics_are_escaped_in_html() {
        let documents = BTreeMap::from([(
            "bottlenecks".into(),
            json!({"hardware":[{"pmu":"<script>bad</script>","metrics":[{"id":"cache","value":null,"unit":"count","unavailable_reason":"<unsafe>"}]}]}),
        )]);
        let rendered = html(&documents);
        assert!(!rendered.contains("<script>"));
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(rendered.contains("unavailable"));
        assert!(rendered.contains("&lt;unsafe&gt;"));
    }
}
