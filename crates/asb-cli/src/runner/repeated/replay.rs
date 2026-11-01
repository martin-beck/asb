//! Run-owned whole-track selection and private JSONL staging; no proxy changes.
use super::{Plan, ReplayInput, Slot, path_text, private_directory, publish_bytes};
use crate::cassette::{Compatibility, Limits, SelectionPlan, Selector, Source, SourceInspection};
use crate::runner::campaign::McpRelayInput;
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
                let (
                    path,
                    bytes,
                    sha256,
                    complete,
                    local_tools,
                    tool_protocol,
                    deterministic_manifest,
                    native_trace,
                    native_tool,
                    mcp_relays,
                    recorder,
                ) = if let Some(manifest) = self.source.manifest() {
                    let track = manifest.track(*id).map_err(input_error)?;
                    let mut mcp_relays = Vec::new();
                    for artifact in &track.artifacts {
                        let Some(server) = artifact.kind.strip_prefix("mcp-relay-") else {
                            continue;
                        };
                        if server.is_empty()
                            || mcp_relays
                                .iter()
                                .any(|relay: &McpRelayInput| relay.id == server)
                        {
                            return Err(Error::Input(
                                "selected cassette has an invalid MCP relay artifact identity"
                                    .into(),
                            ));
                        }
                        mcp_relays.push(McpRelayInput {
                            id: server.into(),
                            bundle: PathBuf::from(format!("inputs/mcp-relay-{server}-{id}.jsonl")),
                        });
                    }
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
                            .any(|artifact| artifact.kind == "tool-protocol")
                            .then(|| PathBuf::from(format!("inputs/tool-protocol-{id}.json"))),
                        track
                            .artifacts
                            .iter()
                            .any(|artifact| artifact.kind == "determinism-manifest")
                            .then(|| {
                                PathBuf::from(format!("inputs/determinism-manifest-{id}.json"))
                            }),
                        track
                            .artifacts
                            .iter()
                            .any(|artifact| artifact.kind == "native-trace")
                            .then(|| PathBuf::from(format!("inputs/native-trace-{id}.jsonl"))),
                        track
                            .native
                            .as_ref()
                            .map(|_| PathBuf::from(format!("inputs/native-tool-{id}.jsonl"))),
                        mcp_relays,
                        track.recorder.clone(),
                    )
                } else {
                    let track = self
                        .source
                        .legacy_tracks()
                        .iter()
                        .find(|track| track.id == *id)
                        .ok_or_else(|| Error::Input("selected legacy track disappeared".into()))?;
                    (
                        track.path.clone(),
                        track.bytes,
                        track.sha256.clone(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Vec::new(),
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
                    recorder,
                    complete,
                    track: Some(*id),
                    source: path,
                    private: PathBuf::from(format!("inputs/track-{id}.jsonl")),
                    local_tools,
                    tool_protocol,
                    deterministic_manifest,
                    native_trace,
                    native_tool,
                    #[cfg(not(test))]
                    native_tool_launch: None,
                    mcp_relays,
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

    pub fn stage(&mut self, root: &Path, inputs: &mut [ReplayInput]) -> Result<(), Error> {
        private_directory(&root.join("inputs"), false)?;
        for input in inputs {
            let expected_recorder = self
                .source
                .manifest()
                .map(|manifest| manifest.track(input.track.expect("selected input identity")))
                .transpose()
                .map_err(input_error)?
                .and_then(|track| track.recorder.as_deref());
            if input.recorder.as_deref() != expected_recorder {
                return Err(Error::Input(
                    "selected recording adapter changed after planning; no agent was started"
                        .into(),
                ));
            }
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
            if let Some(protocol) = &input.tool_protocol {
                let bytes = self
                    .source
                    .read_artifact(input.track.expect("selected bundle track"), "tool-protocol")
                    .map_err(input_error)?;
                publish_bytes(&root.join(protocol), &bytes)?;
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
            if let Some(native_trace) = &input.native_trace {
                let bytes = self
                    .source
                    .read_artifact(input.track.expect("selected bundle track"), "native-trace")
                    .map_err(input_error)?;
                publish_bytes(&root.join(native_trace), &bytes)?;
            }
            if let Some(native_tool) = &input.native_tool {
                let bytes = self
                    .source
                    .read_native(input.track.expect("selected bundle track"))
                    .map_err(input_error)?;
                // Validate before publication. Later consumers receive only
                // the payload-free launch descriptor.
                #[cfg(not(test))]
                let track = self
                    .source
                    .manifest()
                    .and_then(|manifest| {
                        manifest
                            .track(input.track.expect("selected bundle track"))
                            .ok()
                    })
                    .and_then(|track| track.native.clone())
                    .ok_or_else(|| {
                        Error::Input("selected native tool sidecar disappeared".into())
                    })?;
                #[cfg(not(test))]
                {
                    input.native_tool_launch = Some(
                        crate::cassette::NativeToolTransport::launch_descriptor(track, &bytes)
                            .map_err(input_error)?,
                    );
                }
                // The isolated workload runs outside the campaign root. Keep
                // the owner-only replay input pointed at the exact staged
                // sidecar, rather than the relative catalogue name used
                // before staging.
                let staged_native_tool = root.join(native_tool);
                publish_bytes(&staged_native_tool, &bytes)?;
                input.native_tool = Some(staged_native_tool);
            }
            for relay in &input.mcp_relays {
                let bytes = self
                    .source
                    .read_artifact(
                        input.track.expect("selected bundle track"),
                        &format!("mcp-relay-{}", relay.id),
                    )
                    .map_err(input_error)?;
                publish_bytes(&root.join(&relay.bundle), &bytes)?;
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
        stage_tool_protocol(root, input, &destination)?;
        stage_determinism_manifest(root, input, &destination, slot.replica, concurrency)?;
        stage_native_trace(root, input, &destination, slot.replica, concurrency)?;
        stage_mcp_relays(root, input, &destination)?;
    }
    fs::File::open(base.parent().expect("batch parent"))
        .and_then(|file| file.sync_all())
        .map_err(crate::io_error)?;
    path_text(&base)
}

fn stage_mcp_relays(root: &Path, input: &ReplayInput, destination: &Path) -> Result<(), Error> {
    for relay in &input.mcp_relays {
        let destination = replay_sidecar(destination, &format!("mcp-relay-{}", relay.id), "jsonl")?;
        fs::hard_link(root.join(&relay.bundle), destination).map_err(crate::io_error)?;
    }
    Ok(())
}

fn stage_native_trace(
    root: &Path,
    input: &ReplayInput,
    destination: &Path,
    _replica: usize,
    _concurrency: usize,
) -> Result<(), Error> {
    let Some(trace) = &input.native_trace else {
        return Ok(());
    };
    // A native trace is optional for compatibility with provider-only
    // recordings, but a selected trace must be a valid bounded index before
    // the replayed agent can start.  The linked sidecar remains byte-for-byte
    // identical to the sealed bundle artifact; validation never rewrites
    // recorded evidence.
    let bytes = fs::read(root.join(trace)).map_err(crate::io_error)?;
    crate::agents::native_trace::canonicalize(&bytes).map_err(|error| {
        Error::Input(format!(
            "selected native replay trace is invalid; record the benchmark again before replaying it ({error})"
        ))
    })?;
    let destination = replay_sidecar(destination, "native-trace", "jsonl")?;
    fs::hard_link(root.join(trace), destination).map_err(crate::io_error)
}

fn stage_determinism_manifest(
    root: &Path,
    input: &ReplayInput,
    destination: &Path,
    _replica: usize,
    _concurrency: usize,
) -> Result<(), Error> {
    let Some(manifest) = &input.deterministic_manifest else {
        return Ok(());
    };
    let destination = replay_sidecar(destination, "determinism-manifest", "json")?;
    fs::hard_link(root.join(manifest), destination).map_err(crate::io_error)
}

fn stage_local_tools(
    root: &Path,
    input: &ReplayInput,
    destination: &Path,
    _replica: usize,
    _concurrency: usize,
) -> Result<(), Error> {
    let Some(local_tools) = &input.local_tools else {
        return Ok(());
    };
    let destination = replay_sidecar(destination, "local-tools", "jsonl")?;
    fs::hard_link(root.join(local_tools), destination).map_err(crate::io_error)
}

fn stage_tool_protocol(root: &Path, input: &ReplayInput, destination: &Path) -> Result<(), Error> {
    let Some(protocol) = &input.tool_protocol else {
        return Ok(());
    };
    let destination = replay_sidecar(destination, "tool-protocol", "json")?;
    fs::hard_link(root.join(protocol), destination).map_err(crate::io_error)
}

/// Keep sidecar names coupled to the precise replay payload they describe.
///
/// In particular, concurrent replay payloads use the legacy
/// `replay-concurrency-N.jsonl` shape.  Adapters derive their sealed
/// transcripts and deterministic manifests from that exact name, so deriving
/// sidecars from a replica number would leave a valid recorded artifact
/// invisible at replay time.
fn replay_sidecar(destination: &Path, prefix: &str, extension: &str) -> Result<PathBuf, Error> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Input("replay destination has no valid filename".into()))?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .ok_or_else(|| Error::Input("replay destination has an invalid filename".into()))?;
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Input("replay destination has no parent directory".into()))?;
    Ok(parent.join(format!("{prefix}{suffix}.{extension}")))
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
            recorder: None,
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: Some(PathBuf::from("inputs/local-tools-7.jsonl")),
            tool_protocol: None,
            deterministic_manifest: None,
            native_trace: None,
            native_tool: None,
            mcp_relays: Vec::new(),
            bytes: 2,
            sha256: "a".repeat(64),
        };
        let destination = root.join("batch/replay-concurrency-3.jsonl");
        stage_local_tools(&root, &input, &destination, 3, 4).unwrap();
        assert_eq!(
            fs::read(root.join("batch/local-tools-concurrency-3.jsonl")).unwrap(),
            b"{\"sequence\":1}\n"
        );
        assert!(!root.join("batch/local-tools-concurrency-1.jsonl").exists());
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
            recorder: None,
            complete: Some(true),
            track: Some(1),
            source: root.join("source"),
            private: PathBuf::from("inputs/track-1.jsonl"),
            local_tools: Some(PathBuf::from("inputs/local-tools-1.jsonl")),
            tool_protocol: None,
            deterministic_manifest: None,
            native_trace: None,
            native_tool: None,
            mcp_relays: Vec::new(),
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
    fn tool_protocol_identity_is_linked_beside_its_selected_replay_track() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-tool-protocol-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(root.join("inputs/tool-protocol-1.json"), b"{}\n").unwrap();
        let input = ReplayInput {
            recorder: None,
            complete: Some(true),
            track: Some(1),
            source: root.join("source"),
            private: PathBuf::from("inputs/track-1.jsonl"),
            local_tools: None,
            tool_protocol: Some(PathBuf::from("inputs/tool-protocol-1.json")),
            deterministic_manifest: None,
            native_trace: None,
            native_tool: None,
            mcp_relays: Vec::new(),
            bytes: 3,
            sha256: crate::cassette::digest(b"{}\n"),
        };
        stage_tool_protocol(&root, &input, &root.join("batch/replay.jsonl")).unwrap();
        assert_eq!(
            fs::read(root.join("batch/tool-protocol.json")).unwrap(),
            b"{}\n"
        );
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
            recorder: None,
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: None,
            tool_protocol: None,
            deterministic_manifest: Some(PathBuf::from("inputs/determinism-manifest-7.json")),
            native_trace: None,
            native_tool: None,
            mcp_relays: Vec::new(),
            bytes: 2,
            sha256: "a".repeat(64),
        };
        stage_determinism_manifest(
            &root,
            &input,
            &root.join("batch/replay-concurrency-3.jsonl"),
            3,
            4,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("batch/determinism-manifest-concurrency-3.json")).unwrap(),
            b"{}\n"
        );
        assert!(
            !root
                .join("batch/determinism-manifest-concurrency-1.json")
                .exists()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn native_trace_is_linked_beside_its_selected_replay_track() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-native-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        let trace = crate::agents::native_trace::Transcript::default()
            .encode()
            .unwrap();
        fs::write(root.join("inputs/native-trace-7.jsonl"), &trace).unwrap();
        let input = ReplayInput {
            recorder: None,
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: None,
            tool_protocol: None,
            deterministic_manifest: None,
            native_trace: Some(PathBuf::from("inputs/native-trace-7.jsonl")),
            native_tool: None,
            mcp_relays: Vec::new(),
            bytes: 2,
            sha256: "a".repeat(64),
        };
        stage_native_trace(
            &root,
            &input,
            &root.join("batch/replay-concurrency-3.jsonl"),
            3,
            4,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("batch/native-trace-concurrency-3.jsonl")).unwrap(),
            trace
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_native_trace_stops_replay_before_linking_a_sidecar() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-malformed-native-trace-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(root.join("inputs/native-trace-7.jsonl"), b"{}\n").unwrap();
        let input = ReplayInput {
            recorder: None,
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: None,
            tool_protocol: None,
            deterministic_manifest: None,
            native_trace: Some(PathBuf::from("inputs/native-trace-7.jsonl")),
            native_tool: None,
            mcp_relays: Vec::new(),
            bytes: 2,
            sha256: "a".repeat(64),
        };
        let error = stage_native_trace(&root, &input, &root.join("batch/replay.jsonl"), 1, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("selected native replay trace is invalid"));
        assert!(!root.join("batch/native-trace.jsonl").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mcp_relay_bundles_stage_with_the_selected_replay_alias() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-mcp-relay-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::create_dir_all(root.join("batch")).unwrap();
        fs::write(
            root.join("inputs/mcp-relay-filesystem-7.jsonl"),
            b"sealed\n",
        )
        .unwrap();
        let input = ReplayInput {
            recorder: None,
            complete: Some(true),
            track: Some(7),
            source: PathBuf::from("sealed.asb.zip"),
            private: PathBuf::from("inputs/track-7.jsonl"),
            local_tools: None,
            tool_protocol: None,
            deterministic_manifest: None,
            native_trace: None,
            native_tool: None,
            mcp_relays: vec![McpRelayInput {
                id: "filesystem".into(),
                bundle: PathBuf::from("inputs/mcp-relay-filesystem-7.jsonl"),
            }],
            bytes: 2,
            sha256: "a".repeat(64),
        };
        stage_mcp_relays(
            &root,
            &input,
            &root.join("batch/replay-concurrency-2.jsonl"),
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("batch/mcp-relay-filesystem-concurrency-2.jsonl")).unwrap(),
            b"sealed\n"
        );
        let _ = fs::remove_dir_all(root);
    }
}
