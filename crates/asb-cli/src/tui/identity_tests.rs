//! Identity regressions use local files and injected discovery, never providers.
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tui-identity")
            .join(workloads::TaskIdentity::local("identity", None).attempt_id);
        fs::create_dir_all(root.join("workloads")).unwrap();
        Self(root)
    }
    fn write(&self, file: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn app(&self) -> App {
        App::new(self.0.clone(), project::ProjectConfig::default())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn choice(key: ChoiceKey, label: &str) -> Choice {
    Choice {
        key,
        label: label.into(),
        availability: Availability::Available,
    }
}
fn select(pane: &mut Pane, key: &ChoiceKey) {
    pane.cursor = pane
        .choices
        .iter()
        .position(|choice| &choice.key == key)
        .unwrap();
    pane.toggle_single();
}
fn scalar_app(fixture: &Fixture, workload: &str) -> App {
    let mut app = fixture.app();
    app.panes[PaneKind::Agents.index()] =
        Pane::new(vec![choice(ChoiceKey::Agent("pi".into()), "renamed agent")]);
    app.panes[PaneKind::Providers.index()] = Pane::new(vec![choice(
        ChoiceKey::Provider("local".into()),
        "renamed provider",
    )]);
    // Discover only after the raw provider identity is selected.
    app.panes[PaneKind::Agents.index()].selected[0] = true;
    app.panes[PaneKind::Providers.index()].selected[0] = true;
    app.refresh_models_with(|provider| {
        assert_eq!(provider, "local");
        Ok(vec!["fixture-model".into(), "fixture-model".into()])
    });
    assert_eq!(
        app.pane(PaneKind::Models).choices.len(),
        1,
        "deduplicate provider/model keys"
    );
    app.panes[PaneKind::Models.index()].selected[0] = true;
    app.panes[PaneKind::Models.index()].choices[0].label = "all [fallback]".into();
    select(
        app.pane_mut(PaneKind::Workloads),
        &ChoiceKey::Prompt(workload.into()),
    );
    app.panes[PaneKind::Workloads.index()]
        .choices
        .iter_mut()
        .for_each(|choice| choice.label = "presentation only".into());
    app.panes[PaneKind::Measurements.index()]
        .selected
        .fill(false);
    app
}

#[test]
fn persisted_trial_sets_replace_duplicate_workload_rows() {
    let fixture = Fixture::new();
    for name in [
        "foo",
        "foo [trial set]",
        "foo [trial set] [trial set]",
        "all",
        "all [trial set]",
        "echo.txt",
    ] {
        fixture.write(
            &format!("workloads/{name}.txt"),
            "Local fixture, never executed.",
        );
    }
    for name in ["foo", "foo [trial set]", "all"] {
        fixture.write(
            &format!("workloads/{name}.workload.toml"),
            format!("workload='{name}'\n[[trials]]\nid='raw-id'\ndescription='Fixture'\n"),
        );
    }
    let app = fixture.app();
    let pane = app.pane(PaneKind::Workloads);
    assert_eq!(
        pane.choices
            .iter()
            .filter(|choice| choice.label == "foo")
            .count(),
        1
    );
    assert_eq!(
        pane.choices
            .iter()
            .filter(|choice| choice.label == "foo [trial set]")
            .count(),
        1
    );
    for name in ["foo", "foo [trial set]"] {
        assert!(pane.choices.iter().any(|choice| {
            matches!(
                &choice.key,
                ChoiceKey::TrialSet { workload, file }
                    if workload == name && file == &PathBuf::from(format!("{name}.workload.toml"))
            )
        }));
    }
    assert!(
        pane.choices
            .iter()
            .any(|choice| choice.key == ChoiceKey::Prompt("all [trial set]".into()))
    );
    assert!(
        pane.choices
            .iter()
            .any(|choice| choice.key == ChoiceKey::Prompt("echo.txt".into()))
    );
    assert!(
        pane.choices
            .iter()
            .all(|choice| choice.key.workload() != Some("all"))
    );
    for name in [
        "foo [trial set]",
        "foo [trial set] [trial set]",
        "all [trial set]",
    ] {
        let selected = selected_trials(&app, &ChoiceKey::Prompt(name.into())).unwrap();
        assert_eq!(selected, (name.into(), None, vec![None]));
    }
    for name in ["foo", "foo [trial set]"] {
        let file = format!("{name}.workload.toml");
        let selected = selected_trials(
            &app,
            &ChoiceKey::TrialSet {
                workload: name.into(),
                file: file.clone().into(),
            },
        )
        .unwrap();
        assert_eq!(
            selected,
            (name.into(), Some(file), vec![Some("raw-id".into())])
        );
    }
    let duplicate = Pane::new(vec![
        choice(ChoiceKey::Prompt("foo".into()), "original"),
        choice(ChoiceKey::Prompt("foo".into()), "renamed"),
    ]);
    assert_eq!(duplicate.choices.len(), 1);
    assert_eq!(duplicate.choices[0].label, "original");
}

#[test]
fn persisted_swe_bench_trials_do_not_duplicate_the_external_workload() {
    let fixture = Fixture::new();
    fixture.write(
        "workloads/swe-bench.workload.toml",
        "workload='swe-bench'\n[[trials]]\nid='fixture-task'\ndescription='Fixture'\n",
    );
    let app = fixture.app();
    let choices = app
        .pane(PaneKind::Workloads)
        .choices
        .iter()
        .filter(|choice| choice.key.workload() == Some("swe-bench"))
        .collect::<Vec<_>>();
    assert_eq!(choices.len(), 1, "a trial set must not add a second row");
    assert!(matches!(choices[0].key, ChoiceKey::TrialSet { .. }));
}

#[test]
fn every_concurrency_value_publishes_one_numeric_toml_value_independent_of_labels() {
    let fixture = Fixture::new();
    fixture.write("workloads/all [trial set].txt", "Never executed.");
    let mut app = scalar_app(&fixture, "all [trial set]");
    let concurrency = app.pane(PaneKind::Concurrency);
    assert_eq!(
        concurrency
            .choices
            .iter()
            .filter_map(|choice| choice.key.concurrency())
            .collect::<Vec<_>>(),
        (1..=crate::benchmark::MAX_CONCURRENCY).collect::<Vec<_>>()
    );
    for value in 1..=crate::benchmark::MAX_CONCURRENCY {
        app.pane_mut(PaneKind::Concurrency).selected.fill(false);
        select(
            app.pane_mut(PaneKind::Concurrency),
            &ChoiceKey::Concurrency(value),
        );
        app.pane_mut(PaneKind::Concurrency)
            .choices
            .iter_mut()
            .for_each(|choice| choice.label = "999".into());
        let generated = generate_configs(&app).unwrap();
        assert_eq!(generated.tests, 1);
        let sweep = crate::sweep::read_manifest(&generated.sweep).unwrap();
        let config: BenchmarkConfig =
            toml::from_str(&fs::read_to_string(fixture.0.join(&sweep.configs[0])).unwrap())
                .unwrap();
        assert_eq!(config.concurrency, value);
        assert_eq!(
            (
                &*config.agent,
                &*config.provider,
                &*config.model,
                &*config.workload
            ),
            ("pi", "local", "fixture-model", "all [trial set]")
        );
        assert_eq!(config.trial_set, None);
    }
    app.pane_mut(PaneKind::Concurrency).selected.fill(false);
    assert!(
        generate_configs(&app)
            .err()
            .unwrap()
            .to_string()
            .contains("exactly one")
    );
    app.pane_mut(PaneKind::Concurrency).selected[..2].fill(true);
    assert!(
        generate_configs(&app)
            .err()
            .unwrap()
            .to_string()
            .contains("exactly one")
    );
}

#[test]
fn labels_cannot_retarget_install_readiness_or_collector_dependencies() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write("workloads/custom [trial set].txt", "Never executed.");
    #[cfg(unix)]
    for (tool, executable) in [("pi", "pi"), ("perf", "perf"), ("flamegraph", "flamegraph")] {
        let path = format!(".asb-tools/{tool}/bin/{executable}");
        fixture.write(&path, "#!/bin/sh\nexit 99\n");
        fs::set_permissions(fixture.0.join(path), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut app = scalar_app(&fixture, "custom [trial set]");
    for (kind, key, expected, workload) in [
        (PaneKind::Agents, ChoiceKey::Agent("pi".into()), "pi", false),
        (
            PaneKind::Workloads,
            ChoiceKey::Prompt("custom [trial set]".into()),
            "custom [trial set]",
            true,
        ),
        (
            PaneKind::Workloads,
            ChoiceKey::TrialSet {
                workload: "custom [trial set]".into(),
                file: "different-file.workload.toml".into(),
            },
            "custom [trial set]",
            true,
        ),
        (
            PaneKind::Measurements,
            ChoiceKey::Collector("flamegraph".into()),
            "flamegraph",
            false,
        ),
    ] {
        app.step = kind;
        let request =
            setup_install_request(&app, &choice(key, "all [trial set] [fallback]")).unwrap();
        match request {
            install::operation::Request::Workload { name, .. } => {
                assert!(workload);
                assert_eq!(name, expected);
            }
            install::operation::Request::Tool { name, .. } => {
                assert!(!workload);
                assert_eq!(name, expected);
            }
            _ => panic!("unexpected dependency request"),
        }
    }
    let measurements = app.pane_mut(PaneKind::Measurements);
    measurements
        .choices
        .iter_mut()
        .for_each(|choice| choice.label = "unrelated".into());
    select(measurements, &ChoiceKey::Collector("flamegraph".into()));
    let expanded = app.sync_collector_dependencies();
    assert!(expanded.contains(&"flamegraph".into()) && expanded.contains(&"perf".into()));
    assert!(
        app.pane(PaneKind::Measurements)
            .selected_choices()
            .any(|choice| choice.key.collector() == Some("perf"))
    );
    app.refresh_availability();
    #[cfg(unix)]
    {
        assert_eq!(
            app.pane(PaneKind::Agents).choices[0].availability,
            Availability::Available
        );
        assert!(
            app.pane(PaneKind::Measurements)
                .selected_choices()
                .all(|choice| choice.availability == Availability::Available)
        );
    }
    assert_eq!(
        app.pane(PaneKind::Workloads)
            .selected_choices()
            .next()
            .unwrap()
            .availability,
        Availability::Available
    );
}

#[test]
fn saved_trial_reopen_uses_ids_after_labels_change_and_all_steps_have_named_transitions() {
    let mut wizard = WorkloadWizard::default();
    let trials = || {
        Pane::new(vec![
            choice(ChoiceKey::Trial("first-id".into()), "same label"),
            choice(ChoiceKey::Trial("second-id".into()), "same label"),
        ])
    };
    let mut pane = trials();
    pane.selected[1] = true;
    wizard
        .reduce(WorkloadAction::Open {
            workload: "raw-workload".into(),
            trials: pane,
        })
        .unwrap();
    wizard.reduce(WorkloadAction::Save).unwrap();
    assert_eq!(
        wizard.selections["raw-workload"],
        BTreeSet::from(["second-id".into()])
    );
    let mut reopened = trials();
    reopened.choices[1].label = "changed".into();
    wizard
        .reduce(WorkloadAction::Open {
            workload: "raw-workload".into(),
            trials: reopened,
        })
        .unwrap();
    let WorkloadStep::Editing { trials, .. } = wizard.step else {
        panic!("must reopen editor")
    };
    assert_eq!(trials.selected, [false, true]);
    for (index, kind) in PaneKind::ORDER.iter().copied().enumerate() {
        assert_eq!(kind.index(), index);
        assert_eq!(kind.next(), PaneKind::ORDER.get(index + 1).copied());
        assert_eq!(
            kind.previous(),
            index
                .checked_sub(1)
                .map(|previous| PaneKind::ORDER[previous])
        );
        assert_eq!(
            kind.installable(),
            matches!(
                kind,
                PaneKind::Agents | PaneKind::Workloads | PaneKind::Measurements
            )
        );
        assert!(!kind.name().is_empty() && !kind.description().is_empty());
    }
    let fixture = Fixture::new();
    let mut app = fixture.app();
    for (kind, key) in [
        (PaneKind::Providers, ChoiceKey::Provider("local".into())),
        (
            PaneKind::Models,
            ChoiceKey::Model {
                provider: "local".into(),
                model: "fixture".into(),
            },
        ),
        (PaneKind::Concurrency, ChoiceKey::Concurrency(64)),
    ] {
        app.step = kind;
        assert!(setup_install_request(&app, &choice(key, "misleading install label")).is_none());
    }
}
