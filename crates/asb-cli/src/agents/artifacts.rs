//! Attempt-scoped scratch ownership, frozen before interpretation and settled once.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct ArtifactSpec {
    pub namespace: &'static str,
    pub filename: &'static str,
    pub format: ArtifactFormat,
}

/// The bounded, parseable form expected from an agent-owned artifact.
///
/// JSON is the historical default.  JSONL is used only for the causal
/// local-tool transcript, whose individual events are validated by its owning
/// replay module before it enters a cassette.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ArtifactFormat {
    Json,
    JsonLines,
    /// Bounded private UTF-8 text, interpreted only by the owning adapter.
    Text,
}

struct OwnedFile {
    path: PathBuf,
    format: ArtifactFormat,
    namespace: File,
    directory: Observation,
    observed: Option<Observation>,
    snapshot: Option<Vec<u8>>,
    retain: bool,
    #[cfg(test)]
    before_open: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    before_move: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    snapshot_sync_failure: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity(u64, u128);
struct Observation {
    metadata: Metadata,
    identity: Option<Identity>,
}
impl Observation {
    fn read(file: &File) -> std::io::Result<Self> {
        Ok(Self {
            metadata: file.metadata()?,
            identity: identity(file)?,
        })
    }
}

fn open_regular_or_directory(path: &Path) -> std::io::Result<File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("artifact symlink refused"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Match std's handle-based metadata route; open a reparse point itself,
        // never its target, and permit directory handles for namespace identity.
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    options.open(path)
}

#[cfg(unix)]
fn identity(file: &File) -> std::io::Result<Option<Identity>> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(Some(Identity(metadata.dev(), metadata.ino().into())))
}
#[cfg(windows)]
#[allow(unsafe_code)]
fn identity(file: &File) -> std::io::Result<Option<Identity>> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(std::io::Error::other("artifact reparse point refused"));
    }
    // FILE_ID_INFO retains the complete 128-bit identifier, including ReFS.
    // FileIdInfo=18 matches the pinned std Windows ABI declarations.
    #[repr(C)]
    struct Information {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *mut std::ffi::c_void,
            length: u32,
        ) -> i32;
    }
    let mut information = std::mem::MaybeUninit::<Information>::uninit();
    // SAFETY: std owns this live handle; the API initializes Information on success.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            information.as_mut_ptr().cast(),
            std::mem::size_of::<Information>() as u32,
        )
    } == 0
    {
        // A filesystem without stable IDs retains private artifacts rather
        // than disabling an otherwise supported agent or guessing ownership.
        return Ok(None);
    }
    let information = unsafe { information.assume_init() };
    Ok(Some(Identity(
        information.volume,
        u128::from_le_bytes(information.id),
    )))
}
#[cfg(not(any(unix, windows)))]
fn identity(_file: &File) -> std::io::Result<Option<Identity>> {
    Ok(None)
}

#[derive(Default)]
pub(crate) struct ArtifactOwner {
    files: Vec<OwnedFile>,
    settled: bool,
    frozen: bool,
}

#[derive(Default)]
pub(crate) struct ArtifactSettlement {
    pub evidence: Vec<String>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct FrozenArtifacts {
    documents: BTreeMap<String, serde_json::Value>,
    bytes: BTreeMap<String, Vec<u8>>,
    pub error: bool,
    diagnostics: Vec<String>,
}
impl FrozenArtifacts {
    pub fn document(&self, filename: &str) -> Option<&serde_json::Value> {
        self.documents.get(filename)
    }
    pub fn bytes(&self, filename: &str) -> Option<&[u8]> {
        self.bytes.get(filename).map(Vec::as_slice)
    }
    pub fn diagnostic(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| self.diagnostics.join("; "))
    }

