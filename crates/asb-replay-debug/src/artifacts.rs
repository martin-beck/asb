//! Bounded readers for existing evidence; no runtime or replay dependencies.

use serde_json::Value;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

pub(crate) fn read(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is not a regular file within the 32 MiB input limit",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{} grew beyond the 32 MiB input limit",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))
}

fn entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))? {
        paths.push(entry.map_err(|error| error.to_string())?.path());
        if paths.len() > MAX_ENTRIES {
            return Err(format!(
                "{} exceeds the 4096-entry directory limit; open a batch or mismatch file",
                path.display()
            ));
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn latest_campaign(project: &Path) -> Result<PathBuf, String> {
    let root = project.join("results/asb-executions");
    let mut candidates = Vec::new();
    for path in entries(&root)? {
        if path.is_dir() && path.join("plan.json").is_file() {
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| error.to_string())?;
            candidates.push((modified, path));
        }
    }
    candidates.sort();
    candidates
        .pop()
        .map(|(_, path)| path)
        .ok_or_else(|| format!("no campaign with plan.json found in {}", root.display()))
}

#[derive(Default)]
pub(crate) struct Outcome {
    pub(crate) replica: Option<u64>,
    pub(crate) track: Option<u64>,
    pub(crate) execution: Option<bool>,
    pub(crate) grade: Option<bool>,
    pub(crate) cleanup: Option<bool>,
    pub(crate) elapsed_ms: Option<u64>,
}

pub(crate) struct Case {
    pub(crate) path: PathBuf,
    pub(crate) outcome: Outcome,
}

impl Case {
    pub(crate) fn label(&self) -> String {
        let batch = self
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        match self.outcome.replica {
            Some(replica) => format!("{batch} / replica {replica}"),
            None => format!(
                "{batch} / unattributed {}",
                self.path.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }

    pub(crate) fn load(&self) -> Result<Evidence, String> {
        let document: Value = serde_json::from_str(&read(&self.path)?)
            .map_err(|error| format!("{}: invalid mismatch JSON: {error}", self.path.display()))?;
        if document.get("schema_version").and_then(Value::as_u64) != Some(1) {
            return Err("unsupported mismatch schema; expected schema_version = 1".into());
        }
        let request = |side: &str| -> Result<Value, String> {
            let mut value = document
                .get(side)
                .filter(|value| value.is_object())
                .cloned()
                .ok_or_else(|| format!("mismatch evidence is missing its {side} request"))?;
            if !value.get("method").is_some_and(Value::is_string)
                || !value.get("url").is_some_and(Value::is_string)
                || value.get("body").is_none()
            {
                return Err(format!("{side} request needs method, url and body"));
            }
            if let Some(body) = value.get("body").and_then(Value::as_str)
                && let Ok(parsed) = serde_json::from_str::<Value>(body)
            {
                value["body"] = parsed;
            }
            Ok(value)
        };
        let mismatch = document
            .get("mismatch")
            .filter(|value| value.is_object())
            .ok_or("mismatch evidence is missing its mismatch descriptor")?;
        Ok(Evidence {
            expected: request("expected")?,
            actual: request("actual")?,
            sequence: mismatch.get("sequence").and_then(Value::as_u64),
            reported_path: mismatch
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .into(),
            category: mismatch
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .into(),
        })
    }
}

pub(crate) struct Evidence {
    pub(crate) expected: Value,
    pub(crate) actual: Value,
    pub(crate) sequence: Option<u64>,
    pub(crate) reported_path: String,
    pub(crate) category: String,
}

pub(crate) struct ArtifactSet {
    pub(crate) root: PathBuf,
    pub(crate) cases: Vec<Case>,
    pub(crate) warnings: Vec<String>,
}

impl ArtifactSet {
    pub(crate) fn open(input: &Path) -> Result<Self, String> {
        let root =
            fs::canonicalize(input).map_err(|error| format!("{}: {error}", input.display()))?;
        let mut paths = Vec::new();
        if root.is_file() {
            paths.push(root.clone());
        } else {
            for path in entries(&root)? {
                if is_mismatch(&path) {
                    paths.push(path);
                } else if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("batch-"))
                    && path.is_dir()
                {
                    paths.extend(entries(&path)?.into_iter().filter(|path| is_mismatch(path)));
                }
                if paths.len() > MAX_ENTRIES {
                    return Err("more than 4096 mismatches; open a single batch".into());
                }
            }
        }
        if paths.is_empty() {
            return Err(format!(
                "no retained mismatch files in {}; use a failed campaign, batch directory, or .mismatch.json file (missing capture is not proof of a match)",
                root.display()
            ));
        }
        let mut warnings = Vec::new();
        let mut cases: Vec<_> = paths
            .into_iter()
            .map(|path| Case {
                path,
                outcome: Outcome::default(),
            })
            .collect();
        let parents: std::collections::BTreeSet<_> =
            cases.iter().filter_map(|case| case.path.parent()).collect();
        let mut records = Vec::new();
        for parent in parents {
            for path in entries(parent)? {
                if path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("run") && name.ends_with(".jsonl")
                }) {
                    match last_record(&path) {
                        Ok(record) => records.push((parent.to_path_buf(), record)),
                        Err(error) => warnings.push(error),
                    }
                }
            }
        }
        for case in &mut cases {
            let matches: Vec<_> = records
                .iter()
                .filter(|(parent, record)| {
                    case.path.parent() == Some(parent.as_path()) && refers_to(record, &case.path)
                })
                .collect();
            if let [(.., record)] = matches.as_slice() {
                case.outcome = Outcome {
                    replica: record
                        .get("replica_index")
                        .and_then(Value::as_u64)
                        .or_else(|| record.pointer("/execution/replica").and_then(Value::as_u64)),
                    track: record
                        .pointer("/execution/replay_track")
                        .and_then(Value::as_u64),
                    execution: record.get("process_success").and_then(Value::as_bool),
                    grade: record.get("workload_success").and_then(Value::as_bool),
                    cleanup: record
                        .pointer("/benchmark_outcome/cleanup_confirmed")
                        .and_then(Value::as_bool),
                    elapsed_ms: record.get("elapsed_ms").and_then(Value::as_u64),
                };
            } else if !matches.is_empty() {
                warnings.push(format!(
                    "{} is claimed by multiple run logs; attribution left unknown",
                    case.path.display()
                ));
            }
        }
        cases.sort_by(|left, right| {
            (left.path.parent(), left.outcome.replica, &left.path).cmp(&(
                right.path.parent(),
                right.outcome.replica,
                &right.path,
            ))
        });
        Ok(Self {
            root,
            cases,
            warnings,
        })
    }
}

