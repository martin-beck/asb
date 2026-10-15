//! Optional, file-backed measurements for a benchmark run.
//!
//! Measurement collection is deliberately opt-in.  The default run path does
//! not inspect the host or start any auxiliary process.  When enabled, every
//! result is written as bounded JSON so later processing never has to parse
//! terminal output.

use serde::Serialize;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_CAPTURE: usize = 256 * 1024;
const MAX_RUNS: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub source: Option<String>,
}

/// Stable names used in generated configuration files and downstream tools.
pub fn available_collectors() -> Vec<String> {
    [
        "system",
        "runtime",
        "mpstat",
        "iostat",
        "perf",
        "flamegraph",
        "bpftrace",
        "token_usage",
        "success_rate",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct RunMeasurement<'a> {
    pub schema_version: u32,
    pub run_id: String,
    pub status: &'a str,
    pub success: bool,
    pub success_rate: f64,
    pub agent: &'a str,
    pub workload: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub elapsed_ms: u128,
    pub exit_code: Option<i32>,
    pub token_usage: TokenUsage,
    pub started_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct SystemInfo {
    schema_version: u32,
    collected_unix_ms: u128,
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    kernel_release: Option<String>,
    memory_total_kib: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CollectorResult {
    schema_version: u32,
    collector: String,
    status: String,
    exit_code: Option<i32>,
    elapsed_ms: u128,
    stdout: String,
    stderr: String,
}

struct Collector {
    name: String,
    child: Child,
    started: Instant,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// A measurement session owns all optional collector processes for one run.
pub struct Session {
    dir: PathBuf,
    tool_dir: Option<PathBuf>,
    started_unix_ms: u128,
    collectors: Vec<Collector>,
    perf_data: Option<PathBuf>,
    perf_enabled: bool,
}

impl Session {
    pub fn started_unix_ms(&self) -> u128 {
        self.started_unix_ms
    }
    /// Start a session and write the host description.  Collectors are enabled
    /// only by explicit ASB_MEASURE_* variables, keeping CI deterministic.
    /// Start with the stable collector names from a generated configuration.
    /// An empty list means environment-only opt-in, useful for tests and
    /// hand-written configurations.
    pub fn start_with_collectors(
        dir: &Path,
        selected: &[String],
        tool_dir: Option<&Path>,
    ) -> Result<Self, String> {
        fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create measurements directory: {e}"))?;
        let started_unix_ms = unix_ms();
        let info = SystemInfo {
            schema_version: 1,
            collected_unix_ms: started_unix_ms,
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get),
            kernel_release: read_trimmed("/proc/sys/kernel/osrelease"),
            memory_total_kib: read_mem_total(),
        };
        write_json(&dir.join("system.json"), &info)?;

        let mut session = Self {
            dir: dir.to_path_buf(),
            tool_dir: tool_dir.map(Path::to_path_buf),
            started_unix_ms,
            collectors: Vec::new(),
            perf_data: None,
            perf_enabled: selected_or_env("perf", "ASB_MEASURE_PERF", selected),
        };
        session.start_snapshot(
            "mpstat",
            "ASB_MEASURE_MPSTAT",
            &["-P", "ALL", "1"],
            selected,
        )?;
        session.start_snapshot("iostat", "ASB_MEASURE_IOSTAT", &["-x", "1"], selected)?;
        if selected_or_env("bpftrace", "ASB_MEASURE_BPFTRACE", selected)
            && std::env::var_os("ASB_BPFTRACE_PROGRAMS").is_some()
        {
            session.start_bpftrace()?;
        }
        Ok(session)
    }

    /// Attach perf to the selected child after it has been spawned.
    pub fn attach_pid(&mut self, pid: u32) -> Result<(), String> {
        if !self.perf_enabled {
            return Ok(());
        }
        let data = self.dir.join("perf.data");
        let stderr_path = self.dir.join("perf.stderr");
        let output =
            File::create(&stderr_path).map_err(|e| format!("cannot create perf output: {e}"))?;
        let stdout_path = self.dir.join("perf.stdout");
        let stdout = File::create(&stdout_path)
            .map_err(|e| format!("cannot create perf standard output: {e}"))?;
        let child = match self
            .command("perf")
            .args(["record", "-g", "-p"])
            .arg(pid.to_string())
            .args(["-o"])
            .arg(&data)
            .stderr(Stdio::from(output))
            .stdout(Stdio::from(stdout))
            .spawn()
        {
            Ok(child) => child,
            Err(error) if !env_true("ASB_MEASURE_PERF") => {
                write_unavailable(&self.dir.join("perf.json"), "perf", error.to_string())?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start perf (disable with ASB_MEASURE_PERF=0): {error}"
                ));
            }
        };
        self.perf_data = Some(data);
        self.collectors.push(Collector {
            name: "perf".into(),
            child,
            started: Instant::now(),
            stdout_path,
            stderr_path,
        });
        Ok(())
    }

    /// Finish collectors and write the run record and aggregate summary.
    pub fn finish<'a>(mut self, measurement: RunMeasurement<'a>) -> Result<(), String> {
        for mut collector in self.collectors.drain(..) {
            let status = match collector.child.try_wait() {
                Ok(Some(status)) => status,
                Ok(None) => {
                    let _ = collector.child.kill();
                    collector
                        .child
                        .wait()
                        .map_err(|e| format!("cannot finish {} collector: {e}", collector.name))?
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect {} collector: {error}",
                        collector.name
                    ));
                }
            };
            let stdout = read_bounded(&collector.stdout_path);
            let stderr = read_bounded(&collector.stderr_path);
            let _ = fs::remove_file(&collector.stdout_path);
            let _ = fs::remove_file(&collector.stderr_path);
            let result = CollectorResult {
                schema_version: 1,
                collector: collector.name.clone(),
                status: if status.success() {
                    "completed"
                } else {
                    "failed"
                }
                .into(),
                exit_code: status.code(),
                elapsed_ms: collector.started.elapsed().as_millis(),
                stdout,
                stderr,
            };
            write_json(&self.dir.join(format!("{}.json", collector.name)), &result)?;
        }
        write_json(&self.dir.join("run.json"), &measurement)?;
        self.write_flamegraph()?;
        update_summary(&self.dir, &measurement)
    }

    fn start_snapshot(
        &mut self,
        name: &str,
        variable: &str,
        args: &[&str],
        selected: &[String],
    ) -> Result<(), String> {
        if !selected_or_env(name, variable, selected) {
            return Ok(());
        }
        let stdout_path = self.dir.join(format!("{name}.stdout"));
        let stderr_path = self.dir.join(format!("{name}.stderr"));
        let child = match self
            .command(name)
            .args(args)
            .stdout(Stdio::from(
                File::create(&stdout_path)
                    .map_err(|e| format!("cannot create {name} output: {e}"))?,
            ))
            .stderr(Stdio::from(File::create(&stderr_path).map_err(|e| {
                format!("cannot create {name} error output: {e}")
            })?))
            .spawn()
        {
            Ok(child) => child,
            Err(error) if !env_true(variable) => {
                write_unavailable(
                    &self.dir.join(format!("{name}.json")),
                    name,
                    error.to_string(),
                )?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start {name}; install it or disable {variable}=1: {error}"
                ));
            }
        };
        self.collectors.push(Collector {
            name: name.into(),
            child,
            started: Instant::now(),
            stdout_path,
            stderr_path,
        });
        Ok(())
    }

    fn start_bpftrace(&mut self) -> Result<(), String> {
        let specs = std::env::var("ASB_BPFTRACE_PROGRAMS")
            .map_err(|_| "ASB_MEASURE_BPFTRACE=1 requires ASB_BPFTRACE_PROGRAMS".to_owned())?;
        for (index, spec) in specs
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .enumerate()
        {
            let (program, params) = spec.split_once('?').map_or((spec, ""), |parts| parts);
            let mut command = self.command("bpftrace");
            command.arg(program);
            for parameter in params.split('&').filter(|s| !s.is_empty()) {
                let (key, value) = parameter.split_once('=').ok_or_else(|| {
                    format!("invalid bpftrace parameter `{parameter}`; use PROGRAM?name=value")
                })?;
                if key.is_empty() || value.is_empty() {
                    return Err(format!("invalid bpftrace parameter `{parameter}`"));
                }
                command.args(["-D", &format!("{key}={value}")]);
            }
            let stdout_path = self.dir.join(format!("bpftrace-{index}.stdout"));
            let stderr_path = self.dir.join(format!("bpftrace-{index}.stderr"));
            let child = match command
                .stdout(Stdio::from(
                    File::create(&stdout_path)
                        .map_err(|e| format!("cannot create bpftrace output: {e}"))?,
                ))
                .stderr(Stdio::from(File::create(&stderr_path).map_err(|e| {
                    format!("cannot create bpftrace error output: {e}")
                })?))
                .spawn()
            {
                Ok(child) => child,
                Err(error) if !env_true("ASB_MEASURE_BPFTRACE") => {
                    write_unavailable(
                        &self.dir.join(format!("bpftrace-{index}.json")),
                        &format!("bpftrace-{index}"),
                        error.to_string(),
                    )?;
                    continue;
                }
                Err(error) => {
                    return Err(format!("cannot start bpftrace program {program}: {error}"));
                }
            };
            self.collectors.push(Collector {
                name: format!("bpftrace-{index}"),
                child,
                started: Instant::now(),
                stdout_path,
                stderr_path,
            });
        }
        Ok(())
    }

    fn write_flamegraph(&self) -> Result<(), String> {
        let Some(data) = self.perf_data.as_ref() else {
            return Ok(());
        };
        let Some(collapse) = std::env::var_os("ASB_FLAMEGRAPH_STACKCOLLAPSE") else {
            return Ok(());
        };
        let Some(render) = std::env::var_os("ASB_FLAMEGRAPH_RENDER") else {
            return Ok(());
        };
        let script = self
            .command("perf")
            .args(["script", "-i"])
            .arg(data)
            .output()
            .map_err(|e| format!("cannot run perf script for flamegraph: {e}"))?;
        if !script.status.success() {
            return Err("perf script failed; flamegraph was not generated".into());
        }
        let collapsed = Command::new(collapse)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start flamegraph stack collapse script: {e}"))?;
        let mut collapsed = collapsed;
        if let Some(mut input) = collapsed.stdin.take() {
            input
                .write_all(&script.stdout)
                .map_err(|e| format!("cannot feed stack collapse script: {e}"))?;
        }
        let collapsed_output = collapsed
            .wait_with_output()
            .map_err(|e| format!("cannot finish stack collapse script: {e}"))?;
        if !collapsed_output.status.success() {
            return Err("flamegraph stack collapse script failed".into());
        }
        let mut render = Command::new(render);
        render.stdin(Stdio::piped()).stdout(Stdio::from(
            File::create(self.dir.join("flamegraph.svg"))
                .map_err(|e| format!("cannot create flamegraph.svg: {e}"))?,
        ));
        let mut render = render
            .spawn()
            .map_err(|e| format!("cannot start flamegraph renderer: {e}"))?;
        if let Some(mut input) = render.stdin.take() {
            input
                .write_all(&collapsed_output.stdout)
                .map_err(|e| format!("cannot feed flamegraph renderer: {e}"))?;
        }
        let status = render
            .wait()
            .map_err(|e| format!("cannot finish flamegraph renderer: {e}"))?;
        if !status.success() {
            return Err("flamegraph renderer failed".into());
        }
        #[derive(Serialize)]
        struct Flamegraph<'a> {
            schema_version: u32,
            status: &'a str,
            artifact: &'a str,
        }
        write_json(
            &self.dir.join("flamegraph.json"),
            &Flamegraph {
                schema_version: 1,
                status: "completed",
                artifact: "flamegraph.svg",
            },
        )
    }

    fn command(&self, name: &str) -> Command {
        if let Some(tool_dir) = &self.tool_dir {
            let local = tool_dir.join("bin").join(name);
            if local.is_file() {
                return Command::new(local);
            }
        }
        Command::new(name)
    }
}

