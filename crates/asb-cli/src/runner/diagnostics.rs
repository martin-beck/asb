//! Optional continuation evidence is distinct from a fresh benchmark outcome.
use crate::{Error, io_error};
use serde::Serialize;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Outcome,
    Replica,
    Group,
    InputInitial,
    InputFinal,
}
impl Stage {
    fn code(self) -> &'static str {
        match self {
            Self::Outcome => "task_outcome_unavailable",
            Self::Replica => "replica_receipt_unavailable",
            Self::Group => "group_receipt_unavailable",
            Self::InputInitial => "initial_input_seal_unavailable",
            Self::InputFinal => "input_seal_unavailable",
        }
    }
    fn evidence(self) -> &'static str {
        match self {
            Self::Outcome => "private task outcome",
            Self::Replica => "replica continuation receipt",
            Self::Group => "group continuation receipt",
            Self::InputInitial => "initial input-stability seal",
            Self::InputFinal => "final input-stability seal",
        }
    }
}

/// Never echo arbitrary observer/helper errors: they can contain private input
/// data. Keep useful filesystem classes and an internal cause identity instead.
fn safe_reason(error: &Error) -> &'static str {
    let text = error.to_string();
    for (needle, reason) in [
        (
            "output capture is incomplete",
            "agent output capture is incomplete; preserve results for inspection",
        ),
        ("Permission denied", "permission denied"),
        ("No space left", "storage is full"),
        ("Read-only file system", "storage is read-only"),
        ("Not a directory", "a required parent is not a directory"),
        ("Is a directory", "a required file path is a directory"),
        ("No such file", "a required file or directory is missing"),
        ("File exists", "the evidence destination already exists"),
        (
            "usable local Python",
            "a usable local Python 3 interpreter is required for evidence verification",
        ),
        (
            "symlink or non-directory",
            "the evidence directory is a symlink or non-directory",
        ),
        ("deadline", "evidence verification exceeded its deadline"),
        ("changed", "existing evidence changed or does not match"),
        (
            "cannot be reused",
            "existing evidence did not pass continuation validation",
        ),
        (
            "could not be sealed",
            "declared inputs are missing, changing, unsupported or excessive",
        ),
        (
            "runtime inputs discovery failed safely with code `resource_bound`",
            "declared runtime inputs exceed the supported verification bound",
        ),
        (
            "runtime inputs discovery failed safely with code `required_runtime_unavailable`",
            "a declared runtime executable is unavailable or not executable",
        ),
        (
            "runtime inputs discovery failed safely with code `runtime_entry_unsupported`",
            "a declared runtime entry has an unsupported executable or interpreter format",
        ),
        (
            "runtime inputs discovery failed safely with code `runtime_input_validation_failed`",
            "a declared runtime input could not be safely verified",
        ),
    ] {
        if text.contains(needle) {
            return reason;
        }
    }
    match error {
        Error::Io(_) => "evidence storage or publication failed",
        Error::Input(_) | Error::Usage(_) => "evidence validation failed",
        Error::Run(_) => "evidence verification or publication failed",
    }
}

#[derive(Serialize)]
pub(crate) struct Warning {
    schema_version: u32,
    event: &'static str,
    code: &'static str,
    evidence: &'static str,
    path: String,
    path_role: &'static str,
    reason: &'static str,
    remediation: &'static str,
    #[serde(skip)]
    identity: String,
}
impl Warning {
    pub(crate) fn new(stage: Stage, scope: &Path, path: &Path, error: &Error) -> Self {
        let mut cause = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(error).hash(&mut cause);
        error.to_string().hash(&mut cause);
        Self {
            schema_version: 1,
            event: "continuation_warning",
            code: stage.code(),
            evidence: stage.evidence(),
            path: path.to_string_lossy().into_owned(),
            path_role: "destination",
            reason: safe_reason(error),
            remediation: "Preserve the existing results and repair the reported storage or evidence problem. Missing start records or seals may not be reconstructable. Do not rerun completed tasks to recreate evidence; inspect `asb sweep --help` for explicit continuation and reconciliation options.",
            identity: format!(
                "continuation:{}:{}:{:016x}",
                stage.code(),
                scope.display(),
                cause.finish()
            ),
        }
    }
    pub(crate) fn source_outcome(mut self) -> Self {
        self.path_role = "source_outcome";
        self
    }
    pub(crate) fn emit(
        &self,
        json: bool,
        output: &mut dyn Write,
        seen: &Mutex<BTreeSet<String>>,
    ) -> Result<(), Error> {
        let mut seen = seen
            .lock()
            .map_err(|_| Error::Run("continuation diagnostic state unavailable".into()))?;
        if seen.contains(&self.identity) {
            return Ok(());
        }
        if json {
            serde_json::to_writer(&mut *output, self).map_err(|error| {
                Error::Io(format!("cannot write continuation diagnostic: {error}"))
            })?;
            writeln!(output).map_err(io_error)?;
        } else {
            let location = if self.path_role == "source_outcome" {
                format!(
                    "; source outcome at {} (receipt destination unknown)",
                    self.path.escape_debug()
                )
            } else {
                format!(" at {}", self.path.escape_debug())
            };
            writeln!(output,"Warning: {} unavailable{}: {}. Existing grades are unchanged.\nNext: preserve results and repair this evidence; see `asb sweep --help` for reconciliation limits. Do not rerun completed tasks.",self.evidence,location,self.reason).map_err(io_error)?;
        }
        // A failed writer must not suppress a later successfully delivered
        // diagnostic from another replica sharing this scope.
        seen.insert(self.identity.clone());
        Ok(())
    }
}

pub(crate) fn preserve_primary(
    primary: Result<(), Error>,
    presentation: Result<(), Error>,
) -> Result<(), Error> {
    match (primary, presentation) {
        (primary, Ok(())) => primary,
        (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(output)) => {
            let detail = format!(
                "{primary}; result or continuation diagnostic output also failed: {output}"
            );
            // Preserve the primary diagnostic, but never classify broken
            // result output as an ordinary completed agent/grader failure.
            // Repeated admission must stop after a presentation IO error.
            Err(match output {
                Error::Io(_) => Error::Io(detail),
                Error::Input(_) => Error::Input(detail),
                Error::Usage(_) => Error::Input(detail),
                Error::Run(_) => Error::Run(detail),
            })
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/continuation_diagnostics.rs"]
mod tests;
