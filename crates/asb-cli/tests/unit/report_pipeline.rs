//! Offline phase, source, chart and publication contracts; never starts a benchmark.
use super::*;
use std::io;
type ReadHook = Box<dyn FnOnce(&fs::File, &Path)>;
thread_local! { static READ_HOOK: std::cell::RefCell<Option<ReadHook>>=const {std::cell::RefCell::new(None)}; }
thread_local! {static FAIL_SYNC: std::cell::Cell<bool>=const {std::cell::Cell::new(false)};}
pub(super) fn fail_post_commit_sync() -> bool {
    FAIL_SYNC.get()
}
pub(super) fn after_open(file: &fs::File, path: &Path) {
    if let Some(hook) = READ_HOOK.take() {
        hook(file, path);
    }
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/report-pipeline-fixtures")
            .join(crate::workloads::TaskIdentity::local("report", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn seed(&self, base: &str, rows: usize) -> PathBuf {
        let prefix = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        let configs = (0..rows)
            .map(|index| format!("'{index}.toml'"))
            .collect::<Vec<_>>()
            .join(",");
        let manifest = self.write(
            &format!("{prefix}sweep.toml"),
            format!("configs=[{configs}]\n"),
        );
        for index in 0..rows {
            self.write(
                &format!("{prefix}{index}.toml"),
                "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n",
            );
            self.write(
                &format!("{prefix}sweep-results/{index:03}-{index}/measurements/run.json"),
                json!({"status":"completed","elapsed_ms":10+index}).to_string(),
            );
        }
        manifest
    }
    fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(base: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(base, &path, out)
                } else {
                    out.insert(
                        path.strip_prefix(base).unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(path, path, &mut out);
        out
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        READ_HOOK.take();
        FAIL_SYNC.set(false);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn compare_holds_sweep_ownership_before_reading_until_report_is_published() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, _| {
                opened_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }));
        });
        generate(manifest.to_str().unwrap(), &mut Vec::new())
    });
    opened_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let competitor =
        crate::output_lock::OutputLock::acquire(&fixture.0.join(".sweep-asb-sweep.lock"), "busy");
    let blocked = competitor.is_err();
    drop(competitor);
    release_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
    assert!(
        blocked,
        "a new sweep was able to rotate inputs while compare was reading"
    );
}

#[test]
fn external_compare_cannot_read_a_sweep_owned_by_an_active_writer() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let owner = crate::sweep::lock::OutputLock::acquire(&fixture.0, "sweep").unwrap();
    let error = generate(manifest.to_str().unwrap(), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("another sweep owns"));
    assert!(!fixture.0.join("results/sweep-comparison").exists());
    // The sweep's own finalization retains the same owner without re-locking.
    run_in_sweep(manifest.to_str().unwrap(), &mut Vec::new(), &owner).unwrap();
    let mut quiet = Vec::new();
    run_links_in_sweep(manifest.to_str().unwrap(), &mut quiet, &owner).unwrap();
    let quiet = String::from_utf8(quiet).unwrap();
    assert!(quiet.contains("Result report: "));
    assert!(!quiet.contains("Compared "));
    drop(owner);
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
}

#[test]
fn quiet_links_include_collector_defined_viewers() {
    let mut output = Vec::new();
    write_public_links(
        vec![("Other UI".into(), "https://example.invalid/viewer".into())],
        &mut output,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "Other UI: https://example.invalid/viewer\n"
    );
}

#[test]
fn comparison_of_nested_campaign_uses_its_sweep_source_owner() {
    let fixture = Fixture::new();
    let campaign = fixture
        .0
        .join("sweep-results/000-one/campaigns/a/plan.json");
    fs::create_dir_all(campaign.parent().unwrap()).unwrap();
    let owner = crate::sweep::lock::OutputLock::acquire(&fixture.0, "sweep").unwrap();
    let error = crate::sweep::lock::OutputLock::for_comparison(&campaign)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("another sweep owns"));
    drop(owner);
    assert!(
        crate::sweep::lock::OutputLock::for_comparison(&campaign)
            .unwrap()
            .is_some()
    );
}

#[test]
fn comparison_of_nested_benchmark_toml_uses_its_sweep_source_owner() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let benchmark = fixture.0.join("sweep-results/000-0/benchmark.toml");
    fs::copy(fixture.0.join("0.toml"), &benchmark).unwrap();
    let owner = crate::sweep::lock::OutputLock::acquire(&fixture.0, "sweep").unwrap();
    let error = generate(benchmark.to_str().unwrap(), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("another sweep owns"), "{error}");
    assert!(
        !fixture
            .0
            .join("sweep-results/000-0/.benchmark-asb-sweep.lock")
            .exists()
    );
    drop(owner);
    generate(benchmark.to_str().unwrap(), &mut Vec::new()).unwrap();
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
}

