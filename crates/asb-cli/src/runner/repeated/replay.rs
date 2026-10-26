//! Run-owned whole-track selection and private JSONL staging; no proxy changes.
use super::{Plan, ReplayInput, Slot, path_text, private_directory, publish_bytes};
use crate::cassette::{Compatibility, Limits, SelectionPlan, Selector, Source, SourceInspection};
use crate::{Error, benchmark::BenchmarkConfig, runner::RunOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize)]
pub(super) struct BatchChoices {
    pub seed: Option<u64>,
    pub algorithm: Option<String>,
    pub tracks: Vec<u64>,
    #[serde(skip)]
    pub statistics: Vec<Option<crate::cassette::Statistics>>,
}

/// Own the opened source until all selected entries have been verified/staged.
/// Legacy no-selector replay deliberately retains its historical routing.
pub(super) struct Prepared {
    source: Source,
    path: PathBuf,
    chosen: Vec<u64>,
    pub selection: Option<SelectionPlan>,
}

pub(super) struct Inspection {
    source: SourceInspection,
    path: PathBuf,
    chosen: Vec<u64>,
    pub selection: Option<SelectionPlan>,
    pub compatibility: Option<Compatibility>,
}

impl Inspection {
    pub fn validate(&self) -> Result<(), Error> {
        self.source.validate().map_err(input_error)
    }

    pub fn open(
        config_path: &Path,
        config: &BenchmarkConfig,
        options: RunOptions<'_>,
        repeat: u64,
    ) -> Result<Option<Self>, Error> {
        if options.mode != Some(crate::tape::Mode::Replay) {
            return Ok(None);
        }
        let path = super::super::replay_cassette(config_path, config, options.cassette)?;
        if !Source::is_bundle(&path).map_err(input_error)?
            && options.track.is_none()
            && options.replay_seed.is_none()
        {
            return Ok(None);
        }
        let source = Source::inspect(&path, Limits::default()).map_err(input_error)?;
        let (chosen, selection, compatibility) = if let Some(manifest) = source.manifest() {
            // Selection is precomputed once in slot order, never by worker arrival.
            let seed = options.replay_seed.unwrap_or_else(generated_seed);
            let selection = SelectionPlan::build(
                manifest,
                repeat,
                config.concurrency as u64,
                seed,
                options.track,
                super::MAX_EXECUTIONS as usize,
            )
            .map_err(input_error)?;
            (
                selection.tracks.clone(),
                Some(selection),
                Some(manifest.compatibility.clone()),
            )
        } else {
            if options.replay_seed.is_some() {
                return Err(Error::Input(
                    "--seed requires a ZIP bundle; legacy JSONL has no completeness catalogue"
                        .into(),
                ));
            }
            let Some(Selector::Id(id)) = options.track else {
                return Err(Error::Input("legacy JSONL supports numeric --track only; statistic observations are unknown".into()));
            };
            if source.legacy_bytes(id).is_none() {
                return Err(Error::Input(format!(
                    "legacy cassette track {id} does not exist"
                )));
            }
            (
                vec![id; (repeat * config.concurrency as u64) as usize],
                None,
                None,
            )
        };
        Ok(Some(Self {
            source,
            path: std::path::absolute(path).map_err(crate::io_error)?,
            chosen,
            selection,
            compatibility,
        }))
    }

    pub fn input_cost(&self) -> Result<super::InputCost, Error> {
        let ids: std::collections::BTreeSet<_> = self.chosen.iter().copied().collect();
        let mut bytes = 0u64;
        for id in &ids {
            let size = if let Some(manifest) = self.source.manifest() {
                manifest.track(*id).map_err(input_error)?.bytes
            } else {
                self.source
                    .legacy_bytes(*id)
                    .ok_or_else(|| Error::Input("selected legacy metadata is missing".into()))?
            };
            bytes = bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= Limits::default().max_total_bytes)
                .ok_or_else(|| {
                    Error::Input("selected replay inputs exceed aggregate byte budget".into())
                })?;
        }
        let (files, directory_bytes, manifest_bytes, observed_bytes) = self.source.retained_cost();
        Ok(super::InputCost {
            inputs: ids.len(),
            bytes,
            files,
            directory_bytes,
            manifest_bytes,
            observed_bytes,
        })
    }

    pub fn observe(self) -> Result<Prepared, Error> {
        Ok(Prepared {
            source: self.source.observe().map_err(input_error)?,
            path: self.path,
            chosen: self.chosen,
            selection: self.selection,
        })
    }
}

