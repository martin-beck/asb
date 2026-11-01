//! llmtape replay request model and wire-interface discovery.
use crate::Error;
use serde_json::Value;
use std::path::Path;

/// Validate replay without contacting a provider: the cassette must contain a
/// request for the selected model.  Tool usage is an adapter/runtime property,
/// not a requirement of every valid provider recording.
pub(crate) fn ensure_replay_model_recorded(cassette: &Path, model: &str) -> Result<(), Error> {
    let text = crate::models::read_replay_cassette(cassette)?;
    for line in text.lines() {
        let value: Value = serde_json::from_str(line)
            .map_err(|_| Error::Input("replay cassette contains invalid JSONL".into()))?;
        let raw_body = value
            .get("request")
            .and_then(|request| request.get("body"))
            .unwrap_or(&value);
        let body = if let Some(text) = raw_body.as_str() {
            serde_json::from_str::<Value>(text).map_err(|_| {
                Error::Input(
                    "replay cassette contains a request body that is not valid JSON".into(),
                )
            })?
        } else {
            raw_body.clone()
        };
        let Some(recorded_model) = body.get("model").and_then(Value::as_str) else {
            continue;
        };
        if !models_match(recorded_model, model) {
            continue;
        }
        return Ok(());
    }
    Err(Error::Input(format!(
        "replay cassette has no request for model `{model}`; record the same configuration before replay"
    )))
}

/// Infer the recorded wire interface without contacting any provider.
pub(crate) fn replay_interface(
    cassette: &Path,
) -> Result<Option<crate::endpoint::Interface>, Error> {
    let text = crate::models::read_replay_cassette(cassette)?;
    for line in text.lines() {
        let value: Value = serde_json::from_str(line)
            .map_err(|_| Error::Input("replay cassette contains invalid JSONL".into()))?;
        let Some(path) = value
            .get("request")
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let path = path.split('?').next().unwrap_or(path);
        let interface = if path.ends_with("/chat/completions") {
            Some(crate::endpoint::Interface::OpenAiChat)
        } else if path.ends_with("/responses") {
            Some(crate::endpoint::Interface::OpenAiResponses)
        } else if path.ends_with("/messages") {
            Some(crate::endpoint::Interface::AnthropicMessages)
        } else {
            None
        };
        if interface.is_some() {
            return Ok(interface);
        }
    }
    Ok(None)
}

fn models_match(recorded: &str, configured: &str) -> bool {
    recorded == configured
        || recorded.ends_with(&format!("/{configured}"))
        || configured.ends_with(&format!("/{recorded}"))
}
