//! Provider-neutral proxy adapters and ordered chain lifecycle.

pub(crate) mod adapters;
pub(crate) mod capture;
pub(crate) mod tool_calls;

use crate::Error;
use std::any::Any;
use std::collections::HashMap;

/// Generic inputs for a proxy which owns LLM record/replay.
pub(crate) struct RecordingRequest<'a> {
    pub mode: crate::tape::Mode,
    pub recorder: Option<&'a str>,
    pub cassette: &'a std::path::Path,
    pub respect_record_timings: bool,
    pub workspace: &'a std::path::Path,
    pub wire_extension: Option<&'static str>,
}

/// An adapter starts one hop using its own optional typed configuration.
pub(crate) trait ProxyAdapter: Sync {
    fn name(&self) -> &'static str;
    fn default_recording(&self) -> bool {
        false
    }
    /// This adapter can operate during replay without any online backend.
    fn supports_offline_replay(&self) -> bool {
        false
    }
    fn recording_settings(&self, _: &RecordingRequest<'_>) -> Result<Box<dyn Any>, Error> {
        Err(Error::Input(
            "selected proxy does not support recording".into(),
        ))
    }
    fn validate_replay_model(&self, _path: &std::path::Path, _model: &str) -> Result<(), Error> {
        Err(Error::Input(
            "recording proxy does not support replay model validation".into(),
        ))
    }
    fn replay_interface(
        &self,
        _path: &std::path::Path,
    ) -> Result<Option<crate::endpoint::Interface>, Error> {
        Err(Error::Input(
            "recording proxy does not support replay interface discovery".into(),
        ))
    }
    /// Interpret only recordings in this adapter's own format.
    fn recorded_usage(&self, _text: &str, _input_includes_cache: bool) -> crate::usage::TokenUsage {
        crate::usage::TokenUsage::default()
    }
    fn recorded_tool_calls(&self, _path: &std::path::Path) -> Option<Vec<tool_calls::ToolCall>> {
        None
    }
    /// Adapter-owned execution files, without starting a hop or touching a
    /// cassette. Transitive interpreter/library discovery is a separate layer.
    fn input_files(&self) -> Result<Vec<std::path::PathBuf>, Error> {
        Err(Error::Input(
            "selected proxy does not declare its execution inputs".into(),
        ))
    }
    fn runtime_inputs(&self) -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
        Ok(self
            .input_files()?
            .into_iter()
            .map(crate::runtime_inputs::Dependency::program)
            .collect())
    }
    fn start(
        &self,
        upstream: Option<&str>,
        settings: Option<&dyn Any>,
    ) -> Result<Box<dyn ProxySession>, Error>;
}

fn select_recording_adapter<'a>(
    registry: &'a [&'a dyn ProxyAdapter],
) -> Result<&'a dyn ProxyAdapter, Error> {
    let mut candidates = registry
        .iter()
        .copied()
        .filter(|adapter| adapter.default_recording());
    let adapter = candidates
        .next()
        .ok_or_else(|| Error::Input("no default recording proxy is registered".into()))?;
    if candidates.next().is_some() {
        return Err(Error::Input(
            "multiple default recording proxies are registered".into(),
        ));
    }
    Ok(adapter)
}

fn find_recorder<'a>(
    registry: &'a [&'a dyn ProxyAdapter],
    name: &str,
) -> Option<&'a dyn ProxyAdapter> {
    registry
        .iter()
        .copied()
        .find(|adapter| adapter.name() == name)
}

pub(crate) fn recorder(name: &str) -> Option<&'static dyn ProxyAdapter> {
    find_recorder(adapters::REGISTRY, name)
}

pub(crate) fn legacy_recorder_name() -> &'static str {
    adapters::legacy_recording_name()
}

pub(crate) fn default_recorder() -> Option<&'static dyn ProxyAdapter> {
    select_recording_adapter(adapters::REGISTRY).ok()
}

