//! Read-only campaign layout and exact outcome references. No resume authority.
use super::{OutcomeReference, Plan, Slot};
use crate::{Error, io_error};
use serde_json::Value;
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAX_PLAN_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REFERENCE_BYTES: u64 = 16 * 1024;
const MAX_OUTCOME_BYTES: u64 = 1024 * 1024;
const MAX_OBSERVATION_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) struct Layout {
    pub plan: Plan,
    pub root: PathBuf,
    remaining: Cell<u64>,
    shared_remaining: Option<Rc<Cell<u64>>>,
}

pub(crate) struct ReferencedOutcome {
    pub path: PathBuf,
    pub value: Value,
    pub bytes: u64,
    pub sha256: String,
}

fn invalid(reason: &str) -> Error {
    Error::Input(format!("invalid campaign report input: {reason}"))
}

pub(crate) fn bounded(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    bounded_file(file, limit)
}

/// Descriptor-relative metadata/read entry points for private receipt owners.
/// Unlike final-component O_NOFOLLOW, these also reject aliased ancestors.
pub(crate) fn inspect_relative(root: &Path, relative: &Path) -> Result<Option<u64>, Error> {
    let Some(file) = open_relative(root, relative)? else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(invalid("relative input is not a regular file"));
    }
    Ok(Some(metadata.len()))
}

pub(crate) fn bounded_relative(root: &Path, relative: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let file =
        open_relative(root, relative)?.ok_or_else(|| invalid("relative input is missing"))?;
    bounded_file(file, limit)
}

/// Retain the exact regular-file descriptor whose bounded bytes were read.
/// Publication recovery uses it to confirm durability without a pathname reopen.
pub(crate) fn pinned_bytes(
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<(File, Vec<u8>), Error> {
    let file = pinned_regular(root, relative, limit)?;
    let bytes = bounded_file(file.try_clone().map_err(io_error)?, limit)?;
    Ok((file, bytes))
}

/// Open a confined regular descriptor without reading its contents. Metadata
/// alias recovery compares file identities before hashing the one matched leaf.
pub(crate) fn pinned_regular(root: &Path, relative: &Path, limit: u64) -> Result<File, Error> {
    let file =
        open_relative(root, relative)?.ok_or_else(|| invalid("relative input is missing"))?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid("input must be a bounded regular file"));
    }
    Ok(file)
}

/// Any existing slot-owned object prevents untouched admission. Directories
/// are observations too; aliases/errors must never be mistaken for absence.
pub(crate) fn present_relative(root: &Path, relative: &Path) -> Result<bool, Error> {
    Ok(open_relative(root, relative)?.is_some())
}

pub(crate) fn list_relative(
    root: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<Vec<OsString>, Error> {
    // Generation metadata has a final admission, optional successor link and
    // fixed successors directory besides configuration manifests, plus at most
    // one recoverable pending hard-link alias. Slot callers still pass their
    // independent 65,533-record maximum.
    if maximum > 65_537 {
        return Err(invalid("directory inventory exceeds its bound"));
    }
    #[cfg(unix)]
    {
        rooted::list(root, relative, maximum)
    }
    #[cfg(not(unix))]
    {
        let Some(directory) = open_relative(root, relative)? else {
            return Ok(Vec::new());
        };
        if !directory.metadata().map_err(io_error)?.is_dir() {
            return Err(invalid("completion inventory is not a directory"));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(root.join(relative)).map_err(io_error)? {
            if entries.len() >= maximum {
                return Err(invalid("completion inventory exceeds population bound"));
            }
            entries.push(entry.map_err(io_error)?.file_name());
        }
        entries.sort();
        Ok(entries)
    }
}

fn open_relative(root: &Path, relative: &Path) -> Result<Option<File>, Error> {
    #[cfg(unix)]
    {
        rooted::open_optional(root, relative)
    }
    #[cfg(not(unix))]
    {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid("relative input path is not canonical"));
        }
        let mut path = root.to_path_buf();
        for part in relative.components() {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(invalid("relative input ancestor is aliased"));
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_error(error)),
            }
        }
        File::open(path).map(Some).map_err(io_error)
    }
}

fn bounded_file(file: File, limit: u64) -> Result<Vec<u8>, Error> {
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid("input must be a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(metadata.len() + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(invalid("input length changed while reading"));
    }
    Ok(bytes)
}

impl Layout {
    pub(crate) fn share_observation_budget(&mut self, remaining: Rc<Cell<u64>>) {
        self.shared_remaining = Some(remaining);
    }
    /// Read a declared metadata leaf through the same confined descriptor and
    /// observation budget as outcomes. This does not grant resume authority.
    pub(crate) fn declaration(&self, relative: &Path, limit: u64) -> Result<Value, Error> {
        serde_json::from_slice(&self.declaration_bytes(relative, limit)?)
            .map_err(|_| invalid("malformed campaign declaration"))
    }