/// Extract token usage from environment conventions, agent output, and an
/// optional llmtape cassette. Providers and agents use different names and
/// response shapes, so all sources are normalized into one stable record.
pub fn token_usage_from_sources(
    provider: &str,
    agent: &str,
    stdout: &str,
    stderr: &str,
    cassette: Option<&Path>,
) -> TokenUsage {
    let mut usage = usage_from_environment(provider, agent);
    let output_usage = usage_from_text(stdout).merge(usage_from_text(stderr));
    usage = usage.merge_with_source(output_usage, "agent-output");
    if let Some(path) = cassette {
        if let Ok(text) = read_bounded_text(path, 16 * 1024 * 1024) {
            usage = usage.merge_with_source(usage_from_text(&text), "cassette");
        }
    }
    usage.with_derived_total()
}

impl TokenUsage {
    fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.or(other.input_tokens),
            output_tokens: self.output_tokens.or(other.output_tokens),
            cached_tokens: self.cached_tokens.or(other.cached_tokens),
            total_tokens: self.total_tokens.or(other.total_tokens),
            source: self.source.or(other.source),
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: add_values(self.input_tokens, other.input_tokens),
            output_tokens: add_values(self.output_tokens, other.output_tokens),
            cached_tokens: add_values(self.cached_tokens, other.cached_tokens),
            total_tokens: add_values(self.total_tokens, other.total_tokens),
            source: self.source.or(other.source),
        }
    }

    fn merge_with_source(self, other: Self, source: &str) -> Self {
        let found = other.any();
        let mut merged = self.merge(other);
        if found {
            merged.source = Some(match merged.source.take() {
                Some(existing) if existing != source => format!("{existing}+{source}"),
                _ => source.into(),
            });
        }
        merged
    }

    fn with_derived_total(mut self) -> Self {
        if self.total_tokens.is_none() {
            self.total_tokens = self
                .input_tokens
                .zip(self.output_tokens)
                .map(|(input, output)| input + output);
        }
        self
    }

    fn any(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_tokens.is_some()
            || self.total_tokens.is_some()
    }
}

