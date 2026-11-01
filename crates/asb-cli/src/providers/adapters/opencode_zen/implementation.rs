//! OpenCode Zen endpoint metadata discovery.
use crate::Error;
use std::process::Command;
const MAX_ENDPOINT_CATALOG: usize = 8 * 1024 * 1024;

/// Public OpenCode-maintained metadata; never send provider credentials here.
pub(in crate::providers) fn fetch_endpoint_catalog() -> Result<Vec<u8>, Error> {
    let catalog_url = std::env::var("ASB_ENDPOINT_CATALOG_URL")
        .unwrap_or_else(|_| "https://models.dev/api.json".into());
    let mut command = Command::new("curl");
    command.current_dir(crate::project::network_invocation_dir());
    crate::project::apply_network_proxy(&mut command, std::path::Path::new("."))?;
    let output = command
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "3",
            "--max-time",
            "8",
            "--max-filesize",
            "8388608",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            &catalog_url,
        ])
        .output()
        .map_err(|error| Error::Io(format!("cannot retrieve endpoint metadata: {error}")))?;
    if !output.status.success() || output.stdout.len() > MAX_ENDPOINT_CATALOG {
        return Err(Error::Input(
            "current endpoint metadata is unavailable or exceeds 8 MiB".into(),
        ));
    }
    Ok(output.stdout)
}

pub(in crate::providers) fn catalog_tool_support(body: &[u8], model: &str) -> Option<bool> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("opencode")?
        .get("models")?
        .get(model)?
        .get("tool_call")?
        .as_bool()
}

pub(in crate::providers) fn catalog_offer(
    body: &[u8],
    model: &str,
) -> Option<crate::endpoint::Offer> {
    use crate::endpoint::{Address, Authentication, Interface, Offer};
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let provider = value.get("opencode")?;
    let item = provider.get("models")?.get(model)?;
    let metadata = item.get("provider");
    let package = metadata
        .and_then(|m| m.get("npm"))
        .or_else(|| provider.get("npm"))?
        .as_str()?;
    let api = metadata
        .and_then(|m| m.get("api"))
        .or_else(|| provider.get("api"))?
        .as_str()?;
    // Catalog data must never redirect credentials to an unrelated host.
    if !api.starts_with("https://opencode.ai/") || api.contains(['?', '#', '\\', '@']) {
        return None;
    }
    let (interface, authentication) = match package {
        "@ai-sdk/openai" => (Interface::OpenAiResponses, Authentication::Bearer),
        "@ai-sdk/openai-compatible" => (Interface::OpenAiChat, Authentication::Bearer),
        "@ai-sdk/anthropic" => (Interface::AnthropicMessages, Authentication::ApiKey),
        "@ai-sdk/google" => (Interface::Gemini, Authentication::ApiKey),
        _ => return None,
    };
    Some(Offer {
        interface,
        authentication,
        address: Address::Discovered {
            environment: Some("OPENCODE_ZEN_BASE_URL"),
            default: api.trim_end_matches('/').to_owned(),
        },
    })
}

#[cfg(test)]
#[path = "../../../../tests/unit/zen_endpoint_metadata.rs"]
mod transport_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn opencode_catalog_reads_explicit_model_tool_support() {
        let catalog = br#"{"opencode":{"models":{"yes":{"tool_call":true},"no":{"tool_call":false},"unknown":{}}}}"#;
        assert_eq!(super::catalog_tool_support(catalog, "yes"), Some(true));
        assert_eq!(super::catalog_tool_support(catalog, "no"), Some(false));
        assert_eq!(super::catalog_tool_support(catalog, "unknown"), None);
    }

    #[test]
    fn opencode_model_catalog_selects_current_interface_and_address() {
        use crate::endpoint::{Address, Authentication, Interface};
        let catalog = br#"{"opencode":{"api":"https://opencode.ai/inference/openai/v1","npm":"@ai-sdk/openai-compatible","models":{"new-gpt":{"provider":{"npm":"@ai-sdk/openai","api":"https://opencode.ai/inference/openai/v2"}},"new-claude":{"provider":{"npm":"@ai-sdk/anthropic","api":"https://opencode.ai/inference/anthropic/v1"}},"new-chat":{},"bad":{"provider":{"npm":"@ai-sdk/openai","api":"https://opencode.ai.evil.test/v1"}}}}}"#;
        let gpt = super::catalog_offer(catalog, "new-gpt").unwrap();
        assert_eq!(gpt.interface, Interface::OpenAiResponses);
        assert_eq!(gpt.authentication, Authentication::Bearer);
        assert_eq!(
            gpt.address,
            Address::Discovered {
                environment: Some("OPENCODE_ZEN_BASE_URL"),
                default: "https://opencode.ai/inference/openai/v2".into()
            }
        );
        assert_eq!(
            super::catalog_offer(catalog, "new-claude")
                .unwrap()
                .interface,
            Interface::AnthropicMessages
        );
        assert_eq!(
            super::catalog_offer(catalog, "new-chat").unwrap().interface,
            Interface::OpenAiChat
        );
        assert!(super::catalog_offer(catalog, "bad").is_none());
        assert!(super::catalog_offer(catalog, "not-listed").is_none());
        assert!(super::catalog_offer(b"invalid", "new-gpt").is_none());
    }
}
