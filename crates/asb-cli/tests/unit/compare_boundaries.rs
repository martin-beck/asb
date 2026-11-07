//! Offline bounded-artifact and report-input validation, never starts runs.
use super::*;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asb-compare-boundaries-{}",
            crate::workloads::TaskIdentity::local("boundary", None).attempt_id
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn compare_inside_retained_sweep_does_not_create_inner_sweep_lock() {
    let root = Fixture::new();
    root.write(
        ".asb/project.toml",
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    );
    let input = root.write(
        "outer-results/000-one/benchmark.toml",
        "agent='pi'\nprovider='openrouter'\nmodel='m'\nworkload='bug-fix'\n",
    );
    let _ = generate_at(input.to_str().unwrap(), &root.0, &mut Vec::new(), false);
    assert!(
        !input
            .parent()
            .unwrap()
            .join(".benchmark-asb-sweep.lock")
            .exists()
    );
    assert!(root.0.join(".outer-asb-sweep.lock").exists());
}

#[test]
fn structured_reward_without_execution_task_is_rejected() {
    use crate::workloads::outcome::*;
    let root = Fixture::new();
    root.write("sweep.toml", "configs=['one.toml']\n");
    root.write(
        "one.toml",
        "agent='cline'\nprovider='local'\nmodel='m'\nworkload='bug-fix'\n",
    );
    let record = json!({
        "schema_version":2,"status":"completed","success":true,
        "process_success":true,"workload_success":true,
        "workload_evaluation":"bundled-fixture","workload_baseline_passed":false,
        "benchmark_outcome":BenchmarkOutcome {
            schema_version:1,execution_success:Some(true),
            evaluation:EvaluationOutcome::binary(true),cleanup_confirmed:Some(true),
            evidence:vec![],policy:MetricPolicy::default(),
            lineage:EvaluationLineage {execution_attempt:"unbound".into(),evaluation_id:"eval".into(),supersedes:None},
        }
    });
    root.write(
        "sweep-results/000-one/measurements/run.json",
        record.to_string(),
    );
    let error = run(root.0.join("sweep.toml").to_str().unwrap(), &mut Vec::new()).unwrap_err();
    assert!(
        error.to_string().contains("no execution task identity"),
        "{error}"
    );
}

#[test]
fn embedded_svg_distinguishes_missing_oversized_encoding_and_non_svg_documents() {
    let root = Fixture::new();
    assert!(
        inline_svg_artifact(&root.0, "missing.svg", "fixture", "chart")
            .unwrap_err()
            .to_string()
            .contains("cannot read embedded SVG artifact missing.svg")
    );
    root.write("large.svg", b"");
    fs::OpenOptions::new()
        .write(true)
        .open(root.0.join("large.svg"))
        .unwrap()
        .set_len(MAX_BYTES + 1)
        .unwrap();
    assert!(
        inline_svg_artifact(&root.0, "large.svg", "fixture", "chart")
            .unwrap_err()
            .to_string()
            .contains("exceeds 16 MiB")
    );
    root.write("binary.svg", [0xff]);
    assert!(
        inline_svg_artifact(&root.0, "binary.svg", "fixture", "chart")
            .unwrap_err()
            .to_string()
            .contains("cannot read embedded SVG artifact binary.svg")
    );
    root.write("invalid.svg", "not a chart");
    assert!(
        inline_svg_artifact(&root.0, "invalid.svg", "fixture", "chart")
            .unwrap_err()
            .to_string()
            .contains("does not contain an SVG document")
    );
    root.write("valid.svg", "<?xml version='1.0'?><svg/>");
    let html = inline_svg_artifact(&root.0, "valid.svg", "fixture", "A & B").unwrap();
    assert!(html.contains("A &amp; B"));
    assert!(html.contains("<svg/>"));
}

#[test]
fn references_reject_escape_wrong_extension_and_missing_file_without_creating_it() {
    let root = Fixture::new();
    for (reference, reason) in [
        ("../outside.toml", "unsafe benchmark path"),
        ("/tmp/outside.toml", "unsafe benchmark path"),
        ("config.json", "not a .toml benchmark"),
        ("missing.toml", "references missing benchmark"),
    ] {
        assert!(
            checked_benchmark_reference(&root.0, reference, "fixture-sweep")
                .unwrap_err()
                .to_string()
                .contains(reason)
        );
    }
    let path = root.write("nested/valid.toml", "agent='fixture'\n");
    assert_eq!(
        checked_benchmark_reference(&root.0, "nested/valid.toml", "fixture-sweep").unwrap(),
        path
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn benchmark_identity_reports_parse_missing_fields_and_invalid_concurrency_precisely() {
    let root = Fixture::new();
    let path = root.write("benchmark.toml", "agent=[");
    assert!(
        benchmark_identity(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot parse benchmark")
    );
    fs::write(&path, "agent='fixture'\n").unwrap();
    assert!(
        benchmark_identity(&path)
            .unwrap_err()
            .to_string()
            .contains("missing string field `workload`")
    );
    let above_limit = (crate::benchmark::MAX_CONCURRENCY + 1).to_string();
    for value in ["0", above_limit.as_str(), "-1", "1.5", "'2'"] {
        fs::write(&path,format!("agent='fixture'\nworkload='fixture'\nprovider='fixture'\nmodel='fixture'\nconcurrency={value}\n")).unwrap();
        assert!(
            benchmark_identity(&path)
                .unwrap_err()
                .to_string()
                .contains(&format!(
                    "concurrency must be an integer from 1 through {}",
                    crate::benchmark::MAX_CONCURRENCY
                ))
        );
    }
    fs::write(
        &path,
        "agent='fixture'\nworkload='fixture'\nprovider='fixture'\nmodel='fixture'\n",
    )
    .unwrap();
    assert_eq!(benchmark_identity(&path).unwrap().4, 1);
}

#[test]
fn bounded_measurement_json_rejects_sparse_oversize_and_non_utf8() {
    let root = Fixture::new();
    let path = root.write("data.json", b"");
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_BYTES + 1)
        .unwrap();
    assert!(
        read_json(&path)
            .unwrap_err()
            .to_string()
            .contains("measurement file")
    );
    fs::write(&path, [0xff]).unwrap();
    assert!(
        read_json(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot read")
    );
    fs::write(&path, "{").unwrap();
    assert!(
        read_json(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid measurement JSON")
    );
}

#[test]
fn artifact_traversal_is_depth_and_count_bounded() {
    let root = Fixture::new();
    let deep = root.write("deep/a/b/c/d/e/too-deep.txt", "not collected");
    let visible = root.write("deep/a/b/visible.txt", "visible");
    let mut files = Vec::new();
    collect_artifacts(&root.0.join("deep"), 0, &mut files);
    assert!(files.contains(&visible));
    assert!(!files.contains(&deep));
    for index in 0..260 {
        root.write(&format!("wide/{index}.txt"), "fixture");
    }
    let mut files = Vec::new();
    collect_artifacts(&root.0.join("wide"), 0, &mut files);
    assert_eq!(files.len(), 256);
    let before = files.clone();
    collect_artifacts(&root.0, 0, &mut files);
    assert_eq!(files, before);
    assert!(relative_artifact(&root.0, &root.0.join("absent")).is_none());
}
