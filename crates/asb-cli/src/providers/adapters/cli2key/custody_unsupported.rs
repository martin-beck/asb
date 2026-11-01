//! Fail before spawning where bounded local output transport is unavailable.
pub(super) struct Local;
impl Local {
    pub(super) fn new() -> Result<Self, String> {
        Err("local cli2key bridge requires Unix; configure CLI2KEY_BASE_URL and CLI2KEY_API_KEY for an external endpoint".into())
    }
    pub(super) fn spawn_bridge(&mut self, _: &mut std::process::Command) -> Result<String, String> {
        Err("local cli2key bridge requires Unix".into())
    }
    pub(super) fn spawn_proxy(
        &mut self,
        _: std::process::Command,
    ) -> Result<std::net::SocketAddrV4, String> {
        Err("local cli2key bridge requires Unix".into())
    }
    pub(super) fn finish(self) -> Result<(), String> {
        Ok(())
    }
    pub(super) fn confirm_ready(&mut self) -> Result<(), String> {
        Err("local cli2key bridge requires Unix".into())
    }
}
