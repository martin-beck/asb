//! Project configuration for explicitly selected replay normalization.
//!
//! Parsing and validating a profile never executes its module. Execution is
//! admitted separately, after the selected sources and policy are frozen.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Component, Path};

const MAX_PROFILES: usize = 32;
const MAX_SELECTORS: usize = 64;
const MAX_VALUE_BYTES: usize = 256;
const MAX_SOURCE_BYTES: u64 = 64 * 1024;
const MAX_POLICY_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
const ABI: &str = "asb-tool-result-normalizer-v1";

/// Runtime-owned workload identity used to select profile declarations.
pub struct NormalizationSelection<'a> {
    pub agent: &'a str,
    pub workload: &'a str,
    pub family: Option<&'a str>,
    pub task: &'a str,
}

/// Frozen module bytes and a path-independent identity for one policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenNormalizer {
    pub id: String,
    pub source: Vec<u8>,
    pub sha256: String,
    pub selectors: ProfileSelectors,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSelectors {
    pub agents: Option<Vec<String>>,
    pub workload_families: Option<Vec<String>>,
    pub workloads: Option<Vec<String>>,
    pub tasks: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenPolicy {
    pub normalizers: Vec<FrozenNormalizer>,
    pub sha256: String,
}

/// Public, content-free identity retained in a campaign plan. Source code is
/// kept separately as a declared input; inspection never executes it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDescriptor {
    pub abi: String,
    pub sha256: String,
    pub profiles: Vec<ProfileDescriptor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDescriptor {
    pub id: String,
    pub source_sha256: String,
    pub selectors: ProfileSelectors,
}

impl PolicyDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        if self.abi != ABI
            || !valid_digest(&self.sha256)
            || self.profiles.is_empty()
            || self.profiles.len() > MAX_PROFILES
        {
            return Err("invalid normalization policy descriptor".into());
        }
        let mut ids = HashSet::new();
        for profile in &self.profiles {
            if !valid_value(&profile.id)
                || !ids.insert(&profile.id)
                || !valid_digest(&profile.source_sha256)
                || (profile.selectors.workloads.is_none()
                    && profile.selectors.workload_families.is_none())
                || profile
                    .selectors
                    .tasks
                    .as_ref()
                    .is_some_and(|tasks| tasks.iter().any(|task| task.contains(['[', ']', '\\'])))
            {
                return Err("invalid normalization profile descriptor".into());
            }
            for list in [
                &profile.selectors.agents,
                &profile.selectors.workload_families,
                &profile.selectors.workloads,
                &profile.selectors.tasks,
            ]
            .into_iter()
            .flatten()
            {
                if list.is_empty()
                    || list.len() > MAX_SELECTORS
                    || list.iter().any(|value| !valid_value(value))
                {
                    return Err("invalid normalization profile selectors".into());
                }
            }
        }
        if self.sha256 != policy_digest(&self.profiles) {
            return Err("normalization policy digest differs from descriptor".into());
        }
        Ok(())
    }
}

