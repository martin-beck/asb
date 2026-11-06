//! Adapter-neutral validation of snapshot boundary journals.
use crate::Error;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    verify_markers_with_identities(snapshot, expected, None)
}

/// Optionally reject overlapping indistinguishable adapter requests.
pub(super) fn verify_markers_with_identities(
    snapshot: &mut File,
    expected: &[String],
    identities: Option<&BTreeMap<String, Vec<u8>>>,
) -> Result<(), Error> {
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
    let mut active_identities: BTreeSet<&[u8]> = BTreeSet::new();
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
        if kind == "begin"
            && let Some(identities) = identities
        {
            let key = identities.get(alias).ok_or_else(invalid)?;
            if !active_identities.insert(key.as_slice()) {
                return Err(Error::Run("Pi native snapshot contains overlapping indistinguishable tool calls; create a fresh recording with distinct tool call IDs".into()));
            }
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
                if let Some(identities) = identities {
                    let key = identities.get(alias).ok_or_else(invalid)?;
                    if !active_identities.remove(key.as_slice()) {
                        return Err(invalid());
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(path: &std::path::Path, overlap: bool) {
        let mut tar = tar::Builder::new(File::create(path).unwrap());
        let boundaries = concat!(
            "{\"sequence\":1,\"kind\":\"baseline\",\"alias\":null,\"snapshot\":\"snapshot-00000001\"}\n",
            "{\"sequence\":2,\"kind\":\"tool-end\",\"alias\":\"pi-0\",\"snapshot\":\"snapshot-00000002\"}\n",
            "{\"sequence\":3,\"kind\":\"tool-end\",\"alias\":\"pi-1\",\"snapshot\":\"snapshot-00000003\"}\n",
            "{\"sequence\":4,\"kind\":\"final\",\"alias\":null,\"snapshot\":\"snapshot-00000004\"}\n"
        );
        let markers = if overlap {
            concat!(
                "{\"sequence\":1,\"kind\":\"begin\",\"alias\":\"pi-0\",\"snapshot_sequence\":1}\n",
                "{\"sequence\":2,\"kind\":\"begin\",\"alias\":\"pi-1\",\"snapshot_sequence\":1}\n",
                "{\"sequence\":3,\"kind\":\"end\",\"alias\":\"pi-0\",\"snapshot_sequence\":2}\n",
                "{\"sequence\":4,\"kind\":\"end\",\"alias\":\"pi-1\",\"snapshot_sequence\":3}\n"
            )
        } else {
            concat!(
                "{\"sequence\":1,\"kind\":\"begin\",\"alias\":\"pi-0\",\"snapshot_sequence\":1}\n",
                "{\"sequence\":2,\"kind\":\"end\",\"alias\":\"pi-0\",\"snapshot_sequence\":2}\n",
                "{\"sequence\":3,\"kind\":\"begin\",\"alias\":\"pi-1\",\"snapshot_sequence\":2}\n",
                "{\"sequence\":4,\"kind\":\"end\",\"alias\":\"pi-1\",\"snapshot_sequence\":3}\n"
            )
        };
        let mut add = |name: &str, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes).unwrap();
        };
        add("boundaries.jsonl", boundaries.as_bytes());
        add("markers.jsonl", markers.as_bytes());
        for index in 1..=4 {
            add(&format!("snapshot-{index:08}.btrfs"), b"fixture");
        }
        tar.finish().unwrap();
        tar.into_inner().unwrap().flush().unwrap();
    }

    #[test]
    fn rejects_only_overlapping_indistinguishable_calls() {
        let root = crate::fixture_namespace::allocate(
            &std::env::temp_dir(),
            "snapshot-overlap-identities",
        )
        .unwrap();
        let overlap = root.join("overlap.tar");
        let sequential = root.join("sequential.tar");
        archive(&overlap, true);
        archive(&sequential, false);
        let expected = vec!["pi-0".to_owned(), "pi-1".to_owned()];
        let same = BTreeMap::from([
            ("pi-0".to_owned(), b"same".to_vec()),
            ("pi-1".to_owned(), b"same".to_vec()),
        ]);
        let distinct = BTreeMap::from([
            ("pi-0".to_owned(), b"first".to_vec()),
            ("pi-1".to_owned(), b"second".to_vec()),
        ]);
        let error = verify_markers_with_identities(
            &mut File::open(&overlap).unwrap(),
            &expected,
            Some(&same),
        )
        .unwrap_err();
        assert!(error.to_string().contains("overlapping indistinguishable"));
        assert!(
            verify_markers_with_identities(
                &mut File::open(&sequential).unwrap(),
                &expected,
                Some(&same)
            )
            .is_ok()
        );
        assert!(
            verify_markers_with_identities(
                &mut File::open(&overlap).unwrap(),
                &expected,
                Some(&distinct)
            )
            .is_ok()
        );
        assert!(verify_markers(&mut File::open(&overlap).unwrap(), &expected).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
