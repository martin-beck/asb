//! Adapter-owned replay artifact collectors.
//!
//! The runner intentionally treats returned values as bounded opaque sidecars:
//! it stages, seals, and digests them but does not know an adapter's wire
//! protocol or artifact paths.  Adapters opt into this extension through
//! `StaticAdapter::with_tool_replay`.

use super::super::{AgentArtifacts, CollectionContext};

/// Publish the ASB local-tool MCP transcript selected by a replay-capable
/// adapter.  This extension owns the filename and transcript meaning; static
/// adapter dispatch only invokes the explicitly selected collector.
pub(crate) fn collect_local_tool_transcript(
    context: &CollectionContext<'_>,
    artifacts: &mut AgentArtifacts,
) {
    artifacts.local_tool_transcript = context
        .frozen
        .bytes("local-tools.jsonl")
        .map(ToOwned::to_owned);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{ArtifactFormat, ArtifactOwner, ArtifactSpec};

    #[test]
    fn selected_extension_collects_only_the_local_tool_sidecar() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-artifacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let run_dir = root.join("run");
        let mut owner = ArtifactOwner::prepare(
            &run_dir,
            &[ArtifactSpec {
                namespace: "tool-replay",
                filename: "local-tools.jsonl",
                format: ArtifactFormat::JsonLines,
            }],
        )
        .unwrap();
        std::fs::write(run_dir.join("tool-replay/local-tools.jsonl"), b"tool\n").unwrap();
        let frozen = owner.freeze();
        let context = CollectionContext {
            run_dir: &run_dir,
            frozen: &frozen,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        };
        let mut artifacts = AgentArtifacts::default();
        collect_local_tool_transcript(&context, &mut artifacts);
        assert_eq!(
            artifacts.local_tool_transcript.as_deref(),
            Some(b"tool\n".as_slice())
        );
        assert!(artifacts.deterministic_manifest.is_none());
        assert!(artifacts.native_trace.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