impl Prepared {
    pub fn inputs(&self, slots: &mut [Slot]) -> Result<Vec<ReplayInput>, Error> {
        if slots.len() != self.chosen.len() {
            return Err(Error::Input(
                "replay selection population differs from execution plan".into(),
            ));
        }
        let mut seen = BTreeMap::new();
        let mut inputs = Vec::new();
        let mut total = 0u64;
        for (slot, id) in slots.iter_mut().zip(&self.chosen) {
            let index = if let Some(index) = seen.get(id) {
                *index
            } else {
                let (path, bytes, sha256, complete, local_tools, deterministic_manifest) =
                    if let Some(manifest) = self.source.manifest() {
                        let track = manifest.track(*id).map_err(input_error)?;
                        (
                            self.path.clone(),
                            track.bytes,
                            track.sha256.clone(),
                            Some(track.complete),
                            track
                                .artifacts
                                .iter()
                                .any(|artifact| artifact.kind == "local-tools")
                                .then(|| PathBuf::from(format!("inputs/local-tools-{id}.jsonl"))),
                            track
                                .artifacts
                                .iter()
                                .any(|artifact| artifact.kind == "determinism-manifest")
                                .then(|| {
                                    PathBuf::from(format!("inputs/determinism-manifest-{id}.json"))
                                }),
                        )
                    } else {
                        let track = self
                            .source
                            .legacy_tracks()
                            .iter()
                            .find(|track| track.id == *id)
                            .ok_or_else(|| {
                                Error::Input("selected legacy track disappeared".into())
                            })?;
                        (
                            track.path.clone(),
                            track.bytes,
                            track.sha256.clone(),
                            None,
                            None,
                            None,
                        )
                    };
                total = total
                    .checked_add(bytes)
                    .filter(|bytes| *bytes <= Limits::default().max_total_bytes)
                    .ok_or_else(|| {
                        Error::Input("selected replay inputs exceed aggregate byte budget".into())
                    })?;
                let index = inputs.len();
                inputs.push(ReplayInput {
                    complete,
                    track: Some(*id),
                    source: path,
                    private: PathBuf::from(format!("inputs/track-{id}.jsonl")),
                    local_tools,
                    deterministic_manifest,
                    bytes,
                    sha256,
                });
                seen.insert(*id, index);
                index
            };
            slot.replay_input = Some(index);
            slot.replay_track = Some(*id);
        }
        Ok(inputs)
    }

    pub fn stage(&mut self, root: &Path, inputs: &[ReplayInput]) -> Result<(), Error> {
        private_directory(&root.join("inputs"), false)?;
        for input in inputs {
            // One bounded payload at a time, and only unique selected IDs.
            let bytes = self
                .source
                .read_track(input.track.expect("selected input identity"))
                .map_err(input_error)?;
            if bytes.len() as u64 != input.bytes || crate::cassette::digest(&bytes) != input.sha256
            {
                return Err(Error::Input(
                    "selected cassette changed after planning; no agent was started".into(),
                ));
            }
            publish_bytes(&root.join(&input.private), &bytes)?;
            if let Some(local_tools) = &input.local_tools {
                let bytes = self
                    .source
                    .read_artifact(input.track.expect("selected bundle track"), "local-tools")
                    .map_err(input_error)?;
                publish_bytes(&root.join(local_tools), &bytes)?;
            }
            if let Some(manifest) = &input.deterministic_manifest {
                let bytes = self
                    .source
                    .read_artifact(
                        input.track.expect("selected bundle track"),
                        "determinism-manifest",
                    )
                    .map_err(input_error)?;
                publish_bytes(&root.join(manifest), &bytes)?;
            }
        }
        Ok(())
    }

