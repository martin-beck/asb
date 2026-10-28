//! Fixed, credential-free datagrams; EOF and installer exit are never proofs.
#![allow(unsafe_code)]
use super::Request;
use crate::{Error, install, install::ProgressStage, launch::ProcessLaunch, process_gate};
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

const VERSION: &str = "ASBINSTALL1";
const FD_ENV: &str = "ASB_INSTALL_PROGRESS_FD";
const FRAME: usize = 8;
const MAGIC: [u8; 4] = *b"ASI1";
const PROGRESS: u8 = 0;
const SUCCESS: u8 = 1;
const FAILURE: u8 = 2;
const MAX_NAMES: usize = 128;
const MAX_NAME: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Completion {
    Success,
    Input,
    Io,
    Run,
    Usage,
}

impl Completion {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::Success => "installation finished",
            Self::Input => "installation input was rejected",
            Self::Io => "installation filesystem or command failed",
            Self::Run => "installation execution failed",
            Self::Usage => "installation request was rejected",
        }
    }
}

#[derive(Debug, PartialEq)]
enum Message {
    Progress(u8, ProgressStage),
    Final(Completion),
}

pub(crate) struct Operation {
    gate: process_gate::ManagedGate,
    socket: UnixDatagram,
    percent: u8,
    stage: ProgressStage,
    transfer: Option<install::transfer::Progress>,
    transfer_path: PathBuf,
    result: Option<Completion>,
    finished: bool,
}

fn failure(message: impl std::fmt::Display) -> Error {
    Error::Io(format!("installation supervision failed: {message}"))
}

