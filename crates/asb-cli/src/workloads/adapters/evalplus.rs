//! Pinned EvalPlus HumanEval+ and MBPP+ dataset catalog.

use super::{SetupExtension, WorkloadAdapter};
use crate::workloads::{InstallationContext, Trial};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const HUMANEVAL_URL: &str = "https://github.com/evalplus/humanevalplus_release/releases/download/v0.1.10/HumanEvalPlus.jsonl.gz";
const MBPP_URL: &str =
    "https://github.com/evalplus/mbppplus_release/releases/download/v0.2.0/MbppPlus.jsonl.gz";
const MARKER: &str = "evalplus/HumanEvalPlus@v0.1.10+MbppPlus@v0.2.0";
const LIMIT: u64 = 128 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    marker: String,
    tasks: Vec<Task>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    description: String,
}

pub(super) struct EvalPlusAdapter;
pub(super) static ADAPTER: EvalPlusAdapter = EvalPlusAdapter;

impl WorkloadAdapter for EvalPlusAdapter {
    fn id(&self) -> &'static str {
        "evalplus"
    }
    fn source(&self) -> Option<&'static str> {
        Some(HUMANEVAL_URL)
    }
    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }
    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(read(directory).is_ok())
    }
    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        read(directory)
    }
    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn install(
        &self,
        context: &InstallationContext<'_>,
        output: &mut dyn Write,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> crate::install::InstallResult<bool> {
        let root = context.workloads_root.join(self.id());
        fs::create_dir_all(&root).map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(20, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            HUMANEVAL_URL,
            &root.join("humaneval-plus.jsonl.gz"),
            "EvalPlus HumanEval+ official dataset",
            LIMIT,
        )?;
        progress(60, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            MBPP_URL,
            &root.join("mbpp-plus.jsonl.gz"),
            "EvalPlus MBPP+ official dataset",
            LIMIT,
        )?;
        fs::write(root.join("ASB-DATASET"), format!("{MARKER}\n"))
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        write_catalog(context.workloads_root).map_err(crate::Error::Input)?;
        fs::write(
            context.workloads_root.join("evalplus.txt"),
            "Use the selected installed EvalPlus task definition in `external-workload`.\n",
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::materialize_external_adapter(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::write_default_trial_set(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        writeln!(
            output,
            "Installed EvalPlus HumanEval+ and MBPP+ task catalogs at {}",
            root.display()
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(100, crate::install::ProgressStage::Ready);
        Ok(true)
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id(), workloads, workspace)
    }
}

fn stable_id(source: &str, upstream: &str) -> String {
    let encoded = upstream
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{source}-task-{encoded}")
}

fn parse(path: &Path, source: &str) -> Result<Vec<Trial>, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut input = GzDecoder::new(file).take(LIMIT + 1);
    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() as u64 > LIMIT {
        return Err("dataset exceeds bounded decompressed size".into());
    }
    let mut ids = BTreeSet::new();
    let mut tasks = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let upstream = first_string_field(line, "task_id")?;
        let id = stable_id(source, &upstream);
        if !ids.insert(id.clone()) {
            return Err("dataset has duplicate task ids".into());
        }
        tasks.push(Trial {
            description: format!("EvalPlus {source} official task {upstream}"),
            id,
        });
    }
    if tasks.is_empty() || tasks.len() > 100_000 {
        return Err("dataset has an invalid task population".into());
    }
    Ok(tasks)
}

/// EvalPlus releases retain `NaN` in fields that ASB does not consume.  Those
/// records are useful dataset material but not strict RFC-8259 JSON, so parse
/// only their authoritative leading `task_id` property as a JSON string.
fn first_string_field(line: &str, expected: &str) -> Result<String, String> {
    let rest = line
        .trim_start()
        .strip_prefix('{')
        .ok_or("dataset row is not a JSON object")?
        .trim_start();
    let (key, rest) = take_json_string(rest)?;
    if key != expected {
        return Err(format!("dataset row does not begin with `{expected}`"));
    }
    let rest = rest
        .trim_start()
        .strip_prefix(':')
        .ok_or("dataset task_id has no value")?
        .trim_start();
    let (value, _) = take_json_string(rest)?;
    Ok(value)
}

fn take_json_string(input: &str) -> Result<(String, &str), String> {
    if !input.starts_with('"') {
        return Err("dataset field is not a JSON string".into());
    }
    let mut escaped = false;
    for (index, character) in input.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            let value = serde_json::from_str::<String>(&input[..=index])
                .map_err(|error| format!("invalid dataset string: {error}"))?;
            return Ok((value, &input[index + 1..]));
        }
    }
    Err("unterminated dataset string".into())
}

