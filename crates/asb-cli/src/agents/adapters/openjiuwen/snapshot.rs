//! OpenJiuwen-owned binding between native tool events and workspace snapshots.
use crate::{
    Error,
    cassette::{NativeToolTransport, NativeTrack},
};
use std::{collections::BTreeSet, fs::File};

fn invalid() -> Error {
    Error::Run("OpenJiuwen native recording does not match its settled snapshot boundaries".into())
}

fn transport(bytes: &[u8]) -> Result<NativeToolTransport, Error> {
    let track = NativeTrack {
        complete: true,
        events: bytes.iter().filter(|byte| **byte == b'\n').count() as u64,
        bytes: bytes.len() as u64,
        sha256: crate::cassette::digest(bytes),
    };
    NativeToolTransport::from_sidecar(&track, bytes).map_err(|_| invalid())
}

/// The agent writes a completed private sidecar before the supervisor settles.
/// Replace only its known placeholder with the measured archive identity.
pub(super) fn finalize(pending: &[u8], digest: &str) -> Result<Vec<u8>, Error> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let original = transport(pending)?;
    if original.workspace_effect_sha256() != crate::cassette::digest(b"openjiuwen-grep-v1") {
        return Err(invalid());
    }
    let mut rows = pending
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    let last = rows.last_mut().ok_or_else(invalid)?;
    if last.get("event").and_then(serde_json::Value::as_str) != Some("workspace_effect") {
        return Err(invalid());
    }
    last["sha256"] = serde_json::Value::String(digest.to_owned());
    let mut output = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut output, &row).map_err(|_| invalid())?;
        output.push(b'\n');
    }
    transport(&output)?;
    Ok(output)
}

pub(super) fn verify(native: &[u8], archive: &mut File, digest: &str) -> Result<(), Error> {
    let transport = transport(native)?;
    let alias = "openjiuwen-native-tools-1";
    if transport.launch_session() != "openjiuwen-native-1"
        || transport.workspace_effect_sha256() != digest
        || transport.sessions().len() != 1
        || transport.sessions()[0].alias() != alias
        || !transport.sessions()[0].success()
    {
        return Err(invalid());
    }
    let mut order = Vec::new();
    let mut ordinals = BTreeSet::new();
    for request in transport.recorded_requests(alias).ok_or_else(invalid)? {
        let value: serde_json::Value = serde_json::from_slice(request).map_err(|_| invalid())?;
        let invocation = value
            .get("_asb_openjiuwen_invocation")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(invalid)?;
        let ordinal = invocation
            .get("ordinal")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(invalid)?;
        let sequence = invocation
            .get("snapshot_sequence")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(invalid)?;
        if invocation.get("depth").and_then(serde_json::Value::as_u64) != Some(0)
            || invocation.get("parent_ordinal") != Some(&serde_json::Value::Null)
            || !ordinals.insert(ordinal)
        {
            return Err(invalid());
        }
        order.push((sequence, format!("openjiuwen-{ordinal}")));
    }
    order.sort_by_key(|(sequence, _)| *sequence);
    if order
        .iter()
        .enumerate()
        .any(|(index, (sequence, _))| *sequence != index as u64 + 2)
    {
        return Err(invalid());
    }
    let aliases: Vec<_> = order.into_iter().map(|(_, alias)| alias).collect();
    crate::agents::snapshot_markers::verify_markers(archive, &aliases)
}
