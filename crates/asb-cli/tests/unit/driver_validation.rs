//! Guest admission failures must stop before process execution or credentials.
use super::*;
use crate::endpoint::{Authentication, Interface};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/guest-admission-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn request(&self) -> GuestRequest {
        let config = toml::from_str::<BenchmarkConfig>(
            "agent='codex'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\n",
        )
        .unwrap();
        GuestRequest {
            owned_instrumentation: None,
            task_identity: None,
            tape_cassette: None,
            tool_replay: false,
            tool_tape_cassette: None,
            inherit_working_directory: false,
            process_lifetime: Default::default(),
            workload_owner: None,
            config: BenchmarkConfig {
                workspace: self.0.join("workspace").display().to_string(),
                ..config
            },
            connection: Connection {
                interface: Interface::OpenAiResponses,
                base_url: "http://must-never-contact.invalid".into(),
                credential: None,
                authentication: Authentication::None,
            },
            prompt: "offline admission fixture".into(),
            project_root: self.0.join("project"),
            runtime_dir: self.0.join("runtime"),
            run_dir: self.0.join("runtime/run"),
            replay: false,
            timeout_seconds: 1.0,
            namespace_collectors: Vec::new(),
        }
    }
    fn initialize(&self) {
        fs::create_dir_all(self.0.join("project/.asb")).unwrap();
        fs::write(
            self.0.join("project/.asb/project.toml"),
            toml::to_string(&project::ProjectConfig {
                tools_dir: "tools".into(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn fresh_native_state_is_private_create_new_and_preserves_runtime_credentials() {
    let fixture = Fixture::new();
    let root = fixture.0.join("runtime/run/state");
    let mut command = Command::new("not-started");
    command
        .env("CODEX_HOME", fixture.0.join("parent"))
        .env("OPENAI_API_KEY", "fixture-provider-key")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("LLVM_PROFILE_FILE", "fixture-profile-%p.profraw");
    prepare_fresh_state(&mut command, &root).unwrap();
    let values: std::collections::BTreeMap<_, _> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(values["CODEX_HOME"], None);
    assert_eq!(
        values["OPENAI_API_KEY"].as_deref(),
        Some("fixture-provider-key")
    );
    assert_eq!(values["HTTPS_PROXY"].as_deref(), Some("http://127.0.0.1:9"));
    assert_eq!(
        values["LLVM_PROFILE_FILE"].as_deref(),
        Some("fixture-profile-%p.profraw")
    );
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ] {
        let directory = PathBuf::from(values[name].as_ref().unwrap());
        assert!(directory.starts_with(&root) && directory.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
    fs::write(
        root.join("preserve"),
        "existing state must never be cleaned",
    )
    .unwrap();
    assert!(prepare_fresh_state(&mut command, &root).is_err());
    assert_eq!(
        fs::read_to_string(root.join("preserve")).unwrap(),
        "existing state must never be cleaned"
    );
    let file = fixture.0.join("not-a-directory");
    fs::write(&file, "preserve").unwrap();
    assert!(prepare_fresh_state(&mut command, &file.join("child/state")).is_err());
    assert_eq!(fs::read_to_string(file).unwrap(), "preserve");
}

#[cfg(target_os = "linux")]
#[test]
fn unprivileged_access_changes_only_owned_regular_nodes_without_following_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = Fixture::new();
    let owned = fixture.0.join("owned");
    let nested = owned.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let regular = nested.join("task.txt");
    fs::write(&regular, "task source").unwrap();
    fs::set_permissions(&owned, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&regular, fs::Permissions::from_mode(0o600)).unwrap();
    let outside = fixture.0.join("outside.txt");
    fs::write(&outside, "must remain private").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&outside, nested.join("outside-link")).unwrap();

    grant_unprivileged_access(&owned, true).unwrap();

    assert_eq!(
        fs::metadata(&owned).unwrap().permissions().mode() & 0o777,
        0o777
    );
    assert_eq!(
        fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
        0o777
    );
    assert_eq!(
        fs::metadata(&regular).unwrap().permissions().mode() & 0o666,
        0o666
    );
    assert_eq!(
        fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::symlink_metadata(nested.join("outside-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unprivileged_project_access_is_read_execute_only() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let project = fixture.0.join("project-access");
    fs::create_dir_all(&project).unwrap();
    let executable = project.join("client");
    fs::write(&executable, "fixture").unwrap();
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    grant_unprivileged_access(&project, false).unwrap();

    assert_eq!(
        fs::metadata(&project).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unprivileged_runtime_access_grants_traversal_only_at_the_sealed_prefix() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let runtime = fixture.0.join("sealed-runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();

    grant_unprivileged_traverse(&runtime).unwrap();

    assert_eq!(
        fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
        0o711
    );
}

#[test]
fn selected_adapter_replaces_removed_parent_alias_with_its_fresh_configuration() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let mut command = Command::new("not-started");
    command.env("CODEX_HOME", fixture.0.join("parent"));
    prepare_fresh_state(&mut command, &request.run_dir.join("state")).unwrap();
    crate::agents::find("codex")
        .unwrap()
        .prepare(&mut crate::agents::PreparationContext {
            command: &mut command,
            model: "fixture",
            connection: &request.connection,
            workspace: &request.project_root,
            runtime_dir: &request.runtime_dir,
            run_dir: &request.run_dir,
            mcp_servers: &[],
            mcp_tape: None,
            tape: None,
        })
        .unwrap();
    let values: std::collections::BTreeMap<_, _> = command
        .get_envs()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.map(PathBuf::from)))
        .collect();
    assert_eq!(
        values["CODEX_HOME"],
        Some(request.runtime_dir.join("codex/config"))
    );
    assert_eq!(values["HOME"], Some(request.run_dir.join("state/home")));
    assert!(
        request
            .runtime_dir
            .join("codex/config/config.toml")
            .is_file()
    );
}

#[test]
fn guest_prepare_rejects_unbounded_mislocated_unregistered_and_uninstalled_requests() {
    let fixture = Fixture::new();
    for timeout in [f64::NAN, f64::INFINITY, -1.0, 0.0, 0.0001, 86400.1] {
        let mut request = fixture.request();
        request.timeout_seconds = timeout;
        assert!(
            prepare(&request)
                .unwrap_err()
                .to_string()
                .contains("time budget must be")
        );
    }
    for prompt in [false, true] {
        let mut request = fixture.request();
        if prompt {
            request.prompt = "x".repeat(65537);
        } else {
            request.config.concurrency = 2;
        }
        assert!(
            prepare(&request)
                .unwrap_err()
                .to_string()
                .contains("one bounded task invocation")
        );
    }
    for case in 0..7 {
        let mut request = fixture.request();
        match case {
            0 => request.config.workspace = "relative".into(),
            1 => request.project_root = "relative".into(),
            2 => request.runtime_dir = "relative".into(),
            3 => request.run_dir = fixture.0.join("runtime/../escape"),
            4 => request.runtime_dir = fixture.0.join("workspace/runtime"),
            5 => request.run_dir = fixture.0.join("workspace/run"),
            6 => request.run_dir = fixture.0.join("outside-runtime"),
            _ => unreachable!(),
        }
        assert!(
            prepare(&request)
                .unwrap_err()
                .to_string()
                .contains("absolute, separate and owned")
        );
    }
    assert!(
        prepare(&fixture.request())
            .unwrap_err()
            .to_string()
            .contains("project is not initialized")
    );
    fixture.initialize();
    let mut request = fixture.request();
    request.config.agent = "unknown-fixture".into();
    assert!(
        prepare(&request)
            .unwrap_err()
            .to_string()
            .contains("registered agent adapter")
    );
    let mut request = fixture.request();
    request.connection.interface = Interface::Gemini;
    assert!(
        prepare(&request)
            .unwrap_err()
            .to_string()
            .contains("endpoint is not supported")
    );
    let mut request = fixture.request();
    request.connection.authentication = Authentication::ApiKey;
    assert!(
        prepare(&request)
            .unwrap_err()
            .to_string()
            .contains("endpoint is not supported")
    );
    assert!(
        prepare(&fixture.request())
            .unwrap_err()
            .to_string()
            .contains("not installed in the guest tool project")
    );
    assert!(
        !fixture.0.join("runtime").exists(),
        "admission must not publish runtime artifacts"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn retained_guest_owner_validation_precedes_all_adapters_and_process_launch() {
    use crate::workloads::session::ProcessLifetime;
    let fixture = Fixture::new();
    let current = namespace_id().unwrap();
    let mut request = fixture.request();
    request.process_lifetime = ProcessLifetime::ThroughEvaluation;
    assert!(
        run(request)
            .unwrap_err()
            .to_string()
            .contains("no namespace owner")
    );
    for (controller_namespace, guest_namespace, token) in [
        (1, current, "a".repeat(32)),
        (current + 1, current + 2, "a".repeat(32)),
        (current, current, "a".repeat(32)),
        (current + 1, current, "a".repeat(31)),
        (current + 1, current, "g".repeat(32)),
    ] {
        let mut request = fixture.request();
        request.process_lifetime = ProcessLifetime::ThroughEvaluation;
        request.workload_owner = Some(WorkloadOwner {
            controller_namespace,
            guest_namespace,
            token,
        });
        assert!(
            run(request)
                .unwrap_err()
                .to_string()
                .contains("distinct owned PID namespace")
        );
    }
    let mut request = fixture.request();
    request.workload_owner = Some(WorkloadOwner {
        controller_namespace: current + 1,
        guest_namespace: current,
        token: "a".repeat(32),
    });
    assert!(
        run(request)
            .unwrap_err()
            .to_string()
            .contains("quiescent guest execution cannot declare retained ownership")
    );
    assert!(!fixture.0.join("runtime").exists());
}
