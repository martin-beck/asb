//! Offline comparison and SVG chart generation for completed sweeps.

use crate::{Error, read_config};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const PLOT_TOP: f64 = 70.0;
const PLOT_BOTTOM: f64 = 390.0;
const PLOT_RIGHT: f64 = 880.0;
const Y_TICKS: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepFile {
    configs: Vec<String>,
}

#[derive(Debug)]
struct BenchmarkData {
    label: String,
    directory: PathBuf,
    agent: String,
    workload: String,
    provider: String,
    model: String,
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, TimeSeries>,
    status: Option<String>,
    artifacts: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TimeSeries {
    values: Vec<f64>,
    timestamps_ms: Option<Vec<f64>>,
}

/// Compare all completed benchmark measurements referenced by a sweep.
pub fn run(path: &str, output: &mut dyn Write) -> Result<(), Error> {
    let sweep_path = Path::new(path);
    let text = read_config(sweep_path)?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| Error::Input(format!("cannot parse sweep {path}: {error}")))?;
    if !value
        .as_table()
        .is_some_and(|table| table.contains_key("configs"))
    {
        return Err(Error::Input(format!(
            "{path} is not a sweep TOML; expected configs = [...]"
        )));
    }
    let sweep: SweepFile = value
        .try_into()
        .map_err(|error| Error::Input(format!("invalid sweep {path}: {error}")))?;
    if sweep.configs.is_empty() {
        return Err(Error::Input(format!(
            "sweep {path} contains no benchmark configurations"
        )));
    }
    let parent = sweep_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = sweep_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("sweep");
    let sweep_results = parent.join(format!("{stem}-results"));
    let comparison_dir = parent.join("results").join(format!("{stem}-comparison"));
    fs::create_dir_all(&comparison_dir).map_err(|error| {
        Error::Run(format!(
            "cannot create comparison directory {}: {error}",
            comparison_dir.display()
        ))
    })?;

    let mut benchmark_data = Vec::new();
    for (index, reference) in sweep.configs.iter().enumerate() {
        let config = checked_reference(parent, reference, path)?;
        let result_dir = sweep_results.join(format!("{index:03}-{}", safe_name(&config)));
        let measurement_dir = result_dir.join("measurements");
        let run_path = measurement_dir.join("run.json");
        let (agent, workload, provider, model) = benchmark_identity(&config)?;
        let artifacts = artifact_links(parent, &result_dir);
        if !run_path.is_file() {
            benchmark_data.push(BenchmarkData {
                label: format!("{index:03}-{}", safe_name(&config)),
                directory: result_dir,
                agent,
                workload,
                provider,
                model,
                metrics: BTreeMap::new(),
                series: BTreeMap::new(),
                status: Some(format!(
                    "missing measurements: expected {} (run entry failed before writing measurements)",
                    run_path.display()
                )),
                artifacts,
            });
            continue;
        }
        let run_value = read_json(&run_path)?;
        let run_status = if run_value.get("success") == Some(&Value::Bool(false)) {
            Some(format!(
                "benchmark run failed (status={}, exit_code={})",
                run_value
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                run_value
                    .get("exit_code")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".into())
            ))
        } else {
            None
        };
        let mut metrics = BTreeMap::new();
        flatten_numbers("", &run_value, &mut metrics);
        let mut series = BTreeMap::new();
        for entry in
            fs::read_dir(&measurement_dir).map_err(|error| Error::Input(error.to_string()))?
        {
            let file = entry
                .map_err(|error| Error::Input(error.to_string()))?
                .path();
            if file.extension().and_then(|value| value.to_str()) != Some("json")
                || file.file_name().and_then(|value| value.to_str()) == Some("run.json")
            {
                continue;
            }
            let collector = read_json(&file)?;
            let name = file
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("collector");
            if let Some(collector_metrics) = collector.get("metrics") {
                flatten_numbers(name, collector_metrics, &mut metrics);
            }
            // Collector stdout is retained for diagnosis, but is not a
            // numerical series: scraping its last token created noisy and
            // misleading graphs for repeated mpstat/iostat samples.
            if let Some(collector_series) = collector.get("series") {
                let collector_timestamps = collector.get("series_timestamps");
                let timestamp_map = collector_timestamps.and_then(Value::as_object);
                if let Some(series_map) = collector_series.as_object() {
                    for (metric, raw_values) in series_map {
                        let points = series_values(raw_values);
                        if points.len() < 2 {
                            continue;
                        }
                        series.insert(
                            metric.clone(),
                            TimeSeries {
                                values: points,
                                timestamps_ms: timestamp_map
                                    .and_then(|map| map.get(metric))
                                    .map(series_values),
                            },
                        );
                    }
                } else {
                    let points = series_values(collector_series);
                    if points.len() >= 2 {
                        series.insert(
                            name.to_owned(),
                            TimeSeries {
                                values: points,
                                timestamps_ms: None,
                            },
                        );
                    }
                }
            }
        }
        benchmark_data.push(BenchmarkData {
            label: format!("{index:03}-{}", safe_name(&config)),
            directory: result_dir,
            agent,
            workload,
            provider,
            model,
            metrics,
            series,
            status: run_status,
            artifacts,
        });
    }