fn usage_from_environment(provider: &str, agent: &str) -> TokenUsage {
    let mut prefixes = vec!["ASB".to_owned()];
    for value in [provider, agent] {
        let normalized = value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if !normalized.is_empty() && !prefixes.iter().any(|prefix| prefix == &normalized) {
            prefixes.push(normalized);
        }
    }
    let lookup = |suffixes: &[&str]| {
        prefixes.iter().find_map(|prefix| {
            suffixes
                .iter()
                .find_map(|suffix| env_u64(&format!("{prefix}_{suffix}")))
        })
    };
    let input = lookup(&["INPUT_TOKENS", "PROMPT_TOKENS", "INPUT_TOKEN_COUNT"]);
    let output = lookup(&["OUTPUT_TOKENS", "COMPLETION_TOKENS", "OUTPUT_TOKEN_COUNT"]);
    let cached = lookup(&[
        "CACHED_TOKENS",
        "CACHE_READ_INPUT_TOKENS",
        "PROMPT_CACHED_TOKENS",
    ]);
    let total = lookup(&["TOTAL_TOKENS", "TOKEN_COUNT"]);
    let usage = TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        total_tokens: total,
        source: None,
    };
    if usage.any() {
        TokenUsage {
            source: Some("environment".into()),
            ..usage
        }
    } else {
        usage
    }
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            total_tokens: None,
            source: None,
        }
    }
}

