//! Sealed, causal local-tool transcripts used by tape-backed agent adapters.
//!
//! This module deliberately knows nothing about a particular client or MCP
//! transport.  It records the semantic tool turn and the confined workspace
//! effects needed to make replay observationally equivalent without executing
//! a shell command, opening a network connection, or consulting host state.

use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 16_384;
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_SEARCH_FILES: usize = 512;
const MAX_SEARCH_MATCHES: usize = 128;
const MAX_SEARCH_DEPTH: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub schema_version: u32,
    pub sequence: u64,
    pub tool: String,
    pub request: Value,
    pub result: Value,
    /// A bounded, canonical error returned by the local-tool service instead
    /// of a result.  It is an observable tool turn: omitting it would shift
    /// every following replay request after a recorded tool failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Effect {
    CreateDirectory { path: String },
    WriteText { path: String, content: String },
    WriteBinary { path: String, content_hex: String },
    CreateSymlink { path: String, target: String },
    RemoveFile { path: String },
    RemoveDirectory { path: String },
}

#[derive(Default)]
pub(crate) struct Transcript {
    events: Vec<Event>,
    cursor: usize,
}

impl Transcript {
    pub(crate) fn record(&mut self, event: Event) -> Result<(), Error> {
        if self.events.len() >= MAX_EVENTS
            || event.schema_version != SCHEMA_VERSION
            || event.sequence != self.events.len() as u64 + 1
            || !valid_tool(&event.tool)
            || event.failure.as_ref().is_some_and(|failure| {
                failure.is_empty()
                    || failure.len() > 512
                    || failure.contains(['\n', '\r'])
                    || failure.chars().any(char::is_control)
            })
            || event.failure.is_some() && (!event.effects.is_empty() || !event.result.is_null())
        {
            return Err(Error::Run("invalid local-tool transcript event".into()));
        }
        self.events.push(event);
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        self.write_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Write each event independently so publishing a growing transcript does
    /// not allocate a second copy of the whole recording.
    pub(crate) fn write_to(&self, output: &mut impl std::io::Write) -> Result<(), Error> {
        for event in &self.events {
            // Count before publishing any bytes from this event. This preserves
            // the legacy size check without allocating another encoded copy.
            let mut size = EventSize::default();
            serde_json::to_writer(&mut size, event)
                .map_err(|_| Error::Io("cannot encode local-tool transcript".into()))?;
            if size.bytes > MAX_LINE_BYTES {
                return Err(Error::Run(
                    "local-tool transcript event exceeds bound".into(),
                ));
            }
            serde_json::to_writer(&mut *output, event)
                .map_err(|_| Error::Io("cannot write local-tool transcript".into()))?;
            output.write_all(b"\n").map_err(crate::io_error)?;
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_EVENTS * (MAX_LINE_BYTES + 1) {
            return Err(Error::Run("local-tool transcript exceeds bound".into()));
        }
        Self::decode_from(&mut std::io::BufReader::new(bytes))
    }

    /// Decode one framed event at a time without retaining a second raw copy
    /// of the full transcript alongside its parsed events.
    pub(crate) fn decode_from(reader: &mut impl std::io::BufRead) -> Result<Self, Error> {
        use std::io::{BufRead as _, Read as _};
        let mut transcript = Self::default();
        loop {
            let mut line = Vec::new();
            let read = (&mut *reader)
                .take((MAX_LINE_BYTES + 2) as u64)
                .read_until(b'\n', &mut line)
                .map_err(crate::io_error)?;
            if read == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(Error::Run(
                    "local-tool transcript event exceeds bound".into(),
                ));
            }
            let text = std::str::from_utf8(&line)
                .map_err(|_| Error::Run("local-tool transcript is not UTF-8 JSONL".into()))?;
            transcript.record(
                serde_json::from_str(text)
                    .map_err(|_| Error::Run("local-tool transcript is malformed".into()))?,
            )?;
        }
        Ok(transcript)
    }

    /// Restore independently dispatched observations and disjoint file
    /// removals in the order in which an agent returns them to its model.
    pub(crate) fn replay_with_commuted_observations(
        &mut self,
        root: &Path,
        tool: &str,
        request: &Value,
        allow_commuted_observations: bool,
    ) -> Result<Value, Error> {
        let current = self.events.get(self.cursor).ok_or_else(|| {
            Error::Run("local-tool replay has no remaining recorded event".into())
        })?;
        let matching_index = self.matching_index(tool, request, allow_commuted_observations);
        let Some(index) = matching_index else {
            let digest = |value: &Value| {
                let bytes = serde_json::to_vec(value).unwrap_or_default();
                format!("{:x}", Sha256::digest(bytes))
            };
            return Err(Error::Run(format!(
                "local-tool replay request differs from its causal recording \
                     (expected tool {}, arguments sha256 {}; actual tool {}, arguments sha256 {})",
                current.tool,
                digest(&current.request),
                tool,
                digest(request),
            )));
        };
        self.events[self.cursor..=index].rotate_right(1);
        let event = &self.events[self.cursor];
        for effect in &event.effects {
            apply(root, effect)?;
        }
        self.cursor += 1;
        if let Some(failure) = &event.failure {
            return Err(Error::Run(failure.clone()));
        }
        Ok(event.result.clone())
    }

    fn matching_index(
        &self,
        tool: &str,
        request: &Value,
        allow_commuted_observations: bool,
    ) -> Option<usize> {
        let current = self.events.get(self.cursor)?;
        if current.tool == tool && &current.request == request {
            Some(self.cursor)
        } else if allow_commuted_observations {
            self.events[self.cursor..]
                .iter()
                .take_while(|event| event.effects.is_empty())
                .position(|event| event.tool == tool && &event.request == request)
                .map(|offset| self.cursor + offset)
                .or_else(|| {
                    let pending = &self.events[self.cursor..];
                    let offset = pending
                        .iter()
                        .position(|event| event.tool == tool && &event.request == request)?;
                    let candidate = independent_text_write(&pending[offset])?;
                    pending[..offset]
                        .iter()
                        .all(|event| {
                            observation_path(event).is_some_and(|path| {
                                !path.starts_with(candidate) && !candidate.starts_with(path)
                            })
                        })
                        .then_some(self.cursor + offset)
                })
                .or_else(|| {
                    let pending = &self.events[self.cursor..];
                    let offset = pending
                        .iter()
                        .take_while(|event| independent_removal(event).is_some())
                        .position(|event| event.tool == tool && &event.request == request)?;
                    let candidate = independent_removal(&pending[offset])?;
                    pending[..offset]
                        .iter()
                        .all(|event| {
                            independent_removal(event).is_some_and(|path| {
                                !path.starts_with(candidate) && !candidate.starts_with(path)
                            })
                        })
                        .then_some(self.cursor + offset)
                })
        } else {
            None
        }
    }

    /// A later exact request can wait for preceding causal operations without
    /// either executing it early or treating it as a mismatch.
    pub(crate) fn should_defer(
        &self,
        tool: &str,
        request: &Value,
        allow_commuted_observations: bool,
    ) -> bool {
        self.matching_index(tool, request, allow_commuted_observations)
            .is_none()
            && self.events[self.cursor..]
                .iter()
                .any(|event| event.tool == tool && &event.request == request)
    }

    pub(crate) fn finish(&self) -> Result<(), Error> {
        if self.cursor == self.events.len() {
            Ok(())
        } else {
            Err(Error::Run(
                "local-tool replay left recorded events unconsumed".into(),
            ))
        }
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn set_cursor(&mut self, cursor: usize) -> Result<(), Error> {
        if cursor > self.events.len() {
            return Err(Error::Run(
                "local-tool replay progress exceeds its transcript".into(),
            ));
        }
        self.cursor = cursor;
        Ok(())
    }
}

#[derive(Default)]
struct EventSize {
    bytes: usize,
}

impl std::io::Write for EventSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("local-tool transcript event length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// A write may pass earlier observations only when the complete observed
// subtree is disjoint from the written path. Unknown tools, workspace-wide
// observations, unrecognized failures and multi-effect operations are barriers.
fn observation_path(event: &Event) -> Option<&Path> {
    if !event.effects.is_empty()
        || !matches!(
            event.tool.as_str(),
            "read_file" | "search_text" | "list_directory"
        )
    {
        return None;
    }
    if let Some(failure) = event.failure.as_deref()
        && !(event.tool == "search_text"
            && matches!(
                failure,
                "local-tool directory listing exceeds bound" | "cannot list local-tool directory"
            ))
    {
        return None;
    }
    normal_path(event.request.get("path")?.as_str()?)
}

fn independent_text_write(event: &Event) -> Option<&Path> {
    if event.tool != "write_file"
        || event.failure.is_some()
        || event.result != serde_json::json!({"ok": true})
    {
        return None;
    }
    let request = event.request.as_object()?;
    if request.len() != 2 {
        return None;
    }
    let path = request.get("path")?.as_str()?;
    let content = request.get("content")?.as_str()?;
    let [
        Effect::WriteText {
            path: effect_path,
            content: effect_content,
        },
    ] = event.effects.as_slice()
    else {
        return None;
    };
    if path != effect_path || content != effect_content {
        return None;
    }
    normal_path(path)
}

fn normal_path(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    (!path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(path)
}

/// Only successful single-file removals with identical request and effect
/// paths commute. Other writes, failures, and observations remain barriers.
fn independent_removal(event: &Event) -> Option<&Path> {
    if event.tool != "remove_file"
        || event.failure.is_some()
        || event.result != serde_json::json!({"ok": true})
    {
        return None;
    }
    let request = event.request.as_object()?;
    if request.len() != 1 {
        return None;
    }
    let value = request.get("path")?.as_str()?;
    let [Effect::RemoveFile { path }] = event.effects.as_slice() else {
        return None;
    };
    if value != path {
        return None;
    }
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(path)
}

/// Parse and re-encode a transcript before it enters a cassette.  This makes
/// the payload canonical and rejects malformed or out-of-order events while
/// it is still private run evidence.
pub(crate) fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    Transcript::decode(bytes)?.encode()
}

fn valid_tool(tool: &str) -> bool {
    !tool.is_empty()
        && tool.len() <= 128
        && tool
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn confined(root: &Path, value: &str) -> Result<PathBuf, Error> {
    let relative = Path::new(value);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::Run(
            "local-tool replay effect escapes the workspace".into(),
        ));
    }
    Ok(root.join(relative))
}

fn parent(root: &Path, relative: &Path) -> Result<(), Error> {
    let mut current = root.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = component else {
            return Err(Error::Run(
                "local-tool replay effect escapes the workspace".into(),
            ));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::Run(
                    "local-tool replay effect crosses a non-directory or symlink".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|_| {
                    Error::Run("cannot create confined local-tool replay directory".into())
                })?;
            }
            Err(_) => {
                return Err(Error::Run(
                    "cannot inspect confined local-tool replay directory".into(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn apply(root: &Path, effect: &Effect) -> Result<(), Error> {
    let root = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool replay workspace is unavailable".into()))?;
    if !root.is_dir() {
        return Err(Error::Run(
            "local-tool replay workspace is not a directory".into(),
        ));
    }
    let relative = match effect {
        Effect::CreateDirectory { path }
        | Effect::WriteText { path, .. }
        | Effect::WriteBinary { path, .. }
        | Effect::CreateSymlink { path, .. }
        | Effect::RemoveFile { path }
        | Effect::RemoveDirectory { path } => Path::new(path),
    };
    let path = confined(
        &root,
        relative
            .to_str()
            .ok_or_else(|| Error::Run("local-tool replay effect has a non-UTF-8 path".into()))?,
    )?;
    match effect {
        Effect::CreateDirectory { .. } => {
            parent(&root, relative)?;
            match std::fs::create_dir(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(error),
            }
        }
        Effect::WriteText { content, .. } => {
            parent(&root, relative)?;
            std::fs::write(path, content)
        }
        Effect::CreateSymlink { target, .. } => {
            parent(&root, relative)?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(target, path)
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(target, path)
            }
            #[cfg(not(any(unix, windows)))]
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "symlink replay is unsupported",
                ))
            }
        }
        Effect::WriteBinary { content_hex, .. } => {
            let bytes = content_hex.as_bytes();
            if !bytes.len().is_multiple_of(2) || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(Error::Run(
                    "local-tool replay binary effect is malformed".into(),
                ));
            }
            let decoded = bytes
                .chunks_exact(2)
                .map(|pair| {
                    let digit = |value: u8| match value {
                        b'0'..=b'9' => value - b'0',
                        b'a'..=b'f' => value - b'a' + 10,
                        _ => value - b'A' + 10,
                    };
                    digit(pair[0]) * 16 + digit(pair[1])
                })
                .collect::<Vec<_>>();
            parent(&root, relative)?;
            std::fs::write(path, decoded)
        }
        Effect::RemoveFile { .. } => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        Effect::RemoveDirectory { .. } => match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
    .map_err(|_| Error::Run("cannot apply confined local-tool replay effect".into()))
}

pub(crate) fn read_text(root: &Path, value: &str) -> Result<String, Error> {
    let root = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let relative = Path::new(value);
    let path = confined(&root, value)?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = root.clone();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(Error::Run("local-tool path escapes the workspace".into()));
        };
        current.push(name);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|_| Error::Run("local-tool path is unavailable".into()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Run("local-tool path crosses a symlink".into()));
        }
    }
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| Error::Run("cannot read local-tool file".into()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Run("cannot read local-tool file".into()));
    }
    if metadata.len() > MAX_TEXT_BYTES {
        return Err(Error::Run("local-tool file exceeds read bound".into()));
    }
    std::fs::read_to_string(path).map_err(|_| Error::Run("cannot read local-tool file".into()))
}

