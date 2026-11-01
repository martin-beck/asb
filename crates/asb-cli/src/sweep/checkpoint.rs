//! Private content binding primitives for sweep continuation.
use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(any(not(unix), test))]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(any(not(unix), test))]
use std::time::Instant;

const HELPER: &str = include_str!("../../assets/benchmarks/asb_fingerprint.py");
pub(super) mod campaigns;
const MAX_REQUEST: usize = 16 * 1024 * 1024;
pub(super) mod completed;
pub(super) use completed::replicas::Observer as ReplicaObserver;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Root {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "location_bound")]
    pub bind_location: bool,
    #[serde(default)]
    pub direct_file: bool,
}

fn location_bound() -> bool {
    true
}

impl Root {
    pub(super) fn path(name: impl Into<String>, path: PathBuf) -> Self {
        Self {
            name: name.into(),
            path,
            bind_location: true,
            direct_file: false,
        }
    }
    pub(super) fn executable() -> Result<Self, Error> {
        let path = std::env::current_exe().map_err(crate::io_error)?;
        Ok(Self {
            name: "asb-executable".into(),
            path,
            bind_location: false,
            direct_file: true,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fingerprint {
    pub schema_version: u32,
    pub digest: String,
    pub bytes: u64,
    pub nodes: u64,
}

impl Fingerprint {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && self.digest.len() == 64
            && self.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && self.bytes <= 64 * 1024u64.pow(3)
            && self.nodes <= 2_000_000
    }
}

// Deliberately not Debug/Serialize: the private key must not enter diagnostics.
pub(super) struct Snapshotter {
    key: [u8; 32],
    python: PathBuf,
}

/// Borrow every potentially large value. Field order matches the old sorted
/// serde_json::Value projection, including each root object, byte for byte.
#[derive(Serialize)]
struct Request<'a> {
    context: &'a Value,
    environment: bool,
    exclude: &'a [&'a str],
    key: &'a str,
    roots: RootViews<'a>,
}
struct RootViews<'a>(&'a [Root]);
impl Serialize for RootViews<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a> {
            bind_location: bool,
            direct_file: bool,
            name: &'a str,
            path: &'a Path,
        }
        serializer.collect_seq(self.0.iter().map(|root| View {
            bind_location: root.bind_location,
            direct_file: root.direct_file,
            name: &root.name,
            path: &root.path,
        }))
    }
}
fn encode_request(
    key: &[u8; 32],
    roots: &[Root],
    context: &Value,
    environment: bool,
    exclude: &[&str],
) -> Result<Vec<u8>, Error> {
    struct Buffer {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_REQUEST.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other(
                    "checkpoint request exceeds its bound",
                ));
            }
            self.bytes.reserve_exact(bytes.len());
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let key: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut buffer = Buffer {
        bytes: Vec::new(),
        exceeded: false,
    };
    let request = Request {
        context,
        environment,
        exclude,
        key: &key,
        roots: RootViews(roots),
    };
    if serde_json::to_writer(&mut buffer, &request).is_err() {
        return Err(Error::Input(
            if buffer.exceeded {
                "checkpoint input description exceeds 16 MiB; select a smaller batch"
            } else {
                "cannot encode private checkpoint inputs; no input values were logged"
            }
            .into(),
        ));
    }
    Ok(buffer.bytes)
}

