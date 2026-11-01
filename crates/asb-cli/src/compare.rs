//! Offline HTML/SVG reports for benchmark configurations and sweeps.

use crate::benchmark_paths::{benchmark_stem, checked_benchmark_reference};
use crate::{Error, http_server};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
mod coverage;
mod metric_identity;
use metric_identity::{GraphNames, flatten_numbers, insert_unique, path_key};
mod model;
pub(crate) mod outcomes;
mod planning;
mod publication;
mod reconciliation;
mod rendering;
mod series;
mod source;
use model::{ResultSource, RowId, ValidatedReport};
use planning::PreparedReport;
use series::TimeSeries;
use source::read_json;
const PLOT_TOP: f64 = 70.0;
const PLOT_BOTTOM: f64 = 390.0;
const PLOT_RIGHT: f64 = 880.0;
const Y_TICKS: usize = 5;

#[derive(Debug)]
struct BenchmarkData {
    observation: crate::process_termination::Observation,
    id: RowId,
    execution: Option<Value>,
    planning: Option<Value>,
    effective_budgets: Option<Value>,
    legacy_evaluation_provenance: Option<crate::workloads::outcome::LegacyEvaluationProvenance>,
    execution_plan_fingerprint: Option<String>,
    module: Option<crate::workloads::contracts::ModuleDescriptor>,
    benchmark_outcome: Option<crate::workloads::outcome::BenchmarkOutcome>,
    result_source: ResultSource,
    reconciled: bool,
    replica_index: Option<usize>,
    execution_cohort: Option<crate::measurements::ExecutionCohort>,
    label: String,
    directory: PathBuf,
    agent: String,
    workload: String,
    task: Option<crate::workloads::TaskIdentity>,
    selection: Option<crate::workloads::TaskSelection>,
    evaluation_completed: bool,
    workload_evaluation: String,
    provider: String,
    model: String,
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, TimeSeries>,
    host_metadata: Option<Value>,
    components: Option<Value>,
    collector_documents: BTreeMap<String, Value>,
    status: Option<String>,
    artifacts: Vec<String>,
    artifact_sources: BTreeMap<String, PathBuf>,
}

fn trial_label(benchmark: &BenchmarkData) -> &str {
    benchmark
        .task
        .as_ref()
        .map(|task| task.task_id.as_str())
        .or_else(|| {
            benchmark
                .selection
                .as_ref()
                .map(|selection| selection.task_id.as_str())
        })
        .unwrap_or("default")
}

fn chart_label(benchmark: &BenchmarkData) -> String {
    let trial = trial_label(benchmark);
    format!("{}:{trial}", benchmark.workload)
}

fn attempt_label(benchmark: &BenchmarkData) -> String {
    let mut parts = vec![format!(
        "run {}",
        benchmark.id.configuration.saturating_add(1)
    )];
    if let Some(batch) = benchmark.id.batch {
        parts.push(format!("batch {}", batch.saturating_add(1)));
    }
    if benchmark.replica_index.is_some() || benchmark.id.replica != 0 {
        parts.push(format!(
            "replica {}",
            benchmark.id.replica.saturating_add(1)
        ));
    }
    parts.join(" · ")
}

/// Report a benchmark's recorded results or compare the members of a sweep.
#[cfg(test)]
pub fn run(path: &str, output: &mut dyn Write) -> Result<(), Error> {
    generate(path, output).map(|_| ())
}

/// Generate a report while retaining normal run/sweep output discipline.
///
/// Automatic comparison must still tell the user where the shareable report
/// and Perfetto views are.  The quiet execution paths deliberately omit the
/// chart summary, backups, and installer chatter, so relay only those stable
/// public links.
pub(crate) fn run_links(path: &str, output: &mut dyn Write) -> Result<(), Error> {
    let (_, links) = generate_with_links(path, &mut std::io::sink())?;
    write_public_links(links, output)
}

/// The active sweep already holds its source owner; borrowing that owner keeps
/// it alive across comparison without recursively acquiring the same lock.
pub(crate) fn run_in_sweep(
    path: &str,
    output: &mut dyn Write,
    _owner: &crate::sweep::lock::OutputLock,
) -> Result<(), Error> {
    generate_owned_with_links(path, output).map(|_| ())
}

pub(crate) fn run_links_in_sweep(
    path: &str,
    output: &mut dyn Write,
    _owner: &crate::sweep::lock::OutputLock,
) -> Result<(), Error> {
    let (_, links) = generate_owned_with_links(path, &mut std::io::sink())?;
    write_public_links(links, output)
}

fn write_public_links(links: Vec<(String, String)>, output: &mut dyn Write) -> Result<(), Error> {
    for (label, href) in links {
        writeln!(output, "{label}: {href}").map_err(crate::io_error)?;
    }
    Ok(())
}

/// Generate an offline report for either a benchmark or a sweep manifest.
pub(crate) fn generate(path: &str, output: &mut dyn Write) -> Result<PathBuf, Error> {
    generate_with_links(path, output).map(|(path, _)| path)
}

fn generate_with_links(
    path: &str,
    output: &mut dyn Write,
) -> Result<(PathBuf, Vec<(String, String)>), Error> {
    let _owner = crate::sweep::lock::OutputLock::for_comparison(Path::new(path))?;
    generate_owned_with_links(path, output)
}

fn generate_owned_with_links(
    path: &str,
    output: &mut dyn Write,
) -> Result<(PathBuf, Vec<(String, String)>), Error> {
    let loaded = source::load(path)?;
    let validated = ValidatedReport::validate(loaded)?;
    let prepared = PreparedReport::prepare(validated)?;
    let mut files = prepared.graph_names();
    files.extend(["comparison.json".into(), "report.html".into()]);
    let publisher = publication::Publisher::begin(&prepared.data.request.comparison_dir, files)?;
    rendering::render(publisher.stage(), &prepared)?;
    let published = publisher.commit()?;
    let traces = prepared
        .data
        .rows
        .iter()
        .flat_map(|benchmark| benchmark.artifact_sources.iter())
        .filter_map(|(name, source)| {
            crate::collectors::report_artifact_links(name, source)
                .iter()
                .any(|link| link.primary.is_some())
                .then_some(source.clone())
        })
        .collect();
    let server = http_server::start_for_report(&published.report, traces, output)?;
    writeln!(
        output,
        "Compared {} benchmarks; wrote {} graphs, report.html, and {}",
        prepared.data.rows.len(),
        prepared.graphs.len(),
        prepared.data.request.comparison_dir.display()
    )
    .map_err(|error| published.confirmation_error(error))?;
    if let Some(backup) = &published.backup {
        writeln!(output, "Previous report preserved at {}", backup.display())
            .map_err(|error| published.confirmation_error(error))?;
    }
    if published.recoverable_fallback {
        writeln!(output,"Report published using recoverable replacement, not atomic exchange; a brief destination-absence/crash window was possible. The previous report backup was retained.").map_err(|error|published.confirmation_error(error))?;
    }
    let mut links = vec![(
        "Result report".to_owned(),
        server.report_url(&published.report),
    )];
    links.extend(collector_report_links(&prepared));
    let mut viewers = BTreeSet::new();
    for trace in server.traces() {
        let Some(name) = trace.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        append_primary_links(
            &mut links,
            &mut viewers,
            trace,
            server.trace_url(trace),
            crate::collectors::report_artifact_links(name, trace),
        );
    }
    links.extend(viewers);
    for (label, href) in &links {
        writeln!(output, "{label}: {href}").map_err(|error| published.confirmation_error(error))?;
    }
    Ok((published.report, links))
}

fn append_primary_links(
    links: &mut Vec<(String, String)>,
    viewers: &mut BTreeSet<(String, String)>,
    path: &Path,
    url: Option<String>,
    artifacts: Vec<crate::collectors::ReportArtifactLink>,
) {
    for link in artifacts {
        if let Some(primary) = link.primary {
            if let Some(url) = &url {
                links.push((primary.download_label.into(), url.clone()));
            } else {
                links.push((primary.file_label.into(), path.display().to_string()));
            }
            viewers.insert((link.label.to_owned(), link.href));
        }
    }
}