#[cfg(unix)]
#[test]
fn comparison_resolves_symlinked_nested_sources_before_selecting_ownership() {
    let fixture = Fixture::new();
    fixture.seed("", 1);
    let benchmark = fixture.0.join("sweep-results/000-0/benchmark.toml");
    fs::copy(fixture.0.join("0.toml"), &benchmark).unwrap();
    let alias_dir = fixture.0.join("alias");
    std::os::unix::fs::symlink("sweep-results/000-0", &alias_dir).unwrap();
    let alias_file = fixture.0.join("other-benchmark.toml");
    std::os::unix::fs::symlink(&benchmark, &alias_file).unwrap();
    let owner = crate::sweep::lock::OutputLock::acquire(&fixture.0, "sweep").unwrap();
    for path in [alias_dir.join("benchmark.toml"), alias_file] {
        let error = generate(path.to_str().unwrap(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("another sweep owns"), "{error}");
    }
    drop(owner);
    // The file alias has a distinct lexical output namespace; protect it as
    // well as the target's enclosing sweep while it remains a symlink.
    let alias_owner =
        crate::sweep::lock::OutputLock::acquire(&fixture.0, "other-benchmark").unwrap();
    let error = generate(
        fixture.0.join("other-benchmark.toml").to_str().unwrap(),
        &mut Vec::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("another sweep owns"), "{error}");
    drop(alias_owner);
}

#[cfg(unix)]
#[test]
fn top_level_manifest_symlink_keeps_its_own_output_namespace() {
    let fixture = Fixture::new();
    fixture.seed("", 1);
    std::os::unix::fs::symlink("sweep.toml", fixture.0.join("alias.toml")).unwrap();
    fs::create_dir_all(fixture.0.join("alias-results/000-0/measurements")).unwrap();
    fs::copy(
        fixture.0.join("sweep-results/000-0/measurements/run.json"),
        fixture.0.join("alias-results/000-0/measurements/run.json"),
    )
    .unwrap();
    let _owner = crate::sweep::lock::OutputLock::acquire(&fixture.0, "sweep").unwrap();
    let report = generate(
        fixture.0.join("alias.toml").to_str().unwrap(),
        &mut Vec::new(),
    )
    .unwrap();
    assert!(report.ends_with("results/alias-comparison/report.html"));
}
#[test]
fn dispatch_report_preserves_original_slots_and_refuses_a_forged_population_before_writes() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let config_text =
        "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency=4\n";
    fixture.write("0.toml", config_text);
    let config = toml::from_str::<crate::benchmark::BenchmarkConfig>(config_text).unwrap();
    let policy = crate::planning::resolve(&config).unwrap();
    let caps = crate::planning::EffectiveBudgets {
        execution_seconds: Some(900.0),
        ..Default::default()
    };
    let dispatch = crate::planning::Dispatch::new(4, 2).unwrap();
    let fingerprint = crate::planning::protocol_fingerprint_with_dispatch(
        &config,
        &policy,
        caps,
        4,
        None,
        Some(dispatch),
    )
    .unwrap();
    let relative = "sweep-results/000-0/measurements-concurrency-2/run.json";
    let mut run = json!({"schema_version":2,"status":"completed","elapsed_ms":12,"agent":"fixture","provider":"fixture","model":"fixture","workload":"bug-fix","module":crate::workloads::descriptor("bug-fix"),"replica_index":2,"execution_cohort":{"id":"a123","selected_replicas":4,"admitted_slots":[2,4],"dispatch":dispatch},"planning":policy,"effective_budgets":caps,"execution_plan_fingerprint":fingerprint});
    fixture.write(relative, run.to_string());
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let directory = fixture.0.join("results/sweep-comparison");
    let report: Value =
        serde_json::from_slice(&fs::read(directory.join("comparison.json")).unwrap()).unwrap();
    assert_eq!(
        report["benchmarks"][1]["execution_cohort"]["dispatch"],
        json!(dispatch)
    );
    assert_eq!(report["benchmarks"][1]["replica_index"], 2);
    let html = fs::read_to_string(directory.join("report.html")).unwrap();
    assert!(html.contains("Configured 4 · admitted 2 (partial admission)"));
    assert!(html.contains("slots 2, 4"));
    let prior = Fixture::snapshot(&directory);
    // Still valid shape/membership, but its retained hash belongs to P=2.
    run["execution_cohort"]["admitted_slots"] = json!([1, 2, 3, 4]);
    run["execution_cohort"]["dispatch"]["admitted"] = json!(4);
    fixture.write(relative, run.to_string());
    assert!(
        generate(manifest.to_str().unwrap(), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("runtime protocol fingerprint")
    );
    assert_eq!(Fixture::snapshot(&directory), prior);
    // Missing old descriptor is genuinely unknown, not a fabricated new P.
    run["execution_cohort"]
        .as_object_mut()
        .unwrap()
        .remove("dispatch");
    fixture.write(relative, run.to_string());
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let html = fs::read_to_string(directory.join("report.html")).unwrap();
    assert!(html.contains("Runtime population descriptor unknown (historical)"));
}
fn series(values: Value, timestamps: Option<Value>) -> TimeSeries {
    TimeSeries::parse(
        &values,
        timestamps.as_ref(),
        Path::new("fixture.json"),
        "metric",
    )
    .unwrap()
}

#[test]
fn unknown_historical_run_categories_remain_display_only_and_unscored() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 4);
    for index in 0..4 {
        let evaluation_kind = match index {
            2 => "adapter-smoke",
            3 => "invalid-baseline",
            _ => "historical-extension-grader",
        };
        fixture.write(&format!("sweep-results/{index:03}-{index}/measurements/run.json"), json!({
            "status":if index < 2 {"historical-extension-status"} else {"unscored"},"success":true,"process_success":true,
            "workload_success":true,"success_rate":1.0,"elapsed_ms":12,
            "workload_evaluation":evaluation_kind,
            "failure_kind":if index == 0 {Value::Null} else {json!("historical-extension-failure")},
            "failure_reason":if index == 0 {Value::Null} else {json!("original historical diagnostic")}
        }).to_string());
    }
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let comparison: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    let rows = comparison["benchmarks"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "historical-extension-status");
    for row in &rows[1..] {
        assert!(
            row["status"]
                .as_str()
                .unwrap()
                .contains("original historical diagnostic")
        );
    }
    for row in rows {
        assert!(row["metrics"].get("success_rate").is_none());
    }
    let html = fs::read_to_string(report).unwrap();
    assert!(html.contains("historical-extension-status"));
    assert!(html.contains("original historical diagnostic"));
}

#[test]
fn structured_native_rewards_keep_missing_coverage_and_plan_partitions_after_publication() {
    use crate::workloads::outcome::*;
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 3);
    let mut module = crate::workloads::contracts::ModuleDescriptor::local(
        "bug-fix",
        crate::workloads::KnownEvaluationKind::BundledFixture,
    );
    module.protocol.score_policy = "metric:native-points@2".into();
    let policy = MetricPolicy {
        id: "native-points".into(),
        version: 2,
        reward: "points".into(),
        aggregation: Aggregation::AttemptMean,
        rewards: BTreeMap::from([(
            "points".into(),
            RewardSpec {
                unit: "points".into(),
                range: Some([-10.0, 10.0]),
            },
        )]),
        missing_rewards: MissingRewardPolicy::Zero,
    };
    let selection = crate::workloads::TaskSelection {
        module: Some(module.clone()),
        workload: "bug-fix".into(),
        task_id: "bug-fix".into(),
        dataset: None,
        evaluation_kind: "bundled-fixture".into(),
        population_size: None,
    };
    let entries = (0..3).map(|index| {
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            &fs::read_to_string(fixture.0.join(format!("{index}.toml"))).unwrap()).unwrap();
        json!({"reference":format!("{index}.toml"),"configuration":config,"selection":selection})
    }).collect::<Vec<_>>();
    fixture.write(
        "sweep-results/plan.json",
        json!({"schema_version":1,"run_id":"abc123","entries":entries}).to_string(),
    );
    for (index, fingerprint, reward) in [(0, "plan-P", -2.0), (1, "plan-Q", 8.0)] {
        let task = crate::workloads::TaskIdentity::local("bug-fix", Some("bug-fix"));
        let outcome = BenchmarkOutcome {
            schema_version: 1,
            execution_success: Some(true),
            cleanup_confirmed: Some(true),
            evaluation: EvaluationOutcome::Completed {
                resolution: None,
                rewards: BTreeMap::from([("points".into(), reward)]),
                reason: None,
            },
            evidence: vec![PathBuf::from(format!("original/grade-{index}.json"))],
            policy: policy.clone(),
            lineage: EvaluationLineage {
                execution_attempt: task.attempt_id.clone(),
                evaluation_id: format!("evaluation-{index}"),
                supersedes: None,
            },
        };
        fixture.write(&format!("sweep-results/{index:03}-{index}/measurements/run.json"), json!({
            "schema_version":2,"agent":"fixture","provider":"fixture","model":"fixture","workload":"bug-fix",
            "task":task,"module":module,"benchmark_outcome":outcome,"execution_plan_fingerprint":fingerprint,
            "status":"completed","success":false,"process_success":true,"workload_success":null,
            "success_rate":0.0,"workload_evaluation":"bundled-fixture","workload_baseline_passed":false
        }).to_string());
    }
    fs::remove_file(fixture.0.join("sweep-results/002-2/measurements/run.json")).unwrap();
    fixture.write(
        "sweep-results/000-0/measurements/perfetto-state-timeline.svg",
        "<svg><title>native grade physical preview</title></svg>",
    );
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    // A second complete generation must retain the original evidence and grade,
    // while all JSON/HTML summaries share the prepared conservative denominator.
    let first = Fixture::snapshot(report.parent().unwrap());
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    assert_eq!(first, Fixture::snapshot(report.parent().unwrap()));
    let summary: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    let groups = summary["metric_groups"].as_object().unwrap();
    assert_eq!(groups.len(), 2);
    for fingerprint in ["plan-P", "plan-Q"] {
        let (key, group) = groups
            .iter()
            .find(|(key, _)| key.contains(fingerprint))
            .unwrap();
        assert!(key.contains("native-points"));
        assert_eq!(group["selected_attempts"], 2);
        assert_eq!(group["rewarded_attempts"], 1);
        assert_eq!(group["contributing_attempts"], 2);
        assert_eq!(group["complete"], false);
        assert_eq!(
            group["estimate"],
            if fingerprint == "plan-P" { -1.0 } else { 4.0 }
        );
    }
    assert_eq!(summary["evaluation_coverage"]["evaluated_attempts"], 2);
    for index in 0..2 {
        let row = &summary["benchmarks"][index];
        assert_eq!(
            row["benchmark_outcome"]["evaluation"]["resolution"],
            Value::Null
        );
        assert_eq!(
            row["benchmark_outcome"]["evidence"][0],
            format!("original/grade-{index}.json")
        );
        assert_eq!(row["evaluation_completed"], true);
        assert_eq!(row["status"], "completed");
        assert!(row["metrics"].get("success_rate").is_none());
        assert!(
            row["metrics"]
                .get("benchmark_rewards.native-points@2.points.points")
                .is_some()
        );
    }
    let html = fs::read_to_string(report).unwrap();
    assert!(html.contains("native-points v2") && html.contains("1/2"));
    assert!(html.contains("native grade physical preview"));
    assert!(html.contains("../../sweep-results/000-0/measurements/perfetto-state-timeline.svg"));
    assert!(!html.contains(".asb-report-stage-"));
}

