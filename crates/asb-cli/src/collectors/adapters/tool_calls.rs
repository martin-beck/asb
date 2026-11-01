//! Structured agent tool-call observations are published by measurement finalization.

use super::super::{CollectorAdapter, ReportGraph, ReportRow};
use std::collections::BTreeSet;

pub(super) struct ToolCallsCollector;
pub(super) static ADAPTER: ToolCallsCollector = ToolCallsCollector;

impl CollectorAdapter for ToolCallsCollector {
    fn description(&self) -> &'static str {
        "structured agent tool-call observations"
    }
    fn id(&self) -> &'static str {
        "tool-calls"
    }
    fn report_graphs(&self, rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| heatmap(index, row))
            .collect()
    }
}

fn heatmap(index: usize, row: &ReportRow<'_>) -> Option<ReportGraph> {
    let calls = row.documents.get("tool-calls")?.get("calls")?.as_array()?;
    let mut classes = BTreeSet::new();
    let mut maximum = 0u64;
    for call in calls {
        maximum = maximum.max(call.get("response_observed_unix_ms")?.as_u64()?);
        for class in call
            .get("classes")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
        {
            classes.insert(class.to_owned());
        }
    }
    if classes.is_empty() {
        return None;
    }
    let minimum = calls
        .iter()
        .filter_map(|call| {
            call.get("response_observed_unix_ms")
                .and_then(serde_json::Value::as_u64)
        })
        .min()?;
    let span = (maximum.saturating_sub(minimum)).max(1) as f64;
    let classes = classes.into_iter().collect::<Vec<_>>();
    let width = 920.0;
    let left = 190.0;
    let plot_width = 700.0;
    let row_height = 30.0;
    let height = 90.0 + row_height * classes.len() as f64;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><style>text{{font:12px sans-serif;fill:#18212b}}.axis{{stroke:#9aa5b1}}.cell{{fill:#2563eb}}.duration{{stroke:#f59e0b;stroke-width:3}}</style><text x="20" y="25" font-size="16">LLM tool-call classes over provider-response time — {}</text><text x="{}" y="48">response-observed time (ms from first tool call); orange extension = observed tool execution duration</text>"#,
        escape(row.label),
        left
    );
    for (class_index, class) in classes.iter().enumerate() {
        let y = 65.0 + class_index as f64 * row_height;
        svg.push_str(&format!(
            r#"<text x="10" y="{}">{}</text><line class="axis" x1="{}" y1="{}" x2="{}" y2="{}"/>"#,
            y + 5.0,
            escape(class),
            left,
            y + 8.0,
            left + plot_width,
            y + 8.0
        ));
    }
    for call in calls {
        let Some(time) = call
            .get("response_observed_unix_ms")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let x = left + (time.saturating_sub(minimum) as f64 / span) * plot_width;
        let duration = call
            .get("execution_duration_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as f64;
        for class in call
            .get("classes")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            if let Some(class_index) = classes.iter().position(|candidate| candidate == class) {
                let y = 65.0 + class_index as f64 * row_height + 3.0;
                let title = format!(
                    "{}: response {} ms; execution {}",
                    call.get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("tool call"),
                    time,
                    call.get("execution_duration_ms")
                        .map_or("unavailable".into(), |value| format!("{} ms", value))
                );
                svg.push_str(&format!(r#"<rect class="cell" x="{x:.2}" y="{y:.2}" width="8" height="10"><title>{}</title></rect>"#, escape(&title)));
                if duration > 0.0 {
                    svg.push_str(&format!(r#"<line class="duration" x1="{x:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}"><title>{}</title></line>"#, y + 5.0, x + (duration / span) * plot_width, y + 5.0, escape(&title)));
                }
            }
        }
    }
    svg.push_str("</svg>");
    Some(ReportGraph {
        filename: format!("tool-call-heatmap-{index}.svg"),
        svg,
    })
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn heatmap_uses_provider_response_timestamps_and_execution_durations() {
        let metrics = BTreeMap::new();
        let documents = BTreeMap::from([(
            "tool-calls".into(),
            serde_json::json!({"calls":[{"name":"read_file","response_observed_unix_ms":100,"execution_duration_ms":25,"classes":["filesystem.read"]}]}),
        )]);
        let graph = heatmap(
            0,
            &ReportRow {
                label: "agent",
                metrics: &metrics,
                components: None,
                documents: &documents,
            },
        )
        .expect("tool call graph");
        assert_eq!(graph.filename, "tool-call-heatmap-0.svg");
        assert!(graph.svg.contains("filesystem.read"));
        assert!(graph.svg.contains("execution 25 ms"));
    }
}
