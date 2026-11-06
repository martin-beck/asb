//! Explicit, sweep-owned preparation of sampled replay candidates.
use super::{Options, SweepFile, lock, plan};
use crate::{Error, cassette, project};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadySelection {
    schema_version: u8,
    sweep_sha256: String,
    max_steps: Option<u64>,
    winners: Vec<Winner>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Winner {
    workload: String,
    agent: String,
    provider: String,
    model: String,
    trial: String,
    replay_config: String,
    cassette: String,
    cassette_sha256: String,
}

/// A read-only, verified mapping for the ordinary sweep replay engine.
pub(crate) struct Resolved {
    pub(crate) parent: PathBuf,
    pub(crate) references: Vec<String>,
    pub(crate) cassette_root: PathBuf,
    pub(crate) max_steps: Option<u64>,
}

impl ReadySelection {
    fn validate(&self) -> Result<(), Error> {
        use std::collections::BTreeSet;
        if self.schema_version != 1
            || !cassette::valid_digest(&self.sweep_sha256)
            || self.max_steps == Some(0)
            || self.winners.is_empty()
            || self.winners.len() > super::MAX_CONFIGS
        {
            return Err(Error::Input(
                "invalid sampled replay selection header".into(),
            ));
        }
        let mut identities = BTreeSet::new();
        for winner in &self.winners {
            let names = [
                &winner.workload,
                &winner.agent,
                &winner.provider,
                &winner.model,
                &winner.trial,
            ];
            if names.iter().any(|name| {
                name.is_empty() || name.len() > 4096 || name.chars().any(char::is_control)
            }) || !identities.insert((
                Path::new(&winner.replay_config).components().next(),
                &winner.trial,
            )) || !cassette::valid_digest(&winner.cassette_sha256)
                || !safe_reference(&winner.replay_config, "toml")
                || !safe_reference(&winner.cassette, "zip")
            {
                return Err(Error::Input(
                    "invalid or duplicate sampled replay winner".into(),
                ));
            }
        }
        Ok(())
    }
}

fn safe_reference(value: &str, extension: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path.extension().is_some_and(|value| value == extension)
}

fn read_ready(path: &Path) -> Result<ReadySelection, Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(crate::io_error)?;
    if !metadata.file_type().is_file() {
        return Err(Error::Input(
            "sampled replay selection must be a regular file".into(),
        ));
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = open.open(path).map_err(crate::io_error)?;
    if !file.metadata().map_err(crate::io_error)?.is_file() {
        return Err(Error::Input(
            "sampled replay selection must be a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(crate::io_error)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::Input(
            "sampled replay selection exceeds 16 MiB".into(),
        ));
    }
    let ready: ReadySelection = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Input("invalid sampled replay selection".into()))?;
    ready.validate()?;
    Ok(ready)
}

struct Inputs {
    root: PathBuf,
    sweep_digest: String,
    groups: Vec<(
        String,
        crate::benchmark::BenchmarkConfig,
        cassette::TrialOrder,
    )>,
}

fn preparation_root(path: &Path, options: Options<'_>) -> Result<PathBuf, Error> {
    let anchor = project::configuration_anchor(path, options.invocation_dir)?;
    let owner = anchor.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("sweep");
    Ok(owner.join(format!(".{stem}-sample-preparation")))
}

pub(crate) fn has_preparation(path: &Path, options: Options<'_>) -> Result<bool, Error> {
    let root = preparation_root(path, options)?;
    match std::fs::symlink_metadata(root) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(crate::io_error(error)),
    }
}

