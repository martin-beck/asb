//! Registry of proxy implementations.

pub(crate) mod llmtape;
#[cfg(test)]
mod synthetic;

use super::ProxyAdapter;

#[cfg(not(test))]
pub(super) static REGISTRY: &[&dyn ProxyAdapter] = &[&llmtape::ADAPTER];
#[cfg(test)]
pub(super) static REGISTRY: &[&dyn ProxyAdapter] = &[&llmtape::ADAPTER, &synthetic::ADAPTER];

/// Historical untagged cassette tracks were always produced by llmtape.
pub(super) fn legacy_recording_name() -> &'static str {
    llmtape::ADAPTER.name()
}