    let mut metric_values: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    let mut series_values: BTreeMap<String, Vec<(String, TimeSeries)>> = BTreeMap::new();
    for benchmark in &benchmark_data {
        for (name, value) in &benchmark.metrics {
            metric_values
                .entry(name.clone())
                .or_default()
                .push((benchmark.label.clone(), *value));
        }
        for (name, values) in &benchmark.series {
            series_values
                .entry(name.clone())
                .or_default()
                .push((benchmark.label.clone(), values.clone()));
        }
    }
    let mut graphs = Vec::new();
    for (name, values) in &metric_values {
        let filename = format!("metric-{}.svg", safe_name_str(name));
        write_bar_svg(&comparison_dir.join(&filename), name, values)?;
        graphs.push(filename);
    }
    for (name, values) in &series_values {
        let filename = format!("series-{}.svg", safe_name_str(name));
        write_line_svg(&comparison_dir.join(&filename), name, values)?;
        graphs.push(filename);
    }
    let summary = json!({
        "schema_version": 1,
        "sweep": sweep_path,
        "results_directory": sweep_results,
        "comparison_directory": comparison_dir,
        "report": "report.html",
        "benchmarks": benchmark_data.iter().map(|benchmark| json!({
            "name": benchmark.label,
            "directory": benchmark.directory,
            "agent": benchmark.agent,
            "workload": benchmark.workload,
            "provider": benchmark.provider,
            "model": benchmark.model,
            "metrics": benchmark.metrics,
            "series": benchmark.series.iter().map(|(name, series)| (name, &series.values)).collect::<BTreeMap<_, _>>(),
            "series_timestamps": benchmark.series.iter().filter_map(|(name, series)| series.timestamps_ms.as_ref().map(|timestamps| (name, timestamps))).collect::<BTreeMap<_, _>>(),
            "status": benchmark.status,
            "artifacts": benchmark.artifacts,
        })).collect::<Vec<_>>(),
        "graphs": graphs,
    });
    let summary_path = comparison_dir.join("comparison.json");
    let mut file = fs::File::create(&summary_path).map_err(|error| {
        Error::Run(format!("cannot create {}: {error}", summary_path.display()))
    })?;
    serde_json::to_writer_pretty(&mut file, &summary)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", summary_path.display())))?;
    writeln!(file).map_err(|error| Error::Run(error.to_string()))?;
    write_html_report(&comparison_dir, &benchmark_data, &graphs)?;
    writeln!(
        output,
        "Compared {} benchmarks; wrote {} graphs, report.html, and {}",
        benchmark_data.len(),
        graphs.len(),
        comparison_dir.display()
    )
    .map_err(|error| Error::Io(error.to_string()))
}

fn benchmark_identity(path: &Path) -> Result<(String, String, String, String), Error> {
    let text = read_config(path)?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| {
        Error::Input(format!(
            "cannot parse benchmark {}: {error}",
            path.display()
        ))
    })?;
    let table = value
        .as_table()
        .ok_or_else(|| Error::Input(format!("benchmark {} is not a TOML table", path.display())))?;
    let field = |name: &str| {
        table
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Input(format!(
                    "benchmark {} is missing string field `{name}`",
                    path.display()
                ))
            })
    };
    Ok((
        field("agent")?,
        field("workload")?,
        field("provider")?,
        field("model")?,
    ))
}

fn artifact_links(sweep_parent: &Path, result_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    // Measurement artifacts are the primary evidence for a comparison. Add
    // them before the bounded recursive inventory so a large external
    // workload workspace cannot crowd the bpftrace/perfetto SVGs out of the
    // HTML report's artifact links.
    let measurement_dir = result_dir.join("measurements");
    if let Ok(entries) = fs::read_dir(&measurement_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("bpftrace-")
                            || name.starts_with("perfetto-metric-")
                            || name == "flamegraph.svg"
                            || name == "flamegraph.json"
                    })
            {
                files.push(path);
            }
        }
    }
    // Never inventory the benchmark workspace: external workloads can contain
    // thousands of source files and those are not comparison evidence. The
    // measurements directory is the bounded, machine-readable artifact set.
    collect_artifacts(&measurement_dir, 0, &mut files);
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) != Some("run.json"))
        .filter_map(|path| path.strip_prefix(sweep_parent).ok().map(PathBuf::from))
        .map(|path| format!("../../{}", path.to_string_lossy().replace('\\', "/")))
        .collect()
}

fn collect_artifacts(path: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 4 || output.len() >= 256 {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_file() {
            output.push(child);
        } else if child.is_dir() {
            collect_artifacts(&child, depth + 1, output);
        }
        if output.len() >= 256 {
            return;
        }
    }
}