    pub(crate) fn declaration_bytes(&self, relative: &Path, limit: u64) -> Result<Vec<u8>, Error> {
        self.read_observation(&self.root.join(relative), limit)
    }

    fn replay_file(&self, input: &super::ReplayInput) -> Result<File, Error> {
        #[cfg(unix)]
        let file = rooted::open(&self.root, &input.private)?;
        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .read(true)
            .open(self.root.join(&input.private))
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.len() != input.bytes {
            return Err(invalid(
                "private replay input type or length differs from plan",
            ));
        }
        Ok(file)
    }

    /// Metadata-only admission: callers inspect every campaign before hashing
    /// any private payload. This does not read the original source archive.
    pub(crate) fn inspect_private_replay(&self) -> Result<(), Error> {
        for input in &self.plan.replay_inputs {
            self.replay_file(input)?;
        }
        Ok(())
    }

    /// Bind staged bytes to their original declaration, not merely a later
    /// stability seal. Streaming uses one descriptor and a fixed-size buffer.
    /// This alone proves neither slot settlement nor permission to resume.
    pub(crate) fn verify_private_replay(&self) -> Result<(), Error> {
        use sha2::{Digest, Sha256};
        use std::io::Seek;
        for input in &self.plan.replay_inputs {
            let mut source = self.replay_file(input)?;
            if self.plan.requires_raw_replay_requests() {
                crate::cassette::require_raw_requests(std::io::BufReader::new(&mut source))
                    .map_err(|error| invalid(&error.to_string()))?;
                source.rewind().map_err(io_error)?;
            }
            let mut file = source.take(input.bytes + 1);
            let mut digest = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(io_error)?;
                if count == 0 {
                    break;
                }
                bytes += count as u64;
                digest.update(&buffer[..count]);
            }
            if bytes != input.bytes || format!("{:x}", digest.finalize()) != input.sha256 {
                return Err(invalid("private replay input bytes differ from plan"));
            }
        }
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Self, Error> {
        let bytes = bounded(path, MAX_PLAN_BYTES)?;
        let layout = Self::decode(path, &bytes)?;
        layout.preflight()?;
        Ok(layout)
    }

    /// Read only a referenced plan. Callers validate every campaign/index
    /// association before admitting any observation or recording payload read.
    pub fn read_reference(results: &Path, reference: &super::Reference) -> Result<Self, Error> {
        let relative = reference.root.join("plan.json");
        #[cfg(unix)]
        let bytes = bounded_file(rooted::open(results, &relative)?, MAX_PLAN_BYTES)?;
        #[cfg(not(unix))]
        let bytes = bounded(&results.join(&relative), MAX_PLAN_BYTES)?;
        if bytes.len() as u64 != reference.bytes
            || crate::cassette::digest(&bytes) != reference.sha256
        {
            return Err(invalid(
                "campaign plan differs from its owning sweep reference",
            ));
        }
        Self::decode(&results.join(relative), &bytes)
    }