fn collector_report_links(prepared: &PreparedReport) -> BTreeSet<(String, String)> {
    prepared
        .data
        .rows
        .iter()
        .flat_map(|benchmark| benchmark.artifact_sources.iter())
        .flat_map(|(name, source)| crate::collectors::report_artifact_links(name, source))
        .filter(|link| link.primary.is_none())
        .map(|link| (link.label.to_owned(), link.href))
        .collect()
}
/// Invoke the platform opener without a shell; report paths are never commands.
pub(crate) fn open_report(path: &Path) -> Result<(), Error> {
    let path = path.canonicalize().map_err(crate::io_error)?;
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    let status = command.arg(&path).status().map_err(|error| Error::Run(format!(
        "report created at {}, but the default browser could not be opened: {error}; open this HTML file in your browser", path.display())))?;
    if !status.success() {
        return Err(Error::Run(format!(
            "report created at {}, but the browser opener failed ({status}); open this HTML file in your browser",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
fn benchmark_identity(path: &Path) -> Result<(String, String, String, String, usize), Error> {
    benchmark_identity_text(path, &source::read_benchmark_text(path)?)
}
fn benchmark_identity_text(
    path: &Path,
    text: &str,
) -> Result<(String, String, String, String, usize), Error> {
    let value: toml::Value = toml::from_str(text).map_err(|error| {
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
    let concurrency = match table.get("concurrency") {
        None => 1,
        Some(value) => value
            .as_integer()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=crate::benchmark::MAX_CONCURRENCY).contains(n))
            .ok_or_else(|| {
                Error::Input(format!(
                    "benchmark {} concurrency must be an integer from 1 through {}",
                    path.display(),
                    crate::benchmark::MAX_CONCURRENCY
                ))
            })?,
    };
    Ok((
        field("agent")?,
        field("workload")?,
        field("provider")?,
        field("model")?,
        concurrency,
    ))
}

fn artifact_inventory(sweep_parent: &Path, measurement_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    // Measurement artifacts are the primary evidence for a comparison. Add
    // them before the bounded recursive inventory so a large external
    // workload workspace cannot crowd collector artifacts out of the
    // HTML report's artifact links.
    if let Ok(entries) = fs::read_dir(measurement_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(crate::collectors::prioritizes_artifact)
            {
                files.push(path);
            }
        }
    }
    // Do not inventory external workload workspaces; only measurement output
    // is comparison evidence and should appear in the report.
    collect_artifacts(measurement_dir, 0, &mut files);
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) != Some("run.json"))
        .filter_map(|path| {
            let physical = path.canonicalize().ok()?;
            let relative = relative_artifact(sweep_parent, &physical)?;
            Some((
                format!("../../{}", relative.to_string_lossy().replace('\\', "/")),
                physical,
            ))
        })
        .collect()
}

fn relative_artifact(base: &Path, path: &Path) -> Option<PathBuf> {
    // A standalone config and its project-scoped captures may be in sibling
    // directories. Keep those links portable, including absolute capture paths.
    let base = if base.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base
    };
    let base = base.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    let base: Vec<_> = base.components().collect();
    let path: Vec<_> = path.components().collect();
    let shared = base.iter().zip(&path).take_while(|(a, b)| a == b).count();
    if shared == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &path[shared..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
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

fn write_html_report(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let benchmarks = &prepared.data.rows;
    let graphs = prepared.graph_names();
    let selection_mode = prepared
        .data
        .request
        .plan
        .as_ref()
        .map(|plan| plan.execution_mode.as_str())
        .or_else(|| {
            prepared
                .data
                .request
                .campaign
                .as_ref()
                .and_then(|campaign| campaign["execution_mode"].as_str())
        });
    let scheduling = prepared
        .data
        .request
        .plan
        .as_ref()
        .and_then(|plan| plan.scheduling);
    let dimensions = &prepared.dimensions;
    let dispatch_groups = &prepared.dispatch_groups;

    let mut html = String::from(HTML_HEAD);
    html = html.replacen(
        "</style></head>",
        &format!(
            "{}\n{}\n{}\n{}\n</style></head>",
            INTERACTIVE_STYLE, MODAL_STYLE, INLINE_SVG_STYLE, JSON_DETAILS_STYLE
        ),
        1,
    );
    html.push_str("<main class=\"shell\">");
    if let Some(scheduling) = scheduling {
        html.push_str(&format!("<p class=\"scheduling\">Sweep scheduling: up to {} distinct configurations; {} active attempt slots, including replicas. Concurrent tasks share host resources.</p>", scheduling.jobs, scheduling.max_active_attempts));
    }
    html.push_str("<header class=\"hero\"><p class=\"eyebrow\">ASB BENCHMARK REPORT</p><h1>Benchmark comparison</h1><p>Explore recorded benchmark results and measurements. Use the filters to focus the report without rerunning anything.</p></header>");
    if let Some(intent) = prepared
        .data
        .request
        .plan
        .as_ref()
        .and_then(|plan| plan.execution.as_ref())
    {
        html.push_str(&format!("<p>Each configuration selected {} sequential batches at its configured concurrency. Every planned execution remains a separate row. Shared replay seeds apply only to bundled inputs; legacy sampling stays unknown. Response replay does not measure current provider latency or live model quality.</p>", intent.repeat));
    }
    let coverage = &prepared.evaluation;
    html.push_str("<p>Declared planning and controller caps are recorded separately in comparison.json, not plotted as measurements. Missing policy metadata remains unknown.</p>");
    html.push_str(&format!("<section class=\"explain\"><strong>Evaluation coverage</strong><p>Execution mode: {}.</p><p>{}/{} eligible selected attempts evaluated; {} have no completed evaluation. {} attempts are unscored. {}</p><p>Partial results are not a complete-population benchmark score. See population_coverage in comparison.json for dataset selection, unique tasks and complete one-attempt-per-task scores.</p></section>",
        escape(selection_mode.unwrap_or("unknown")),
        coverage.evaluated_attempts, coverage.eligible_selected_attempts, coverage.unevaluated_attempts, coverage.unscored_attempts,
        if prepared.data.request.plan.is_some() {"Pre-run task selection is preserved and matches the configurations."} else if prepared.data.request.campaign.is_some() {"Saved campaign slots are enumerated; repeated attempts do not establish a complete one-attempt-per-task population score."} else {"No pre-run selection snapshot exists; dataset completeness cannot be established from these legacy records."}));
    html.push_str(&coverage::html_table(&prepared.populations));
    html.push_str(&coverage::metric_html(&prepared.metric_groups));
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Each row is one selected task attempt for an agent/provider/model/workload combination. Runtime, success rate, token usage, host metadata, component timings, and structured collector measurements are summarized in the table and charts. Network wait remains null unless directly observed. The complete machine-readable evidence inventory is available in comparison.json.</p></section>");
    html.push_str("<section class=\"filters\"><div class=\"section-title\"><h2>Filter results</h2><div><button type=\"button\" data-action=\"all\">Select all</button><button type=\"button\" data-action=\"none\">Clear all</button></div></div><div class=\"filter-grid\">");
    for (group, values) in dimensions {
        html.push_str(&format!("<fieldset><legend>{}</legend>", title_case(group)));
        for value in values {
            let escaped = escape(value);
            html.push_str(&format!("<label><input type=\"checkbox\" data-group=\"{group}\" value=\"{escaped}\" checked><span>{escaped}</span></label>"));
        }
        html.push_str("</fieldset>");
    }
    html.push_str("</div><p class=\"filter-count\" id=\"filter-count\"></p></section>");
    html.push_str("<section><div class=\"section-title\"><h2>Benchmark results</h2><a class=\"text-link\" href=\"comparison.json\">Download comparison.json</a></div><div class=\"table-wrap\"><table><thead><tr><th>Agent</th><th>Provider</th><th>Model</th><th>Workload</th><th>Trial</th><th>Attempt</th><th>Runtime</th><th>Success</th><th>Tokens</th><th>Details</th></tr></thead><tbody id=\"results\">");
    for benchmark in benchmarks {
        let runtime = benchmark
            .metrics
            .get("elapsed_ms")
            .map_or_else(|| "—".into(), |value| format!("{value:.0} ms"));
        let success = benchmark.metrics.get("success_rate").map_or_else(
            || "Not measured".into(),
            |value| format!("{:.1}% ({}/1)", value * 100.0, usize::from(*value == 1.0)),
        );
        let tokens = benchmark
            .metrics
            .get("token_usage.total_tokens")
            .map_or_else(|| "—".into(), |value| format!("{value:.0}"));
        let trial = trial_label(benchmark);
        let attempt = attempt_label(benchmark);
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
        let evaluation_details = format!(
            "<p class=\"evaluation\">Workload evaluation: <code>{}</code>. {}</p>",
            escape(&benchmark.workload_evaluation),
            if crate::workloads::resolution_eligible(&benchmark.workload_evaluation) {
                "Task-resolution rate; failures remain in the denominator. Local fixture scores are not upstream suite scores."
            } else {
                "No upstream task resolution was measured. Adapter smoke exercises and unknown graders cannot earn benchmark success credit."
            }
        );
        let host_details = benchmark.host_metadata.as_ref().map_or_else(
            String::new,
            |metadata| {
                let text = serde_json::to_string_pretty(metadata).unwrap_or_else(|_| "{}".into());
                format!(
                    "<details><summary>Host metadata</summary><pre class=\"json-details\">{}</pre></details>",
                    escape(&text)
                )
            },
        );
        let component_details = benchmark.components.as_ref().map_or_else(
            String::new,
            |components| {
                let text =
                    serde_json::to_string_pretty(components).unwrap_or_else(|_| "{}".into());
                format!(
                    "<details><summary>Component timings</summary><pre class=\"json-details\">{}</pre></details>",
                    escape(&text)
                )
            },
        );
        let execution_details = benchmark.execution.as_ref().map_or_else(String::new, |execution| {
            let text = serde_json::to_string_pretty(execution).unwrap_or_else(|_| "{}".into());
            format!("<details><summary>Execution / replay identity</summary><pre class=\"json-details\">{}</pre></details>", escape(&text))
        });
        let primary_details = benchmark
            .artifacts
            .iter()
            .flat_map(|artifact| {
                benchmark
                    .artifact_sources
                    .get(artifact)
                    .map(|path| crate::collectors::report_artifact_links(artifact, path))
                    .unwrap_or_default()
            })
            .filter_map(|link| link.primary.map(|primary| (primary, link.href)))
            .map(|(primary, href)| primary_link_markup(&primary, &href))
            .collect::<String>();
        let artifacts = if benchmark.artifacts.is_empty() {
            String::new()
        } else {
            let links = benchmark
                .artifacts
                .iter()
                .map(|artifact| {
                    let supplemental_links = benchmark
                        .artifact_sources
                        .get(artifact)
                        .map(|path| crate::collectors::report_artifact_links(artifact, path))
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|link| link.primary.is_none())
                        .map(|link| {
                            format!(
                                " <a href=\"{}\" target=\"_blank\" rel=\"noopener\">Open in {}</a>",
                                escape(&link.href),
                                escape(link.label)
                            )
                        })
                        .collect::<String>();
                    format!(
                        "<li><a href=\"{}\" download>{}</a>{supplemental_links}</li>",
                        escape(artifact),
                        escape(artifact.rsplit('/').next().unwrap_or(artifact))
                    )
                })
                .collect::<String>();
            let mut previews = String::new();
            for artifact in &benchmark.artifacts {
                if benchmark.result_source == ResultSource::RecoveredOfficialGrade {
                    // Preserved partial collectors are diagnostic downloads,
                    // never final execution charts for a grade-only closure.
                    continue;
                }
                let name = artifact.rsplit('/').next().unwrap_or(artifact);
                match crate::collectors::display_artifact(name) {
                    Some(crate::collectors::ArtifactDisplay::Svg { class, label }) => {
                        let svg =
                            prepared.data.previews.get(artifact).ok_or_else(|| {
                                Error::Run("validated SVG preview is missing".into())
                            })?;
                        previews.push_str(&inline_svg_markup(svg, class, label));
                    }
                    Some(crate::collectors::ArtifactDisplay::Status {
                        class,
                        label,
                        suffix,
                    }) => {
                        previews.push_str(&format!(
                            "<p class=\"{}\">{}: <a href=\"{}\">{}</a>. {}</p>",
                            escape(class),
                            escape(label),
                            escape(artifact),
                            escape(name),
                            escape(suffix)
                        ));
                    }
                    None => {}
                }
            }
            format!(
                "<details><summary>Run artifacts</summary>{previews}<ul class=\"artifact-list\">{links}</ul></details>"
            )
        };
        let dispatch_details = benchmark.execution_cohort.as_ref().map_or_else(
            || "Dispatch context unknown (legacy or missing measurement).".into(),
            |cohort| {
                let descriptor = cohort.dispatch.map_or_else(
                    || "Runtime population descriptor unknown (historical).".into(),
                    |dispatch| format!("Configured {} · admitted {}{}.", dispatch.configured(), dispatch.admitted(), if dispatch.admitted() < dispatch.configured() { " (partial admission)" } else { "" }),
                );
                format!("Dispatch group {}: {} of {} selected replicas admitted; slots {}. {descriptor} Dispatch membership is not proof of simultaneous execution. Resumed measurements may belong to different cohorts.", dispatch_groups[cohort.id.as_str()], cohort.admitted_slots.len(), cohort.selected_replicas, cohort.admitted_slots.iter().map(usize::to_string).collect::<Vec<_>>().join(", "))
            },
        );
        html.push_str(&format!("<tr data-dimension-benchmark=\"{}\" data-dimension-agent=\"{}\" data-dimension-provider=\"{}\" data-dimension-model=\"{}\" data-dimension-workload=\"{}\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"details\">{}{}{}{}{}{}</td></tr>",
            escape(&benchmark.label), escape(&benchmark.agent), escape(&benchmark.provider), escape(&benchmark.model), escape(&benchmark.workload),
            escape(&benchmark.agent), escape(&benchmark.provider), escape(&benchmark.model), escape(&benchmark.workload), escape(trial), escape(&attempt),
            runtime, success, tokens,
            format_args!("{primary_details}{}{}<p>{dispatch_details}</p>", evaluation_details, if benchmark.reconciled { "<p>Reconciled retained local outcome: current artifact consistency checked; no agent rerun or historical attestation.</p>" } else { "" }),
            format_args!("{execution_details}{component_details}"),
            host_details,
            metrics_details,
            artifacts,
            format_args!("<p>{}</p>{}", escape(&benchmark.observation.description()), benchmark.status.as_ref().map_or(String::new(), |status| format!("<span class=\"status\">{}</span>", escape(status)))),
            ));
    }
    html.push_str("</tbody></table></div></section>");
    let mut sections = std::collections::BTreeMap::<(&str, &str), Vec<&String>>::new();
    for graph in &graphs {
        if let Some(section) = crate::collectors::graph_section(graph) {
            sections
                .entry((section.title, section.description))
                .or_default()
                .push(graph);
        }
    }
    for ((title, description), section_graphs) in sections {
        html.push_str(&format!("<section><div class=\"section-title\"><h2>{}</h2><p class=\"muted\">{}</p></div><div class=\"charts\">", escape(title), escape(description)));
        for graph in section_graphs {
            html.push_str(&chart_figure(stage, graph)?);
        }
        html.push_str("</div></section>");
    }
    html.push_str("<section><div class=\"section-title\"><h2>Charts</h2><p class=\"muted\">SVG files are linked below and remain available for download.</p></div><div class=\"charts\">");
    for graph in graphs
        .iter()
        .filter(|graph| crate::collectors::graph_section(graph).is_none())
    {
        html.push_str(&chart_figure(stage, graph)?);
    }
    html.push_str("</div></section><footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str(HTML_SCRIPT);
    fs::write(stage.join("report.html"), html)
        .map_err(|error| Error::Run(format!("cannot write HTML report: {error}")))
}

fn chart_figure(comparison_dir: &Path, graph: &str) -> Result<String, Error> {
    let svg = fs::read_to_string(comparison_dir.join(graph))
        .map_err(|error| Error::Run(format!("cannot read generated chart {}: {error}", graph)))?;
    Ok(format!(
        "<figure class=\"interactive-chart\" data-graph=\"{}\"><div class=\"chart-backdrop\" aria-hidden=\"true\"></div><div class=\"chart-toolbar\"><strong>{}</strong><span><button type=\"button\" class=\"chart-reset\">Reset view</button><button type=\"button\" class=\"chart-expand\" aria-label=\"Enlarge chart\">Enlarge</button><button type=\"button\" class=\"chart-close\" aria-label=\"Close enlarged chart\" hidden>Close</button></span></div><div class=\"chart-stage\"><div class=\"chart-tooltip\" role=\"status\" aria-live=\"polite\"></div>{svg}<p class=\"chart-empty\" role=\"status\" hidden>No selected benchmarks have data for this graph.</p></div><figcaption><a href=\"{}\" download>Download SVG (all results)</a></figcaption></figure>",
        escape(graph),
        escape(graph.strip_suffix(".svg").unwrap_or(graph)),
        escape(graph),
    ))
}

#[cfg(test)]
fn inline_svg_artifact(
    comparison_dir: &Path,
    artifact: &str,
    class: &str,
    fallback: &str,
) -> Result<String, Error> {
    let svg = source::read_svg(&comparison_dir.join(artifact), artifact)?;
    Ok(inline_svg_markup(&svg, class, fallback))
}
fn inline_svg_markup(svg: &str, class: &str, fallback: &str) -> String {
    format!(
        "<div class=\"embedded-svg {class}\" aria-label=\"{}\">{svg}</div>",
        escape(fallback)
    )
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

const MODAL_STYLE: &str = r#"body.chart-open{overflow:hidden}.chart-backdrop{display:none;position:fixed;inset:0;background:#081522c9;z-index:30}.interactive-chart.expanded{position:fixed;overflow:visible;z-index:31;left:10vw;top:10vh;width:80vw;height:80vh;margin:0;padding:18px;background:var(--panel);border:1px solid #b9c9d8;border-radius:16px;box-shadow:0 24px 80px #08152299}.interactive-chart.expanded .chart-backdrop{display:block;z-index:0}.interactive-chart.expanded .chart-toolbar{position:relative;z-index:32}.interactive-chart.expanded .chart-stage{position:relative;z-index:1;height:calc(80vh - 92px);min-height:0}.interactive-chart.expanded .chart-stage svg{height:100%;min-height:0}.interactive-chart.expanded figcaption{position:relative;z-index:32}@media(max-width:700px){.interactive-chart.expanded{left:3vw;top:3vh;width:94vw;height:94vh;padding:10px}.interactive-chart.expanded .chart-stage{height:calc(94vh - 78px)}}"#;

const INLINE_SVG_STYLE: &str = r#".embedded-svg{display:block;overflow:auto;border:1px solid var(--line);border-radius:8px;background:#fff;margin:8px 0}.embedded-svg svg{display:block;width:100%;height:auto}.embedded-svg.perfetto-metric{width:min(760px,100%);max-height:260px}.embedded-svg.bpftrace-profile{width:min(760px,100%);max-height:300px}.embedded-svg.flamegraph{width:100%;max-height:520px}.embedded-svg.flamegraph svg{min-width:900px}"#;

const JSON_DETAILS_STYLE: &str = ".json-details{max-height:360px;overflow:auto;background:#f4f7fa;border:1px solid var(--line);border-radius:7px;padding:10px;font:12px/1.4 ui-monospace,SFMono-Regular,Consolas,monospace;white-space:pre-wrap}";

const INTERACTIVE_SCRIPT: &str = concat!(
    "<script>\n",
    include_str!("compare/report_interactions.js"),
    "\n</script>"
);

const HTML_SCRIPT: &str = concat!(
    "<script>\n",
    include_str!("compare/report_filters.js"),
    "\n</script></body></html>"
);

fn write_bar_svg(
    path: &Path,
    title: &str,
    values: &[(String, Option<f64>)],
    labels: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let min = values
        .iter()
        .filter_map(|(_, value)| *value)
        .fold(0.0_f64, f64::min);
    let max = values
        .iter()
        .filter_map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
        .max(if min < 0.0 { 0.0 } else { 1.0 });
    let width = 900.0;
    let height = 460.0;
    let left = 70.0;
    let bottom = PLOT_BOTTOM;
    let bar_width = (width - left - 20.0) / values.len().max(1) as f64;
    let mut svg = svg_header(width, height, title);
    append_y_axis(
        &mut svg,
        left,
        PLOT_TOP,
        bottom,
        PLOT_RIGHT,
        (min, max),
        title,
    );
    let baseline = bottom - domain_fraction(0.0, min, max) * (bottom - PLOT_TOP);
    if min < 0.0 {
        svg.push_str(&format!("<line class=\"zero-baseline\" x1=\"{left}\" y1=\"{baseline:.1}\" x2=\"{PLOT_RIGHT}\" y2=\"{baseline:.1}\" stroke=\"black\"/>"));
    }
    for (index, (label, value)) in values.iter().enumerate() {
        svg.push_str(&format!("<g data-benchmark=\"{}\">", escape(label)));
        let display = labels.get(label).unwrap_or(label);
        let x = left + index as f64 * bar_width + 4.0;
        let width = (bar_width - 8.0).max(2.0);
        if value.is_none() {
            let unavailable_height = 24.0;
            let unavailable_y = baseline - unavailable_height;
            svg.push_str(&format!(
                "<rect class=\"bar bar-unavailable\" data-label=\"{}\" data-status=\"unavailable\" x=\"{x:.1}\" y=\"{unavailable_y:.1}\" width=\"{width:.1}\" height=\"{unavailable_height:.1}\" fill=\"#e3e8ed\" stroke=\"#667788\" stroke-dasharray=\"4 3\"><title>{}: unavailable</title></rect><text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" class=\"bar-value bar-unavailable-value\" font-size=\"10\" fill=\"#405466\">unavailable</text><text x=\"{:.1}\" y=\"410\" transform=\"rotate(35 {:.1} 410)\" font-size=\"11\">{}</text>",
                escape(display), escape(display), x + width / 2.0, unavailable_y + 16.0, x, x, escape(display),
            ));
            svg.push_str("</g>");
            continue;
        }
        let value = value.expect("unavailable values are handled above");
        let endpoint = bottom - domain_fraction(value, min, max) * (bottom - PLOT_TOP);
        let bar_height = (endpoint - baseline).abs();
        let y = endpoint.min(baseline);
        svg.push_str(&format!(
            "<rect class=\"bar\" data-label=\"{}\" data-value=\"{value}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{width:.1}\" height=\"{bar_height:.1}\" fill=\"#4472c4\"><title>{} [{}]: {value}</title></rect>",
            escape(display),
            escape(display),
            escape(label),
        ));
        let value_label = format_tick(value);
        let (label_y, placement, color) = if bar_height >= 24.0 {
            // Keep a small top inset while retaining enough contrast against
            // the bar fill for the value to remain readable.
            (
                if value < 0.0 {
                    endpoint - 6.0
                } else {
                    y + 16.0
                },
                "inside",
                "#ffffff",
            )
        } else if value < 0.0 {
            (endpoint + 14.0, "below", "#18212b")
        } else {
            // Short and zero-height bars cannot contain a label. Place the
            // baseline just above the bar with the same small spacing.
            (y - 5.0, "above", "#18212b")
        };
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{label_y:.1}\" text-anchor=\"middle\" class=\"bar-value\" data-placement=\"{placement}\" font-size=\"11\" font-weight=\"600\" fill=\"{color}\">{value_label}</text>",
            x + width / 2.0
        ));
        svg.push_str(&format!("<text x=\"{:.1}\" y=\"410\" transform=\"rotate(35 {:.1} 410)\" font-size=\"11\">{}</text>", x, x, escape(display)));
        svg.push_str("</g>");
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

#[cfg(test)]
fn write_series_svg(
    path: &Path,
    title: &str,
    values: &[(String, &TimeSeries)],
) -> Result<(), Error> {
    let values = values
        .iter()
        .map(|(label, series)| (label.clone(), Some(*series)))
        .collect::<Vec<_>>();
    write_series_svg_with_labels(path, title, &values, &BTreeMap::new())
}

fn write_series_svg_with_labels(
    path: &Path,
    title: &str,
    values: &[(String, Option<&TimeSeries>)],
    labels: &BTreeMap<String, String>,
) -> Result<(), Error> {
    const WIDTH: f64 = 1_200.0;
    const LEFT: f64 = 100.0;
    const RIGHT: f64 = 1_160.0;
    const LEGEND_TOP: f64 = 68.0;
    const LEGEND_COLUMNS: usize = 2;
    const LEGEND_MAX_CHARS: usize = 46;
    const LEGEND_LINE_HEIGHT: f64 = 16.0;
    const LEGEND_ROW_PADDING: f64 = 12.0;
    const PLOT_HEIGHT: f64 = 320.0;
    let max = values
        .iter()
        .flat_map(|(_, points)| {
            points
                .iter()
                .flat_map(|points| points.values.iter().flatten().copied())
        })
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let min = values
        .iter()
        .flat_map(|(_, points)| {
            points
                .iter()
                .flat_map(|points| points.values.iter().flatten().copied())
        })
        .fold(0.0_f64, f64::min);
    let index_length = values
        .iter()
        .filter_map(|(_, points)| points.map(|points| points.values.len()))
        .max()
        .unwrap_or(0);
    let timestamp_bounds = values
        .iter()
        .filter_map(|(_, points)| points.and_then(|points| points.timestamps_ms.as_ref()))
        .flat_map(|timestamps| timestamps.iter().copied())
        .fold(None, |bounds: Option<(f64, f64)>, timestamp| {
            Some(match bounds {
                Some((min, max)) => (min.min(timestamp), max.max(timestamp)),
                None => (timestamp, timestamp),
            })
        });
    let legend_columns = values.len().clamp(1, LEGEND_COLUMNS);
    let legend_column_width = (RIGHT - LEFT) / legend_columns as f64;
    let legend_lines = values
        .iter()
        .map(|(label, points)| {
            let display = labels.get(label).unwrap_or(label);
            let available = points
                .and_then(|points| points.render_indices())
                .is_some_and(|indices| !indices.is_empty());
            wrap_label(
                &if available {
                    display.to_owned()
                } else {
                    format!("{display} — unavailable")
                },
                LEGEND_MAX_CHARS,
            )
        })
        .collect::<Vec<_>>();
    let legend_rows = values.len().div_ceil(legend_columns);
    let legend_height = (0..legend_rows)
        .map(|row| {
            (0..legend_columns)
                .filter_map(|column| legend_lines.get(row * legend_columns + column))
                .map(|lines| lines.len())
                .max()
                .unwrap_or(1) as f64
                * LEGEND_LINE_HEIGHT
                + LEGEND_ROW_PADDING
        })
        .sum::<f64>();
    let plot_top = LEGEND_TOP + legend_height + 18.0;
    let bottom = plot_top + PLOT_HEIGHT;
    let height = bottom + 72.0;
    let mut svg = svg_header(WIDTH, height, title);
    svg.push_str("<text x=\"24\" y=\"44\" font-size=\"11\">Bounded display: at most 2048 original observations per series, preserving endpoints/extrema and null gaps. Full aligned observations: comparison.json.</text>");
    svg.push_str(&format!(
        "<rect class=\"legend-panel\" x=\"{LEFT:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{legend_height:.1}\" rx=\"8\" fill=\"#f4f8fb\" stroke=\"#dce4ec\"/>",
        LEGEND_TOP - 12.0,
        RIGHT - LEFT
    ));
    append_y_axis(&mut svg, LEFT, plot_top, bottom, RIGHT, (min, max), title);
    append_series_x_axis(
        &mut svg,
        timestamp_bounds,
        index_length,
        LEFT,
        bottom,
        RIGHT,
    );
    for (series_index, (label, points)) in values.iter().enumerate() {
        let display = labels.get(label).unwrap_or(label);
        svg.push_str(&format!("<g data-benchmark=\"{}\">", escape(label)));
        let color = ["#4472c4", "#ed7d31", "#70ad47", "#a5a5a5", "#ffc000"][series_index % 5];
        let indices = points.and_then(|points| points.render_indices());
        if let Some(indices) = indices {
            if indices.is_empty() {
                append_unavailable_series(
                    &mut svg,
                    series_index,
                    display,
                    color,
                    LEFT,
                    RIGHT,
                    plot_top,
                );
            }
            let mut segment = Vec::new();
            let mut previous = None;
            for index in indices {
                let points = points.expect("series exists for rendered index");
                let Some(value) = points.values[index] else {
                    continue;
                };
                if previous
                    .is_some_and(|old| points.values[old + 1..index].iter().any(Option::is_none))
                    && !segment.is_empty()
                {
                    append_polyline(&mut svg, &segment, series_index, display, color);
                    segment.clear();
                }
                let x = line_x(
                    index,
                    points.timestamps_ms.as_deref(),
                    timestamp_bounds,
                    index_length,
                    LEFT,
                    RIGHT,
                );
                let fraction = domain_fraction(value, min, max);
                let y = bottom - fraction * (bottom - plot_top);
                segment.push(format!("{x:.1},{y:.1}"));
                let timestamp = points
                    .timestamps_ms
                    .as_ref()
                    .map_or_else(String::new, |times| {
                        format!(" data-timestamp=\"{}\"", times[index])
                    });
                svg.push_str(&format!("<circle class=\"series-point\" data-series-index=\"{series_index}\" data-label=\"{}\" data-value=\"{value}\" data-sample-index=\"{index}\"{timestamp} cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\" fill=\"transparent\" stroke=\"none\"/>",escape(display)));
                previous = Some(index);
            }
            if !segment.is_empty() {
                append_polyline(&mut svg, &segment, series_index, display, color);
            }
        } else {
            append_unavailable_series(
                &mut svg,
                series_index,
                display,
                color,
                LEFT,
                RIGHT,
                plot_top,
            );
        }
        let column = series_index % legend_columns;
        let row = series_index / legend_columns;
        let legend_x = LEFT + column as f64 * legend_column_width;
        let row_y = LEGEND_TOP
            + (0..row)
                .map(|previous_row| {
                    (0..legend_columns)
                        .filter_map(|previous_column| {
                            legend_lines.get(previous_row * legend_columns + previous_column)
                        })
                        .map(|lines| lines.len())
                        .max()
                        .unwrap_or(1) as f64
                        * LEGEND_LINE_HEIGHT
                        + LEGEND_ROW_PADDING
                })
                .sum::<f64>();
        let lines = &legend_lines[series_index];
        let legend_y = row_y + 6.0;
        svg.push_str(&format!(
            "<rect class=\"legend-swatch\" x=\"{legend_x:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{color}\"/><text class=\"legend-item\" data-series-index=\"{series_index}\" x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" fill=\"{color}\" aria-label=\"{}\">",
            legend_y - 10.0,
            legend_x + 16.0,
            legend_y,
            escape(display)
        ));
        for (line_index, line) in lines.iter().enumerate() {
            svg.push_str(&format!(
                "<tspan x=\"{:.1}\" dy=\"{}\">{}</tspan>",
                legend_x + 16.0,
                if line_index == 0 { 0 } else { 15 },
                escape(line)
            ));
        }
        svg.push_str("</text></g>");
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn append_unavailable_series(
    svg: &mut String,
    series_index: usize,
    display: &str,
    color: &str,
    left: f64,
    right: f64,
    plot_top: f64,
) {
    let y = plot_top + 14.0 + (series_index % 8) as f64 * 10.0;
    svg.push_str(&format!(
        "<polyline class=\"series series-unavailable\" data-series-index=\"{series_index}\" data-label=\"{}\" data-status=\"unavailable\" points=\"{left:.1},{y:.1} {right:.1},{y:.1}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\" stroke-dasharray=\"6 5\"/><text x=\"{left:.1}\" y=\"{:.1}\" class=\"series-unavailable\" fill=\"{color}\">{}: unavailable (graph unavailable)</text>",
        escape(display),
        y - 4.0,
        escape(display),
    ));
}
fn append_polyline(
    svg: &mut String,
    coordinates: &[String],
    series_index: usize,
    display: &str,
    color: &str,
) {
    svg.push_str(&format!("<polyline class=\"series\" data-series-index=\"{series_index}\" data-label=\"{}\" points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\"/>",escape(display),coordinates.join(" ")));
}
#[cfg(test)]
fn write_line_svg(path: &Path, title: &str, values: &[(String, TimeSeries)]) -> Result<(), Error> {
    let borrowed = values
        .iter()
        .map(|(label, series)| (label.clone(), series))
        .collect::<Vec<_>>();
    write_series_svg(path, title, &borrowed)
}

fn line_x(
    index: usize,
    timestamps: Option<&[f64]>,
    bounds: Option<(f64, f64)>,
    length: usize,
    left: f64,
    right: f64,
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
                    0.0
                }
            },
        );
    left + fraction.clamp(0.0, 1.0) * (right - left)
}

fn append_series_x_axis(
    svg: &mut String,
    timestamp_bounds: Option<(f64, f64)>,
    length: usize,
    left: f64,
    bottom: f64,
    right: f64,
) {
    let (label, unit) = if timestamp_bounds.is_some() {
        ("time", "ms")
    } else {
        ("sample", "index")
    };
    for tick in 0..=Y_TICKS {
        let fraction = tick as f64 / Y_TICKS as f64;
        let x = left + fraction * (right - left);
        let value = timestamp_bounds
            .map(|(min, max)| min + fraction * (max - min))
            .unwrap_or_else(|| fraction * length.saturating_sub(1) as f64);
        svg.push_str(&format!(
            "<line class=\"x-axis-tick\" x1=\"{x:.1}\" y1=\"{bottom:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"#18212b\"/><text class=\"x-axis-value\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\">{}</text>",
            bottom + 6.0,
            bottom + 22.0,
            format_args!("{} {}", format_tick(value), unit)
        ));
    }
    svg.push_str(&format!(
        "<text class=\"x-axis-label\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"12\">{label} ({unit})</text>",
        (left + right) / 2.0,
        bottom + 43.0
    ));
}

fn wrap_label(value: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut remaining = value.trim();
    while remaining.chars().count() > max_chars {
        let boundary = remaining
            .char_indices()
            .take_while(|(index, _)| *index <= max_chars)
            .filter(|(_, character)| matches!(character, '_' | '-' | '.' | '/' | ' '))
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or_else(|| {
                remaining
                    .char_indices()
                    .nth(max_chars)
                    .map_or(remaining.len(), |(index, _)| index)
            });
        let split_at = boundary.max(1).min(remaining.len());
        let (line, rest) = remaining.split_at(split_at);
        lines.push(line.trim().to_owned());
        remaining = rest.trim_start();
    }
    if !remaining.is_empty() || lines.is_empty() {
        lines.push(remaining.to_owned());
    }
    lines
}

/// Normalize finite observations without overflowing an opposite-sign domain.
fn domain_fraction(value: f64, min: f64, max: f64) -> f64 {
    if (max - min).is_finite() {
        (value - min) / (max - min)
    } else {
        (value * 0.5 - min * 0.5) / (max * 0.5 - min * 0.5)
    }
}

fn append_y_axis(
    svg: &mut String,
    left: f64,
    top: f64,
    bottom: f64,
    right: f64,
    domain: (f64, f64),
    title: &str,
) {
    svg.push_str(&format!(
        "<line x1=\"{left}\" y1=\"{top}\" x2=\"{left}\" y2=\"{bottom}\" stroke=\"black\"/><line x1=\"{left}\" y1=\"{bottom}\" x2=\"{right}\" y2=\"{bottom}\" stroke=\"black\"/>"
    ));
    for tick in 0..=Y_TICKS {
        let fraction = tick as f64 / Y_TICKS as f64;
        let y = bottom - fraction * (bottom - top);
        let value = domain.0 * (1.0 - fraction) + domain.1 * fraction;
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
    } else if name.ends_with("_mib") {
        "MiB"
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

fn primary_link_markup(primary: &crate::collectors::ReportPrimaryLink, href: &str) -> String {
    format!(
        "<p class=\"{}\"><strong>{}</strong><a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a></p>",
        escape(primary.class),
        escape(primary.heading),
        escape(href),
        escape(primary.text),
    )
}

#[cfg(test)]
#[path = "../tests/unit/compare_boundaries.rs"]
mod boundary_tests;
#[cfg(test)]
#[path = "../tests/unit/report_pipeline.rs"]
mod pipeline_tests;

#[cfg(test)]
mod tests {
    use super::GraphNames;
    use super::run;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn another_collector_can_render_a_primary_viewer_link() {
        use crate::collectors::CollectorAdapter;
        struct OtherCollector;
        impl CollectorAdapter for OtherCollector {
            fn id(&self) -> &'static str {
                "other"
            }
            fn report_artifact_links(
                &self,
                name: &str,
                _: &std::path::Path,
            ) -> Vec<crate::collectors::ReportArtifactLink> {
                if !name.ends_with(".trace") {
                    return Vec::new();
                }
                vec![crate::collectors::ReportArtifactLink {
                    label: "Other UI",
                    href: "https://example.invalid/?a=1&b=2".into(),
                    primary: Some(crate::collectors::ReportPrimaryLink {
                        heading: "Other trace",
                        text: "Open viewer",
                        class: "other-link",
                        download_label: "Other download",
                        file_label: "Other file",
                    }),
                }]
            }
        }
        let artifact = std::path::Path::new("other.trace");
        let mut result = OtherCollector.report_artifact_links("other.trace", artifact);
        let metadata = result[0].primary.as_ref().unwrap();
        let html = super::primary_link_markup(metadata, &result[0].href);
        assert!(html.contains("<strong>Other trace</strong>"));
        assert!(html.contains("https://example.invalid/?a=1&amp;b=2"));
        assert!(html.contains("Open viewer"));
        let mut links = Vec::new();
        let mut viewers = BTreeSet::new();
        super::append_primary_links(
            &mut links,
            &mut viewers,
            artifact,
            Some("http://localhost/traces/0.trace".into()),
            std::mem::take(&mut result),
        );
        assert_eq!(
            links,
            vec![(
                "Other download".into(),
                "http://localhost/traces/0.trace".into()
            )]
        );
        assert!(viewers.contains(&("Other UI".into(), "https://example.invalid/?a=1&b=2".into())));
    }

    #[test]
    fn dispatch_provenance_is_not_a_numeric_measurement() {
        let mut metrics = std::collections::BTreeMap::new();
        super::flatten_numbers(
            "",
            &json!({"replica_index":2,"execution_cohort":{
            "id":"abcd","selected_replicas":3,"admitted_slots":[2]},"elapsed_ms":42}),
            &mut metrics,
        )
        .unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics["elapsed_ms"], 42.0);
    }

    #[test]
    fn agent_process_graphs_include_all_agents_but_not_unavailable_measurements() {
        let root = std::env::temp_dir().join(format!(
            "asb-agent-compare-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sweep.toml"),
            "configs=['one.toml','two.toml','missing.toml']\n",
        )
        .unwrap();
        for (index, (name, agent, memory, workers)) in [
            ("one", "codex", 10.0, 0),
            ("two", "pi", 25.5, 2),
            ("missing", "opencode", 0.0, 0),
        ]
        .into_iter()
        .enumerate()
        {
            std::fs::write(
                root.join(format!("{name}.toml")),
                format!("agent='{agent}'\nworkload='bug-fix'\nprovider='local'\nmodel='m'\n"),
            )
            .unwrap();
            let dir = root.join(format!("sweep-results/{index:03}-{name}/measurements"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("run.json"),
                r#"{"status":"completed","elapsed_ms":12}"#,
            )
            .unwrap();
            for (collector, metric, value) in [
                ("agent-memory", "peak_rss_mib", json!(memory)),
                ("agent-workers", "spawned_worker_count", json!(workers)),
            ] {
                let data = if name == "missing" {
                    json!({"status":"unavailable","metrics":{}})
                } else {
                    json!({"status":"observed","metrics":{metric:value}})
                };
                std::fs::write(dir.join(format!("{collector}.json")), data.to_string()).unwrap();
            }
        }
        run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap();
        let dir = root.join("results/sweep-comparison");
        for (file, unit) in [
            ("metric-agent-memory.peak_rss_mib.svg", "MiB"),
            ("metric-agent-workers.spawned_worker_count.svg", "count"),
        ] {
            let svg = std::fs::read_to_string(dir.join(file)).unwrap();
            assert_eq!(svg.matches("<rect class=\"bar").count(), 3, "{svg}");
            assert!(svg.contains("codex / bug-fix:default"));
            assert!(svg.contains("data-label=\"codex / bug-fix:default\""));
            assert!(svg.contains("data-status=\"unavailable\""));
            assert!(svg.contains("opencode / bug-fix:default: unavailable"));
            assert!(svg.contains(&format!("unit: {unit}")));
            assert!(svg.contains("opencode / bug-fix:default"));
        }
        let html = std::fs::read_to_string(dir.join("report.html")).unwrap();
        assert!(html.contains("<h2>Agent memory</h2>"));
        assert!(html.contains("<h2>Agent workers</h2>"));
        assert!(html.contains("Sampled peak aggregate RSS"));
        assert!(html.contains("metric-agent-workers.spawned_worker_count.svg"));
        assert!(html.contains("data-value=\"25.5\""));
        assert!(html.contains("<th>Trial</th><th>Attempt</th>"));
        assert!(!html.contains("<th>Benchmark</th>"));
        assert!(!html.contains("<strong>000-one</strong>"));
        assert!(html.contains("<td>default</td><td>run 1"));
        std::fs::remove_dir_all(root).unwrap();
    }

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
            r#"{"status":"completed","success":true,"success_rate":1.0,"elapsed_ms":12,"tool_call_count":5,"token_usage":{"total_tokens":7}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/system.json"),
            r#"{"schema_version":2,"architecture":"aarch64","cpu_model":"Neoverse-N1","logical_cpus":8,"memory_total_kib":16384,"kernel_settings":{"numa_balancing":"1"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/components.json"),
            r#"{"schema_version":1,"clock_ticks_per_second":100,"components":{"agent.execute":{"wall_ms":12,"cpu_user_ms":3.5,"cpu_system_ms":1.5,"io_wait_ms":2.0,"scheduler_wait_ms":1.0,"network_wait_ms":null}}}"#,
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
            root.join("sweep-results/000-one/measurements/perfetto-scheduling.perfetto-trace"),
            b"trace",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/perfetto-metric-scheduler_events.svg"),
            "<svg><title>Perfetto scheduler events</title></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/perfetto-state-timeline.svg"),
            "<svg><title>Exact scheduler state timeline</title></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("sweep-results/000-one/measurements/perfetto-state-summary.json"),
            r#"{"schema_version":2,"metrics":{"running_ms":8,"runnable_ms":4,"interruptible_sleep_ms":20,"io_wait_ms":null,"io_wait_observed_ms":2}}"#,
        ).unwrap();
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
        assert!(
            root.join("results/sweep-comparison/metric-tool_call_count.svg")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison")
                .join(
                    GraphNames::default()
                        .allocate("metric", "component.agent\\.execute.cpu_user_ms")
                )
                .is_file()
        );
        let timeline = root.join("results/sweep-comparison/perfetto-execution-composition.svg");
        assert!(timeline.is_file());
        let timeline_svg = std::fs::read_to_string(timeline).unwrap();
        assert!(timeline_svg.contains("Agent execution time composition"));
        assert!(timeline_svg.contains("<g data-benchmark=\"000-one\">"));
        assert!(timeline_svg.contains("I/O wait"));
        assert!(
            timeline_svg
                .contains("Observed thread-time: 34.00 ms; elapsed wall reference: 12.00 ms")
        );
        assert!(!timeline_svg.contains("unclassified elapsed wall time"));
        assert!(timeline_svg.contains("not additive wall-time categories"));
        assert!(
            root.join("results/sweep-comparison/metric-host.logical_cpus.svg")
                .is_file()
        );
        let elapsed_svg =
            std::fs::read_to_string(root.join("results/sweep-comparison/metric-elapsed_ms.svg"))
                .unwrap();
        assert!(elapsed_svg.contains("text-anchor=\"end\""));
        assert!(elapsed_svg.contains("<g data-benchmark=\"000-one\">"));
        assert!(elapsed_svg.contains("unit: ms"));
        assert!(elapsed_svg.contains(">0</text>"));
        assert!(elapsed_svg.contains(">12</text>"));
        assert!(
            root.join("results/sweep-comparison")
                .join(GraphNames::default().allocate("metric", "mpstat.cpu\\.user_pct"))
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison")
                .join(
                    GraphNames::default()
                        .allocate("metric", "perfetto.perfetto\\.scheduler_events")
                )
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/series-perfetto.scheduler_events_per_s.svg")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison")
                .join(GraphNames::default().allocate(
                    "metric",
                    "bpftrace-sched-latency.bpftrace\\.sched-latency\\.wakeup_latency_events"
                ))
                .is_file()
        );
        assert!(
            !root
                .join("results/sweep-comparison/series-mpstat.svg")
                .exists()
        );
        let report =
            std::fs::read_to_string(root.join("results/sweep-comparison/report.html")).unwrap();
        let comparison: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("results/sweep-comparison/comparison.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(comparison["benchmarks"][0]["status"], "completed");
        assert!(report.contains("Filter results"));
        assert!(report.contains("data-group=\"agent\""));
        assert!(report.contains("No selected benchmarks have data for this graph."));
        assert!(report.contains("installReportFilters(document)"));
        let line_svg = std::fs::read_to_string(
            root.join("results/sweep-comparison/series-perfetto.scheduler_events_per_s.svg"),
        )
        .unwrap();
        assert!(line_svg.contains("<g data-benchmark=\"000-one\">"));
        assert!(line_svg.contains("</text></g>"));
        assert!(report.contains(">Details</th>"));
        assert!(report.contains("All metrics"));
        assert!(report.contains("metric-tool_call_count.svg"));
        assert!(report.contains("Host metadata"));
        assert!(report.contains("Neoverse-N1"));
        assert!(report.contains("Component timings"));
        assert!(report.contains("network_wait_ms"));
        assert!(comparison["benchmarks"][0]["host_metadata"]["architecture"] == "aarch64");
        assert_eq!(
            comparison["benchmarks"][0]["components"]["components"]["agent.execute"]["cpu_user_ms"],
            3.5
        );
        assert!(!report.contains("run.json"));
        assert!(report.contains("Perfetto metric SVG"));
        assert!(report.contains("<p class=\"perfetto-link\"><strong>Perfetto trace</strong>"));
        assert_eq!(
            report
                .matches("Open Perfetto UI and upload this trace")
                .count(),
            1
        );
        assert!(
            report.find("Perfetto trace").unwrap() < report.find("Run artifacts").unwrap(),
            "the dedicated Perfetto link precedes the generic artifact disclosure"
        );
        assert!(report.contains("perfetto-metric-scheduler_events.svg"));
        assert!(report.contains("perfetto-state-timeline.svg"));
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
        assert!(report.contains("overflow:visible"));
        assert!(
            report.contains(".interactive-chart.expanded .chart-backdrop{display:block;z-index:0}")
        );
        assert!(
            report.contains(".interactive-chart.expanded .chart-stage{position:relative;z-index:1")
        );
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
                "agent-with-a-deliberately-long-series-name-for-the-legend".into(),
                super::TimeSeries {
                    values: vec![Some(0.0), Some(100.0)],
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
        assert!(svg.contains("class=\"x-axis-tick\""));
        assert!(svg.contains("class=\"x-axis-value\""));
        assert!(svg.contains("class=\"x-axis-label\""));
        assert!(svg.contains(">time (ms)</text>"));
        assert!(svg.contains("10 ms"));
        assert!(svg.contains("20 ms"));
        assert!(svg.contains("class=\"legend-item\""));
        assert!(svg.contains("class=\"legend-panel\""));
        assert!(svg.contains("<tspan"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn line_graph_labels_sample_axis_and_gives_long_legends_room() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-sample-axis-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let series = (0..8)
            .map(|index| {
                (
                    format!(
                        "collector-{}-with-a-long-human-readable-series-name",
                        index + 1
                    ),
                    super::TimeSeries {
                        values: vec![Some(1.0), Some(2.0), Some(3.0)],
                        timestamps_ms: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        super::write_line_svg(&root.join("samples.svg"), "cpu.user_pct", &series).unwrap();
        let svg = std::fs::read_to_string(root.join("samples.svg")).unwrap();
        assert!(svg.contains(">sample (index)</text>"));
        assert!(svg.contains("0 index"));
        assert!(svg.contains("2 index"));
        assert!(svg.contains("class=\"legend-panel\""));
        assert!(svg.matches("class=\"legend-item\"").count() == 8);
        let height = svg
            .split_once("height=\"")
            .and_then(|(_, value)| value.split_once('"'))
            .and_then(|(value, _)| value.parse::<f64>().ok())
            .unwrap();
        assert!(height > 620.0);
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
            &[("tall".into(), Some(100.0)), ("short".into(), Some(1.0))],
            &Default::default(),
        )
        .unwrap();
        let svg = std::fs::read_to_string(root.join("values.svg")).unwrap();
        assert!(svg.contains("class=\"bar-value\" data-placement=\"inside\""));
        assert!(svg.contains("class=\"bar-value\" data-placement=\"above\""));
        assert!(svg.contains(">100</text>"));
        assert!(svg.contains(">1</text>"));
        assert!(!svg.contains("zero-baseline"));
        assert!(svg.contains("y=\"70.0\" width=\"397.0\" height=\"320.0\""));
        assert!(svg.contains("y=\"386.8\" width=\"397.0\" height=\"3.2\""));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unavailable_series_remain_visible_and_named() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-unavailable-series-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let observed = super::TimeSeries {
            values: vec![Some(1.0), Some(2.0)],
            timestamps_ms: None,
        };
        super::write_series_svg_with_labels(
            &root.join("values.svg"),
            "cpu.user_pct",
            &[
                ("observed".into(), Some(&observed)),
                ("missing".into(), None),
            ],
            &BTreeMap::from([
                ("observed".into(), "bug-fix:case-a".into()),
                ("missing".into(), "bug-fix:case-b".into()),
            ]),
        )
        .unwrap();
        let svg = std::fs::read_to_string(root.join("values.svg")).unwrap();
        assert!(svg.contains("class=\"series series-unavailable\""));
        assert!(svg.contains("data-status=\"unavailable\""));
        assert!(svg.contains("bug-fix:case-b: unavailable"));
        assert!(svg.contains("bug-fix:case-b — unavailable"));
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
        assert!(summary.contains("measurement collection may be disabled"));
        let parsed: Value = serde_json::from_str(&summary).unwrap();
        let group = parsed["resolution_groups"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(group["task_count"], 1);
        assert_eq!(group["unresolved_count"], 1);
        assert_eq!(group["success_rate"], 0.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn official_resolution_uses_typed_baseline_and_evaluator_not_client_exit() {
        let root = std::env::temp_dir().join(format!(
            "asb-official-resolution-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sweep.toml"),
            "configs=['resolved.toml','unresolved.toml','missing.toml','invalid.toml']\n",
        )
        .unwrap();
        for (index, name, resolved) in [
            (0, "resolved", json!(true)),
            (1, "unresolved", json!(false)),
            (2, "missing", Value::Null),
            (3, "invalid", json!(true)),
        ] {
            std::fs::write(
                root.join(format!("{name}.toml")),
                "agent='pi'\nworkload='swe-bench-lite'\nprovider='local'\nmodel='m'\n",
            )
            .unwrap();
            let baseline = if name == "invalid" {
                json!({"kind":"upstream-task-definition","base_commit":"HEAD"})
            } else {
                json!({"kind":"upstream-task-definition","base_commit":"a".repeat(40)})
            };
            let directory = root.join(format!("sweep-results/{index:03}-{name}/measurements"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("run.json"),
                json!({"schema_version":2,"status":"failed",
                "success":false,"process_success":false,"workload_success":resolved,
                "workload_evaluation":"upstream-harness","workload_baseline":baseline,
                "workload_baseline_passed":false})
                .to_string(),
            )
            .unwrap();
        }
        run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap();
        let comparison: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("results/sweep-comparison/comparison.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(comparison["benchmarks"][0]["metrics"]["success_rate"], 1.0);
        assert_eq!(comparison["benchmarks"][1]["metrics"]["success_rate"], 0.0);
        assert_eq!(comparison["benchmarks"][2]["metrics"]["success_rate"], 0.0);
        assert!(
            comparison["benchmarks"][3]["metrics"]
                .get("success_rate")
                .is_none()
        );
        let groups = comparison["resolution_groups"].as_object().unwrap();
        assert_eq!(
            groups
                .values()
                .map(|group| group["task_count"].as_u64().unwrap())
                .sum::<u64>(),
            3
        );
        assert_eq!(
            groups
                .values()
                .map(|group| group["resolved_count"].as_u64().unwrap())
                .sum::<u64>(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_smoke_and_unknown_scores_cannot_become_task_resolution() {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-scope-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sweep.toml"),
            "configs=['smoke.toml','unknown.toml','task.toml']\n",
        )
        .unwrap();
        for (index, name, workload, evaluation) in [
            (0, "smoke", "swe-bench", "adapter-smoke"),
            (1, "unknown", "custom", "unknown"),
            (2, "task", "bug-fix", "bundled-fixture"),
        ] {
            std::fs::write(
                root.join(format!("{name}.toml")),
                format!("agent='cline'\nworkload='{workload}'\nprovider='local'\nmodel='m'\n"),
            )
            .unwrap();
            let dir = root.join(format!("sweep-results/{index:03}-{name}/measurements"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("run.json"), json!({"schema_version":1,"status":"completed","success":true,"process_success":true,"workload_success":true,"workload_baseline_passed":false,"workload_evaluation":evaluation,"success_rate":1.0,"steps":1,"token_usage":{"total_tokens":8917}}).to_string()).unwrap();
            if index == 0 {
                std::fs::write(dir.join("perfetto-scheduling.perfetto-trace"), b"trace").unwrap();
            }
        }
        let mut output = Vec::new();
        run(root.join("sweep.toml").to_str().unwrap(), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Result report: file://"));
        assert!(output.contains("Perfetto trace file: "));
        assert!(output.contains("Perfetto UI: https://ui.perfetto.dev/"));
        assert!(!output.contains("#!/?url="));
        let comparison: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("results/sweep-comparison/comparison.json"))
                .unwrap(),
        )
        .unwrap();
        for index in [0, 1] {
            assert!(
                comparison["benchmarks"][index]["metrics"]
                    .get("success_rate")
                    .is_none()
            );
            assert!(
                comparison["benchmarks"][index]["metrics"]
                    .get("steps")
                    .is_none()
            );
            assert_eq!(
                comparison["benchmarks"][index]["metrics"]["token_usage.total_tokens"],
                8917.0
            );
        }
        assert_eq!(comparison["benchmarks"][2]["metrics"]["success_rate"], 1.0);
        let groups = comparison["resolution_groups"].as_object().unwrap();
        assert_eq!(
            groups
                .values()
                .filter(|value| value["success_rate"].is_null())
                .count(),
            2
        );
        let html =
            std::fs::read_to_string(root.join("results/sweep-comparison/report.html")).unwrap();
        assert!(html.contains("Not measured"));
        assert!(html.contains("100.0% (1/1)"));
        assert!(html.contains("No upstream task resolution was measured"));
        assert!(html.contains("Open Perfetto UI and upload this trace"));
        assert!(html.contains("https://ui.perfetto.dev/\""));
        assert!(!html.contains("#!/?url="));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn report_preserves_colliding_metric_names_paths_and_repeated_config_stems() {
        let root = std::env::temp_dir().join(format!(
            "asb-metric-identities-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sweep.toml"),
            "configs=['left/same.toml','right/same.toml']",
        )
        .unwrap();
        for (index, side) in ["left", "right"].into_iter().enumerate() {
            std::fs::create_dir_all(root.join(side)).unwrap();
            std::fs::write(
                root.join(side).join("same.toml"),
                "agent='pi'\nworkload='bug-fix'\nprovider='local'\nmodel='m'\n",
            )
            .unwrap();
            let measured = root.join(format!("sweep-results/{index:03}-same/measurements"));
            std::fs::create_dir_all(&measured).unwrap();
            std::fs::write(
                measured.join("run.json"),
                json!({"status":"completed","elapsed_ms":10+index}).to_string(),
            )
            .unwrap();
            std::fs::write(
                measured.join("custom.json"),
                json!({
                    "metrics":{"a/b":113,"a?b":227,"a.b":331,"a":{"b":449},"é":557,"ü":661},
                    "series":{"custom.a/b":[1,2],"custom.a?b":[3,4]}
                })
                .to_string(),
            )
            .unwrap();
        }
        let manifest = root.join("sweep.toml");
        run(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
        let output = root.join("results/sweep-comparison");
        let summary: Value =
            serde_json::from_slice(&std::fs::read(output.join("comparison.json")).unwrap())
                .unwrap();
        let rows = summary["benchmarks"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0]["name"], rows[1]["name"]);
        for row in rows {
            for (key, value) in [
                ("custom.a/b", 113),
                ("custom.a?b", 227),
                ("custom.a\\.b", 331),
                ("custom.a.b", 449),
                ("custom.é", 557),
                ("custom.ü", 661),
            ] {
                assert_eq!(row["metrics"][key].as_f64(), Some(value as f64), "{row}");
            }
        }
        let graphs = summary["graphs"].as_array().unwrap();
        let names: BTreeSet<_> = graphs.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names.len(), graphs.len());
        let html = std::fs::read_to_string(output.join("report.html")).unwrap();
        let mut bar_titles = BTreeMap::new();
        let mut series_titles = BTreeMap::new();
        for name in names {
            let svg = std::fs::read_to_string(output.join(name)).unwrap();
            assert!(html.contains(name), "missing graph reference {name}");
            let title = svg
                .split("<title>")
                .nth(1)
                .unwrap()
                .split("</title>")
                .next()
                .unwrap()
                .to_owned();
            if name.starts_with("metric-") {
                assert_eq!(svg.matches("class=\"bar\"").count(), 2);
                assert!(bar_titles.insert(title, svg).is_none());
            } else if name.starts_with("series-") {
                assert!(series_titles.insert(title, svg).is_none());
            }
        }
        for (name, value) in [
            ("custom.a/b", 113),
            ("custom.a?b", 227),
            ("custom.a\\.b", 331),
            ("custom.a.b", 449),
            ("custom.é", 557),
            ("custom.ü", 661),
        ] {
            assert!(bar_titles[name].contains(&format!("data-value=\"{value}\"")));
        }
        assert!(series_titles.contains_key("custom.a/b"));
        assert!(series_titles.contains_key("custom.a?b"));
        let original_names = summary["graphs"].clone();
        run(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
        let regenerated: Value =
            serde_json::from_slice(&std::fs::read(output.join("comparison.json")).unwrap())
                .unwrap();
        assert_eq!(regenerated["graphs"], original_names);
        // Series keys are a shared collector contract. An ambiguous declaration
        // must fail explicitly regardless of directory iteration order.
        let conflict = root.join("sweep-results/000-same/measurements/other.json");
        std::fs::write(
            &conflict,
            json!({"series":{"custom.a/b":[9,10]}}).to_string(),
        )
        .unwrap();
        let error = run(manifest.to_str().unwrap(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("conflicting measurement identity `custom.a/b`"),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
