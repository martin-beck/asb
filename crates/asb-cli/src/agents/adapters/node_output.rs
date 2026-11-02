//! Preserve final Node CLI output when a client exits immediately after writing.

use super::PreparationContext;
use crate::Error;
use std::ffi::{OsStr, OsString};
use std::path::Path;

const HOOK: &str = r#"const path = require('node:path');
const entryPath = (process.argv[1] || '').replaceAll('\\', '/');
const entry = path.basename(entryPath);
if (__ASB_AGENT_MATCH__) {
  for (const stream of [process.stdout, process.stderr]) {
    if (!stream.isTTY && typeof stream._handle?.setBlocking === 'function') {
      stream._handle.setBlocking(true);
    }
  }
}
"#;

pub(super) fn prepare(
    context: &mut PreparationContext<'_>,
    agent: &str,
    predicate: &str,
) -> Result<(), Error> {
    let directory = context.runtime_dir.join(agent);
    std::fs::create_dir_all(&directory)
        .map_err(|error| Error::Run(format!("cannot create {agent} runtime directory: {error}")))?;
    let hook = directory.join("flush-output.cjs");
    std::fs::write(&hook, HOOK.replace("__ASB_AGENT_MATCH__", predicate))
        .map_err(|error| Error::Run(format!("cannot prepare {agent} output flush: {error}")))?;
    let inherited = context
        .command
        .get_envs()
        .find(|(key, _)| *key == "NODE_OPTIONS")
        .map(|(_, value)| value.map(OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os("NODE_OPTIONS"));
    context
        .command
        .env("NODE_OPTIONS", options(inherited.as_deref(), &hook)?);
    Ok(())
}

fn options(existing: Option<&OsStr>, hook: &Path) -> Result<OsString, Error> {
    let path = hook
        .to_str()
        .ok_or_else(|| Error::Run("Node output flush path is not valid UTF-8".into()))?;
    let quoted = path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut result = existing.unwrap_or_default().to_os_string();
    if !result.is_empty() {
        result.push(" ");
    }
    result.push(format!("--require=\"{quoted}\""));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_node_entry_flushes_large_output_at_immediate_exit() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "asb node flush {}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hook = root.join("flush-output.cjs");
        for (entry, predicate) in [
            ("qwen", super::super::qwen_code::OUTPUT_ENTRY),
            ("codebuddy", super::super::workbuddy::OUTPUT_ENTRY),
        ] {
            std::fs::write(&hook, HOOK.replace("__ASB_AGENT_MATCH__", predicate)).unwrap();
            let agent = root.join(entry);
            std::fs::write(
                &agent,
                "process.stdout.write('x'.repeat(1048576)); process.exit(0);",
            )
            .unwrap();
            let output = std::process::Command::new("node")
                .arg(agent)
                .env(
                    "NODE_OPTIONS",
                    options(Some(OsStr::new("--no-warnings")), &hook).unwrap(),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout.len(), 1048576);
            assert!(output.stdout.iter().all(|byte| *byte == b'x'));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
