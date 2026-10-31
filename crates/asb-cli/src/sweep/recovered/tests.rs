//! Grade-import fail-closed contracts; no evaluator, provider or agent starts.
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asb-import-coverage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn entry(&self) -> PlannedBenchmark {
        serde_json::from_value(json!({
            "reference":"benchmark.toml",
            "configuration":{"agent":"pi","provider":"openrouter","model":"fixture",
                             "workload":"fixture","concurrency":2},
            "selection":{"workload":"fixture","task_id":"case-1","dataset":null,
                         "evaluation_kind":"unscored","population_size":null}
        }))
        .unwrap()
    }
    fn imported(&self) -> Value {
        json!({"schema_version":1,"scope":SCOPE,"configuration":0,
            "replica":{"index":1,"total":2},
            "reference":{"schema_version":1,"preparation_id":"a1","workload":"fixture",
                "task_id":"case-1","dataset":{"source":"fixture","revision":"pinned","split":"test"},
                "artifacts":self.0.join("never-inspected")},
            "binding":{},"report":{}})
    }
    fn write_private(&self, path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn report_import_removes_private_evidence_without_fabricating_execution_metrics() {
    let fixture = Fixture::new();
    let source = json!({"schema_version":1,"evidence":["private-path"],
        "workload_success":true,"task":{"attempt_id":"owned-attempt"}});
    let report = reporting_summary(&fixture.entry(), &source);
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["agent"], "pi");
    assert_eq!(report["provider"], "openrouter");
    assert_eq!(report["workload_success"], true);
    assert_eq!(
        report["legacy_evaluation_provenance"]["execution_attempt"],
        "owned-attempt"
    );
    assert!(report["legacy_evaluation_provenance"]["evaluation_id"].is_null());
    assert!(report["legacy_evaluation_provenance"]["supersedes"].is_null());
    assert_eq!(
        report["legacy_evaluation_provenance"]["evidence_manifest_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(report.get("module").is_none());
    assert!(report.get("execution_plan_fingerprint").is_none());
    assert!(report.get("benchmark_outcome").is_none());
    assert_eq!(
        report,
        reporting_summary(&fixture.entry(), &source),
        "inspection must not mint new evaluation IDs"
    );
    for field in [
        "evidence",
        "elapsed_ms",
        "token_usage",
        "cohort",
        "process_success",
    ] {
        assert!(report.get(field).is_none(), "must not invent {field}");
    }
    assert!(
        source.get("evidence").is_some(),
        "source must remain untouched"
    );
    assert!(report["process_termination"].is_null());
    let mut extended = source.clone();
    extended["process_termination"] = json!({"kind":"signal","signal":15});
    extended["process_termination_diagnostic"] = json!("unavailable");
    let grade_only = reporting_summary(&fixture.entry(), &extended);
    assert!(grade_only["process_termination"].is_null());
    assert!(grade_only.get("process_termination_diagnostic").is_none());
    assert_eq!(extended["process_termination"]["signal"], 15);
}

#[test]
fn duplicate_real_attempt_ids_are_rejected_but_unknown_attempts_are_not_invented() {
    let mut claimed = BTreeSet::new();
    claim_attempt(&mut claimed, &json!({})).unwrap();
    claim_attempt(&mut claimed, &json!({"task":{"attempt_id":null}})).unwrap();
    let report = json!({"task":{"attempt_id":"actual-attempt"}});
    claim_attempt(&mut claimed, &report).unwrap();
    assert!(
        claim_attempt(&mut claimed, &report)
            .unwrap_err()
            .to_string()
            .contains("multiple")
    );
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        measurement(Path::new("root"), 1, 1),
        Path::new("root/measurements/run.json")
    );
    assert_eq!(
        measurement(Path::new("root"), 2, 3),
        Path::new("root/measurements-concurrency-2/run.json")
    );
}

#[test]
fn absent_imports_are_empty_but_files_and_symlinks_are_rejected() {
    let fixture = Fixture::new();
    let directory = fixture.0.join(".asb-imported-grades");
    assert!(
        load(&fixture.0, &fixture.0, 0, &fixture.entry())
            .unwrap()
            .is_empty()
    );
    std::fs::write(&directory, b"not a directory").unwrap();
    assert!(
        load(&fixture.0, &fixture.0, 0, &fixture.entry())
            .unwrap_err()
            .to_string()
            .contains("real directory")
    );
    std::fs::remove_file(&directory).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&fixture.0, &directory).unwrap();
        assert!(load(&fixture.0, &fixture.0, 0, &fixture.entry()).is_err());
    }
}