    pub fn batch_choices(&self, plan: &Plan, batch: u64, concurrency: usize) -> BatchChoices {
        let start = (batch as usize - 1) * concurrency;
        let tracks: Vec<_> = plan.slots[start..start + concurrency]
            .iter()
            .map(|slot| slot.replay_track.expect("planned replay slot"))
            .collect();
        let statistics = tracks
            .iter()
            .map(|id| {
                self.source
                    .manifest()
                    .and_then(|manifest| manifest.track(*id).ok())
                    .map(|track| track.statistics.clone())
            })
            .collect();
        BatchChoices {
            seed: self.selection.as_ref().map(|selection| selection.seed),
            algorithm: self
                .selection
                .as_ref()
                .map(|selection| selection.algorithm.clone()),
            tracks,
            statistics,
        }
    }
}

/// Each batch has its own replica aliases; each proxy still owns its own cursor.
pub(super) fn stage_batch(
    root: &Path,
    plan: &Plan,
    batch: u64,
    concurrency: usize,
) -> Result<String, Error> {
    stage_selected(
        root,
        plan,
        batch,
        &(1..=concurrency).collect::<Vec<_>>(),
        &root.join(format!("batch-{batch}")),
    )
}

pub(super) fn saved_batch_choices(plan: &Plan, batch: u64) -> Option<BatchChoices> {
    let start = (batch as usize - 1) * plan.concurrency;
    let slots = &plan.slots[start..start + plan.concurrency];
    let tracks = slots
        .iter()
        .map(|slot| slot.replay_track)
        .collect::<Option<Vec<_>>>()?;
    Some(BatchChoices {
        seed: plan
            .replay_selection
            .as_ref()
            .map(|selection| selection.seed),
        algorithm: plan
            .replay_selection
            .as_ref()
            .map(|selection| selection.algorithm.clone()),
        tracks,
        // Saved plans retain selected track identities but not a source handle
        // or a copied manifest. Statistics remain unavailable until a fresh
        // replay source is inspected.
        statistics: vec![None; plan.concurrency],
    })
}

/// Derive aliases only from saved private inputs and original replica IDs.
/// This route never opens the public source or samples a replacement track.
pub(super) fn stage_selected(
    root: &Path,
    plan: &Plan,
    batch: u64,
    members: &[usize],
    destination: &Path,
) -> Result<String, Error> {
    let concurrency = plan.concurrency;
    let base = destination.join("replay.jsonl");
    let start = (batch as usize - 1) * concurrency;
    for slot in plan.slots[start..start + concurrency]
        .iter()
        .filter(|slot| members.contains(&slot.replica))
    {
        let input = &plan.replay_inputs[slot.replay_input.expect("planned replay input")];
        let destination = if concurrency == 1 {
            base.clone()
        } else {
            crate::benchmark_paths::replica_path(&base, slot.replica)
        };
        fs::hard_link(root.join(&input.private), &destination).map_err(crate::io_error)?;
        stage_local_tools(root, input, &destination, slot.replica, concurrency)?;
        stage_determinism_manifest(root, input, &destination, slot.replica, concurrency)?;
    }
    fs::File::open(base.parent().expect("batch parent"))
        .and_then(|file| file.sync_all())
        .map_err(crate::io_error)?;
    path_text(&base)
}

fn stage_determinism_manifest(
    root: &Path,
    input: &ReplayInput,
    destination: &Path,
    replica: usize,
    concurrency: usize,
) -> Result<(), Error> {
    let Some(manifest) = &input.deterministic_manifest else {
        return Ok(());
    };
    let destination = destination
        .parent()
        .expect("replay destination has parent")
        .join(if concurrency == 1 {
            "determinism-manifest.json".into()
        } else {
            format!("determinism-manifest-{replica}.json")
        });
    fs::hard_link(root.join(manifest), destination).map_err(crate::io_error)
}