#[test]
fn paired_samples_preserve_null_positions_reject_bad_timestamps_and_never_bridge_gaps() {
    let data = series(json!([1, null, 3]), Some(json!([0, 100, 200])));
    assert_eq!(data.values, vec![Some(1.0), None, Some(3.0)]);
    assert_eq!(data.render_indices(), Some(vec![0, 2]));
    for (values, times, reason) in [
        (json!([1, 2]), Some(json!([0])), "lengths differ"),
        (json!([1, 2]), Some(json!([10, 0])), "nondecreasing"),
        (json!([1, 2]), Some(json!([0, null])), "finite nonnegative"),
        (json!([1, "wrong"]), None, "finite numbers or null"),
        (json!([1, 2]), Some(json!("wrong")), "aligned array"),
    ] {
        let error = TimeSeries::parse(
            &values,
            times.as_ref(),
            Path::new("owned.json"),
            "my.metric",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(reason) && error.contains("owned.json") && error.contains("my.metric"),
            "{error}"
        );
    }
    let fixture = Fixture::new();
    let path = fixture.0.join("gaps.svg");
    write_series_svg(&path, "metric", &[("row".into(), &data)]).unwrap();
    let svg = fs::read_to_string(path).unwrap();
    assert_eq!(
        svg.matches("class=\"series\"").count(),
        2,
        "null separates two observed segments"
    );
    assert!(svg.contains("data-value=\"3\" data-sample-index=\"2\" data-timestamp=\"200\""));
    assert!(!svg.contains("data-timestamp=\"100\""));
    assert_eq!(
        series(json!([null, null]), None).rendering_metadata()["status"],
        "unavailable: no observed values"
    );
}

#[test]
fn bounded_graph_decimation_retains_full_data_extrema_endpoints_and_gap_fallback() {
    let mut values = (0..10000).map(|_| Some(10.0)).collect::<Vec<_>>();
    values[3333] = Some(-99.0);
    values[7777] = Some(999.0);
    let data = TimeSeries {
        values,
        timestamps_ms: Some((0..10000).map(|index| index as f64 * 2.0).collect()),
    };
    let indices = data.render_indices().unwrap();
    assert!(indices.len() <= series::MAX_RENDERED_POINTS);
    assert_eq!(indices.first(), Some(&0));
    assert_eq!(indices.last(), Some(&9999));
    assert!(indices.contains(&3333) && indices.contains(&7777));
    assert_eq!(data.values.len(), 10000);
    assert_eq!(data.rendering_metadata()["source_points"], 10000);
    assert_eq!(data.rendering_metadata()["minimum"], -99.0);
    let fragmented = TimeSeries {
        values: (0..6000)
            .map(|index| if index % 2 == 0 { Some(1.0) } else { None })
            .collect(),
        timestamps_ms: None,
    };
    assert!(fragmented.render_indices().is_none());
    assert!(
        fragmented.rendering_metadata()["status"]
            .as_str()
            .unwrap()
            .contains("too many gap")
    );
    let mut almost_full = TimeSeries {
        values: (0..1024)
            .flat_map(|_| [Some(10.0), Some(10.0), Some(10.0), None])
            .collect(),
        timestamps_ms: None,
    };
    almost_full.values[1] = Some(999.0);
    assert!(
        almost_full.render_indices().is_none(),
        "2048 gap endpoints plus an interior extreme cannot fit; do not silently discard the extreme"
    );
}

#[test]
fn report_publication_rejects_artifact_traversal_and_duplicate_names_before_writes() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("not-created/report");
    for names in [
        vec!["../outside"],
        vec!["/absolute"],
        vec!["nested/chart.svg"],
        vec!["report.html", "report.html"],
    ] {
        let error = publication::Publisher::begin(
            &destination,
            names.into_iter().map(String::from).collect(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("unique single filenames"));
        assert!(!destination.parent().unwrap().exists());
    }
}

#[test]
fn opened_once_sources_are_hard_bounded_under_growth_and_path_replacement() {
    let fixture = Fixture::new();
    let path = fixture.write("data.json", "{\"value\":1}");
    READ_HOOK.set(Some(Box::new(|_, path| {
        fs::rename(path, path.with_extension("original")).unwrap();
        fs::write(path, "{\"value\":2}").unwrap();
    })));
    assert_eq!(
        read_json(&path).unwrap()["value"],
        1,
        "read the original opened inode, not replacement path"
    );
    READ_HOOK.set(Some(Box::new(|_, path| {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
    })));
    assert!(
        read_json(&path)
            .unwrap_err()
            .to_string()
            .contains("exceeds 16 MiB")
    );
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn nonregular_sources_and_symlink_output_destinations_fail_without_blocking_or_writes() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let fixture = Fixture::new();
    let fifo = fixture.0.join("pipe.json");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: the FIFO is a new exact path beneath this fixture, not a host pipe.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(
        read_json(&fifo)
            .unwrap_err()
            .to_string()
            .contains("must be a regular file")
    );
    let real = fixture.0.join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("user-added"), "keep").unwrap();
    let link = fixture.0.join("link");
    symlink(&real, &link).unwrap();
    assert!(
        publication::Publisher::begin(&link, vec![])
            .err()
            .unwrap()
            .to_string()
            .contains("real directory")
    );
    assert_eq!(fs::read_to_string(real.join("user-added")).unwrap(), "keep");
}

