//! Expand reserved selectors using the same owning registries as the wizard.
use super::*;
use crate::selection_names::{ALL, validate};
use crate::workloads::catalog::{self as workload_catalog, Catalog};

fn expand_values(values: &mut Vec<String>, catalog: Vec<String>, kind: &str) -> Result<(), Error> {
    for name in &catalog {
        validate(name, kind).map_err(Error::Input)?;
    }
    if values.iter().any(|value| value == ALL) {
        if catalog.is_empty() {
            return Err(Error::Input(format!(
                "cannot select all {kind}: the selection catalog is empty"
            )));
        }
        let mut expanded = Vec::new();
        for value in values.iter() {
            if value == ALL {
                expanded.extend(catalog.iter().cloned());
            } else {
                expanded.push(value.clone());
            }
        }
        *values = unique(&expanded).into_iter().cloned().collect();
    }
    Ok(())
}

pub(super) fn expand(options: &mut SetupOptions) -> Result<(), Error> {
    let (root, settings) = project::load(Path::new("."))?;
    let _network = project::network_project(&root);
    let directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project::resolve(&root, &settings.workloads_dir));
    expand_values(
        &mut options.agent,
        agents::all()
            .iter()
            .map(|adapter| adapter.id().to_owned())
            .collect(),
        "agent",
    )?;
    expand_values(
        &mut options.provider,
        providers::names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        "provider",
    )?;
    expand_values(
        &mut options.measure,
        measurements::available_collectors(),
        "measurement",
    )?;
    if options.workload.iter().any(|value| value == ALL) {
        expand_values(
            &mut options.workload,
            workload_names(&directory)?,
            "workload",
        )?;
    }
    if options.trial_set.iter().any(|value| value == ALL) {
        let mut filenames = Vec::new();
        for entry in catalog(&directory)?.trial_sets() {
            validate(&entry.name, "trial set").map_err(Error::Input)?;
            let set = crate::workloads::read_trial_set(&directory.join(&entry.file))
                .map_err(Error::Input)?;
            if options.workload.contains(&set.workload) {
                filenames.push(
                    entry
                        .file
                        .to_str()
                        .expect("discovered UTF-8 filename")
                        .to_owned(),
                );
            }
        }
        expand_values(&mut options.trial_set, filenames, "trial set")?;
    }
    // Resolve only explicit `all` requests. Scalar setup stays entirely offline.
    let mut expanded = Vec::new();
    let mut discovered = BTreeMap::<String, Vec<String>>::new();
    for value in &options.model {
        let (scope, model) = value
            .split_once('=')
            .map_or((None, value.as_str()), |(scope, model)| {
                (Some(scope), model)
            });
        if model != ALL {
            expanded.push(value.clone());
            continue;
        }
        if let Some(scope) = scope
            && !options.provider.iter().any(|name| name == scope)
        {
            return Err(Error::Input(format!(
                "--model `{value}` refers to unselected provider `{scope}`"
            )));
        }
        for provider in unique(&options.provider)
            .into_iter()
            .filter(|provider| scope.is_none_or(|scope| scope == provider.as_str()))
        {
            if !discovered.contains_key(provider) {
                let models = crate::models::discover_for_setup(provider).map_err(|error| Error::Input(format!("cannot expand --model {value} for provider `{provider}`: {error}; fix discovery or select explicit model IDs")))?;
                if models.is_empty() {
                    return Err(Error::Input(format!(
                        "cannot select all models for provider `{provider}`: no tool-capable models were returned"
                    )));
                }
                discovered.insert(provider.clone(), models);
            }
            for model in &discovered[provider] {
                validate(model, "model").map_err(Error::Input)?;
                expanded.push(format!("{provider}={model}"));
            }
        }
    }
    options.model = unique(&expanded).into_iter().cloned().collect();
    Ok(())
}

fn catalog(directory: &Path) -> Result<Catalog, Error> {
    let entries = match workload_catalog::paths(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        result => result.map_err(|error| {
            Error::Input(format!(
                "cannot enumerate selections in {}: {error}",
                directory.display()
            ))
        })?,
    };
    let paths = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::io_error)?;
    Ok(Catalog::from_paths(paths))
}

fn workload_names(directory: &Path) -> Result<Vec<String>, Error> {
    let mut names = crate::workloads::builtin_names()
        .chain(workload_catalog::installable_names())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let catalog = catalog(directory)?;
    names.extend(catalog.prompt_names().map(str::to_owned));
    for entry in catalog.trial_sets() {
        validate(&entry.name, "trial set").map_err(Error::Input)?;
        names.insert(
            crate::workloads::read_trial_set(&directory.join(&entry.file))
                .map_err(Error::Input)?
                .workload,
        );
    }
    for name in &names {
        validate(name, "workload").map_err(Error::Input)?;
    }
    Ok(names.into_iter().collect())
}