fn inputs(path: &Path, sweep: &SweepFile, options: Options<'_>) -> Result<Inputs, Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = options.invocation_dir.unwrap_or_else(|| Path::new("."));
    let plan = plan::build_at(parent, sweep, base)?;
    let templates =
        plan::sampled_templates(sweep, plan.entries.iter().map(|entry| &entry.configuration))?;
    let samples: BTreeMap<_, _> = sweep
        .sampled_trials
        .iter()
        .map(|sample| (sample.workload.as_str(), sample))
        .collect();
    let mut groups = Vec::new();
    for (key, config) in templates {
        let sample = samples[config.workload.as_str()];
        let seed = u64::from_str_radix(&sample.seed, 16)
            .map_err(|_| Error::Input("invalid sample seed".into()))?;
        let order = cassette::TrialOrder::new(&sample.candidates, sample.count, seed)
            .map_err(|error| Error::Input(error.to_string()))?;
        let name = cassette::digest(
            &serde_json::to_vec(&key)
                .map_err(|_| Error::Input("cannot encode sampled combination".into()))?,
        );
        groups.push((name, config, order));
    }
    for (index, entry) in plan.entries.iter().enumerate() {
        let config = &entry.configuration;
        if samples.contains_key(config.workload.as_str()) {
            continue;
        }
        // Fixed entries retain their exact trial, including an absent trial.
        // Their distinct input index prevents unrelated configurations sharing a ledger.
        let name = cassette::digest(format!("fixed-entry-v1:{index}").as_bytes());
        let id = config
            .trial
            .clone()
            .unwrap_or_else(|| config.workload.clone());
        let order = cassette::TrialOrder::new(&[id], 1, 0)
            .map_err(|error| Error::Input(error.to_string()))?;
        groups.push((name, config.clone(), order));
    }
    let mut digest = Sha256::new();
    digest.update(b"asb-sampled-sweep-input-v1\0");
    let manifest_text = super::read_manifest_text(path)?;
    digest.update((manifest_text.len() as u64).to_le_bytes());
    digest.update(manifest_text.as_bytes());
    let entries = serde_json::to_vec(&plan.entries)
        .map_err(|_| Error::Input("cannot encode sampled sweep inputs".into()))?;
    digest.update((entries.len() as u64).to_le_bytes());
    digest.update(entries);
    Ok(Inputs {
        root: preparation_root(path, options)?,
        sweep_digest: format!("{:x}", digest.finalize()),
        groups,
    })
}

/// Resolve previously published winners without starting a provider or
/// creating any files. The caller holds the sweep output lock.
pub(crate) fn resolve(
    path: &Path,
    sweep: &SweepFile,
    options: Options<'_>,
) -> Result<Resolved, Error> {
    let Inputs {
        root,
        sweep_digest,
        groups,
    } = inputs(path, sweep, options)?;
    let ready = read_ready(&root.join("ready.json")).map_err(|error| {
        Error::Input(format!(
            "sampled replay is not ready: {error}; run `asb sweep {} --prepare-replay`",
            path.display()
        ))
    })?;
    if ready.sweep_sha256 != sweep_digest
        || options
            .max_steps
            .is_some_and(|value| Some(value) != ready.max_steps)
    {
        return Err(Error::Input("sampled replay inputs or max-steps changed; preserve the prepared evidence and create a new sweep".into()));
    }
    let mut expected = Vec::new();
    for (group_name, config, order) in groups {
        let group = root.join(&group_name);
        let template = group.join("template.toml");
        let expected_template = toml::to_string_pretty(&config)
            .map_err(|error| Error::Input(format!("cannot encode sampled template: {error}")))?;
        if crate::read_config(&template)? != expected_template {
            return Err(Error::Input(
                "sampled benchmark template changed; preserve existing evidence".into(),
            ));
        }
        let state = cassette::load_sampled_group(
            &group.join("ledger.json"),
            &group.join("candidates"),
            &template,
            &sweep_digest,
            &order,
            ready.max_steps,
            options.invocation_dir.unwrap_or_else(|| Path::new(".")),
        )
        .map_err(|error| {
            Error::Input(format!("sampled replay evidence is unavailable: {error}"))
        })?;
        append_winners(&mut expected, &group_name, &config, &state);
    }
    if expected != ready.winners {
        return Err(Error::Input(
            "sampled replay selection does not match the completed group ledgers".into(),
        ));
    }
    verify_cassette_aliases(&root, &ready)?;
    Ok(Resolved {
        parent: root.canonicalize().map_err(crate::io_error)?,
        references: ready
            .winners
            .iter()
            .map(|winner| winner.replay_config.clone())
            .collect(),
        cassette_root: root
            .join("cassettes")
            .canonicalize()
            .map_err(crate::io_error)?,
        max_steps: ready.max_steps,
    })
}

