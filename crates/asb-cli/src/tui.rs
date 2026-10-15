//! Interactive, modular setup wizard built with Ratatui.

use crate::benchmark::{BenchmarkConfig, MeasurementConfig};
use crate::{Error, measurements, models, project, providers};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const AGENTS: &[&str] = &[
    "codex",
    "pi",
    "opencode",
    "codeagent",
    "workbuddy",
    "gemini",
    "qwen-code",
    "aider",
    "mini-swe",
    "openhands",
    "openjiuwen",
];

const DESCRIPTIONS: &[&str] = &[
    "Choose one or more agents that will execute each benchmark.",
    "Choose the provider endpoint used by the selected agents.",
    "Choose tool-capable models available in the selected providers.",
    "Choose one or more workload prompts installed in this project.",
    "Choose the measurements written for every generated benchmark.",
];

#[derive(Clone, Debug)]
struct Choice {
    label: String,
    provider: Option<String>,
    model: Option<String>,
}

#[derive(Clone, Debug)]
struct Pane {
    choices: Vec<Choice>,
    selected: Vec<bool>,
    cursor: usize,
    search: String,
}

impl Pane {
    fn new(choices: Vec<Choice>) -> Self {
        Self {
            selected: vec![false; choices.len()],
            choices,
            cursor: 0,
            search: String::new(),
        }
    }

    fn visible(&self) -> Vec<usize> {
        let query = self.search.to_ascii_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                query.is_empty().then_some(index).or_else(|| {
                    choice
                        .label
                        .to_ascii_lowercase()
                        .contains(&query)
                        .then_some(index)
                })
            })
            .collect()
    }

    fn move_cursor(&mut self, delta: i32) {
        let visible = self.visible();
        if visible.is_empty() {
            self.cursor = 0;
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.cursor)
            .unwrap_or(0);
        let next = if delta.is_negative() {
            position.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            position
                .saturating_add(delta as usize)
                .min(visible.len() - 1)
        };
        self.cursor = visible[next];
    }

    fn toggle(&mut self) {
        self.search_changed();
        if let Some(value) = self.selected.get_mut(self.cursor) {
            *value = !*value;
        }
    }

    fn search_changed(&mut self) {
        let visible = self.visible();
        if let Some(first) = visible.first() {
            if !visible.contains(&self.cursor) {
                self.cursor = *first;
            }
        } else {
            self.cursor = 0;
        }
    }

    fn selected_choices(&self) -> impl Iterator<Item = &Choice> {
        self.choices
            .iter()
            .zip(self.selected.iter())
            .filter_map(|(choice, selected)| selected.then_some(choice))
    }

    fn has_selection(&self) -> bool {
        self.selected.iter().any(|selected| *selected)
    }
}

struct App {
    root: PathBuf,
    project: project::ProjectConfig,
    panes: Vec<Pane>,
    step: usize,
    message: String,
}

impl App {
    fn new(root: PathBuf, project: project::ProjectConfig) -> Self {
        let agents = AGENTS
            .iter()
            .map(|name| Choice {
                label: (*name).into(),
                provider: None,
                model: None,
            })
            .collect();
        let provider_choices = providers::names()
            .iter()
            .copied()
            .chain(std::iter::once("local"))
            .map(|name| Choice {
                label: name.into(),
                provider: None,
                model: None,
            })
            .collect();
        let workloads = workload_choices(&root, &project.workloads_dir);
        let measures = measurements::available_collectors()
            .into_iter()
            .map(|name| Choice {
                label: name,
                provider: None,
                model: None,
            })
            .collect();
        let mut measure_pane = Pane::new(measures);
        measure_pane.selected.fill(true);
        Self {
            root,
            project,
            panes: vec![
                Pane::new(agents),
                Pane::new(provider_choices),
                Pane::new(Vec::new()),
                Pane::new(workloads),
                measure_pane,
            ],
            step: 0,
            message: "Space selects; arrows move; Tab or Enter advances; Esc cancels.".into(),
        }
    }

