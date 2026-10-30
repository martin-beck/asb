//! Project-local static report server.

use super::{StaticTool, ToolKind};
use crate::install::Recipe;

/// `http-server` is an established, small static-file server. ASB's report
/// lifecycle wrapper supplies the per-project allowlist and idle expiry.
pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "http",
    Recipe::Npm {
        package: "http-server@14.1.1",
        executable: "http-server",
        manifest: None,
    },
    ToolKind::Support,
);