impl Snapshotter {
    pub(super) fn open(directory: &Path, python: PathBuf) -> Result<Self, Error> {
        if let Ok(metadata) = std::fs::symlink_metadata(directory)
            && !metadata.is_dir()
        {
            return Err(Error::Input(
                "checkpoint directory must be a real directory, not a symlink or file".into(),
            ));
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory).map_err(crate::io_error)?;
        let key_path = directory.join("key");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&key_path) {
            Ok(mut file) => {
                let mut key = [0; 32];
                fill_random(&mut key)?;
                file.write_all(&key)
                    .and_then(|()| file.sync_all())
                    .map_err(crate::io_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(crate::io_error(error)),
        }
        let metadata = std::fs::symlink_metadata(&key_path).map_err(crate::io_error)?;
        if !metadata.is_file() || metadata.len() != 32 {
            return Err(Error::Input("checkpoint key is missing or invalid; preserve the old results and start a fresh sweep".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::Input("checkpoint key permissions are not private; protect its existing key with mode 0600 before continuing".into()));
            }
        }
        let mut key = [0; 32];
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        options
            .open(&key_path)
            .and_then(|mut file| file.read_exact(&mut key))
            .map_err(crate::io_error)?;
        Ok(Self { key, python })
    }

    pub(super) fn fingerprint(
        &self,
        roots: &[Root],
        context: &Value,
        environment: bool,
        exclude: &[&str],
    ) -> Result<Fingerprint, Error> {
        self.fingerprint_with_timeout(
            roots,
            context,
            environment,
            exclude,
            Duration::from_secs(130),
        )
    }

    // Real-child tests exercise the same exchange with a short allowance.
    fn fingerprint_with_timeout(
        &self,
        roots: &[Root],
        context: &Value,
        environment: bool,
        exclude: &[&str],
        timeout: Duration,
    ) -> Result<Fingerprint, Error> {
        let request = encode_request(&self.key, roots, context, environment, exclude)?;
        #[cfg(unix)]
        let bytes = crate::helper_exchange::run(
            &self.python,
            HELPER,
            &request,
            crate::helper_exchange::Policy {
                request_bytes: MAX_REQUEST,
                response_bytes: 2048,
                timeout,
            },
        )
        .map_err(|reason| {
            Error::Run(format!(
                "checkpoint inputs could not be sealed; no input values were logged; {reason}"
            ))
        })?;
        #[cfg(not(unix))]
        let bytes = self.legacy_exchange(&request, timeout)?;
        let fingerprint: Fingerprint = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Run("checkpoint helper returned an invalid digest".into()))?;
        if !fingerprint.valid() {
            return Err(Error::Run(
                "checkpoint helper returned an invalid or unbounded digest".into(),
            ));
        }
        Ok(fingerprint)
    }

    // Preserve the former non-Unix route without claiming cancellable Windows
    // pipes or Linux descendant-custody qualification for it.
    #[cfg(not(unix))]
    fn legacy_exchange(&self, request: &[u8], timeout: Duration) -> Result<Vec<u8>, Error> {
        let mut child = Command::new(&self.python)
            .args(["-I", "-S", "-B", "-c", HELPER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| {
                Error::Input("checkpoint hashing needs a usable local Python 3 interpreter".into())
            })?;
        let send = child
            .stdin
            .take()
            .expect("private helper stdin")
            .write_all(&request);
        if send.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Io(
                "cannot send private checkpoint input description".into(),
            ));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Run(
                        "checkpoint hashing exceeded its deadline or helper wait failed".into(),
                    ));
                }
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|_| Error::Io("cannot collect checkpoint digest".into()))?;
        if output.stdout.len() > 2048 || !output.status.success() {
            return Err(Error::Run("checkpoint inputs could not be sealed (missing, changing, unsupported or excessive input); no input values were logged".into()));
        }
        Ok(output.stdout)
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../../tests/unit/checkpoint_transport.rs"]
mod transport_tests;

pub(super) struct Inputs {
    directory: PathBuf,
    snapshotter: Snapshotter,
    roots: Vec<Root>,
    context: Value,
    initial: Fingerprint,
    runtime: Option<(Vec<crate::runtime_inputs::Dependency>, PathBuf)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedInputSeal {
    schema_version: u32,
    status: String,
    scope: String,
    roots: Vec<Root>,
    initial: Fingerprint,
    r#final: Option<Fingerprint>,
}

impl SavedInputSeal {
    fn validate(&self) -> Result<(), Error> {
        if self.schema_version != 1
            || self.scope != "input-stability-only-not-a-resume-receipt"
            || !self.initial.valid()
            || self.roots.len() > 100000
            || match self.status.as_str() {
                "pending" => self.r#final.is_some(),
                "stable" => self.r#final.as_ref() != Some(&self.initial),
                _ => true,
            }
        {
            return Err(resume_error(
                "the previous sweep does not have stable or pending declared inputs",
            ));
        }
        Ok(())
    }
}

impl Inputs {
    pub(super) fn begin(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
        mode: Option<crate::tape::Mode>,
        replay_sources: &[PathBuf],
        invocation_dir: &Path,
    ) -> Result<Self, Error> {
        let state = Self::build(parent, results, plan, mode, replay_sources, invocation_dir)?;
        state.publish("pending", None)?;
        if plan.execution.is_some() {
            state.reserve_campaigns(results, plan)?;
        }
        Ok(state)
    }

