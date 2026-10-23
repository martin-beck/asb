//! Explicit host-setup requests. Capability is not consent, and consent for one
//! group never authorizes another. The backend is injectable without a host CLI.
use crate::Error;
use std::io::{BufRead, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SudoMode {
    Passwordless,
    Interactive,
}
#[derive(Clone, Copy)]
pub(super) enum Requirement {
    Passwordless,
    AllowInteractive,
}
#[derive(Clone, Copy)]
pub(super) enum FailurePolicy {
    Continue,
    Stop,
}

#[derive(Clone, Debug)]
pub(super) struct Operation {
    pub args: Vec<String>,
    pub input: Option<Vec<u8>>,
    pub terminal: bool,
}
impl Operation {
    pub fn command(args: &[&str], terminal: bool) -> Self {
        Self {
            args: args.iter().map(|s| (*s).into()).collect(),
            input: None,
            terminal,
        }
    }
    pub fn write(path: &str, contents: &[u8]) -> Self {
        Self {
            args: vec!["tee".into(), path.into()],
            input: Some(contents.into()),
            terminal: false,
        }
    }
    fn display(&self, requirement: Requirement) -> String {
        let args = self
            .args
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(" ");
        let noninteractive = if matches!(requirement, Requirement::Passwordless) {
            " -n"
        } else {
            ""
        };
        match &self.input {
            Some(bytes) => format!(
                "printf %s {} | sudo{noninteractive} {args} >/dev/null",
                quote(&String::from_utf8_lossy(bytes))
            ),
            None => format!("sudo{noninteractive} {args}"),
        }
    }
}
fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.=:+".contains(&c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
pub(super) struct Request {
    pub heading: &'static str,
    pub explanation: &'static str,
    pub operations: Vec<Operation>,
    pub requirement: Requirement,
    pub failure: FailurePolicy,
}
// Private fields prevent execution without passing the request/consent boundary.
pub(super) struct Approved {
    operations: Vec<Operation>,
    mode: SudoMode,
    failure: FailurePolicy,
}
pub(super) enum Decision {
    Approved(Approved),
    Declined,
    Unavailable,
}
pub(super) trait Executor {
    fn passwordless(&mut self) -> bool;
    fn execute(&mut self, mode: SudoMode, operation: &Operation) -> bool;
}
pub(super) struct SystemExecutor;
impl Executor for SystemExecutor {
    fn passwordless(&mut self) -> bool {
        std::process::Command::new("sudo")
            .args(["-n", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    fn execute(&mut self, mode: SudoMode, operation: &Operation) -> bool {
        let mut command = std::process::Command::new("sudo");
        if mode == SudoMode::Passwordless {
            command.arg("-n");
        }
        command.args(&operation.args);
        if operation.terminal {
            command
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
        } else {
            command
                .stdin(if operation.input.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let wrote = match &operation.input {
            Some(contents) => child
                .stdin
                .take()
                .is_some_and(|mut input| input.write_all(contents).is_ok()),
            None => true,
        };
        // Always reap, even if writing the child's stdin failed.
        let exited = child.wait().is_ok_and(|s| s.success());
        wrote && exited
    }
}
pub(super) struct Policy<'a> {
    reader: &'a mut dyn BufRead,
    executor: &'a mut dyn Executor,
    capability: Option<bool>,
}
impl<'a> Policy<'a> {
    pub fn new(reader: &'a mut dyn BufRead, executor: &'a mut dyn Executor) -> Self {
        Self {
            reader,
            executor,
            capability: None,
        }
    }
    pub fn passwordless(&mut self, output: &mut dyn Write) -> Result<bool, Error> {
        if let Some(available) = self.capability {
            return Ok(available);
        }
        writeln!(
            output,
            "Checking passwordless sudo with: sudo -n true (read-only capability probe)."
        )
        .map_err(crate::io_error)?;
        let available = self.executor.passwordless();
        self.capability = Some(available);
        Ok(available)
    }
    pub fn approve(
        &mut self,
        output: &mut dyn Write,
        yes: bool,
        request: Request,
    ) -> Result<Decision, Error> {
        writeln!(output, "{}", request.heading).map_err(crate::io_error)?;
        for operation in &request.operations {
            writeln!(output, "  {}", operation.display(request.requirement))
                .map_err(crate::io_error)?;
        }
        writeln!(output, "{}", request.explanation).map_err(crate::io_error)?;
        if !consent(self.reader, output, yes)? {
            return Ok(Decision::Declined);
        }
        let mode = if self.passwordless(output)? {
            SudoMode::Passwordless
        } else if matches!(request.requirement, Requirement::AllowInteractive) {
            writeln!(output, "Passwordless sudo is unavailable. Because you explicitly approved the commands above, sudo will now ask for your password.").map_err(crate::io_error)?;
            SudoMode::Interactive
        } else {
            writeln!(output, "Host capability setup: passwordless sudo is unavailable; the listed commands were not run.").map_err(crate::io_error)?;
            return Ok(Decision::Unavailable);
        };
        if mode == SudoMode::Passwordless {
            writeln!(
                output,
                "Applying the commands above with passwordless sudo."
            )
            .map_err(crate::io_error)?;
        }
        output.flush().map_err(crate::io_error)?;
        Ok(Decision::Approved(Approved {
            operations: request.operations,
            mode,
            failure: request.failure,
        }))
    }
    pub fn execute(&mut self, approved: Approved) -> Vec<bool> {
        let mut results = Vec::new();
        for operation in approved.operations {
            let success = self.executor.execute(approved.mode, &operation);
            results.push(success);
            if !success && matches!(approved.failure, FailurePolicy::Stop) {
                break;
            }
        }
        results
    }
}
pub(super) fn parse_confirmation(answer: &str) -> Option<bool> {
    let answer = answer.strip_suffix('\n').unwrap_or(answer);
    let answer = answer.strip_suffix('\r').unwrap_or(answer);
    match answer {
        "y" | "Y" => Some(true),
        "n" | "N" => Some(false),
        "" => Some(false),
        _ => None,
    }
}
pub(super) fn consent(
    reader: &mut dyn BufRead,
    output: &mut dyn Write,
    yes: bool,
) -> Result<bool, Error> {
    if yes {
        writeln!(
            output,
            "Privileged host setup approved by --yes; no confirmation prompt will be shown."
        )
        .map_err(crate::io_error)?;
        output.flush().map_err(crate::io_error)?;
        return Ok(true);
    }
    loop {
        write!(output, "Apply the listed privileged commands? y/N: ").map_err(crate::io_error)?;
        output.flush().map_err(crate::io_error)?;
        let mut answer = String::new();
        let read = reader
            .read_line(&mut answer)
            .map_err(|e| Error::Io(format!("cannot read host capability confirmation: {e}")))?;
        if read == 0 {
            writeln!(output, "No answer received; defaulting to no.").map_err(crate::io_error)?;
            return Ok(false);
        }
        match parse_confirmation(&answer) {
            Some(value) => return Ok(value),
            None => writeln!(output, "Please answer y or n.").map_err(crate::io_error)?,
        }
    }
}

#[cfg(test)]
mod tests;