    fn decode(path: &Path, bytes: &[u8]) -> Result<Self, Error> {
        let plan: Plan =
            serde_json::from_slice(bytes).map_err(|_| invalid("malformed execution plan"))?;
        plan.validate_layout()?;
        let root = std::path::absolute(
            path.parent()
                .ok_or_else(|| invalid("plan needs a campaign parent"))?,
        )
        .map_err(io_error)?;
        if root.file_name().and_then(|name| name.to_str()) != Some(plan.campaign.as_str())
            || !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir())
        {
            return Err(invalid(
                "plan campaign does not match its real root directory",
            ));
        }
        let layout = Self {
            plan,
            root,
            remaining: Cell::new(MAX_OBSERVATION_BYTES),
            shared_remaining: None,
        };
        Ok(layout)
    }

    /// Bound the complete primary observation population before any outcome
    /// hash or recording payload verification. Actual reads share another budget
    /// so growth/replacement after this inspection cannot evade the cap.
    pub fn preflight(&self) -> Result<u64, Error> {
        let mut total = 0u64;
        let mut inspect = |path: &Path, limit: u64| -> Result<(), Error> {
            let metadata = match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(io_error(error)),
                Ok(metadata) => metadata,
            };
            if !metadata.is_file() || metadata.len() > limit {
                return Err(invalid("observation must be a bounded regular file"));
            }
            total = total
                .checked_add(metadata.len())
                .filter(|value| *value <= MAX_OBSERVATION_BYTES)
                .ok_or_else(|| {
                    invalid(
                        "aggregate primary observations exceed 256 MiB; select a smaller campaign",
                    )
                })?;
            Ok(())
        };
        for slot in &self.plan.slots {
            let reference_path = self.root.join(format!(
                "batch-{}/outcomes/{}.json",
                slot.batch, slot.replica
            ));
            inspect(&reference_path, MAX_REFERENCE_BYTES)?;
            if reference_path.exists() {
                let bytes = self.read_observation(&reference_path, MAX_REFERENCE_BYTES)?;
                let reference: OutcomeReference = serde_json::from_slice(&bytes)
                    .map_err(|_| invalid("malformed outcome reference"))?;
                self.validate_reference(slot, &reference)?;
                inspect(&self.root.join(reference.path), MAX_OUTCOME_BYTES)?;
            }
            inspect(&self.root.join(&slot.log), MAX_PLAN_BYTES)?;
            if let Some(directory) = self.measurement_directory(slot) {
                inspect(&directory.join("run.json"), MAX_PLAN_BYTES)?;
            }
        }
        Ok(total)
    }

    fn read_observation(&self, path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
        let limit = limit.min(self.remaining.get()).min(
            self.shared_remaining
                .as_ref()
                .map_or(u64::MAX, |remaining| remaining.get()),
        );
        #[cfg(unix)]
        let bytes = {
            let (root, relative) = if let Ok(relative) = path.strip_prefix(&self.root) {
                (&self.root, relative)
            } else {
                let root = self
                    .plan
                    .measurement_root
                    .as_ref()
                    .ok_or_else(|| invalid("observation is outside campaign roots"))?;
                (
                    root,
                    path.strip_prefix(root)
                        .map_err(|_| invalid("observation is outside campaign roots"))?,
                )
            };
            bounded_file(rooted::open(root, relative)?, limit)?
        };
        #[cfg(not(unix))]
        let bytes = bounded(path, limit)?;
        self.remaining
            .set(self.remaining.get() - bytes.len() as u64);
        if let Some(remaining) = &self.shared_remaining {
            remaining.set(remaining.get() - bytes.len() as u64);
        }
        Ok(bytes)
    }

    pub fn measurement(&self, slot: &Slot) -> Result<Option<Value>, Error> {
        let Some(directory) = self.measurement_directory(slot) else {
            return Ok(None);
        };
        let path = directory.join("run.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
            Ok(_) => (),
        }
        serde_json::from_slice(&self.read_observation(&path, MAX_PLAN_BYTES)?)
            .map(Some)
            .map_err(|_| invalid("malformed measurement record"))
    }

    pub fn measurement_directory(&self, slot: &Slot) -> Option<PathBuf> {
        let relative = slot.measurements.as_ref()?;
        let root = if self.plan.measurement_storage == Some(super::MeasurementStorage::Campaign) {
            &self.root
        } else {
            self.plan
                .measurement_root
                .as_ref()
                .expect("validated measurement root")
        };
        Some(root.join(relative))
    }

    /// Missing references remain missing observations; malformed/present ones
    /// must not silently fall back to an unrelated or newer log.
    pub fn outcome(&self, slot: &Slot) -> Result<Option<ReferencedOutcome>, Error> {
        let path = self.root.join(format!(
            "batch-{}/outcomes/{}.json",
            slot.batch, slot.replica
        ));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
            Ok(_) => (),
        }
        let bytes = self.read_observation(&path, MAX_REFERENCE_BYTES)?;
        let reference: OutcomeReference =
            serde_json::from_slice(&bytes).map_err(|_| invalid("malformed outcome reference"))?;
        self.validate_reference(slot, &reference)?;
        let path = self.root.join(&reference.path);
        let bytes = self.read_observation(&path, MAX_OUTCOME_BYTES)?;
        if bytes.len() as u64 != reference.bytes
            || crate::cassette::digest(&bytes) != reference.sha256
        {
            return Err(invalid(
                "referenced outcome bytes differ from immutable association",
            ));
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("malformed referenced outcome"))?;
        if value["schema_version"] != 2
            || value["replica"]
                != serde_json::json!({"index":slot.replica,"total":self.plan.concurrency})
            || value["summary"]["execution"]
                != serde_json::to_value(self.plan.execution(slot))
                    .map_err(|error| invalid(&error.to_string()))?
            || value["summary"]["task"] != reference.task
        {
            return Err(invalid(
                "outcome summary differs from planned execution or referenced task",
            ));
        }
        Ok(Some(ReferencedOutcome {
            path,
            value,
            bytes: reference.bytes,
            sha256: reference.sha256,
        }))
    }

    fn validate_reference(&self, slot: &Slot, reference: &OutcomeReference) -> Result<(), Error> {
        if reference.schema_version != 1
            || reference.execution != self.plan.execution(slot)
            || reference.bytes > MAX_OUTCOME_BYTES
            || !crate::cassette::valid_digest(&reference.sha256)
        {
            return Err(invalid(
                "outcome reference identity or bounds differ from planned slot",
            ));
        }
        let name = reference
            .path
            .file_name()
            .ok_or_else(|| invalid("outcome reference has no filename"))?;
        let expected = PathBuf::from(format!("batch-{}/.asb-completions", slot.batch)).join(name);
        if reference.path.as_os_str() != expected.as_os_str()
            || reference
                .path
                .extension()
                .is_none_or(|value| value != "json")
            || name.to_str().is_none_or(|name| name.starts_with('.'))
        {
            return Err(invalid(
                "outcome reference is outside its batch completion namespace",
            ));
        }
        Ok(())
    }

    pub fn log(&self, slot: &Slot) -> Result<Option<(PathBuf, Value)>, Error> {
        let path = self.root.join(&slot.log);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
            Ok(_) => (),
        }
        let bytes = self.read_observation(&path, MAX_PLAN_BYTES)?;
        let mut lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace));
        let line = lines
            .next()
            .ok_or_else(|| invalid("empty execution-private log"))?;
        if lines.next().is_some() {
            return Err(invalid("multiple records claim one execution-private log"));
        }
        let value: Value =
            serde_json::from_slice(line).map_err(|_| invalid("malformed execution-private log"))?;
        if value["execution"]
            != serde_json::to_value(self.plan.execution(slot))
                .map_err(|error| invalid(&error.to_string()))?
        {
            return Err(invalid("execution log differs from planned slot identity"));
        }
        Ok(Some((path, value)))
    }
}