    /// Verify existing declared inputs without replacing their historical seal.
    /// A killed owner can leave a pending seal; completed outcomes still need
    /// their separate immutable evidence checks before any execution is skipped.
    pub(super) fn resume(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
        mode: Option<crate::tape::Mode>,
        replay_sources: &[PathBuf],
        invocation_dir: &Path,
    ) -> Result<Self, Error> {
        let directory = results.join(".checkpoint");
        if !std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir())
            || !std::fs::symlink_metadata(directory.join("key"))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == 32)
        {
            return Err(resume_error(
                "the original private input checkpoint is missing",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(directory.join("inputs.json"))
            .map_err(|_| resume_error("the original input seal is missing"))?;
        let metadata = file.metadata().map_err(crate::io_error)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            return Err(resume_error(
                "the original input seal is not a bounded regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(crate::io_error)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(resume_error(
                "the original input seal exceeds its size limit",
            ));
        }
        let saved: SavedInputSeal = serde_json::from_slice(&bytes)
            .map_err(|_| resume_error("the original input seal is malformed"))?;
        saved.validate()?;
        if plan.execution.is_some() {
            campaigns::verify_private_inputs(results, plan)?;
        }
        let current = Self::build(parent, results, plan, mode, replay_sources, invocation_dir)?;
        if current.initial != saved.initial {
            return Err(resume_error(
                "the executable, configuration, runtime, environment or replay inputs changed",
            ));
        }
        if plan.execution.is_some() {
            current.verify_campaign_reservations(results, plan)?;
        }
        Ok(current)
    }

    /// A metadata-only operation, never an Inputs capability returned to workers.
    /// Current credentials/runtime environment are deliberately not compared to
    /// the execution seal: no provider, agent or evaluator can be called here.
    pub(super) fn recover_recordings(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
    ) -> Result<Vec<Value>, Error> {
        Self::recovery_inputs(parent, results, plan)?
            .recover_recording_publications(parent, results, plan)
    }

    pub(super) fn recover_continuation(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
    ) -> Result<Value, Error> {
        Self::recovery_inputs(parent, results, plan)?
            .recover_continuation_link(parent, results, plan)
    }

    fn recovery_inputs(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
    ) -> Result<Self, Error> {
        use crate::runner::campaign::reading;
        let directory = results.join(".checkpoint");
        let saved: SavedInputSeal = serde_json::from_slice(&reading::bounded_relative(
            &directory,
            Path::new("inputs.json"),
            16 * 1024 * 1024,
        )?)
        .map_err(|_| resume_error("original input seal is malformed"))?;
        saved.validate()?;
        if reading::bounded_relative(&directory, Path::new("key"), 32)?.len() != 32 {
            return Err(resume_error("original checkpoint key is invalid"));
        }
        let first = plan
            .entries
            .first()
            .ok_or_else(|| resume_error("no original configuration"))?;
        let (root, _) = crate::project::load(&parent.join(&first.reference))?;
        let candidate = crate::python_runtime::project_python(&root);
        let python = if candidate.is_file() {
            candidate
        } else {
            PathBuf::from("python3")
        };
        let snapshotter = Snapshotter::open(&directory, python)?;
        // Saved execution roots may include the departed controller's procfs
        // executable or now-uninstalled runtimes. Publication recovery reads
        // retained metadata/payloads only; it must not rediscover those tools.
        let mut roots = campaigns::input_roots(results, plan)?;
        roots.push(Root::path(
            "recovery-original-input-seal",
            directory.join("inputs.json"),
        ));
        roots.push(Root::path(
            "recovery-original-index",
            results.join("plan.json"),
        ));
        roots.push(Root::path("recovery-original-key", directory.join("key")));
        Ok(Self {
            directory,
            snapshotter,
            roots,
            context: json!({"protocol":"asb-publication-recovery-file-stability-v1","scope":"not-execution-admission"}),
            initial: saved.initial,
            runtime: None,
        })
    }

    fn build(
        parent: &Path,
        results: &Path,
        plan: &super::plan::SweepPlan,
        mode: Option<crate::tape::Mode>,
        replay_sources: &[PathBuf],
        invocation_dir: &Path,
    ) -> Result<Self, Error> {
        let directory = results.join(".checkpoint");
        let mut roots = vec![
            Root::executable()?,
            Root::path("selection-plan", results.join("plan.json")),
        ];
        // Seal the actual recorder(s) selected by the immutable replay plans.
        // Legacy raw cassettes always use the historical llmtape format.
        let mut recorder_names = BTreeSet::new();
        if mode == Some(crate::tape::Mode::Replay) && plan.execution.is_some() {
            for layout in plan.read_campaigns(results)? {
                for input in &layout.plan.replay_inputs {
                    recorder_names.insert(
                        input
                            .recorder
                            .as_deref()
                            .unwrap_or(crate::proxy::legacy_recorder_name())
                            .to_owned(),
                    );
                }
            }
            if recorder_names.is_empty() {
                recorder_names.insert(crate::proxy::legacy_recorder_name().to_owned());
            }
        } else if mode == Some(crate::tape::Mode::Replay) {
            recorder_names.insert(crate::proxy::legacy_recorder_name().to_owned());
        } else if mode.is_some() {
            let adapter = crate::proxy::default_recorder()
                .ok_or_else(|| resume_error("no recording proxy is registered"))?;
            recorder_names.insert(adapter.name().to_owned());
        }
        let mut recorder_runtime = Vec::new();
        let mut recorder_files = BTreeSet::new();
        for name in &recorder_names {
            recorder_files.extend(crate::proxy::input_files(name, invocation_dir)?);
            recorder_runtime.extend(crate::proxy::runtime_inputs(name, invocation_dir)?);
        }
        // Keep existing root labels for historical single-recorder seals.
        for (index, path) in recorder_files.into_iter().enumerate() {
            roots.push(Root::path(format!("proxy-execution-file-{index}"), path));
        }
        if plan.execution.is_some() {
            roots.extend(campaigns::input_roots(results, plan)?);
        } else if mode == Some(crate::tape::Mode::Replay) {
            for (index, path) in replay_sources.iter().enumerate() {
                roots.push(Root::path(
                    format!("replay-source-{index}"),
                    std::path::absolute(path).map_err(crate::io_error)?,
                ));
            }
        }
        let mut projects = BTreeSet::new();
        let mut python = PathBuf::from("python3");
        for entry in &plan.entries {
            let (root, _) = crate::project::load(&parent.join(&entry.reference))?;
            projects.insert(root.canonicalize().map_err(crate::io_error)?);
        }
        for (index, root) in projects.iter().enumerate() {
            let (_, config) = crate::project::load(root)?;
            let candidate = crate::python_runtime::project_python(root);
            if candidate.is_file() {
                python = candidate;
            }
            for (name, path) in [
                ("configuration", root.join(".asb/project.toml")),
                ("tools", crate::project::resolve(root, &config.tools_dir)),
                (
                    "workloads",
                    crate::project::environment_path("ASB_WORKLOAD_DIR", invocation_dir)
                        .unwrap_or_else(|| crate::project::resolve(root, &config.workloads_dir)),
                ),
                ("python", root.join(".asb/python")),
            ] {
                roots.push(Root::path(format!("project-{index}-{name}"), path));
            }
        }
        let mut environment_files = BTreeSet::new();
        for (_, value) in std::env::vars_os() {
            let path = invocation_dir.join(value);
            if path.is_file() {
                // Keep the declared symlink in the input graph. Canonicalizing
                // it here would miss retargeting to different content while
                // the previous target stayed unchanged.
                environment_files.insert(std::path::absolute(path).map_err(crate::io_error)?);
            }
        }
        for (index, path) in environment_files.into_iter().enumerate() {
            roots.push(Root::path(
                format!("explicit-environment-file-{index}"),
                path,
            ));
        }
        let mut runtime = crate::runtime_inputs::for_plan(parent, plan, mode, invocation_dir)?;
        runtime.extend(recorder_runtime);
        runtime.push(crate::runtime_inputs::Dependency::Python {
            path: python.clone(),
            site: false,
        });
        let runtime_paths = crate::runtime_inputs::discover_at(&python, &runtime, invocation_dir)?;
        for (index, path) in runtime_paths.into_iter().enumerate() {
            roots.push(Root::path(format!("runtime-source-{index}"), path));
        }
        let snapshotter = Snapshotter::open(&directory, python)?;
        let context = json!({"protocol":"asb-input-seal-v1","scope":"declared-project-inputs-and-process-environment"});
        let initial = snapshotter.fingerprint(&roots, &context, true, &[])?;
        let state = Self {
            directory,
            snapshotter,
            roots,
            context,
            initial,
            runtime: Some((runtime, invocation_dir.to_path_buf())),
        };
        Ok(state)
    }

    pub(super) fn finish(mut self) -> Result<(), Error> {
        self.finish_borrowed()
    }

    pub(super) fn finish_borrowed(&mut self) -> Result<(), Error> {
        match self.final_fingerprint() {
            Ok(after) => self.publish(
                if after.digest == self.initial.digest {
                    "stable"
                } else {
                    "changed"
                },
                Some(&after),
            ),
            Err(error) => {
                let _ = self.publish("unavailable", None);
                Err(error)
            }
        }
    }

    /// Completed-only campaign reuse has no newly admitted work to settle.
    /// Recheck inputs without rewriting the original seal/provenance locators.
    pub(super) fn verify_stable_borrowed(&mut self) -> Result<(), Error> {
        if self.final_fingerprint()? != self.initial {
            return Err(resume_error(
                "declared inputs changed while reusing completed campaigns",
            ));
        }
        Ok(())
    }

    fn final_fingerprint(&mut self) -> Result<Fingerprint, Error> {
        if let Some((runtime, invocation_dir)) = &self.runtime {
            let paths = crate::runtime_inputs::discover_at(
                &self.snapshotter.python,
                runtime,
                invocation_dir,
            )?;
            self.roots
                .retain(|root| !root.name.starts_with("runtime-source-"));
            for (index, path) in paths.into_iter().enumerate() {
                self.roots
                    .push(Root::path(format!("runtime-source-{index}"), path));
            }
        }
        self.snapshotter
            .fingerprint(&self.roots, &self.context, true, &[])
    }

    fn publish(&self, status: &str, after: Option<&Fingerprint>) -> Result<(), Error> {
        let content = serde_json::to_vec_pretty(&json!({"schema_version":1,"status":status,
            "scope":"input-stability-only-not-a-resume-receipt", "roots":self.roots,
            "initial":self.initial,"final":after}))
        .map_err(|_| Error::Io("cannot encode private input seal".into()))?;
        let temporary = self.directory.join(format!(
            ".state-{}",
            crate::workloads::TaskIdentity::local("input-seal", None).attempt_id
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(crate::io_error)?;
        file.write_all(&content)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&temporary, self.directory.join("inputs.json")))
            .map_err(crate::io_error)
    }
}

fn resume_error(reason: &str) -> Error {
    Error::Input(format!(
        "cannot resume this sweep: {reason}; existing results were not replaced; restore the original inputs or run without --resume for a fresh sweep"
    ))
}

#[cfg(unix)]
fn fill_random(bytes: &mut [u8]) -> Result<(), Error> {
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(bytes))
        .map_err(|_| Error::Io("cannot generate a private checkpoint key".into()))
}

