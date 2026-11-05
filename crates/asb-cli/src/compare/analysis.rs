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
    for pmu in ["hardware", "sampling"].into_iter().flat_map(|family| {
        analysis
            .get(family)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }) {
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
    html.push_str("</details>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
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
