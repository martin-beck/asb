//! Versioned descriptions of benchmark modules, independent of orchestration.
use super::session::DatasetIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SupportLevel {
    LocalTask,
    ReferenceSmoke,
    OfficialAdapter,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EnvironmentKind {
    Native,
    LocalDocker,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SubmissionKind {
    Workspace,
    ResponseText,
    Patch,
    EnvironmentState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EvaluationMode {
    Local,
    FreshEnvironment,
    InPlace,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarnessIdentity {
    pub id: String,
    /// Exact package recipe; the owning tool registry is authoritative.
    pub package: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Protocol {
    pub id: String,
    pub revision: u32,
    pub environment: EnvironmentKind,
    pub submission: SubmissionKind,
    pub evaluation: EvaluationMode,
    pub score_policy: String,
    pub input_modalities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModuleDescriptor {
    pub schema_version: u32,
    pub id: String,
    pub family: String,
    pub suite_version: String,
    pub support: SupportLevel,
    pub dataset: Option<DatasetIdentity>,
    pub harness: Option<HarnessIdentity>,
    pub protocol: Protocol,
}

impl ModuleDescriptor {
    pub(crate) fn local(id: &str, evaluation_kind: super::KnownEvaluationKind) -> Self {
        let smoke = evaluation_kind == super::KnownEvaluationKind::AdapterSmoke;
        Self {
            schema_version: 1,
            id: id.into(),
            family: if smoke { id } else { "asb-local" }.into(),
            suite_version: "1".into(),
            support: if smoke {
                SupportLevel::ReferenceSmoke
            } else {
                SupportLevel::LocalTask
            },
            dataset: None,
            harness: None,
            protocol: Protocol {
                id: evaluation_kind.as_str().into(),
                revision: 1,
                environment: EnvironmentKind::Native,
                submission: SubmissionKind::Workspace,
                evaluation: EvaluationMode::Local,
                score_policy: if smoke { "unscored" } else { "task-resolution" }.into(),
                input_modalities: vec!["text".into()],
            },
        }
    }

    pub(crate) fn official(
        id: &str,
        family: &str,
        suite_version: &str,
        dataset: DatasetIdentity,
        tool: &str,
        submission: SubmissionKind,
        evaluation: EvaluationMode,
    ) -> Self {
        let package = match crate::tools::find(tool)
            .expect("registered harness tool")
            .recipe()
        {
            crate::install::Recipe::Python { package, .. } => package,
            _ => unreachable!("official harnesses use project-local Python recipes"),
        };
        Self {
            schema_version: 1,
            id: id.into(),
            family: family.into(),
            suite_version: suite_version.into(),
            support: SupportLevel::OfficialAdapter,
            dataset: Some(dataset),
            harness: Some(HarnessIdentity {
                id: tool.into(),
                package: package.into(),
            }),
            protocol: Protocol {
                id: format!("{tool}-official"),
                revision: 1,
                environment: EnvironmentKind::LocalDocker,
                submission,
                evaluation,
                score_policy: "task-resolution".into(),
                input_modalities: vec!["text".into()],
            },
        }
    }

    pub(crate) fn valid(&self) -> bool {
        let bounded =
            |s: &str| !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control);
        let score_policy = self.protocol.score_policy == "task-resolution"
            || self
                .protocol
                .score_policy
                .strip_prefix("metric:")
                .and_then(|s| s.rsplit_once('@'))
                .is_some_and(|(id, version)| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                        && version
                            .parse::<u32>()
                            .is_ok_and(|v| v > 0 && v.to_string() == version)
                });
        self.schema_version == 1
            && [
                &self.id,
                &self.family,
                &self.suite_version,
                &self.protocol.id,
                &self.protocol.score_policy,
            ]
            .into_iter()
            .all(|s| bounded(s))
            && self.protocol.revision > 0
            && self.protocol.input_modalities == ["text"]
            && self.dataset.as_ref().is_none_or(|d| {
                [&d.source, &d.revision, &d.split]
                    .into_iter()
                    .all(|s| bounded(s))
            })
            && self
                .harness
                .as_ref()
                .is_none_or(|h| bounded(&h.id) && bounded(&h.package))
            && match self.support {
                SupportLevel::OfficialAdapter => {
                    self.dataset.is_some()
                        && self.harness.is_some()
                        && self.protocol.environment == EnvironmentKind::LocalDocker
                        && score_policy
                }
                SupportLevel::ReferenceSmoke => {
                    self.dataset.is_none()
                        && self.harness.is_none()
                        && self.protocol.score_policy == "unscored"
                }
                SupportLevel::LocalTask => {
                    self.dataset.is_none() && self.harness.is_none() && score_policy
                }
            }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/workload_contract_baselines.rs"]
mod baselines;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_profiles_bind_dataset_harness_and_protocol_without_installation() {
        for id in [
            "swe-bench",
            "swe-bench-lite",
            "swe-bench-verified",
            "terminal-bench-1.0",
            "terminal-bench-2.1",
        ] {
            let profile = super::super::descriptor(id).unwrap();
            assert!(profile.valid(), "{id}");
            assert_eq!(profile.support, SupportLevel::OfficialAdapter);
            assert_eq!(
                profile,
                serde_json::from_value(serde_json::to_value(&profile).unwrap()).unwrap()
            );
            let mut invalid = profile.clone();
            invalid.harness = None;
            assert!(!invalid.valid());
            invalid = profile;
            invalid.schema_version = 2;
            assert!(!invalid.valid());
        }
        let swe = super::super::descriptor("swe-bench-lite").unwrap();
        let terminal = super::super::descriptor("terminal-bench-2.1").unwrap();
        assert_ne!(swe.family, terminal.family);
        assert_eq!(terminal.harness.unwrap().package, "harbor==0.24.0");
        assert_eq!(swe.protocol.submission, SubmissionKind::Patch);
    }

    #[test]
    fn reference_names_cannot_advertise_official_scores_and_unknowns_stay_unknown() {
        for id in ["swe-bench-pro", "tau-bench"] {
            let mut profile = super::super::descriptor(id).unwrap();
            assert!(profile.valid());
            assert_eq!(profile.support, SupportLevel::ReferenceSmoke);
            profile.protocol.score_policy = "task-resolution".into();
            assert!(!profile.valid());
        }
        assert!(super::super::descriptor("unregistered-user-prompt").is_none());
        assert!(super::super::descriptor("bug-fix").unwrap().valid());
    }

    #[test]
    fn discovery_uses_the_same_registered_contract_without_project_side_effects() {
        let mut output = Vec::new();
        super::super::describe_module("swe-bench-lite", &mut output).unwrap();
        let described: ModuleDescriptor = serde_json::from_slice(&output).unwrap();
        assert_eq!(Some(described), super::super::descriptor("swe-bench-lite"));
        assert!(super::super::describe_module("not-registered", &mut Vec::new()).is_err());
    }
}
