//! Shared process isolation for offline fixtures, retaining only instrumentation.

use std::process::Command;

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
        self.env("ASB_INIT_OFFLINE", "1");
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
            assert_eq!(actual.len(), 3);
            assert!(
                actual
                    .iter()
                    .any(|(key, item)| *key == "LLVM_PROFILE_FILE"
                        && *item == Some(value.as_os_str()))
            );
            assert!(
                actual
                    .iter()
                    .any(|(key, value)| *key == "ASB_TEST_AVAILABLE_MIB"
                        && *value == Some("18446744073709551615".as_ref()))
            );
            assert!(
                actual
                    .iter()
                    .any(|(key, value)| *key == "ASB_INIT_OFFLINE" && *value == Some("1".as_ref()))
            );
        }
        None => {
            assert_eq!(actual.len(), 2);
            assert!(
                actual
                    .iter()
                    .any(|(key, value)| *key == "ASB_TEST_AVAILABLE_MIB"
                        && *value == Some("18446744073709551615".as_ref()))
            );
            assert!(
                actual
                    .iter()
                    .any(|(key, value)| *key == "ASB_INIT_OFFLINE" && *value == Some("1".as_ref()))
            );
        }
    }
}