fn write_html_report(
    comparison_dir: &Path,
    benchmarks: &[BenchmarkData],
    graphs: &[String],
) -> Result<(), Error> {
    let mut dimensions: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    dimensions.insert(
        "benchmark",
        benchmarks.iter().map(|item| item.label.clone()).collect(),
    );
    dimensions.insert(
        "agent",
        benchmarks.iter().map(|item| item.agent.clone()).collect(),
    );
    dimensions.insert(
        "provider",
        benchmarks
            .iter()
            .map(|item| item.provider.clone())
            .collect(),
    );
    dimensions.insert(
        "model",
        benchmarks.iter().map(|item| item.model.clone()).collect(),
    );
    dimensions.insert(
        "workload",
        benchmarks
            .iter()
            .map(|item| item.workload.clone())
            .collect(),
    );
    for values in dimensions.values_mut() {
        values.sort();
        values.dedup();
    }

    let mut html = String::from(HTML_HEAD);
    html = html.replacen(
        "</style></head>",
        &format!(
            "{}\n{}\n{}\n</style></head>",
            INTERACTIVE_STYLE, MODAL_STYLE, INLINE_SVG_STYLE
        ),
        1,
    );
    html.push_str("<main class=\"shell\">");
    html.push_str("<header class=\"hero\"><p class=\"eyebrow\">ASB BENCHMARK REPORT</p><h1>Sweep comparison</h1><p>Explore benchmark results and measurements from this sweep. Use the filters to focus the report without rerunning anything.</p></header>");
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Each row is one agent/provider/model/workload combination. Runtime, success rate, token usage, and structured system measurements are summarized in the table and charts. The complete machine-readable evidence inventory is available in comparison.json.</p></section>");
    html.push_str("<section class=\"filters\"><div class=\"section-title\"><h2>Filter results</h2><div><button type=\"button\" data-action=\"all\">Select all</button><button type=\"button\" data-action=\"none\">Clear all</button></div></div><div class=\"filter-grid\">");
    for (group, values) in &dimensions {
        html.push_str(&format!("<fieldset><legend>{}</legend>", title_case(group)));
        for value in values {
            let escaped = escape(value);
            html.push_str(&format!("<label><input type=\"checkbox\" data-group=\"{group}\" value=\"{escaped}\" checked><span>{escaped}</span></label>"));
        }
        html.push_str("</fieldset>");
    }
    html.push_str("</div><p class=\"filter-count\" id=\"filter-count\"></p></section>");
    html.push_str("<section><div class=\"section-title\"><h2>Benchmark results</h2><a class=\"text-link\" href=\"comparison.json\">Download comparison.json</a></div><div class=\"table-wrap\"><table><thead><tr><th>Benchmark</th><th>Agent</th><th>Provider</th><th>Model</th><th>Workload</th><th>Runtime</th><th>Success</th><th>Tokens</th><th>Details</th></tr></thead><tbody id=\"results\">");
    for benchmark in benchmarks {
        let runtime = benchmark
            .metrics
            .get("elapsed_ms")
            .map_or_else(|| "—".into(), |value| format!("{value:.0} ms"));
        let success = benchmark
            .metrics
            .get("success_rate")
            .map_or_else(|| "—".into(), |value| format!("{:.1}%", value * 100.0));
        let tokens = benchmark
            .metrics
            .get("token_usage.total_tokens")
            .map_or_else(|| "—".into(), |value| format!("{value:.0}"));
        let metrics_details = if benchmark.metrics.is_empty() {
            "<p class=\"muted\">No numeric measurements were written.</p>".to_owned()
        } else {
            let items = benchmark
                .metrics
                .iter()
                .map(|(name, value)| format!("<dt>{}</dt><dd>{value:.4}</dd>", escape(name)))
                .collect::<String>();
            format!("<details><summary>All metrics</summary><dl>{items}</dl></details>")
        };
        let artifacts = if benchmark.artifacts.is_empty() {
            String::new()
        } else {
            let links = benchmark
                .artifacts
                .iter()
                .map(|artifact| {
                    format!(
                        "<li><a href=\"{}\" download>{}</a></li>",
                        escape(artifact),
                        escape(artifact.rsplit('/').next().unwrap_or(artifact))
                    )
                })
                .collect::<String>();
            let perfetto_svgs = benchmark
                .artifacts
                .iter()
                .filter(|artifact| {
                    artifact.contains("/perfetto-metric-") && artifact.ends_with(".svg")
                })
                .map(|artifact| {
                    inline_svg_artifact(
                        comparison_dir,
                        artifact,
                        "perfetto-metric",
                        "Perfetto metric SVG",
                    )
                })
                .collect::<Result<String, Error>>()?;
            let bpftrace_svgs = benchmark
                .artifacts
                .iter()
                .filter(|artifact| {
                    artifact
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.starts_with("bpftrace-") && name.ends_with(".svg"))
                })
                .map(|artifact| {
                    inline_svg_artifact(
                        comparison_dir,
                        artifact,
                        "bpftrace-profile",
                        "bpftrace profile SVG",
                    )
                })
                .collect::<Result<String, Error>>()?;
            let flamegraph_svgs = benchmark
                .artifacts
                .iter()
                .filter(|artifact| {
                    artifact
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name == "flamegraph.svg")
                })
                .map(|artifact| {
                    inline_svg_artifact(comparison_dir, artifact, "flamegraph", "Flamegraph SVG")
                })
                .collect::<Result<String, Error>>()?;
            let flamegraph_status = benchmark
                .artifacts
                .iter()
                .filter(|artifact| {
                    artifact.rsplit('/').next().is_some_and(|name| name == "flamegraph.json")
                })
                .map(|artifact| {
                    format!(
                        "<p class=\"flamegraph-status\">Flamegraph collector status: <a href=\"{}\">flamegraph.json</a>. An SVG is embedded here when collection completed.</p>",
                        escape(artifact)
                    )
                })
                .collect::<String>();
            format!(
                "<details><summary>Run artifacts</summary>{perfetto_svgs}{bpftrace_svgs}{flamegraph_svgs}{flamegraph_status}<ul class=\"artifact-list\">{links}</ul></details>"
            )
        };
        html.push_str(&format!("<tr data-dimension-benchmark=\"{}\" data-dimension-agent=\"{}\" data-dimension-provider=\"{}\" data-dimension-model=\"{}\" data-dimension-workload=\"{}\"><td><strong>{}</strong></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"details\">{}{}{}</td></tr>",
            escape(&benchmark.label), escape(&benchmark.agent), escape(&benchmark.provider), escape(&benchmark.model), escape(&benchmark.workload),
            escape(&benchmark.label), escape(&benchmark.agent), escape(&benchmark.provider), escape(&benchmark.model), escape(&benchmark.workload),
            runtime, success, tokens,
            metrics_details,
            artifacts,
            benchmark.status.as_ref().map_or(String::new(), |status| format!("<span class=\"status\">{}</span>", escape(status))),
            ));
    }
    let bpftrace_graphs = graphs
        .iter()
        .filter(|graph| {
            graph.starts_with("metric-bpftrace-") || graph.starts_with("series-bpftrace-")
        })
        .collect::<Vec<_>>();
    if !bpftrace_graphs.is_empty() {
        html.push_str("</tbody></table></div></section><section><div class=\"section-title\"><h2>bpftrace profiles</h2><p class=\"muted\">Cross-run SVG graphs for every bpftrace aggregate emitted by the selected profiles.</p></div><div class=\"charts\">");
        for graph in &bpftrace_graphs {
            html.push_str(&chart_figure(&comparison_dir, graph)?);
        }
        html.push_str("</div></section>");
    } else {
        html.push_str("</tbody></table></div></section>");
    }
    html.push_str("<section><div class=\"section-title\"><h2>Charts</h2><p class=\"muted\">SVG files are linked below and remain available for download.</p></div><div class=\"charts\">");
    for graph in graphs
        .iter()
        .filter(|graph| !bpftrace_graphs.iter().any(|bpf| *bpf == *graph))
    {
        html.push_str(&chart_figure(&comparison_dir, graph)?);
    }
    html.push_str("</div></section><footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str(HTML_SCRIPT);
    fs::write(comparison_dir.join("report.html"), html)
        .map_err(|error| Error::Run(format!("cannot write HTML report: {error}")))
}

