//! One isolated configuration, independent of scheduling and workload identity.
use super::{
    Options, benchmark_stem, cassette_filename, checked_benchmark_reference, isolated_config, plan,
    reuse_replay_inputs, spool,
};
use crate::{Error, runner, tape};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Benchmark errors and inability to deliver output have different admission
/// policies. Do not recover this distinction from an error's display text.
pub(super) struct ConfigurationReport {
    pub stop_admission: bool,
    pub result: Result<(), Error>,
    pub output_error: Option<Error>,
    pub spool_created: bool,
}

pub(super) fn result_name(index: usize, reference: &Path) -> String {
    format!("{index:03}-{}", benchmark_stem(reference))
}

pub(super) fn replay_sources(plan: &plan::SweepPlan, root: &Path) -> Vec<PathBuf> {
    plan.entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            root.join(result_name(index, Path::new(&entry.reference)))
                .join("cassettes")
        })
        .collect()
}

pub(super) struct Execution<'a> {
    pub campaigns: Option<&'a super::campaigns::Pool>,
    pub source: &'a str,
    pub parent: &'a Path,
    pub plan: &'a plan::SweepPlan,
    pub results: &'a Path,
    pub backup: &'a Path,
    pub replay_inputs: Option<&'a Path>,
    pub restore_recordings: bool,
    pub options: Options<'a>,
    pub warnings: &'a Mutex<BTreeSet<String>>,
    pub checkpoint: Option<&'a super::checkpoint::Inputs>,
    pub reused: &'a super::resume::State,
}