fn usage_from_text(text: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if text.contains('\n')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim())
    {
        usage = usage.add(usage_from_json(&value));
    }
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            usage = usage.add(usage_from_json(&value));
            continue;
        }
        usage = usage.add(usage_from_key_values(line));
    }
    usage
}

fn usage_from_json(value: &serde_json::Value) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Some(object) = value.as_object() {
        usage.input_tokens = find_number(object, &["input_tokens", "prompt_tokens", "inputTokens"]);
        usage.output_tokens = find_number(
            object,
            &["output_tokens", "completion_tokens", "outputTokens"],
        );
        usage.cached_tokens = find_number(
            object,
            &["cached_tokens", "cache_read_input_tokens", "cachedTokens"],
        );
        usage.total_tokens = find_number(object, &["total_tokens", "totalTokens"]);
        for (name, child) in object {
            if name == "tokens" {
                usage = usage.merge(usage_from_opencode_tokens(child));
            } else if ![
                "input_tokens",
                "prompt_tokens",
                "inputTokens",
                "output_tokens",
                "completion_tokens",
                "outputTokens",
                "cached_tokens",
                "cache_read_input_tokens",
                "cachedTokens",
                "total_tokens",
                "totalTokens",
            ]
            .contains(&name.as_str())
            {
                usage = usage.merge(usage_from_json(child));
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            usage = usage.merge(usage_from_json(child));
        }
    } else if let Some(text) = value.as_str() {
        if let Ok(nested) = serde_json::from_str::<serde_json::Value>(text) {
            usage = usage.merge(usage_from_json(&nested));
        }
    }
    usage
}

fn usage_from_opencode_tokens(value: &serde_json::Value) -> TokenUsage {
    let Some(object) = value.as_object() else {
        return TokenUsage::default();
    };
    let cached = object.get("cache").and_then(|cache| {
        cache
            .get("read")
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
    });
    TokenUsage {
        input_tokens: find_number(object, &["input"]),
        output_tokens: find_number(object, &["output"]),
        cached_tokens: cached,
        total_tokens: find_number(object, &["total"]),
        source: None,
    }
}