#[test]
fn every_cross_row_invariant_and_population_failure_precedes_publication() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 2);
    for fault in ["row", "attempt", "cohort", "population"] {
        let mut loaded = source::load(manifest.to_str().unwrap()).unwrap();
        match fault {
            "row" => loaded.rows[1].id = loaded.rows[0].id,
            "attempt" => {
                for row in &mut loaded.rows {
                    row.task = Some(crate::workloads::TaskIdentity {
                        workload: "bug-fix".into(),
                        task_id: "task".into(),
                        attempt_id: "duplicate".into(),
                        dataset: None,
                    });
                }
            }
            "cohort" => {
                for (index, row) in loaded.rows.iter_mut().enumerate() {
                    row.execution_cohort = Some(crate::measurements::ExecutionCohort {
                        id: "aaa".into(),
                        selected_replicas: 2,
                        admitted_slots: vec![index + 1],
                        dispatch: None,
                    });
                }
            }
            _ => {
                for (index, row) in loaded.rows.iter_mut().enumerate() {
                    row.selection = Some(crate::workloads::TaskSelection {
                        module: None,
                        workload: "bug-fix".into(),
                        task_id: format!("task{index}"),
                        dataset: Some(crate::workloads::session::DatasetIdentity {
                            source: "fixture".into(),
                            revision: "fixed".into(),
                            split: "test".into(),
                        }),
                        evaluation_kind: "fixture".into(),
                        population_size: Some(index + 1),
                    });
                }
            }
        }
        let prepared = ValidatedReport::validate(loaded).and_then(PreparedReport::prepare);
        assert!(prepared.is_err(), "{fault}");
        assert!(
            !fixture.0.join("results").exists(),
            "no output files before global validation"
        );
    }
}

#[test]
fn failed_regeneration_preserves_entire_previous_report_and_raw_results() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let destination = fixture.0.join("results/sweep-comparison");
    fixture.write("results/sweep-comparison/user-added.txt", "preserve me");
    let old = Fixture::snapshot(&destination);
    let raw = fixture.0.join("sweep-results");
    let before = Fixture::snapshot(&raw);
    fixture.write(
        "sweep-results/000-0/measurements/bad.json",
        json!({"series":{"broken":[1,2]},"series_timestamps":{"broken":[0]}}).to_string(),
    );
    let error = generate(manifest.to_str().unwrap(), &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("timestamp and value lengths differ"),
        "{error}"
    );
    assert_eq!(Fixture::snapshot(&destination), old);
    fs::remove_file(raw.join("000-0/measurements/bad.json")).unwrap();
    assert_eq!(Fixture::snapshot(&raw), before);
    for malformed in [json!([0, 100]), json!("invalid"), Value::Null, json!(5)] {
        fixture.write(
            "sweep-results/000-0/measurements/bad.json",
            json!({"series":{"broken":[1,2]},"series_timestamps":malformed}).to_string(),
        );
        let error = generate(manifest.to_str().unwrap(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("object keyed by metric name") && error.contains("bad.json"),
            "{error}"
        );
        assert_eq!(Fixture::snapshot(&destination), old);
    }
}

#[test]
fn generated_line_coordinates_share_index_domain_and_preserve_constant_time_and_signed_values() {
    let fixture = Fixture::new();
    let short = series(json!([1, 2, 3]), None);
    let long = series(json!([1, 2, 3, 4, 5]), None);
    let path = fixture.0.join("axis.svg");
    write_series_svg(
        &path,
        "metric",
        &[("short".into(), &short), ("long".into(), &long)],
    )
    .unwrap();
    let svg = fs::read_to_string(&path).unwrap();
    let coordinates = |svg: &str, series_index: usize| -> Vec<(f64, f64)> {
        svg.split("<circle ")
            .skip(1)
            .filter(|tag| {
                tag.starts_with(&format!(
                    "class=\"series-point\" data-series-index=\"{series_index}\""
                ))
            })
            .map(|tag| {
                let attr = |name| {
                    tag.split(&format!(" {name}=\""))
                        .nth(1)
                        .unwrap()
                        .split('"')
                        .next()
                        .unwrap()
                        .parse::<f64>()
                        .unwrap()
                };
                (attr("cx"), attr("cy"))
            })
            .collect()
    };
    let short_xy = coordinates(&svg, 0);
    let long_xy = coordinates(&svg, 1);
    assert_eq!(short_xy[2].0, long_xy[2].0);
    assert!(short_xy[2].0 < long_xy[4].0);
    let timed = series(json!([-99, 0, 999]), Some(json!([100, 100, 100])));
    write_series_svg(&path, "signed", &[("signed".into(), &timed)]).unwrap();
    let svg = fs::read_to_string(&path).unwrap();
    let xy = coordinates(&svg, 0);
    assert!(
        xy.iter().all(|point| point.0 == xy[0].0),
        "equal timestamps must share one x coordinate"
    );
    assert!(
        xy[0].1 > xy[1].1 && xy[1].1 > xy[2].1,
        "negative observation must not be clamped to zero"
    );
    assert!(svg.contains(">-99</text>") && svg.contains(">999</text>"));
    assert!(svg.contains("data-value=\"-99\" data-sample-index=\"0\" data-timestamp=\"100\""));
}

#[test]
fn saved_resource_capacity_is_checked_before_replacing_a_published_report() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let config_text = "agent='pi'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n[planning.reservation]\ncpu_millis=1000\nmemory_mib=512\ndisk_mib=1024\n";
    fixture.write("0.toml", config_text);
    let config: crate::benchmark::BenchmarkConfig = toml::from_str(config_text).unwrap();
    let policy = crate::planning::resolve(&config).unwrap();
    let resources = json!({"cpu_millis":2000,"memory_mib":1024,"disk_mib":2048});
    fixture.write(
        "sweep-results/plan.json",
        json!({
            "schema_version":1,"run_id":"abc123","resources":resources,
            "entries":[{
                "reference":"0.toml","configuration":config,"policy":policy,
                "selection":{"module":null,"workload":"bug-fix","task_id":"bug-fix",
                    "dataset":null,"evaluation_kind":"local-structural","population_size":null}
            }]
        })
        .to_string(),
    );
    fs::remove_file(fixture.0.join("sweep-results/000-0/measurements/run.json")).unwrap();
    let manifest_text =
        "configs=['0.toml']\n[resources]\ncpu_millis=2000\nmemory_mib=1024\ndisk_mib=2048\n";
    fs::write(&manifest, manifest_text).unwrap();
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let before = Fixture::snapshot(report.parent().unwrap());
    for changed in [
        manifest_text.replace("cpu_millis=2000", "cpu_millis=3000"),
        "configs=['0.toml']\n".to_owned(),
    ] {
        fs::write(&manifest, changed).unwrap();
        let error = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resource capacity no longer matches saved plan")
        );
        assert_eq!(Fixture::snapshot(report.parent().unwrap()), before);
    }
    fs::write(&manifest, manifest_text).unwrap();
    generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    assert_eq!(Fixture::snapshot(report.parent().unwrap()), before);
}