impl Execution<'_> {
    pub(super) fn config_path(&self, index: usize) -> PathBuf {
        self.parent.join(&self.plan.entries[index].reference)
    }

    fn directory(&self, index: usize) -> PathBuf {
        self.results
            .join(result_name(index, &self.config_path(index)))
    }

    pub(super) fn output_path(&self, index: usize) -> PathBuf {
        // Keep temporary presentation bytes outside the immutable task result
        // tree. Evidence hashing must not omit similarly named grader files.
        self.results
            .join(format!(".terminal-output-{index:06}.tmp"))
    }

    pub(super) fn run(&self, index: usize) -> ConfigurationReport {
        let mut output_error = None;
        let mut prepared_output = false;
        let result = (|| {
            std::fs::create_dir_all(self.directory(index)).map_err(crate::io_error)?;
            if self.options.resume {
                let path = self.output_path(index);
                let history = self.results.join(".checkpoint/interrupted-terminal");
                if self.plan.execution.is_some() {
                    spool::preserve_campaign(&path, self.presentation_count(index), &history)?;
                } else {
                    spool::preserve(&path, self.presentation_count(index), &history)?;
                }
            }
            let mut spool = if self.plan.execution.is_some() {
                spool::Output::create_campaign(
                    &self.output_path(index),
                    self.presentation_count(index),
                )?
            } else {
                spool::Output::create(&self.output_path(index), self.presentation_count(index))?
            };
            let mut output = super::presentation::Presentation::new(&mut spool);
            prepared_output = true;
            let result = self.run_to(index, &mut output);
            let flush = output.flush().map_err(crate::io_error);
            output_error = output.error();
            match (result, flush) {
                (Err(error), Err(flush)) => {
                    Err(Error::Run(format!("{error}; output flush failed: {flush}")))
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
                _ => Ok(()),
            }
        })();
        // Failure before a spool could be opened also prevents reliable
        // presentation. No client has started on that path.
        if !prepared_output && let Err(error) = &result {
            output_error = Some(Error::Io(format!("cannot prepare sweep output: {error}")));
        }
        ConfigurationReport {
            stop_admission: self.stop_requested(index),
            result,
            output_error,
            spool_created: prepared_output,
        }
    }

    /// Sequential defaults retain immediate warnings and --show-output.
    pub(super) fn run_to(&self, index: usize, output: &mut dyn Write) -> Result<(), Error> {
        if let Some(campaigns) = self.campaigns {
            return campaigns.run(index, output, self.warnings);
        }
        if let Some(summaries) = self.reused.groups.get(&index) {
            return super::resume::render_reused(summaries, self.options.json, output);
        }
        let entry = &self.plan.entries[index];
        let config = &entry.configuration;
        let config_path = checked_benchmark_reference(self.parent, &entry.reference, self.source)?;
        let result_dir = self.directory(index);
        std::fs::create_dir_all(&result_dir).map_err(|error| {
            Error::Run(format!(
                "cannot create results directory {}: {error}",
                result_dir.display()
            ))
        })?;
        if self.options.mode == Some(tape::Mode::Replay) && self.restore_recordings {
            let previous = self
                .backup
                .join(self.results.file_name().expect("results name"))
                .join(result_name(index, &config_path))
                .join("cassettes");
            reuse_replay_inputs(
                &previous,
                &result_dir.join("cassettes"),
                config,
                self.options.resume,
            )?;
        }
        let resolved = result_dir.canonicalize().map_err(|error| {
            Error::Run(format!(
                "cannot resolve isolated benchmark {}: {error}",
                result_dir.display()
            ))
        })?;
        let isolated = isolated_config(config, &resolved);
        let isolated_path = resolved.join("benchmark.toml");
        let serialized = toml::to_string_pretty(&isolated)
            .map_err(|error| Error::Run(format!("cannot render sweep benchmark: {error}")))?;
        if !self.options.resume || !isolated_path.exists() {
            std::fs::write(&isolated_path, serialized).map_err(|error| {
                Error::Run(format!("cannot write {}: {error}", isolated_path.display()))
            })?;
        } else if crate::read_config(&isolated_path)? != serialized {
            return Err(Error::Input("isolated sweep configuration changed; preserve the existing attempts before starting a fresh sweep".into()));
        }
        let cassette = self.replay_inputs.map(|root| {
            root.join(result_name(index, &config_path))
                .join("cassettes")
                .join(cassette_filename(config))
        });
        let observer = self
            .checkpoint
            .map(|inputs| super::checkpoint::ReplicaObserver {
                inputs,
                index,
                directory: &resolved,
                entry,
                publication: Mutex::new(()),
            });
        if !self.reused.slots.contains_key(&index)
            && let Some(observer) = observer.as_ref()
        {
            observer.reserve()?;
        }
        let result = runner::run_with_diagnostics(
            isolated_path.to_string_lossy().as_ref(),
            runner::RunOptions {
                policy: entry.policy.as_ref(),
                max_steps: self.options.max_steps,
                json: self.options.json,
                show_output: self.options.show_output,
                quiet_output: self.options.quiet_output,
                interactive: self.options.live_progress(),
                measurement_dir: None,
                mode: self.options.mode,
                tool_mode: self.options.tool_mode,
                tool_mode_explicit: self.options.tool_mode_explicit,
                mcp_mode: self.options.mcp_mode,
                cassette: cassette.as_deref().and_then(Path::to_str),
                respect_record_timings: self.options.respect_record_timings,
                requirements_prepared: !self.options.resume,
                on_completion: observer
                    .as_ref()
                    .map(|observer| observer as &dyn runner::replica::CompletionObserver),
                reused: self.reused.slots.get(&index),
                ..Default::default()
            },
            output,
            self.warnings,
        );
        // Preserve official false grades too. Failure to produce a complete
        // evidence seal must not change the benchmark result or retry a task.
        let presentation = if let Some(checkpoint) = self.checkpoint
            && let Err(error) = checkpoint.completed(index, &resolved, entry)
        {
            runner::diagnostics::Warning::new(
                runner::diagnostics::Stage::Group,
                &config_path,
                &checkpoint.completion_path(index),
                &error,
            )
            .emit(self.options.json, output, self.warnings)
        } else {
            Ok(())
        };
        runner::diagnostics::preserve_primary(result, presentation)
    }

    pub(super) fn stop_requested(&self, index: usize) -> bool {
        self.campaigns
            .is_some_and(|campaigns| campaigns.stop_requested(index))
    }

    fn presentation_count(&self, index: usize) -> usize {
        let repeat = self
            .plan
            .execution
            .as_ref()
            .map_or(1, |intent| intent.repeat as usize);
        repeat * self.plan.entries[index].configuration.concurrency
    }

    pub(super) fn drain_output(&self, index: usize, output: &mut dyn Write) -> Result<(), Error> {
        if self.plan.execution.is_some() {
            spool::drain_campaign(
                &self.output_path(index),
                self.presentation_count(index),
                output,
            )
        } else {
            spool::drain(
                &self.output_path(index),
                self.presentation_count(index),
                output,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_repeated_campaign_restores_interrupted_output_without_a_live_pool() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sweep-execution-fixtures")
            .join(crate::workloads::TaskIdentity::local("campaign-spool-resume", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\n",
        )
        .unwrap();
        let selection = crate::workloads::TaskSelection {
            module: None,
            workload: "bug-fix".into(),
            task_id: "bug-fix".into(),
            dataset: None,
            evaluation_kind: "bundled-fixture".into(),
            population_size: None,
        };
        let plan: plan::SweepPlan = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "run_id": "abc",
            "execution": {"repeat": 65, "seed": null, "selector": null},
            "entries": [{"reference": "a.toml", "configuration": config, "selection": selection}]
        }))
        .unwrap();
        let mut reused = super::super::resume::State::default();
        reused.groups.insert(
            0,
            vec![serde_json::json!({"process_success":true,"workload_success":true})],
        );
        let warnings = Mutex::new(BTreeSet::new());
        let execution = Execution {
            campaigns: None,
            source: "fixture",
            parent: &root,
            plan: &plan,
            results: &root,
            backup: &root,
            replay_inputs: None,
            restore_recordings: false,
            options: Options {
                resume: true,
                json: true,
                ..Default::default()
            },
            warnings: &warnings,
            checkpoint: None,
            reused: &reused,
        };
        let interrupted = vec![b'x'; 4 * 1024 * 1024 + 65_537];
        std::fs::write(execution.output_path(0), &interrupted).unwrap();
        let report = execution.run(0);
        assert!(report.result.is_ok(), "{:?}", report.result.err());
        assert!(report.spool_created);
        let history = root.join(".checkpoint/interrupted-terminal");
        let saved = std::fs::read_dir(history)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read(saved).unwrap(), interrupted);
        let mut output = Vec::new();
        execution.drain_output(0, &mut output).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("\"reused\":true")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_creation_and_old_spool_preservation_never_claim_cleanup_ownership() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sweep-execution-fixtures")
            .join(crate::workloads::TaskIdentity::local("spool-ownership", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\n",
        )
        .unwrap();
        let selection = crate::workloads::TaskSelection {
            module: None,
            workload: "bug-fix".into(),
            task_id: "bug-fix".into(),
            dataset: None,
            evaluation_kind: "bundled-fixture".into(),
            population_size: None,
        };
        let plan:plan::SweepPlan=serde_json::from_value(serde_json::json!({"schema_version":1,"run_id":"abc","entries":[{"reference":"a.toml","configuration":config,"selection":selection}]})).unwrap();
        let warnings = Mutex::new(BTreeSet::new());
        let reused = super::super::resume::State::default();
        let path = root.join(".terminal-output-000000.tmp");
        std::fs::write(&path, "old output must survive").unwrap();
        std::fs::create_dir(root.join(".checkpoint")).unwrap();
        std::fs::write(
            root.join(".checkpoint/interrupted-terminal"),
            "history blocker",
        )
        .unwrap();
        for resume in [false, true] {
            let execution = Execution {
                campaigns: None,
                source: "fixture",
                parent: &root,
                plan: &plan,
                results: &root,
                backup: &root,
                replay_inputs: None,
                restore_recordings: false,
                options: Options {
                    resume,
                    ..Default::default()
                },
                warnings: &warnings,
                checkpoint: None,
                reused: &reused,
            };
            let report = execution.run(0);
            assert!(report.result.is_err());
            assert!(report.output_error.is_some());
            assert!(!report.spool_created);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "old output must survive"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
