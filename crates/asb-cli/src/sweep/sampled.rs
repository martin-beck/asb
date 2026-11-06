//! Explicit, sweep-owned preparation of sampled replay candidates.
use super::{Options, SweepFile, lock, plan};
use crate::{Error, cassette, project};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

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
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = options.invocation_dir.unwrap_or_else(|| Path::new("."));
    let anchor = project::configuration_anchor(path, options.invocation_dir)?;
    let owner = anchor.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("sweep");
    let _lock = lock::OutputLock::acquire_for_source(owner, stem, path)?;
    let plan = plan::build_at(parent, sweep, base)?;
    let templates =
        plan::sampled_templates(sweep, plan.entries.iter().map(|entry| &entry.configuration))?;
    // All entries must participate in preparation. Do not silently omit a
    // fixed workload while its cassette mapping is not yet supported.
    if plan.entries.iter().any(|entry| {
        !sweep
            .sampled_trials
            .iter()
            .any(|sample| sample.workload == entry.configuration.workload)
    }) {
        return Err(Error::Input("preparing a mix of sampled and fixed workloads is not yet supported; no candidate was started".into()));
    }
    let mut digest = Sha256::new();
    digest.update(b"asb-sampled-sweep-input-v1\0");
    digest.update(super::read_manifest_text(path)?.as_bytes());
    let entries = serde_json::to_vec(&plan.entries)
        .map_err(|_| Error::Input("cannot encode sampled sweep inputs".into()))?;
    digest.update(entries);
    let sweep_digest = format!("{:x}", digest.finalize());
    let root = owner.join(format!(".{stem}-sample-preparation"));
    if let Ok(metadata) = std::fs::symlink_metadata(&root)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(Error::Input(
            "sample preparation path must be a real directory".into(),
        ));
    }
    std::fs::create_dir_all(&root).map_err(crate::io_error)?;
    let samples: BTreeMap<_, _> = sweep
        .sampled_trials
        .iter()
        .map(|sample| (sample.workload.as_str(), sample))
        .collect();
    for (key, config) in templates {
        let sample = samples[config.workload.as_str()];
        let seed = u64::from_str_radix(&sample.seed, 16)
            .map_err(|_| Error::Input("invalid sample seed".into()))?;
        let order = cassette::TrialOrder::new(&sample.candidates, sample.count, seed)
            .map_err(|error| Error::Input(error.to_string()))?;
        let key_bytes = serde_json::to_vec(&key)
            .map_err(|_| Error::Input("cannot encode sampled combination".into()))?;
        let group = root.join(cassette::digest(&key_bytes));
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
    Err(Error::Run("sampled candidates were verified, but publication of the replay selection is not yet available; existing evidence was retained".into()))
}