impl FrozenPolicy {
    /// Encode bounded source snapshots for a declared cassette artifact.
    /// This artifact is inspection evidence only; replay must authorize and
    /// compare an independently configured local policy before execution.
    pub(crate) fn cassette_artifact(&self) -> Result<Option<Vec<u8>>, String> {
        let Some(descriptor) = self.descriptor()? else {
            return Ok(None);
        };
        let profiles = self
            .normalizers
            .iter()
            .map(|profile| {
                let source = std::str::from_utf8(&profile.source)
                    .map_err(|_| "invalid frozen normalizer source".to_owned())?;
                Ok(serde_json::json!({ "id": profile.id, "source": source }))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "scope": "normalization-source-inspection-only",
            "policy": descriptor,
            "profiles": profiles,
        }))
        .map_err(|error| format!("cannot encode normalizer source artifact: {error}"))?;
        if bytes.len() > MAX_POLICY_ARTIFACT_BYTES {
            return Err("normalizer source artifact exceeds the 8 MiB limit".into());
        }
        Ok(Some(bytes))
    }
    /// Publish a new private snapshot directory. Never replace existing files;
    /// partial publications are retained for inspection on error.
    pub(crate) fn persist(&self, directory: &Path) -> Result<(), String> {
        self.validate_sources()?;
        if self.normalizers.is_empty() {
            return Err("cannot persist an empty normalizer policy".into());
        }
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .map_err(|error| format!("cannot create normalizer snapshot directory: {error}"))?;
        for (index, profile) in self.normalizers.iter().enumerate() {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options
                .open(directory.join(format!("{index:02}.mjs")))
                .map_err(|error| {
                    format!("cannot create normalizer snapshot {}: {error}", profile.id)
                })?;
            file.write_all(&profile.source)
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    format!("cannot write normalizer snapshot {}: {error}", profile.id)
                })?;
        }
        std::fs::File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| format!("cannot sync normalizer snapshot directory: {error}"))?;
        Ok(())
    }

    /// Restore only declared files, checking their exact bytes against the
    /// descriptor. This does not authorize executing a cassette's source.
    pub(crate) fn restore(directory: &Path, descriptor: &PolicyDescriptor) -> Result<Self, String> {
        descriptor.validate()?;
        let mut normalizers = Vec::new();
        for (index, entry) in descriptor.profiles.iter().enumerate() {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
            }
            let file = options
                .open(directory.join(format!("{index:02}.mjs")))
                .map_err(|error| {
                    format!("cannot open normalizer snapshot {}: {error}", entry.id)
                })?;
            let metadata = file.metadata().map_err(|error| {
                format!("cannot inspect normalizer snapshot {}: {error}", entry.id)
            })?;
            if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
                return Err(format!("invalid normalizer snapshot {}", entry.id));
            }
            let mut source = Vec::new();
            file.take(MAX_SOURCE_BYTES + 1)
                .read_to_end(&mut source)
                .map_err(|error| {
                    format!("cannot read normalizer snapshot {}: {error}", entry.id)
                })?;
            normalizers.push(FrozenNormalizer {
                id: entry.id.clone(),
                source,
                sha256: entry.source_sha256.clone(),
                selectors: entry.selectors.clone(),
            });
        }
        let restored = Self {
            normalizers,
            sha256: descriptor.sha256.clone(),
        };
        restored.validate_sources()?;
        Ok(restored)
    }
    /// Construct the policy identity from its ordered, frozen profile data.
    pub fn from_normalizers(normalizers: Vec<FrozenNormalizer>) -> Result<Self, String> {
        let profiles: Vec<_> = normalizers
            .iter()
            .map(|profile| ProfileDescriptor {
                id: profile.id.clone(),
                source_sha256: profile.sha256.clone(),
                selectors: profile.selectors.clone(),
            })
            .collect();
        let policy = Self {
            sha256: policy_digest(&profiles),
            normalizers,
        };
        policy.validate_sources()?;
        Ok(policy)
    }
    pub fn descriptor(&self) -> Result<Option<PolicyDescriptor>, String> {
        self.validate_sources()?;
        if self.normalizers.is_empty() {
            return Ok(None);
        }
        let descriptor = PolicyDescriptor {
            abi: ABI.into(),
            sha256: self.sha256.clone(),
            profiles: self
                .normalizers
                .iter()
                .map(|profile| ProfileDescriptor {
                    id: profile.id.clone(),
                    source_sha256: profile.sha256.clone(),
                    selectors: profile.selectors.clone(),
                })
                .collect(),
        };
        descriptor.validate()?;
        Ok(Some(descriptor))
    }
    /// Check source bytes and the policy digest before handing them to a recorder.
    pub fn validate_sources(&self) -> Result<(), String> {
        if self.normalizers.len() > MAX_PROFILES || !valid_digest(&self.sha256) {
            return Err("invalid frozen normalizer policy identity".into());
        }
        let mut ids = HashSet::new();
        for profile in &self.normalizers {
            if !valid_value(&profile.id)
                || !ids.insert(&profile.id)
                || profile.source.len() as u64 > MAX_SOURCE_BYTES
                || std::str::from_utf8(&profile.source).is_err()
                || profile.sha256 != format!("{:x}", Sha256::digest(&profile.source))
            {
                return Err(format!(
                    "invalid frozen source for normalizer {}",
                    profile.id
                ));
            }
        }
        let profiles: Vec<_> = self
            .normalizers
            .iter()
            .map(|profile| ProfileDescriptor {
                id: profile.id.clone(),
                source_sha256: profile.sha256.clone(),
                selectors: profile.selectors.clone(),
            })
            .collect();
        if self.sha256 != policy_digest(&profiles) {
            return Err(
                "frozen normalizer policy digest differs from its sources and selectors".into(),
            );
        }
        if !profiles.is_empty() {
            PolicyDescriptor {
                abi: ABI.into(),
                sha256: self.sha256.clone(),
                profiles,
            }
            .validate()?;
        }
        Ok(())
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReplayConfig {
    normalizers: Vec<Profile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    id: String,
    module: String,
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_enabled")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload_families: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workloads: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tasks: Option<Vec<String>>,
}

fn enabled_by_default() -> bool {
    true
}
fn is_enabled(value: &bool) -> bool {
    *value
}

impl ReplayConfig {
    pub(crate) fn is_empty(&self) -> bool {
        self.normalizers.iter().all(|profile| !profile.enabled)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        self.validate_declarations()
    }

    fn validate_declarations(&self) -> Result<(), String> {
        if self.normalizers.len() > MAX_PROFILES {
            return Err(format!(
                "replay.normalizers exceeds the limit of {MAX_PROFILES} profiles"
            ));
        }
        let mut ids = HashSet::new();
        for profile in &self.normalizers {
            if !valid_value(&profile.id) || !ids.insert(&profile.id) {
                return Err("normalizer IDs must be nonempty, bounded and unique".into());
            }
            let path = Path::new(&profile.module);
            if !valid_value(&profile.module)
                || !path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err(format!(
                    "normalizer {} must use a relative module path without traversal",
                    profile.id
                ));
            }
            if profile.workloads.is_none() && profile.workload_families.is_none() {
                return Err(format!(
                    "normalizer {} requires a workload or workload-family selector",
                    profile.id
                ));
            }
            for (name, values) in [
                ("agents", &profile.agents),
                ("workload_families", &profile.workload_families),
                ("workloads", &profile.workloads),
                ("tasks", &profile.tasks),
            ] {
                if let Some(values) = values {
                    if values.is_empty()
                        || values.len() > MAX_SELECTORS
                        || values.iter().any(|v| !valid_value(v))
                    {
                        return Err(format!(
                            "normalizer {} has invalid {name} selectors",
                            profile.id
                        ));
                    }
                    if name == "tasks" && values.iter().any(|v| v.contains(['[', ']', '\\'])) {
                        return Err(format!(
                            "normalizer {} task patterns support only * and ? wildcards",
                            profile.id
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Select and snapshot project-local modules in declaration order. Callers
    /// must bind the returned policy digest and bytes to the execution plan
    /// before allowing any normalizer to execute.
    pub fn freeze(
        &self,
        root: &Path,
        selection: &NormalizationSelection<'_>,
    ) -> Result<FrozenPolicy, String> {
        self.validate_declarations()?;
        let root = root
            .canonicalize()
            .map_err(|e| format!("cannot resolve project directory: {e}"))?;
        let mut normalizers = Vec::new();
        for profile in &self.normalizers {
            if !profile.matches(selection) {
                continue;
            }
            let path = root
                .join(&profile.module)
                .canonicalize()
                .map_err(|e| format!("cannot resolve normalizer {}: {e}", profile.id))?;
            if !path.starts_with(&root) {
                return Err(format!(
                    "normalizer {} resolves outside the project",
                    profile.id
                ));
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            // A FIFO must never make planning block while waiting for a writer.
            // Reject final-component replacements with symlinks at open time.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
            }
            let file = options
                .open(&path)
                .map_err(|e| format!("cannot open normalizer {}: {e}", profile.id))?;
            let metadata = file
                .metadata()
                .map_err(|e| format!("cannot inspect normalizer {}: {e}", profile.id))?;
            if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
                return Err(format!(
                    "normalizer {} must be a regular file of at most {MAX_SOURCE_BYTES} bytes",
                    profile.id
                ));
            }
            let mut source = Vec::new();
            file.take(MAX_SOURCE_BYTES + 1)
                .read_to_end(&mut source)
                .map_err(|e| format!("cannot read normalizer {}: {e}", profile.id))?;
            if source.len() as u64 > MAX_SOURCE_BYTES || std::str::from_utf8(&source).is_err() {
                return Err(format!(
                    "normalizer {} source must be bounded UTF-8",
                    profile.id
                ));
            }
            let source_digest = format!("{:x}", Sha256::digest(&source));
            normalizers.push(FrozenNormalizer {
                id: profile.id.clone(),
                source,
                sha256: source_digest,
                selectors: ProfileSelectors {
                    agents: profile.agents.clone(),
                    workload_families: profile.workload_families.clone(),
                    workloads: profile.workloads.clone(),
                    tasks: profile.tasks.clone(),
                },
            });
        }
        let policy = FrozenPolicy::from_normalizers(normalizers)?;
        // Check the exact declared artifact size while planning, before any
        // client or provider is started.
        policy.cassette_artifact()?;
        Ok(policy)
    }

    /// Resolve against a verified planning selection rather than guessing
    /// family or task identity from request text or repository names.
    pub(crate) fn freeze_for_task(
        &self,
        root: &Path,
        agent: &str,
        selection: &crate::workloads::TaskSelection,
    ) -> Result<FrozenPolicy, String> {
        if !selection.valid() {
            return Err("invalid workload task selection for replay normalizers".into());
        }
        self.freeze(
            root,
            &NormalizationSelection {
                agent,
                workload: &selection.workload,
                family: selection
                    .module
                    .as_ref()
                    .map(|module| module.family.as_str()),
                task: &selection.task_id,
            },
        )
    }
}

impl Profile {
    fn matches(&self, selection: &NormalizationSelection<'_>) -> bool {
        let contains = |values: &Option<Vec<String>>, value: Option<&str>| {
            values
                .as_ref()
                .is_none_or(|list| value.is_some_and(|item| list.iter().any(|v| v == item)))
        };
        self.enabled
            && contains(&self.agents, Some(selection.agent))
            && contains(&self.workloads, Some(selection.workload))
            && contains(&self.workload_families, selection.family)
            && self.tasks.as_ref().is_none_or(|tasks| {
                tasks
                    .iter()
                    .any(|pattern| glob_matches(pattern, selection.task))
            })
    }
}

fn hash_field(digest: &mut Sha256, field: &[u8]) {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
}

fn policy_digest(profiles: &[ProfileDescriptor]) -> String {
    let mut digest = Sha256::new();
    hash_field(&mut digest, ABI.as_bytes());
    for profile in profiles {
        hash_field(&mut digest, profile.id.as_bytes());
        for selectors in [
            &profile.selectors.agents,
            &profile.selectors.workload_families,
            &profile.selectors.workloads,
            &profile.selectors.tasks,
        ] {
            hash_field(
                &mut digest,
                if selectors.is_some() {
                    b"present"
                } else {
                    b"absent"
                },
            );
            if let Some(selectors) = selectors {
                hash_field(&mut digest, &(selectors.len() as u64).to_be_bytes());
                for value in selectors {
                    hash_field(&mut digest, value.as_bytes());
                }
            }
        }
        hash_field(&mut digest, profile.source_sha256.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let text: Vec<char> = value.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    for token in pattern.chars() {
        let mut next = vec![false; text.len() + 1];
        if token == '*' {
            next[0] = previous[0];
        }
        for index in 1..=text.len() {
            next[index] = if token == '*' {
                previous[index] || next[index - 1]
            } else {
                previous[index - 1] && (token == '?' || token == text[index - 1])
            };
        }
        previous = next;
    }
    previous[text.len()]
}

fn valid_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_VALUE_BYTES && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{NormalizationSelection, ReplayConfig, glob_matches};

    #[test]
    fn disabled_profiles_are_not_loaded_or_selected() {
        let root = std::env::temp_dir();
        let text = "[[normalizers]]\nid='off'\nmodule='missing.mjs'\nworkloads=['swe-bench']\nenabled=false\n";
        let config: ReplayConfig = toml::from_str(text).unwrap();
        assert!(config.is_empty());
        let selected = NormalizationSelection {
            agent: "pi",
            workload: "swe-bench",
            family: None,
            task: "case",
        };
        assert!(
            config
                .freeze(&root, &selected)
                .unwrap()
                .normalizers
                .is_empty()
        );
        let enabled: ReplayConfig = toml::from_str(&text.replace("false", "true")).unwrap();
        assert!(!enabled.is_empty());
        assert!(enabled.freeze(&root, &selected).is_err());
    }

    #[test]
    fn profile_selection_snapshots_bytes_and_binds_selectors() {
        let root = std::env::temp_dir().join(format!(
            "asb-normalizer-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("module.mjs"), "export const value = 1;").unwrap();
        let config: ReplayConfig = toml::from_str("[[normalizers]]\nid='sympy'\nmodule='module.mjs'\nagents=['pi']\nworkload_families=['swe-bench']\nworkloads=['swe-bench-lite']\ntasks=['sympy__sympy-*']").unwrap();
        let selection = NormalizationSelection {
            agent: "pi",
            workload: "swe-bench-lite",
            family: Some("swe-bench"),
            task: "sympy__sympy-22236",
        };
        let first = config.freeze(&root, &selection).unwrap();
        first.validate_sources().unwrap();
        let descriptor = first.descriptor().unwrap().unwrap();
        let artifact: serde_json::Value =
            serde_json::from_slice(&first.cassette_artifact().unwrap().unwrap()).unwrap();
        assert_eq!(artifact["scope"], "normalization-source-inspection-only");
        assert_eq!(artifact["policy"]["sha256"], first.sha256);
        assert_eq!(artifact["profiles"][0]["source"], "export const value = 1;");
        let snapshot = root.join("snapshot");
        first.persist(&snapshot).unwrap();
        assert_eq!(
            super::FrozenPolicy::restore(&snapshot, &descriptor).unwrap(),
            first
        );
        assert!(first.persist(&snapshot).is_err());
        assert_eq!(descriptor.profiles[0].id, "sympy");
        assert_eq!(
            descriptor.profiles[0].source_sha256,
            first.normalizers[0].sha256
        );
        assert!(
            !serde_json::to_string(&descriptor)
                .unwrap()
                .contains("export const")
        );
        let mut altered_descriptor = descriptor.clone();
        altered_descriptor.profiles[0].selectors.tasks = Some(vec!["other".into()]);
        assert!(
            altered_descriptor
                .validate()
                .unwrap_err()
                .contains("digest differs")
        );
        assert_eq!(first.normalizers.len(), 1);
        std::fs::write(root.join("module.mjs"), "export const value = 2;").unwrap();
        let second = config.freeze(&root, &selection).unwrap();
        assert_ne!(first.sha256, second.sha256);
        assert_eq!(first.normalizers[0].source, b"export const value = 1;");
        assert_eq!(
            super::FrozenPolicy::restore(&snapshot, &descriptor).unwrap(),
            first
        );
        std::fs::write(snapshot.join("00.mjs"), "tampered").unwrap();
        assert!(super::FrozenPolicy::restore(&snapshot, &descriptor).is_err());
        let mut tampered = first.clone();
        tampered.normalizers[0].source.push(b'!');
        assert!(
            tampered
                .validate_sources()
                .unwrap_err()
                .contains("invalid frozen source")
        );
        let mut changed_selector = first.clone();
        changed_selector.normalizers[0].selectors.tasks = Some(vec!["other".into()]);
        assert!(
            changed_selector
                .validate_sources()
                .unwrap_err()
                .contains("policy digest differs")
        );
        let wrong_family = NormalizationSelection {
            family: None,
            ..selection
        };
        assert!(
            config
                .freeze(&root, &wrong_family)
                .unwrap()
                .normalizers
                .is_empty()
        );
        assert!(glob_matches("sympy__?*", "sympy__x"));
        assert!(!glob_matches("sympy__?", "sympy__xx"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn selected_source_must_stay_in_project_and_fit_the_size_limit() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "asb-normalizer-bounds-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let external = root.join("external.mjs");
        std::fs::write(&external, "outside").unwrap();
        symlink(&external, project.join("module.mjs")).unwrap();
        let config: ReplayConfig =
            toml::from_str("[[normalizers]]\nid='x'\nmodule='module.mjs'\nworkloads=['swe-bench']")
                .unwrap();
        let selection = NormalizationSelection {
            agent: "pi",
            workload: "swe-bench",
            family: None,
            task: "case",
        };
        assert!(
            config
                .freeze(&project, &selection)
                .unwrap_err()
                .contains("outside")
        );
        std::fs::remove_file(project.join("module.mjs")).unwrap();
        std::fs::write(project.join("module.mjs"), vec![b'a'; 64 * 1024 + 1]).unwrap();
        assert!(
            config
                .freeze(&project, &selection)
                .unwrap_err()
                .contains("regular file")
        );
        std::fs::remove_file(project.join("module.mjs")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(project.join("module.mjs"))
                .status()
                .unwrap()
                .success()
        );
        assert!(
            config
                .freeze(&project, &selection)
                .unwrap_err()
                .contains("regular file")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_identity_ignores_module_path_but_tracks_selector_order() {
        let root = std::env::temp_dir().join(format!(
            "asb-normalizer-identity-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("one.mjs"), "same source").unwrap();
        std::fs::write(root.join("two.mjs"), "same source").unwrap();
        let declaration = "[[normalizers]]\nid='x'\nmodule='one.mjs'\nworkloads=['one','two']";
        let config: ReplayConfig = toml::from_str(declaration).unwrap();
        let moved: ReplayConfig =
            toml::from_str(&declaration.replace("one.mjs", "two.mjs")).unwrap();
        let reordered: ReplayConfig =
            toml::from_str(&declaration.replace("['one','two']", "['two','one']")).unwrap();
        let selection = NormalizationSelection {
            agent: "pi",
            workload: "one",
            family: None,
            task: "case",
        };
        let digest = config.freeze(&root, &selection).unwrap().sha256;
        assert_eq!(digest, moved.freeze(&root, &selection).unwrap().sha256);
        assert_ne!(digest, reordered.freeze(&root, &selection).unwrap().sha256);
        let mut descriptor = config
            .freeze(&root, &selection)
            .unwrap()
            .descriptor()
            .unwrap()
            .unwrap();
        descriptor.profiles[0].selectors.workloads = None;
        descriptor.sha256 = super::policy_digest(&descriptor.profiles);
        assert!(
            descriptor.validate().is_err(),
            "a saved profile must still select a workload"
        );
        descriptor.profiles[0].selectors.workloads = Some(vec!["one".into()]);
        descriptor.profiles[0].selectors.tasks = Some(vec!["[bad]".into()]);
        descriptor.sha256 = super::policy_digest(&descriptor.profiles);
        assert!(
            descriptor.validate().is_err(),
            "saved task patterns obey the declaration grammar"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn planned_task_selection_owns_the_profile_matching_identity() {
        let root = std::env::temp_dir().join(format!(
            "asb-normalizer-selection-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("module.mjs"), "source").unwrap();
        let config: ReplayConfig = toml::from_str(
            "[[normalizers]]\nid='one'\nmodule='module.mjs'\nagents=['pi']\nworkloads=['bug-fix']\ntasks=['bug-fix']",
        ).unwrap();
        let selection = crate::workloads::TaskSelection {
            module: None,
            workload: "bug-fix".into(),
            task_id: "bug-fix".into(),
            dataset: None,
            evaluation_kind: "local".into(),
            population_size: None,
        };
        assert_eq!(
            config
                .freeze_for_task(&root, "pi", &selection)
                .unwrap()
                .normalizers
                .len(),
            1
        );
        assert!(
            config
                .freeze_for_task(&root, "codex", &selection)
                .unwrap()
                .normalizers
                .is_empty()
        );
        let mut invalid = selection;
        invalid.task_id.clear();
        assert!(
            config
                .freeze_for_task(&root, "pi", &invalid)
                .unwrap_err()
                .contains("invalid workload")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_malformed_policies_without_executing_code() {
        for text in [
            "[[normalizers]]\nid='x'\nmodule='../x.mjs'\nworkloads=['swe-bench']",
            "[[normalizers]]\nid='x'\nmodule='x.mjs'\nworkloads=[]",
            "[[normalizers]]\nid='x'\nmodule='x.mjs'\nworkloads=['swe-bench']\ntasks=['[bad]']",
            "[[normalizers]]\nid='x'\nmodule='x.mjs'\nworkloads=['swe-bench']\n[[normalizers]]\nid='x'\nmodule='y.mjs'\nworkloads=['swe-bench']",
        ] {
            let config: ReplayConfig = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "{text}");
        }
        let config: ReplayConfig = toml::from_str(
            "[[normalizers]]\nid='x'\nmodule='normalizers/x.mjs'\nworkloads=['swe-bench']",
        )
        .unwrap();
        assert!(config.validate().is_ok());
        assert!(toml::from_str::<ReplayConfig>("unknown = true").is_err());
    }
}
