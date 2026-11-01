//! Explicit client intent, kept separate from the command carrying its gate.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Adapter-owned executable and initial arguments, before shared preparation.
pub(crate) struct ProcessLaunch {
    pub(crate) program: OsString,
    args: Vec<OsString>,
    arg0: Option<OsString>,
}

/// Frozen interpretation of a configured command, never inferred from gate argv.
pub(crate) struct TargetLaunch {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) path: Option<OsString>,
}

/// Private framing, including explicit client `argv[0]`, not the gate's `argv[0]`.
pub(crate) const FORMAT: &str = "--asb-launch-v1";

pub(crate) struct CommandRoute {
    program: OsString,
    prefix: usize,
    gated: bool,
    #[cfg(target_os = "linux")]
    carrier: OsString,
    #[cfg(target_os = "linux")]
    framing: Vec<OsString>,
}

/// Validation holds exclusive command access until admission takes custody.
#[cfg(target_os = "linux")]
pub(crate) struct ValidatedGate<'a>(&'a mut Command);

#[cfg(target_os = "linux")]
impl<'a> ValidatedGate<'a> {
    pub(crate) fn into_command(self) -> &'a mut Command {
        self.0
    }
}

impl ProcessLaunch {
    pub(crate) fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            arg0: None,
        }
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn arg0(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.arg0 = Some(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn direct(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        #[cfg(unix)]
        if let Some(arg0) = &self.arg0 {
            use std::os::unix::process::CommandExt;
            command.arg0(arg0);
        }
        command
    }

    /// Materialize once, before configuration. Never reconstruct this command
    /// from getters: std owns env_clear, I/O, credentials and pre-exec state.
    pub(crate) fn native(&self, gate: Option<&Path>) -> (Command, CommandRoute) {
        let (command, prefix) = if let Some(gate) = gate {
            let mut command = Command::new(gate);
            command.env_remove("ASB_GATE_INSTALL_LOCK_FDS");
            command.arg("__asb-gate").arg(FORMAT);
            match &self.arg0 {
                Some(arg0) => {
                    command.arg("1").arg(arg0);
                }
                None => {
                    command.arg("0");
                }
            }
            command.arg(&self.program).args(&self.args);
            let prefix = command.get_args().count() - self.args.len();
            (command, prefix)
        } else {
            (self.direct(), 0)
        };
        #[cfg(target_os = "linux")]
        let carrier = command.get_program().to_owned();
        #[cfg(target_os = "linux")]
        let framing = command.get_args().map(OsStr::to_owned).collect();
        (
            command,
            CommandRoute {
                program: self.program.clone(),
                prefix,
                gated: gate.is_some(),
                #[cfg(target_os = "linux")]
                carrier,
                #[cfg(target_os = "linux")]
                framing,
            },
        )
    }
}

impl CommandRoute {
    pub(crate) fn is_gated(&self) -> bool {
        self.gated
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn validate_gate<'a>(
        self,
        command: &'a mut Command,
    ) -> std::io::Result<ValidatedGate<'a>> {
        if !self.gated {
            return Err(std::io::Error::other(
                "native gate admission requires a gated launch route",
            ));
        }
        if command.get_program() != self.carrier
            || !command
                .get_args()
                .take(self.framing.len())
                .eq(self.framing.iter().map(OsString::as_os_str))
        {
            return Err(std::io::Error::other(
                "prepared native command no longer matches its gate executable or target framing; configure the original command without replacing it",
            ));
        }
        Ok(ValidatedGate(command))
    }

    pub(crate) fn target(&self, command: &Command) -> TargetLaunch {
        TargetLaunch::from_parts(&self.program, command.get_args().skip(self.prefix), command)
    }
}

impl TargetLaunch {
    pub(crate) fn direct(command: &Command) -> Self {
        Self::from_parts(command.get_program(), command.get_args(), command)
    }

    fn from_parts<'a>(
        program: &OsStr,
        args: impl Iterator<Item = &'a OsStr>,
        command: &Command,
    ) -> Self {
        Self {
            program: program.to_owned(),
            args: args.map(OsStr::to_owned).collect(),
            cwd: command.get_current_dir().map(Path::to_path_buf),
            // Only an explicit assignment is proof of this command's PATH.
            // No assignment does not prove inheritance: env_clear is opaque.
            path: command
                .get_envs()
                .find_map(|(key, value)| (key == "PATH").then(|| value.map(OsStr::to_owned)))
                .flatten(),
        }
    }

    /// Resolve the exact program that this frozen command would launch. The
    /// result is an execution identity input, never a recorded pathname.
    pub(crate) fn resolved_executable(&self) -> Option<PathBuf> {
        let cwd = self.cwd.as_deref().unwrap_or_else(|| Path::new("."));
        let inherited_path = std::env::var_os("PATH");
        let path = self.path.as_deref().or(inherited_path.as_deref());
        crate::tools::host_executable_in_at(Path::new(&self.program), path, cwd)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
