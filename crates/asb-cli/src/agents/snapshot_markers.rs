//! Adapter-neutral validation of snapshot boundary journals.
use crate::Error;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
};
const JOURNAL_LIMIT: u64 = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 8194;
fn invalid() -> Error {
    Error::Run("native recording does not match its settled snapshot markers".into())
}

/// Validate bounded Btrfs journals against an adapter-authored completion order.
pub(super) fn verify_markers(snapshot: &mut File, expected: &[String]) -> Result<(), Error> {
    snapshot.seek(SeekFrom::Start(0)).map_err(|_| invalid())?;
    let mut archive = tar::Archive::new(snapshot);
    let mut names = BTreeSet::new();
    let mut boundaries = None;
    let mut markers = None;
    for entry in archive.entries().map_err(|_| invalid())? {
        let entry = entry.map_err(|_| invalid())?;
        let name = entry
            .path()
            .map_err(|_| invalid())?
            .to_str()
            .ok_or_else(invalid)?
            .to_owned();
        if names.len() >= ENTRY_LIMIT
            || !names.insert(name.clone())
            || !entry.header().entry_type().is_file()
        {
            return Err(invalid());
        }
        if name == "boundaries.jsonl" || name == "markers.jsonl" {
            let mut bytes = Vec::new();
            entry
                .take(JOURNAL_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| invalid())?;
            if bytes.len() as u64 > JOURNAL_LIMIT || !bytes.ends_with(b"\n") {
                return Err(invalid());
            }
            let rows = bytes
                .strip_suffix(b"\n")
                .ok_or_else(invalid)?
                .split(|b| *b == b'\n')
                .map(|line| {
                    serde_json::from_slice::<serde_json::Value>(line).map_err(|_| invalid())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if name == "boundaries.jsonl" {
                boundaries = Some(rows);
            } else {
                markers = Some(rows);
            }
        } else if !(name.is_ascii()
            && name.starts_with("snapshot-")
            && name.ends_with(".btrfs")
            && name.len() == 23
            && name[9..17].bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid());
        }
    }
    let boundaries = boundaries.ok_or_else(invalid)?;
    let markers = markers.ok_or_else(invalid)?;
    if boundaries.len() != expected.len() + 2 || names.len() != boundaries.len() + 2 {
        return Err(invalid());
    }
    for (index, entry) in boundaries.iter().enumerate() {
        let seq = index + 1;
        let kind = if index == 0 {
            "baseline"
        } else if index + 1 == boundaries.len() {
            "final"
        } else {
            "tool-end"
        };
        let alias = if kind == "tool-end" {
            Some(expected[index - 1].as_str())
        } else {
            None
        };
        if entry.get("sequence").and_then(serde_json::Value::as_u64) != Some(seq as u64)
            || entry.get("kind").and_then(serde_json::Value::as_str) != Some(kind)
            || entry.get("alias").and_then(serde_json::Value::as_str) != alias
            || (alias.is_none() && entry.get("alias") != Some(&serde_json::Value::Null))
            || entry.get("snapshot").and_then(serde_json::Value::as_str)
                != Some(format!("snapshot-{seq:08}").as_str())
            || !names.contains(&format!("snapshot-{seq:08}.btrfs"))
        {
            return Err(invalid());
        }
    }
    let mut active = BTreeSet::new();
    let mut ends = Vec::new();
    for (index, marker) in markers.iter().enumerate() {
        let alias = marker
            .get("alias")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        let kind = marker
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        if marker.get("sequence").and_then(serde_json::Value::as_u64) != Some((index + 1) as u64) {
            return Err(invalid());
        }
        match kind {
            "begin"
                if active.insert(alias.to_owned())
                    && marker
                        .get("snapshot_sequence")
                        .and_then(serde_json::Value::as_u64)
                        == Some((ends.len() + 1) as u64) => {}
            "end"
                if active.remove(alias)
                    && marker
                        .get("snapshot_sequence")
                        .and_then(serde_json::Value::as_u64)
                        == Some((ends.len() + 2) as u64) =>
            {
                ends.push(alias.to_owned())
            }
            _ => return Err(invalid()),
        }
    }
    if !active.is_empty() || ends.as_slice() != expected {
        return Err(invalid());
    }
    Ok(())
}