fn add_values(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn find_number(object: &serde_json::Map<String, serde_json::Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn usage_from_key_values(line: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    for part in line.split(|character: char| {
        character.is_whitespace() || character == ',' || character == '{' || character == '}'
    }) {
        let Some((key, value)) = part.split_once(['=', ':']) else {
            continue;
        };
        let number = value.trim_matches(['"', '\'']).parse::<u64>().ok();
        let Some(number) = number else { continue };
        match key.trim_matches(['"', '\'']).to_ascii_lowercase().as_str() {
            "input_tokens" | "prompt_tokens" | "inputtokens" | "prompttokens" => {
                usage.input_tokens = Some(number)
            }
            "output_tokens" | "completion_tokens" | "outputtokens" | "completiontokens" => {
                usage.output_tokens = Some(number)
            }
            "cached_tokens" | "cachedtokens" | "cache_read_input_tokens" => {
                usage.cached_tokens = Some(number)
            }
            "total_tokens" | "totaltokens" => usage.total_tokens = Some(number),
            _ => {}
        }
    }
    usage
}

fn read_bounded_text(path: &Path, max_bytes: u64) -> Result<String, std::io::Error> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn update_summary(dir: &Path, run: &RunMeasurement<'_>) -> Result<(), String> {
    let path = dir.join("summary.json");
    let mut runs: Vec<serde_json::Value> = if path.exists() {
        let mut text = String::new();
        File::open(&path)
            .map_err(|e| format!("cannot open summary: {e}"))?
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read summary: {e}"))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("runs").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    runs.push(serde_json::to_value(run).map_err(|e| e.to_string())?);
    if runs.len() > MAX_RUNS {
        runs.drain(..runs.len() - MAX_RUNS);
    }
    let success_count = runs
        .iter()
        .filter(|v| v.get("success").and_then(serde_json::Value::as_bool) == Some(true))
        .count();
    let summary = serde_json::json!({"schema_version": 1, "runs": runs, "success_count": success_count, "failure_count": runs.len() - success_count, "success_rate": success_count as f64 / runs.len().max(1) as f64});
    write_json(&path, &summary)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    serde_json::to_writer_pretty(file, value)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
fn write_unavailable(path: &Path, collector: &str, error: String) -> Result<(), String> {
    write_json(
        path,
        &CollectorResult {
            schema_version: 1,
            collector: collector.into(),
            status: "unavailable".into(),
            exit_code: None,
            elapsed_ms: 0,
            stdout: String::new(),
            stderr: error,
        },
    )
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_CAPTURE)]).into_owned()
}
fn read_bounded(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(MAX_CAPTURE as u64);
    if limited.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    bounded(&bytes)
}
fn env_true(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}
fn selected_or_env(name: &str, variable: &str, selected: &[String]) -> bool {
    selected.iter().any(|value| value == name) || env_true(variable)
}
fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}
fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}
fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}
fn read_mem_total() -> Option<u64> {
    read_trimmed("/proc/meminfo")?.lines().find_map(|line| {
        line.strip_prefix("MemTotal:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn writes_machine_readable_run_and_summary() {
        let dir = std::env::temp_dir().join(format!("asb-measure-{}", unix_ms()));
        let session = Session::start_with_collectors(&dir, &[], None).expect("session");
        let run = RunMeasurement {
            schema_version: 1,
            run_id: "test".into(),
            status: "completed",
            success: true,
            success_rate: 1.0,
            agent: "a",
            workload: "w",
            provider: "p",
            model: "m",
            elapsed_ms: 2,
            exit_code: Some(0),
            token_usage: TokenUsage::default(),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        };
        session.finish(run).expect("finish");
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(parsed["success_rate"], 1.0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_openrouter_usage_details_from_agent_json() {
        let usage = token_usage_from_sources(
            "openrouter",
            "opencode",
            r#"{"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":40}}}"#,
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.cached_tokens, Some(40));
        assert_eq!(usage.total_tokens, Some(150));
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
    }

    #[test]
    fn parses_cli2key_text_aliases_and_derives_total() {
        let usage = token_usage_from_sources(
            "cli2key",
            "codex",
            "input_tokens=8 output_tokens=5 cached_tokens=2",
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cached_tokens, Some(2));
        assert_eq!(usage.total_tokens, Some(13));
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
    }

    #[test]
    fn reads_usage_from_a_recorded_cassette() {
        let path = std::env::temp_dir().join(format!("asb-usage-cassette-{}", unix_ms()));
        fs::write(
            &path,
            r#"{"request":{},"response":{"body":"{\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}"}}"#,
        )
        .expect("cassette writes");
        let usage = token_usage_from_sources("cli2key", "codex", "", "", Some(&path));
        assert_eq!(usage.input_tokens, Some(2));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(5));
        let _ = fs::remove_file(path);
    }
}
