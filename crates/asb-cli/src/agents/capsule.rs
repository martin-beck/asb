//! Private, bounded manifest for explicit native replay capsules.
//!
//! A capsule records workspace-relative objects at controlled boundaries. It
//! is not a filesystem interceptor or security policy: a later staging backend
//! reconstructs these declared inputs/effects before launching the real agent.

use super::native_trace::Blob;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    /// A workspace-relative path. The manifest is private recording material.
    pub path: String,
    pub object: Blob,
    pub mode: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_executable: Option<Blob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Entry>,
}

/// Reject non-canonical/private-unsafe manifest data before a capsule enters a
/// recording. Object bytes are staged separately and verified by digest.
pub(crate) fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Run("native capsule manifest exceeds bound".into()));
    }
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|_| Error::Run("native capsule manifest is malformed".into()))?;
    validate(&manifest)?;
    serde_json::to_vec(&manifest)
        .map_err(|_| Error::Io("cannot encode native capsule manifest".into()))
}

fn validate(manifest: &Manifest) -> Result<(), Error> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.base.len() > MAX_ENTRIES
        || manifest.effects.len() > MAX_ENTRIES
        || !manifest.root_executable.iter().all(valid_blob)
    {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    let mut base = BTreeSet::new();
    for entry in &manifest.base {
        valid_entry(entry, &mut base)?;
    }
    let mut effects = BTreeSet::new();
    for entry in &manifest.effects {
        valid_entry(entry, &mut effects)?;
    }
    Ok(())
}

fn valid_entry(entry: &Entry, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    let path = Path::new(&entry.path);
    if entry.path.len() > 1024
        || path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || entry.mode > 0o777
        || !valid_blob(&entry.object)
        || !paths.insert(entry.path.clone())
    {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    Ok(())
}

fn valid_blob(blob: &Blob) -> bool {
    blob.sha256.len() == 64 && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(byte: char) -> Blob {
        Blob {
            sha256: std::iter::repeat_n(byte, 64).collect(),
            bytes: 3,
        }
    }

    #[test]
    fn canonical_manifest_retains_only_valid_bounded_workspace_entries() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            root_executable: Some(blob('a')),
            base: vec![Entry {
                path: "src/main.rs".into(),
                object: blob('b'),
                mode: 0o644,
            }],
            effects: vec![Entry {
                path: "answer.txt".into(),
                object: blob('c'),
                mode: 0o644,
            }],
        };
        let bytes = canonicalize(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Manifest>(&bytes).unwrap(),
            manifest
        );
    }

    #[test]
    fn rejects_escape_duplicate_unknown_and_unbounded_entries() {
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            root_executable: None,
            base: vec![Entry {
                path: "../escape".into(),
                object: blob('a'),
                mode: 0o644,
            }],
            effects: Vec::new(),
        };
        assert!(canonicalize(&serde_json::to_vec(&manifest).unwrap()).is_err());
        manifest.base[0].path = "safe".into();
        manifest.base.push(manifest.base[0].clone());
        assert!(canonicalize(&serde_json::to_vec(&manifest).unwrap()).is_err());
        assert!(canonicalize(br#"{"schema_version":1,"unknown":true}"#).is_err());
    }
}
