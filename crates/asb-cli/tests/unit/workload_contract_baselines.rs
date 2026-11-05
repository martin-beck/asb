//! Frozen before engine binding extraction, against unchanged main-35 owners.
use serde_json::{Value, json};

struct Profile {
    id: &'static str,
    source: &'static str,
    revision: &'static str,
    split: &'static str,
    tool: &'static str,
    package: &'static str,
    family: &'static str,
    version: &'static str,
    submission: &'static str,
    evaluation: &'static str,
    x86: &'static str,
    arm: &'static str,
}

const PROFILES: [Profile; 5] = [
    Profile {
        id: "swe-bench",
        source: "SWE-bench/SWE-bench",
        revision: "c6fe717fd7a4c3ac1daa4055a4fd082c6a1d28a2",
        split: "test",
        tool: "swebench",
        package: "swebench @ git+https://github.com/SWE-bench/SWE-bench.git@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
        family: "swe-bench",
        version: "original",
        submission: "patch",
        evaluation: "fresh-environment",
        x86: "c1d019b096fe31453a3a7ba5927b9d514e50201491ae677f4c90918375086800",
        arm: "f635f3b123fe02107b7f7e83bc2783f888c13948ef0991ed3c73c05c36b29bd2",
    },
    Profile {
        id: "swe-bench-lite",
        source: "SWE-bench/SWE-bench_Lite",
        revision: "b0dde1093fe417d83b7184254edf8199c1f0dff5",
        split: "test",
        tool: "swebench",
        package: "swebench @ git+https://github.com/SWE-bench/SWE-bench.git@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
        family: "swe-bench",
        version: "original",
        submission: "patch",
        evaluation: "fresh-environment",
        x86: "e9bcb88ac84420d287a28bf52d0173e7ba770ea46111613eb9e6254f10c0d631",
        arm: "abdb30dd74c9c7d7c3f72df76ee98bdbab023807089a73b14ddcbc73bcfe678a",
    },
    Profile {
        id: "swe-bench-verified",
        source: "SWE-bench/SWE-bench_Verified",
        revision: "78f471bf655a3137b2e8a75af1501690ec009ec3",
        split: "test",
        tool: "swebench",
        package: "swebench @ git+https://github.com/SWE-bench/SWE-bench.git@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
        family: "swe-bench",
        version: "original",
        submission: "patch",
        evaluation: "fresh-environment",
        x86: "81c432a4e4ed7ba74316cb6da17897ba7254eefd854f9e7147c895a1a4e583ff",
        arm: "e03bb93d4786897169beec85c3c552306e59ff6a8759f1fe48aac7e6755c3ca4",
    },
    Profile {
        id: "terminal-bench-1.0",
        source: "harbor-framework/terminal-bench-1",
        revision: "91e10457b5410f16c44364da1a34cb6de8c488a5",
        split: "terminal-bench-core@0.1.1",
        tool: "terminal-bench",
        package: "terminal-bench==0.2.18",
        family: "terminal-bench",
        version: "terminal-bench-1.0",
        submission: "environment-state",
        evaluation: "in-place",
        x86: "8b2ba25b65bc6e6fe9f8c29bf25399ecd2bc2372d33cf9c3bf6010e44ef0bf44",
        arm: "c6c273dda45a11e8b582245dd163f84facbbcdb82a46a55f115f4703773b4157",
    },
    Profile {
        id: "terminal-bench-2.1",
        source: "harbor-framework/terminal-bench-2-1",
        revision: "7131e4375048a0e408a8fb404b5f499d726b695b",
        split: "tasks",
        tool: "harbor",
        package: "harbor==0.24.0",
        family: "terminal-bench",
        version: "terminal-bench-2.1",
        submission: "environment-state",
        evaluation: "in-place",
        x86: "cb8a28725533ee1834b4c51117f15e55e5b15ee015de34710786cfa41bcbc775",
        arm: "0690384749702d1414a042040ca258737b7f4a8fb8d44849050138eced41e5b8",
    },
];

fn expected(p: &Profile) -> Value {
    json!({"schema_version":1,"id":p.id,"family":p.family,"suite_version":p.version,
        "support":"official-adapter","dataset":{"source":p.source,"revision":p.revision,"split":p.split},
        "harness":{"id":p.tool,"package":p.package},"protocol":{"id":format!("{}-official",p.tool),
        "revision":1,"environment":"local-docker","submission":p.submission,"evaluation":p.evaluation,
        "score_policy":"task-resolution","input_modalities":["text"]}})
}

#[test]
fn five_official_serialized_descriptors_match_pre_binding_baselines() {
    for p in &PROFILES {
        let actual = super::super::descriptor(p.id).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            expected(p),
            "{}",
            p.id
        );
    }
}

#[test]
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn five_official_protocol_fingerprints_match_pre_binding_baselines() {
    // Resolve only metadata, in a child with no inherited Docker context or
    // user configuration. No Docker executable, SDK or daemon is invoked.
    const CHILD: &str = "ASB_OFFICIAL_BASELINE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("baseline", None).attempt_id);
        std::fs::create_dir(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "workloads::contracts::baselines::five_official_protocol_fingerprints_match_pre_binding_baselines", "--nocapture"])
            .env_clear().env(CHILD,"1").env("HOME",&root).output().unwrap();
        std::fs::remove_dir(&root).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for p in &PROFILES {
        let config = toml::from_str(&format!(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='{}'\n",
            p.id
        ))
        .unwrap();
        let mut policy = crate::planning::resolve(&config).unwrap();
        // This golden checks the historical policy, including its old budget.
        policy.default_budgets.environment_preparation_seconds = Some(900);
        let caps = crate::planning::EffectiveBudgets {
            environment_preparation_seconds: Some(900),
            execution_seconds: Some(600.0),
            evaluation_seconds: Some(1800),
            task_default_execution_seconds: Some(600.0),
            ..Default::default()
        };
        let descriptor = super::super::descriptor(p.id).unwrap();
        let actual =
            crate::planning::historical_protocol_fingerprint(Some(&descriptor), &policy, caps, 2)
                .unwrap();
        let golden = if cfg!(target_arch = "x86_64") {
            p.x86
        } else {
            p.arm
        };
        assert_eq!(
            actual.as_str(),
            format!("asb-protocol-v1:{golden}"),
            "{}",
            p.id
        );
    }
}
