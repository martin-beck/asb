//! Compatibility backend only: retains the previous direct-child policy.
//! This is NOT Linux tree-custody or bounded-pipe qualification for other hosts.
//! In particular, destination locks cannot survive abrupt owner death here;
//! guardian descriptor transfer is implemented only by the Linux backend.
use super::*;
use std::process::Stdio;

#[derive(Clone, Debug, Default)]
pub(crate) struct LogReport {
    pub(crate) path: Option<PathBuf>,
    pub(crate) retained: usize,
    pub(crate) accepted: usize,
    pub(crate) discarded: usize,
    pub(crate) fault: Option<LogFault>,
    pub(crate) settled: bool,
    pub(crate) truncated: bool,
}

pub(super) fn run(
    mut step: PreparedStep,
    lease: ProducerLease,
    ticket: AdmissionTicket,
) -> InstallResult<SettledStep> {
    // Preserve the former non-Linux direct-child semantics explicitly. Do not
    // label direct-child exit a descendant-quiescence receipt or invent one.
    let result = match step.capture {
        Capture::Status => {
            let path = step.context.root.join(".asb/install.log");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(crate::io_error)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(crate::io_error)?;
            step.command
                .stdout(Stdio::from(file.try_clone().map_err(crate::io_error)?))
                .stderr(Stdio::from(file));
            step.command.status().map(|status| SettledStep {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
                log: LogReport {
                    path: Some(path),
                    settled: true,
                    ..Default::default()
                },
                lease,
            })
        }
        Capture::Inspect { .. } => step.command.output().map(|output| SettledStep {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
            log: LogReport {
                settled: true,
                ..Default::default()
            },
            lease,
        }),
    };
    match result {
        Ok(result) => {
            ticket.settle(ProducerProof::DirectChildOnly);
            Ok(result)
        }
        Err(error) => {
            // An output/stdio error cannot prove no child was admitted.
            ticket.poison();
            Err(crate::io_error(error).into())
        }
    }
}
