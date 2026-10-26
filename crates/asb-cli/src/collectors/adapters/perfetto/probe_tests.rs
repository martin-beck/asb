//! Client-protocol fixtures, not qualification of tracefs or a real Tracebox.
use super::*;

#[cfg(unix)]
#[test]
fn isolated_preflight_client_diagnostics_and_artifact_cleanup() {
    const CASE: &str = "ASB_PERFETTO_CLIENT_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-probe-fixtures")
            .join(crate::workloads::TaskIdentity::local("probe", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let name = format!(
            "{}::isolated_preflight_client_diagnostics_and_artifact_cleanup",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "success",
            "reset-failed",
            "controller-failed",
            "missing-source",
            "client-failed",
            "spawn-failed",
            "stderr-blocked",
            "config-blocked",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(&directory).unwrap();
            // /bin/sh reads this as data; no generated executable is run and
            // no host collector, namespace, sudo, or kernel interface is used.
            fs::write(
                directory.join("perfetto"),
                r#"
[ "$PERFETTO_PRODUCER_SOCK_NAME" = '@fixture-producer' ] || exit 81
[ "$PERFETTO_CONSUMER_SOCK_NAME" = '@fixture-consumer' ] || exit 82
[ "$1" = --txt ] && [ "$2" = -c ] && [ "$4" = -o ] || exit 83
found=no
while IFS= read -r line; do
 case "$line" in *'duration_ms: 250'*) found=yes;; esac
done < "$3"
[ "$found" = yes ] || exit 84
printf 'fixture trace bytes' > "$5"
case "$ASB_PERFETTO_CLIENT_CASE" in
 reset-failed) printf 'Failed to reset ftrace\n' >&2;;
 controller-failed) printf 'Failed to create FtraceController\n' >&2;;
 missing-source) printf 'Data source id=1 not found\n' >&2;;
 client-failed) printf 'fixture client failure\n' >&2; exit 7;;
esac
"#,
            )
            .unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&directory)
                .env_clear()
                .env(CASE, case)
                .env("HOME", &directory)
                .env("PATH", &directory);
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let service = Service {
        instance: "asb-fixture-instance".into(),
        marker: root.join("unused-marker"),
        executable: if case == "spawn-failed" {
            root.join("missing-executable")
        } else {
            "/bin/sh".into()
        },
        producer: "@fixture-producer".into(),
        consumer: "@fixture-consumer".into(),
        children: Arc::new(Mutex::new(Vec::new())),
        owner: None,
        namespace_helper: None,
        _lease: File::create(root.join("unused-lease")).unwrap(),
    };
    let blocked = match case.as_str() {
        "stderr-blocked" => Some(root.join(".perfetto-preflight.stderr")),
        "config-blocked" => Some(root.join(".perfetto-preflight.pbtx")),
        _ => None,
    };
    if let Some(path) = &blocked {
        fs::create_dir(path).unwrap();
    }
    let result = super::super::perfetto_probe(&service, &root);
    match case.as_str() {
        "success" => assert!(result.unwrap()),
        "spawn-failed" | "stderr-blocked" => {
            let error = result.unwrap_err();
            assert!(
                error.contains("cannot execute owned Perfetto preflight client"),
                "{error}"
            );
            assert!(error.contains("asb tool install perfetto"));
        }
        "config-blocked" => assert!(
            result
                .unwrap_err()
                .contains("cannot write Perfetto preflight configuration")
        ),
        _ => assert!(
            !result.unwrap(),
            "diagnostic must reject a zero-exit client"
        ),
    }
    for file in [
        ".perfetto-preflight.pbtx",
        ".perfetto-preflight.perfetto-trace",
        ".perfetto-preflight.stderr",
    ] {
        let path = root.join(file);
        if blocked.as_ref() == Some(&path) {
            assert!(path.is_dir(), "must not remove pre-existing directories");
        } else {
            assert!(
                !path.exists(),
                "temporary client artifact survived: {}",
                path.display()
            );
        }
    }
    assert!(
        !service.marker.exists(),
        "probe must not access trace_marker"
    );
    assert!(service.children.lock().unwrap().is_empty());
}
