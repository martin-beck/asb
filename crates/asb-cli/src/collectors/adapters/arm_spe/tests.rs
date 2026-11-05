use super::*;
#[test]
fn selection_is_bounded_and_does_not_accept_unknown_or_duplicate_options() {
    assert_eq!(options("arm-spe").unwrap(), Options::default());
    let configured = options("arm-spe:period=65536:budget-mib=16").unwrap();
    assert_eq!(configured.period, 65536);
    assert_eq!(configured.budget_bytes, 16 * MIB);
    for invalid in [
        "other",
        "arm-spe:",
        "arm-spe:period=1",
        "arm-spe:period=NaN",
        "arm-spe:budget-mib=1025",
        "arm-spe:budget-mib=0",
        "arm-spe:unknown=1",
        "arm-spe:period=65536:period=65536",
    ] {
        assert!(!ADAPTER.accepts_selection(invalid), "{invalid}");
    }
}
#[test]
fn record_selection_never_expands_to_host_or_cpu_wide_scope() {
    let args = args("arm_spe_0", 42, &Options::default());
    assert!(args.windows(2).any(|pair| pair == ["-p", "42"]));
    assert!(args.contains(&"--inherit".into()));
    assert!(args.windows(2).any(|pair| pair[0] == "--max-size"));
    assert!(args.windows(2).any(|pair| pair == ["--max-size", "256M"]));
    assert!(!args.iter().any(|arg| matches!(
        arg.as_str(),
        "-a" | "--all-cpus" | "-C" | "--cpu" | "--no-inherit"
    )));
    assert!(!ADAPTER.default_enabled());
    assert!(measurements::available_collectors().contains(&"arm-spe".into()));
    assert!(!measurements::default_collectors().contains(&"arm-spe".into()));
    assert_eq!(
        ADAPTER.target_scope("local-docker"),
        TargetScope::AgentProcess
    );
}
#[test]
fn pmu_plan_requires_advertised_controls_and_never_guesses_a_domain() {
    let mut pmu = super::super::arm_platform::Pmu {
        name: "arm_spe_0".into(),
        domain: "spe",
        event_type: Some(48),
        cpus: Some("0-3".into()),
        cpumask: None,
        events: Default::default(),
        formats: [
            ("ts_enable".into(), "config:16".into()),
            ("load_filter".into(), "config:32".into()),
            ("store_filter".into(), "config:33".into()),
        ]
        .into(),
    };
    assert_eq!(select_pmu(&[pmu.clone()]).unwrap(), "arm_spe_0");
    assert!(select_pmu(&[]).is_err());
    assert!(select_pmu(&[pmu.clone(), pmu.clone()]).is_err());
    pmu.formats.remove("ts_enable");
    assert!(select_pmu(&[pmu]).is_err());
}
#[test]
#[ignore = "requires explicit retained native SPE raw and decoded fixture paths"]
fn retained_native_record_and_decode_prove_task_scope() {
    let raw = std::env::var_os("ASB_TEST_SPE_RAW").expect("raw SPE capture path");
    let decoded = std::env::var_os("ASB_TEST_SPE_DECODED").expect("decoded SPE capture path");
    let root = std::env::var("ASB_TEST_SPE_PID")
        .expect("root PID")
        .parse()
        .unwrap();
    let quality = quality::inspect(std::fs::File::open(raw).unwrap(), root, 1024 * MIB).unwrap();
    let result = samples::parse(std::fs::File::open(decoded).unwrap(), &quality).unwrap();
    assert!(result.observed_tasks.len() >= 2);
    assert!(result.counts.contains_key("memory"));
}