#[test]
fn manifest_and_benchmark_snapshots_preserve_plan_identity_across_path_replacement() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let config_path = fixture.0.join("0.toml");
    let config: crate::benchmark::BenchmarkConfig =
        toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let selection = crate::workloads::TaskSelection {
        module: None,
        workload: "bug-fix".into(),
        task_id: "bug-fix".into(),
        dataset: None,
        evaluation_kind: "local-structural".into(),
        population_size: None,
    };
    let bytes=serde_json::to_vec(&json!({"schema_version":1,"run_id":"abc123","entries":[{"reference":"0.toml","configuration":config,"selection":selection}]})).unwrap();
    let plan_path = fixture.write("sweep-results/plan.json", &bytes);
    fs::remove_file(fixture.0.join("sweep-results/000-0/measurements/run.json")).unwrap();
    let sweep = crate::sweep::SweepFile {
        repeat: None,
        resources: None,
        configs: vec!["0.toml".into()],
    };
    let disk = crate::sweep::plan::SweepPlan::load(plan_path.parent().unwrap(), &fixture.0, &sweep)
        .unwrap()
        .unwrap();
    let decoded =
        crate::sweep::plan::SweepPlan::decode(&plan_path, &bytes, &fixture.0, &sweep, |_| {
            Ok(config.clone())
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(disk).unwrap(),
        serde_json::to_value(decoded).unwrap()
    );
    let mut invalid: Value = serde_json::from_slice(&bytes).unwrap();
    invalid["schema_version"] = json!(2);
    fs::write(&plan_path, invalid.to_string()).unwrap();
    let ordinary =
        crate::sweep::plan::SweepPlan::load(plan_path.parent().unwrap(), &fixture.0, &sweep)
            .unwrap_err()
            .to_string();
    let captured = crate::sweep::plan::SweepPlan::decode(
        &plan_path,
        invalid.to_string().as_bytes(),
        &fixture.0,
        &sweep,
        |_| Ok(config.clone()),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(ordinary, captured);
    fs::write(&plan_path, &bytes).unwrap();
    READ_HOOK.set(Some(Box::new(|_, _| {
        READ_HOOK.set(Some(Box::new(|_, path| {
            fs::rename(path, path.with_extension("original")).unwrap();
            fs::write(
                path,
                "agent='changed'\nprovider='changed'\nmodel='changed'\nworkload='bug-fix'\n",
            )
            .unwrap();
        })));
    })));
    let loaded = source::load(manifest.to_str().unwrap()).unwrap();
    assert_eq!(loaded.rows[0].agent, "fixture");
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("changed")
    );
    let prepared = PreparedReport::prepare(ValidatedReport::validate(loaded).unwrap()).unwrap();
    assert_eq!(prepared.data.rows[0].model, "fixture");
}

#[test]
fn benchmark_snapshot_limits_apply_to_actual_growth_and_max_plus_one() {
    let fixture = Fixture::new();
    let path = fixture.write("config.toml", vec![b' '; 64 * 1024]);
    assert_eq!(source::read_benchmark_text(&path).unwrap().len(), 64 * 1024);
    READ_HOOK.set(Some(Box::new(|_, path| {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(64 * 1024 + 1)
            .unwrap();
    })));
    assert!(
        source::read_benchmark_text(&path)
            .unwrap_err()
            .to_string()
            .contains("exceeds 64 KiB")
    );
    assert!(
        source::read_benchmark_text(&path)
            .unwrap_err()
            .to_string()
            .contains("exceeds 64 KiB")
    );
    let manifest = fixture.write("sweep.toml", "# small");
    fs::OpenOptions::new()
        .write(true)
        .open(&manifest)
        .unwrap()
        .set_len(MAX_BYTES + 1)
        .unwrap();
    assert!(
        source::load(manifest.to_str().unwrap())
            .err()
            .unwrap()
            .to_string()
            .contains("exceeds 16 MiB")
    );
    let standalone = fixture.write(
        "single.toml",
        format!(
            "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n#{}",
            " ".repeat(64 * 1024)
        ),
    );
    assert!(
        source::load(standalone.to_str().unwrap())
            .err()
            .unwrap()
            .to_string()
            .contains("exceeds 64 KiB")
    );
}

#[test]
fn standalone_identity_and_configuration_share_the_initial_opened_inode() {
    let fixture = Fixture::new();
    let path=fixture.write("single.toml","agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nlog_file='original/attempt.log'\n");
    fixture.write(
        ".asb/project.toml",
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspaces'\n",
    );
    READ_HOOK.set(Some(Box::new(|_,path|{
        fs::rename(path,path.with_extension("original")).unwrap();
        fs::write(path,"agent='changed'\nprovider='changed'\nmodel='changed'\nworkload='bug-fix'\nlog_file='changed/attempt.log'\n").unwrap();
    })));
    let loaded = source::load(path.to_str().unwrap()).unwrap();
    assert_eq!(loaded.rows[0].agent, "fixture");
    assert_eq!(loaded.rows[0].directory, fixture.0.join("original"));
    assert!(
        loaded.rows[0]
            .artifacts
            .iter()
            .all(|path| !path.contains("changed/attempt.log"))
    );
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains("agent='changed'")
    );
}