pub(crate) fn start_recording_chain(
    request: &RecordingRequest<'_>,
    upstream: Option<&str>,
) -> Result<Chain, Error> {
    let adapter = resolve_recorder(adapters::REGISTRY, request.recorder)?;
    let names = configured_chain(adapter.name())?;
    start_recording_chain_with(request, upstream, &names, adapters::REGISTRY)
}

fn resolve_recorder<'a>(
    registry: &'a [&'a dyn ProxyAdapter],
    name: Option<&str>,
) -> Result<&'a dyn ProxyAdapter, Error> {
    match name {
        Some(name) => find_recorder(registry, name)
            .ok_or_else(|| Error::Input(format!("recording proxy `{name}` is not registered"))),
        None => select_recording_adapter(registry),
    }
}

fn start_recording_chain_with(
    request: &RecordingRequest<'_>,
    upstream: Option<&str>,
    names: &[String],
    registry: &[&dyn ProxyAdapter],
) -> Result<Chain, Error> {
    let adapter = resolve_recorder(registry, request.recorder)?;
    validate_recording_chain(names, adapter.name(), request.mode, registry)?;
    let mut configurations = HashMap::new();
    configurations.insert(
        adapter.name().to_owned(),
        adapter.recording_settings(request)?,
    );
    let upstream = if request.mode == crate::tape::Mode::Replay {
        None
    } else {
        upstream
    };
    let selected = names
        .iter()
        .map(|name| {
            registry
                .iter()
                .copied()
                .find(|adapter| adapter.name().eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    Error::Input(format!("unknown proxy adapter `{name}` in ASB_PROXY_CHAIN"))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Chain::start_selected(&selected, upstream, &configurations)
}

fn validate_recording_chain(
    names: &[String],
    recorder: &str,
    mode: crate::tape::Mode,
    registry: &[&dyn ProxyAdapter],
) -> Result<(), Error> {
    let matches = names
        .iter()
        .filter(|name| name.eq_ignore_ascii_case(recorder))
        .count();
    if matches != 1 {
        return Err(Error::Input(format!(
            "ASB_PROXY_CHAIN must contain the recording adapter `{recorder}` exactly once"
        )));
    }
    if mode == crate::tape::Mode::Replay {
        if names
            .last()
            .is_none_or(|name| !name.eq_ignore_ascii_case(recorder))
        {
            return Err(Error::Input(format!(
                "for offline replay, recording adapter `{recorder}` must be last in ASB_PROXY_CHAIN"
            )));
        }
        for name in names {
            let adapter = registry
                .iter()
                .find(|adapter| adapter.name().eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    Error::Input(format!("unknown proxy adapter `{name}` in ASB_PROXY_CHAIN"))
                })?;
            if !adapter.supports_offline_replay() {
                return Err(Error::Input(format!(
                    "proxy adapter `{name}` does not support offline replay"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn input_files(default_name: &str) -> Result<Vec<std::path::PathBuf>, Error> {
    let mut paths = Vec::new();
    for name in configured_chain(default_name)? {
        let adapter = adapters::REGISTRY
            .iter()
            .copied()
            .find(|adapter| adapter.name().eq_ignore_ascii_case(&name))
            .ok_or_else(|| {
                Error::Input("selected proxy has no registered input declaration".into())
            })?;
        paths.extend(adapter.input_files()?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn runtime_inputs(
    default_name: &str,
) -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
    let mut inputs = Vec::new();
    for name in configured_chain(default_name)? {
        let adapter = adapters::REGISTRY
            .iter()
            .copied()
            .find(|adapter| adapter.name().eq_ignore_ascii_case(&name))
            .ok_or_else(|| Error::Input("selected proxy has no runtime declaration".into()))?;
        inputs.extend(adapter.runtime_inputs()?);
    }
    Ok(inputs)
}

/// An owned hop. Both shutdown methods must be safe to call more than once.
pub(crate) trait ProxySession {
    fn url(&self) -> &str;
    /// Persistent outputs owned by this hop, not replay inputs or scratch.
    fn output_files(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// Resource teardown is distinct from request/completion validation.
    fn cleanup_confirmed(&self) -> bool {
        false
    }
    fn capture_evidence(&self) -> Option<capture::CaptureEvidence> {
        None
    }
    /// Stop and validate the completed session, if the adapter requires it.
    fn finish(&mut self) -> Result<(), Error>;
    /// Stop without completion validation, including on partial startup.
    fn abort(&mut self);
}

pub(crate) struct Completion {
    pub result: Result<(), Error>,
    pub evidence: Evidence,
}

pub(crate) struct Evidence {
    pub captures: Vec<capture::CaptureEvidence>,
    pub output_files: Vec<std::path::PathBuf>,
    pub cleanup_confirmed: bool,
}

pub(crate) struct Chain {
    hops: Vec<Box<dyn ProxySession>>,
    url: String,
}

impl Chain {
    #[cfg(test)]
    pub(crate) fn start(
        names: &[String],
        upstream: Option<&str>,
        configurations: &HashMap<String, Box<dyn Any>>,
    ) -> Result<Self, Error> {
        let selected = names
            .iter()
            .map(|name| {
                adapters::REGISTRY
                    .iter()
                    .copied()
                    .find(|adapter| adapter.name().eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        let available = adapters::REGISTRY
                            .iter()
                            .map(|adapter| adapter.name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        Error::Input(format!(
                            "unknown proxy adapter `{name}` in ASB_PROXY_CHAIN; available adapters: {available}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::start_selected(&selected, upstream, configurations)
    }

    fn start_selected(
        selected: &[&dyn ProxyAdapter],
        upstream: Option<&str>,
        configurations: &HashMap<String, Box<dyn Any>>,
    ) -> Result<Self, Error> {
        if selected.is_empty() {
            return Err(Error::Input(
                "proxy chain must contain at least one adapter".into(),
            ));
        }
        let mut hops: Vec<Box<dyn ProxySession>> = Vec::with_capacity(selected.len());
        let mut next_hop = upstream.map(str::to_owned);
        for adapter in selected.iter().rev() {
            let settings = configurations
                .get(adapter.name())
                .map(|value| value.as_ref());
            match adapter.start(next_hop.as_deref(), settings) {
                Ok(hop) => {
                    next_hop = Some(hop.url().to_owned());
                    hops.push(hop);
                }
                Err(error) => {
                    for hop in hops.iter_mut().rev() {
                        hop.abort();
                    }
                    return Err(Error::Run(format!(
                        "proxy adapter `{}` failed: {error}",
                        adapter.name()
                    )));
                }
            }
        }
        hops.reverse();
        Ok(Self {
            url: next_hop.expect("nonempty chain"),
            hops,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    #[cfg(test)]
    pub(crate) fn stop(self) -> Result<(), Error> {
        self.finish().result
    }

    pub(crate) fn finish(mut self) -> Completion {
        let mut failure = None;
        let mut output_files = self
            .hops
            .iter()
            .flat_map(|hop| hop.output_files())
            .collect::<Vec<_>>();
        output_files.sort();
        output_files.dedup();
        for hop in self.hops.iter_mut().rev() {
            if let Err(error) = hop.finish() {
                failure.get_or_insert(error);
            }
        }
        Completion {
            result: failure.map_or(Ok(()), Err),
            evidence: Evidence {
                captures: self
                    .hops
                    .iter()
                    .filter_map(|hop| hop.capture_evidence())
                    .collect(),
                output_files,
                cleanup_confirmed: self.hops.iter().all(|hop| hop.cleanup_confirmed()),
            },
        }
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        for hop in self.hops.iter_mut().rev() {
            hop.abort();
        }
    }
}

pub(crate) fn configured_chain(default: &str) -> Result<Vec<String>, Error> {
    let raw = std::env::var("ASB_PROXY_CHAIN").unwrap_or_else(|_| default.into());
    parse_chain(&raw)
}

#[cfg(test)]
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    adapters::REGISTRY.iter().map(|adapter| adapter.name())
}

fn parse_chain(raw: &str) -> Result<Vec<String>, Error> {
    let names = raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Err(Error::Input(
            "ASB_PROXY_CHAIN must contain at least one proxy adapter".into(),
        ));
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingFixture(&'static str);
    impl ProxyAdapter for RecordingFixture {
        fn name(&self) -> &'static str {
            self.0
        }
        fn default_recording(&self) -> bool {
            true
        }
        fn supports_offline_replay(&self) -> bool {
            true
        }
        fn validate_replay_model(&self, _: &std::path::Path, model: &str) -> Result<(), Error> {
            if model == "fixture-model" {
                Ok(())
            } else {
                Err(Error::Input("wrong model".into()))
            }
        }
        fn replay_interface(
            &self,
            _: &std::path::Path,
        ) -> Result<Option<crate::endpoint::Interface>, Error> {
            Ok(Some(crate::endpoint::Interface::OpenAiChat))
        }
        fn recorded_usage(&self, _: &str, _: bool) -> crate::usage::TokenUsage {
            crate::usage::TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                total_tokens: Some(7),
                ..Default::default()
            }
        }
        fn recorded_tool_calls(&self, _: &std::path::Path) -> Option<Vec<tool_calls::ToolCall>> {
            Some(vec![tool_calls::ToolCall {
                ordinal: 1,
                source: "fixture.response",
                kind: "function".into(),
                name: Some("read".into()),
                id: None,
                arguments_present: false,
                response_observed_unix_ms: None,
                execution_duration_ms: None,
                classes: Vec::new(),
            }])
        }
        fn recording_settings(
            &self,
            request: &RecordingRequest<'_>,
        ) -> Result<Box<dyn Any>, Error> {
            Ok(Box::new((
                request.mode,
                request.cassette.to_path_buf(),
                request.respect_record_timings,
            )))
        }
        fn start(
            &self,
            _: Option<&str>,
            _: Option<&dyn Any>,
        ) -> Result<Box<dyn ProxySession>, Error> {
            Ok(Box::new(FixtureSession {
                name: self.0,
                url: format!("{}-url", self.0),
                active: true,
                fail_finish: false,
                events: Arc::new(Mutex::new(Vec::new())),
            }))
        }
    }

    #[test]
    fn selects_recorded_identity_from_multiple_recorders() {
        let first = RecordingFixture("first");
        let second = RecordingFixture("second");
        let registry: &[&dyn ProxyAdapter] = &[&first, &second];
        assert_eq!(find_recorder(registry, "second").unwrap().name(), "second");
        assert!(find_recorder(registry, "missing").is_none());
    }

    #[test]
    fn replay_starts_historical_recorder_from_multi_adapter_registry() {
        let first = RecordingFixture("first");
        let second = RecordingFixture("second");
        let registry: &[&dyn ProxyAdapter] = &[&first, &second];
        let request = RecordingRequest {
            recorder: Some("second"),
            mode: crate::tape::Mode::Replay,
            cassette: std::path::Path::new("fixture"),
            respect_record_timings: false,
            workspace: std::path::Path::new("."),
            wire_extension: None,
        };
        let selected = resolve_recorder(registry, request.recorder).unwrap();
        selected
            .validate_replay_model(request.cassette, "fixture-model")
            .unwrap();
        assert!(
            selected
                .validate_replay_model(request.cassette, "other-model")
                .is_err()
        );
        assert_eq!(
            selected.replay_interface(request.cassette).unwrap(),
            Some(crate::endpoint::Interface::OpenAiChat)
        );
        assert_eq!(
            selected.recorded_usage("other-format", false).total_tokens,
            Some(7)
        );
        assert_eq!(
            selected.recorded_tool_calls(request.cassette).unwrap()[0].source,
            "fixture.response"
        );
        let names = vec!["second".to_owned()];
        let chain =
            start_recording_chain_with(&request, Some("forbidden-online"), &names, registry)
                .unwrap();
        assert_eq!(chain.url(), "second-url");
        chain.finish().result.unwrap();
        let missing = RecordingRequest {
            recorder: Some("missing"),
            ..request
        };
        assert!(start_recording_chain_with(&missing, None, &names, registry).is_err());
    }

    #[test]
    fn recording_chain_requires_recorder_and_offline_replay_hops() {
        let recorder = RecordingFixture("recorder");
        let offline = RecordingFixture("offline");
        let upstream = FixtureAdapter {
            name: "other",
            fail: false,
            fail_finish: false,
            events: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(
            upstream
                .recorded_tool_calls(std::path::Path::new("fixture"))
                .is_none()
        );
        let registry: &[&dyn ProxyAdapter] = &[&recorder, &upstream, &offline];
        let names = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
        for mode in [crate::tape::Mode::Record, crate::tape::Mode::Replay] {
            assert!(
                validate_recording_chain(&names(&["other"]), "recorder", mode, registry).is_err()
            );
            assert!(
                validate_recording_chain(
                    &names(&["recorder", "recorder"]),
                    "recorder",
                    mode,
                    registry
                )
                .is_err()
            );
            assert!(
                validate_recording_chain(&names(&["recorder"]), "recorder", mode, registry).is_ok()
            );
        }
        assert!(
            validate_recording_chain(
                &names(&["recorder", "other"]),
                "recorder",
                crate::tape::Mode::Replay,
                registry
            )
            .is_err()
        );
        assert!(
            validate_recording_chain(
                &names(&["other", "recorder"]),
                "recorder",
                crate::tape::Mode::Replay,
                registry
            )
            .is_err()
        );
        assert!(
            validate_recording_chain(
                &names(&["other", "recorder"]),
                "recorder",
                crate::tape::Mode::Record,
                registry
            )
            .is_ok()
        );
        assert!(
            validate_recording_chain(
                &names(&["offline", "recorder"]),
                "recorder",
                crate::tape::Mode::Replay,
                registry
            )
            .is_ok()
        );
        assert!(
            upstream.events.lock().unwrap().is_empty(),
            "validation must precede proxy startup"
        );
    }

    #[test]
    fn recording_selection_is_adapter_owned_and_unambiguous() {
        let one = RecordingFixture("other-recorder");
        let two = RecordingFixture("second-recorder");
        assert!(select_recording_adapter(&[]).is_err());
        assert!(select_recording_adapter(&[&one, &two]).is_err());
        let registry: &[&dyn ProxyAdapter] = &[&one];
        let selected = select_recording_adapter(registry).unwrap();
        assert_eq!(selected.name(), "other-recorder");
        let cassette = std::path::Path::new("fixture.jsonl");
        let request = RecordingRequest {
            recorder: None,
            mode: crate::tape::Mode::Replay,
            cassette,
            respect_record_timings: true,
            workspace: std::path::Path::new("."),
            wire_extension: None,
        };
        let settings = selected.recording_settings(&request).unwrap();
        let captured = settings
            .downcast_ref::<(crate::tape::Mode, std::path::PathBuf, bool)>()
            .unwrap();
        assert_eq!(
            captured,
            &(crate::tape::Mode::Replay, cassette.to_path_buf(), true)
        );
    }

    struct FixtureAdapter {
        name: &'static str,
        fail: bool,
        fail_finish: bool,
        events: Arc<Mutex<Vec<String>>>,
    }
    struct FixtureSession {
        name: &'static str,
        url: String,
        active: bool,
        fail_finish: bool,
        events: Arc<Mutex<Vec<String>>>,
    }
    impl ProxyAdapter for FixtureAdapter {
        fn name(&self) -> &'static str {
            self.name
        }
        fn start(
            &self,
            upstream: Option<&str>,
            settings: Option<&dyn Any>,
        ) -> Result<Box<dyn ProxySession>, Error> {
            let value = settings
                .and_then(|value| value.downcast_ref::<u32>())
                .copied()
                .unwrap_or(0);
            self.events.lock().unwrap().push(format!(
                "start:{}:{}:{value}",
                self.name,
                upstream.unwrap_or("none")
            ));
            if self.fail {
                return Err(Error::Run("fixture failure".into()));
            }
            Ok(Box::new(FixtureSession {
                name: self.name,
                url: format!("{}-url", self.name),
                active: true,
                fail_finish: self.fail_finish,
                events: self.events.clone(),
            }))
        }
    }
    impl ProxySession for FixtureSession {
        fn cleanup_confirmed(&self) -> bool {
            !self.active
        }
        fn url(&self) -> &str {
            &self.url
        }
        fn finish(&mut self) -> Result<(), Error> {
            self.events
                .lock()
                .unwrap()
                .push(format!("finish:{}", self.name));
            self.active = false;
            if self.fail_finish {
                Err(Error::Run("fixture finish failure".into()))
            } else {
                Ok(())
            }
        }
        fn abort(&mut self) {
            if self.active {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("abort:{}", self.name));
                self.active = false;
            }
        }
    }

    #[test]
    fn rejected_completion_keeps_resource_cleanup_as_a_separate_fact() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FixtureAdapter {
            name: "fixture",
            fail: false,
            fail_finish: true,
            events,
        };
        let completed = Chain::start_selected(&[&adapter], None, &HashMap::new())
            .unwrap()
            .finish();
        assert!(completed.result.is_err());
        assert!(completed.evidence.cleanup_confirmed);
        assert!(completed.evidence.output_files.is_empty());
    }

    #[test]
    fn arbitrary_adapters_use_own_settings_and_next_hop() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let a = FixtureAdapter {
            name: "a",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let b = FixtureAdapter {
            name: "b",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let mut settings: HashMap<String, Box<dyn Any>> = HashMap::new();
        settings.insert("a".into(), Box::new(17_u32));
        settings.insert("b".into(), Box::new(42_u32));
        let chain = Chain::start_selected(&[&a, &b], Some("provider"), &settings).unwrap();
        assert_eq!(chain.url(), "a-url");
        chain.stop().unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                "start:b:provider:42",
                "start:a:b-url:17",
                "finish:b",
                "finish:a"
            ]
        );
    }

    #[test]
    fn startup_failure_aborts_already_started_hops() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let a = FixtureAdapter {
            name: "a",
            fail: true,
            fail_finish: false,
            events: events.clone(),
        };
        let b = FixtureAdapter {
            name: "b",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let result = Chain::start_selected(&[&a, &b], Some("provider"), &HashMap::new());
        assert!(result.is_err());
        assert_eq!(
            *events.lock().unwrap(),
            ["start:b:provider:0", "start:a:b-url:0", "abort:b"]
        );
    }

    #[test]
    fn chain_names_are_normalized_and_empty_chains_rejected() {
        assert_eq!(
            parse_chain("llmtape, audit, REDACT").unwrap(),
            vec!["llmtape", "audit", "redact"]
        );
        assert!(parse_chain(" , ").is_err());
    }

    #[test]
    fn finish_failure_still_finishes_every_hop() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let a = FixtureAdapter {
            name: "a",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let b = FixtureAdapter {
            name: "b",
            fail: false,
            fail_finish: true,
            events: events.clone(),
        };
        let chain = Chain::start_selected(&[&a, &b], None, &HashMap::new()).unwrap();
        assert!(chain.stop().is_err());
        assert_eq!(
            *events.lock().unwrap(),
            ["start:b:none:0", "start:a:b-url:0", "finish:b", "finish:a"]
        );
    }

    #[test]
    fn dropping_chain_aborts_every_hop() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let a = FixtureAdapter {
            name: "a",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let b = FixtureAdapter {
            name: "b",
            fail: false,
            fail_finish: false,
            events: events.clone(),
        };
        let chain = Chain::start_selected(&[&a, &b], None, &HashMap::new()).unwrap();
        drop(chain);
        assert_eq!(
            *events.lock().unwrap(),
            ["start:b:none:0", "start:a:b-url:0", "abort:b", "abort:a"]
        );
    }

    #[test]
    fn unknown_adapter_is_rejected_before_startup() {
        assert!(Chain::start(&["no-such-adapter".into()], None, &HashMap::new()).is_err());
    }
}
