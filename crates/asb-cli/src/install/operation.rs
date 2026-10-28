//! A cancellable installation process, supervised by the existing native gate.
//!
//! The installer itself is deliberately synchronous. Killing a Rust thread
//! cannot cancel npm/pip/build descendants; the exclusive gate owns that tree.
#[cfg(not(target_os = "linux"))]
use crate::Error;
#[cfg(not(target_os = "linux"))]
use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) enum Request {
    Tool {
        name: String,
        directory: PathBuf,
    },
    Workload {
        name: String,
        directory: PathBuf,
    },
    Dependencies {
        selected: Vec<String>,
        directory: PathBuf,
    },
}

#[cfg(target_os = "linux")]
#[path = "operation_native.rs"]
mod native;
#[cfg(target_os = "linux")]
pub(crate) use native::{Operation, entry};

#[cfg(not(target_os = "linux"))]
pub(crate) struct Operation;

#[cfg(not(target_os = "linux"))]
impl Operation {
    pub(crate) fn start(_: PathBuf, request: Request) -> Result<Self, Error> {
        match request {
            Request::Tool { name, directory } | Request::Workload { name, directory } => {
                drop((name, directory))
            }
            Request::Dependencies {
                selected,
                directory,
            } => drop((selected, directory)),
        }
        Err(Error::Io(
            "TUI installation requires Linux native process supervision; use a supported host"
                .into(),
        ))
    }
    pub(crate) fn poll(&mut self) -> Result<Option<Result<(), Error>>, Error> {
        Err(Error::Io(
            "TUI installation requires Linux native process supervision".into(),
        ))
    }
    pub(crate) fn percent(&self) -> u8 {
        0
    }
    pub(crate) fn stage(&self) -> crate::install::ProgressStage {
        crate::install::ProgressStage::Preparing
    }
    pub(crate) fn transfer(&self) -> Option<crate::install::transfer::Progress> {
        None
    }
    pub(crate) fn cancel(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn entry(_: &[OsString]) -> std::process::ExitCode {
    std::process::ExitCode::from(2)
}
