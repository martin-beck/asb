//! Pure selection serialization: no directory or destination mutation here.
use super::*;

/// Lower bound on pretty-TOML size, BEFORE the serializer clones strings or
/// builds encoded values. TOML never compresses decoded UTF-8 string bytes;
/// even a multiline literal preserves them and adds delimiters. Every string
/// array member also requires at least two delimiter bytes. Fixed keys/numbers
/// only add output, so ignoring those cannot reject a valid <=64KiB document.
/// This bounds serializer SOURCE storage, not arbitrary serde implementations.
pub(super) fn source_bound(config: &BenchmarkConfig) -> Result<(), Error> {
    let mut remaining = crate::benchmark::MAX_CONFIG_BYTES;
    let mut string = |value: &str| -> Result<(), Error> {
        remaining = remaining.checked_sub(value.len().saturating_add(2)).ok_or_else(||
            Error::Input("generated benchmark configuration exceeds 64 KiB before serialization; shorten selected values; no setup files were written".into()))?;
        Ok(())
    };
    // Destructure exhaustively: adding a field requires reviewing this bound.
    let BenchmarkConfig {
        planning,
        agent,
        workload,
        provider,
        model,
        concurrency,
        trial_set,
        trial,
        workspace,
        log_file,
        measurements,
        mcp_servers,
    } = config;
    let _: &usize = concurrency;
    for value in [agent, workload, provider, model, workspace, log_file] {
        string(value)?;
    }
    for value in [trial_set, trial].into_iter().flatten() {
        string(value)?;
    }
    if let Some(crate::benchmark::MeasurementConfig {
        enabled,
        directory,
        collectors,
    }) = measurements
    {
        let _: &bool = enabled;
        string(directory)?;
        for value in collectors {
            string(value)?;
        }
    }
    for server in mcp_servers {
        string(&server.id)?;
        string(&server.program)?;
        for value in &server.args {
            string(value)?;
        }
    }
    if let Some(crate::planning::PlanningConfig {
        backend,
        required_policies,
        budgets,
        reservation,
        repetition,
    }) = planning
    {
        // Also destructure numeric-only nested records exhaustively so schema
        // additions cannot silently bypass the source-storage review.
        let crate::planning::PhaseBudgets {
            environment_preparation_seconds,
            agent_preparation_seconds,
            execution_seconds,
            evaluation_seconds,
        } = budgets;
        let _: [&Option<u64>; 4] = [
            environment_preparation_seconds,
            agent_preparation_seconds,
            execution_seconds,
            evaluation_seconds,
        ];
        if let Some(crate::planning::Resources {
            cpu_millis,
            memory_mib,
            disk_mib,
        }) = reservation
        {
            let _: [&u64; 3] = [cpu_millis, memory_mib, disk_mib];
        }
        let crate::planning::Repetition { index, total } = repetition;
        let _: [&usize; 2] = [index, total];
        if let Some(value) = backend {
            string(value)?;
        }
        for value in required_policies {
            string(value)?;
        }
    }
    Ok(())
}

/// Check each expanded selection before retaining another cloned configuration.
/// Final serialization rechecks bounds after per-output paths are assigned.
#[derive(Default)]
pub(crate) struct Population {
    configurations: Vec<BenchmarkConfig>,
    encoded_bytes: usize,
}
impl Population {
    pub(crate) fn len(&self) -> usize {
        self.configurations.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.configurations.is_empty()
    }
    pub(crate) fn push(&mut self, configuration: BenchmarkConfig) -> Result<(), Error> {
        if self.len() >= crate::sweep::MAX_CONFIGS {
            return Err(Error::Input(
                "setup configuration population limit exceeded".into(),
            ));
        }
        let bytes = render(&configuration)?.len();
        let total = self.encoded_bytes.checked_add(bytes).ok_or_else(|| {
            Error::Input("setup configuration size overflow; no setup files were written".into())
        })?;
        if total > crate::sweep::plan::MAX_CONFIG_BYTES {
            return Err(Error::Input(
                "generated sweep configurations exceed 32 MiB during selection expansion; select a smaller batch; no setup files were written".into(),
            ));
        }
        self.configurations.push(configuration);
        self.encoded_bytes = total;
        Ok(())
    }
    pub(crate) fn into_configs(self) -> Vec<BenchmarkConfig> {
        self.configurations
    }
}

pub(super) struct Sweep {
    pub(super) directory: PathBuf,
    pub(super) configurations: Vec<(String, String)>,
    pub(super) manifest: String,
}

