//! Project-owned replay normalizer management. No normalizer is executed here.
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub id: String,
    pub module: String,
    pub enabled: bool,
}

fn read_config(root: &Path) -> Result<String, String> {
    let path = root.join(".asb/project.toml");
    let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return Err("project configuration must be a regular file under 1 MiB".into());
    }
    fs::read_to_string(path).map_err(|e| e.to_string())
}
fn parse(text: &str) -> Result<Vec<Profile>, String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|e| format!("invalid project configuration: {e}"))?;
    let Some(replay) = value.get("replay") else {
        return Ok(vec![]);
    };
    let Some(items) = replay.get("normalizers") else {
        return Ok(vec![]);
    };
    let array = items
        .as_array()
        .ok_or("replay.normalizers must be a table array")?;
    if array.len() > 32 {
        return Err("too many normalizers".into());
    }
    let mut out = Vec::new();
    for item in array {
        let id = item
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or("normalizer has no id")?;
        let module = item
            .get("module")
            .and_then(toml::Value::as_str)
            .ok_or("normalizer has no module")?;
        let enabled = item
            .get("enabled")
            .map(|v| v.as_bool().ok_or("enabled must be boolean"))
            .transpose()?
            .unwrap_or(true);
        if out.iter().any(|p: &Profile| p.id == id) {
            return Err("duplicate normalizer ID".into());
        }
        out.push(Profile {
            id: id.into(),
            module: module.into(),
            enabled,
        });
    }
    Ok(out)
}
pub(crate) fn list(root: &Path) -> Result<Vec<Profile>, String> {
    parse(&read_config(root)?)
}
fn lock(root: &Path) -> Result<fs::File, String> {
    let path = root.join(".asb-project-config.lock");
    if let Ok(m) = fs::symlink_metadata(&path)
        && !m.is_file()
    {
        return Err("project lock is not a regular file".into());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = options.open(path).map_err(|e| e.to_string())?;
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("project lock is not a regular file".into());
    }
    f.try_lock()
        .map_err(|_| "project configuration is in use".to_owned())?;
    Ok(f)
}
fn publish(root: &Path, old: &str, new: &str) -> Result<(), String> {
    parse(new)?;
    if read_config(root)? != old {
        return Err("project configuration changed; reload and try again".into());
    }
    let config = root.join(".asb/project.toml");
    let path = root.join(".asb").join(format!(
        ".normalizer-edit-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        f.set_permissions(
            fs::metadata(&config)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
        f.write_all(new.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| e.to_string())?;
        if read_config(root)? != old {
            return Err("project configuration changed; reload and try again".into());
        }
        fs::rename(&path, &config).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}
// Only edit well-formed, conventional table-array blocks. Refuse ambiguous
// layouts instead of rewriting unrelated user configuration or comments.
fn blocks(text: &str) -> Result<Vec<(usize, usize, String)>, String> {
    let expected = parse(text)?;
    let mut starts = Vec::new();
    let mut headers = Vec::new();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let token = line.split('#').next().unwrap_or("").trim();
        if token.starts_with('[') && token.ends_with(']') {
            headers.push(pos);
        }
        if token == "[[replay.normalizers]]" {
            starts.push(pos);
        }
        pos += line.len();
    }
    if starts.len() != expected.len() {
        return Err(
            "normalizer declarations use an unsupported layout; edit project.toml directly".into(),
        );
    }
    let mut out = Vec::new();
    for (i, start) in starts.iter().enumerate() {
        let end = headers
            .iter()
            .copied()
            .find(|offset| offset > start)
            .unwrap_or(text.len());
        let block = &text[*start..end];
        let parsed = parse(block)?;
        if parsed.len() != 1 || parsed[0].id != expected[i].id {
            return Err("ambiguous normalizer table layout; edit project.toml directly".into());
        }
        out.push((*start, end, parsed[0].id.clone()));
    }
    Ok(out)
}
pub(crate) fn set_enabled(root: &Path, id: &str, enabled: bool) -> Result<(), String> {
    let _guard = lock(root)?;
    let old = read_config(root)?;
    let spans = blocks(&old)?;
    let (start, end, _) = spans
        .iter()
        .find(|(_, _, name)| name == id)
        .ok_or("normalizer no longer exists")?;
    let block = &old[*start..*end];
    let mut lines = block.split_inclusive('\n');
    let header = lines.next().ok_or("missing table header")?;
    let mut rest = String::new();
    let mut found = false;
    for line in lines {
        let key = line.split('#').next().unwrap_or("").trim();
        if key.starts_with("enabled")
            && key
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == "enabled")
        {
            if found {
                return Err("duplicate enabled field".into());
            }
            found = true;
            let comment = line
                .find('#')
                .map(|index| line[index..].trim_end())
                .unwrap_or("");
            rest.push_str(&format!(
                "enabled = {enabled}{}{}\n",
                if comment.is_empty() { "" } else { " " },
                comment
            ));
            continue;
        }
        rest.push_str(line);
    }
    let replacement = if found {
        format!("{header}{rest}")
    } else {
        format!("{header}enabled = {enabled}\n{rest}")
    };
    let new = format!("{}{}{}", &old[..*start], replacement, &old[*end..]);
    publish(root, &old, &new)
}
pub(crate) fn module_path(root: &Path, profile: &Profile) -> Result<PathBuf, String> {
    let relative = Path::new(&profile.module);
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err("module path must stay inside the project".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err("module must be a regular file inside the project".into());
    }
    Ok(path)
}
pub(crate) fn create(
    root: &Path,
    workload: &str,
    examples: Option<(&str, &str)>,
) -> Result<String, String> {
    if workload.is_empty() || workload.len() > 256 {
        return Err("select a workload first".into());
    }
    if examples
        .is_some_and(|(expected, actual)| expected.len() > 16 * 1024 || actual.len() > 16 * 1024)
    {
        return Err("selected text exceeds 16 KiB".into());
    }
    let _guard = lock(root)?;
    let old = read_config(root)?;
    let profiles = parse(&old)?;
    if profiles.len() >= 32 {
        return Err("project already has 32 normalizers".into());
    }
    let dir = root.join("normalizers");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let canonical_root = root.canonicalize().map_err(|e| e.to_string())?;
    if !dir
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(&canonical_root)
    {
        return Err("normalizer directory resolves outside the project".into());
    }
    let (id, path) = (1..=9999)
        .find_map(|n| {
            let id = format!("debugger-suggestion-{n}");
            let path = dir.join(format!("{id}.mjs"));
            (!profiles.iter().any(|p| p.id == id) && !path.exists()).then_some((id, path))
        })
        .ok_or("cannot allocate a normalizer name")?;
    let code = if let Some((expected, actual)) = examples {
        let expected = serde_json::to_string(expected).map_err(|e| e.to_string())?;
        let actual = serde_json::to_string(actual).map_err(|e| e.to_string())?;
        format!(
            "// Exact observed examples only. Review and refine before enabling.\nexport function normalizeToolResult({{text}}) {{\n  if (text === {actual} || text === {expected}) return {expected};\n  return text;\n}}\n"
        )
    } else {
        "// Edit this function, then enable the normalizer.\nexport function normalizeToolResult({text, workload, tool}) {\n  return text;\n}\n".into()
    };
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    if let Err(e) = f.write_all(code.as_bytes()).and_then(|()| f.sync_all()) {
        let _ = fs::remove_file(&path);
        return Err(e.to_string());
    }
    let workload = toml::Value::String(workload.into()).to_string();
    let new = format!(
        "{}\n[[replay.normalizers]]\nid = \"{id}\"\nmodule = \"normalizers/{id}.mjs\"\nenabled = false\nworkloads = [{workload}]\n",
        old.trim_end()
    );
    if let Err(e) = publish(root, &old, &new) {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(id)
}

pub(crate) fn find_project(start: &Path) -> Option<PathBuf> {
    let path = start.canonicalize().ok()?;
    path.ancestors()
        .find(|p| p.join(".asb/project.toml").is_file())
        .map(Path::to_owned)
}

pub(crate) fn workload_from_evidence(path: &Path) -> Option<String> {
    for parent in path.ancestors() {
        let file = parent.join("plan.json");
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        if text.len() > 4 * 1024 * 1024 {
            return None;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(value) = value
            .pointer("/configuration/workload")
            .and_then(serde_json::Value::as_str)
        {
            return Some(value.to_owned());
        }
    }
    None
}

pub(crate) fn run(
    root: &Path,
    initial_workload: Option<String>,
    examples: Option<(String, String)>,
) -> Result<(), String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use ratatui::{
        layout::{Constraint, Layout},
        style::{Color, Style},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    };
    let (mut guard, mut terminal) = crate::app::start_terminal().map_err(|e| e.to_string())?;
    let mut profiles = list(root)?;
    let mut selected = 0usize;
    let mut workload = initial_workload.unwrap_or_default();
    let mut entering = false;
    let mut message = String::new();
    loop {
        terminal.draw(|frame| {
            let areas=Layout::vertical([Constraint::Length(3),Constraint::Min(4),Constraint::Length(4)]).split(frame.area());
            frame.render_widget(Paragraph::new(format!("Project: {}   Workload: {}{}",root.display(),if workload.is_empty(){"<select with w>"}else{&workload},if entering{"_"}else{""})).block(Block::default().borders(Borders::ALL).title("Replay normalizers")),areas[0]);
            let items=profiles.iter().map(|p|ListItem::new(format!("[{}] {}  {}",if p.enabled{"on"}else{"off"},crate::diff::visible(&p.id),crate::diff::visible(&p.module)))).collect::<Vec<_>>();
            let mut state=ListState::default().with_selected((!profiles.is_empty()).then_some(selected));
            frame.render_stateful_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Profiles")).highlight_style(Style::default().fg(Color::Cyan)).highlight_symbol("> "),areas[1],&mut state);
            frame.render_widget(Paragraph::new(format!("Up/Down select  Space enable/disable  e edit  E settings  a add  n from evidence  w workload  q back\n{message}")).block(Block::default().borders(Borders::ALL)),areas[2]);
        }).map_err(|e|e.to_string())?;
        if !event::poll(std::time::Duration::from_millis(200)).map_err(|e| e.to_string())? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if entering {
            match key.code {
                KeyCode::Enter => entering = false,
                KeyCode::Esc => {
                    entering = false;
                }
                KeyCode::Backspace => {
                    workload.pop();
                }
                KeyCode::Char(c) if workload.len() < 256 => workload.push(c),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(profiles.len().saturating_sub(1))
            }
            KeyCode::Char('w') => {
                workload.clear();
                entering = true;
                message = "Enter workload name, then press Enter".into()
            }
            KeyCode::Char(' ') => {
                if let Some(p) = profiles.get(selected) {
                    message = match set_enabled(root, &p.id, !p.enabled) {
                        Ok(()) => "Updated. Future replays use the new setting.".into(),
                        Err(e) => e,
                    };
                    profiles = list(root)?
                }
            }
            KeyCode::Char('a') | KeyCode::Char('n') => {
                let proposal = if key.code == KeyCode::Char('n') {
                    match examples.as_ref() {
                        Some((a, b)) => Some((a.as_str(), b.as_str())),
                        None => {
                            message = "Select a changed text field in a mismatch first".into();
                            continue;
                        }
                    }
                } else {
                    None
                };
                message = match create(root, &workload, proposal) {
                    Ok(id) => format!(
                        "Created {id} disabled. Press e to review it, then Space to enable."
                    ),
                    Err(e) => e,
                };
                profiles = list(root)?;
                selected = profiles.len().saturating_sub(1);
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                let path = if key.code == KeyCode::Char('E') {
                    Ok(root.join(".asb/project.toml"))
                } else {
                    profiles
                        .get(selected)
                        .ok_or_else(|| "Select a normalizer first".to_owned())
                        .and_then(|profile| module_path(root, profile))
                };
                match path {
                    Ok(path) => {
                        drop(terminal);
                        drop(guard);
                        let editor = std::env::var("VISUAL")
                            .ok()
                            .or_else(|| std::env::var("EDITOR").ok())
                            .unwrap_or_else(|| "vi".into());
                        let mut words = editor.split_whitespace();
                        let result = words
                            .next()
                            .ok_or_else(|| "VISUAL/EDITOR is empty".to_owned())
                            .and_then(|exe| {
                                std::process::Command::new(exe)
                                    .args(words)
                                    .arg(path)
                                    .status()
                                    .map_err(|e| e.to_string())
                            })
                            .and_then(|status| {
                                if status.success() {
                                    Ok(())
                                } else {
                                    Err(format!("editor exited with {status}"))
                                }
                            });
                        (guard, terminal) =
                            crate::app::start_terminal().map_err(|e| e.to_string())?;
                        message = result.err().unwrap_or_else(|| {
                            "Editor closed; settings apply to future replays.".into()
                        });
                        match list(root) {
                            Ok(updated) => profiles = updated,
                            Err(error) => message = error,
                        }
                        selected = selected.min(profiles.len().saturating_sub(1));
                    }
                    Err(error) => message = error,
                }
            }
            _ => {}
        }
    }
}

/// Pick only explicit tool-result text from a selected recorded exchange.
pub(crate) fn tool_output_example(event: &serde_json::Value) -> Option<String> {
    let body = event.pointer("/request/body")?;
    let parsed = if let Some(text) = body.as_str() {
        serde_json::from_str::<serde_json::Value>(text).ok()?
    } else {
        body.clone()
    };
    if let Some(items) = parsed.get("input").and_then(serde_json::Value::as_array) {
        for item in items.iter().rev() {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
                && let Some(text) = item.get("output").and_then(serde_json::Value::as_str)
            {
                return Some(text.to_owned());
            }
        }
    }
    if let Some(items) = parsed.get("messages").and_then(serde_json::Value::as_array) {
        for item in items.iter().rev() {
            if item.get("role").and_then(serde_json::Value::as_str) == Some("tool")
                && let Some(text) = item.get("content").and_then(serde_json::Value::as_str)
            {
                return Some(text.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    #[test]
    fn toggles_only_selected_profile_and_preserves_other_tables_and_comments() {
        let f = Fixture::new();
        let config=f.write(".asb/project.toml", "# project\nname = \"x\"\n\n[[replay.normalizers]]\nid = \"one\"\nmodule = \"normalizers/one.mjs\"\nworkloads = [\"bug-fix\"]\n# keep me\n\n[other]\nenabled = true # unrelated\n");
        set_enabled(&f.path, "one", false).unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("enabled = false\n"));
        assert!(text.contains("[other]\nenabled = true # unrelated"));
        assert!(text.contains("# keep me"));
        assert!(!list(&f.path).unwrap()[0].enabled);
        set_enabled(&f.path, "one", true).unwrap();
        assert!(list(&f.path).unwrap()[0].enabled);
    }
    #[test]
    fn creation_is_exact_scoped_disabled_and_never_overwrites() {
        let f = Fixture::new();
        f.write(".asb/project.toml", "# original\nname = \"x\"\n");
        let id = create(
            &f.path,
            "swe-bench",
            Some(("expected\nseed=1", "expected\nseed=2")),
        )
        .unwrap();
        let entry = list(&f.path).unwrap().remove(0);
        assert_eq!(entry.id, id);
        assert!(!entry.enabled);
        let code = fs::read_to_string(module_path(&f.path, &entry).unwrap()).unwrap();
        assert!(code.contains("text === \"expected\\nseed=1\""));
        assert!(code.contains("text === \"expected\\nseed=2\""));
        assert!(
            fs::read_to_string(f.path.join(".asb/project.toml"))
                .unwrap()
                .contains("# original")
        );
        assert_ne!(create(&f.path, "swe-bench", Some(("a", "b"))).unwrap(), id);
    }
}