/// Descriptor-relative traversal rejects intermediate aliases as well as final
/// symlinks. This is path confinement, not historical artifact authentication.
#[cfg(unix)]
mod rooted {
    #![allow(unsafe_code)]
    use super::*;
    use std::ffi::{CStr, CString, OsStr};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

    fn child(parent: &File, name: &OsStr, directory: bool) -> std::io::Result<File> {
        let name = CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::other("invalid observation component"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: the borrowed parent remains open, name is NUL-terminated, and
        // successful openat yields a fresh descriptor owned only by this File.
        let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    pub(super) fn open(root: &Path, relative: &Path) -> Result<File, Error> {
        open_optional(root, relative)?.ok_or_else(|| invalid("observation path is missing"))
    }

    pub(super) fn list(
        root: &Path,
        relative: &Path,
        maximum: usize,
    ) -> Result<Vec<OsString>, Error> {
        let Some(file) = open_optional(root, relative)? else {
            return Ok(Vec::new());
        };
        if !file.metadata().map_err(io_error)?.is_dir() {
            return Err(invalid("completion inventory is not a directory"));
        }
        let descriptor = file.into_raw_fd();
        // SAFETY: the fresh descriptor is transferred to fdopendir exactly
        // once. On failure it remains ours and is closed by the restored File.
        let pointer = unsafe { libc::fdopendir(descriptor) };
        let Some(pointer) = std::ptr::NonNull::new(pointer) else {
            let error = std::io::Error::last_os_error();
            drop(unsafe { File::from_raw_fd(descriptor) });
            return Err(io_error(error));
        };
        struct Directory(std::ptr::NonNull<libc::DIR>);
        impl Drop for Directory {
            fn drop(&mut self) {
                // SAFETY: Directory exclusively owns this live DIR stream.
                unsafe {
                    libc::closedir(self.0.as_ptr());
                }
            }
        }
        let directory = Directory(pointer);
        let mut names = Vec::new();
        let mut name_bytes = 0usize;
        loop {
            errno::set_errno(errno::Errno(0));
            // SAFETY: this stream remains exclusively borrowed until the next
            // readdir; its returned name is copied before advancing the stream.
            let entry = unsafe { libc::readdir(directory.0.as_ptr()) };
            if entry.is_null() {
                let error = errno::errno();
                if error.0 != 0 {
                    return Err(io_error(error.into()));
                }
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if matches!(bytes, b"." | b"..") {
                continue;
            }
            name_bytes = name_bytes
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= 16 * 1024 * 1024)
                .ok_or_else(|| invalid("completion inventory names exceed 16 MiB"))?;
            if names.len() >= maximum {
                return Err(invalid("completion inventory exceeds population bound"));
            }
            names.push(OsStr::from_bytes(bytes).to_os_string());
        }
        names.sort();
        Ok(names)
    }

    pub(super) fn open_optional(root: &Path, relative: &Path) -> Result<Option<File>, Error> {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid("observation path is not relative and canonical"));
        }
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(io_error)?;
        let mut parts = relative.components().peekable();
        while let Some(part) = parts.next() {
            let std::path::Component::Normal(name) = part else {
                return Err(invalid("observation path is not relative and canonical"));
            };
            let file = match child(&directory, name, parts.peek().is_some()) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_error(error)),
            };
            if parts.peek().is_none() {
                return Ok(Some(file));
            }
            directory = file;
        }
        Err(invalid("observation path has no filename"))
    }
}
