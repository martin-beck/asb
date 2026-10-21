//! Common execution/evaluation protocol for workload-owned official engines.
use crate::workloads::controller::{Controller, decode_base64};
use crate::workloads::session::{
    BaselineEvidence, ExecutionContext, ExecutionResult, PreparationContext, TaskInvocation,
};
use crate::workloads::{Evaluation, TaskIdentity, WorkloadSession};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Project paths/proxies cross the workload boundary, host capabilities do not.
fn guest_project_configuration(project: &crate::project::ProjectConfig) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct GuestProject<'a> {
        tools_dir: &'static str,
        workloads_dir: &'static str,
        workspace_dir: &'static str,
        proxy: &'a crate::project::NetworkProxy,
    }
    toml::to_string(&GuestProject {
        tools_dir: ".asb-tools",
        workloads_dir: "workloads",
        workspace_dir: "workspace",
        proxy: &project.proxy,
    })
    .map_err(|_| "cannot encode guest project configuration".into())
}

pub(crate) fn prefetch(context: &super::PreflightContext<'_>) -> Result<(), crate::Error> {
    let tools = crate::project::resolve(context.project_root, &context.project_config.tools_dir);
    crate::install::guest_bootstrap(context.agent, context.project_root, &tools)?;
    Ok(())
}

