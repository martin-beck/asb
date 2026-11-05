use super::writer::Record;
use super::*;

fn record(
    benchmark: &BenchmarkData,
    family: &str,
    name: &str,
    unit: &str,
    scope: &str,
    status: &str,
    observation: (&Value, Option<f64>),
) -> Result<Record, Error> {
    let (details, value) = observation;
    Ok(Record {
        text: [
            serde_json::to_string(&benchmark.id).map_err(failure)?,
            benchmark.agent.clone(),
            benchmark.provider.clone(),
            benchmark.model.clone(),
            benchmark.workload.clone(),
            trial_label(benchmark).into(),
            family.into(),
            name.into(),
            unit.into(),
            scope.into(),
            status.into(),
            serde_json::to_string(details).map_err(failure)?,
        ],
        value,
        timestamp: None,
        exact: None,
    })
}

pub(super) fn observations(export: &mut Export, benchmark: &BenchmarkData) -> Result<(), Error> {
    // Adapters retain ownership of clock semantics. Project only declared
    // timestamp/scope metadata; never scrape captures or special-case a tool.
    let references = benchmark
        .collector_documents
        .iter()
        .filter_map(|(name, document)| {
            let fields = [
                "clock",
                "scope",
                "root",
                "target",
                "series_origin",
                "series_timestamp_unit",
                "phase_alignment",
            ]
            .into_iter()
            .filter_map(|key| {
                document
                    .get(key)
                    .map(|value| (key.to_owned(), value.clone()))
            })
            .collect::<serde_json::Map<_, _>>();
            (!fields.is_empty()).then_some((name.clone(), Value::Object(fields)))
        })
        .collect::<BTreeMap<_, _>>();
    let context = json!({"result_source":benchmark.result_source,"execution_cohort":benchmark.execution_cohort,
        "measurement_target":benchmark.collector_documents.get("measurement-target"),
        "time_references":references,"limitations":"Collector-defined scalar semantics; missing metrics are not synthesized. Analysis-specific rows retain their explicit scope and evidence separately."});
    export.push(record(benchmark,"context","host-metadata","not-numeric","retained-host-metadata","retained",
        (&json!({"host_metadata":benchmark.host_metadata,"arm_platform":benchmark.collector_documents.get("arm-platform"),"context":context}),None))?)?;
    for (name, value) in &benchmark.metrics {
        export.push(record(
            benchmark,
            if name.starts_with("host.") {
                "context-value"
            } else {
                "measurement"
            },
            name,
            metric_unit(name),
            if name.starts_with("host.") {
                "retained-host-metadata"
            } else {
                "collector-defined"
            },
            if name.starts_with("host.") {
                "retained"
            } else {
                "observed"
            },
            (&context, Some(*value)),
        )?)?;
    }
    for (name, series) in &benchmark.series {
        for (index, value) in series.values.iter().enumerate() {
            let mut row = record(
                benchmark,
                "series",
                name,
                metric_unit(name),
                "collector-defined",
                if value.is_some() {
                    "observed"
                } else {
                    "unavailable"
                },
                (
                    &json!({"sample_index":index,"time_axis":if series.timestamps_ms.is_some(){"collector-relative seconds"}else{"sample index; no time inferred"},"context":context}),
                    *value,
                ),
            )?;
            row.timestamp = series
                .timestamps_ms
                .as_ref()
                .map(|times| times[index] / 1000.0);
            export.push(row)?;
        }
    }
    let Some(analysis) = benchmark.collector_documents.get("bottlenecks") else {
        return Ok(());
    };
    for family in ["observations", "findings", "kernel", "phase_alignments"] {
        for item in analysis[family].as_array().into_iter().flatten() {
            export.push(record(
                benchmark,
                family,
                item["id"]
                    .as_str()
                    .or(item["rule"].as_str())
                    .or(item["source"].as_str())
                    .unwrap_or("observation"),
                item["unit"].as_str().unwrap_or("not-numeric"),
                item["scope"].as_str().unwrap_or("source-defined"),
                item["availability"]
                    .as_str()
                    .or(item["assessment"].as_str())
                    .or(item["status"].as_str())
                    .unwrap_or("unavailable"),
                (item, item["value"].as_f64()),
            )?)?;
        }
    }
    for family in ["hardware", "sampling"] {
        for group in analysis[family].as_array().into_iter().flatten() {
            for metric in group["metrics"].as_array().into_iter().flatten() {
                export.push(record(
                    benchmark,
                    family,
                    metric["id"].as_str().unwrap_or("metric"),
                    metric["unit"].as_str().unwrap_or("unknown"),
                    group["scope"].as_str().unwrap_or("source-defined"),
                    if metric["value"].is_number() {
                        "observed"
                    } else {
                        "unavailable"
                    },
                    (
                        &json!({"group":group,"metric":metric}),
                        metric["value"].as_f64(),
                    ),
                )?)?;
            }
        }
    }
    for group in analysis["counter_groups"].as_array().into_iter().flatten() {
        for (name, count) in group["raw_integer_counts"]
            .as_object()
            .into_iter()
            .flatten()
        {
            let mut row = record(
                benchmark,
                "counter",
                name,
                "count",
                group["scope"].as_str().unwrap_or("unknown"),
                "observed-unscaled",
                (group, None),
            )?;
            row.exact = Some(
                count
                    .as_u64()
                    .ok_or_else(|| failure("raw counter is not an unsigned integer"))?
                    .to_string(),
            );
            export.push(row)?;
        }
    }
    Ok(())
}

pub(super) fn cohorts(export: &mut Export, report: &Value) -> Result<(), Error> {
    for cohort in report["cohorts"].as_array().into_iter().flatten() {
        let context = &cohort["context"];
        for (name, unit) in [
            ("peak_live_principals_lower_bound", "count"),
            ("multiple_principals_overlap_lower_bound_ms", "ms"),
            ("all_principals_overlap_lower_bound_ms", "ms"),
            ("agent_phase_span_upper_bound_ms", "ms"),
            ("completed_agent_phases_per_second_lower_bound", "phases/s"),
        ] {
            export.push(Record {
                text: [
                    json!({"cohort":cohort["id"]}).to_string(),
                    context["agent"].as_str().unwrap_or("").into(),
                    context["provider"].as_str().unwrap_or("").into(),
                    context["model"].as_str().unwrap_or("").into(),
                    context["workload"].as_str().unwrap_or("").into(),
                    context["trial"].as_str().unwrap_or("").into(),
                    "cohort".into(),
                    name.into(),
                    unit.into(),
                    "verified-native-agent-principal-lifetimes".into(),
                    cohort["status"].as_str().unwrap_or("unavailable").into(),
                    json!({"cohort":cohort,"limitations":report["limitations"]}).to_string(),
                ],
                value: cohort[name].as_f64(),
                timestamp: None,
                exact: None,
            })?;
        }
    }
    Ok(())
}