    /// Serialize a frozen, adapter-owned snapshot for an isolated guest's
    /// host-side collection phase.  The envelope is generic custody data: its
    /// payload is not interpreted until the owning adapter receives the
    /// reconstructed `FrozenArtifacts` after the provider tape has settled.
    pub(crate) fn deferred_collection_bytes(&self) -> Result<Vec<u8>, crate::Error> {
        #[derive(serde::Serialize)]
        struct Envelope<'a> {
            schema_version: u8,
            documents: &'a BTreeMap<String, serde_json::Value>,
            bytes: &'a BTreeMap<String, Vec<u8>>,
        }
        serde_json::to_vec(&Envelope {
            schema_version: 1,
            documents: &self.documents,
            bytes: &self.bytes,
        })
        .map_err(|_| crate::Error::Run("cannot encode deferred adapter artifacts".into()))
    }

    /// Reconstruct generic frozen custody data received through the bounded
    /// guest artifact channel.  This deliberately validates only the common
    /// envelope; adapter codecs remain responsible for their own documents.
    pub(crate) fn from_deferred_collection_bytes(bytes: &[u8]) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            schema_version: u8,
            documents: BTreeMap<String, serde_json::Value>,
            bytes: BTreeMap<String, Vec<u8>>,
        }
        let envelope: Envelope = serde_json::from_slice(bytes)
            .map_err(|_| "deferred adapter artifact receipt is invalid".to_owned())?;
        if envelope.schema_version != 1
            || envelope.documents.len() > 16
            || envelope.bytes.len() > 16
            || envelope
                .bytes
                .values()
                .try_fold(0usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|total| total > LIMIT as usize)
        {
            return Err("deferred adapter artifact receipt exceeds its bounds".into());
        }
        Ok(Self {
            documents: envelope.documents,
            bytes: envelope.bytes,
            error: false,
            diagnostics: Vec::new(),
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test(filename: &str, value: serde_json::Value) -> Self {
        Self {
            documents: [(filename.into(), value)].into_iter().collect(),
            bytes: BTreeMap::new(),
            error: false,
            diagnostics: Vec::new(),
        }
    }
}

fn same_identity(a: &Observation, b: &Observation) -> bool {
    identities_match(a.identity, b.identity)
}
fn identities_match(a: Option<Identity>, b: Option<Identity>) -> bool {
    a.zip(b).is_some_and(|(a, b)| a == b)
}
fn unchanged(a: &Observation, b: &Observation) -> bool {
    same_identity(a, b)
        && a.metadata.len() == b.metadata.len()
        && a.metadata
            .modified()
            .ok()
            .zip(b.metadata.modified().ok())
            .is_some_and(|(a, b)| a == b)
}

impl ArtifactOwner {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    pub fn prepare(run_dir: &Path, specs: &[ArtifactSpec]) -> Result<Self, crate::Error> {
        assert!(specs.len() <= 16);
        let mut owner = Self::default();
        // A namespace may contain several independently bounded artifacts.
        // Claim it exactly once, then pin a fresh descriptor for each child.
        // A pre-existing namespace still fails closed as foreign evidence.
        let mut claimed_namespaces = BTreeSet::new();
        for spec in specs {
            for component in [spec.namespace, spec.filename] {
                assert!(
                    !component.is_empty()
                        && component.len() <= 128
                        && component != "."
                        && component != ".."
                );
                assert!(
                    component
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                );
            }
            fs::create_dir_all(run_dir).map_err(crate::io_error)?;
            let directory = run_dir.join(spec.namespace);
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if claimed_namespaces.insert(spec.namespace) {
                builder.create(&directory).map_err(|_| {
                    crate::Error::Run(
                        "cannot create fresh agent artifact namespace; existing paths were preserved"
                            .into(),
                    )
                })?;
            }
            let namespace = open_regular_or_directory(&directory).map_err(crate::io_error)?;
            let metadata = Observation::read(&namespace).map_err(crate::io_error)?;
            owner.files.push(OwnedFile {
                path: directory.join(spec.filename),
                format: spec.format,
                namespace,
                directory: metadata,
                observed: None,
                snapshot: None,
                retain: false,
                #[cfg(test)]
                before_open: None,
                #[cfg(test)]
                before_move: None,
                #[cfg(test)]
                snapshot_sync_failure: false,
            });
        }
        Ok(owner)
    }

    pub fn freeze(&mut self) -> FrozenArtifacts {
        if self.frozen {
            return FrozenArtifacts {
                error: true,
                ..Default::default()
            };
        }
        self.frozen = true;
        let mut frozen = FrozenArtifacts::default();
        for owned in &mut self.files {
            let result = (|| {
                let directory = Observation::read(&open_regular_or_directory(
                    owned.path.parent().expect("artifact directory"),
                )?)?;
                if !directory.metadata.is_dir()
                    || ((directory.identity.is_some() || owned.directory.identity.is_some())
                        && !same_identity(&directory, &owned.directory))
                {
                    return Err(std::io::Error::other("artifact namespace replaced"));
                }
                #[cfg(test)]
                if let Some(hook) = owned.before_open.take() {
                    hook();
                }
                let file = match open_owned(owned, false) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    other => other?,
                };
                let before = Observation::read(&file)?;
                if !before.metadata.is_file() {
                    return Err(std::io::Error::other("artifact is not regular"));
                }
                owned.observed = Some(Observation::read(&file)?);
                if before.metadata.len() > LIMIT {
                    return Err(std::io::Error::other("artifact exceeds bound"));
                }
                let mut bytes = Vec::new();
                (&file).take(LIMIT + 1).read_to_end(&mut bytes)?;
                let after = Observation::read(&file)?;
                if bytes.len() as u64 > LIMIT
                    || before.metadata.len() != after.metadata.len()
                    || before.metadata.modified().ok() != after.metadata.modified().ok()
                    || ((before.identity.is_some() || after.identity.is_some())
                        && !same_identity(&before, &after))
                {
                    return Err(std::io::Error::other("artifact changed or exceeds bound"));
                }
                match owned.format {
                    ArtifactFormat::Json => serde_json::from_slice(&bytes)
                        .map(|document| Some((Some(document), bytes)))
                        .map_err(|_| std::io::Error::other("malformed artifact JSON")),
                    ArtifactFormat::JsonLines => Ok(Some((None, bytes))),
                    ArtifactFormat::Text => {
                        if std::str::from_utf8(&bytes).is_err() {
                            Err(std::io::Error::other("malformed artifact UTF-8"))
                        } else {
                            Ok(Some((None, bytes)))
                        }
                    }
                }
            })();
            match result {
                Ok(Some((document, bytes))) => {
                    owned.snapshot = Some(bytes);
                    let name = owned
                        .path
                        .file_name()
                        .expect("artifact filename")
                        .to_string_lossy()
                        .into_owned();
                    if let Some(document) = document {
                        frozen.documents.insert(name.clone(), document);
                    }
                    if let Some(snapshot) = owned.snapshot.as_ref() {
                        frozen.bytes.insert(name, snapshot.clone());
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    owned.retain = true;
                    frozen.error = true;
                    // Only locally selected fixed reasons cross this boundary;
                    // OS errors and JSON contents are never included.
                    let reason = match error.to_string().as_str() {
                        "artifact namespace replaced" => "namespace replaced",
                        "artifact exceeds bound" => "size bound exceeded",
                        "artifact changed or exceeds bound" => "snapshot changed or exceeded bound",
                        "malformed artifact JSON" => "malformed JSON",
                        "malformed artifact UTF-8" => "malformed UTF-8",
                        _ => "regular snapshot unavailable",
                    };
                    frozen.diagnostics.push(owned.diagnostic(reason));
                }
            }
        }
        frozen
    }

    /// Caller has frozen all evidence and completed independent grading/cleanup.
    pub fn settle(mut self) -> Result<(), String> {
        self.settle_inner()
    }

    /// Native publishers consume one immutable result. Transfer the exact
    /// validated bytes first, so even loss of every publisher cannot erase them.
    pub fn handoff_and_settle(mut self) -> ArtifactSettlement {
        let mut report = ArtifactSettlement::default();
        if !self.frozen {
            self.settled = true;
            report.error = Some(
                "agent artifact handoff requires a frozen snapshot; source not consumed".into(),
            );
            return report;
        }
        let mut failures = Vec::new();
        for owned in &mut self.files {
            let Some(bytes) = owned.snapshot.as_ref() else {
                continue;
            };
            let name = format!(
                "frozen-{}",
                owned.path.file_name().unwrap().to_string_lossy()
            );
            let copied = (|| {
                let mut file = create_snapshot(owned, &name)?;
                file.write_all(bytes)?;
                #[cfg(test)]
                if owned.snapshot_sync_failure {
                    return Err(std::io::Error::other(
                        "injected evidence synchronization failure",
                    ));
                }
                file.sync_all()?;
                // Unix directory synchronization confirms the new entry too.
                // Windows uses the write-through creation/file-flush route;
                // std does not provide a portable directory-flush contract.
                #[cfg(unix)]
                owned.namespace.sync_all()?;
                Ok::<_, std::io::Error>(())
            })();
            if copied.is_ok() {
                report.evidence.push(owned.location(&name));
            } else {
                // Never consume the only source when copying or synchronization
                // failed. Drop never retries this explicit finalization.
                owned.retain = true;
                failures
                    .push(owned.diagnostic("frozen evidence handoff failed; source not consumed"));
            }
        }
        if let Err(error) = self.settle_inner() {
            failures.push(error);
        }
        report.error = (!failures.is_empty()).then(|| failures.join("; "));
        report
    }

    fn settle_inner(&mut self) -> Result<(), String> {
        self.settled = true;
        let mut failures = Vec::new();
        for owned in &mut self.files {
            let result = (|| {
                if owned.retain {
                    return Err(std::io::Error::other("retention required"));
                }
                let parent = owned.path.parent().expect("artifact directory");
                let directory = Observation::read(&open_regular_or_directory(parent)?)?;
                if !directory.metadata.is_dir() || !same_identity(&directory, &owned.directory) {
                    return Err(std::io::Error::other("artifact directory changed"));
                }
                match open_owned(owned, true)
                    .and_then(|file| Observation::read(&file).map(|metadata| (file, metadata)))
                {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Ok((file, metadata))
                        if metadata.metadata.is_file()
                            && owned
                                .observed
                                .as_ref()
                                .map_or(!self.frozen, |before| unchanged(before, &metadata)) =>
                    {
                        #[cfg(test)]
                        if let Some(hook) = owned.before_move.take() {
                            hook();
                        }
                        dispose_child(owned, &file, &metadata)?;
                    }
                    _ => return Err(std::io::Error::other("foreign artifact retained")),
                }
                // Do not path-delete the namespace: a replacement can arrive
                // after any identity check. Empty private namespaces are retained.
                Ok(())
            })();
            if let Err(error) = result {
                let reason = if owned.directory.identity.is_none() {
                    "scratch settlement failed: stable file identity unavailable"
                } else {
                    match error.to_string().as_str() {
                        "retention required" => {
                            "scratch settlement failed: evidence requires retention"
                        }
                        "artifact directory changed" => {
                            "scratch settlement failed: namespace changed"
                        }
                        "foreign artifact retained" => {
                            "scratch settlement failed: file changed or nonregular"
                        }
                        "foreign artifact moved to custody; retained" => {
                            "scratch settlement failed: raced replacement retained in custody"
                        }
                        _ => {
                            "scratch settlement failed: ownership or filesystem operation unavailable"
                        }
                    }
                };
                failures.push(owned.diagnostic(reason));
            }
        }
        if !failures.is_empty() {
            Err(failures.join("; "))
        } else {
            Ok(())
        }
    }
}
impl Drop for ArtifactOwner {
    fn drop(&mut self) {
        if !self.settled && !self.frozen {
            // Fallback only; never print or retry a completed explicit settlement.
            let _ = self.settle_inner();
        }
    }
}

impl OwnedFile {
    fn location(&self, filename: &str) -> String {
        let namespace = self.path.parent().unwrap();
        let attempt = namespace.parent().unwrap().file_name().unwrap();
        format!(
            "{}/{}/{}",
            attempt.to_string_lossy(),
            namespace.file_name().unwrap().to_string_lossy(),
            filename
        )
    }
    fn diagnostic(&self, reason: &str) -> String {
        format!(
            "agent artifact {}: {reason}; inspect source or custody (which may remain)",
            self.location(&self.path.file_name().unwrap().to_string_lossy())
        )
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn create_snapshot(owned: &OwnedFile, name: &str) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name)?;
    // Create-new and nofollow: never adopt or overwrite a client-created entry.
    let fd = unsafe {
        libc::openat(
            owned.namespace.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn create_snapshot(owned: &OwnedFile, name: &str) -> std::io::Result<File> {
    windows_child(
        &owned.namespace,
        std::ffi::OsStr::new(name),
        0x4010_0000,
        2,
        0x0020_0022,
    )
}

#[cfg(not(any(unix, windows)))]
fn create_snapshot(_owned: &OwnedFile, _name: &str) -> std::io::Result<File> {
    Err(std::io::Error::other(
        "pinned evidence creation unavailable; source not consumed",
    ))
}

fn open_owned(owned: &OwnedFile, delete: bool) -> std::io::Result<File> {
    #[cfg(any(unix, windows))]
    {
        open_child(&owned.namespace, owned.path.file_name().unwrap(), delete)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = delete;
        open_regular_or_directory(&owned.path)
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn open_child(directory: &File, name: &std::ffi::OsStr, _delete: bool) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())?;
    // SAFETY: the pinned directory and NUL-terminated single component live
    // across the call. The returned descriptor is uniquely owned by File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn dispose_child(owned: &OwnedFile, _file: &File, expected: &Observation) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(owned.path.file_name().unwrap().as_bytes())?;
    let custody = format!(
        ".custody-{}",
        crate::workloads::TaskIdentity::local("artifact", None).attempt_id
    );
    let custody_name = std::ffi::CString::new(custody)?;
    let fd = owned.namespace.as_raw_fd();
    // Create a fresh private destination, not a shared or pre-existing scratch
    // path. Its pinned descriptor remains our custody authority after renames.
    if unsafe { libc::mkdirat(fd, custody_name.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let directory = open_child(
        &owned.namespace,
        std::ffi::OsStr::new(custody_name.to_str().unwrap()),
        false,
    )?;
    let destination = c"source";
    // The destination is absent in our newly created custody directory. No
    // original pathname is ever unlinked after a separate identity check.
    if unsafe {
        libc::renameat(
            fd,
            name.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let moved = open_child(&directory, std::ffi::OsStr::new("source"), false)?;
    let actual = Observation::read(&moved)?;
    if !actual.metadata.is_file() || !unchanged(expected, &actual) {
        // A raced replacement is now retained, not deleted or restored over
        // another file. The fixed custody name remains discoverable locally.
        return Err(std::io::Error::other(
            "foreign artifact moved to custody; retained",
        ));
    }
    // Only our fresh private custody entry is removed. Foreign entries at the
    // original source or namespace pathname are never addressed again.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), destination.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn open_child(directory: &File, name: &std::ffi::OsStr, delete: bool) -> std::io::Result<File> {
    windows_child(
        directory,
        name,
        0x8010_0000 | if delete { 0x10000 } else { 0 },
        1,
        0x0020_0020,
    )
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_child(
    directory: &File,
    name: &std::ffi::OsStr,
    access: u32,
    disposition: u32,
    options: u32,
) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    #[repr(C)]
    struct Unicode {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct Attributes {
        length: u32,
        root: *mut std::ffi::c_void,
        name: *mut Unicode,
        attributes: u32,
        security: *mut std::ffi::c_void,
        qos: *mut std::ffi::c_void,
    }
    #[repr(C)]
    struct Status {
        status: usize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            handle: *mut *mut std::ffi::c_void,
            access: u32,
            attributes: *mut Attributes,
            status: *mut Status,
            allocation: *mut i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *mut std::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    let mut text: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(text.len() * 2).map_err(std::io::Error::other)?;
    let mut name = Unicode {
        length,
        maximum_length: length,
        buffer: text.as_mut_ptr(),
    };
    let mut attributes = Attributes {
        length: std::mem::size_of::<Attributes>() as u32,
        root: directory.as_raw_handle(),
        name: &mut name,
        attributes: 0x40,
        security: std::ptr::null_mut(),
        qos: std::ptr::null_mut(),
    };
    let mut status = Status {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // RootDirectory is the pinned namespace, not a re-resolved full path.
    // FILE_OPEN_REPARSE_POINT prevents traversal; identity() rejects reparse
    // handles before reading. Synchronous read handles are owned by std File.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut attributes,
            &mut status,
            std::ptr::null_mut(),
            0,
            7,
            disposition,
            options,
            std::ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        return Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn dispose_child(_owned: &OwnedFile, file: &File, _expected: &Observation) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            class: i32,
            information: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    // FILE_DISPOSITION_INFO marks the opened, identity-checked file object,
    // never a new file at its former name. Deletion completes on handle close.
    let delete: u8 = 1;
    if unsafe {
        SetFileInformationByHandle(file.as_raw_handle(), 4, (&delete as *const u8).cast(), 1)
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn dispose_child(_owned: &OwnedFile, _file: &File, _expected: &Observation) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "stable artifact settlement unavailable",
    ))
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn only_two_matching_stable_identities_authorize_destructive_settlement() {
        assert!(identities_match(Some(Identity(1, 2)), Some(Identity(1, 2))));
        for (a, b) in [
            (None, None),
            (Some(Identity(1, 2)), None),
            (None, Some(Identity(1, 2))),
            (Some(Identity(1, 2)), Some(Identity(2, 2))),
            (Some(Identity(1, 2)), Some(Identity(1, 3))),
            (Some(Identity(1, 2)), Some(Identity(1, 2 + (1_u128 << 100)))),
        ] {
            assert!(!identities_match(a, b));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const SPEC: &[ArtifactSpec] = &[ArtifactSpec {
        namespace: "scratch",
        filename: "evidence.json",
        format: ArtifactFormat::Json,
    }];
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/agent-artifact-unit")
                .join(crate::workloads::TaskIdentity::local("artifact", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn file(&self) -> PathBuf {
            self.0.join("scratch/evidence.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0.join("scratch"), fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn freeze_is_single_bounded_snapshot_and_settlement_preserves_replacements() {
        for replacement in ["file", "symlink", "same-inode", "new-after-absent"] {
            let fixture = Fixture::new();
            let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            if replacement != "new-after-absent" {
                fs::write(fixture.file(), r#"{"answer":"A","usage":34}"#).unwrap();
            }
            let frozen = owner.freeze();
            assert!(!frozen.error);
            if replacement != "new-after-absent" {
                assert_eq!(frozen.document("evidence.json").unwrap()["answer"], "A");
            }
            match replacement {
                "file" => {
                    fs::rename(fixture.file(), fixture.0.join("original.saved")).unwrap();
                    fs::write(fixture.file(), "foreign replacement").unwrap();
                }
                "symlink" => {
                    fs::rename(fixture.file(), fixture.0.join("original.saved")).unwrap();
                    fs::write(fixture.0.join("foreign"), "foreign target").unwrap();
                    symlink(fixture.0.join("foreign"), fixture.file()).unwrap();
                }
                _ => fs::write(fixture.file(), "changed foreign bytes").unwrap(),
            }
            assert!(owner.freeze().error, "never reread current bytes");
            assert!(owner.settle().is_err());
            assert!(fs::symlink_metadata(fixture.file()).is_ok());
            if replacement != "new-after-absent" {
                assert_eq!(frozen.document("evidence.json").unwrap()["usage"], 34);
            }
        }
    }

    #[test]
    fn no_output_malformed_partial_oversized_and_symlink_have_no_stale_document() {
        for content in ["absent", "{", "[]", "large", "symlink"] {
            let fixture = Fixture::new();
            fs::write(fixture.0.join("legacy.json"), r#"{"answer":"old"}"#).unwrap();
            let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            match content {
                "absent" => (),
                "large" => {
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(fixture.file())
                        .unwrap()
                        .set_len(LIMIT + 1)
                        .unwrap();
                }
                "symlink" => symlink(fixture.0.join("legacy.json"), fixture.file()).unwrap(),
                _ => fs::write(fixture.file(), content).unwrap(),
            }
            let frozen = owner.freeze();
            assert_eq!(frozen.error, matches!(content, "{" | "large" | "symlink"));
            assert!(
                frozen
                    .document("evidence.json")
                    .is_none_or(|value| value.get("answer").is_none())
            );
            assert_eq!(
                owner.settle().is_err(),
                matches!(content, "{" | "large" | "symlink")
            );
            assert_eq!(
                fs::read_to_string(fixture.0.join("legacy.json")).unwrap(),
                r#"{"answer":"old"}"#
            );
        }
    }

    #[test]
    fn jsonl_artifact_is_frozen_as_exact_private_bytes_not_parsed_as_json() {
        let fixture = Fixture::new();
        let spec = [ArtifactSpec {
            namespace: "tool-replay",
            filename: "local-tools.jsonl",
            format: ArtifactFormat::JsonLines,
        }];
        let mut owner = ArtifactOwner::prepare(&fixture.0, &spec).unwrap();
        let bytes = b"{\"schema_version\":1}\n{\"schema_version\":1}\n";
        fs::write(fixture.0.join("tool-replay/local-tools.jsonl"), bytes).unwrap();
        let frozen = owner.freeze();
        assert!(!frozen.error);
        assert_eq!(frozen.bytes("local-tools.jsonl"), Some(bytes.as_slice()));
        assert!(frozen.document("local-tools.jsonl").is_none());
        assert!(owner.settle().is_ok());
    }

    #[test]
    fn several_artifacts_can_share_one_fresh_owned_namespace() {
        let fixture = Fixture::new();
        let specs = [
            ArtifactSpec {
                namespace: "shared",
                filename: "first.json",
                format: ArtifactFormat::Json,
            },
            ArtifactSpec {
                namespace: "shared",
                filename: "second.jsonl",
                format: ArtifactFormat::JsonLines,
            },
        ];
        let mut owner = ArtifactOwner::prepare(&fixture.0, &specs).unwrap();
        fs::write(fixture.0.join("shared/first.json"), "{}\n").unwrap();
        fs::write(fixture.0.join("shared/second.jsonl"), "{\"event\":1}\n").unwrap();
        let frozen = owner.freeze();
        assert!(!frozen.error);
        assert_eq!(frozen.document("first.json"), Some(&serde_json::json!({})));
        assert_eq!(
            frozen.bytes("second.jsonl"),
            Some(b"{\"event\":1}\n".as_slice())
        );
        assert!(owner.settle().is_ok());
    }

    #[test]
    fn namespace_collision_and_replacement_are_never_adopted_or_deleted() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("scratch")).unwrap();
        assert!(ArtifactOwner::prepare(&fixture.0, SPEC).is_err());
        fs::remove_dir_all(fixture.0.join("scratch")).unwrap();
        let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
        fs::rename(fixture.0.join("scratch"), fixture.0.join("owned.saved")).unwrap();
        fs::create_dir(fixture.0.join("scratch")).unwrap();
        fs::write(fixture.file(), "foreign bytes").unwrap();
        assert!(owner.freeze().error);
        assert!(owner.settle().is_err());
        assert_eq!(fs::read_to_string(fixture.file()).unwrap(), "foreign bytes");
    }

    #[test]
    fn explicit_failed_settlement_is_not_retried_and_unwind_cleans_owned_file() {
        let fixture = Fixture::new();
        let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
        fs::write(fixture.file(), "{}").unwrap();
        owner.freeze();
        fs::set_permissions(fixture.0.join("scratch"), fs::Permissions::from_mode(0o500)).unwrap();
        assert!(owner.settle().is_err());
        assert!(fixture.file().is_file());
        fs::set_permissions(fixture.0.join("scratch"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(fixture.file()).unwrap();
        fs::remove_dir(fixture.0.join("scratch")).unwrap();
        let result = std::panic::catch_unwind(|| {
            let _owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            fs::write(fixture.file(), "{}").unwrap();
            panic!("owned artifact unwind fixture");
        });
        assert!(result.is_err());
        assert!(!fixture.file().exists());
        assert!(
            fixture.0.join("scratch").is_dir(),
            "never path-delete a namespace"
        );
    }

    #[test]
    fn replacement_in_namespace_check_to_open_window_cannot_redirect_snapshot() {
        let fixture = Fixture::new();
        let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
        fs::write(fixture.file(), r#"{"answer":"owned"}"#).unwrap();
        let root = fixture.0.clone();
        owner.files[0].before_open = Some(Box::new(move || {
            fs::rename(root.join("scratch"), root.join("owned.saved")).unwrap();
            fs::create_dir(root.join("scratch")).unwrap();
            fs::write(
                root.join("scratch/evidence.json"),
                r#"{"answer":"foreign"}"#,
            )
            .unwrap();
        }));
        let frozen = owner.freeze();
        assert!(!frozen.error);
        assert_eq!(frozen.document("evidence.json").unwrap()["answer"], "owned");
        assert!(owner.settle().is_err());
        assert_eq!(
            fs::read_to_string(fixture.file()).unwrap(),
            r#"{"answer":"foreign"}"#
        );
        assert!(fixture.0.join("owned.saved/evidence.json").is_file());
    }

    #[test]
    fn replacement_in_check_to_move_window_is_retained_in_custody_not_unlinked() {
        for symlink_replacement in [false, true] {
            let fixture = Fixture::new();
            let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            fs::write(fixture.file(), r#"{"answer":"owned"}"#).unwrap();
            assert!(!owner.freeze().error);
            let root = fixture.0.clone();
            owner.files[0].before_move = Some(Box::new(move || {
                fs::rename(root.join("scratch/evidence.json"), root.join("owned.saved")).unwrap();
                if symlink_replacement {
                    fs::write(root.join("foreign"), "foreign target").unwrap();
                    symlink(root.join("foreign"), root.join("scratch/evidence.json")).unwrap();
                } else {
                    fs::write(root.join("scratch/evidence.json"), "foreign replacement").unwrap();
                }
            }));
            let error = owner.settle().unwrap_err();
            assert!(error.contains("scratch/evidence.json"), "{error}");
            let custody = fs::read_dir(fixture.0.join("scratch"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".custody-")
                })
                .unwrap();
            assert_eq!(
                fs::read_to_string(custody.join("source")).unwrap(),
                if symlink_replacement {
                    "foreign target"
                } else {
                    "foreign replacement"
                }
            );
            if symlink_replacement {
                assert!(
                    fs::symlink_metadata(custody.join("source"))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    fs::read_to_string(fixture.0.join("foreign")).unwrap(),
                    "foreign target"
                );
            }
            assert_eq!(
                fs::read_to_string(fixture.0.join("owned.saved")).unwrap(),
                r#"{"answer":"owned"}"#
            );
        }
    }

    #[test]
    fn frozen_evidence_is_retained_on_publication_unwind_and_reasons_are_specific() {
        let fixture = Fixture::new();
        let result = std::panic::catch_unwind(|| {
            let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            fs::write(fixture.file(), "{secret malformed bytes").unwrap();
            let frozen = owner.freeze();
            let reason = frozen.diagnostic().unwrap();
            assert!(reason.contains("scratch/evidence.json: malformed JSON"));
            assert!(!reason.contains("secret"));
            panic!("publication failed");
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(fixture.file()).unwrap(),
            "{secret malformed bytes"
        );
    }

    #[test]
    fn native_handoff_uses_exact_frozen_bytes_and_never_refreshes_the_source() {
        let fixture = Fixture::new();
        let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
        let bytes = b"{ \"answer\": \"owned\", \"usage\": 34 }\n";
        fs::write(fixture.file(), bytes).unwrap();
        assert!(!owner.freeze().error);
        fs::rename(fixture.file(), fixture.0.join("original.saved")).unwrap();
        fs::write(fixture.file(), "foreign replacement bytes").unwrap();
        let report = owner.handoff_and_settle();
        assert!(report.error.unwrap().contains("file changed"));
        assert_eq!(report.evidence.len(), 1);
        assert!(report.evidence[0].contains(fixture.0.file_name().unwrap().to_str().unwrap()));
        let copy = fixture.0.join("scratch/frozen-evidence.json");
        assert_eq!(fs::read(&copy).unwrap(), bytes);
        assert_eq!(
            fs::metadata(copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_to_string(fixture.file()).unwrap(),
            "foreign replacement bytes"
        );
    }

    #[test]
    fn failed_handoff_never_consumes_original_or_overwrites_foreign_evidence() {
        for failure in ["file", "symlink", "permission", "sync"] {
            let fixture = Fixture::new();
            let mut owner = ArtifactOwner::prepare(&fixture.0, SPEC).unwrap();
            fs::write(fixture.file(), "{\"usage\":34}").unwrap();
            assert!(!owner.freeze().error);
            let destination = fixture.0.join("scratch/frozen-evidence.json");
            match failure {
                "file" => fs::write(&destination, "foreign evidence").unwrap(),
                "symlink" => {
                    fs::write(fixture.0.join("foreign"), "foreign target").unwrap();
                    symlink(fixture.0.join("foreign"), &destination).unwrap();
                }
                "permission" => fs::set_permissions(
                    fixture.0.join("scratch"),
                    fs::Permissions::from_mode(0o500),
                )
                .unwrap(),
                "sync" => owner.files[0].snapshot_sync_failure = true,
                _ => unreachable!(),
            }
            let report = owner.handoff_and_settle();
            assert!(
                report.evidence.is_empty(),
                "unconfirmed copies are not handed off"
            );
            assert!(
                report
                    .error
                    .unwrap()
                    .contains("handoff failed; source not consumed")
            );
            assert_eq!(
                fs::read_to_string(fixture.file()).unwrap(),
                "{\"usage\":34}"
            );
            if failure == "file" {
                assert_eq!(fs::read_to_string(destination).unwrap(), "foreign evidence");
            }
            if failure == "symlink" {
                assert_eq!(
                    fs::read_to_string(fixture.0.join("foreign")).unwrap(),
                    "foreign target"
                );
            }
        }
    }

    #[test]
    fn deferred_collection_round_trip_preserves_generic_frozen_snapshots() {
        let frozen = FrozenArtifacts {
            documents: [(
                "progress.json".into(),
                serde_json::json!({"complete": true}),
            )]
            .into_iter()
            .collect(),
            bytes: [("sidecar.jsonl".into(), b"event\n".to_vec())]
                .into_iter()
                .collect(),
            error: false,
            diagnostics: Vec::new(),
        };
        let restored = FrozenArtifacts::from_deferred_collection_bytes(
            &frozen.deferred_collection_bytes().unwrap(),
        )
        .unwrap();
        assert_eq!(
            restored.document("progress.json").unwrap()["complete"],
            true
        );
        assert_eq!(restored.bytes("sidecar.jsonl"), Some(&b"event\n"[..]));
    }

    #[test]
    fn deferred_collection_rejects_unknown_or_oversized_envelopes() {
        assert!(
            FrozenArtifacts::from_deferred_collection_bytes(
                br#"{"schema_version":1,"extra":true,"documents":{},"bytes":{}}"#
            )
            .is_err()
        );
        let bytes = vec![b'x'; LIMIT as usize + 1];
        let value =
            serde_json::json!({"schema_version":1,"documents":{},"bytes":{"sidecar":bytes}});
        assert!(
            FrozenArtifacts::from_deferred_collection_bytes(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
    }
}