#[cfg(not(unix))]
fn fill_random(_: &mut [u8]) -> Result<(), Error> {
    Err(Error::Input(
        "secure checkpoint key generation is unavailable on this host".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_root_is_a_durable_path_not_a_process_lifetime_alias() {
        let root = Root::executable().unwrap();
        assert_eq!(root.name, "asb-executable");
        assert!(root.direct_file);
        assert!(!root.bind_location);
        assert!(root.path.is_file());
        #[cfg(target_os = "linux")]
        assert!(!root.path.starts_with("/proc/"));
    }

    #[test]
    fn accepts_the_supported_bounded_fingerprint_node_limit() {
        let fingerprint = Fingerprint {
            schema_version: 1,
            digest: "a".repeat(64),
            bytes: 0,
            nodes: 2_000_000,
        };
        assert!(fingerprint.valid());
        assert!(
            !Fingerprint {
                nodes: 2_000_001,
                ..fingerprint
            }
            .valid()
        );
    }

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let directory = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("checkpoint", None).attempt_id);
            std::fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn keyed_content_binding_is_private_stable_and_sensitive_to_input_changes() {
        let directory = Directory::new();
        let file = directory.0.join("input");
        std::fs::write(&file, "one").unwrap();
        let state = directory.0.join("state");
        let snapshotter = Snapshotter::open(&state, "python3".into()).unwrap();
        let roots = [Root::path("runtime", file.clone())];
        let context = json!({"model":"logical-model","private-test-value":"never expose this"});
        let original = snapshotter
            .fingerprint(&roots, &context, false, &[])
            .unwrap();
        assert!(original.valid());
        assert!(
            !serde_json::to_string(&original)
                .unwrap()
                .contains("never expose")
        );
        let reopened = Snapshotter::open(&state, "python3".into()).unwrap();
        assert_eq!(
            original.digest,
            reopened
                .fingerprint(&roots, &context, false, &[])
                .unwrap()
                .digest
        );
        std::fs::write(file, "two").unwrap();
        assert_ne!(
            original.digest,
            reopened
                .fingerprint(&roots, &context, false, &[])
                .unwrap()
                .digest
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(state.join("key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn malformed_existing_keys_are_preserved_instead_of_regenerated() {
        let directory = Directory::new();
        std::fs::write(directory.0.join("key"), "existing malformed key").unwrap();
        assert!(Snapshotter::open(&directory.0, "python3".into()).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.0.join("key")).unwrap(),
            "existing malformed key"
        );
    }

    #[test]
    fn input_stability_records_distinguish_stable_changed_and_unavailable() {
        for (action, status) in [
            ("none", "stable"),
            ("change", "changed"),
            ("fifo", "unavailable"),
        ] {
            let directory = Directory::new();
            let input = directory.0.join("input");
            std::fs::write(&input, "before").unwrap();
            let state = directory.0.join("state");
            let snapshotter = Snapshotter::open(&state, "python3".into()).unwrap();
            let roots = vec![Root::path("declared-input", input.clone())];
            let context = json!({"scope":"unit-fixture"});
            let initial = snapshotter
                .fingerprint(&roots, &context, true, &[])
                .unwrap();
            let inputs = Inputs {
                directory: state.clone(),
                snapshotter,
                roots,
                context,
                initial,
                runtime: None,
            };
            inputs.publish("pending", None).unwrap();
            if action == "change" {
                std::fs::write(&input, "after").unwrap();
            }
            #[cfg(unix)]
            if action == "fifo" {
                std::fs::remove_file(&input).unwrap();
                // The FIFO is owned by this fixture; hashing must reject it
                // without opening/blocking on a writer.
                assert!(
                    Command::new("python3")
                        .args([
                            "-I",
                            "-S",
                            "-B",
                            "-c",
                            "import os,sys; os.mkfifo(sys.argv[1], 0o600)"
                        ])
                        .arg(&input)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            #[cfg(not(unix))]
            if action == "fifo" {
                continue;
            }
            assert_eq!(inputs.finish().is_ok(), action != "fifo");
            let record: Value =
                serde_json::from_slice(&std::fs::read(state.join("inputs.json")).unwrap()).unwrap();
            assert_eq!(record["status"], status);
            assert_eq!(record["scope"], "input-stability-only-not-a-resume-receipt");
            if action == "none" {
                assert_eq!(record["initial"]["digest"], record["final"]["digest"]);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_checkpoint_directory_and_nonprivate_key_are_not_accepted() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = Directory::new();
        let target = directory.0.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = directory.0.join("redirect");
        symlink(&target, &link).unwrap();
        assert!(Snapshotter::open(&link, "python3".into()).is_err());
        assert!(!target.join("key").exists());
        Snapshotter::open(&target, "python3".into()).unwrap();
        let key = std::fs::read(target.join("key")).unwrap();
        std::fs::set_permissions(target.join("key"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(Snapshotter::open(&target, "python3".into()).is_err());
        assert_eq!(std::fs::read(target.join("key")).unwrap(), key);
    }
}