fn append_winners(
    winners: &mut Vec<Winner>,
    group_name: &str,
    config: &crate::benchmark::BenchmarkConfig,
    state: &cassette::BoundTrialPreparation,
) {
    for (trial, digest) in state.preparation().successful() {
        let candidate = format!(
            "{group_name}/candidates/candidate-{}",
            cassette::digest(trial.as_bytes())
        );
        winners.push(Winner {
            workload: config.workload.clone(),
            agent: config.agent.clone(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            trial: trial.to_owned(),
            replay_config: format!("{candidate}/replay.toml"),
            cassette: format!("{candidate}/recording.asb.zip"),
            cassette_sha256: digest.to_owned(),
        });
    }
}

pub(super) fn prepare(
    path: &Path,
    sweep: &SweepFile,
    options: Options<'_>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    if options.repeat.is_some() || options.jobs != 1 || options.max_active_attempts.is_some() {
        return Err(Error::Input(
            "sample preparation currently requires the default repeat and sequential scheduling"
                .into(),
        ));
    }
    let base = options.invocation_dir.unwrap_or_else(|| Path::new("."));
    let anchor = project::configuration_anchor(path, options.invocation_dir)?;
    let owner = anchor.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("sweep");
    let _lock = lock::OutputLock::acquire_for_source(owner, stem, path)?;
    let Inputs {
        root,
        sweep_digest,
        groups,
    } = inputs(path, sweep, options)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&root)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(Error::Input(
            "sample preparation path must be a real directory".into(),
        ));
    }
    std::fs::create_dir_all(&root).map_err(crate::io_error)?;
    let mut winners = Vec::new();
    for (group_name, config, order) in groups {
        let group = root.join(&group_name);
        std::fs::create_dir_all(&group).map_err(crate::io_error)?;
        let template = group.join("template.toml");
        let text = toml::to_string_pretty(&config)
            .map_err(|error| Error::Input(format!("cannot encode sampled template: {error}")))?;
        let created = crate::config::matrix::scalar_if_absent(&template, text.as_bytes())?;
        if !created && crate::read_config(&template)? != text {
            return Err(Error::Input("sampled benchmark template changed; preserve existing evidence and create a new sweep".into()));
        }
        let state = cassette::prepare_sampled_group(
            &group.join("ledger.json"),
            &group.join("candidates"),
            &template,
            base,
            &sweep_digest,
            order,
            options.max_steps,
        )
        .map_err(|error| Error::Run(format!("sampled workload `{}`: {error}", config.workload)))?;
        let count = state.preparation().successful().len();
        append_winners(&mut winners, &group_name, &config, &state);
        if options.json {
            writeln!(output, "{}", serde_json::json!({"type":"sample-prepared","workload":config.workload,"agent":config.agent,"provider":config.provider,"model":config.model,"qualified":count})).map_err(crate::io_error)?;
        } else {
            writeln!(
                output,
                "Prepared {count} replayable trials for {} / {} / {} / {}",
                config.workload, config.agent, config.provider, config.model
            )
            .map_err(crate::io_error)?;
        }
    }
    let ready = ReadySelection {
        schema_version: 1,
        sweep_sha256: sweep_digest,
        max_steps: options.max_steps,
        winners,
    };
    ready.validate()?;
    publish_cassette_aliases(&root, &ready)?;
    let mut bytes = BoundedReady(Vec::new());
    serde_json::to_writer(&mut bytes, &ready).map_err(|_| {
        Error::Input("sampled replay selection exceeds 16 MiB or cannot be encoded".into())
    })?;
    let ready_path = root.join("ready.json");
    let _created = crate::config::matrix::scalar_if_absent(&ready_path, &bytes.0)?;
    if read_ready(&ready_path).ok().as_ref() != Some(&ready) {
        return Err(Error::Input("existing sampled replay selection differs; preserve its evidence and create a new sweep".into()));
    }
    let resolved = resolve(path, sweep, options)?;
    if resolved.references.len() != ready.winners.len()
        || resolved.max_steps != ready.max_steps
        || !resolved.parent.is_dir()
        || !resolved.cassette_root.is_dir()
    {
        return Err(Error::Run(
            "published sampled replay selection could not be verified".into(),
        ));
    }
    if options.json {
        writeln!(
            output,
            "{}",
            serde_json::json!({"type":"sample-preparation-complete", "sweep":path})
        )
        .map_err(crate::io_error)?;
    } else {
        writeln!(
            output,
            "Replay preparation complete. Run: asb sweep {} --replay",
            path.display()
        )
        .map_err(crate::io_error)?;
    }
    Ok(())
}

fn verify_cassette_aliases(root: &Path, ready: &ReadySelection) -> Result<(), Error> {
    for (index, winner) in ready.winners.iter().enumerate() {
        let name = super::execution::result_name(index, Path::new(&winner.replay_config));
        let alias = root.join("cassettes").join(name).join("recording.asb.zip");
        let metadata = std::fs::symlink_metadata(&alias).map_err(crate::io_error)?;
        if !metadata.file_type().is_file() {
            return Err(Error::Input(
                "sample cassette alias must be a regular file".into(),
            ));
        }
        cassette::Archive::open_verified(
            &alias,
            metadata.len(),
            &winner.cassette_sha256,
            cassette::Limits::default(),
        )
        .map_err(|error| {
            Error::Input(format!(
                "sample cassette alias verification failed: {error}"
            ))
        })?;
    }
    Ok(())
}