    fn refresh_models(&mut self) {
        let selected_providers = self.panes[1]
            .selected_choices()
            .map(|choice| choice.label.clone())
            .collect::<Vec<_>>();
        let mut choices = Vec::new();
        for provider in selected_providers {
            let discovered = models::discover_for_setup(&provider).unwrap_or_default();
            let models = if discovered.is_empty() {
                providers::known_models(&provider)
                    .map(|values| values.iter().map(|value| (*value).to_owned()).collect())
                    .unwrap_or_else(|| vec![default_model(&provider).to_owned()])
            } else {
                discovered
            };
            for model in models {
                choices.push(Choice {
                    label: format!("{provider}:{model}"),
                    provider: Some(provider.clone()),
                    model: Some(model),
                });
            }
        }
        self.panes[2] = Pane::new(choices);
    }

    fn advance(&mut self) -> Result<Option<Generated>, Error> {
        if !self.panes[self.step].has_selection() {
            self.message = "Select at least one item before continuing.".into();
            return Ok(None);
        }
        if self.step == 1 {
            self.refresh_models();
        }
        if self.step + 1 < self.panes.len() {
            self.step += 1;
            self.message = DESCRIPTIONS[self.step].into();
            Ok(None)
        } else {
            generate_configs(self).map(Some)
        }
    }

    fn back(&mut self) {
        if self.step > 0 {
            self.step -= 1;
            self.message = DESCRIPTIONS[self.step].into();
        }
    }
}

struct Generated {
    tests: usize,
    sweep: PathBuf,
}

/// Launch the setup wizard and write one TOML per selected combination.
pub fn setup(args: &[String], output: &mut dyn Write) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(Error::Usage("setup tui does not accept options"));
    }
    let (root, project) = project::load(Path::new("."))?;
    let mut app = App::new(root, project);
    let mut terminal =
        start_terminal().map_err(|error| Error::Io(format!("cannot start setup TUI: {error}")))?;
    let result = run_loop(&mut terminal, &mut app);
    let cleanup = stop_terminal(terminal);
    if let Err(error) = cleanup {
        if result.is_ok() {
            return Err(Error::Io(format!("cannot restore terminal: {error}")));
        }
    }
    let generated = result?;
    writeln!(
        output,
        "Generated {} benchmark configurations and sweep {}",
        generated.tests,
        generated.sweep.display()
    )
    .map_err(crate::io_error)?;
    output.flush().map_err(crate::io_error)
}