fn stage_local_tools(
    root: &Path,
    input: &ReplayInput,
    destination: &Path,
    replica: usize,
    concurrency: usize,
) -> Result<(), Error> {
    let Some(local_tools) = &input.local_tools else {
        return Ok(());
    };
    let destination = destination
        .parent()
        .expect("replay destination has parent")
        .join(if concurrency == 1 {
            "local-tools.jsonl".into()
        } else {
            format!("local-tools-{replica}.jsonl")
        });
    fs::hard_link(root.join(local_tools), destination).map_err(crate::io_error)
}

fn generated_seed() -> u64 {
    // Reproducibility, not a security token: disclose and persist this seed.
    let identity = crate::workloads::TaskIdentity::local("replay-selection-seed", None);
    let hash = Sha256::digest(identity.attempt_id.as_bytes());
    u64::from_le_bytes(hash[..8].try_into().expect("SHA-256 prefix"))
}

/// Explicit selection permits incomplete traces but must disclose their status
/// before any material/client admission. Unknown legacy status stays unknown.
pub(super) fn disclose_incomplete<'a>(
    inputs: impl IntoIterator<Item = &'a ReplayInput>,
    json: bool,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let ids: Vec<_> = inputs
        .into_iter()
        .filter(|input| input.complete == Some(false))
        .filter_map(|input| input.track)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    if json {
        serde_json::to_writer(
            &mut *output,
            &serde_json::json!({
                "type":"replay-warning", "code":"incomplete-track", "track_ids":ids,
                "complete":false, "strict_matching":true, "full_consumption_required":true
            }),
        )
        .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(crate::io_error)?;
    } else {
        writeln!(output, "Warning: selected track(s) {} are incomplete; strict matching and full consumption are still required.", ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")).map_err(crate::io_error)?;
    }
    Ok(())
}

fn input_error(error: crate::cassette::Error) -> Error {
    Error::Input(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_local_tool_payload_is_linked_only_into_its_replica_batch() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-local-tools-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(
            root.join("inputs/local-tools-7.jsonl"),
            b"{\"sequence\":1}\n",
        )
        .unwrap();
        let input = ReplayInput {
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: Some(PathBuf::from("inputs/local-tools-7.jsonl")),
            deterministic_manifest: None,
            bytes: 2,
            sha256: "a".repeat(64),
        };
        let destination = root.join("batch/replay.jsonl");
        stage_local_tools(&root, &input, &destination, 3, 4).unwrap();
        assert_eq!(
            fs::read(root.join("batch/local-tools-3.jsonl")).unwrap(),
            b"{\"sequence\":1}\n"
        );
        assert!(!root.join("batch/local-tools-1.jsonl").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn singleton_local_tool_payload_uses_the_provider_basename() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-local-tools-one-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(root.join("inputs/local-tools-1.jsonl"), b"{}\n").unwrap();
        let input = ReplayInput {
            complete: Some(true),
            track: Some(1),
            source: root.join("source"),
            private: PathBuf::from("inputs/track-1.jsonl"),
            local_tools: Some(PathBuf::from("inputs/local-tools-1.jsonl")),
            deterministic_manifest: None,
            bytes: 3,
            sha256: crate::cassette::digest(b"{}\n"),
        };
        stage_local_tools(&root, &input, &root.join("batch/replay.jsonl"), 1, 1).unwrap();
        assert_eq!(
            fs::read(root.join("batch/local-tools.jsonl")).unwrap(),
            b"{}\n"
        );
        assert!(!root.join("batch/local-tools-1.jsonl").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deterministic_manifest_is_linked_beside_its_selected_replay_track() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-determinism-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(root.join("inputs/determinism-manifest-7.json"), b"{}\n").unwrap();
        let input = ReplayInput {
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: None,
            deterministic_manifest: Some(PathBuf::from("inputs/determinism-manifest-7.json")),
            bytes: 2,
            sha256: "a".repeat(64),
        };
        stage_determinism_manifest(&root, &input, &root.join("batch/replay.jsonl"), 3, 4).unwrap();
        assert_eq!(
            fs::read(root.join("batch/determinism-manifest-3.json")).unwrap(),
            b"{}\n"
        );
        assert!(!root.join("batch/determinism-manifest-1.json").exists());
        let _ = fs::remove_dir_all(root);
    }
}
