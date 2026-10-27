//! Shared process isolation for offline fixtures, retaining only instrumentation.

use std::process::Command;

/// Keep Cargo's already-installed toolchain available to a fixture whose
/// private HOME deliberately excludes all ambient application state.  Rustup
/// otherwise treats that private HOME as a new installation and may try to
/// bootstrap a toolchain while a workload only needs to probe `cargo`.
pub fn retain_host_rust_toolchain(command: &mut Command) -> &mut Command {
    let host_home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let rustup_home = std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        std::path::PathBuf::from(&host_home)
            .join(".rustup")
            .into_os_string()
    });
    let cargo_home = std::env::var_os("CARGO_HOME").unwrap_or_else(|| {
        std::path::PathBuf::from(&host_home)
            .join(".cargo")
            .into_os_string()
    });
    command
        .env("RUSTUP_HOME", rustup_home)
        .env("CARGO_HOME", cargo_home)
}

/// Clear inherited credentials/state without losing subprocess LLVM coverage.
pub trait IsolatedEnvironment {
    fn isolated_env(&mut self) -> &mut Self;
}

impl IsolatedEnvironment for Command {
    fn isolated_env(&mut self) -> &mut Self {
        self.env_clear();
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            self.env("LLVM_PROFILE_FILE", profile);
        }
        // Integration fixtures launch the separately compiled debug CLI. Its
        // capacity premise is explicit and cannot exist in release builds.
        self.env("ASB_TEST_AVAILABLE_MIB", "18446744073709551615");
        self
    }
}

#[test]
fn isolated_environment_preserves_only_coverage_instrumentation() {
    let mut command = Command::new("fixture-not-executed");
    command.env("OPENROUTER_API_KEY", "must-not-be-inherited");
    command.env("ASB_GUEST_AGENT_REQUEST", "must-not-be-inherited");
    command.isolated_env();
    let actual = command.get_envs().collect::<Vec<_>>();
    let expected = std::env::var_os("LLVM_PROFILE_FILE");
    match expected.as_ref() {
        Some(value) => {
            assert_eq!(actual.len(), 2);
            assert_eq!(actual[0].0, "LLVM_PROFILE_FILE");
            assert_eq!(actual[0].1, Some(value.as_os_str()));
            assert_eq!(actual[1].0, "ASB_TEST_AVAILABLE_MIB");
            assert_eq!(actual[1].1, Some("18446744073709551615".as_ref()));
        }
        None => {
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].0, "ASB_TEST_AVAILABLE_MIB");
            assert_eq!(actual[0].1, Some("18446744073709551615".as_ref()));
        }
    }
}
