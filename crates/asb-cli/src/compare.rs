//! Offline comparison and SVG chart generation for completed sweeps.

use crate::{Error, read_config};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepFile {
    configs: Vec<String>,
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
        if !run_path.is_file() {
            return Err(Error::Input(format!(
                "missing measurements for {}: expected {} (run the sweep first with measurements enabled)",
                config.display(),
                run_path.display()
            )));
        }
        let run_value = read_json(&run_path)?;
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
            flatten_numbers(name, &collector, &mut metrics);
            if let Some(stdout) = collector.get("stdout").and_then(Value::as_str) {
                let points = numeric_series(stdout);
                if points.len() >= 2 {
                    series.insert(name.to_owned(), points);
                }
            }
        }
        benchmark_data.push((
            format!("{index:03}-{}", safe_name(&config)),
            result_dir,
            metrics,
            series,
        ));
    }

    let mut metric_values: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    let mut series_values: BTreeMap<String, Vec<(String, Vec<f64>)>> = BTreeMap::new();
    for (label, _, metrics, series) in &benchmark_data {
        for (name, value) in metrics {
            metric_values
                .entry(name.clone())
                .or_default()
                .push((label.clone(), *value));
        }
        for (name, values) in series {
            series_values
                .entry(name.clone())
                .or_default()
                .push((label.clone(), values.clone()));
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
        "benchmarks": benchmark_data.iter().map(|(label, directory, metrics, series)| json!({
            "name": label, "directory": directory, "metrics": metrics, "series": series
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
    writeln!(
        output,
        "Compared {} benchmarks; wrote {} graphs and {}",
        benchmark_data.len(),
        graphs.len(),
        comparison_dir.display()
    )
    .map_err(|error| Error::Io(error.to_string()))
}

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

fn numeric_series(text: &str) -> Vec<f64> {
    text.lines()
        .take(2_048)
        .filter_map(|line| {
            line.split_whitespace().rev().find_map(|token| {
                token
                    .trim_matches(|character: char| {
                        !character.is_ascii_digit() && character != '.' && character != '-'
                    })
                    .parse()
                    .ok()
            })
        })
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
    let bottom = 390.0;
    let bar_width = (width - left - 20.0) / values.len().max(1) as f64;
    let mut svg = svg_header(width, height, title);
    svg.push_str(&format!("<line x1=\"{left}\" y1=\"70\" x2=\"{left}\" y2=\"{bottom}\" stroke=\"black\"/><line x1=\"{left}\" y1=\"{bottom}\" x2=\"880\" y2=\"{bottom}\" stroke=\"black\"/>"));
    for (index, (label, value)) in values.iter().enumerate() {
        let x = left + index as f64 * bar_width + 4.0;
        let bar_height = value.max(0.0) / max * 320.0;
        let y = bottom - bar_height;
        svg.push_str(&format!("<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{bar_height:.1}\" fill=\"#4472c4\"><title>{}: {value}</title></rect>", (bar_width - 8.0).max(2.0), escape(label)));
        svg.push_str(&format!("<text x=\"{:.1}\" y=\"410\" transform=\"rotate(35 {:.1} 410)\" font-size=\"11\">{}</text>", x, x, escape(label)));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn write_line_svg(path: &Path, title: &str, values: &[(String, Vec<f64>)]) -> Result<(), Error> {
    let max = values
        .iter()
        .flat_map(|(_, points)| points.iter().copied())
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let mut svg = svg_header(900.0, 460.0, title);
    svg.push_str("<line x1=\"70\" y1=\"70\" x2=\"70\" y2=\"390\" stroke=\"black\"/><line x1=\"70\" y1=\"390\" x2=\"880\" y2=\"390\" stroke=\"black\"/>");
    for (series_index, (label, points)) in values.iter().enumerate() {
        let color = ["#4472c4", "#ed7d31", "#70ad47", "#a5a5a5", "#ffc000"][series_index % 5];
        let coordinates = points
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let x = 75.0 + index as f64 / points.len().max(1) as f64 * 800.0;
                let y = 390.0 - value.max(0.0) / max * 300.0;
                format!("{x:.1},{y:.1}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        svg.push_str(&format!("<polyline points=\"{coordinates}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\"/><text x=\"{}\" y=\"55\" font-size=\"12\" fill=\"{color}\">{}</text>", 80 + series_index * 150, escape(label)));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn svg_header(width: f64, height: f64, title: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\"><title>{}</title><text x=\"20\" y=\"30\" font-size=\"18\">{}</text>",
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
        run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap();
        assert!(
            root.join("results/sweep-comparison/comparison.json")
                .is_file()
        );
        assert!(
            root.join("results/sweep-comparison/metric-elapsed_ms.svg")
                .is_file()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_missing_measurements() {
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
        let error = run(root.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("missing measurements"));
        let _ = std::fs::remove_dir_all(root);
    }
}