fn publish_cassette_aliases(root: &Path, ready: &ReadySelection) -> Result<(), Error> {
    let destination = root.join("cassettes");
    std::fs::create_dir_all(&destination).map_err(crate::io_error)?;
    let metadata = std::fs::symlink_metadata(&destination).map_err(crate::io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Input(
            "sample cassette destination must be a real directory".into(),
        ));
    }
    for (index, winner) in ready.winners.iter().enumerate() {
        let source = root.join(&winner.cassette);
        let metadata = std::fs::symlink_metadata(&source).map_err(crate::io_error)?;
        if !metadata.file_type().is_file() {
            return Err(Error::Input(
                "sample cassette must be a regular file".into(),
            ));
        }
        cassette::Archive::open_verified(
            &source,
            metadata.len(),
            &winner.cassette_sha256,
            cassette::Limits::default(),
        )
        .map_err(|error| Error::Input(format!("sample cassette verification failed: {error}")))?;
        let name = super::execution::result_name(index, Path::new(&winner.replay_config));
        let directory = destination.join(name);
        std::fs::create_dir_all(&directory).map_err(crate::io_error)?;
        let metadata = std::fs::symlink_metadata(&directory).map_err(crate::io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Input(
                "sample cassette alias must be a real directory".into(),
            ));
        }
        let alias = directory.join("recording.asb.zip");
        match std::fs::hard_link(&source, &alias) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => {
                return Err(Error::Io(format!(
                    "cannot retain sampled cassette alias: {error}"
                )));
            }
        }
        let metadata = std::fs::symlink_metadata(&alias).map_err(crate::io_error)?;
        if !metadata.file_type().is_file() {
            return Err(Error::Input(
                "sample cassette alias must be a regular file".into(),
            ));
        }
        cassette::Archive::open_verified(
            &alias,
            metadata.len(),
            &winner.cassette_sha256,
            cassette::Limits::default(),
        )
        .map_err(|error| {
            Error::Input(format!(
                "sample cassette alias verification failed: {error}"
            ))
        })?;
        #[cfg(unix)]
        std::fs::File::open(&directory)
            .and_then(|directory| directory.sync_all())
            .map_err(crate::io_error)?;
    }
    #[cfg(unix)]
    std::fs::File::open(&destination)
        .and_then(|directory| directory.sync_all())
        .map_err(crate::io_error)?;
    Ok(())
}

struct BoundedReady(Vec<u8>);
impl Write for BoundedReady {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > (16 * 1024 * 1024usize).saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "sampled replay selection exceeds 16 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> ReadySelection {
        ReadySelection {
            schema_version: 1,
            sweep_sha256: "a".repeat(64),
            max_steps: None,
            winners: vec![Winner {
                workload: "suite".into(),
                agent: "agent".into(),
                provider: "provider".into(),
                model: "model".into(),
                trial: "trial".into(),
                replay_config: "group/candidate/replay.toml".into(),
                cassette: "group/candidate/recording.zip".into(),
                cassette_sha256: "b".repeat(64),
            }],
        }
    }

    #[test]
    fn winner_selection_rejects_unsafe_paths_and_invalid_or_duplicate_evidence() {
        let mut value = ready();
        assert!(value.validate().is_ok());
        value.winners[0].cassette = "../outside.zip".into();
        assert!(value.validate().is_err());
        value = ready();
        value.winners[0].cassette_sha256 = "not-a-digest".into();
        assert!(value.validate().is_err());
        value = ready();
        let mut distinct: Winner =
            serde_json::from_value(serde_json::to_value(&value.winners[0]).unwrap()).unwrap();
        distinct.replay_config = "another-group/candidate/replay.toml".into();
        distinct.cassette = "another-group/candidate/recording.zip".into();
        value.winners.push(distinct);
        assert!(value.validate().is_ok());
        value = ready();
        let duplicate =
            serde_json::from_value(serde_json::to_value(&value.winners[0]).unwrap()).unwrap();
        value.winners.push(duplicate);
        assert!(value.validate().is_err());
    }
}
