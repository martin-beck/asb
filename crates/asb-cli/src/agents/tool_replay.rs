//! Sealed, causal local-tool transcripts used by tape-backed agent adapters.
//!
//! This module deliberately knows nothing about a particular client or MCP
//! transport.  It records the semantic tool turn and the confined workspace
//! effects needed to make replay observationally equivalent without executing
//! a shell command, opening a network connection, or consulting host state.

use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 16_384;
const MAX_LINE_BYTES: usize = 1024 * 1024;
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Effect {
    CreateDirectory { path: String },
    WriteText { path: String, content: String },
    RemoveFile { path: String },
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
        {
            return Err(Error::Run("invalid local-tool transcript event".into()));
        }
        self.events.push(event);
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        for event in &self.events {
            let line = serde_json::to_vec(event)
                .map_err(|_| Error::Io("cannot encode local-tool transcript".into()))?;
            if line.len() > MAX_LINE_BYTES {
                return Err(Error::Run(
                    "local-tool transcript event exceeds bound".into(),
                ));
            }
            bytes.extend(line);
            bytes.push(b'\n');
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_EVENTS * (MAX_LINE_BYTES + 1) {
            return Err(Error::Run("local-tool transcript exceeds bound".into()));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Run("local-tool transcript is not UTF-8 JSONL".into()))?;
        let mut transcript = Self::default();
        for line in text.lines() {
            if line.len() > MAX_LINE_BYTES {
                return Err(Error::Run(
                    "local-tool transcript event exceeds bound".into(),
                ));
            }
            transcript.record(
                serde_json::from_str(line)
                    .map_err(|_| Error::Run("local-tool transcript is malformed".into()))?,
            )?;
        }
        Ok(transcript)
    }

    /// Consume exactly the next causally-recorded request, then apply only its
    /// declared workspace effects.  A replay never invokes the original tool.
    pub(crate) fn replay(
        &mut self,
        root: &Path,
        tool: &str,
        request: &Value,
    ) -> Result<Value, Error> {
        let event = self.events.get(self.cursor).ok_or_else(|| {
            Error::Run("local-tool replay has no remaining recorded event".into())
        })?;
        if event.tool != tool || &event.request != request {
            return Err(Error::Run(
                "local-tool replay request differs from its causal recording".into(),
            ));
        }
        for effect in &event.effects {
            apply(root, effect)?;
        }
        self.cursor += 1;
        Ok(event.result.clone())
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
        | Effect::RemoveFile { path } => Path::new(path),
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
        Effect::RemoveFile { .. } => match std::fs::remove_file(path) {
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
        if entries.len() >= 256 {
            return Err(Error::Run(
                "local-tool directory listing exceeds bound".into(),
            ));
        }
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
    search_directory(&root, &start, query, 0, &mut files, &mut matches)?;
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
        for (index, line) in text.lines().enumerate() {
            if line.contains(query) {
                if line.len() > 4096 {
                    return Err(Error::Run("local-tool search line exceeds bound".into()));
                }
                matches.push((child.clone(), index as u64 + 1, line.into()));
                if matches.len() >= MAX_SEARCH_MATCHES {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(sequence: u64) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            tool: "write_file".into(),
            request: serde_json::json!({"path":"answer.txt"}),
            result: serde_json::json!({"ok":true}),
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
        let root = std::env::temp_dir().join(format!("asb-local-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            replay
                .replay(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"})
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
    fn replay_rejects_reordered_or_escaping_effects() {
        let mut transcript = Transcript::default();
        transcript.record(event(1)).unwrap();
        let root = std::env::temp_dir();
        assert!(
            transcript
                .replay(&root, "other", &serde_json::json!({}))
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
                .replay(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"})
                )
                .is_err()
        );
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
                .replay(
                    &root,
                    "write_file",
                    &serde_json::json!({"path":"answer.txt"})
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