fn start_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, io::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn stop_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<(), io::Error> {
    terminal.clear()?;
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal.show_cursor()
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<Generated, Error> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|error| Error::Io(format!("cannot draw setup TUI: {error}")))?;
        if !event::poll(std::time::Duration::from_millis(250))
            .map_err(|error| Error::Io(format!("cannot read setup TUI input: {error}")))?
        {
            continue;
        }
        let Event::Key(key) = event::read()
            .map_err(|error| Error::Io(format!("cannot read setup TUI input: {error}")))?
        else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                return Err(Error::Usage("setup TUI cancelled"));
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(Error::Usage("setup TUI cancelled"));
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::SHIFT) => {
                app.back();
            }
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => app.back(),
            KeyEvent {
                code: KeyCode::Tab | KeyCode::Enter,
                ..
            } => {
                if let Some(generated) = app.advance()? {
                    return Ok(generated);
                }
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } => app.panes[app.step].toggle(),
            KeyEvent {
                code: KeyCode::Up, ..
            } => app.panes[app.step].move_cursor(-1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => app.panes[app.step].move_cursor(1),
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => app.panes[app.step].move_cursor(-8),
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } => app.panes[app.step].move_cursor(8),
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => app.panes[app.step].cursor = 0,
            KeyEvent {
                code: KeyCode::End, ..
            } => {
                if let Some(last) = app.panes[app.step].choices.len().checked_sub(1) {
                    app.panes[app.step].cursor = last;
                }
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                app.panes[app.step].search.pop();
                app.panes[app.step].search_changed();
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
            {
                app.panes[app.step].search.push(ch);
                app.panes[app.step].search_changed();
            }
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let steps = ["agent", "provider", "model", "workload", "measures"];
    let step_line = steps
        .iter()
        .enumerate()
        .flat_map(|(index, name)| {
            let style = if index == app.step {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < app.step {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            [
                Span::styled(format!(" {name} "), style),
                Span::raw(if index + 1 == steps.len() {
                    ""
                } else {
                    " ─ "
                }),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(step_line))
            .block(Block::default().borders(Borders::ALL).title("ASB setup")),
        vertical[0],
    );
    frame.render_widget(
        Paragraph::new(DESCRIPTIONS[app.step])
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Step")),
        vertical[1],
    );
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(vertical[2]);
    let pane = &app.panes[app.step];
    let search = if pane.search.is_empty() {
        String::new()
    } else {
        format!("search: {}", pane.search)
    };
    frame.render_widget(
        Paragraph::new(search).alignment(ratatui::layout::Alignment::Right),
        body[0],
    );
    let visible = pane.visible();
    let items = visible
        .iter()
        .map(|index| {
            let choice = &pane.choices[*index];
            let marker = if pane.selected[*index] { "[x]" } else { "[ ]" };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{marker} ")),
                Span::raw(choice.label.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    let selected = visible
        .iter()
        .position(|index| *index == pane.cursor)
        .unwrap_or(0);
    let mut state = ListState::default();
    state.select((!visible.is_empty()).then_some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Available choices"),
            )
            .highlight_style(Style::default().bg(Color::Blue)),
        body[1],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(Text::from(Line::from(vec![
            Span::styled(app.message.clone(), Style::default().fg(Color::Yellow)),
            Span::raw("  Space select  ↑/↓ move  Tab/Enter next  Shift-Tab back  Esc cancel"),
        ]))),
        vertical[3],
    );
}

fn workload_choices(root: &Path, directory: &str) -> Vec<Choice> {
    let path = project::resolve(root, directory);
    let mut names = fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    if names.is_empty() {
        names.extend(
            [
                "bug-fix",
                "build-repair",
                "dependency-migration",
                "feature-addition",
                "refactoring",
                "repository-navigation",
                "test-generation",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }
    names
        .into_iter()
        .map(|label| Choice {
            label,
            provider: None,
            model: None,
        })
        .collect()
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "openai" => "gpt-4o-mini",
        "openrouter" => "openai/gpt-4o-mini",
        "opencode-zen" => "mimo-v2.6-flash-free",
        "anthropic" => "claude-3-5-sonnet-latest",
        "gemini" => "gemini-2.0-flash",
        "ollama" => "llama3.2",
        "groq" => "llama-3.3-70b-versatile",
        "mistral" => "mistral-small-latest",
        "deepseek" => "deepseek-chat",
        "xai" => "grok-3-mini",
        "together" => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "fireworks" => "accounts/fireworks/models/llama-v3p1-8b-instruct",
        "cli2key" => "gpt-6.1-sol",
        _ => "test-model",
    }
}

#[derive(Serialize)]
struct SweepFile {
    configs: Vec<String>,
}

fn generate_configs(app: &App) -> Result<Generated, Error> {
    let agents = app.panes[0]
        .selected_choices()
        .map(|choice| choice.label.clone())
        .collect::<Vec<_>>();
    let providers = app.panes[1]
        .selected_choices()
        .map(|choice| choice.label.clone())
        .collect::<Vec<_>>();
    let models = app.panes[2]
        .selected_choices()
        .filter_map(|choice| Some((choice.provider.clone()?, choice.model.clone()?)))
        .collect::<Vec<_>>();
    let workloads = app.panes[3]
        .selected_choices()
        .map(|choice| choice.label.clone())
        .collect::<Vec<_>>();
    let measures = app.panes[4]
        .selected_choices()
        .map(|choice| choice.label.clone())
        .collect::<Vec<_>>();
    let initial_stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Io(format!("cannot create setup name: {error}")))?
        .as_millis();
    let mut stamp = initial_stamp;
    while app.root.join(format!("asb-sweep-{stamp}.toml")).exists()
        || app.root.join(format!("asb-test-{stamp}-000.toml")).exists()
    {
        stamp = stamp.saturating_add(1);
    }
    let mut paths = Vec::new();
    let mut index = 0usize;
    for agent in &agents {
        for provider in &providers {
            for (model_provider, model) in &models {
                if model_provider != provider {
                    continue;
                }
                for workload in &workloads {
                    let name = format!("asb-test-{stamp}-{index:03}.toml");
                    let path = app.root.join(&name);
                    let config = BenchmarkConfig {
                        agent: agent.clone(),
                        workload: workload.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        workspace: app.project.workspace_dir.clone(),
                        log_file: format!("{}.log", name.trim_end_matches(".toml")),
                        measurements: Some(MeasurementConfig {
                            enabled: true,
                            directory: "measurements".into(),
                            collectors: measures.clone(),
                        }),
                    };
                    let text = toml::to_string_pretty(&config)
                        .map_err(|error| Error::Input(format!("cannot render {name}: {error}")))?;
                    fs::write(&path, text).map_err(|error| {
                        Error::Io(format!("cannot write {}: {error}", path.display()))
                    })?;
                    paths.push(name);
                    index += 1;
                }
            }
        }
    }
    if paths.is_empty() {
        return Err(Error::Input(
            "no compatible provider/model selections were made".into(),
        ));
    }
    let sweep_name = format!("asb-sweep-{stamp}.toml");
    let sweep = app.root.join(&sweep_name);
    let text = toml::to_string_pretty(&SweepFile { configs: paths })
        .map_err(|error| Error::Input(format!("cannot render {sweep_name}: {error}")))?;
    fs::write(&sweep, text)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", sweep.display())))?;
    Ok(Generated {
        tests: index,
        sweep,
    })
}

#[cfg(test)]
mod tests {
    use super::{App, Choice, Pane, default_model, generate_configs};
    use crate::project::ProjectConfig;

    #[test]
    fn pane_filters_and_toggles_visible_choices() {
        let mut pane = Pane::new(vec![
            Choice {
                label: "alpha".into(),
                provider: None,
                model: None,
            },
            Choice {
                label: "beta".into(),
                provider: None,
                model: None,
            },
        ]);
        pane.search = "bet".into();
        pane.search_changed();
        assert_eq!(pane.visible(), vec![1]);
        pane.toggle();
        assert!(!pane.selected[0]);
        assert!(pane.selected[1]);
        pane.search.clear();
        pane.search_changed();
        assert_eq!(pane.visible().len(), 2);
    }

    #[test]
    fn model_defaults_are_provider_specific_and_stable() {
        assert_eq!(default_model("cli2key"), "gpt-6.1-sol");
        assert_eq!(default_model("opencode-zen"), "mimo-v2.6-flash-free");
        assert_eq!(default_model("unknown"), "test-model");
    }

    #[test]
    fn generation_writes_test_configs_and_sweep_references() {
        let root = std::env::temp_dir().join(format!("asb-tui-generation-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let project = ProjectConfig::default();
        let mut app = App::new(root.clone(), project);
        app.panes[0].selected[0] = true;
        app.panes[1].selected[0] = true;
        app.refresh_models();
        app.panes[2].selected[0] = true;
        app.panes[3].selected[0] = true;
        let generated = generate_configs(&app).expect("configs generated");
        assert_eq!(generated.tests, 1);
        let test_path = std::fs::read_dir(&root)
            .expect("test directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("asb-test-") && name.ends_with(".toml"))
            })
            .expect("generated test config");
        assert!(std::fs::read_to_string(test_path).is_ok());
        let sweep = std::fs::read_to_string(&generated.sweep).expect("sweep written");
        assert!(sweep.contains("asb-test-"));
        let _ = std::fs::remove_dir_all(root);
    }
}