/// Enumerate a single confined directory without ever following a symlink.
/// The caller records this observation, so replay does not inspect the host
/// workspace at all.
pub(crate) fn list_directory(root: &Path, value: &str) -> Result<Vec<(String, bool)>, Error> {
    let root = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let path = if value.is_empty() || value == "." {
        root.clone()
    } else {
        confined(&root, value)?
    };
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| Error::Run("cannot list local-tool directory".into()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Run("cannot list local-tool directory".into()));
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path)
        .map_err(|_| Error::Run("cannot list local-tool directory".into()))?
    {
        let entry = entry.map_err(|_| Error::Run("cannot list local-tool directory".into()))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .filter(|name| !name.is_empty() && name.len() <= 255)
            .ok_or_else(|| Error::Run("local-tool directory has an invalid name".into()))?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|_| Error::Run("cannot inspect local-tool directory entry".into()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        entries.push((name.to_owned(), metadata.is_dir()));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

/// Search bounded UTF-8 source text through the confined workspace.  Results
/// are an observation captured in the transcript; a replay does not rescan a
/// changed checkout.  Build/vendor trees are skipped to retain a predictable
/// cost and keep agent reasoning focused on task material.
pub(crate) fn search_text(
    root: &Path,
    value: &str,
    query: &str,
) -> Result<Vec<(String, u64, String)>, Error> {
    if query.is_empty() || query.len() > 1024 {
        return Err(Error::Run("local-tool search query is invalid".into()));
    }
    let root = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let start = if value.is_empty() || value == "." {
        String::new()
    } else {
        // Validate the requested subtree before traversal.
        let _ = confined(&root, value)?;
        value.into()
    };
    let mut files = 0usize;
    let mut matches = Vec::new();
    if !start.is_empty()
        && std::fs::symlink_metadata(root.join(&start))
            .map_err(|_| Error::Run("cannot inspect local-tool search path".into()))?
            .is_file()
    {
        let text = read_text(&root, &start)?;
        append_search_matches(&start, &text, query, &mut matches)?;
    } else {
        search_directory(&root, &start, query, 0, &mut files, &mut matches)?;
    }
    Ok(matches)
}

fn search_directory(
    root: &Path,
    relative: &str,
    query: &str,
    depth: usize,
    files: &mut usize,
    matches: &mut Vec<(String, u64, String)>,
) -> Result<(), Error> {
    if depth > MAX_SEARCH_DEPTH {
        return Err(Error::Run(
            "local-tool search exceeds directory-depth bound".into(),
        ));
    }
    if matches.len() >= MAX_SEARCH_MATCHES {
        return Ok(());
    }
    for (name, directory) in list_directory(root, relative)? {
        if matches.len() >= MAX_SEARCH_MATCHES {
            break;
        }
        if matches!(name.as_str(), ".git" | "target" | "node_modules" | ".venv") {
            continue;
        }
        let child = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        if directory {
            search_directory(root, &child, query, depth + 1, files, matches)?;
            continue;
        }
        *files += 1;
        if *files > MAX_SEARCH_FILES {
            return Err(Error::Run("local-tool search exceeds file bound".into()));
        }
        let Ok(text) = read_text(root, &child) else {
            // Binary and oversized files are outside this deliberately text
            // only observation surface; they cannot change replay behavior.
            continue;
        };
        append_search_matches(&child, &text, query, matches)?;
    }
    Ok(())
}

fn append_search_matches(
    path: &str,
    text: &str,
    query: &str,
    matches: &mut Vec<(String, u64, String)>,
) -> Result<(), Error> {
    for (index, line) in text.lines().enumerate() {
        if line.contains(query) {
            if line.len() > 4096 {
                return Err(Error::Run("local-tool search line exceeds bound".into()));
            }
            matches.push((path.to_owned(), index as u64 + 1, line.into()));
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_reader_accepts_chunked_lines_and_rejects_oversized_or_invalid_input() {
        struct Chunked<'a>(&'a [u8]);
        impl std::io::Read for Chunked<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let n = buffer.len().min(self.0.len()).min(7);
                buffer[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let mut transcript = Transcript::default();
        transcript.record(removal(1, "one")).unwrap();
        transcript.record(removal(2, "two")).unwrap();
        let canonical = transcript.encode().unwrap();
        let mixed = String::from_utf8(canonical.clone())
            .unwrap()
            .replacen('\n', "\r\n", 1);
        let mixed = mixed.trim_end_matches('\n');
        let parsed =
            Transcript::decode_from(&mut std::io::BufReader::new(Chunked(mixed.as_bytes())))
                .unwrap();
        assert_eq!(parsed.encode().unwrap(), canonical);
        assert!(
            Transcript::decode_from(&mut std::io::BufReader::new(Chunked(&[0xff, b'\n']))).is_err()
        );
        let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        let error = Transcript::decode_from(&mut std::io::BufReader::new(Chunked(&oversized)))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("event exceeds bound"), "{error}");
    }

    #[test]
    fn transcript_writer_streams_events_and_reports_write_errors() {
        struct RecordingWriter {
            bytes: Vec<u8>,
            max_write: usize,
        }
        impl std::io::Write for RecordingWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.max_write = self.max_write.max(data.len());
                self.bytes.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct FailedWriter;
        impl std::io::Write for FailedWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture write failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut transcript = Transcript::default();
        let mut expected = Vec::new();
        for sequence in 1..=3 {
            let event = Event {
                schema_version: SCHEMA_VERSION,
                sequence,
                tool: "read_file".into(),
                request: serde_json::json!({"path":"source.txt"}),
                result: serde_json::json!({"text":"x\n\"é".repeat(80_000)}),
                failure: None,
                effects: Vec::new(),
            };
            expected.extend(serde_json::to_vec(&event).unwrap());
            expected.push(b'\n');
            transcript.record(event).unwrap();
        }
        let mut writer = RecordingWriter {
            bytes: Vec::new(),
            max_write: 0,
        };
        transcript.write_to(&mut writer).unwrap();
        assert_eq!(writer.bytes, expected);
        assert!(writer.max_write < writer.bytes.len());
        assert!(transcript.write_to(&mut FailedWriter).is_err());

        let mut oversized = Transcript::default();
        oversized
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 1,
                tool: "read_file".into(),
                request: serde_json::json!({"path":"large.txt"}),
                result: serde_json::json!({"text":"x".repeat(MAX_LINE_BYTES)}),
                failure: None,
                effects: Vec::new(),
            })
            .unwrap();
        let mut output = Vec::new();
        assert!(oversized.write_to(&mut output).is_err());
        assert!(output.is_empty());
    }

    fn removal(sequence: u64, path: &str) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            tool: "remove_file".into(),
            request: serde_json::json!({"path":path}),
            result: serde_json::json!({"ok":true}),
            failure: None,
            effects: vec![Effect::RemoveFile { path: path.into() }],
        }
    }

    #[test]
    fn disjoint_removals_commute_but_duplicates_and_other_effects_do_not() {
        let root = std::env::temp_dir().join(format!("asb-removal-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for path in ["a", "b"] {
            std::fs::write(root.join(path), "fixture").unwrap();
        }
        let mut transcript = Transcript::default();
        transcript.record(removal(1, "a")).unwrap();
        transcript.record(removal(2, "b")).unwrap();
        transcript
            .replay_with_commuted_observations(
                &root,
                "remove_file",
                &serde_json::json!({"path":"b"}),
                true,
            )
            .unwrap();
        assert!(root.join("a").exists());
        assert!(!root.join("b").exists());
        transcript
            .replay_with_commuted_observations(
                &root,
                "remove_file",
                &serde_json::json!({"path":"a"}),
                true,
            )
            .unwrap();
        transcript.finish().unwrap();
        let mut transcript = Transcript::default();
        transcript.record(removal(1, "a")).unwrap();
        let mut duplicate = removal(2, "a");
        duplicate.request["extra"] = serde_json::json!(true);
        transcript.record(duplicate).unwrap();
        assert!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "remove_file",
                    &serde_json::json!({"path":"a","extra":true}),
                    true
                )
                .is_err()
        );
        let mut transcript = Transcript::default();
        transcript.record(removal(1, "a")).unwrap();
        transcript.record(removal(2, "a/child")).unwrap();
        assert!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "remove_file",
                    &serde_json::json!({"path":"a/child"}),
                    true
                )
                .is_err()
        );
        let mut transcript = Transcript::default();
        transcript.record(event(1)).unwrap();
        transcript.record(removal(2, "b")).unwrap();
        assert!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "remove_file",
                    &serde_json::json!({"path":"b"}),
                    true
                )
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn binary_effect_round_trips_and_rejects_invalid_hex() {
        let root = std::env::temp_dir().join(format!("asb-binary-effect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let effect = Effect::WriteBinary {
            path: "result.bin".into(),
            content_hex: "00ff8041".into(),
        };
        let mut recorded = event(1);
        recorded.effects = vec![effect];
        let mut transcript = Transcript::default();
        transcript.record(recorded).unwrap();
        let encoded = transcript.encode().unwrap();
        let mut decoded = Transcript::decode(&encoded).unwrap();
        decoded
            .replay_with_commuted_observations(
                &root,
                "write_file",
                &serde_json::json!({"path":"answer.txt"}),
                false,
            )
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("result.bin")).unwrap(),
            [0, 255, 128, 65]
        );
        let mut maximum = event(1);
        maximum.effects = vec![Effect::WriteBinary {
            path: "large.bin".into(),
            content_hex: "ff".repeat(512 * 1024),
        }];
        let mut transcript = Transcript::default();
        transcript.record(maximum).unwrap();
        assert!(Transcript::decode(&transcript.encode().unwrap()).is_ok());
        for invalid in ["f", "fg", "00 ff"] {
            let effect = Effect::WriteBinary {
                path: "bad.bin".into(),
                content_hex: invalid.into(),
            };
            assert!(apply(&root, &effect).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    fn event(sequence: u64) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            tool: "write_file".into(),
            request: serde_json::json!({"path":"answer.txt"}),
            result: serde_json::json!({"ok":true}),
            failure: None,
            effects: vec![Effect::WriteText {
                path: "answer.txt".into(),
                content: "recorded".into(),
            }],
        }
    }
    #[test]
    fn replay_is_causal_and_applies_only_recorded_effects() {
        let mut recorded = Transcript::default();
        recorded.record(event(1)).unwrap();
        let mut replay = Transcript::decode(&recorded.encode().unwrap()).unwrap();
        let root = std::env::temp_dir().join(format!(
            "asb-local-tool-{}-causal-effects",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            replay
                .replay_with_commuted_observations(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"}),
                    false,
                )
                .unwrap(),
            serde_json::json!({"ok":true})
        );
        assert_eq!(
            std::fs::read_to_string(root.join("answer.txt")).unwrap(),
            "recorded"
        );
        replay.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disjoint_write_can_pass_only_known_disjoint_observations() {
        let root = std::env::temp_dir().join(format!(
            "asb-disjoint-write-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let observation = |path: Option<&str>, failure: Option<&str>| Event {
            schema_version: SCHEMA_VERSION,
            sequence: 1,
            tool: "search_text".into(),
            request: match path {
                Some(path) => serde_json::json!({"path":path,"query":"needle"}),
                None => serde_json::json!({"query":"needle"}),
            },
            result: if failure.is_some() {
                Value::Null
            } else {
                serde_json::json!({"matches":[]})
            },
            failure: failure.map(str::to_owned),
            effects: Vec::new(),
        };
        let write = Event {
            schema_version: SCHEMA_VERSION,
            sequence: 2,
            tool: "write_file".into(),
            request: serde_json::json!({"path":"tmp/probe.txt","content":"probe"}),
            result: serde_json::json!({"ok":true}),
            failure: None,
            effects: vec![Effect::WriteText {
                path: "tmp/probe.txt".into(),
                content: "probe".into(),
            }],
        };
        for (path, failure, allowed) in [
            (
                Some("lib"),
                Some("local-tool directory listing exceeds bound"),
                true,
            ),
            (Some("lib"), None, true),
            (Some("tmp"), None, false),
            (Some("tmp/probe.txt"), None, false),
            (None, None, false),
            (Some("lib"), Some("other failure"), false),
        ] {
            let mut transcript = Transcript::default();
            transcript.record(observation(path, failure)).unwrap();
            transcript.record(write.clone()).unwrap();
            let outcome = transcript.replay_with_commuted_observations(
                &root,
                "write_file",
                &write.request,
                true,
            );
            assert_eq!(
                outcome.is_ok(),
                allowed,
                "path={path:?}, failure={failure:?}"
            );
            let _ = std::fs::remove_dir_all(root.join("tmp"));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_can_commute_effect_free_observations_but_not_effects() {
        let mut transcript = Transcript::default();
        transcript
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 1,
                tool: "search_text".into(),
                request: serde_json::json!({"path":"src","query":"needle"}),
                result: serde_json::json!({"matches":[]}),
                failure: None,
                effects: Vec::new(),
            })
            .unwrap();
        transcript
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 2,
                tool: "read_file".into(),
                request: serde_json::json!({"path":"src/main.rs"}),
                result: serde_json::json!({"text":"recorded","truncated":false}),
                failure: None,
                effects: Vec::new(),
            })
            .unwrap();
        transcript.record(event(3)).unwrap();
        let root = std::env::temp_dir().join(format!(
            "asb-local-tool-{}-commuted-observations",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "read_file",
                    &serde_json::json!({"path":"src/main.rs"}),
                    true,
                )
                .unwrap(),
            serde_json::json!({"text":"recorded","truncated":false})
        );
        assert!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"}),
                    true,
                )
                .is_err()
        );
        assert_eq!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "search_text",
                    &serde_json::json!({"path":"src","query":"needle"}),
                    true,
                )
                .unwrap(),
            serde_json::json!({"matches":[]})
        );
        assert_eq!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"}),
                    true,
                )
                .unwrap(),
            serde_json::json!({"ok":true})
        );
        transcript.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn replay_rejects_reordered_or_escaping_effects() {
        let mut transcript = Transcript::default();
        transcript.record(event(1)).unwrap();
        let root = std::env::temp_dir();
        assert!(
            transcript
                .replay_with_commuted_observations(&root, "other", &serde_json::json!({}), false)
                .is_err()
        );
        let mut bad = event(1);
        bad.effects = vec![Effect::WriteText {
            path: "../escape".into(),
            content: "no".into(),
        }];
        let mut replay = Transcript {
            events: vec![bad],
            cursor: 0,
        };
        assert!(
            replay
                .replay_with_commuted_observations(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"}),
                    false,
                )
                .is_err()
        );
    }

    #[test]
    fn transcript_rejects_an_unbounded_or_effectful_failure_event() {
        let mut transcript = Transcript::default();
        let mut invalid = event(1);
        invalid.failure = Some("x".repeat(513));
        invalid.result = Value::Null;
        assert!(transcript.record(invalid).is_err());

        let mut invalid = event(1);
        invalid.failure = Some("bounded failure".into());
        invalid.result = Value::Null;
        assert!(transcript.record(invalid).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replay_refuses_a_workspace_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("asb-local-tool-link-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("asb-local-tool-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        let mut transcript = Transcript {
            events: vec![Event {
                effects: vec![Effect::WriteText {
                    path: "linked/escape.txt".into(),
                    content: "no".into(),
                }],
                ..event(1)
            }],
            cursor: 0,
        };
        assert!(
            transcript
                .replay_with_commuted_observations(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"}),
                    false,
                )
                .is_err()
        );
        assert!(!outside.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn directory_listing_is_confined_sorted_and_skips_symlinks() {
        let root = std::env::temp_dir().join(format!("asb-local-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("directory")).unwrap();
        std::fs::write(root.join("z.txt"), "z").unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp", root.join("outside")).unwrap();
        let entries = list_directory(&root, ".").unwrap();
        assert_eq!(entries[0], ("a.txt".into(), false));
        assert!(
            entries
                .iter()
                .any(|entry| entry == &("directory".into(), true))
        );
        #[cfg(unix)]
        assert!(!entries.iter().any(|entry| entry.0 == "outside"));
        assert!(list_directory(&root, "../outside").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn directory_listing_and_search_handle_more_than_256_entries() {
        let root = std::env::temp_dir().join(format!(
            "asb-large-directory-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..300 {
            std::fs::write(
                root.join(format!("source-{index:03}.txt")),
                if index == 299 { "needle" } else { "other" },
            )
            .unwrap();
        }
        assert_eq!(list_directory(&root, ".").unwrap().len(), 300);
        assert_eq!(
            search_text(&root, ".", "needle").unwrap(),
            vec![("source-299.txt".into(), 1, "needle".into())]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn text_search_accepts_an_explicit_file_path() {
        let root = std::env::temp_dir().join(format!(
            "asb-file-search-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "first\nfind needle\nlast needle\n").unwrap();
        assert_eq!(
            search_text(&root, "src/lib.rs", "needle").unwrap(),
            vec![
                ("src/lib.rs".into(), 2, "find needle".into()),
                ("src/lib.rs".into(), 3, "last needle".into()),
            ]
        );
        assert!(search_text(&root, "src/missing.rs", "needle").is_err());
        assert!(search_text(&root, "../outside", "needle").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn text_search_is_bounded_confined_and_skips_vendor_trees() {
        let root = std::env::temp_dir().join(format!("asb-local-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn found() {}\n").unwrap();
        std::fs::write(root.join("target/ignored.rs"), "fn found() {}\n").unwrap();
        let matches = search_text(&root, ".", "found").unwrap();
        assert_eq!(
            matches,
            vec![("src/lib.rs".into(), 1, "fn found() {}".into())]
        );
        assert!(search_text(&root, "../outside", "found").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
