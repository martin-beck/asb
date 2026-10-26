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
        std::fs::remove_dir_all(root).unwrap();
    }
}
