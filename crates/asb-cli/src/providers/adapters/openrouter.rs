//! Adapter for openrouter.
pub(in crate::providers) mod implementation;
use super::*;
use std::collections::BTreeMap;

const PRICING_FILENAME: &str = "openrouter-pricing.json";

fn pricing_snapshot_from_catalog(
    body: &[u8],
    source: &str,
) -> Result<serde_json::Value, crate::Error> {
    use serde_json::Value;
    let catalog: Value = serde_json::from_slice(body)
        .map_err(|_| crate::Error::Input("OpenRouter returned invalid model JSON".into()))?;
    let entries = catalog
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::Error::Input("OpenRouter model catalog has no model list".into()))?;
    let mut models = serde_json::Map::new();
    for entry in entries {
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            models.insert(
                id.to_owned(),
                entry.get("pricing").cloned().unwrap_or(Value::Null),
            );
        }
    }
    Ok(serde_json::json!({
        "schema_version": 1, "provider": "openrouter", "source": source,
        "captured_unix_seconds": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| crate::Error::Run("system clock precedes Unix epoch".into()))?.as_secs(),
        "pricing_unit": "USD per token", "models": models,
    }))
}

fn cost(
    pricing: &serde_json::Map<String, serde_json::Value>,
    metrics: &BTreeMap<String, f64>,
) -> Option<f64> {
    let rate = |name: &str| {
        pricing.get(name).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    };
    let tokens = |name: &str| metrics.get(name).copied();
    let cached_tokens = tokens("token_usage.cached_tokens");
    let input = tokens("token_usage.input_tokens")
        .map(|input| input - cached_tokens.unwrap_or(0.0))
        .filter(|input| *input >= 0.0)
        .zip(rate("prompt"));
    let output = tokens("token_usage.output_tokens").zip(rate("completion"));
    let cached = cached_tokens.zip(rate("cache_read").or_else(|| rate("cacheRead")));
    let observed = input.is_some() || output.is_some() || cached.is_some();
    let estimate = input
        .into_iter()
        .chain(output)
        .chain(cached)
        .map(|(count, price)| count * price)
        .sum::<f64>();
    (observed && estimate.is_finite()).then_some(estimate)
}

fixed_offers!(
    openrouter_offers,
    Some("OPENROUTER_BASE_URL"),
    "https://openrouter.ai/api/v1",
    "",
    false,
    &[(Chat, Bearer), (Responses, Bearer), (Messages, Bearer)]
);

adapter!(
    "openrouter",
    Some("OPENROUTER_API_KEY"),
    Some("OPENROUTER_BASE_URL"),
    Some("OPENROUTER_MODEL"),
    None,
    &["OPENROUTER_BASE_URL"],
    Some("OPENROUTER_BASE_URL"),
    Some("https://openrouter.ai/api/v1/models"),
    DataUnpaginated,
    "openai/gpt-4o-mini",
    openrouter_offers,
    fn prepare(
        &self,
        command: &mut std::process::Command,
        model: &str,
        _bridge: Option<(&str, &str)>,
        tape: Option<&str>,
        replay: bool,
    ) -> Result<(), crate::Error> {
        implementation::prepare(command, model, tape, replay)
    },
    fn public_catalog(&self, credential_present: bool) -> bool {
        !credential_present
    },
    fn supports_free(&self) -> bool {
        true
    },
    fn free_model(&self, entry: &serde_json::Value, id: &str) -> bool {
        id.ends_with(":free")
            || entry
                .get("pricing")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|pricing| {
                    crate::models::is_zero_price(pricing.get("prompt"))
                        && crate::models::is_zero_price(pricing.get("completion"))
                })
    },
    fn pricing_snapshot(&self) -> Result<Option<(&'static str, serde_json::Value)>, crate::Error> {
        let spec = self.model_spec();
        let url = spec.url_for_model(None).ok_or_else(|| {
            crate::Error::Input("OpenRouter model catalog endpoint is unavailable".into())
        })?;
        let body = crate::models::fetch_pages(spec, &url)?;
        pricing_snapshot_from_catalog(&body, &url)
            .map(|snapshot| Some((PRICING_FILENAME, snapshot)))
    },
    fn pricing_filename(&self) -> Option<&'static str> {
        Some(PRICING_FILENAME)
    },
    fn validate_pricing(&self, snapshot: &serde_json::Value) -> Result<(), crate::Error> {
        if snapshot
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
            || snapshot.get("provider").and_then(serde_json::Value::as_str) != Some("openrouter")
        {
            return Err(crate::Error::Input(
                "invalid OpenRouter pricing snapshot schema".into(),
            ));
        }
        if !snapshot
            .get("models")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(crate::Error::Input(
                "OpenRouter pricing snapshot has no models".into(),
            ));
        }
        Ok(())
    },
    fn estimate_cost(
        &self,
        snapshot: &serde_json::Value,
        model: &str,
        metrics: &BTreeMap<String, f64>,
    ) -> Option<f64> {
        let pricing = snapshot.get("models")?.get(model)?.as_object()?;
        cost(pricing, metrics)
    }
);

#[cfg(test)]
mod tests {
    use std::process::Command;

    #[test]
    fn snapshot_preserves_prices_and_estimates_cached_tokens() {
        let snapshot = super::pricing_snapshot_from_catalog(
            br#"{"data":[{"id":"vendor/one","pricing":{"prompt":"0.000002","completion":"0.000004","cache_read":"0.0000005"}}]}"#,
            "https://example.invalid/models",
        ).unwrap();
        assert_eq!(snapshot["schema_version"], 1);
        assert_eq!(snapshot["provider"], "openrouter");
        let metrics = std::collections::BTreeMap::from([
            ("token_usage.input_tokens".into(), 100.0),
            ("token_usage.output_tokens".into(), 25.0),
            ("token_usage.cached_tokens".into(), 40.0),
        ]);
        let value =
            crate::providers::estimate_cost("openrouter", &snapshot, "vendor/one", &metrics)
                .unwrap();
        assert!((value - 0.00024).abs() < 1e-12);
        assert!(
            crate::providers::estimate_cost("openrouter", &snapshot, "missing", &metrics).is_none()
        );
        assert!(
            crate::providers::estimate_cost(
                "openrouter",
                &snapshot,
                "vendor/one",
                &std::collections::BTreeMap::from([("token_usage.total_tokens".into(), 100.0)])
            )
            .is_none()
        );
    }

    fn values(command: &Command) -> std::collections::HashMap<String, String> {
        command
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn openrouter_baseline_uses_compatible_protocol_endpoints() {
        let mut command = Command::new("bridge-client");
        crate::providers::adapters::openrouter::implementation::configure_openai_compatible(
            &mut command,
            "example/model",
            "or-test",
            "https://openrouter.ai/api/v1",
        );
        let values = values(&command);
        assert_eq!(
            values.get("OPENAI_BASE_URL"),
            Some(&"https://openrouter.ai/api/v1".to_owned())
        );
        assert_eq!(
            values.get("ANTHROPIC_BASE_URL"),
            Some(&"https://openrouter.ai/api".to_owned())
        );
    }

    #[test]
    fn replay_openrouter_does_not_need_a_live_credential() {
        let mut command = Command::new("bridge-client");
        crate::providers::preparation::apply_with_tape(
            &mut command,
            "openrouter",
            "example/model",
            None,
            Some("http://127.0.0.1:4567/v1"),
            true,
        )
        .expect("replay preparation succeeds");
        assert_eq!(
            values(&command).get("OPENROUTER_API_KEY"),
            Some(&"asb-replay".to_owned())
        );
    }
}