pub(super) fn trials(
    values: &[String],
    catalog: Option<&crate::workloads::PlanningCatalog>,
    enabled: Option<&BTreeSet<String>>,
    workload: &str,
) -> Result<Vec<Option<String>>, Error> {
    let mut expanded = Vec::new();
    for value in values {
        if value == ALL {
            let available = enabled.or_else(|| catalog.map(|catalog| &catalog.task_ids));
            if let Some(available) = available {
                if available.is_empty() {
                    return Err(Error::Input(format!(
                        "cannot select all trials for `{workload}`: no trials are available"
                    )));
                }
                for trial in available {
                    validate(trial, "trial").map_err(Error::Input)?;
                    expanded.push(Some(trial.clone()));
                }
            } else if workload_catalog::is_installable(workload) {
                return Err(Error::Input(format!(
                    "cannot select all trials for `{workload}` without an installed catalog or --trial-set; select enabled trials with `asb workload tui` first"
                )));
            } else {
                // Native prompt workloads have one implicit task, not synthetic trials.
                expanded.push(None);
            }
        } else {
            expanded.push(Some(value.clone()));
        }
    }
    let mut seen = BTreeSet::new();
    expanded.retain(|value| seen.insert(value.clone()));
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_all_expansion_matches_explicit_complete_lists_without_discovery() {
        let mut options = SetupOptions {
            agent: vec![ALL.into()],
            provider: vec![ALL.into()],
            measure: vec![ALL.into()],
            workload: vec!["bug-fix".into()],
            model: vec!["explicit-offline-model".into()],
            trial: vec![],
            trial_set: vec![],
            concurrency: None,
            workspace: None,
            output: None,
        };
        expand(&mut options).unwrap();
        assert_eq!(
            options.agent,
            agents::all()
                .iter()
                .map(|adapter| adapter.id())
                .collect::<Vec<_>>()
        );
        assert_eq!(options.provider, providers::names());
        assert_eq!(options.measure, measurements::available_collectors());
    }

    #[test]
    fn trial_all_uses_enabled_subset_or_original_catalog_and_never_synthetic_ids() {
        let catalog = crate::workloads::PlanningCatalog {
            requirements: Default::default(),
            dataset: crate::workloads::session::DatasetIdentity {
                source: "fixture".into(),
                revision: "a".repeat(40),
                split: "test".into(),
            },
            task_ids: ["one".into(), "two".into()].into_iter().collect(),
            evaluation_kind: crate::workloads::KnownEvaluationKind::UpstreamHarness,
        };
        let all = vec![ALL.into()];
        assert_eq!(
            trials(&all, Some(&catalog), None, "fixture").unwrap(),
            vec![Some("one".into()), Some("two".into())]
        );
        let enabled = ["two".into()].into_iter().collect();
        assert_eq!(
            trials(&all, Some(&catalog), Some(&enabled), "fixture").unwrap(),
            vec![Some("two".into())]
        );
        assert_eq!(trials(&all, None, None, "bug-fix").unwrap(), vec![None]);
        assert!(trials(&all, None, None, "agentbench").is_err());
    }

    #[test]
    fn all_expands_in_order_deduplicates_and_never_accepts_a_reserved_entry() {
        let mut values = vec!["first".into(), ALL.into(), ALL.into()];
        expand_values(
            &mut values,
            vec!["first".into(), "second".into()],
            "fixture",
        )
        .unwrap();
        assert_eq!(values, ["first", "second"]);
        assert!(expand_values(&mut vec![ALL.into()], vec![], "fixture").is_err());
        assert!(
            expand_values(&mut vec![ALL.into()], vec![ALL.into()], "fixture")
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }

    #[test]
    fn workload_all_contains_every_registered_and_local_selection() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("all-workloads", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("custom.txt"), "fixture").unwrap();
        let names = workload_names(&root).unwrap();
        for name in crate::workloads::builtin_names()
            .chain(crate::install::external_workload_names())
            .chain(["custom"])
        {
            assert!(names.iter().any(|value| value == name), "{name}");
        }
        fs::write(root.join("all.txt"), "fixture").unwrap();
        assert!(
            workload_names(&root)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        fs::remove_file(root.join("all.txt")).unwrap();
        fs::remove_file(root.join("custom.txt")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