fn is_mismatch(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".mismatch.json"))
        && path.is_file()
}

fn last_record(path: &Path) -> Result<Value, String> {
    let text = read(path)?;
    let last = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| format!("{}: empty run log", path.display()))?;
    serde_json::from_str(last)
        .map_err(|error| format!("{}: invalid final run record: {error}", path.display()))
}

fn refers_to(record: &Value, path: &Path) -> bool {
    record
        .get("failure_reason")
        .and_then(Value::as_str)
        .is_some_and(|reason| {
            reason.split_whitespace().any(|word| {
                Path::new(word.trim_end_matches([';', ','])).file_name() == path.file_name()
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use serde_json::json;

    #[test]
    fn attributes_moved_evidence_by_log_reference_not_scratch_suffix() {
        let fixture = Fixture::new();
        for (suffix, replica) in [(0, 2), (3, 1)] {
            let filename = format!(".asb-tape-12-{suffix}.mismatch.json");
            fixture.write(
                &format!("batch-1/{filename}"),
                &json!({
                    "schema_version":1,"mismatch":{"sequence":18},
                    "expected":{"method":"POST","url":"/responses","body":"{\"input\":\"old\"}"},
                    "actual":{"method":"POST","url":"/responses","body":{"input":"new"}}
                })
                .to_string(),
            );
            fixture.write(&format!("batch-1/run-concurrency-{replica}.jsonl"),&json!({
                "replica_index":replica,"execution":{"replay_track":1},
                "process_success":false,"workload_success":true,
                "failure_reason":format!("private mismatch evidence retained at /previous/project/batch-1/{filename}; mismatch")
            }).to_string());
        }
        let set = ArtifactSet::open(&fixture.path).unwrap();
        assert_eq!(set.cases[0].outcome.replica, Some(1));
        assert!(set.cases[0].path.ends_with(".asb-tape-12-3.mismatch.json"));
        assert_eq!(set.cases[0].outcome.execution, Some(false));
        assert_eq!(set.cases[0].outcome.grade, Some(true));
        assert_eq!(set.cases[0].outcome.cleanup, None);
        assert!(set.cases[0].load().unwrap().expected["body"].is_object());
    }

    #[test]
    fn malformed_last_record_does_not_borrow_earlier_attribution() {
        let fixture = Fixture::new();
        let path = fixture.write("batch-1/example.mismatch.json", "{}");
        fixture.write(
            "batch-1/run.jsonl",
            "{\"replica_index\":1,\"failure_reason\":\"example.mismatch.json\"}\n{broken",
        );
        let set = ArtifactSet::open(&fixture.path).unwrap();
        assert_eq!(set.warnings.len(), 1);
        assert_eq!(set.cases[0].outcome.replica, None);
        assert!(set.cases[0].load().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{}");
    }

    #[test]
    fn rejects_oversized_input_before_reading_it() {
        let fixture = Fixture::new();
        let path = fixture.write("oversized.mismatch.json", "");
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(read(&path).unwrap_err().contains("32 MiB"));
    }

    #[test]
    fn latest_does_not_silently_substitute_an_older_failed_campaign() {
        let fixture = Fixture::new();
        fixture.write("results/asb-executions/older/plan.json", "{}");
        fixture.write(
            "results/asb-executions/older/batch-1/example.mismatch.json",
            "{}",
        );
        fixture.write("results/asb-executions/newer/plan.json", "{}");
        // Set a deterministic timestamp without relying on sleeps or directory order.
        let older = fixture.path.join("results/asb-executions/older");
        let newer = fixture.path.join("results/asb-executions/newer");
        File::open(&older)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        assert_eq!(latest_campaign(&fixture.path).unwrap(), newer);
        assert!(
            ArtifactSet::open(&newer)
                .err()
                .unwrap()
                .contains("no retained mismatch")
        );
    }
}
