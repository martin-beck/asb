//! Portable deterministic execution extension for adapter-owned runs.
//!
//! This deliberately applies only stable process inputs.  Syscall interposition
//! and schedule control are separate, opt-in backends because they require a
//! sealed cassette payload and must never be silently applied to a client.

use super::PreparationContext;
use crate::Error;
use serde::Serialize;

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    seed: &'static str,
    mode: &'static str,
    workspace: String,
    controls: [&'static str; 6],
    adapter: &'a str,
}

/// Apply the portable extension selected by an adapter.
pub(crate) fn prepare(adapter: &str, context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let mode = context
        .tape
        .map(|tape| match tape.mode {
            crate::tape::Mode::Record => "record",
            crate::tape::Mode::Replay => "replay",
        })
        .unwrap_or("live");
    let directory = context.runtime_dir.join("determinism");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::Run(format!(
            "cannot create deterministic runtime state: {error}"
        ))
    })?;
    let manifest = Manifest {
        schema_version: 1,
        // A deliberately public, fixed base seed.  A later sealed randomness
        // backend derives per-cassette entropy from this contract.
        seed: "asb-determinism-v1",
        mode,
        workspace: context.workspace.display().to_string(),
        controls: [
            "TZ",
            "LC_ALL",
            "LANG",
            "PYTHONHASHSEED",
            "SOURCE_DATE_EPOCH",
            "ASB_DETERMINISM_SEED",
        ],
        adapter,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| Error::Run("cannot encode deterministic execution manifest".into()))?;
    std::fs::write(directory.join("manifest.json"), bytes).map_err(|error| {
        Error::Run(format!(
            "cannot write deterministic execution manifest: {error}"
        ))
    })?;
    let artifact_directory = context.run_dir.join("determinism");
    std::fs::create_dir_all(&artifact_directory).map_err(|error| {
        Error::Run(format!(
            "cannot create deterministic artifact state: {error}"
        ))
    })?;
    std::fs::copy(
        directory.join("manifest.json"),
        artifact_directory.join("manifest.json"),
    )
    .map_err(|error| Error::Run(format!("cannot publish deterministic manifest: {error}")))?;
    if let Some(tape) = context
        .tape
        .filter(|tape| tape.mode == crate::tape::Mode::Replay)
    {
        verify_replay_manifest(adapter, tape.cassette)?;
    }
    context
        .command
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("PYTHONHASHSEED", "0")
        .env("SOURCE_DATE_EPOCH", "0")
        .env("ASB_DETERMINISM_SEED", "asb-determinism-v1")
        .env("ASB_DETERMINISM_MANIFEST", directory.join("manifest.json"));
    Ok(())
}

/// Confirm that replay starts with the immutable controls originally sealed
/// with the provider exchange. Mode and workspace naturally differ between
/// record and replay, so neither is part of this equivalence check.
fn verify_replay_manifest(adapter: &str, cassette: &std::path::Path) -> Result<(), Error> {
    let name = cassette
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Run("replay cassette has no valid private filename".into()))?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"));
    // Bare legacy provider traces cannot prove a deterministic execution
    // envelope. Preserve their historical replay route, but do not claim
    // deterministic equivalence; sealed recordings always use replay*.jsonl.
    let Some(suffix) = suffix else {
        return Ok(());
    };
    let path = cassette
        .parent()
        .ok_or_else(|| Error::Run("replay cassette has no private directory".into()))?
        .join(format!("determinism-manifest{suffix}.json"));
    let recorded: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).map_err(|_| {
        Error::Run(
            "this replay requires its sealed deterministic manifest; record the benchmark again before replaying it".into(),
        )
    })?)
    .map_err(|_| Error::Run("sealed deterministic manifest is malformed".into()))?;
    let expected = serde_json::json!({
        "schema_version": 1,
        "seed": "asb-determinism-v1",
        "adapter": adapter,
        "controls": ["TZ", "LC_ALL", "LANG", "PYTHONHASHSEED", "SOURCE_DATE_EPOCH", "ASB_DETERMINISM_SEED"],
    });
    for field in ["schema_version", "seed", "adapter", "controls"] {
        if recorded.get(field) != expected.get(field) {
            return Err(Error::Run(
                "sealed deterministic manifest is incompatible with the selected adapter".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{Authentication, Connection, Interface};
    use std::process::Command;

    #[test]
    fn portable_extension_writes_manifest_and_pins_process_inputs() {
        let root = std::env::temp_dir().join(format!("asb-determinism-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let mut command = Command::new("fixture");
        let connection = Connection {
            interface: Interface::OpenAiResponses,
            base_url: "http://127.0.0.1/v1".into(),
            credential: None,
            authentication: Authentication::Bearer,
        };
        let mut context = PreparationContext {
            command: &mut command,
            model: "fixture",
            connection: &connection,
            workspace: &root.join("workspace"),
            runtime_dir: &root.join("runtime"),
            run_dir: &root.join("run"),
            tape: None,
        };
        prepare("fixture", &mut context).unwrap();
        let vars: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(vars["TZ"], "UTC");
        assert_eq!(vars["PYTHONHASHSEED"], "0");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("runtime/determinism/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["mode"], "live");
        assert_eq!(
            std::fs::read(root.join("run/determinism/manifest.json")).unwrap(),
            std::fs::read(root.join("runtime/determinism/manifest.json")).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_requires_a_matching_sealed_manifest() {
        let root =
            std::env::temp_dir().join(format!("asb-determinism-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cassette = root.join("replay.jsonl");
        std::fs::write(&cassette, b"{}\n").unwrap();
        std::fs::write(
            root.join("determinism-manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "seed": "asb-determinism-v1",
                "adapter": "fixture",
                "controls": ["TZ", "LC_ALL", "LANG", "PYTHONHASHSEED", "SOURCE_DATE_EPOCH", "ASB_DETERMINISM_SEED"],
            })
            .to_string(),
        )
        .unwrap();
        verify_replay_manifest("fixture", &cassette).unwrap();
        assert!(
            verify_replay_manifest("other", &cassette)
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        std::fs::remove_file(root.join("determinism-manifest.json")).unwrap();
        assert!(
            verify_replay_manifest("fixture", &cassette)
                .unwrap_err()
                .to_string()
                .contains("requires its sealed deterministic manifest")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