fn source_tasks(root: &Path) -> Result<Vec<Trial>, String> {
    let mut tasks = parse(&root.join("humaneval-plus.jsonl.gz"), "humaneval-plus")?;
    tasks.extend(parse(&root.join("mbpp-plus.jsonl.gz"), "mbpp-plus")?);
    if tasks
        .iter()
        .map(|task| &task.id)
        .collect::<BTreeSet<_>>()
        .len()
        != tasks.len()
    {
        return Err("EvalPlus datasets have colliding task ids".into());
    }
    Ok(tasks)
}

fn write_catalog(directory: &Path) -> Result<(), String> {
    let root = directory.join("evalplus");
    let tasks = source_tasks(&root)?;
    let bytes = serde_json::to_vec_pretty(&Catalog {
        schema_version: 1,
        marker: MARKER.into(),
        tasks: tasks
            .into_iter()
            .map(|task| Task {
                id: task.id,
                description: task.description,
            })
            .collect(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(root.join("catalog.json"), bytes).map_err(|error| error.to_string())
}

fn read(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("evalplus");
    if fs::read_to_string(root.join("ASB-DATASET"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(
            "EvalPlus has no pinned installed dataset; run `asb workload install evalplus`".into(),
        );
    }
    let catalog: Catalog = serde_json::from_slice(
        &fs::read(root.join("catalog.json")).map_err(|_| "EvalPlus catalog missing")?,
    )
    .map_err(|_| "EvalPlus catalog invalid")?;
    let tasks = catalog
        .tasks
        .into_iter()
        .map(|task| Trial {
            id: task.id,
            description: task.description,
        })
        .collect::<Vec<_>>();
    if catalog.schema_version != 1
        || catalog.marker != MARKER
        || tasks.iter().map(|task| &task.id).collect::<Vec<_>>()
            != source_tasks(&root)?
                .iter()
                .map(|task| &task.id)
                .collect::<Vec<_>>()
    {
        return Err("EvalPlus catalog differs from its installed datasets; reinstall".into());
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_fixture_enumerates_and_loads_both_sources_without_download() {
        let root = std::env::temp_dir().join(format!("asb-evalplus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dataset = root.join("evalplus");
        fs::create_dir_all(&dataset).unwrap();
        for (name, task) in [
            ("humaneval-plus.jsonl.gz", "HumanEval/0"),
            ("mbpp-plus.jsonl.gz", "Mbpp/1"),
        ] {
            let file = fs::File::create(dataset.join(name)).unwrap();
            let mut output = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            writeln!(output, "{{\"task_id\":\"{task}\"}}").unwrap();
            output.finish().unwrap();
        }
        fs::write(dataset.join("ASB-DATASET"), format!("{MARKER}\n")).unwrap();
        write_catalog(&root).unwrap();
        assert_eq!(
            read(&root).unwrap()[0].id,
            "humaneval-plus-task-48756d616e4576616c2f30"
        );
        assert_eq!(read(&root).unwrap()[1].id, "mbpp-plus-task-4d6270702f31");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task_id_extraction_accepts_evalplus_nonfinite_payloads() {
        assert_eq!(
            first_string_field(r#"{ "task_id": "HumanEval/0", "atol": NaN }"#, "task_id").unwrap(),
            "HumanEval/0"
        );
    }
}