fn chart_figure(comparison_dir: &Path, graph: &str) -> Result<String, Error> {
    let svg = fs::read_to_string(comparison_dir.join(graph))
        .map_err(|error| Error::Run(format!("cannot read generated chart {}: {error}", graph)))?;
    Ok(format!(
        "<figure class=\"interactive-chart\" data-graph=\"{}\"><div class=\"chart-backdrop\" aria-hidden=\"true\"></div><div class=\"chart-toolbar\"><strong>{}</strong><span><button type=\"button\" class=\"chart-reset\">Reset view</button><button type=\"button\" class=\"chart-expand\" aria-label=\"Enlarge chart\">Enlarge</button><button type=\"button\" class=\"chart-close\" aria-label=\"Close enlarged chart\" hidden>Close</button></span></div><div class=\"chart-stage\"><div class=\"chart-tooltip\" role=\"status\" aria-live=\"polite\"></div>{svg}</div><figcaption><a href=\"{}\" download>Download SVG</a></figcaption></figure>",
        escape(graph),
        escape(graph.strip_suffix(".svg").unwrap_or(graph)),
        escape(graph),
    ))
}

fn inline_svg_artifact(
    comparison_dir: &Path,
    artifact: &str,
    class: &str,
    fallback: &str,
) -> Result<String, Error> {
    let path = comparison_dir.join(artifact);
    let metadata = fs::metadata(&path).map_err(|error| {
        Error::Run(format!(
            "cannot read embedded SVG artifact {artifact}: {error}"
        ))
    })?;
    if metadata.len() > MAX_BYTES {
        return Err(Error::Run(format!(
            "embedded SVG artifact {artifact} exceeds 16 MiB"
        )));
    }
    let svg = fs::read_to_string(&path).map_err(|error| {
        Error::Run(format!(
            "cannot read embedded SVG artifact {artifact}: {error}"
        ))
    })?;
    let trimmed = svg.trim_start();
    if !trimmed.starts_with("<svg") && !trimmed.starts_with("<?xml") {
        return Err(Error::Run(format!(
            "embedded SVG artifact {artifact} does not contain an SVG document"
        )));
    }
    Ok(format!(
        "<div class=\"embedded-svg {class}\" aria-label=\"{}\">{svg}</div>",
        escape(fallback)
    ))
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

const HTML_HEAD: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>ASB sweep report</title><style>
:root{color-scheme:light;--ink:#18212b;--muted:#61707f;--line:#dce4ec;--panel:#fff;--accent:#1769aa;--accent-soft:#e8f2fa;--bg:#f4f7fa}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}.shell{max-width:1500px;margin:auto;padding:36px 28px 56px}.hero{background:linear-gradient(135deg,#12344d,#1769aa);color:#fff;border-radius:18px;padding:42px 46px;box-shadow:0 14px 32px #12344d33}.hero h1{font-size:clamp(2rem,4vw,3.4rem);line-height:1.05;margin:4px 0 14px;letter-spacing:-.04em}.hero p{max-width:760px;margin:0;color:#e5f2fb;font-size:16px}.eyebrow{font-size:11px!important;letter-spacing:.16em;font-weight:700}.explain,.filters,section{background:var(--panel);border:1px solid var(--line);border-radius:14px;margin-top:20px;padding:24px;box-shadow:0 4px 14px #17324d0b}.explain{border-left:4px solid var(--accent)}h2{font-size:19px;margin:0}.section-title{display:flex;align-items:center;justify-content:space-between;gap:16px;margin-bottom:16px}.filter-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr));gap:14px}fieldset{border:1px solid var(--line);border-radius:10px;padding:10px 12px;min-width:0}legend{font-weight:700;color:var(--accent);padding:0 5px}label{display:flex;gap:8px;align-items:flex-start;margin:6px 0;color:var(--muted);overflow-wrap:anywhere}input{accent-color:var(--accent);margin-top:4px}.filter-count,.muted{color:var(--muted);margin:14px 0 0}.text-link,a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}button{background:#fff;border:1px solid #b9c9d8;border-radius:7px;color:var(--accent);cursor:pointer;padding:7px 11px;margin-left:6px}button:hover{background:var(--accent-soft)}.table-wrap{overflow:auto}table{border-collapse:collapse;min-width:100%;white-space:nowrap}th{text-align:left;background:#eef4f8;color:#405466;font-size:12px;letter-spacing:.04em;text-transform:uppercase}th,td{border-bottom:1px solid var(--line);padding:12px 10px;vertical-align:top}.links{white-space:normal;min-width:170px}.links a{display:inline-block;margin:0 8px 5px 0;font-size:12px}.artifact-list{padding-left:18px;margin:8px 0;white-space:normal}.perfetto-metric{display:block;width:min(760px,100%);height:180px;border:0;margin:8px 0}.status{display:block;color:#9a4b00;background:#fff4e5;border-radius:5px;font-size:11px;padding:3px 6px;margin-top:4px;max-width:260px;white-space:normal}.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:18px}.charts figure{margin:0;border:1px solid var(--line);border-radius:10px;padding:10px;background:#fbfdff}.charts img{width:100%;height:auto;display:block}.charts figcaption{padding:8px 2px 2px;font-size:12px;overflow-wrap:anywhere}footer{color:var(--muted);text-align:center;padding:28px 0 0}code{background:#e9eef3;border-radius:4px;padding:2px 5px}@media(max-width:700px){.shell{padding:18px 12px}.hero{padding:30px 24px}.section-title{align-items:flex-start;flex-direction:column}.section-title button{margin:0 6px 0 0}}
</style></head><body>"#;

const INTERACTIVE_STYLE: &str = r#".interactive-chart{position:relative;overflow:hidden}.chart-toolbar{display:flex;align-items:center;justify-content:space-between;gap:12px;margin:0 0 8px;color:var(--ink);font-size:12px}.chart-toolbar strong{overflow-wrap:anywhere}.chart-toolbar button{margin:0;font-size:12px}.chart-stage{position:relative;overflow:hidden;border:1px solid var(--line);border-radius:8px;background:linear-gradient(180deg,#fff,#f7fbfe)}.chart-stage svg{display:block;width:100%;height:auto;min-height:260px;cursor:grab;touch-action:none;user-select:none}.chart-stage svg.is-panning{cursor:grabbing}.chart-stage .bar{transition:fill .12s,opacity .12s}.chart-stage .bar.hovered{fill:#d95f02;stroke:#8f3f00;stroke-width:1.5}.chart-stage .series{transition:stroke-width .12s,opacity .12s}.chart-stage .series.hovered{stroke-width:4;filter:drop-shadow(0 2px 2px #18212b44)}.chart-stage .series-point{pointer-events:all;transition:fill .12s,r .12s}.chart-stage .series-point.hovered{fill:#d95f02;stroke:#fff;stroke-width:2;r:7}.chart-tooltip{position:absolute;z-index:2;pointer-events:none;opacity:0;transform:translateY(-3px);padding:7px 10px;border:1px solid #17324d33;border-radius:7px;background:#18212bf2;color:#fff;box-shadow:0 6px 16px #17324d44;font-size:12px;line-height:1.3;white-space:nowrap;transition:opacity .12s}.chart-tooltip.visible{opacity:1}.interactive-chart figcaption{display:flex;justify-content:space-between;gap:8px}.interactive-chart figcaption::after{content:'Hover values · wheel zoom · drag pan';color:var(--muted);font-size:11px}.bpftrace-profile{display:block;width:min(760px,100%);height:220px;border:0;margin:8px 0}.flamegraph{display:block;width:min(1100px,100%);height:420px;border:1px solid var(--line);border-radius:8px;margin:10px 0;background:#fff}.flamegraph-status{color:var(--muted);font-size:12px;margin:8px 0}"#;

const MODAL_STYLE: &str = r#"body.chart-open{overflow:hidden}.chart-backdrop{display:none;position:fixed;inset:0;background:#081522c9;z-index:30}.interactive-chart.expanded{position:fixed;z-index:31;left:10vw;top:10vh;width:80vw;height:80vh;margin:0;padding:18px;background:var(--panel);border:1px solid #b9c9d8;border-radius:16px;box-shadow:0 24px 80px #08152299}.interactive-chart.expanded .chart-backdrop{display:block}.interactive-chart.expanded .chart-toolbar{position:relative;z-index:32}.interactive-chart.expanded .chart-stage{height:calc(80vh - 92px);min-height:0}.interactive-chart.expanded .chart-stage svg{height:100%;min-height:0}.interactive-chart.expanded figcaption{position:relative;z-index:32}@media(max-width:700px){.interactive-chart.expanded{left:3vw;top:3vh;width:94vw;height:94vh;padding:10px}.interactive-chart.expanded .chart-stage{height:calc(94vh - 78px)}}"#;

const INLINE_SVG_STYLE: &str = r#".embedded-svg{display:block;overflow:auto;border:1px solid var(--line);border-radius:8px;background:#fff;margin:8px 0}.embedded-svg svg{display:block;width:100%;height:auto}.embedded-svg.perfetto-metric{width:min(760px,100%);max-height:260px}.embedded-svg.bpftrace-profile{width:min(760px,100%);max-height:300px}.embedded-svg.flamegraph{width:100%;max-height:520px}.embedded-svg.flamegraph svg{min-width:900px}"#;

const INTERACTIVE_SCRIPT: &str = r#"<script>
document.querySelectorAll('.interactive-chart').forEach(chart=>{
  const stage=chart.querySelector('.chart-stage');
  const svg=stage.querySelector('svg');
  const tooltip=chart.querySelector('.chart-tooltip');
  const reset=chart.querySelector('.chart-reset');
  const expand=chart.querySelector('.chart-expand');
  const close=chart.querySelector('.chart-close');
  const backdrop=chart.querySelector('.chart-backdrop');
  const original=svg.getAttribute('viewBox').split(/\s+/).map(Number);
  let view={x:original[0],y:original[1],w:original[2],h:original[3]};
  let drag=null;
  const interactive=[...svg.querySelectorAll('.bar,.series,.series-point')];
  const clearHover=()=>interactive.forEach(item=>item.classList.remove('hovered'));
  const positionTooltip=(event)=>{
    const bounds=stage.getBoundingClientRect();
    const left=Math.min(Math.max(8,event.clientX-bounds.left+12),Math.max(8,bounds.width-tooltip.offsetWidth-8));
    const top=Math.min(Math.max(8,event.clientY-bounds.top-42),Math.max(8,bounds.height-tooltip.offsetHeight-8));
    tooltip.style.left=`${left}px`;tooltip.style.top=`${top}px`;
  };
  const show=(item,event)=>{
    clearHover();
    item.classList.add('hovered');
    if(item.classList.contains('series-point')){
      const series=svg.querySelector(`.series[data-series-index="${item.dataset.seriesIndex}"]`);
      if(series)series.classList.add('hovered');
    }
    tooltip.textContent=item.dataset.timestamp?`${item.dataset.label}: ${item.dataset.value} @ ${item.dataset.timestamp} ms`:`${item.dataset.label}: ${item.dataset.value}`;
    tooltip.classList.add('visible');
    positionTooltip(event);
  };
  interactive.forEach(item=>item.addEventListener('pointerenter',event=>show(item,event)));
  stage.addEventListener('pointermove',event=>{if(!drag)positionTooltip(event)});
  stage.addEventListener('pointerleave',()=>{clearHover();tooltip.classList.remove('visible')});
  const applyView=()=>svg.setAttribute('viewBox',`${view.x} ${view.y} ${view.w} ${view.h}`);
  const setExpanded=expanded=>{
    chart.classList.toggle('expanded',expanded);
    document.body.classList.toggle('chart-open',expanded);
    expand.hidden=expanded;close.hidden=!expanded;
    if(expanded)close.focus();else expand.focus();
  };
  svg.addEventListener('wheel',event=>{
    event.preventDefault();
    const bounds=svg.getBoundingClientRect();
    const factor=event.deltaY<0?.85:1.15;
    const mx=view.x+(event.clientX-bounds.left)/bounds.width*view.w;
    const my=view.y+(event.clientY-bounds.top)/bounds.height*view.h;
    const w=Math.min(original[2],Math.max(original[2]/8,view.w*factor));
    const h=Math.min(original[3],Math.max(original[3]/8,view.h*factor));
    view.x=Math.max(0,Math.min(original[2]-w,mx-(mx-view.x)*w/view.w));
    view.y=Math.max(0,Math.min(original[3]-h,my-(my-view.y)*h/view.h));
    view.w=w;view.h=h;applyView();
  },{passive:false});
  svg.addEventListener('pointerdown',event=>{
    if(event.target.closest('.bar,.series-point'))return;
    drag={x:event.clientX,y:event.clientY,view:{...view}};svg.classList.add('is-panning');svg.setPointerCapture(event.pointerId);
  });
  svg.addEventListener('pointermove',event=>{
    if(!drag)return;
    const bounds=svg.getBoundingClientRect();
    view.x=Math.max(0,Math.min(original[2]-view.w,drag.view.x-(event.clientX-drag.x)/bounds.width*view.w));
    view.y=Math.max(0,Math.min(original[3]-view.h,drag.view.y-(event.clientY-drag.y)/bounds.height*view.h));
    applyView();
  });
  const stopDrag=event=>{if(!drag)return;drag=null;svg.classList.remove('is-panning');if(svg.hasPointerCapture(event.pointerId))svg.releasePointerCapture(event.pointerId)};
  svg.addEventListener('pointerup',stopDrag);svg.addEventListener('pointercancel',stopDrag);
  reset.addEventListener('click',()=>{view={x:original[0],y:original[1],w:original[2],h:original[3]};applyView()});
  expand.addEventListener('click',()=>setExpanded(true));
  close.addEventListener('click',()=>setExpanded(false));
  backdrop.addEventListener('click',()=>setExpanded(false));
  document.addEventListener('keydown',event=>{if(event.key==='Escape'&&chart.classList.contains('expanded'))setExpanded(false)});
});
</script>"#;

const HTML_SCRIPT: &str = r#"<script>
const groups=[...document.querySelectorAll('input[data-group]')];const rows=[...document.querySelectorAll('#results tr')];const count=document.querySelector('#filter-count');
function selected(group){return new Set(groups.filter(i=>i.dataset.group===group&&i.checked).map(i=>i.value))}function update(){const sets=Object.fromEntries(['benchmark','agent','provider','model','workload'].map(g=>[g,selected(g)]));let visible=0;rows.forEach(row=>{const show=Object.entries(sets).every(([g,set])=>set.has(row.dataset['dimension'+g[0].toUpperCase()+g.slice(1)]));row.hidden=!show;if(show)visible++});count.textContent=`Showing ${visible} of ${rows.length} benchmarks. Charts above summarize the complete sweep.`}groups.forEach(i=>i.addEventListener('change',update));document.querySelectorAll('button[data-action]').forEach(button=>button.addEventListener('click',()=>{groups.forEach(i=>i.checked=button.dataset.action==='all');update()}));update();
</script></body></html>"#;

fn checked_reference(parent: &Path, reference: &str, sweep: &str) -> Result<PathBuf, Error> {
    let path = Path::new(reference);
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(Error::Input(format!(
            "sweep {sweep} contains unsafe benchmark path {reference}"
        )));
    }
    if path.extension().and_then(|value| value.to_str()) != Some("toml") {
        return Err(Error::Input(format!(
            "sweep {sweep} entry {reference} is not a .toml benchmark"
        )));
    }
    let resolved = parent.join(path);
    if !resolved.is_file() {
        return Err(Error::Input(format!(
            "sweep {sweep} references missing benchmark {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn read_json(path: &Path) -> Result<Value, Error> {
    let metadata = fs::metadata(path)
        .map_err(|error| Error::Input(format!("cannot read {}: {error}", path.display())))?;
    if metadata.len() > MAX_BYTES {
        return Err(Error::Input(format!(
            "measurement file {} exceeds 16 MiB",
            path.display()
        )));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| Error::Input(format!("cannot read {}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| {
        Error::Input(format!(
            "invalid measurement JSON {}: {error}",
            path.display()
        ))
    })
}

fn flatten_numbers(prefix: &str, value: &Value, output: &mut BTreeMap<String, f64>) {
    match value {
        Value::Number(number) => {
            if let Some(number) = number.as_f64()
                && !prefix.is_empty()
                && !prefix.ends_with("schema_version")
                && !prefix.ends_with("started_unix_ms")
            {
                output.insert(prefix.to_owned(), number);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_numbers(&next, child, output);
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                flatten_numbers(&format!("{prefix}[{index}]"), child, output);
            }
        }
        _ => {}
    }
}

fn series_values(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .take(2_048)
        .collect()
}

fn write_bar_svg(path: &Path, title: &str, values: &[(String, f64)]) -> Result<(), Error> {
    let max = values
        .iter()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let width = 900.0;
    let height = 460.0;
    let left = 70.0;
    let bottom = PLOT_BOTTOM;
    let bar_width = (width - left - 20.0) / values.len().max(1) as f64;
    let mut svg = svg_header(width, height, title);
    append_y_axis(&mut svg, left, PLOT_TOP, bottom, PLOT_RIGHT, max, title);
    for (index, (label, value)) in values.iter().enumerate() {
        let x = left + index as f64 * bar_width + 4.0;
        let bar_height = value.max(0.0) / max * (bottom - PLOT_TOP);
        let y = bottom - bar_height;
        let width = (bar_width - 8.0).max(2.0);
        svg.push_str(&format!(
            "<rect class=\"bar\" data-label=\"{}\" data-value=\"{value}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{width:.1}\" height=\"{bar_height:.1}\" fill=\"#4472c4\"><title>{}: {value}</title></rect>",
            escape(label),
            escape(label),
        ));
        let value_label = format_tick(*value);
        let (label_y, placement, color) = if bar_height >= 24.0 {
            // Keep a small top inset while retaining enough contrast against
            // the bar fill for the value to remain readable.
            (y + 16.0, "inside", "#ffffff")
        } else {
            // Short and zero-height bars cannot contain a label. Place the
            // baseline just above the bar with the same small spacing.
            (y - 5.0, "above", "#18212b")
        };
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{label_y:.1}\" text-anchor=\"middle\" class=\"bar-value\" data-placement=\"{placement}\" font-size=\"11\" font-weight=\"600\" fill=\"{color}\">{value_label}</text>",
            x + width / 2.0
        ));
        svg.push_str(&format!("<text x=\"{:.1}\" y=\"410\" transform=\"rotate(35 {:.1} 410)\" font-size=\"11\">{}</text>", x, x, escape(label)));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn write_line_svg(path: &Path, title: &str, values: &[(String, TimeSeries)]) -> Result<(), Error> {
    let max = values
        .iter()
        .flat_map(|(_, points)| points.values.iter().copied())
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let timestamp_bounds = values
        .iter()
        .filter_map(|(_, points)| points.timestamps_ms.as_ref())
        .flat_map(|timestamps| timestamps.iter().copied())
        .fold(None, |bounds: Option<(f64, f64)>, timestamp| {
            Some(match bounds {
                Some((min, max)) => (min.min(timestamp), max.max(timestamp)),
                None => (timestamp, timestamp),
            })
        });
    let mut svg = svg_header(900.0, 460.0, title);
    append_y_axis(
        &mut svg,
        70.0,
        PLOT_TOP,
        PLOT_BOTTOM,
        PLOT_RIGHT,
        max,
        title,
    );
    for (series_index, (label, points)) in values.iter().enumerate() {
        let color = ["#4472c4", "#ed7d31", "#70ad47", "#a5a5a5", "#ffc000"][series_index % 5];
        let coordinates = points
            .values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let x = line_x(
                    index,
                    points.timestamps_ms.as_deref(),
                    timestamp_bounds,
                    points.values.len(),
                );
                let y = PLOT_BOTTOM - value.max(0.0) / max * (PLOT_BOTTOM - PLOT_TOP);
                format!("{x:.1},{y:.1}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        svg.push_str(&format!(
            "<polyline class=\"series\" data-series-index=\"{series_index}\" data-label=\"{}\" points=\"{coordinates}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\"/>",
            escape(label)
        ));
        for (index, value) in points.values.iter().enumerate() {
            let x = line_x(
                index,
                points.timestamps_ms.as_deref(),
                timestamp_bounds,
                points.values.len(),
            );
            let y = PLOT_BOTTOM - value.max(0.0) / max * (PLOT_BOTTOM - PLOT_TOP);
            let timestamp = points
                .timestamps_ms
                .as_ref()
                .and_then(|timestamps| timestamps.get(index))
                .map_or_else(String::new, |timestamp| {
                    format!(" data-timestamp=\"{timestamp}\"")
                });
            svg.push_str(&format!(
                "<circle class=\"series-point\" data-series-index=\"{series_index}\" data-label=\"{}\" data-value=\"{value}\"{timestamp} cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\" fill=\"transparent\" stroke=\"none\"/>",
                escape(label)
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"55\" font-size=\"12\" fill=\"{color}\">{}</text>",
            80 + series_index * 150,
            escape(label)
        ));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn line_x(
    index: usize,
    timestamps: Option<&[f64]>,
    bounds: Option<(f64, f64)>,
    length: usize,
) -> f64 {
    let fraction = timestamps
        .and_then(|timestamps| timestamps.get(index))
        .zip(bounds)
        .map_or_else(
            || index as f64 / length.saturating_sub(1).max(1) as f64,
            |(timestamp, (min, max))| {
                if max > min {
                    (*timestamp - min) / (max - min)
                } else {
                    index as f64 / length.saturating_sub(1).max(1) as f64
                }
            },
        );
    75.0 + fraction.clamp(0.0, 1.0) * 800.0
}

fn append_y_axis(
    svg: &mut String,
    left: f64,
    top: f64,
    bottom: f64,
    right: f64,
    max: f64,
    title: &str,
) {
    svg.push_str(&format!(
        "<line x1=\"{left}\" y1=\"{top}\" x2=\"{left}\" y2=\"{bottom}\" stroke=\"black\"/><line x1=\"{left}\" y1=\"{bottom}\" x2=\"{right}\" y2=\"{bottom}\" stroke=\"black\"/>"
    ));
    for tick in 0..=Y_TICKS {
        let fraction = tick as f64 / Y_TICKS as f64;
        let y = bottom - fraction * (bottom - top);
        let value = max * fraction;
        svg.push_str(&format!(
            "<line x1=\"{left}\" y1=\"{y:.1}\" x2=\"{right}\" y2=\"{y:.1}\" stroke=\"#dddddd\"/><text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"11\">{}</text>",
            left - 6.0,
            y + 4.0,
            format_tick(value)
        ));
    }
    let unit = metric_unit(title);
    svg.push_str(&format!(
        "<text x=\"15\" y=\"230\" text-anchor=\"middle\" transform=\"rotate(-90 15 230)\" font-size=\"12\">unit: {}</text>",
        escape(unit)
    ));
}

fn format_tick(value: f64) -> String {
    if value.abs() >= 100.0 || value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn metric_unit(title: &str) -> &'static str {
    let name = title.to_ascii_lowercase();
    if name.ends_with("_pct") {
        "%"
    } else if name.ends_with("_ms") || name.contains("elapsed_ms") {
        "ms"
    } else if name.ends_with("_kib_per_s") {
        "KiB/s"
    } else if name.ends_with("_ops_per_s") {
        "ops/s"
    } else if name.ends_with("_per_s") {
        "/s"
    } else if name.ends_with("_kib") {
        "KiB"
    } else if name.ends_with("_depth") {
        "depth"
    } else if name.contains("elapsed_s") || name.ends_with("_seconds") {
        "s"
    } else if name.contains("token") {
        "tokens"
    } else if name == "steps" || name.ends_with("_steps") {
        "steps"
    } else if name.contains("success_rate") || name.ends_with("_rate") {
        "ratio"
    } else if name.contains("count") {
        "count"
    } else {
        "value"
    }
}

fn svg_header(width: f64, height: f64, title: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><title>{}</title><text x=\"20\" y=\"30\" font-size=\"18\">{}</text>",
        escape(title),
        escape(title)
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn safe_name(path: &Path) -> String {
    safe_name_str(
        path.file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("benchmark"),
    )
}

fn safe_name_str(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn compares_measurements_and_writes_svg() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sweep-results/000-one/measurements")).unwrap();
        std::fs::create_dir_all(root.join("sweep-results/000-one/workspace")).unwrap();
        for index in 0..300 {
            std::fs::write(
                root.join(format!("sweep-results/000-one/workspace/file-{index}.txt")),
                "external workload evidence",
            )
            .unwrap();
        }
        std::fs::write(
            root.join("one.toml"),
            "agent=\"codex\"\nworkload=\"bug-fix\"\nprovider=\"local\"\nmodel=\"m\"\n",
        )
        .unwrap();
        std::fs::write(root.join("sweep.toml"), "configs=[\"one.toml\"]\n").unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/run.json"),
            r#"{"success_rate":1.0,"elapsed_ms":12,"token_usage":{"total_tokens":7}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/mpstat.json"),
            r#"{"stdout":"sample 1\nsample 2\n","metrics":{"cpu.user_pct":12.5,"cpu.iowait_pct":3.0}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/perfetto.json"),
            r#"{"metrics":{"perfetto.scheduler_events":24,"perfetto.tracepoints":4},"series":{"perfetto.scheduler_events_per_s":[3,7]},"series_timestamps":{"perfetto.scheduler_events_per_s":[12000,13000]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/perfetto-metric-scheduler_events.svg"),
            "<svg><title>Perfetto scheduler events</title></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/bpftrace-sched-latency.json"),
            r#"{"metrics":{"bpftrace.sched-latency.wakeup_latency_events":7}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/bpftrace-sched-latency.svg"),
            "<svg><title>bpftrace sched-latency</title></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/flamegraph.svg"),
            "<svg><title>flamegraph</title></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/flamegraph.json"),
            r#"{"status":"completed","artifact":"flamegraph.svg"}"#,
        )
        .unwrap();
        run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap();
        assert!(
            root.join("results/sweep-comparison/comparison.json")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/metric-elapsed_ms.svg")
                .is_file()
        );
        let elapsed_svg =
            std::fs::read_to_string(root.join("results/sweep-comparison/metric-elapsed_ms.svg"))
                .unwrap();
        assert!(elapsed_svg.contains("text-anchor=\"end\""));
        assert!(elapsed_svg.contains("unit: ms"));
        assert!(elapsed_svg.contains(">0</text>"));
        assert!(elapsed_svg.contains(">12</text>"));
        assert!(
            root.join("results/sweep-comparison/metric-mpstat.cpu.user_pct.svg")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/metric-perfetto.perfetto.scheduler_events.svg")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/series-perfetto.scheduler_events_per_s.svg")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/metric-bpftrace-sched-latency.bpftrace.sched-latency.wakeup_latency_events.svg")
                .is_file()
        );
        assert!(
            !root
                .join("results/sweep-comparison/series-mpstat.svg")
                .exists()
        );
        let report =
            std::fs::read_to_string(root.join("results/sweep-comparison/report.html")).unwrap();
        assert!(report.contains("Filter results"));
        assert!(report.contains("data-group=\"agent\""));
        assert!(report.contains(">Details</th>"));
        assert!(report.contains("All metrics"));
        assert!(!report.contains("run.json"));
        assert!(report.contains("Perfetto metric SVG"));
        assert!(report.contains("perfetto-metric-scheduler_events.svg"));
        assert!(report.contains("<div class=\"embedded-svg perfetto-metric\""));
        assert!(report.contains("bpftrace profile SVG"));
        assert!(report.contains("bpftrace-sched-latency.svg"));
        assert!(report.contains(
            "<div class=\"embedded-svg bpftrace-profile\" aria-label=\"bpftrace profile SVG\"><svg>"
        ));
        assert!(report.contains("bpftrace profiles"));
        assert!(report.contains("class=\"embedded-svg flamegraph\""));
        assert!(report.contains("flamegraph.svg"));
        assert!(report.contains(
            "<div class=\"embedded-svg flamegraph\" aria-label=\"Flamegraph SVG\"><svg>"
        ));
        assert!(!report.contains("<object class=\"flamegraph\""));
        assert!(report.contains("Flamegraph collector status"));
        assert!(report.contains("class=\"interactive-chart\""));
        assert!(report.contains("series-perfetto.scheduler_events_per_s.svg"));
        assert!(report.contains("data-timestamp=\"12000\""));
        assert!(report.contains("chart-tooltip"));
        assert!(report.contains("wheel zoom"));
        assert!(report.contains("chart-expand"));
        assert!(report.contains("chart-close"));
        assert!(report.contains("80vw"));
        assert!(report.contains("Select all"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn line_graph_labels_token_axis_unit_and_ticks() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-svg-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        super::write_line_svg(
            &root.join("tokens.svg"),
            "token_usage.total_tokens",
            &[(
                "run".into(),
                super::TimeSeries {
                    values: vec![0.0, 100.0],
                    timestamps_ms: Some(vec![10.0, 20.0]),
                },
            )],
        )
        .unwrap();
        let svg = std::fs::read_to_string(root.join("tokens.svg")).unwrap();
        assert!(svg.contains("unit: tokens"));
        assert!(svg.contains(">0</text>"));
        assert!(svg.contains(">100</text>"));
        assert!(svg.contains("data-timestamp=\"10\""));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bar_graph_labels_values_inside_or_above_each_bar() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-bars-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        super::write_bar_svg(
            &root.join("values.svg"),
            "elapsed_ms",
            &[("tall".into(), 100.0), ("short".into(), 1.0)],
        )
        .unwrap();
        let svg = std::fs::read_to_string(root.join("values.svg")).unwrap();
        assert!(svg.contains("class=\"bar-value\" data-placement=\"inside\""));
        assert!(svg.contains("class=\"bar-value\" data-placement=\"above\""));
        assert!(svg.contains(">100</text>"));
        assert!(svg.contains(">1</text>"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn records_missing_measurements_without_discarding_sweep_results() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-missing-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("one.toml"),
            "agent=\"codex\"\nworkload=\"bug-fix\"\nprovider=\"local\"\nmodel=\"m\"\n",
        )
        .unwrap();
        std::fs::write(root.join("sweep.toml"), "configs=[\"one.toml\"]\n").unwrap();
        run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap();
        let summary =
            std::fs::read_to_string(root.join("results/sweep-comparison/comparison.json")).unwrap();
        assert!(summary.contains("missing measurements"));
        let _ = std::fs::remove_dir_all(root);
    }
}