#[test]
fn temporary_imports_are_ignored_but_malformed_identity_slots_fail_before_inspection() {
    let fixture = Fixture::new();
    let directory = fixture.0.join(".asb-imported-grades");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join(".pending.tmp"), b"unfinished publication").unwrap();
    assert!(
        load(&fixture.0, &fixture.0, 0, &fixture.entry())
            .unwrap()
            .is_empty()
    );
    for (field, value) in [
        ("schema_version", json!(2)),
        ("scope", json!("execution")),
        ("configuration", json!(1)),
        ("replica", json!({"index":0,"total":2})),
        ("replica", json!({"index":1,"total":3})),
    ] {
        let mut record = fixture.imported();
        record[field] = value;
        fixture.write_private(&directory.join("001.json"), &record);
        let error = load(&fixture.0, &fixture.0, 0, &fixture.entry())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("invalid scope, configuration or slot"),
            "{error}"
        );
    }
    fixture.write_private(&directory.join("001.json"), &fixture.imported());
    std::fs::rename(directory.join("001.json"), directory.join("wrong.json")).unwrap();
    assert!(
        load(&fixture.0, &fixture.0, 0, &fixture.entry())
            .unwrap_err()
            .to_string()
            .contains("slot")
    );
    assert!(!fixture.0.join("never-inspected").exists());
}

#[test]
fn invalid_or_unbound_original_results_cannot_be_imported() {
    let fixture = Fixture::new();
    let manifest = super::super::SweepFile {
        repeat: None,
        resources: None,
        configs: vec![],
    };
    let run = |output: &mut Vec<u8>| {
        import(
            &fixture.0.join("sweep.toml"),
            &manifest,
            &fixture.0,
            "sweep",
            false,
            output,
        )
    };
    assert!(
        run(&mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("real directory")
    );
    std::fs::create_dir(fixture.0.join("sweep-results")).unwrap();
    assert!(
        run(&mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("plan is missing")
    );
    let replica = Identity { index: 1, total: 2 };
    assert!(original_binding(&fixture.0, 0, replica, &fixture.0, &fixture.entry()).is_err());
}

#[test]
fn no_grade_import_reports_unknown_metrics_in_both_output_formats() {
    let fixture = Fixture::new();
    let results = fixture.0.join("sweep-results");
    std::fs::create_dir(&results).unwrap();
    fixture.write_private(
        &results.join("plan.json"),
        &json!({
            "schema_version":1,"run_id":"a1","execution_mode":"live","entries":[]
        }),
    );
    let manifest = super::super::SweepFile {
        repeat: None,
        resources: None,
        configs: vec![],
    };
    for json_output in [true, false] {
        let mut output = Vec::new();
        import(
            &fixture.0.join("sweep.toml"),
            &manifest,
            &fixture.0,
            "sweep",
            json_output,
            &mut output,
        )
        .unwrap();
        if json_output {
            let report: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(report["imported"], 0);
            assert_eq!(report["execution_recovered"], false);
            assert_eq!(report["resume_receipts_created"], false);
        } else {
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("Execution metrics remain unknown"));
            assert!(text.contains("No agents/evaluators ran"));
        }
    }
    assert!(
        !results.join(".checkpoint").exists(),
        "import must not make execution receipts"
    );
}
