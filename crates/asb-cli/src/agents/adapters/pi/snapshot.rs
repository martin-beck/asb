//! Pi-owned consistency checks between native exchanges and snapshot markers.
use crate::{
    Error,
    cassette::{NativeToolTransport, NativeTrack},
};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
};

const JOURNAL_LIMIT: u64 = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 8194;

fn invalid() -> Error {
    Error::Run("Pi native recording does not match its settled snapshot markers".into())
}

/// Checks bounded private metadata; no captured tool arguments are reported.
pub(super) fn verify(native: &[u8], snapshot: &mut File, digest: &str) -> Result<(), Error> {
    let track = NativeTrack {
        complete: true,
        events: native.iter().filter(|b| **b == b'\n').count() as u64,
        bytes: native.len() as u64,
        sha256: crate::cassette::digest(native),
    };
    let transport = NativeToolTransport::from_sidecar(&track, native).map_err(|_| invalid())?;
    if transport.launch_session() != "pi-launch-1"
        || transport.workspace_effect_sha256() != digest
        || transport.sessions().len() != 1
        || transport.sessions()[0].alias() != "pi-tools-1"
        || !transport.sessions()[0].success()
    {
        return Err(invalid());
    }
    let requests = transport
        .recorded_requests("pi-tools-1")
        .ok_or_else(invalid)?;
    let mut expected = Vec::with_capacity(requests.len());
    let mut ordinals = BTreeSet::new();
    for request in requests {
        let value: serde_json::Value = serde_json::from_slice(request).map_err(|_| invalid())?;
        if value.as_object().is_none_or(|obj| {
            !obj.contains_key("request")
                || !(obj.len() == 2 || (obj.len() == 3 && obj.contains_key("snapshot_sequence")))
        }) {
            return Err(invalid());
        }
        let ordinal = value
            .get("ordinal")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(invalid)?;
        if !ordinals.insert(ordinal) {
            return Err(invalid());
        }
        let sequence = value
            .get("snapshot_sequence")
            .map(|item| item.as_u64().ok_or_else(invalid))
            .transpose()?;
        expected.push((sequence, format!("pi-{ordinal}")));
    }
    if ordinals.iter().copied().ne(0..ordinals.len() as u64) {
        return Err(invalid());
    }
    if expected.iter().any(|(sequence, _)| sequence.is_some()) {
        if expected.iter().any(|(sequence, _)| sequence.is_none()) {
            return Err(invalid());
        }
        expected.sort_by_key(|(sequence, _)| *sequence);
        if expected
            .iter()
            .enumerate()
            .any(|(index, (sequence, _))| *sequence != Some(index as u64 + 2))
        {
            return Err(invalid());
        }
    }
    let expected: Vec<String> = expected.into_iter().map(|(_, alias)| alias).collect();

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
    if !active.is_empty() || ends != expected {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn native(sequenced: bool) -> Vec<u8> {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/continue_native_sidecar_emit.mjs");
        let mut command = std::process::Command::new("node");
        command.arg(fixture).args(["--pi", "--pending"]);
        if sequenced {
            command.arg("--sequence");
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        crate::agents::native_guest::finalize("pi", &output.stdout, &"a".repeat(64)).unwrap()
    }

    fn archive(path: &std::path::Path, wrong: bool, sequenced: bool) {
        let file = File::create(path).unwrap();
        let mut tar = tar::Builder::new(file);
        let mut add = |name: &str, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes).unwrap();
        };
        let first = if sequenced || wrong { "pi-0" } else { "pi-1" };
        let second = if sequenced { "pi-1" } else { "pi-0" };
        let boundaries = format!(
            "{{\"sequence\":1,\"kind\":\"baseline\",\"alias\":null,\"snapshot\":\"snapshot-00000001\"}}\n{{\"sequence\":2,\"kind\":\"tool-end\",\"alias\":\"{first}\",\"snapshot\":\"snapshot-00000002\"}}\n{{\"sequence\":3,\"kind\":\"tool-end\",\"alias\":\"{second}\",\"snapshot\":\"snapshot-00000003\"}}\n{{\"sequence\":4,\"kind\":\"final\",\"alias\":null,\"snapshot\":\"snapshot-00000004\"}}\n"
        );
        let markers = format!(
            "{{\"sequence\":1,\"kind\":\"begin\",\"alias\":\"pi-0\",\"snapshot_sequence\":1}}\n{{\"sequence\":2,\"kind\":\"begin\",\"alias\":\"pi-1\",\"snapshot_sequence\":1}}\n{{\"sequence\":3,\"kind\":\"end\",\"alias\":\"{first}\",\"snapshot_sequence\":2}}\n{{\"sequence\":4,\"kind\":\"end\",\"alias\":\"{second}\",\"snapshot_sequence\":3}}\n"
        );
        add("boundaries.jsonl", boundaries.as_bytes());
        add("markers.jsonl", markers.as_bytes());
        for i in 1..=4 {
            add(&format!("snapshot-{i:08}.btrfs"), b"fixture");
        }
        tar.finish().unwrap();
        tar.into_inner().unwrap().flush().unwrap();
    }

    #[test]
    fn checks_pi_completion_order_and_rejects_mismatched_markers_or_digest() {
        let root =
            std::env::temp_dir().join(format!("asb-pi-snapshot-binding-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let good = root.join("good.tar");
        let bad = root.join("bad.tar");
        archive(&good, false, false);
        archive(&bad, true, false);
        let legacy_native = native(false);
        assert!(
            crate::agents::find("pi")
                .unwrap()
                .verify_native_snapshot(
                    &legacy_native,
                    &mut File::open(&good).unwrap(),
                    &"a".repeat(64)
                )
                .is_ok()
        );
        assert!(
            verify(
                &legacy_native,
                &mut File::open(&bad).unwrap(),
                &"a".repeat(64)
            )
            .is_err()
        );
        assert!(
            verify(
                &legacy_native,
                &mut File::open(&good).unwrap(),
                &"b".repeat(64)
            )
            .is_err()
        );
        let sequenced = root.join("sequenced.tar");
        archive(&sequenced, false, true);
        let ordered_native = native(true);
        assert!(
            verify(
                &ordered_native,
                &mut File::open(&sequenced).unwrap(),
                &"a".repeat(64)
            )
            .is_ok()
        );
        assert!(
            verify(
                &ordered_native,
                &mut File::open(&good).unwrap(),
                &"a".repeat(64)
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an explicitly supplied private real Pi cassette extraction"]
    fn checks_preserved_real_pi_recording() {
        use sha2::{Digest, Sha256};
        let native = std::fs::read(std::env::var_os("ASB_TEST_PI_NATIVE").unwrap()).unwrap();
        let mut snapshot = File::open(std::env::var_os("ASB_TEST_PI_SNAPSHOT").unwrap()).unwrap();
        let mut hash = Sha256::new();
        std::io::copy(&mut snapshot, &mut hash).unwrap();
        let digest = format!("{:x}", hash.finalize());
        assert!(verify(&native, &mut snapshot, &digest).is_ok());
    }
}