#[test]
fn retained_configuration_budget_is_bounded_before_loading_results_or_writing_outputs() {
    let fixture = Fixture::new();
    let mut config =
        "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n#".to_string();
    config.push_str(&" ".repeat(64 * 1024 - config.len()));
    let mut references = Vec::new();
    for index in 0..513 {
        let name = format!("{index}.toml");
        fixture.write(&name, &config);
        references.push(name);
    }
    let manifest = fixture.write(
        "sweep.toml",
        toml::to_string(&crate::sweep::SweepFile {
            repeat: None,
            resources: None,
            configs: references,
        })
        .unwrap(),
    );
    let error = source::load(manifest.to_str().unwrap())
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("configuration inputs exceed 32 MiB"),
        "{error}"
    );
    assert!(!fixture.0.join("results").exists());
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct RecoverableBackend {
    destination: PathBuf,
    stage: PathBuf,
    backup: std::cell::RefCell<Option<PathBuf>>,
    no_replace_unsupported: bool,
    fail_forward: bool,
    race_foreign: bool,
    fail_restore: bool,
    fail_preserve_sync: std::cell::Cell<bool>,
    fail_final_sync: bool,
    published: std::cell::Cell<bool>,
}
#[cfg(target_os = "linux")]
impl publication::RenameBackend for RecoverableBackend {
    fn rename(
        &self,
        from: &Path,
        to: &Path,
        operation: publication::RenameOperation,
    ) -> io::Result<()> {
        use publication::RenameOperation;
        if operation == RenameOperation::Exchange {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fixture filesystem has no exchange",
            ));
        }
        if self.no_replace_unsupported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fixture filesystem has no no-replace",
            ));
        }
        if from == self.destination {
            *self.backup.borrow_mut() = Some(to.to_path_buf());
        }
        if from == self.stage && to == self.destination {
            if self.race_foreign {
                fs::create_dir(to)?;
                fs::write(to.join("foreign-user-file"), "never overwrite")?;
            }
            if self.fail_forward {
                return Err(io::Error::other("fixture forward rename failed"));
            }
        }
        if self.backup.borrow().as_deref() == Some(from) && self.fail_restore {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture restore denied",
            ));
        }
        publication::native_rename(from, to, operation)?;
        if from == self.stage && to == self.destination {
            self.published.set(true);
        }
        Ok(())
    }
    fn synchronize(&self, parent: &Path) -> io::Result<()> {
        if self.backup.borrow().is_some()
            && !self.published.get()
            && self.fail_preserve_sync.replace(false)
        {
            return Err(io::Error::other("fixture backup durability failed"));
        }
        if self.published.get() && self.fail_final_sync {
            return Err(io::Error::other("fixture postcommit durability failed"));
        }
        fs::File::open(parent).and_then(|file| file.sync_all())
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_initial_publication_and_forced_recoverable_replacement_preserve_complete_generations() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("report");
    fixture.write("unrelated-sentinel", "untouched");
    let first = publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    fs::write(first.stage().join("report.html"), "first").unwrap();
    let initial_backend = RecoverableBackend {
        destination: destination.clone(),
        stage: first.stage().into(),
        ..Default::default()
    };
    let initial = first.commit_with(&initial_backend).unwrap();
    assert!(!initial.recoverable_fallback && initial.backup.is_none());
    fs::write(destination.join("user-added"), "keep user data").unwrap();
    let old = Fixture::snapshot(&destination);
    let second = publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    fs::write(second.stage().join("report.html"), "second").unwrap();
    let backend = RecoverableBackend {
        destination: destination.clone(),
        stage: second.stage().into(),
        ..Default::default()
    };
    let replaced = second.commit_with(&backend).unwrap();
    assert!(replaced.recoverable_fallback);
    assert_eq!(Fixture::snapshot(replaced.backup.as_ref().unwrap()), old);
    assert_eq!(
        fs::read_to_string(destination.join("report.html")).unwrap(),
        "second"
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("unrelated-sentinel")).unwrap(),
        "untouched"
    );
    assert!(
        replaced
            .confirmation_error("writer refused")
            .to_string()
            .contains("recoverable non-atomic replacement")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unsupported_no_replace_is_probed_only_on_owned_stage_before_moving_any_previous_report() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("report");
    fixture.write("report/report.html", "old");
    fixture.write("report/user-added", "keep");
    let old = Fixture::snapshot(&destination);
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    let stage = publisher.stage().to_path_buf();
    fs::write(stage.join("report.html"), "new").unwrap();
    let backend = RecoverableBackend {
        destination: destination.clone(),
        stage: stage.clone(),
        no_replace_unsupported: true,
        ..Default::default()
    };
    let error = publisher.commit_with(&backend).err().unwrap().to_string();
    assert!(
        error.contains("not published") && error.contains("no-replace"),
        "{error}"
    );
    assert_eq!(Fixture::snapshot(&destination), old);
    assert!(backend.backup.borrow().is_none());
    assert!(!stage.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn capability_probe_rejects_a_backend_that_overwrites_existing_private_targets() {
    struct UnsafeBackend;
    impl publication::RenameBackend for UnsafeBackend {
        fn rename(
            &self,
            from: &Path,
            to: &Path,
            operation: publication::RenameOperation,
        ) -> io::Result<()> {
            if operation == publication::RenameOperation::Exchange {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "no swap"));
            }
            // Intentionally wrong implementation, exercised ONLY inside the
            // disposable stage. The probe must reject it before user data moves.
            fs::rename(from, to)
        }
    }
    let fixture = Fixture::new();
    let destination = fixture.0.join("report");
    fixture.write("report/user-added", "preserve user bytes");
    let old = Fixture::snapshot(&destination);
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    let stage = publisher.stage().to_path_buf();
    fs::write(stage.join("report.html"), "new").unwrap();
    let error = publisher
        .commit_with(&UnsafeBackend)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("did not enforce no-replace"), "{error}");
    assert_eq!(Fixture::snapshot(&destination), old);
    assert!(!stage.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn recoverable_precommit_failures_restore_old_report_and_keep_original_reason() {
    for preserve_sync_failure in [false, true] {
        let fixture = Fixture::new();
        let destination = fixture.0.join("report");
        fixture.write("report/report.html", "old");
        fixture.write("report/user-added", "keep");
        let old = Fixture::snapshot(&destination);
        let publisher =
            publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
        let stage = publisher.stage().to_path_buf();
        fs::write(stage.join("report.html"), "new").unwrap();
        let backend = RecoverableBackend {
            destination: destination.clone(),
            stage: stage.clone(),
            fail_forward: !preserve_sync_failure,
            fail_preserve_sync: std::cell::Cell::new(preserve_sync_failure),
            ..Default::default()
        };
        let error = publisher.commit_with(&backend).err().unwrap().to_string();
        assert!(error.contains("previous report restored"), "{error}");
        assert!(
            error.contains(if preserve_sync_failure {
                "backup durability failed"
            } else {
                "forward rename failed"
            }),
            "{error}"
        );
        assert_eq!(Fixture::snapshot(&destination), old);
        assert!(!stage.exists());
        assert!(!backend.backup.borrow().as_ref().unwrap().exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rollback_failure_retains_old_backup_without_overwriting_racing_foreign_destination() {
    for foreign_race in [false, true] {
        let fixture = Fixture::new();
        let destination = fixture.0.join("report");
        fixture.write("report/report.html", "old");
        fixture.write("report/user-added", "keep");
        fixture.write("unrelated", "sentinel");
        let old = Fixture::snapshot(&destination);
        let publisher =
            publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
        let stage = publisher.stage().to_path_buf();
        fs::write(stage.join("report.html"), "new").unwrap();
        let backend = RecoverableBackend {
            destination: destination.clone(),
            stage: stage.clone(),
            fail_forward: !foreign_race,
            fail_restore: !foreign_race,
            race_foreign: foreign_race,
            ..Default::default()
        };
        let error = publisher.commit_with(&backend).err().unwrap().to_string();
        let backup = backend.backup.borrow();
        let backup = backup.as_ref().unwrap();
        assert!(
            error.contains("recoverable backup")
                && error.contains("automatic restore")
                && error.contains("mv --"),
            "{error}"
        );
        assert!(error.contains(backup.to_str().unwrap()));
        assert_eq!(Fixture::snapshot(backup), old);
        assert!(!stage.exists());
        if foreign_race {
            assert_eq!(
                fs::read_to_string(destination.join("foreign-user-file")).unwrap(),
                "never overwrite"
            );
            assert!(!destination.join("report.html").exists());
        } else {
            assert!(!destination.exists());
        }
        assert_eq!(
            fs::read_to_string(fixture.0.join("unrelated")).unwrap(),
            "sentinel"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn recoverable_commit_sync_failure_reports_new_publication_and_retained_old_backup() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("report");
    fixture.write("report/report.html", "old");
    let old = Fixture::snapshot(&destination);
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    let stage = publisher.stage().to_path_buf();
    fs::write(stage.join("report.html"), "new").unwrap();
    let backend = RecoverableBackend {
        destination: destination.clone(),
        stage,
        fail_final_sync: true,
        ..Default::default()
    };
    let error = publisher.commit_with(&backend).err().unwrap().to_string();
    assert!(
        error.contains("Report was published") && error.contains("durability is unconfirmed"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(destination.join("report.html")).unwrap(),
        "new"
    );
    assert_eq!(
        Fixture::snapshot(backend.backup.borrow().as_ref().unwrap()),
        old
    );
}

#[cfg(target_os = "linux")]
#[test]
fn owned_publication_excludes_writers_preserves_backups_and_does_not_hide_confirmation_failure() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("reports/final");
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    fs::write(publisher.stage().join("report.html"), "first").unwrap();
    assert!(
        publication::Publisher::begin(&destination, vec![])
            .err()
            .unwrap()
            .to_string()
            .contains("another comparison")
    );
    publisher.commit().unwrap();
    fs::write(destination.join("user-added"), "keep").unwrap();
    let old = Fixture::snapshot(&destination);
    let failed = publication::Publisher::begin(&destination, vec!["missing.html".into()]).unwrap();
    assert!(failed.commit().is_err());
    assert_eq!(Fixture::snapshot(&destination), old);
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    fs::write(publisher.stage().join("report.html"), "second").unwrap();
    let published = publisher.commit().unwrap();
    assert_eq!(Fixture::snapshot(published.backup.as_ref().unwrap()), old);
    assert_eq!(fs::read_to_string(&published.report).unwrap(), "second");
    let manifest = fixture.seed("input", 1);
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture terminal closed",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let error = generate(manifest.to_str().unwrap(), &mut Broken)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Report was published") && error.contains("confirmation output failed"),
        "{error}"
    );
    assert!(
        fixture
            .0
            .join("input/results/sweep-comparison/report.html")
            .is_file()
    );
}

#[cfg(unix)]
#[test]
fn symlink_parent_components_keep_physical_previews_and_logical_report_paths_separate() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.seed("actual", 1);
    fs::create_dir(fixture.0.join("actual/child")).unwrap();
    symlink(fixture.0.join("actual/child"), fixture.0.join("link")).unwrap();
    fixture.write(
        "actual/sweep-results/000-0/measurements/perfetto-state-timeline.svg",
        "<svg><title>correct physical preview</title></svg>",
    );
    let manifest = fixture.0.join("link/../sweep.toml");
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    assert_eq!(
        report,
        fixture
            .0
            .join("link/../results/sweep-comparison/report.html")
    );
    let html = fs::read_to_string(&report).unwrap();
    assert!(html.contains("correct physical preview"));
    assert!(html.contains("../../sweep-results/000-0/measurements/perfetto-state-timeline.svg"));
    assert!(!html.contains(".asb-report-backup-"));
    let summary: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    assert_eq!(
        summary["comparison_directory"],
        fixture
            .0
            .join("link/../results/sweep-comparison")
            .to_string_lossy()
            .as_ref()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn post_commit_durability_failure_reports_published_state_and_retains_previous_generation() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("reports/final");
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("report.html"), "old report").unwrap();
    fs::write(destination.join("user-file"), "old user file").unwrap();
    let old = Fixture::snapshot(&destination);
    let publisher =
        publication::Publisher::begin(&destination, vec!["report.html".into()]).unwrap();
    let backup = publisher.stage().to_owned();
    fs::write(publisher.stage().join("report.html"), "new report").unwrap();
    FAIL_SYNC.set(true);
    let error = publisher.commit().err().unwrap().to_string();
    FAIL_SYNC.set(false);
    assert!(
        error.contains("Report was published") && error.contains("durability is unconfirmed"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(destination.join("report.html")).unwrap(),
        "new report"
    );
    assert_eq!(Fixture::snapshot(&backup), old);
}

#[test]
fn generated_svg_and_embedded_html_script_execute_observed_hover_zoom_pan_and_modal_actions() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    fixture.write("sweep-results/000-0/measurements/profile.json",json!({"metrics":{"signed":-17.25},"series":{"profile.latency_ms":[1,null,3]},"series_timestamps":{"profile.latency_ms":[0,100,200]}}).to_string());
    let raw = Fixture::snapshot(&fixture.0.join("sweep-results"));
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let graph = report.with_file_name("series-profile.latency_ms.svg");
    let node = crate::tools::host_executable(Path::new("node"))
        .expect("Node is required by offline report tests");
    let output = std::process::Command::new(node)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/report_interactions_fixture.js"))
        .arg(&report)
        .arg(&graph)
        .arg(report.with_file_name("metric-profile.signed.svg"))
        .env_clear()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("actual generated report interaction checks passed")
    );
    assert_eq!(Fixture::snapshot(&fixture.0.join("sweep-results")), raw);
}

#[test]
fn signed_scalar_graphs_retain_observations_and_finite_zero_based_geometry() {
    let fixture = Fixture::new();
    let cases = [
        ("negative", vec![-100.0, -1.0, 0.0]),
        ("mixed", vec![-100.0, -1.0, 0.0, 1.0, 100.0]),
        ("extreme", vec![-f64::MAX, 0.0, f64::MAX]),
        ("tiny", vec![-f64::from_bits(1), 0.0, f64::from_bits(1)]),
    ];
    let manifest = fixture.seed("", 5);
    for index in 0..5 {
        let metrics = cases
            .iter()
            .filter_map(|(name, values)| {
                values
                    .get(index)
                    .map(|value| ((*name).to_owned(), json!(value)))
            })
            .collect::<serde_json::Map<_, _>>();
        fixture.write(
            &format!("sweep-results/{index:03}-{index}/measurements/profile.json"),
            json!({"metrics":metrics}).to_string(),
        );
    }
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let html = fs::read_to_string(&report).unwrap();
    let summary: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    fn attribute(tag: &str, name: &str) -> f64 {
        tag.split_once(&format!("{name}=\""))
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
    for (name, values) in cases {
        let graph = format!("metric-profile.{name}.svg");
        let svg = fs::read_to_string(report.with_file_name(&graph)).unwrap();
        assert!(html.contains(&format!("data-chart=\"profile.{name}\"")) || html.contains(&graph));
        // Inline HTML contains the same generated bars used by the standalone SVG.
        assert!(
            html.contains(
                &svg[svg.find("<g data-benchmark=").unwrap()..svg.rfind("</svg>").unwrap()]
            )
        );
        let baseline_tag = svg
            .split("class=\"zero-baseline\"")
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        let baseline = attribute(baseline_tag, "y1");
        let bars = svg
            .split("class=\"bar\"")
            .skip(1)
            .map(|tail| tail.split('>').next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(bars.len(), values.len());
        for (index, (bar, original)) in bars.iter().zip(values).enumerate() {
            let value = attribute(bar, "data-value");
            assert_eq!(value, original);
            assert_eq!(
                summary["benchmarks"][index]["metrics"][format!("profile.{name}")],
                json!(original)
            );
            let y = attribute(bar, "y");
            let height = attribute(bar, "height");
            assert!(y.is_finite() && height.is_finite());
            assert!(y >= PLOT_TOP && y + height <= PLOT_BOTTOM + 0.1 && height >= 0.0);
            if value < 0.0 {
                assert!((y - baseline).abs() < 0.1 && height > 0.0);
            } else if value > 0.0 {
                assert!((y + height - baseline).abs() < 0.1 && height > 0.0);
            } else {
                assert_eq!(height, 0.0);
            }
        }
        assert!(svg.contains("data-placement=\"inside\""));
        if name == "negative" || name == "mixed" {
            assert!(svg.contains("data-placement=\"below\""));
        }
    }
}

#[test]
fn full_series_exports_survive_bounded_rendering_and_timed_indexed_axes_are_separate() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 2);
    let mut values = (0..5000).map(|_| 10).collect::<Vec<_>>();
    values[1234] = 999;
    values[4321] = 1;
    fixture.write("sweep-results/000-0/measurements/profile.json",json!({"series":{"profile.latency_ms":values},"series_timestamps":{"profile.latency_ms":(0..5000).map(|index|index*2).collect::<Vec<_>>()}}).to_string());
    fixture.write(
        "sweep-results/001-1/measurements/profile.json",
        json!({"series":{"profile.latency_ms":[5,6,7]}}).to_string(),
    );
    let raw = Fixture::snapshot(&fixture.0.join("sweep-results"));
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let summary: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    assert_eq!(
        summary["benchmarks"][0]["series"]["profile.latency_ms"]
            .as_array()
            .unwrap()
            .len(),
        5000
    );
    assert_eq!(
        summary["benchmarks"][0]["series_timestamps"]["profile.latency_ms"][4999],
        9998.0
    );
    assert_eq!(
        summary["benchmarks"][0]["series_rendering"]["profile.latency_ms"]["source_points"],
        5000
    );
    let timed = fs::read_to_string(report.with_file_name("series-profile.latency_ms.svg")).unwrap();
    assert!(timed.matches("class=\"series-point\"").count() <= series::MAX_RENDERED_POINTS);
    assert!(timed.contains("data-sample-index=\"4999\" data-timestamp=\"9998\""));
    assert!(timed.contains("data-value=\"999\"") && timed.contains("data-value=\"1\""));
    let indexed =
        fs::read_to_string(report.with_file_name("series-index-profile.latency_ms.svg")).unwrap();
    assert!(indexed.contains(">sample (index)</text>"));
    assert!(!indexed.contains("data-timestamp="));
    assert_eq!(Fixture::snapshot(&fixture.0.join("sweep-results")), raw);
}

#[test]
fn unknown_gap_overload_is_explicitly_unavailable_without_truncating_source_data() {
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    let values = (0..6000)
        .map(|index| if index % 2 == 0 { Some(1) } else { None })
        .collect::<Vec<_>>();
    fixture.write(
        "sweep-results/000-0/measurements/profile.json",
        json!({"series":{"profile.latency_ms":values}}).to_string(),
    );
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let summary: Value =
        serde_json::from_slice(&fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    assert_eq!(
        summary["benchmarks"][0]["series"]["profile.latency_ms"]
            .as_array()
            .unwrap()
            .len(),
        6000
    );
    assert!(
        summary["benchmarks"][0]["series_rendering"]["profile.latency_ms"]["status"]
            .as_str()
            .unwrap()
            .contains("too many gap")
    );
    let svg = fs::read_to_string(report.with_file_name("series-profile.latency_ms.svg")).unwrap();
    assert!(svg.contains("graph unavailable"));
    assert!(!svg.contains("class=\"series\""));
}

#[test]
#[ignore = "requires an explicitly selected local headless Chromium; no download"]
fn real_headless_browser_qualifies_generated_report_interactions() {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    struct OwnedBrowser {
        child: Option<Child>,
        reaper: crate::process_cleanup::OwnedReaper,
    }
    impl Drop for OwnedBrowser {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                self.reaper.adopt(child);
            }
        }
    }
    let browser = std::env::var_os("ASB_TEST_CHROMIUM")
        .expect("set ASB_TEST_CHROMIUM to an existing Chromium executable");
    let fixture = Fixture::new();
    let manifest = fixture.seed("", 1);
    fixture.write("sweep-results/000-0/measurements/profile.json",json!({"series":{"profile.latency_ms":[1,null,3]},"series_timestamps":{"profile.latency_ms":[0,100,200]}}).to_string());
    let report = generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    let html = fs::read_to_string(&report).unwrap().replace(
        "</body>",
        &format!(
            "<script>{}</script></body>",
            include_str!("../report_browser_fixture.js")
        ),
    );
    fs::write(&report, html).unwrap();
    let output_path = fixture.0.join("browser-output.html");
    let errors_path = fixture.0.join("browser-errors.log");
    let reaper = crate::process_cleanup::OwnedReaper::new("report-browser-fixture").unwrap();
    let child = Command::new(browser)
        .args([
            "--headless",
            "--no-sandbox",
            "--single-process",
            "--no-zygote",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--metrics-recording-only",
            "--host-resolver-rules=MAP * ~NOTFOUND",
            "--dump-dom",
        ])
        .arg(format!(
            "--user-data-dir={}",
            fixture.0.join("browser-profile").display()
        ))
        .arg(format!("file://{}", report.display()))
        .env_clear()
        .env("HOME", &fixture.0)
        .env("PATH", "/usr/bin:/bin:/snap/bin")
        .env("TMPDIR", &fixture.0)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output_path).unwrap())
        .stderr(fs::File::create(&errors_path).unwrap())
        .spawn()
        .unwrap();
    let mut owned = OwnedBrowser {
        child: Some(child),
        reaper,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = owned.child.as_mut().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "owned browser did not finish within20seconds"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let output = fs::read_to_string(&output_path).unwrap();
    let errors = fs::read_to_string(&errors_path).unwrap();
    assert!(status.success(), "{status}: {errors}");
    assert!(
        output.contains("data-asb-browser-test=\"passed\""),
        "qualification marker absent: {errors}\n{}",
        output.chars().take(2000).collect::<String>()
    );
}