impl Sweep {
    pub(super) fn new(path: &Path, mut configs: Vec<BenchmarkConfig>) -> Result<Self, Error> {
        if configs.is_empty() || configs.len() > crate::sweep::MAX_CONFIGS {
            return Err(Error::Input(
                "invalid setup configuration population".into(),
            ));
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_str()
            .ok_or_else(|| unrepresentable_path(path))?;
        let group = format!("{stem}-configs-{}", stamp());
        let directory = std::path::absolute(parent.join(&group)).map_err(crate::io_error)?;
        // The wire format stores generated runtime paths as UTF-8 strings.
        // Refuse before any mkdir/write instead of silently targeting U+FFFD.
        directory
            .to_str()
            .ok_or_else(|| unrepresentable_path(&directory))?;
        let mut configurations = Vec::with_capacity(configs.len());
        let mut references = Vec::with_capacity(configs.len());
        let mut reference_bytes = 0usize;
        let mut total = 0usize;
        for (index, config) in configs.iter_mut().enumerate() {
            source_bound(config)?;
            crate::planning::resolve(config)?;
            let filename = format!("benchmark-{index:03}.toml");
            config.log_file =
                crate::benchmark::default_log_path(&format!("benchmark-{index:03}.log"));
            if let Some(measures) = config.measurements.as_mut() {
                measures.directory = directory
                    .join(format!("measurements-{index:03}"))
                    .to_str()
                    .ok_or_else(|| unrepresentable_path(&directory))?
                    .to_owned();
            }
            let text = render(config)?;
            total = total.checked_add(text.len()).ok_or_else(|| {
                Error::Input(
                    "setup configuration size overflow; no setup files were written".into(),
                )
            })?;
            if total > crate::sweep::plan::MAX_CONFIG_BYTES {
                return Err(Error::Input(
                    "generated sweep configurations exceed 32 MiB; select a smaller batch; no setup files were written".into(),
                ));
            }
            reference_bytes = reference_bytes
                .checked_add(group.len())
                .and_then(|size| size.checked_add(filename.len() + 3))
                .filter(|size| *size as u64 <= crate::sweep::MAX_SWEEP_BYTES)
                .ok_or_else(manifest_size_error)?;
            references.push(format!("{group}/{filename}"));
            configurations.push((filename, text));
        }
        let manifest = render_manifest(references)?;
        Ok(Self {
            directory,
            configurations,
            manifest,
        })
    }
}

fn unrepresentable_path(path: &Path) -> Error {
    Error::Input(format!(
        "setup output path {path:?} cannot be represented as UTF-8 in generated configuration; choose UTF-8 project/output directories; no setup files were written"
    ))
}

fn manifest_size_error() -> Error {
    Error::Input("generated sweep manifest exceeds 16 MiB; shorten the output filename or select a smaller batch; no setup files were written".into())
}
fn render_manifest(references: Vec<String>) -> Result<String, Error> {
    let mut remaining = crate::sweep::MAX_SWEEP_BYTES as usize;
    for reference in &references {
        remaining = remaining
            .checked_sub(reference.len().saturating_add(2))
            .ok_or_else(manifest_size_error)?;
    }
    let text = toml::to_string_pretty(&crate::sweep::SweepFile {
        resources: None,
        configs: references,
    })
    .map_err(|error| Error::Input(format!("cannot render sweep: {error}")))?;
    if text.len() as u64 > crate::sweep::MAX_SWEEP_BYTES {
        return Err(manifest_size_error());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BenchmarkConfig {
        BenchmarkConfig {
            planning: None,
            agent: "pi".into(),
            workload: "bug-fix".into(),
            provider: "openrouter".into(),
            model: "fixture-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "benchmark.log".into(),
            measurements: None,
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn scalar_serialized_bound_accepts_exact_limit_and_rejects_one_more_byte() {
        let mut config = config();
        config.workspace.clear();
        let overhead = render(&config).unwrap().len();
        config.workspace = "x".repeat(crate::benchmark::MAX_CONFIG_BYTES - overhead);
        assert_eq!(
            render(&config).unwrap().len(),
            crate::benchmark::MAX_CONFIG_BYTES
        );
        config.workspace.push('x');
        assert!(render(&config).unwrap_err().to_string().contains("64 KiB"));
    }

    #[test]
    fn generated_sweep_configurations_keep_each_log_under_asb_logs() {
        let prepared = Sweep::new(Path::new("selection.toml"), vec![config(), config()])
            .expect("generated sweep");
        let logs: Vec<_> = prepared
            .configurations
            .iter()
            .map(|(_, text)| {
                toml::from_str::<BenchmarkConfig>(text)
                    .expect("generated benchmark configuration")
                    .log_file
            })
            .collect();
        assert_eq!(
            logs,
            [".asb-logs/benchmark-000.log", ".asb-logs/benchmark-001.log",]
        );
    }

    #[test]
    fn escaped_values_are_bounded_after_toml_encoding_not_by_character_count() {
        let mut config = config();
        config.workspace = "\n".repeat(40_000);
        // TOML may choose a multiline literal rather than escaping every byte;
        // measure its real serialization rather than assuming one encoding.
        let bytes = toml::to_string_pretty(&config).unwrap().len();
        assert_eq!(
            render(&config).is_ok(),
            bytes <= crate::benchmark::MAX_CONFIG_BYTES
        );
        config.workspace = "\u{0000}".repeat(12_000);
        assert!(config.workspace.len() < crate::benchmark::MAX_CONFIG_BYTES);
        assert!(render(&config).unwrap_err().to_string().contains("64 KiB"));
    }

    #[test]
    fn late_oversized_configuration_is_rejected_before_any_setup_publication() {
        let good = config();
        let mut invalid = good.clone();
        invalid.workspace = "x".repeat(crate::benchmark::MAX_CONFIG_BYTES);
        let error = match Sweep::new(Path::new("unused/selection.toml"), vec![good, invalid]) {
            Ok(_) => panic!("oversized late configuration was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no setup files were written"));
    }

    #[test]
    fn serialized_aggregate_budget_is_checked_before_manifest_publication() {
        let mut config = config();
        config.workspace = "x".repeat(40_000);
        let error = match Sweep::new(Path::new("unused/selection.toml"), vec![config; 840]) {
            Ok(_) => panic!("oversized aggregate was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("32 MiB"), "{error}");
    }

    #[test]
    fn expansion_rejects_byte_budget_before_retaining_a_gib_sized_population() {
        let mut population = Population::default();
        let mut config = config();
        config.workspace = "x".repeat(60_000);
        for _ in 0..600 {
            let retained = population.len();
            if let Err(error) = population.push(config.clone()) {
                assert!(
                    error.to_string().contains("during selection expansion"),
                    "{error}"
                );
                assert_eq!(
                    population.len(),
                    retained,
                    "rejection must not retain the candidate"
                );
                assert!(retained < 600);
                return;
            }
        }
        panic!("selection expansion retained an excessive population");
    }

    #[test]
    fn raw_storage_check_covers_optional_planning_and_collection_cardinality() {
        let mut candidate = config();
        candidate.planning = Some(crate::planning::PlanningConfig {
            backend: Some("x".repeat(crate::benchmark::MAX_CONFIG_BYTES)),
            ..Default::default()
        });
        assert!(source_bound(&candidate).is_err());
        candidate.planning.as_mut().unwrap().backend = None;
        candidate.planning.as_mut().unwrap().required_policies = vec![String::new(); 40_000];
        assert!(source_bound(&candidate).is_err());
        candidate.planning = None;
        candidate.measurements = Some(crate::benchmark::MeasurementConfig {
            enabled: false,
            directory: String::new(),
            collectors: vec![String::new(); 40_000],
        });
        assert!(source_bound(&candidate).is_err());
        candidate.measurements = None;
        for value in [
            "\n".repeat(40_000),
            "é".repeat(25_000),
            "'\"\\\r\n\u{0000}".repeat(1000),
        ] {
            candidate.workspace = value;
            let expected = toml::to_string_pretty(&candidate).unwrap();
            assert!(expected.len() >= candidate.workspace.len());
            assert_eq!(
                render(&candidate).is_ok(),
                expected.len() <= crate::benchmark::MAX_CONFIG_BYTES
            );
        }
    }

    #[test]
    fn aggregate_serialization_accepts_exact_32_mib_and_rejects_one_extra_byte() {
        let mut base = config();
        base.log_file = crate::benchmark::default_log_path("benchmark-000.log");
        base.workspace.clear();
        let overhead = render(&base).unwrap().len();
        let mut full = base.clone();
        full.workspace = "x".repeat(crate::benchmark::MAX_CONFIG_BYTES - overhead);
        let last_size = render(&base).unwrap().len();
        let mut reduced = full.clone();
        reduced
            .workspace
            .truncate(reduced.workspace.len() - last_size);
        let mut configs = vec![full; 511];
        configs.push(reduced);
        configs.push(base);
        let prepared = Sweep::new(Path::new("unused/exact.toml"), configs.clone()).unwrap();
        assert_eq!(
            prepared
                .configurations
                .iter()
                .map(|(_, text)| text.len())
                .sum::<usize>(),
            crate::sweep::plan::MAX_CONFIG_BYTES
        );
        drop(prepared);
        configs.last_mut().unwrap().workspace.push('x');
        let error = match Sweep::new(Path::new("unused/over.toml"), configs) {
            Ok(_) => panic!("one-byte aggregate overflow accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("32 MiB"), "{error}");
    }

    #[test]
    fn manifest_serialization_accepts_exact_16_mib_and_rejects_one_extra_byte() {
        let overhead = render_manifest(vec![String::new()]).unwrap().len();
        let reference = "x".repeat(crate::sweep::MAX_SWEEP_BYTES as usize - overhead);
        assert_eq!(
            render_manifest(vec![reference.clone()]).unwrap().len() as u64,
            crate::sweep::MAX_SWEEP_BYTES
        );
        assert!(
            render_manifest(vec![format!("{reference}x")])
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
    }
}
