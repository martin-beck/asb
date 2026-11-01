//! Run-owned recording coordination; providers and proxies see only JSONL.
mod raw;
pub(crate) mod union;
use super::{RunResult, io_error, replica};
use crate::cassette::{
    Artifact, ArtifactInput, Compatibility, Limits, Manifest, NativeInput, NativeToolTransport,
    NativeTrack, Publication, RecordingJournal, Statistics, Track, TrackEvidence,
};
use crate::{Error, proxy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

fn capture_matches_track(track: &Track, capture: &proxy::capture::CaptureEvidence) -> bool {
    capture.bytes == track.bytes
        && track
            .recorder
            .as_deref()
            .unwrap_or(crate::proxy::legacy_recorder_name())
            == capture.adapter
        && capture.sha256 == track.sha256
        && capture.complete == track.complete
}

/// Adapter-owned sidecars share a deliberately small aggregate budget.  Relay
/// bundles are deduplicated protocol records, not a second copy of an agent
/// session or its process output.
const MAX_SEALED_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

/// Historical producer association plus immutable track proof. Temporary raw
/// files are not completion closure inputs after their exact bytes are committed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Evidence {
    pub campaign_root: PathBuf,
    pub producer: PathBuf,
    pub attempt_id: String,
    pub track: TrackEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<union::Binding>,
}

impl Evidence {
    pub(crate) fn association_path(&self) -> Result<PathBuf, Error> {
        association_path(&self.track)
    }

    pub fn validate_summary(
        &self,
        summary: &serde_json::Value,
        captures: &[proxy::capture::CaptureEvidence],
    ) -> Result<(), Error> {
        self.validate_summary_metadata(summary, captures)?;
        self.track
            .verify(Limits::default())
            .map_err(recording_error)
    }

    /// Validate the association and capture declarations without reading payloads.
    /// Reports must subsequently verify the collected tracks as one batch.
    pub fn validate_summary_metadata(
        &self,
        summary: &serde_json::Value,
        captures: &[proxy::capture::CaptureEvidence],
    ) -> Result<(), Error> {
        self.validate_report_metadata(summary)?;
        let track = &self.track.track;
        let matching: Vec<_> = captures
            .iter()
            .filter(|capture| capture.output == self.producer)
            .collect();
        if matching.len() > 1
            || matching
                .iter()
                .any(|capture| !capture_matches_track(track, capture))
            || (track.complete && matching.is_empty())
            || track.statistics.requests
                != matching.first().map(|capture| capture.inference_requests)
        {
            return Err(Error::Input(
                "recorded track differs from its capture evidence".into(),
            ));
        }
        Ok(())
    }

    /// A report-only summary lacks completion capture receipts. Validate its
    /// identity and immutable producer association before grouped payload proof.
    /// This does not establish cleanup or authorize execution continuation.
    pub fn validate_report_metadata(&self, summary: &serde_json::Value) -> Result<(), Error> {
        self.track
            .validate(Limits::default())
            .map_err(recording_error)?;
        let track = &self.track.track;
        let execution = &summary["execution"];
        if !self.campaign_root.is_absolute()
            || execution["campaign"].as_str().is_none()
            || self
                .campaign_root
                .file_name()
                .and_then(|name| name.to_str())
                != execution["campaign"].as_str()
            || (self.generation.is_none() && execution["campaign"] != self.track.recording_id)
            || execution["batch"].as_u64() != Some(track.batch)
            || execution["replica"].as_u64() != Some(track.replica)
            || execution["slot"].as_u64() != Some(track.id)
            || summary["task"]["attempt_id"] != self.attempt_id
            || summary["agent"] != self.track.compatibility.agent
            || summary["provider"] != self.track.compatibility.provider
            || summary["model"] != self.track.compatibility.model
            || summary["workload"] != self.track.compatibility.workload
            || summary["task"]["task_id"] != self.track.compatibility.task
            || track.process_success != summary["process_success"].as_bool()
            || track.workload_success != summary["workload_success"].as_bool()
            || track.statistics.tokens != summary["token_usage"]["total_tokens"].as_u64()
            || track.statistics.duration_ms != summary["elapsed_ms"].as_u64()
            || track.statistics.steps != summary["steps"].as_u64()
            || self.producer
                != producer(
                    &self.campaign_root,
                    track.batch,
                    track.replica as usize,
                    self.track.concurrency as usize,
                )
        {
            return Err(Error::Input(
                "recorded track does not belong to this execution/producer".into(),
            ));
        }
        // Bind the association itself, not just a self-consistent claim copied
        // into a different summary. This immutable receipt precedes completion.
        let association = read_association(&self.track)?;
        let digest = crate::cassette::digest(
            &serde_json::to_vec(self).map_err(|error| Error::Io(error.to_string()))?,
        );
        if association.schema_version != 1 || association.evidence_sha256 != digest {
            return Err(Error::Input(
                "recording producer/attempt association changed".into(),
            ));
        }
        Ok(())
    }
}

struct State {
    capacity_verified: bool,
    journal: Option<RecordingJournal>,
    compatibility: Option<Compatibility>,
    committed: std::collections::BTreeSet<u64>,
    association_bytes: u64,
    raw_identities: std::collections::BTreeMap<u64, fs::Metadata>,
}