pub(crate) fn verify_retained(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RetainedOutcomeContext<'_>,
) -> Result<(), crate::Error> {
    let recovery = &context.recovery;
    recovery
        .reference
        .validate_location(recovery.project_root)?;
    let tool = crate::project::resolve(recovery.project_root, &recovery.project_config.tools_dir)
        .join(tool_id);
    require_sdk(&tool, &recovery.reference.workload)?;
    let mut command = Command::new(tool.join("venv/bin/python"));
    command
        .arg(tool.join("asb_retained.py"))
        .arg("--module")
        .arg(module)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    controller
        .send(&json!({"reference":recovery.reference,"summary":context.summary}))
        .map_err(crate::Error::Run)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let result = controller.receive(deadline).map_err(crate::Error::Run)?;
    if result["ok"] != true
        || result["scope"] != "retained-artifact-consistency-not-historical-attestation"
    {
        let reason = result["reason"]
            .as_str()
            .filter(|reason| reason.len() <= 512 && !reason.chars().any(char::is_control))
            .unwrap_or(
                "retained upstream artifacts do not establish a consistent completed outcome",
            );
        return Err(crate::Error::Input(format!(
            "cannot reconcile retained outcome: {reason}; inspect {} and preserve the original records rather than repeating the task",
            recovery.reference.artifacts.display()
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)
}

pub(crate) fn inspect_grade(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
) -> Result<Value, crate::Error> {
    retained_grade_request(tool_id, module, context, runtime, None)
}

pub(crate) fn grade_retained(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
    catalog: &std::path::Path,
) -> Result<Value, crate::Error> {
    retained_grade_request(tool_id, module, context, runtime, Some(catalog))
}

fn retained_grade_request(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
    grading_catalog: Option<&std::path::Path>,
) -> Result<Value, crate::Error> {
    context.reference.validate_location(context.project_root)?;
    let tool = crate::project::resolve(context.project_root, &context.project_config.tools_dir)
        .join(tool_id);
    require_sdk(&tool, &context.reference.workload)?;
    let mut command = Command::new(tool.join("venv/bin/python"));
    command
        .arg(runtime.join("asb_retained.py"))
        .args([
            "--module",
            module,
            if grading_catalog.is_some() {
                "--grade"
            } else {
                "--inspect-grade"
            },
        ])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    controller
        .send(&json!({"reference":context.reference,"catalog":grading_catalog}))
        .map_err(crate::Error::Run)?;
    let deadline =
        Instant::now() + Duration::from_secs(if grading_catalog.is_some() { 1980 } else { 60 });
    let report = controller.receive(deadline).map_err(crate::Error::Run)?;
    if report["ok"] == false {
        let error_class = report["error_class"]
            .as_str()
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .unwrap_or("unknown_error");
        let fallback = format!(
            "retained official grade is missing or inconsistent ({error_class}); inspect preserved artifacts"
        );
        let reason = report["reason"]
            .as_str()
            .filter(|reason| reason.len() <= 512 && !reason.chars().any(char::is_control))
            .unwrap_or(&fallback);
        return Err(crate::Error::Input(format!(
            "cannot {} retained grade: {reason}; no agent was run",
            if grading_catalog.is_some() {
                "complete"
            } else {
                "inspect"
            }
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)?;
    validate_inspected_grade(&report, context.reference)?;
    Ok(report)
}

/// Private support copies must not replace an active worker's installed SDK helpers.
pub(crate) struct InspectionRuntime(pub(crate) PathBuf);

impl InspectionRuntime {
    pub(crate) fn new(project_root: &std::path::Path) -> Result<Self, crate::Error> {
        let path = project_root.join(".asb").join(format!(
            "grade-inspection-{}",
            TaskIdentity::local("inspection", None).attempt_id
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(crate::io_error)?;
        Ok(Self(path))
    }
}

impl Drop for InspectionRuntime {
    fn drop(&mut self) {
        // Only the unique directory successfully created by this guard.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn validate_inspected_grade(
    report: &Value,
    reference: &super::recovery::AttemptReference,
) -> Result<(), crate::Error> {
    let invalid = || {
        crate::Error::Input("retained grade inspection returned an invalid or mismatched report; no outcome was imported".into())
    };
    let task: TaskIdentity =
        serde_json::from_value(report["task"].clone()).map_err(|_| invalid())?;
    let baseline: BaselineEvidence =
        serde_json::from_value(report["workload_baseline"].clone()).map_err(|_| invalid())?;
    let evidence = report["evidence"].as_object().ok_or_else(invalid)?;
    let mut evidence_bytes = 0u64;
    if evidence.is_empty() || evidence.len() > 256 {
        return Err(invalid());
    }
    for (relative, item) in evidence {
        if relative.is_empty()
            || relative.len() > 4096
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || item["sha256"].as_str().is_none_or(|hash| {
                hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(invalid());
        }
        evidence_bytes = evidence_bytes
            .checked_add(item["bytes"].as_u64().ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        if evidence_bytes > 512 * 1024 * 1024 {
            return Err(invalid());
        }
    }
    if report["schema_version"] != 1
        || report["scope"] != "retained-official-grade-not-execution-recovery"
        || task.workload != reference.workload
        || task.task_id != reference.task_id
        || Some(&reference.dataset) != task.dataset.as_ref()
        || task.attempt_id.len() != 36
        || !task.attempt_id.starts_with("asb-")
        || !task.attempt_id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !baseline.supports("upstream-harness")
        || report["workload_evaluation"] != "upstream-harness"
        || report["workload_success"].as_bool().is_none()
        || !matches!(
            report["recorded_cleanup"].as_str(),
            Some("removed" | "stopped-preserved")
        )
        || [
            "process_success",
            "exit_code",
            "elapsed_ms",
            "token_usage",
            "steps",
        ]
        .iter()
        .any(|field| !report.get(field).is_some_and(Value::is_null))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn runtime_inputs(
    tool: &str,
    context: &crate::runtime_inputs::Context<'_>,
) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
    use crate::runtime_inputs::Dependency;
    use std::path::Path;
    // This controller owns the Python SDK and Docker CLI transport, not
    // benchmark dispatch or the selected agent/provider implementation.
    let mut inputs = vec![
        Dependency::Python {
            path: context.tool_root(tool).join("venv/bin/python"),
            site: true,
        },
        Dependency::program("docker"),
    ];
    if crate::tools::host_executable(Path::new("sudo")).is_some() {
        inputs.push(Dependency::program("sudo"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        for path in [
            ".docker/config.json",
            ".docker/contexts",
            ".docker/cli-plugins",
        ] {
            inputs.push(Dependency::file(Path::new(&home).join(path)));
        }
    }
    for path in [
        "/usr/libexec/docker/cli-plugins",
        "/usr/local/lib/docker/cli-plugins",
        "/usr/lib/docker/cli-plugins",
    ] {
        inputs.push(Dependency::file(path));
    }
    Ok(inputs)
}

struct Session {
    process_lifetime: super::session::ProcessLifetime,
    controller: Controller,
    invocation: TaskInvocation,
    baseline: BaselineEvidence,
    guest_workspace: String,
    agent_timeout: Duration,
    measurement_active: bool,
    preparation_active: bool,
    cleanup_attempted: bool,
    evaluation_completed: bool,
    artifacts: PathBuf,
}

pub(crate) fn require_sdk(root: &std::path::Path, workload: &str) -> Result<(), crate::Error> {
    if !root.join("venv/bin/python").is_file() {
        return Err(crate::Error::Input(format!(
            "the official evaluator for `{workload}` is not installed; run `asb workload install {workload}` before starting the benchmark"
        )));
    }
    Ok(())
}

pub(crate) fn recover(
    tool_id: &str,
    context: &super::recovery::RecoveryContext<'_>,
) -> Result<Value, crate::Error> {
    context.reference.validate_location(context.project_root)?;
    let tools = crate::project::resolve(context.project_root, &context.project_config.tools_dir);
    let tool = tools.join(tool_id);
    require_sdk(&tool, &context.reference.workload)?;
    if !tool.join("asb_resources.py").is_file() {
        return Err(crate::Error::Input("the installed workload recovery helper is missing; reinstall the workload before recovery".into()));
    }
    let mut command = Command::new(tool.join("venv/bin/python"));
    command
        .arg(tool.join("asb_resources.py"))
        .arg("--quiesce")
        .arg(&context.reference.artifacts)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    crate::project::apply_network_proxy(&mut command, context.project_root)?;
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let report = controller.receive(deadline).map_err(crate::Error::Run)?;
    if report["ok"] == false {
        let reason = if report["error_class"] == "RecoveryError" {
            report["reason"]
                .as_str()
                .filter(|reason| reason.len() <= 512 && !reason.contains(['\n', '\r']))
        } else {
            None
        };
        return Err(crate::Error::Run(format!(
            "workload recovery failed: {}",
            reason.unwrap_or(
                "Docker resource inspection or access failed; run `docker info` and inspect the retained attempt journal"
            )
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)?;
    let attempt = report["attempt"].as_str().filter(|attempt| {
        attempt.len() == 36
            && attempt.starts_with("asb-")
            && attempt[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if report["schema_version"] != 1
        || attempt.is_none()
        || report["scope"] != "observed-resource-quiescence-not-a-retry-receipt"
        || report["resources"].as_array().is_none_or(|resources| {
            resources.len() > 64
                || resources.iter().any(|resource| {
                    !matches!(
                        resource["status"].as_str(),
                        Some("absent" | "preserved" | "stopped-preserved")
                    ) || !matches!(
                        resource["kind"].as_str(),
                        Some("containers" | "networks" | "volumes" | "images")
                    ) || resource["name"].as_str().is_none_or(|name| {
                        name.len() > 255 || attempt.is_none_or(|attempt| !name.contains(attempt))
                    }) || (resource["kind"] == "images"
                        && resource["status"] != "absent"
                        && resource["image_id"].as_str().is_none_or(|identity| {
                            identity.strip_prefix("sha256:").is_none_or(|hash| {
                                hash.len() != 64
                                    || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            })
                        }))
                })
        })
    {
        return Err(crate::Error::Run(
            "workload recovery returned an invalid bounded report".into(),
        ));
    }
    Ok(report)
}

pub(crate) fn launch(
    tool_id: &str,
    helper: &str,
    operation: Option<&str>,
    workload: &str,
    dataset: super::session::DatasetIdentity,
    context: &PreparationContext<'_>,
) -> Result<Box<dyn WorkloadSession>, String> {
    let task = context
        .trial
        .ok_or("select an official instance ID in setup before running this benchmark")?;
    if context.trial_set.is_some() {
        crate::workloads::selected_trial(
            workload,
            task,
            context.trial_set,
            context.workload_directory,
        )?;
    }
    let project_root = context.project_root;
    let tool = context.tool_root(tool_id);
    let attempts = project_root.join(".asb/attempts");
    std::fs::create_dir_all(&attempts).map_err(|error| error.to_string())?;
    let preparation_id = TaskIdentity::local(workload, Some(task)).attempt_id;
    let artifacts =
        std::path::absolute(attempts.join(&preparation_id)).map_err(|error| error.to_string())?;
    let reference = super::recovery::AttemptReference {
        schema_version: 1,
        preparation_id,
        workload: workload.into(),
        task_id: task.into(),
        dataset: dataset.clone(),
        artifacts: artifacts.clone(),
    };
    reference
        .validate_location(project_root)
        .map_err(|error| error.to_string())?;
    super::recovery::remember(context.workspace, &reference).map_err(|error| error.to_string())?;
    if let Some(observer) = context.on_allocation {
        observer(&reference)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let error_log = options
        .open(artifacts.with_extension("controller.log"))
        .map_err(|error| error.to_string())?;
    let mut command = Command::new(tool.join("venv/bin/python"));
    command.stderr(error_log);
    command.arg(tool.join(helper));
    if let Some(operation) = operation {
        command.arg(operation);
    }
    command
        .arg("--catalog")
        .arg(
            context
                .workload_directory
                .join(workload)
                .join("catalog.json"),
        )
        .arg("--task")
        .arg(task)
        .arg("--artifacts")
        .arg(&artifacts)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    crate::project::apply_network_proxy(&mut command, project_root)
        .map_err(|error| error.to_string())?;
    let mut controller = Controller::spawn(&mut command)?;
    let prepared = (|| -> Result<_, String> {
        let ready = controller.receive(Instant::now() + Duration::from_secs(900))?;
        if ready["event"] != "ready" {
            return Err("official task did not become ready".into());
        }
        let invocation = &ready["invocation"];
        let identity: TaskIdentity = serde_json::from_value(invocation["identity"].clone())
            .map_err(|_| "official task returned invalid identity")?;
        if identity.workload != workload
            || identity.task_id != task
            || identity.dataset.as_ref() != Some(&dataset)
        {
            return Err("prepared task does not match the selected dataset instance".into());
        }
        let prompt = invocation["prompt"]
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= 64 * 1024)
            .ok_or("official task returned invalid public instructions")?
            .to_owned();
        let guest_workspace = invocation["workspace"]
            .as_str()
            .filter(|path| path.starts_with('/') && !path.contains(".."))
            .ok_or("official task returned invalid environment workspace")?
            .to_owned();
        let baseline: BaselineEvidence = serde_json::from_value(invocation["baseline"].clone())
            .map_err(|_| "official baseline evidence is invalid")?;
        if !baseline.supports("upstream-harness") {
            return Err("official task definition does not establish baseline identity".into());
        }
        let agent_timeout = invocation
            .get("agent_timeout_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(900.0);
        if !agent_timeout.is_finite() || !(0.001..=86400.0).contains(&agent_timeout) {
            return Err("official task returned an invalid agent time budget".into());
        }
        Ok((identity, prompt, guest_workspace, baseline, agent_timeout))
    })();
    let (identity, prompt, guest_workspace, baseline, agent_timeout) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(match controller.cancel(Duration::from_secs(240)) {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; workload cancellation cleanup failed: {cleanup}"),
            });
        }
    };
    Ok(Box::new(Session {
        process_lifetime: super::adapters::find(workload)
            .ok_or("workload adapter disappeared before execution")?
            .process_lifetime(),
        controller,
        invocation: TaskInvocation {
            identity,
            prompt,
            workspace: context.workspace.to_path_buf(),
        },
        baseline,
        guest_workspace,
        agent_timeout: Duration::from_secs_f64(agent_timeout),
        measurement_active: false,
        preparation_active: false,
        cleanup_attempted: false,
        evaluation_completed: false,
        artifacts,
    }))
}

impl Session {
    fn cancel(&mut self) -> Result<(), String> {
        self.cleanup_attempted = true;
        self.controller.cancel(Duration::from_secs(240))
    }

    fn run(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
    ) -> Result<ExecutionResult, String> {
        let connection = context
            .connection
            .ok_or("isolated execution needs a negotiated API connection")?;
        if let Some(session) = context.measurements.as_deref_mut() {
            session.event(crate::collectors::LifecycleEvent::PhaseBegin(
                crate::collectors::LifecyclePhase::AgentPreparation,
            ))?;
            self.preparation_active = true;
        }
        let binary = runtime_binary()?;
        let tools_root =
            crate::project::resolve(context.project_root, &context.project_config.tools_dir);
        let bootstrap = crate::install::guest_bootstrap(
            &context.config.agent,
            context.project_root,
            &tools_root,
        )
        .map_err(|error| {
            format!("cannot prepare selected agent installation artifacts: {error}")
        })?;
        let mut config = context.config.clone();
        config.workspace = self.guest_workspace.clone();
        config.concurrency = 1;
        let mut environment = serde_json::Map::new();
        let namespace_collectors = context
            .measurements
            .as_deref_mut()
            .map(|session| session.execution_namespace_collectors())
            .unwrap_or_default();
        for name in [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "socks_proxy",
            "SOCKS_PROXY",
            "no_proxy",
            "NO_PROXY",
            "NPM_CONFIG_OFFLINE",
            "ASB_MEASURE_INTERVAL_MS",
        ] {
            if let Ok(value) = std::env::var(name) {
                environment.insert(name.into(), Value::String(value));
            }
        }
        self.controller.send(&json!({"op":"agent", "binary":binary, "project_root":context.project_root,
            "tools_root":tools_root, "bootstrap":bootstrap, "measure_target":context.measurements.is_some(),
            "project_configuration":guest_project_configuration(context.project_config)?,
            "environment":environment, "network":std::env::var("ASB_BENCHMARK_NETWORK").ok(),
            "request":{"process_lifetime":self.process_lifetime,"config":config,"connection":connection,"prompt":self.invocation.prompt,
                "project_root":"/opt/asb-project","runtime_dir":"/opt/asb-private","run_dir":"/opt/asb-private/attempt","replay":context.replay,
                "namespace_collectors":namespace_collectors}}))?;
        let deadline = Instant::now() + self.agent_timeout + Duration::from_secs(1820);
        loop {
            let event = self.controller.receive(deadline)?;
            match event["event"].as_str() {
                Some("paused") => {
                    if let Some(session) = context.measurements.as_deref_mut() {
                        use crate::collectors::LifecycleEvent as Event;
                        if self.measurement_active {
                            return Err(
                                "isolated execution returned a second measurement target".into()
                            );
                        }
                        let target = super::target::verify(&event["target"])?;
                        crate::measurements::write_json(
                            &session.dir.join("execution-target.json"),
                            &json!({
                                "schema_version":1,"scope":"verified-local-isolated-process-tree",
                                "host_pid":target.pid,"namespace_pid":target.namespace_pid,
                                "start_time_ticks":target.start_time_ticks,"cgroup":target.cgroup,
                                "cgroup_membership":"preserved"
                            }),
                        )?;
                        if let Err(error) =
                            session.attach_isolated_pid(target.pid, target.start_time_ticks)
                        {
                            session.event(Event::AgentAttachFailed)?;
                            return Err(error);
                        }
                        session.event(Event::PhaseEnd(
                            crate::collectors::LifecyclePhase::AgentPreparation,
                        ))?;
                        self.preparation_active = false;
                        session.event(Event::MeasurementsReady)?;
                        session.event(Event::AgentSpawn)?;
                        session.event(Event::AgentExecuteBegin)?;
                        self.measurement_active = true;
                        session.event(Event::AgentStarted)?;
                    }
                    self.controller
                        .send(&json!({"op":"resume","exec_id":event["target"]["exec_id"]}))?;
                }
                Some("output") => {
                    let bytes = decode_base64(&event["data"])?;
                    if context.show_output {
                        output
                            .write_all(&bytes)
                            .map_err(|_| "cannot write agent output")?;
                    }
                }
                Some("execution_ended") => {
                    if let Some(session) = context.measurements.as_deref_mut() {
                        if !self.measurement_active {
                            return Err(
                                "isolated execution ended without an active measured target".into(),
                            );
                        }
                        session.event(crate::collectors::LifecycleEvent::AgentExecuteEnd)?;
                        session.event(crate::collectors::LifecycleEvent::AgentExited)?;
                        self.measurement_active = false;
                    }
                }
                Some("completed") => {
                    if self.measurement_active {
                        return Err("isolated execution omitted its completion boundary".into());
                    }
                    let result = &event["result"];
                    let metadata: crate::agents::driver::GuestResult =
                        serde_json::from_value(result["metadata"].clone())
                            .map_err(|_| "guest returned invalid agent result metadata")?;
                    let delegated = self.process_lifetime
                        == super::session::ProcessLifetime::ThroughEvaluation
                        && result["workload_processes_retained"].as_bool() == Some(true)
                        && metadata.workload_owner.as_ref().is_some_and(|owner| {
                            owner.controller_namespace > 1
                                && owner.guest_namespace > 1
                                && owner.controller_namespace != owner.guest_namespace
                                && owner.token.len() == 32
                                && owner.token.bytes().all(|byte| byte.is_ascii_hexdigit())
                        });
                    if !metadata.tree_quiesced && !delegated {
                        return Err("isolated agent process tree was not confirmed stopped before evaluation".into());
                    }
                    if let Some(session) = context.measurements.as_deref_mut() {
                        session.import_execution_namespace_state(&metadata.namespace_collectors)?;
                    }
                    let code = result["exit_code"].as_i64().unwrap_or(1).clamp(0, 255) as i32;
                    #[cfg(unix)]
                    let status = {
                        use std::os::unix::process::ExitStatusExt;
                        std::process::ExitStatus::from_raw(code << 8)
                    };
                    #[cfg(not(unix))]
                    return Err("isolated execution requires Unix".into());
                    return Ok(ExecutionResult {
                        output: std::process::Output {
                            status,
                            stdout: decode_base64(&result["stdout"])?,
                            stderr: decode_base64(&result["stderr"])?,
                        },
                        metadata,
                        elapsed_ms: result["elapsed_seconds"]
                            .as_f64()
                            .filter(|seconds| {
                                seconds.is_finite() && (0.0..=86410.0).contains(seconds)
                            })
                            .map(|seconds| (seconds * 1000.0).round() as u128),
                    });
                }
                _ => return Err("unexpected isolated execution event".into()),
            }
        }
    }
}

fn runtime_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ASB_GUEST_BINARY") {
        return Ok(PathBuf::from(path));
    }
    // Pin the actual running executable, even if an update atomically replaces
    // its pathname while the SDK prepares the task. Numeric PID is essential:
    // /proc/self/exe would refer to the Python controller when it reads it.
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from(format!("/proc/{}/exe", std::process::id())));
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe().map_err(|_| "cannot find portable ASB runtime".into())
}

impl WorkloadSession for Session {
    fn checkpoint_evidence(&self) -> Vec<PathBuf> {
        vec![self.artifacts.clone()]
    }
    fn finish(&mut self) -> Result<(), String> {
        if !self.evaluation_completed {
            self.cancel()?;
            return Err(format!(
                "official evaluation is incomplete; task resources and partial work were stopped and preserved at {}; do not repeat the agent as the same attempt",
                self.artifacts.display()
            ));
        }
        self.cleanup_attempted = true;
        match self.controller.finish(Duration::from_secs(240)) {
            Ok(()) => Ok(()),
            Err(error) => Err(match self.cancel() {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; workload cancellation cleanup failed: {cleanup}"),
            }),
        }
    }
    fn invocation(&self) -> &TaskInvocation {
        &self.invocation
    }
    fn evaluation_kind(&self) -> &'static str {
        "upstream-harness"
    }
    fn baseline_passed(&self) -> Option<bool> {
        None
    }
    fn baseline_evidence(&self) -> BaselineEvidence {
        self.baseline.clone()
    }
    fn execute(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
    ) -> Option<Result<ExecutionResult, String>> {
        let mut result = self.run(context, output);
        if result.is_err() && self.preparation_active {
            self.preparation_active = false;
            if let Some(session) = context.measurements.as_deref_mut() {
                let _ = session.event(crate::collectors::LifecycleEvent::PhaseFailed(
                    crate::collectors::LifecyclePhase::AgentPreparation,
                ));
            }
        }
        if result.is_err() && self.measurement_active {
            self.measurement_active = false;
            if let Some(session) = context.measurements.as_deref_mut() {
                let _ = session.event(crate::collectors::LifecycleEvent::AgentExecuteEnd);
                let _ = session.event(crate::collectors::LifecycleEvent::AgentWaitFailed);
            }
        }
        if let Err(error) = &mut result
            && let Err(cleanup) = self.cancel()
        {
            error.push_str(&format!(
                "; workload cancellation cleanup failed: {cleanup}"
            ));
        }
        Some(result)
    }
    fn evaluate(&mut self, _response: &str) -> Result<Evaluation, String> {
        self.evaluation_completed = false;
        let event = self.controller.request(
            json!({"op":"evaluate","timeout":1800}),
            "evaluated",
            Duration::from_secs(1830),
        )?;
        let result = &event["result"];
        if result["workload"] != self.invocation.identity.workload
            || result["task_id"] != self.invocation.identity.task_id
            || result["dataset"]
                != serde_json::to_value(&self.invocation.identity.dataset)
                    .map_err(|_| "cannot compare evaluation dataset identity")?
        {
            return Err("official evaluation does not match this dataset instance".into());
        }
        match (result["outcome"].as_str(), result["resolved"].as_bool()) {
            (Some("resolved"), Some(true)) => {
                self.evaluation_completed = true;
                Ok(Evaluation::Passed)
            }
            (Some("unresolved" | "empty_submission"), Some(false)) => {
                self.evaluation_completed = true;
                Ok(Evaluation::Failed("official evaluator did not resolve the selected instance".into()))
            }
            _ => Err("official evaluator did not return a completed task-specific grade; inspect task artifacts".into()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Preparation can succeed before provider/proxy setup fails. Those
        // callers never reach execute/finish; still give the owner its full
        // bounded teardown budget instead of the controller's kill fallback.
        // Explicit paths report failures; Drop cannot replace a caller error.
        if !self.cleanup_attempted {
            let _ = self.cancel();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/remote.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_project_projection_excludes_host_capabilities_and_survives_proxy_rewrite() {
        use std::io::Write;
        use std::process::Stdio;
        for instances in [
            Vec::new(),
            vec!["asb-host-only-0".into(), "asb-host-only-1".into()],
        ] {
            let host = crate::project::ProjectConfig {
                tools_dir: "host-tools".into(),
                workloads_dir: "host-workloads".into(),
                workspace_dir: "host-workspace".into(),
                cgroup_parent: Some("/host-only/cgroup".into()),
                perfetto_instances: instances.clone(),
                proxy: crate::project::NetworkProxy {
                    http: Some("http://localhost:1234/proxy".into()),
                    ..Default::default()
                },
            };
            let encoded = guest_project_configuration(&host).unwrap();
            let table: toml::Value = toml::from_str(&encoded).unwrap();
            assert_eq!(table.as_table().unwrap().len(), 4);
            assert!(table.get("perfetto_instances").is_none());
            assert!(table.get("cgroup_parent").is_none());
            let source = r#"
import sys,tomllib
from types import SimpleNamespace
sys.path.insert(0,sys.argv[1])
from asb_relay import forward_request
configuration=sys.stdin.read()
targets=[]
def start(values):
    targets.extend(values)
    return SimpleNamespace(ports=[2345])
result=forward_request({'project_configuration':configuration,'environment':{},'request':{'connection':{'base_url':'http://localhost:1234/v1'}}},start)
assert targets == [('localhost',1234)]
assert result['request']['connection']['base_url'] == 'http://localhost:2345/v1'
assert tomllib.loads(result['project_configuration'])['proxy']['http'] == 'http://localhost:2345/proxy'
sys.stdout.write(result['project_configuration'])
"#;
            let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks");
            let mut child = Command::new("python3")
                .args(["-I", "-S", "-B", "-c", source])
                .arg(assets)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(encoded.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let guest: crate::project::ProjectConfig =
                toml::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
            assert_eq!(guest.tools_dir, ".asb-tools");
            assert_eq!(guest.workloads_dir, "workloads");
            assert_eq!(guest.workspace_dir, "workspace");
            assert_eq!(guest.cgroup_parent, None);
            assert!(guest.perfetto_instances.is_empty());
            assert_eq!(host.perfetto_instances, instances);
            assert_eq!(host.cgroup_parent.as_deref(), Some("/host-only/cgroup"));
        }
    }

    #[test]
    fn inspected_grade_requires_bound_identity_and_explicitly_unknown_execution() {
        let reference = super::super::recovery::AttemptReference {
            schema_version: 1,
            preparation_id: "a-b-c".into(),
            workload: "fixture".into(),
            task_id: "original-task".into(),
            dataset: super::super::session::DatasetIdentity {
                source: "fixture/dataset".into(),
                revision: "b".repeat(40),
                split: "test".into(),
            },
            artifacts: "unused".into(),
        };
        let original = json!({"schema_version":1,"scope":"retained-official-grade-not-execution-recovery",
            "task":{"workload":reference.workload,"task_id":reference.task_id,"dataset":reference.dataset,"attempt_id":format!("asb-{}","a".repeat(32))},
            "workload_baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)},
            "workload_evaluation":"upstream-harness","workload_success":false,"recorded_cleanup":"stopped-preserved",
            "evidence":{"task.json":{"sha256":"d".repeat(64),"bytes":100}},
            "process_success":null,"exit_code":null,"elapsed_ms":null,"token_usage":null,"steps":null});
        validate_inspected_grade(&original, &reference).unwrap();
        for evidence in [
            json!(null),
            json!({}),
            json!({"../task.json":{"sha256":"d".repeat(64),"bytes":1}}),
            json!({"/task.json":{"sha256":"d".repeat(64),"bytes":1}}),
            json!({"task.json":{"sha256":"bad","bytes":1}}),
            json!({"task.json":{"sha256":"d".repeat(64),"bytes":-1}}),
            json!({"task.json":{"sha256":"d".repeat(64),"bytes":513 * 1024 * 1024}}),
        ] {
            let mut forged = original.clone();
            forged["evidence"] = evidence;
            assert!(validate_inspected_grade(&forged, &reference).is_err());
        }
        for field in [
            "process_success",
            "exit_code",
            "elapsed_ms",
            "token_usage",
            "steps",
        ] {
            let mut forged = original.clone();
            forged[field] = json!(0);
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "{field}"
            );
            forged.as_object_mut().unwrap().remove(field);
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "missing {field}"
            );
        }
        for (field, value) in [
            ("scope", json!("complete")),
            ("workload_success", Value::Null),
            ("recorded_cleanup", json!("pending")),
            ("workload_evaluation", json!("adapter-smoke")),
        ] {
            let mut forged = original.clone();
            forged[field] = value;
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "{field}"
            );
        }
        let mut forged = original.clone();
        forged["task"]["task_id"] = json!("another-task");
        assert!(validate_inspected_grade(&forged, &reference).is_err());
        forged = original.clone();
        forged["task"]["attempt_id"] = json!("invalid");
        assert!(validate_inspected_grade(&forged, &reference).is_err());
        forged = original;
        forged["workload_baseline"] = json!({"kind":"unavailable"});
        assert!(validate_inspected_grade(&forged, &reference).is_err());
    }

    fn session(results: &[Value], completed: bool) -> Session {
        let script = format!(
            "import json,sys\nresults=json.loads(sys.argv[1])\nprint(json.dumps(dict(event='ready')),flush=True)\nfor result in results:\n request=json.loads(sys.stdin.readline())\n assert request['op']=='evaluate'\n print(json.dumps(dict(event='evaluated',result=result)),flush=True)\nlast=sys.stdin.readline()\nassert bool(last)=={completed}\nif last: assert json.loads(last)['op']=='close'\nprint(json.dumps(dict(event='closed',cleanup='removed' if last else 'stopped-preserved')),flush=True)\n",
            completed = if completed { "True" } else { "False" },
        );
        let mut command = Command::new("python3");
        command.args([
            "-u",
            "-c",
            &script,
            &serde_json::to_string(results).unwrap(),
        ]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        Session {
            process_lifetime: super::super::session::ProcessLifetime::Quiesce,
            controller,
            invocation: TaskInvocation {
                identity: TaskIdentity::local("fixture", Some("original-task")),
                prompt: "fixture issue".into(),
                workspace: "fixture-workspace".into(),
            },
            baseline: BaselineEvidence::UpstreamTaskDefinition {
                base_commit: "a".repeat(40),
            },
            guest_workspace: "/task".into(),
            agent_timeout: Duration::from_secs(1),
            measurement_active: false,
            preparation_active: false,
            cleanup_attempted: false,
            evaluation_completed: false,
            artifacts: "retained-attempt".into(),
        }
    }

    fn result(outcome: &str, resolved: Value) -> Value {
        json!({"workload":"fixture", "task_id":"original-task", "dataset":null,
               "outcome":outcome, "resolved":resolved})
    }

    #[test]
    fn only_matching_completed_grades_allow_destructive_session_finish() {
        for (outcome, resolved) in [
            ("resolved", true),
            ("unresolved", false),
            ("empty_submission", false),
        ] {
            let mut task = session(&[result(outcome, json!(resolved))], true);
            assert!(task.evaluate("").is_ok());
            assert!(task.evaluation_completed);
            task.finish().unwrap();
        }
        let mut wrong_task = result("resolved", json!(true));
        wrong_task["task_id"] = json!("another-task");
        for invalid in [
            result("evaluation_error", json!(false)),
            result("resolved", json!(false)),
            result("unresolved", json!(true)),
            result("resolved", Value::Null),
            wrong_task,
        ] {
            let mut task = session(&[invalid], false);
            assert!(task.evaluate("").is_err());
            assert!(!task.evaluation_completed);
            let error = task.finish().unwrap_err();
            assert!(error.contains("stopped and preserved at retained-attempt"));
            assert!(task.cleanup_attempted);
        }
    }

    #[test]
    fn later_evaluator_failure_cannot_reuse_an_earlier_grade_to_delete_work() {
        let mut task = session(
            &[
                result("unresolved", json!(false)),
                result("evaluation_error", json!(false)),
            ],
            false,
        );
        assert!(task.evaluate("").is_ok());
        assert!(task.evaluation_completed);
        assert!(task.evaluate("").is_err());
        assert!(!task.evaluation_completed);
        assert!(task.finish().unwrap_err().contains("preserved"));
    }
}
