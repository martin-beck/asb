use super::writer::{GROUP_ROWS, MAX_ROW_BYTES, Record};
use super::*;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::RowAccessor;
#[test]
fn optional_export_uses_regenerated_evidence_and_preserves_report_on_failure() {
    let root =
        crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-columnar-report-").unwrap();
    fs::write(root.join("sweep.toml"), "configs=['one.toml']\n").unwrap();
    fs::write(
        root.join("one.toml"),
        "agent='codex'\nworkload='bug-fix'\nprovider='local'\nmodel='fixture'\n",
    )
    .unwrap();
    let measured = root.join("sweep-results/000-one/measurements");
    fs::create_dir_all(&measured).unwrap();
    fs::write(
        measured.join("run.json"),
        r#"{"status":"completed","elapsed_ms":12}"#,
    )
    .unwrap();
    let components = json!({"schema_version":1,"components":{"agent.execute":{"wall_ms":12,"cpu_user_ms":0.0,"cpu_system_ms":2.0,"cpu_accounting_source":"cgroup-v2"}}});
    fs::write(measured.join("components.json"), components.to_string()).unwrap();
    let counter = |n| json!({"observed_integer_count":n,"observed_rows":1,"unavailable_rows":0,"unknown_quality_rows":0,"known_event_runtime_ns":1000,"min_running_pct":100,"max_running_pct":100});
    for (name, value) in [
        (
            "perf-stat-plan",
            json!({"schema_version":1,"scope":"agent-process-tree","root":{"pid":42,"start_time_ticks":7},"excluded_controller_pid":1,"inherit":true,"system_wide":false,"counter_mode":"unscaled-observed","groups":["{cycles,instructions}"]}),
        ),
        (
            "perf-stat-quality",
            json!({"schema_version":1,"counter_mode":"unscaled-observed","events":{"cycles":counter(u64::MAX),"instructions":counter(7)}}),
        ),
        ("perf-stat", json!({"status":"completed","metrics":{}})),
    ] {
        fs::write(measured.join(format!("{name}.json")), value.to_string()).unwrap();
    }
    fs::write(
        measured.join("bottlenecks.json"),
        r#"{"findings":[{"rule":"forged-old-analysis"}]}"#,
    )
    .unwrap();
    let source = root.join("sweep.toml");
    let path = source.to_str().unwrap();
    generate_with_links(path, &mut Vec::new(), false).unwrap();
    let report = root.join("results/sweep-comparison");
    assert!(!report.join("observations.parquet").exists());
    generate_with_links(path, &mut Vec::new(), true).unwrap();
    assert!(
        fs::read_to_string(report.join("report.html"))
            .unwrap()
            .contains("href=\"observations.parquet\"")
    );
    let reader =
        SerializedFileReader::new(fs::File::open(report.join("observations.parquet")).unwrap())
            .unwrap();
    let rows = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        rows.iter()
            .any(|r| r.get_string(6).unwrap() == "observations"
                && r.get_string(7).unwrap() == "cpu.user_ms"
                && r.get_double(12).unwrap() == 0.0)
    );
    assert!(
        rows.iter()
            .any(|r| r.get_string(6).unwrap() == "observations"
                && r.get_string(7).unwrap() == "io.wait_ms"
                && r.get_string(10).unwrap() == "unavailable"
                && matches!(
                    r.get_column_iter().nth(12).unwrap().1,
                    parquet::record::Field::Null
                ))
    );
    assert!(rows.iter().any(|r| r.get_string(6).unwrap() == "counter"
        && r.get_string(7).unwrap() == "cycles"
        && r.get_string(14).unwrap() == &u64::MAX.to_string()
        && matches!(
            r.get_column_iter().nth(12).unwrap().1,
            parquet::record::Field::Null
        )));
    assert!(
        rows.iter()
            .all(|r| !r.get_string(11).unwrap().contains("forged-old-analysis"))
    );
    assert!(
        fs::read_to_string(measured.join("bottlenecks.json"))
            .unwrap()
            .contains("forged-old-analysis")
    );
    drop(reader);
    let before_html = fs::read(report.join("report.html")).unwrap();
    let before_parquet = fs::read(report.join("observations.parquet")).unwrap();
    let mut oversized = components.clone();
    oversized["components"]["agent.execute"]["cpu_accounting_source"] =
        json!("x".repeat(MAX_ROW_BYTES + 1));
    fs::write(measured.join("components.json"), oversized.to_string()).unwrap();
    let error = generate_with_links(path, &mut Vec::new(), true).unwrap_err();
    assert!(error.to_string().contains("row bound"), "{error}");
    assert_eq!(fs::read(report.join("report.html")).unwrap(), before_html);
    assert_eq!(
        fs::read(report.join("observations.parquet")).unwrap(),
        before_parquet
    );
    assert_eq!(
        fs::read_to_string(measured.join("components.json")).unwrap(),
        oversized.to_string()
    );
    fs::write(measured.join("components.json"), components.to_string()).unwrap();
    generate_with_links(path, &mut Vec::new(), false).unwrap();
    assert!(!report.join("observations.parquet").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn parquet_round_trip_preserves_nulls_exact_integers_and_bounded_groups() {
    let dir = crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-columnar-").unwrap();
    let path = dir.join("test.parquet");
    let mut export = Export::new(&path).unwrap();
    for index in 0..GROUP_ROWS + 1 {
        export
            .push(Record {
                text: std::array::from_fn(|i| format!("field-{i}")),
                value: if index == 0 { None } else { Some(0.0) },
                timestamp: Some(0.125),
                exact: Some(u64::MAX.to_string()),
            })
            .unwrap();
    }
    export.close().unwrap();
    let reader = SerializedFileReader::new(fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(reader.num_row_groups(), 2);
    let rows = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), GROUP_ROWS + 1);
    assert!(matches!(
        rows[0].get_column_iter().nth(12).unwrap().1,
        parquet::record::Field::Null
    ));
    assert_eq!(rows[1].get_double(12).unwrap(), 0.0);
    assert_eq!(rows[0].get_double(13).unwrap(), 0.125);
    assert_eq!(rows[0].get_string(14).unwrap(), &u64::MAX.to_string());
    assert!(Export::new(&path).is_err());
    let mut invalid = Export::new(&dir.join("invalid.parquet")).unwrap();
    let mut row = Record {
        text: std::array::from_fn(|_| String::new()),
        value: Some(f64::NAN),
        timestamp: None,
        exact: None,
    };
    assert!(invalid.push(row).is_err());
    row = Record {
        text: std::array::from_fn(|_| String::new()),
        value: None,
        timestamp: None,
        exact: None,
    };
    row.text[0] = "x".repeat(MAX_ROW_BYTES + 1);
    assert!(invalid.push(row).is_err());
    drop(invalid);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires explicit retained native two-replica Parquet report"]
fn retained_native_export_preserves_scoped_counter_and_phase_evidence() {
    let path = std::env::var_os("ASB_TEST_PARQUET").expect("retained Parquet path");
    let reader = SerializedFileReader::new(fs::File::open(path).unwrap()).unwrap();
    let mut phase_ids = BTreeSet::new();
    let mut counter_ids = BTreeSet::new();
    let mut cohort = false;
    let mut series = false;
    let mut series_ends = BTreeMap::<String, (f64, f64)>::new();
    for row in reader.get_row_iter(None).unwrap() {
        let row = row.unwrap();
        let family = row.get_string(6).unwrap();
        if family == "phase_alignments" && row.get_string(10).unwrap() == "observed" {
            let details: Value = serde_json::from_str(row.get_string(11).unwrap()).unwrap();
            assert_eq!(details["clock"], "PERF_TIME_CONV");
            assert!(details["first_sample_offset_seconds"].as_f64().unwrap() >= 0.0);
            phase_ids.insert(row.get_string(0).unwrap().clone());
        }
        if family == "counter" {
            assert_eq!(row.get_string(9).unwrap(), "agent-process-tree");
            let _: u64 = row.get_string(14).unwrap().parse().unwrap();
            counter_ids.insert(row.get_string(0).unwrap().clone());
        }
        cohort |= family == "cohort" && row.get_string(10).unwrap() == "observed";
        if family == "series" && row.get_string(7).unwrap().contains("arm-spe") {
            series = true;
            let details: Value = serde_json::from_str(row.get_string(11).unwrap()).unwrap();
            let expected = details["context"]["spe_phase_alignment"]["last_sample_offset_seconds"]
                .as_f64()
                .unwrap();
            let time = row.get_double(13).unwrap();
            let end = series_ends
                .entry(row.get_string(0).unwrap().clone())
                .or_insert((0.0, expected));
            end.0 = end.0.max(time);
        }
    }
    assert_eq!(phase_ids.len(), 2);
    assert_eq!(counter_ids, phase_ids);
    assert!(cohort && series);
    assert_eq!(series_ends.len(), 2);
    for (last, expected) in series_ends.values() {
        assert!((last - expected).abs() < 1e-9, "{last} != {expected}");
    }
}