pub(crate) struct Recorder {
    destination: PathBuf,
    root: PathBuf,
    campaign: String,
    repeat: u64,
    concurrency: usize,
    failed: AtomicBool,
    state: Mutex<State>,
    planned: Option<Planned>,
    union: Option<union::Descriptor>,
    binding: Option<union::Binding>,
    current_relative: PathBuf,
}

struct Planned {
    configuration: crate::benchmark::BenchmarkConfig,
    selection: crate::workloads::TaskSelection,
    reservation: crate::cassette::RecordingReservation,
}

/// Read-only destination planning. This observes names, never reserves them.
/// Activation and the journal must independently recheck no-overwrite conditions.
pub(super) fn inspect_destination(destination: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(destination).map_err(io_error)?;
    let name = absolute
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Input("recording cassette needs a UTF-8 filename".into()))?;
    let requested_parent = absolute
        .parent()
        .ok_or_else(|| Error::Input("recording cassette needs a parent".into()))?;
    // Resolve existing directory/symlink prefixes without creating missing
    // parents. Parent components after a missing name behave as create_dir_all
    // followed by canonicalization did, but planning has no filesystem writes.
    let mut parent = PathBuf::new();
    for component in requested_parent.components() {
        match component {
            std::path::Component::CurDir => (),
            std::path::Component::ParentDir => {
                parent.pop();
            }
            component => {
                parent.push(component.as_os_str());
                match fs::symlink_metadata(&parent) {
                    Ok(_) => {
                        parent = parent.canonicalize().map_err(io_error)?;
                        if !parent.is_dir() {
                            return Err(Error::Input(
                                "recording cassette parent must be a directory".into(),
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(io_error(error)),
                }
            }
        }
    }
    let absolute = parent.join(name);
    for path in [&absolute, &parent.join(format!(".{name}.partial"))] {
        match fs::symlink_metadata(path) {
            Ok(_) => return Err(Error::Input("recording destination or partial artifact already exists; choose a new cassette or explicitly recover it".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(absolute)
}

/// Refuse final/partial destinations before workload/tool preparation. The
/// journal independently owns the authoritative no-replace publication boundary.
pub(super) fn preflight(destination: &Path) -> Result<PathBuf, Error> {
    let expected = inspect_destination(destination)?;
    fs::create_dir_all(expected.parent().expect("inspected parent")).map_err(io_error)?;
    let observed = inspect_destination(&expected)?;
    if expected != observed {
        return Err(Error::Input(
            "recording cassette parent changed during activation".into(),
        ));
    }
    Ok(observed)
}

impl Recorder {
    pub fn new(
        destination: PathBuf,
        root: PathBuf,
        campaign: String,
        repeat: u64,
        concurrency: usize,
        plan: Option<&super::campaign::Plan>,
    ) -> Self {
        Self {
            destination,
            root,
            campaign,
            repeat,
            concurrency,
            failed: AtomicBool::new(false),
            state: Mutex::new(State {
                capacity_verified: false,
                journal: None,
                compatibility: None,
                committed: Default::default(),
                association_bytes: 0,
                raw_identities: Default::default(),
            }),
            planned: plan.and_then(|plan| {
                plan.recording_selection
                    .as_ref()
                    .zip(plan.recording_reservation.as_ref())
                    .map(|(selection, reservation)| Planned {
                        configuration: plan.configuration.clone(),
                        selection: selection.clone(),
                        reservation: reservation.clone(),
                    })
            }),
            union: None,
            binding: None,
            current_relative: PathBuf::from("recording-current.json"),
        }
    }

    /// Construct only while the parent holds the admitted generation's owners.
    pub(crate) fn for_union(
        root: PathBuf,
        plan: &super::campaign::Plan,
        descriptor: union::Descriptor,
        reference: super::campaign::generation::StartReference,
        compatibility: Option<Compatibility>,
        current_relative: PathBuf,
        staging: union::Staging,
    ) -> Result<Self, Error> {
        let journal = match staging {
            union::Staging::Fresh => {
                descriptor.stage(compatibility.as_ref(), plan.concurrency, plan.repeat)?
            }
            union::Staging::RecoverUnused => {
                descriptor.recover_unused(compatibility.as_ref(), plan.concurrency)?
            }
        };
        let binding = union::Binding {
            reference,
            origins_sha256: descriptor.origins.commitment().map_err(recording_error)?,
        };
        let imported = descriptor
            .origins
            .imports
            .iter()
            .map(|track| track.slot)
            .collect();
        let planned = Planned {
            configuration: plan.configuration.clone(),
            selection: plan.recording_selection.clone().ok_or_else(|| {
                Error::Input("union lost its reserved recording selection".into())
            })?,
            reservation: descriptor.reservation.clone(),
        };
        Ok(Self {
            destination: descriptor.destination.clone(),
            root,
            campaign: plan.campaign.clone(),
            repeat: plan.repeat,
            concurrency: plan.concurrency,
            failed: AtomicBool::new(false),
            state: Mutex::new(State {
                capacity_verified: false,
                journal,
                compatibility,
                committed: imported,
                association_bytes: 0,
                raw_identities: Default::default(),
            }),
            planned: Some(planned),
            union: Some(descriptor),
            binding: Some(binding),
            current_relative,
        })
    }

    /// Called outside the material gate's mutex after all material agrees. These
    /// are recording-slot intents, not claims that a client process was launched.
    pub fn prepare(
        &self,
        compatibility: &Compatibility,
        batch: u64,
        members: &[usize],
    ) -> Result<(), Error> {
        if members.is_empty()
            || members.windows(2).any(|pair| pair[0] >= pair[1])
            || members
                .iter()
                .any(|member| !(1..=self.concurrency).contains(member))
        {
            return Err(Error::Input("invalid recording dispatch membership".into()));
        }
        association_budget(self.repeat, self.concurrency)?;
        self.operation(|state| {
            if let Some(planned) = &self.planned {
                compatibility.validate().map_err(recording_error)?;
                let selected = &planned.configuration;
                if compatibility.agent != selected.agent
                    || compatibility.provider != selected.provider
                    || compatibility.model != selected.model
                    || compatibility.workload != planned.selection.workload
                    || compatibility.task != planned.selection.task_id
                {
                    return Err(Error::Input(
                        "prepared recording identity differs from reserved selection".into(),
                    ));
                }
                if !state.capacity_verified {
                    #[cfg(test)]
                    tests::CAPACITY_CHECKS.set(tests::CAPACITY_CHECKS.get() + 1);
                    let shape = crate::cassette::CompatibilityShape {
                        agent: &compatibility.agent,
                        provider: &compatibility.provider,
                        model: &compatibility.model,
                        workload: &compatibility.workload,
                        task: &compatibility.task,
                    };
                    let actual = if let Some(union) = &self.union {
                        crate::cassette::RecordingReservation::for_origin_plan(
                            shape,
                            &union.physical_id,
                            &self.destination,
                            self.concurrency as u64,
                            &union.origins,
                            Limits::default(),
                        )
                    } else {
                        crate::cassette::RecordingReservation::for_plan(
                            shape,
                            &self.campaign,
                            &self.destination,
                            self.repeat,
                            self.concurrency as u64,
                            Limits::default(),
                        )
                    }
                    .map_err(recording_error)?;
                    if actual != planned.reservation {
                        return Err(Error::Input(
                            "prepared recording metadata exceeds or differs from reserved capacity"
                                .into(),
                        ));
                    }
                    state.capacity_verified = true;
                }
            }
            if let Some(identity) = &state.compatibility {
                identity
                    .require_match(compatibility)
                    .map_err(recording_error)?;
            } else {
                state.journal = Some(
                    RecordingJournal::create(
                        &self.destination,
                        Manifest {
                            schema_version: crate::cassette::SCHEMA_VERSION,
                            recording_id: self
                                .union
                                .as_ref()
                                .map(|union| union.physical_id.clone())
                                .unwrap_or_else(|| self.campaign.clone()),
                            compatibility: compatibility.clone(),
                            concurrency: self.concurrency as u64,
                            tracks: Vec::new(),
                            provenance: self.union.as_ref().map(|union| union.origins.clone()),
                        },
                        self.repeat,
                        Limits::default(),
                    )
                    .map_err(recording_error)?,
                );
            }
            state.compatibility = Some(compatibility.clone());
            let journal = state.journal.as_mut().expect("initialized journal");
            for replica in members {
                journal
                    .begin(batch, *replica as u64)
                    .map_err(recording_error)?;
            }
            Ok(())
        })
    }

    /// Single writer held only during bounded payload commit/verification.
    pub(super) fn commit(
        &self,
        batch: u64,
        replica: replica::Identity,
        result: &RunResult<'_>,
        proxy: &proxy::Evidence,
    ) -> Result<Evidence, Error> {
        self.operation(|state| {
            let output = producer(&self.root, batch, replica.index, self.concurrency);
            let mut matching = proxy
                .captures
                .iter()
                .filter(|capture| capture.output == output);
            let capture = matching.next();
            if matching.next().is_some() {
                return Err(Error::Io(
                    "multiple recorder captures claim one producer".into(),
                ));
            }
            if let Some(capture) = capture {
                capture.validate()?;
            }
            let mut file = open_payload(&output)?;
            let raw_identity = file
                .as_ref()
                .map(File::metadata)
                .transpose()
                .map_err(io_error)?;
            if file.is_none() && capture.is_some() {
                return Err(Error::Io("captured recording payload is missing".into()));
            }
            let (bytes, sha256) = if let Some(capture) = capture {
                (capture.bytes, capture.sha256.clone())
            } else if let Some(file) = &mut file {
                payload_identity(file)?
            } else {
                (0, crate::cassette::digest(b""))
            };
            // The producer is adapter-owned, but the recorder owns the only
            // conversion from private bytes to cassette-native metadata. This
            // prevents adapters from minting an unchecked track declaration.
            let mut native_tool = result
                .native_tool_sidecar
                .as_deref()
                .map(seal_native_tool_sidecar)
                .transpose()?;
            let track = Track {
                recorder: capture.map(|capture| capture.adapter.clone()),
                id: (batch - 1) * self.concurrency as u64 + replica.index as u64,
                batch,
                replica: replica.index as u64,
                complete: capture.is_some_and(|capture| capture.complete),
                process_success: Some(result.process_success),
                workload_success: result.workload_success,
                bytes,
                sha256,
                statistics: Statistics {
                    tokens: result.token_usage.total_tokens,
                    duration_ms: u64::try_from(result.elapsed_ms).ok(),
                    requests: capture.map(|capture| capture.inference_requests),
                    steps: result.steps,
                },
                // `commit_with_native` is the sole owner that derives the
                // immutable native catalogue entry from the supplied opaque
                // sidecar.  Pre-populating this field would let callers
                // bypass that custody boundary and is rejected by the
                // journal before it stages the payload.
                native: None,
                artifacts: Vec::new(),
            };
            let journal = state
                .journal
                .as_mut()
                .ok_or_else(|| Error::Io("recording has no verified material identity".into()))?;
            let mut local_tools = result
                .local_tool_transcript
                .as_deref()
                .map(crate::agents::tool_replay::canonicalize)
                .transpose()?
                .map(std::io::Cursor::new);
            let local_tools_artifact = local_tools.as_ref().map(|bytes| Artifact {
                kind: "local-tools".into(),
                bytes: bytes.get_ref().len() as u64,
                sha256: crate::cassette::digest(bytes.get_ref()),
            });
            let mut deterministic_manifest = result
                .deterministic_manifest
                .as_deref()
                .map(std::io::Cursor::new);
            let deterministic_manifest_artifact =
                deterministic_manifest.as_ref().map(|bytes| Artifact {
                    kind: "determinism-manifest".into(),
                    bytes: bytes.get_ref().len() as u64,
                    sha256: crate::cassette::digest(bytes.get_ref()),
                });
            let mut native_trace = result
                .native_trace
                .as_deref()
                .map(crate::agents::native_trace::canonicalize)
                .transpose()?
                .map(std::io::Cursor::new);
            let native_trace_artifact = native_trace.as_ref().map(|bytes| Artifact {
                kind: "native-trace".into(),
                bytes: bytes.get_ref().len() as u64,
                sha256: crate::cassette::digest(bytes.get_ref()),
            });
            let mut artifact_inputs = Vec::new();
            if let (Some(artifact), Some(source)) =
                (local_tools_artifact.clone(), local_tools.as_mut())
            {
                artifact_inputs.push(ArtifactInput { artifact, source });
            }
            if let (Some(artifact), Some(source)) = (
                deterministic_manifest_artifact.clone(),
                deterministic_manifest.as_mut(),
            ) {
                artifact_inputs.push(ArtifactInput { artifact, source });
            }
            if let (Some(artifact), Some(source)) =
                (native_trace_artifact.clone(), native_trace.as_mut())
            {
                artifact_inputs.push(ArtifactInput { artifact, source });
            }
            validate_sealed_artifact_budget(&result.sealed_artifacts)?;
            let mut sealed_artifacts: Vec<_> = result
                .sealed_artifacts
                .iter()
                .map(|sidecar| {
                    (
                        Artifact {
                            kind: sidecar.kind.clone(),
                            bytes: sidecar.bytes.len() as u64,
                            sha256: crate::cassette::digest(&sidecar.bytes),
                        },
                        std::io::Cursor::new(sidecar.bytes.as_slice()),
                    )
                })
                .collect();
            for (artifact, source) in &mut sealed_artifacts {
                artifact_inputs.push(ArtifactInput {
                    artifact: artifact.clone(),
                    source,
                });
            }
            if let Some(file) = &mut file {
                file.seek(SeekFrom::Start(0)).map_err(io_error)?;
                journal
                    .commit_with_native(
                        track.clone(),
                        file,
                        artifact_inputs,
                        native_tool.as_mut().map(|(native, source)| NativeInput {
                            native: native.clone(),
                            source,
                        }),
                    )
                    .map_err(recording_error)?;
            } else {
                journal
                    .commit_with_native(
                        track.clone(),
                        &mut std::io::empty(),
                        artifact_inputs,
                        native_tool.as_mut().map(|(native, source)| NativeInput {
                            native: native.clone(),
                            source,
                        }),
                    )
                    .map_err(recording_error)?;
            }
            // commit_with_native constructs the immutable auxiliary catalogue
            // itself; obtain that committed track before asking for evidence.
            // Supplying an already-populated list would be rejected by the
            // journal, while retaining the pre-commit track would fail its
            // strict immutable-metadata comparison.
            let committed_track = journal
                .snapshot()
                .map_err(recording_error)?
                .tracks
                .into_iter()
                .find(|candidate| candidate.id == track.id)
                .ok_or_else(|| Error::Io("recording lost its committed track".into()))?;
            let evidence = Evidence {
                campaign_root: self.root.clone(),
                producer: output,
                attempt_id: result.task.attempt_id.clone(),
                track: journal
                    .track_evidence(&committed_track)
                    .map_err(recording_error)?,
                generation: self.binding.clone(),
            };
            let evidence_bytes =
                serde_json::to_vec(&evidence).map_err(|error| Error::Io(error.to_string()))?;
            if evidence_bytes.len() > 64 * 1024 {
                return Err(Error::Io("recording evidence exceeds bound".into()));
            }
            let bytes = serde_json::to_vec(&Association {
                schema_version: 1,
                evidence_sha256: crate::cassette::digest(&evidence_bytes),
            })
            .map_err(|error| Error::Io(error.to_string()))?;
            let total = state
                .association_bytes
                .checked_add(bytes.len() as u64)
                .filter(|total| {
                    *total <= Limits::default().max_manifest_bytes
                        && bytes.len() <= ASSOCIATION_BOUND as usize
                })
                .ok_or_else(|| Error::Io("recording association metadata exceeds bounds".into()))?;
            super::repeated::publish_bytes(&association_path(&evidence.track)?, &bytes)?;
            state.association_bytes = total;
            if let Some(identity) = raw_identity {
                state.raw_identities.insert(track.id, identity);
            }
            state.committed.insert(track.id);
            Ok(evidence)
        })
    }

    /// Called only after every admitted producer was joined. Unknown producer
    /// cleanup retains raw files even when publication itself can be confirmed.
    pub fn finalize(&self, producers_quiescent: bool) -> Result<Option<Publication>, Error> {
        self.operation(|state| {
            let Some(journal) = state.journal.as_mut() else { return Ok(None) };
            // Preserve partial producer bytes from setup/launch failure without
            // fabricating an execution outcome or positive completeness.
            for mut track in journal.snapshot().map_err(recording_error)?.tracks {
                if state.committed.contains(&track.id) { continue }
                let output = producer(&self.root, track.batch, track.replica as usize, self.concurrency);
                let mut file = open_payload(&output)?;
                let raw_identity=file.as_ref().map(File::metadata).transpose().map_err(io_error)?;
                let id=track.id;
                let (bytes, sha256) = if let Some(file) = &mut file { payload_identity(file)? } else { (0, crate::cassette::digest(b"")) };
                track.bytes=bytes; track.sha256=sha256; track.complete=false;
                if let Some(file) = &mut file { file.seek(SeekFrom::Start(0)).map_err(io_error)?; journal.commit(track,file).map_err(recording_error)?; }
                else { journal.commit(track,&mut std::io::empty()).map_err(recording_error)?; }
                if let Some(identity)=raw_identity { state.raw_identities.insert(id,identity); }
            }
            let mut published = journal.finalize().map_err(recording_error)?;
            if producers_quiescent {
                let mut cleanup_failures=0;
                for track in &published.manifest.tracks {
                    if self.union.as_ref().is_some_and(|union| union.origins.imported(track.id).is_some()) { continue; }
                    let output = producer(&self.root,track.batch,track.replica as usize,self.concurrency);
                    match cleanup_raw(&output,track,state.raw_identities.get(&track.id)) {
                        Ok(()) => (),
                        Err(error) => {
                            published.cleanup_complete=false;
                            cleanup_failures+=1;
                            if cleanup_failures<=16 {
                                let _ = error;
                                published.warnings.push(raw_cleanup_warning(track.id));
                            }
                        }
                    }
                }
                if cleanup_failures>0 { published.warnings.push(format!("published cassette is usable; {cleanup_failures} raw capture cleanups were incomplete or durability-unconfirmed; inspect artifacts (at most 16 details retained)")); }
            } else { published.cleanup_complete=false; published.warnings.push("published cassette is usable; raw captures retained because producer cleanup is unconfirmed".into()); }
            Ok(Some(published))
        })
    }

    pub(crate) fn publish_union(&self, publication: &Publication) -> Result<(), Error> {
        let descriptor = self
            .union
            .as_ref()
            .ok_or_else(|| Error::Input("recorder has no union descriptor".into()))?;
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::Input("recorder has no union generation binding".into()))?;
        union::publish(
            &self.root,
            descriptor,
            binding,
            publication,
            &self.current_relative,
        )
    }

    fn operation<T>(
        &self,
        action: impl FnOnce(&mut State) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if self.failed.load(Ordering::Acquire) {
            return Err(Error::Io(
                "recording state requires explicit recovery; partial evidence retained".into(),
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Io("recording writer state was interrupted".into()))?;
        if self.failed.load(Ordering::Acquire) {
            return Err(Error::Io(
                "recording state requires explicit recovery; partial evidence retained".into(),
            ));
        }
        let result = action(&mut state);
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
}

/// Validate a complete adapter-owned sidecar before the journal sees it.
///
/// This boundary intentionally retains its payload only in the cursor handed
/// to the journal. Result JSON and durable evidence receive just the sealed
/// `NativeTrack` metadata.
fn seal_native_tool_sidecar(bytes: &[u8]) -> Result<(NativeTrack, std::io::Cursor<&[u8]>), Error> {
    let native = NativeTrack {
        complete: true,
        events: bytes.iter().filter(|byte| **byte == b'\n').count() as u64,
        bytes: bytes.len() as u64,
        sha256: crate::cassette::digest(bytes),
    };
    NativeToolTransport::from_sidecar(&native, bytes)
        .map_err(|_| Error::Run("adapter native tool sidecar is invalid or incomplete".into()))?;
    Ok((native, std::io::Cursor::new(bytes)))
}

fn validate_sealed_artifact_budget(
    artifacts: &[crate::agents::SealedArtifact],
) -> Result<(), Error> {
    let bytes = artifacts.iter().try_fold(0usize, |total, artifact| {
        total.checked_add(artifact.bytes.len())
    });
    if bytes.is_none_or(|bytes| bytes > MAX_SEALED_ARTIFACT_BYTES) {
        return Err(Error::Input(format!(
            "adapter sealed artifacts exceed the {} MiB aggregate bound",
            MAX_SEALED_ARTIFACT_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

pub(super) fn base(root: &Path, batch: u64) -> PathBuf {
    root.join(format!("batch-{batch}/capture.jsonl"))
}
pub(crate) fn producer(root: &Path, batch: u64, replica: usize, concurrency: usize) -> PathBuf {
    let base = base(root, batch);
    if concurrency == 1 {
        base
    } else {
        crate::benchmark_paths::replica_path(&base, replica)
    }
}
const ASSOCIATION_BOUND: u64 = 128;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Association {
    schema_version: u32,
    evidence_sha256: String,
}

fn association_budget(repeat: u64, concurrency: usize) -> Result<u64, Error> {
    repeat
        .checked_mul(concurrency as u64)
        .filter(|count| {
            repeat > 0
                && (1..=64).contains(&concurrency)
                && *count <= Limits::default().max_tracks as u64
        })
        .and_then(|count| count.checked_mul(ASSOCIATION_BOUND))
        .filter(|bytes| *bytes <= Limits::default().max_manifest_bytes)
        .ok_or_else(|| {
            Error::Input("recording population exceeds association metadata budget".into())
        })
}
fn association_path(evidence: &TrackEvidence) -> Result<PathBuf, Error> {
    association_for(&evidence.destination, evidence.track.id)
}

pub(crate) fn association_for(destination: &Path, slot: u64) -> Result<PathBuf, Error> {
    if slot == 0 || slot > 65_533 {
        return Err(Error::Input("invalid recording association slot".into()));
    }
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Input("invalid recording destination".into()))?;
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Input("invalid recording parent".into()))?;
    Ok(parent.join(format!(".{name}.partial/association-{}.json", slot)))
}
fn read_association(evidence: &TrackEvidence) -> Result<Association, Error> {
    let path = association_path(evidence)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > ASSOCIATION_BOUND {
        return Err(Error::Input(
            "recording association is invalid or oversized".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(ASSOCIATION_BOUND + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > ASSOCIATION_BOUND as usize {
        return Err(Error::Input(
            "recording association grew beyond bound".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Input("recording association is malformed".into()))
}
#[cfg(test)]
std::thread_local! {
    static BEFORE_RAW_UNLINK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

fn cleanup_raw(output: &Path, track: &Track, original: Option<&fs::Metadata>) -> Result<(), Error> {
    raw::cleanup(output, track, original)
}

fn raw_cleanup_warning(track_id: u64) -> String {
    format!(
        "published cassette is usable; track {track_id} raw capture cleanup incomplete; private evidence retained"
    )
}

fn open_payload(output: &Path) -> Result<Option<File>, Error> {
    match fs::symlink_metadata(output) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
        Ok(metadata) if !metadata.is_file() => {
            return Err(Error::Io(
                "recording producer must be a regular non-symlink file".into(),
            ));
        }
        Ok(_) => (),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(output).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > Limits::default().max_track_bytes {
        return Err(Error::Io(
            "recording producer is invalid or oversized".into(),
        ));
    }
    Ok(Some(file))
}
fn payload_identity(file: &mut File) -> Result<(u64, String), Error> {
    let limit = Limits::default().max_track_bytes;
    if file.metadata().map_err(io_error)?.len() > limit {
        return Err(Error::Io("recording payload exceeds limit".into()));
    }
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .filter(|bytes| *bytes <= limit)
            .ok_or_else(|| Error::Io("recording payload grew beyond limit".into()))?;
        hash.update(&buffer[..count]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}
fn recording_error(error: crate::cassette::Error) -> Error {
    Error::Io(format!("recording: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    std::thread_local! {
        pub(super) static CAPACITY_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn capture_recorder_identity_is_bound_to_track_and_legacy_llmtape() {
        let mut track = Track {
            recorder: Some("other-recorder".into()),
            id: 1,
            batch: 1,
            replica: 1,
            complete: true,
            process_success: Some(true),
            workload_success: Some(true),
            bytes: 3,
            sha256: crate::cassette::digest(b"{}\n"),
            statistics: Statistics::default(),
            native: None,
            artifacts: Vec::new(),
        };
        let mut capture = proxy::capture::CaptureEvidence {
            adapter: "other-recorder".into(),
            output: PathBuf::from("/fixture"),
            complete: true,
            accepted: 1,
            completed: 1,
            failed: 0,
            recorded: 1,
            inference_requests: 1,
            bytes: track.bytes,
            sha256: track.sha256.clone(),
        };
        assert!(capture_matches_track(&track, &capture));
        capture.adapter = "llmtape".into();
        assert!(!capture_matches_track(&track, &capture));
        track.recorder = None;
        assert!(capture_matches_track(&track, &capture));
        capture.adapter = "other-recorder".into();
        assert!(!capture_matches_track(&track, &capture));
    }

    #[test]
    fn raw_cleanup_warning_has_only_a_stable_track_label() {
        let warning = raw_cleanup_warning(7);
        assert_eq!(
            warning,
            "published cassette is usable; track 7 raw capture cleanup incomplete; private evidence retained"
        );
        assert!(!warning.contains('/'));
    }

    #[test]
    fn reserved_capacity_is_checked_once_but_later_material_and_members_remain_strict() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/recording-capacity-cache-tests")
            .join(crate::workloads::TaskIdentity::local("capacity", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let configuration: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='fixture'",
        )
        .unwrap();
        let compatibility = Compatibility {
            agent: "fixture".into(),
            provider: "fixture".into(),
            model: "fixture".into(),
            workload: "fixture".into(),
            task: "fixture".into(),
            dataset_sha256: None,
            prompt_sha256: crate::cassette::digest(b"prompt"),
            material_sha256: crate::cassette::digest(b"material"),
            protocol_sha256: None,
        };
        for mutation in 0..4 {
            let destination = root.join(format!("{mutation}.zip"));
            let mut recorder = Recorder::new(
                destination.clone(),
                root.clone(),
                "campaign".into(),
                3,
                1,
                None,
            );
            let reservation = crate::cassette::RecordingReservation::for_plan(
                crate::cassette::CompatibilityShape {
                    agent: &compatibility.agent,
                    provider: &compatibility.provider,
                    model: &compatibility.model,
                    workload: &compatibility.workload,
                    task: &compatibility.task,
                },
                "campaign",
                &destination,
                3,
                1,
                Limits::default(),
            )
            .unwrap();
            recorder.planned = Some(Planned {
                configuration: configuration.clone(),
                selection: crate::workloads::TaskSelection {
                    module: None,
                    workload: "fixture".into(),
                    task_id: "fixture".into(),
                    dataset: None,
                    evaluation_kind: "fixture".into(),
                    population_size: None,
                },
                reservation,
            });
            CAPACITY_CHECKS.set(0);
            if mutation == 3 {
                recorder
                    .planned
                    .as_mut()
                    .unwrap()
                    .reservation
                    .reference_bytes += 1;
                assert!(recorder.prepare(&compatibility, 1, &[1]).is_err());
                assert!(!recorder.state.lock().unwrap().capacity_verified);
                assert!(recorder.failed.load(Ordering::Acquire));
            } else {
                recorder.prepare(&compatibility, 1, &[1]).unwrap();
                recorder.prepare(&compatibility, 2, &[1]).unwrap();
                assert!(recorder.state.lock().unwrap().capacity_verified);
                let mut changed = compatibility.clone();
                match mutation {
                    0 => changed.material_sha256 = crate::cassette::digest(b"different material"),
                    1 => changed.agent = "changed".into(),
                    _ => (),
                }
                let members = if mutation == 2 { &[1, 1][..] } else { &[1][..] };
                assert!(recorder.prepare(&changed, 3, members).is_err());
            }
            assert_eq!(CAPACITY_CHECKS.get(), 1);
            drop(recorder);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn raw_cleanup_preserves_foreign_entries_replaced_after_final_check() {
        struct Owned(PathBuf);
        impl Drop for Owned {
            fn drop(&mut self) {
                BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
                    hook.take();
                });
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let mut failures = Vec::new();
        for mode in ["regular", "symlink", "parent"] {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/raw-cleanup-race-fixtures")
                .join(crate::workloads::TaskIdentity::local("raw-cleanup-race", None).attempt_id);
            fs::create_dir_all(root.join("producer")).unwrap();
            let _owned = Owned(root.clone());
            let output = root.join("producer/capture.jsonl");
            fs::write(&output, b"{}\n").unwrap();
            let original = fs::metadata(&output).unwrap();
            let track = Track {
                recorder: None,
                id: 1,
                batch: 1,
                replica: 1,
                complete: false,
                process_success: None,
                workload_success: None,
                bytes: 3,
                sha256: crate::cassette::digest(b"{}\n"),
                statistics: Statistics::default(),
                native: None,
                artifacts: Vec::new(),
            };
            let replacement_root = root.clone();
            BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    let output = replacement_root.join("producer/capture.jsonl");
                    if mode == "parent" {
                        fs::rename(
                            replacement_root.join("producer"),
                            replacement_root.join("saved-producer"),
                        )
                        .unwrap();
                        fs::create_dir(replacement_root.join("producer")).unwrap();
                    } else {
                        fs::rename(&output, replacement_root.join("saved-original")).unwrap();
                    }
                    if mode == "symlink" {
                        let target = replacement_root.join("foreign-target");
                        fs::write(&target, b"foreign content").unwrap();
                        std::os::unix::fs::symlink(target, &output).unwrap();
                    } else {
                        fs::write(&output, b"foreign content").unwrap();
                    }
                }));
            });
            let cleaned = cleanup_raw(&output, &track, Some(&original));
            let Ok(metadata) = fs::symlink_metadata(&output) else {
                failures.push(format!(
                    "{mode}: removed a foreign pathname after validating the original: {cleaned:?}"
                ));
                continue;
            };
            if fs::read(&output).unwrap() != b"foreign content" {
                failures.push(format!("{mode}: changed foreign bytes"));
            }
            if mode == "symlink" && !metadata.file_type().is_symlink() {
                failures.push("symlink: replaced the foreign link".into());
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn destination_inspection_is_read_only_and_activation_rechecks_foreign_outputs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/recording-destination-fixtures")
            .join(crate::workloads::TaskIdentity::local("destination", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let destination = root.join("missing/nested/recording.asb.zip");
        assert_eq!(inspect_destination(&destination).unwrap(), destination);
        assert!(!root.join("missing").exists());
        assert_eq!(preflight(&destination).unwrap(), destination);
        assert!(destination.parent().unwrap().is_dir());
        fs::write(&destination, b"foreign").unwrap();
        assert!(preflight(&destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"foreign");
        let other = root.join("other.asb.zip");
        inspect_destination(&other).unwrap();
        let partial = root.join(".other.asb.zip.partial");
        fs::create_dir(&partial).unwrap();
        assert!(preflight(&other).is_err());
        assert!(partial.is_dir());
        let non_directory = root.join("file");
        fs::write(&non_directory, b"sentinel").unwrap();
        assert!(inspect_destination(&non_directory.join("new.zip")).is_err());
        assert_eq!(fs::read(&non_directory).unwrap(), b"sentinel");
        let with_parent = root.join("never-created/../next/archive.zip");
        assert_eq!(
            inspect_destination(&with_parent).unwrap(),
            root.join("next/archive.zip")
        );
        assert!(!root.join("never-created").exists());
        assert!(!root.join("next").exists());
        #[cfg(unix)]
        {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(root.join("missing"), &alias).unwrap();
            let candidate = alias.join("future/next.zip");
            assert_eq!(
                inspect_destination(&candidate).unwrap(),
                root.join("missing/future/next.zip")
            );
            assert!(!root.join("missing/future").exists());
            let nested_alias = root.join("nested-alias");
            std::os::unix::fs::symlink(root.join("missing/nested"), &nested_alias).unwrap();
            assert_eq!(
                inspect_destination(&nested_alias.join("../sibling/next.zip")).unwrap(),
                root.join("missing/sibling/next.zip")
            );
            assert!(!root.join("missing/sibling").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compact_association_budget_fits_maximum_population_before_launch() {
        assert_eq!(
            association_budget(65_533, 1).unwrap(),
            65_533 * ASSOCIATION_BOUND
        );
        for (repeat, concurrency) in [(0, 1), (1, 0), (1, 65), (1024, 64), (u64::MAX, 2)] {
            assert!(association_budget(repeat, concurrency).is_err());
        }
        let association = Association {
            schema_version: 1,
            evidence_sha256: "f".repeat(64),
        };
        assert!(serde_json::to_vec(&association).unwrap().len() as u64 <= ASSOCIATION_BOUND);
    }

    #[cfg(unix)]
    #[test]
    fn raw_cleanup_retains_mutation_replacement_links_and_directories() {
        struct Owned(PathBuf);
        impl Drop for Owned {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
        for mode in ["unchanged", "mutated", "replaced", "symlink", "directory"] {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/raw-cleanup-fixtures")
                .join(crate::workloads::TaskIdentity::local("raw-cleanup", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            let _owned = Owned(root.clone());
            let output = root.join("capture.jsonl");
            fs::write(&output, b"{}\n").unwrap();
            let original = fs::metadata(&output).unwrap();
            let track = Track {
                recorder: None,
                id: 1,
                batch: 1,
                replica: 1,
                complete: false,
                process_success: None,
                workload_success: None,
                bytes: 3,
                sha256: crate::cassette::digest(b"{}\n"),
                statistics: Statistics::default(),
                native: None,
                artifacts: Vec::new(),
            };
            match mode {
                "mutated" => fs::write(&output, b"[]\n").unwrap(),
                "replaced" => {
                    fs::rename(&output, root.join("preserved-original")).unwrap();
                    fs::write(&output, b"{}\n").unwrap();
                }
                "symlink" => {
                    fs::rename(&output, root.join("sentinel")).unwrap();
                    std::os::unix::fs::symlink(root.join("sentinel"), &output).unwrap();
                }
                "directory" => {
                    fs::rename(&output, root.join("sentinel")).unwrap();
                    fs::create_dir(&output).unwrap();
                }
                _ => (),
            }
            let cleaned = cleanup_raw(&output, &track, Some(&original));
            if mode == "unchanged" {
                cleaned.unwrap();
                assert!(!output.exists());
            } else {
                assert!(cleaned.is_err(), "{mode}");
                assert!(fs::symlink_metadata(&output).is_ok());
            }
            if root.join("sentinel").exists() {
                assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"{}\n");
            }
        }
    }

    #[test]
    fn sealed_sidecars_have_a_small_aggregate_budget() {
        let within = crate::agents::SealedArtifact {
            kind: "mcp-relay-filesystem".into(),
            bytes: vec![0; MAX_SEALED_ARTIFACT_BYTES],
        };
        validate_sealed_artifact_budget(&[within]).expect("exact bound is allowed");

        let over = crate::agents::SealedArtifact {
            kind: "mcp-relay-filesystem".into(),
            bytes: vec![0; MAX_SEALED_ARTIFACT_BYTES + 1],
        };
        assert!(validate_sealed_artifact_budget(&[over]).is_err());
    }

    #[test]
    fn native_tool_sidecar_is_validated_before_private_track_publication() {
        let digest = "a".repeat(64);
        let request = b"{}";
        let lines = [
            format!(
                "{{\"event\":\"launch\",\"session\":\"launch-1\",\"program_sha256\":\"{digest}\"}}"
            ),
            "{\"event\":\"session_alias\",\"alias\":\"shell-1\",\"session\":\"launch-1\"}".into(),
            "{\"event\":\"poll\",\"alias\":\"shell-1\"}".into(),
            format!(
                "{{\"event\":\"write\",\"alias\":\"shell-1\",\"request_sha256\":\"{}\",\"request_b64\":\"e30=\"}}",
                crate::cassette::digest(request)
            ),
            format!(
                "{{\"event\":\"output\",\"alias\":\"shell-1\",\"response_sha256\":\"{}\",\"output_b64\":\"e30=\"}}",
                crate::cassette::digest(request)
            ),
            "{\"event\":\"exit\",\"alias\":\"shell-1\",\"success\":true}".into(),
            format!("{{\"event\":\"workspace_effect\",\"sha256\":\"{digest}\"}}"),
        ];
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let (native, source) = seal_native_tool_sidecar(&bytes).expect("valid sidecar");
        assert!(native.complete);
        assert_eq!(native.events, lines.len() as u64);
        assert_eq!(native.bytes, bytes.len() as u64);
        assert_eq!(source.into_inner(), bytes.as_slice());
        assert!(seal_native_tool_sidecar(b"invalid\n").is_err());
    }
}