impl Operation {
    pub(crate) fn start(root: PathBuf, request: Request) -> Result<Self, Error> {
        let (mode, directory, names) = match request {
            Request::Tool { name, directory } => ("tool", directory, vec![name]),
            Request::Workload { name, directory } => ("workload", directory, vec![name]),
            Request::Dependencies {
                selected,
                directory,
            } => ("dependencies", directory, selected),
        };
        validate_names(&names).map_err(failure)?;
        if !root.is_absolute() || !directory.is_absolute() {
            return Err(failure("installation paths must be absolute"));
        }
        let executable = process_gate::executable().map_err(failure)?;
        let transfer_path = root.join(".asb").join(format!(
            ".install-transfer-{}",
            crate::tools::runtime::unique_id()
        ));
        let mut launch = ProcessLaunch::new(&executable);
        launch
            .arg("__asb-install")
            .arg(VERSION)
            .arg(mode)
            .arg(&root)
            .arg(&directory);
        for name in names {
            launch.arg(name);
        }
        let (socket, writer) = UnixDatagram::pair().map_err(failure)?;
        socket.set_nonblocking(true).map_err(failure)?;
        writer.set_nonblocking(true).map_err(failure)?;
        // Bound queued progress independently of installer duration/output.
        for (fd, option) in [
            (socket.as_raw_fd(), libc::SO_RCVBUF),
            // Linux AF_UNIX datagram queue capacity is governed by SNDBUF.
            (writer.as_raw_fd(), libc::SO_SNDBUF),
        ] {
            let size: libc::c_int = 16 * 1024;
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            } < 0
            {
                return Err(failure(io::Error::last_os_error()));
            }
        }
        let (mut command, route) = launch.native(Some(&executable));
        command
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let descriptor = writer.as_raw_fd();
        command.env(FD_ENV, descriptor.to_string());
        command.env(install::transfer::STATUS_ENV, &transfer_path);
        // No allocation or environment access occurs in the post-fork callback.
        unsafe {
            command.pre_exec(move || set_cloexec(descriptor, false));
        }
        let admission =
            process_gate::Admission::new(route.validate_gate(&mut command).map_err(failure)?)
                .map_err(failure)?;
        let mut gate = admission.spawn().map_err(failure)?;
        drop(writer);
        if let Err(error) = gate.resume() {
            let primary = failure(error);
            return match gate.stop() {
                Ok(_) => Err(primary),
                Err(cleanup) => Err(failure(format!("{primary}; cleanup: {cleanup}"))),
            };
        }
        Ok(Self {
            gate,
            socket,
            percent: 0,
            stage: ProgressStage::Preparing,
            transfer: None,
            transfer_path,
            result: None,
            finished: false,
        })
    }

    pub(crate) fn percent(&self) -> u8 {
        self.percent
    }

    pub(crate) fn stage(&self) -> ProgressStage {
        self.stage
    }

    pub(crate) fn transfer(&self) -> Option<install::transfer::Progress> {
        self.transfer
    }

    fn receive_transfer(&mut self) {
        if matches!(
            self.stage,
            ProgressStage::Downloading
                | ProgressStage::DownloadingAndValidating
                | ProgressStage::InstallingPackage
                | ProgressStage::Dependencies
        ) {
            self.transfer = install::transfer::read_snapshot(&self.transfer_path);
        }
    }

    fn receive(&mut self) -> Result<(), Error> {
        // A sender cannot starve input, cancellation or rendering with progress.
        for _ in 0..64 {
            let mut bytes = [0; FRAME + 1];
            match self.socket.recv(&mut bytes) {
                Ok(FRAME) => {
                    let message = decode(&bytes[..FRAME]).map_err(failure)?;
                    if self.result.is_some() {
                        return Err(failure("message after final status"));
                    }
                    match message {
                        Message::Progress(percent, stage) => {
                            self.percent = percent;
                            self.stage = stage;
                            if !matches!(
                                stage,
                                ProgressStage::Downloading
                                    | ProgressStage::DownloadingAndValidating
                                    | ProgressStage::InstallingPackage
                                    | ProgressStage::Dependencies
                            ) {
                                self.transfer = None;
                            }
                        }
                        Message::Final(result) => self.result = Some(result),
                    }
                }
                Ok(_) => return Err(failure("invalid status frame length")),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(failure(error)),
            }
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Result<Option<Result<(), Error>>, Error> {
        self.receive()?;
        self.receive_transfer();
        let Some(status) = self.gate.try_wait().map_err(failure)? else {
            return Ok(None);
        };
        // Gate completion synchronizes the final send. The bounded kernel queue
        // may contain more than one tick of progress; drain at most 4096 frames.
        for _ in 0..64 {
            self.receive()?;
        }
        self.finished = true;
        let _ = std::fs::remove_file(&self.transfer_path);
        Ok(Some(match self.result {
            Some(Completion::Success) if status.success() => Ok(()),
            Some(result) if result != Completion::Success => Err(Error::Io(format!(
                "{}; check .asb/install.log in the selected project",
                result.diagnostic()
            ))),
            _ => Err(failure(
                "missing final status or unsuccessful gate exit; check .asb/install.log",
            )),
        }))
    }

    pub(crate) fn cancel(&mut self) -> Result<(), Error> {
        let result = self.gate.stop().map(|_| ()).map_err(failure);
        self.finished = true;
        let _ = std::fs::remove_file(&self.transfer_path);
        result
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        // ManagedGate already owns the pre-established fallback; never spawn
        // or join a worker here, and never directly kill an admitted guardian.
        if !self.finished
            && let Err(error) = self.cancel()
        {
            // Drop may already be unwinding. A broken diagnostic sink must
            // never cause a second panic before terminal restoration runs.
            let _ = writeln!(io::stderr().lock(), "{error}");
        }
    }
}

fn validate_names(names: &[String]) -> io::Result<()> {
    if names.is_empty()
        || names.len() > MAX_NAMES
        || names.iter().any(|name| {
            name.is_empty()
                || name.len() > MAX_NAME
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    {
        return Err(io::Error::other("invalid installation identifiers"));
    }
    Ok(())
}

fn frame(kind: u8, percent: u8) -> [u8; FRAME] {
    let mut bytes = [0; FRAME];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4] = kind;
    bytes[5] = percent;
    bytes
}

fn progress_frame(percent: u8, stage: ProgressStage) -> [u8; FRAME] {
    let mut bytes = frame(PROGRESS, percent);
    bytes[6] = stage as u8;
    bytes
}

fn decode(bytes: &[u8]) -> io::Result<Message> {
    if bytes.len() != FRAME
        || bytes[..4] != MAGIC
        || bytes[4] > FAILURE
        || bytes[5] > 100
        || bytes[7] != 0
        || (bytes[4] != PROGRESS && bytes[6] != 0)
        || (bytes[4] == SUCCESS && bytes[5] != 0)
    {
        return Err(io::Error::other("invalid installation status frame"));
    }
    match (bytes[4], bytes[5]) {
        (PROGRESS, percent) => ProgressStage::from_code(bytes[6])
            .map(|stage| Message::Progress(percent, stage))
            .ok_or_else(|| io::Error::other("invalid installation progress stage")),
        (SUCCESS, 0) => Ok(Message::Final(Completion::Success)),
        (FAILURE, 1) => Ok(Message::Final(Completion::Input)),
        (FAILURE, 2) => Ok(Message::Final(Completion::Io)),
        (FAILURE, 3) => Ok(Message::Final(Completion::Run)),
        (FAILURE, 4) => Ok(Message::Final(Completion::Usage)),
        _ => Err(io::Error::other("invalid installation final status")),
    }
}

fn set_cloexec(fd: i32, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                fd,
                libc::F_SETFD,
                if enabled {
                    flags | libc::FD_CLOEXEC
                } else {
                    flags & !libc::FD_CLOEXEC
                },
            )
        } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn entry(args: &[OsString]) -> ExitCode {
    match helper(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::from(2),
    }
}

fn helper(args: &[OsString]) -> Result<(), ()> {
    // Validate all framing before calling installers or touching project files.
    if args.len() < 5 || args.len() > MAX_NAMES + 4 || args[0] != VERSION {
        return Err(());
    }
    let mode = args[1].to_str().ok_or(())?;
    if !matches!(mode, "tool" | "workload" | "dependencies") {
        return Err(());
    }
    let root = PathBuf::from(&args[2]);
    let directory = PathBuf::from(&args[3]);
    if !root.is_absolute()
        || !directory.is_absolute()
        || std::env::current_dir().map_err(|_| ())?
            != std::fs::canonicalize(&root).map_err(|_| ())?
    {
        return Err(());
    }
    let names = args[4..]
        .iter()
        .map(|name| name.to_str().map(str::to_owned).ok_or(()))
        .collect::<Result<Vec<_>, _>>()?;
    validate_names(&names).map_err(|_| ())?;
    if mode != "dependencies" && names.len() != 1 {
        return Err(());
    }
    let descriptor = std::env::var(FD_ENV)
        .map_err(|_| ())?
        .parse::<i32>()
        .map_err(|_| ())?;
    if descriptor <= 2 {
        return Err(());
    }
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
        || kind != libc::SOCK_DGRAM
    {
        return Err(());
    }
    set_cloexec(descriptor, true).map_err(|_| ())?;
    // SAFETY: this private inherited descriptor is consumed exactly once.
    let socket = unsafe { UnixDatagram::from_raw_fd(descriptor) };
    socket.set_nonblocking(true).map_err(|_| ())?;
    let mut progress = |percent: u8, stage: ProgressStage| {
        // Full buffers discard transient progress. No raw installer message,
        // subprocess diagnostic or credential can enter this fixed protocol.
        let _ = socket.send(&progress_frame(percent.min(100), stage));
    };
    let mut output = io::sink();
    let result = match mode {
        "tool" => install::tool_with_progress(
            install::ToolInstallOptions {
                name: names[0].clone(),
                source: None,
                dir: Some(directory),
                dry_run: false,
            },
            &mut output,
            &mut progress,
        ),
        "workload" => install::workload_with_progress(
            install::WorkloadInstallOptions {
                source: names[0].clone(),
                name: None,
                dir: Some(directory),
            },
            &mut output,
            &mut progress,
        ),
        _ => install::ensure_collector_dependencies(
            &names,
            &root,
            &directory,
            &mut output,
            &mut progress,
            false,
        ),
    };
    if let Err(error) = &result {
        // Retain the original error using the existing project install-log
        // policy, never its contents in frames. This is not a privacy guarantee.
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(".asb/install.log"))
        {
            let _ = writeln!(log, "ASB installation failed: {error}");
        }
    }
    let final_frame = match &result {
        Ok(()) => frame(SUCCESS, 0),
        Err(Error::Input(_)) => frame(FAILURE, 1),
        Err(Error::Io(_)) => frame(FAILURE, 2),
        Err(Error::Run(_)) => frame(FAILURE, 3),
        Err(Error::Usage(_)) => frame(FAILURE, 4),
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match socket.send(&final_frame) {
            Ok(FRAME) => return result.map_err(|_| ()),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => return Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Receiver/custody qualification, separate from the real helper/PTY matrix:
    /// a real native guardian supervises a local program, while this fixture
    /// owns the only status sender and deliberately violates its wire contract.
    fn receiver_fixture(script: &str) -> (Operation, UnixDatagram) {
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("asb");
        let mut launch = ProcessLaunch::new("/bin/sh");
        launch.arg("-c").arg(script);
        let (mut command, route) = launch.native(Some(&binary));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let admission =
            process_gate::Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
        let mut gate = admission.spawn().unwrap();
        gate.resume().unwrap();
        let (socket, writer) = UnixDatagram::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        (
            Operation {
                gate,
                socket,
                percent: 0,
                stage: ProgressStage::Preparing,
                transfer: None,
                transfer_path: std::env::temp_dir().join(format!(
                    "asb-install-transfer-test-{}",
                    crate::tools::runtime::unique_id()
                )),
                result: None,
                finished: false,
            },
            writer,
        )
    }

    #[test]
    fn actual_receiver_rejects_bad_order_frames_and_completion_disagreement() {
        let scope = crate::process_cleanup::test_support::Scope::new();
        for frames in [
            vec![frame(SUCCESS, 0).to_vec(), frame(SUCCESS, 0).to_vec()],
            vec![
                frame(SUCCESS, 0).to_vec(),
                progress_frame(40, ProgressStage::Building).to_vec(),
            ],
            vec![vec![0; FRAME]],
            vec![vec![0; FRAME + 1]],
        ] {
            let (mut operation, writer) = receiver_fixture("exit 0");
            for frame in frames {
                writer.send(&frame).unwrap();
            }
            assert!(
                operation.poll().is_err(),
                "malformed/order-violating frames must fail at the actual receiver"
            );
            operation.cancel().unwrap();
        }
        for (script, send_success) in [("exit 0", false), ("exit 7", true)] {
            let (mut operation, writer) = receiver_fixture(script);
            if send_success {
                writer.send(&frame(SUCCESS, 0)).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let error = loop {
                if let Some(result) = operation.poll().unwrap() {
                    break result.unwrap_err();
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(
                error
                    .to_string()
                    .contains("missing final status or unsuccessful gate exit")
            );
            assert_eq!(
                operation.gate.try_wait().unwrap().unwrap().success(),
                !send_success
            );
        }
        scope.wait_finished();
    }

    #[test]
    fn actual_receiver_caps_a_flooded_tick_while_guardian_is_live() {
        let scope = crate::process_cleanup::test_support::Scope::new();
        let (mut operation, writer) = receiver_fixture("exec /bin/sleep 30");
        let mut sent = 0;
        loop {
            match writer.send(&progress_frame(
                (sent % 100) as u8,
                ProgressStage::Downloading,
            )) {
                Ok(FRAME) => sent += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                result => panic!("unexpected send {result:?}"),
            }
            assert!(sent < 10000);
        }
        assert!(sent > 64);
        let started = Instant::now();
        assert!(operation.poll().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            operation.percent(),
            63,
            "one tick must consume only its 64-frame budget"
        );
        assert_eq!(operation.stage(), ProgressStage::Downloading);
        operation.cancel().unwrap();
        drop(operation);
        scope.wait_finished();
    }
    #[test]
    fn protocol_is_fixed_and_rejects_untrusted_messages() {
        for code in 1..=13 {
            let stage = ProgressStage::from_code(code).unwrap();
            assert_eq!(
                decode(&progress_frame(80, stage)).unwrap(),
                Message::Progress(80, stage)
            );
        }
        for code in [0, 14, 255] {
            let mut unknown = progress_frame(10, ProgressStage::Preparing);
            unknown[6] = code;
            assert!(decode(&unknown).is_err());
        }
        let mut reserved = progress_frame(10, ProgressStage::Preparing);
        reserved[7] = 1;
        assert!(decode(&reserved).is_err());
        let mut final_stage = frame(SUCCESS, 0);
        final_stage[6] = ProgressStage::Ready as u8;
        assert!(decode(&final_stage).is_err());
        assert_eq!(
            decode(&frame(SUCCESS, 0)).unwrap(),
            Message::Final(Completion::Success)
        );
        assert_eq!(
            decode(&frame(FAILURE, 2)).unwrap(),
            Message::Final(Completion::Io)
        );
        for bytes in [
            vec![],
            vec![0; 8],
            frame(PROGRESS, 101).to_vec(),
            frame(SUCCESS, 1).to_vec(),
        ] {
            assert!(decode(&bytes).is_err());
        }
        assert!(validate_names(&["secret\nvalue".into()]).is_err());
        assert!(validate_names(&vec!["x".into(); MAX_NAMES + 1]).is_err());
    }
    #[test]
    fn helper_rejects_bad_framing_without_installing() {
        assert!(helper(&[]).is_err());
        assert!(helper(&[OsString::from("old")]).is_err());
    }

    #[test]
    fn nonblocking_progress_pressure_never_waits_for_a_reader() {
        let (reader, writer) = UnixDatagram::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let started = Instant::now();
        let mut sent = 0;
        loop {
            match writer.send(&progress_frame(50, ProgressStage::Downloading)) {
                Ok(FRAME) => sent += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected send: {other:?}"),
            }
            assert!(sent < 10000, "progress queue is not bounded");
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        reader.set_nonblocking(true).unwrap();
        let mut received = 0;
        while reader.recv(&mut [0; FRAME]).is_ok() {
            received += 1;
        }
        assert_eq!(received, sent);
    }

    #[test]
    fn unwinding_an_operation_stops_its_actual_installer() {
        const CASE: &str = "ASB_INSTALL_UNWIND_ROOT";
        if let Some(root) = std::env::var_os(CASE) {
            let root = PathBuf::from(root);
            let pidfile = root.join("principal");
            let scope = crate::process_cleanup::test_support::Scope::new();
            std::panic::set_hook(Box::new(|_| {}));
            let unwind = std::panic::catch_unwind(|| {
                crate::tui::with_terminal_for_test(|| {
                    let operation = Operation::start(
                        root.clone(),
                        Request::Tool {
                            name: "pi".into(),
                            directory: root.join("tools"),
                        },
                    )
                    .unwrap();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !pidfile.exists() {
                        assert!(Instant::now() < deadline, "installer never started");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if std::env::var_os("ASB_INSTALL_UNWIND_FAULT").is_some() {
                        operation.gate.lose_receipt_for_test();
                    }
                    panic!("fixture unwind after installer admission");
                });
            });
            assert!(unwind.is_err());
            scope.wait_finished();
            let pid = std::fs::read_to_string(pidfile).unwrap();
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            return;
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/install-operation-fixtures")
            .join(crate::workloads::TaskIdentity::local("install", None).attempt_id);
        for directory in [".asb", "bin", "home"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        for (name, script) in [
            ("node", "#!/bin/sh\nprintf 'v22.0.0\\n'\n"),
            (
                "npm",
                "#!/bin/sh\nprintf '%s' $$ > principal\nexec /usr/bin/sleep 60\n",
            ),
        ] {
            let path = root.join("bin").join(name);
            crate::write_test_file(&root, &path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let current = std::env::current_exe().unwrap();
        let binary = current.parent().unwrap().parent().unwrap().join("asb");
        let name = format!(
            "{}::unwinding_an_operation_stops_its_actual_installer",
            module_path!().split_once("::").unwrap().1
        );
        let mut command = std::process::Command::new(current);
        command
            .args(["--exact", &name, "--nocapture"])
            .env_clear()
            .env(CASE, &root)
            .env("ASB_RUNTIME_EXECUTABLE", binary)
            .env("HOME", root.join("home"))
            .env("PATH", root.join("bin"))
            .current_dir(&root);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        for fault in [false, true] {
            if fault {
                command.env("ASB_INSTALL_UNWIND_FAULT", "1");
                std::fs::remove_file(root.join("principal")).unwrap();
            }
            command
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/full")
                        .unwrap(),
                );
            assert!(
                command.status().unwrap().success(),
                "unwind must not panic a second time on failed stderr"
            );
            let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
            assert_eq!(
                unsafe { libc::tcgetattr(slave.as_raw_fd(), attributes.as_mut_ptr()) },
                0
            );
            let attributes = unsafe { attributes.assume_init() };
            assert_ne!(
                attributes.c_lflag & libc::ECHO,
                0,
                "unwind must restore echo"
            );
            assert_ne!(
                attributes.c_lflag & libc::ICANON,
                0,
                "unwind must restore canonical mode"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeper_failure_starts_no_installation() {
        let scope = crate::process_cleanup::test_support::Scope::new();
        crate::process_cleanup::test_support::set_creation_failure(true);
        let root = PathBuf::from("/unused-install-fixture");
        let result = Operation::start(
            root.clone(),
            Request::Tool {
                name: "pi".into(),
                directory: root,
            },
        );
        crate::process_cleanup::test_support::set_creation_failure(false);
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("fixture resource exhaustion")
        );
        assert_eq!(scope.count(), 0);
    }
}
