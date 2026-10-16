//! Interactive, modular setup wizard built with Ratatui.

use crate::benchmark::{BenchmarkConfig, MeasurementConfig};
use crate::{Error, install, measurements, models, project, providers, workloads};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const AGENTS: &[&str] = &[
    "codex",
    "pi",
    "opencode",
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
    "Choose the measurement collectors; host tools are installed project-locally when possible.",
];

// U+2139 is commonly rendered as a blue/white emoji. Use the text-style
// circled glyph so the terminal honors the readiness foreground color.
const INFO_GLYPH: &str = "ⓘ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Availability {
    Available,
    Missing,
    Unknown,
}

#[derive(Clone, Debug)]
struct Choice {
    label: String,
    provider: Option<String>,
    model: Option<String>,
    availability: Availability,
}

#[derive(Clone, Debug)]
struct Pane {
    choices: Vec<Choice>,
    selected: Vec<bool>,
    cursor: usize,
    search: String,
    sections: Vec<Option<&'static str>>,
}

impl Pane {
    fn new(choices: Vec<Choice>) -> Self {
        Self {
            selected: vec![false; choices.len()],
            sections: vec![None; choices.len()],
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
    message_color: Color,
    message_until: Option<std::time::Instant>,
}

impl App {
    fn new(root: PathBuf, project: project::ProjectConfig) -> Self {
        let agents = AGENTS
            .iter()
            .map(|name| Choice {
                label: (*name).into(),
                provider: None,
                model: None,
                availability: Availability::Unknown,
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
                availability: Availability::Unknown,
            })
            .collect();
        let workloads = workload_choices(&root, &project.workloads_dir);
        let mut workload_pane = Pane::new(workloads);
        let external_names = install::external_workload_names()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for (index, choice) in workload_pane.choices.iter().enumerate() {
            let name = choice
                .label
                .strip_suffix(" [trial set]")
                .unwrap_or(&choice.label);
            workload_pane.sections[index] = Some(if external_names.contains(name) {
                "External workloads"
            } else {
                "Built-in workloads"
            });
        }
        let measures = measurements::available_collectors()
            .into_iter()
            .map(|name| Choice {
                label: name,
                provider: None,
                model: None,
                availability: Availability::Available,
            })
            .collect();
        let mut measure_pane = Pane::new(measures);
        measure_pane.selected.fill(true);
        let mut app = Self {
            root,
            project,
            panes: vec![
                Pane::new(agents),
                Pane::new(provider_choices),
                Pane::new(Vec::new()),
                workload_pane,
                measure_pane,
            ],
            step: 0,
            message: "Space selects; arrows move; Tab or Enter advances; Esc cancels.".into(),
            message_color: Color::Yellow,
            message_until: None,
        };
        app.refresh_availability();
        app
    }

    fn refresh_availability(&mut self) {
        let tools = project::resolve(&self.root, &self.project.tools_dir);
        for choice in &mut self.panes[0].choices {
            choice.availability = if install::tool_available(&choice.label, &tools) {
                Availability::Available
            } else {
                Availability::Missing
            };
        }
        let workloads = project::resolve(&self.root, &self.project.workloads_dir);
        for choice in &mut self.panes[3].choices {
            let name = choice
                .label
                .strip_suffix(" [trial set]")
                .unwrap_or(&choice.label);
            choice.availability = if install::workload_available(name, &workloads)
                || workloads::load(name, &workloads).is_ok()
            {
                Availability::Available
            } else {
                Availability::Missing
            };
        }
        for choice in &mut self.panes[4].choices {
            choice.availability = match choice.label.as_str() {
                "token_usage" | "steps" | "success_rate" => Availability::Available,
                name if install::tool_available(name, &tools) => Availability::Available,
                _ => Availability::Missing,
            };
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
                    availability: Availability::Unknown,
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

fn generated_summary(generated: &Generated) -> String {
    format!(
        "Generated {} benchmark configurations and sweep {}\nRun it with: asb sweep {}",
        generated.tests,
        generated.sweep.display(),
        generated.sweep.display(),
    )
}

const TEMPORARY_MESSAGE: std::time::Duration = std::time::Duration::from_secs(4);

fn is_install_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('i') | KeyCode::Tab)
}

/// Legacy terminals encode Ctrl-I as an indistinguishable bare Tab byte. In
/// that mode, use Tab as an install fallback only for an unavailable item;
/// Enter remains the unambiguous step/navigation key.
fn legacy_setup_install_key(key: &KeyEvent, app: &App) -> bool {
    key.code == KeyCode::Tab
        && key.modifiers.is_empty()
        && matches!(app.step, 0 | 3 | 4)
        && app.panes[app.step]
            .choices
            .get(app.panes[app.step].cursor)
            .is_some_and(|choice| choice.availability != Availability::Available)
}

fn legacy_workload_install_key(key: &KeyEvent, app: &WorkloadApp) -> bool {
    key.code == KeyCode::Tab
        && key.modifiers.is_empty()
        && app.level == 0
        && app
            .workloads
            .choices
            .get(app.workloads.cursor)
            .is_some_and(|choice| choice.availability != Availability::Available)
}

fn set_message(
    message: &mut String,
    color: &mut Color,
    until: &mut Option<std::time::Instant>,
    text: impl Into<String>,
    status: Color,
    temporary: bool,
) {
    *message = text.into();
    *color = status;
    *until = temporary.then(|| std::time::Instant::now() + TEMPORARY_MESSAGE);
}

fn visible_message(message: &str, until: Option<std::time::Instant>) -> &str {
    if until.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        ""
    } else {
        message
    }
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
    writeln!(output, "{}", generated_summary(&generated)).map_err(crate::io_error)?;
    output.flush().map_err(crate::io_error)
}

/// Launch the tree-shaped workload/trial selector. It reuses the setup TUI's
/// Pane, terminal lifecycle, search, selection, and keyboard conventions.
pub fn workload_tui(args: &[String], output: &mut dyn Write) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(Error::Usage("workload tui does not accept options"));
    }
    let (root, project) = project::load(Path::new("."))?;
    let workload_dir = project::resolve(&root, &project.workloads_dir);
    let choices = external_workload_choices(&workload_dir);
    if choices.is_empty() {
        return Err(Error::Input(
            "no installed external workloads; install one with `asb workload install`".into(),
        ));
    }
    let mut app = WorkloadApp {
        root,
        project,
        workloads: Pane::new(choices),
        trials: Pane::new(Vec::new()),
        level: 0,
        selected_workload: None,
        selections: BTreeSet::new(),
        message: "Space selects; Enter opens trials/saves; Tab down; Shift-Tab up; Esc cancels."
            .into(),
        message_color: Color::Yellow,
        message_until: None,
    };
    refresh_workload_availability(&mut app);
    let mut terminal = start_terminal()
        .map_err(|error| Error::Io(format!("cannot start workload TUI: {error}")))?;
    let result = workload_loop(&mut terminal, &mut app);
    let cleanup = stop_terminal(terminal);
    if result.is_ok() {
        cleanup.map_err(|error| Error::Io(format!("cannot restore terminal: {error}")))?;
    }
    let count = result?;
    writeln!(output, "Wrote {count} workload trial-set configurations").map_err(crate::io_error)
}

struct WorkloadApp {
    root: PathBuf,
    project: project::ProjectConfig,
    workloads: Pane,
    trials: Pane,
    level: usize,
    selected_workload: Option<String>,
    selections: BTreeSet<String>,
    message: String,
    message_color: Color,
    message_until: Option<std::time::Instant>,
}

fn external_workload_choices(directory: &Path) -> Vec<Choice> {
    let mut names = install::external_workload_names()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    names.extend(
        fs::read_dir(directory)
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .filter_map(|entry| {
                let path = entry.path();
                (path.is_dir() && path.join("source").is_dir())
                    .then(|| path.file_name()?.to_str().map(str::to_owned))
                    .flatten()
            }),
    );
    let external_names = install::external_workload_names()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut choices = names
        .into_iter()
        .map(|label| {
            let availability = if install::workload_available(&label, directory) {
                Availability::Available
            } else {
                Availability::Missing
            };
            Choice {
                label,
                provider: None,
                model: None,
                availability,
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|choice| {
        let name = choice
            .label
            .strip_suffix(" [trial set]")
            .unwrap_or(&choice.label);
        (!external_names.contains(name), choice.label.clone())
    });
    choices
}

fn refresh_workload_availability(app: &mut WorkloadApp) {
    let directory = project::resolve(&app.root, &app.project.workloads_dir);
    for choice in &mut app.workloads.choices {
        let name = &choice.label;
        choice.availability = if install::workload_available(name, &directory) {
            Availability::Available
        } else {
            Availability::Missing
        };
    }
}

fn workload_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut WorkloadApp,
) -> Result<usize, Error> {
    loop {
        terminal
            .draw(|frame| draw_workload(frame, app))
            .map_err(|error| Error::Io(format!("cannot draw workload TUI: {error}")))?;
        if !event::poll(std::time::Duration::from_millis(250))
            .map_err(|error| Error::Io(format!("cannot read workload TUI input: {error}")))?
        {
            continue;
        }
        let Event::Key(key) = event::read()
            .map_err(|error| Error::Io(format!("cannot read workload TUI input: {error}")))?
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
                return Err(Error::Usage("workload TUI cancelled"));
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(Error::Usage("workload TUI cancelled"));
            }
            key if is_install_key(&key) || legacy_workload_install_key(&key, app) => {
                install_workload_selection(terminal, app)?;
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::SHIFT) => app.level = 0,
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => app.level = 0,
            KeyEvent {
                code: KeyCode::Tab | KeyCode::Enter,
                ..
            } => {
                if app.level == 0 {
                    if !app.workloads.has_selection() {
                        app.message = "Select at least one external workload first.".into();
                        continue;
                    }
                    let current = app.workloads.choices[app.workloads.cursor].label.clone();
                    if app.selected_workload.as_deref() != Some(&current) {
                        app.selected_workload = Some(current.clone());
                        app.trials = Pane::new(
                            workloads::trial_ids(&current)
                                .into_iter()
                                .map(|trial| Choice {
                                    label: trial.id,
                                    provider: None,
                                    model: None,
                                    availability: Availability::Available,
                                })
                                .collect(),
                        );
                        app.level = 1;
                        app.message = "Select enabled trials; Enter saves this workload. Search filters trials.".into();
                    } else if !app.selections.is_empty() {
                        return write_trial_sets(app);
                    }
                } else if app.trials.has_selection() {
                    let workload = app.selected_workload.take().expect("trial workload");
                    for choice in app.trials.selected_choices() {
                        app.selections
                            .insert(format!("{workload}\t{}", choice.label));
                    }
                    app.level = 0;
                    app.message =
                        "Trial set saved. Select another workload or press Enter again to finish."
                            .into();
                } else {
                    app.message = "Select at least one trial before continuing.".into();
                }
            }
            key if handle_pane_key(
                if app.level == 0 {
                    &mut app.workloads
                } else {
                    &mut app.trials
                },
                key,
            ) => {}
            _ => {}
        }
    }
}

fn install_workload_selection(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut WorkloadApp,
) -> Result<(), Error> {
    if app.level != 0 {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "Trials are configured locally; return to workloads before installing.",
            Color::Red,
            true,
        );
        return Ok(());
    }
    let Some(choice) = app.workloads.choices.get(app.workloads.cursor) else {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "Nothing is highlighted to install.",
            Color::Red,
            true,
        );
        return Ok(());
    };
    let name = choice.label.clone();
    set_message(
        &mut app.message,
        &mut app.message_color,
        &mut app.message_until,
        format!("Installing `{name}` (8%)..."),
        Color::Cyan,
        false,
    );
    let progress_state = std::sync::Arc::new(std::sync::Mutex::new((8_u8, "preparing".to_owned())));
    let worker_progress = std::sync::Arc::clone(&progress_state);
    let (sender, receiver) = std::sync::mpsc::channel();
    let install_name = name.clone();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut progress = |percent: u8, stage: &str| {
            if let Ok(mut state) = worker_progress.lock() {
                *state = (percent, stage.to_owned());
            }
        };
        let result = install::workload_with_progress(&[install_name], &mut output, &mut progress);
        let _ = sender.send(result);
    });
    let started = Instant::now();
    let mut tick = 0_u64;
    let result = loop {
        if let Ok(result) = receiver.try_recv() {
            break result;
        }
        let (percent, stage) = progress_state
            .lock()
            .map(|state| state.clone())
            .unwrap_or((8, "installing".to_owned()));
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            install_status_message(percent, &stage, started, tick),
            Color::Cyan,
            false,
        );
        terminal
            .draw(|frame| draw_workload(frame, app))
            .map_err(|error| Error::Io(format!("cannot draw workload install status: {error}")))?;
        tick = tick.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    refresh_workload_availability(app);
    match result {
        Ok(()) => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!("Installed or refreshed `{name}`; status is ready."),
            Color::Green,
            true,
        ),
        Err(error) => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!("Cannot install `{name}`: {error}"),
            Color::Red,
            true,
        ),
    };
    Ok(())
}

/// Apply the shared searchable-list controls used by both terminal TUIs.
fn handle_pane_key(pane: &mut Pane, key: KeyEvent) -> bool {
    match key {
        KeyEvent {
            code: KeyCode::Char(' '),
            ..
        } => pane.toggle(),
        KeyEvent {
            code: KeyCode::Up, ..
        } => pane.move_cursor(-1),
        KeyEvent {
            code: KeyCode::Down,
            ..
        } => pane.move_cursor(1),
        KeyEvent {
            code: KeyCode::PageUp,
            ..
        } => pane.move_cursor(-8),
        KeyEvent {
            code: KeyCode::PageDown,
            ..
        } => pane.move_cursor(8),
        KeyEvent {
            code: KeyCode::Home,
            ..
        } => pane.cursor = 0,
        KeyEvent {
            code: KeyCode::End, ..
        } => {
            if let Some(last) = pane.choices.len().checked_sub(1) {
                pane.cursor = last;
            }
        }
        KeyEvent {
            code: KeyCode::Backspace,
            ..
        } => {
            pane.search.pop();
            pane.search_changed();
        }
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        } if !modifiers.contains(KeyModifiers::CONTROL)
            && !modifiers.contains(KeyModifiers::ALT) =>
        {
            pane.search.push(ch);
            pane.search_changed();
        }
        _ => return false,
    }
    true
}

fn draw_workload(frame: &mut ratatui::Frame<'_>, app: &WorkloadApp) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let steps = ["workloads", "trials"];
    let step_line = steps
        .iter()
        .enumerate()
        .flat_map(|(index, name)| {
            let style = if index == app.level {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < app.level {
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
        Paragraph::new(Line::from(step_line)).block(
            Block::default()
                .borders(Borders::ALL)
                .title("ASB workload trials"),
        ),
        vertical[0],
    );
    let description = if app.level == 0 {
        "Choose external workloads; red entries can be installed with Ctrl-I."
    } else {
        "Choose enabled trials for the selected workload."
    };
    frame.render_widget(
        Paragraph::new(description).block(Block::default().borders(Borders::ALL).title("Step")),
        vertical[1],
    );
    let pane = if app.level == 0 {
        &app.workloads
    } else {
        &app.trials
    };
    render_choice_pane(frame, vertical[2], pane, true);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Ctrl-I install", Style::default().fg(Color::Cyan)),
            Span::raw(" | ⓘ green=ready red=missing yellow=unknown | Tab fallback install | Enter next | "),
            Span::styled(
                visible_message(&app.message, app.message_until),
                Style::default().fg(app.message_color),
            ),
        ])),
        vertical[3],
    );
}

fn write_trial_sets(app: &WorkloadApp) -> Result<usize, Error> {
    let directory = project::resolve(&app.root, &app.project.workloads_dir);
    let mut grouped = std::collections::BTreeMap::<String, Vec<String>>::new();
    for value in &app.selections {
        if let Some((workload, trial)) = value.split_once('\t') {
            grouped
                .entry(workload.to_owned())
                .or_default()
                .push(trial.to_owned());
        }
    }
    let mut written = 0;
    for (workload, ids) in grouped {
        let catalog = workloads::trial_ids(&workload);
        let trials = catalog
            .into_iter()
            .filter(|trial| ids.contains(&trial.id))
            .collect();
        let config = workloads::TrialSet {
            workload: workload.clone(),
            trials,
        };
        let path = directory.join(format!("{workload}.workload.toml"));
        let text =
            toml::to_string_pretty(&config).map_err(|error| Error::Input(error.to_string()))?;
        fs::write(path, text).map_err(|error| Error::Io(error.to_string()))?;
        written += 1;
    }
    Ok(written)
}

fn start_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, io::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Ctrl-I is encoded as the same byte as Tab by legacy terminals. Request
    // Kitty's disambiguating keyboard protocol so crossterm can preserve the
    // control modifier and the shortcut cannot fall through to navigation.
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::cursor::Hide,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    )?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn stop_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<(), io::Error> {
    terminal.clear()?;
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
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
            key if is_install_key(&key) || legacy_setup_install_key(&key, app) => {
                install_setup_selection(terminal, app)?;
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
            key if handle_pane_key(&mut app.panes[app.step], key) => {}
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
    let pane = &app.panes[app.step];
    render_choice_pane(frame, vertical[2], pane, !matches!(app.step, 1 | 2));
    frame.render_widget(
        Paragraph::new(Text::from(Line::from(vec![
            if matches!(app.step, 1 | 2) {
                Span::styled("Discovery only", Style::default().fg(Color::Yellow))
            } else {
                Span::styled("Ctrl-I install", Style::default().fg(Color::Cyan))
            },
            if matches!(app.step, 1 | 2) {
                Span::raw(" | Enter next | ")
            } else {
                Span::raw(" | ⓘ green=ready red=missing yellow=unknown | Tab fallback install | Enter next | ")
            },
            Span::styled(
                visible_message(&app.message, app.message_until),
                Style::default().fg(app.message_color),
            ),
        ]))),
        vertical[3],
    );
}

/// Format an honest, live installer indicator. Installer subprocesses write
/// their output to `.asb/install.log` and do not expose byte-level progress,
/// so the fixed percentage marks the completed stage while the moving marker
/// proves that a download or package install is still being monitored.
fn install_status_message(percent: u8, stage: &str, started: Instant, tick: u64) -> String {
    const WIDTH: usize = 20;
    const FRAMES: &[char] = &['▸', '▹', '▻', '▹'];
    let completed = (usize::from(percent).min(100) * WIDTH / 100).min(WIDTH);
    let pulse = if completed < WIDTH {
        tick as usize % (WIDTH - completed).max(1)
    } else {
        0
    };
    let mut bar = String::with_capacity(WIDTH);
    for index in 0..WIDTH {
        if index < completed {
            bar.push('━');
        } else if index == completed + pulse && completed < WIDTH {
            bar.push(FRAMES[(tick as usize) % FRAMES.len()]);
        } else {
            bar.push('─');
        }
    }
    format!(
        "Installing [{bar}] {percent}%: {stage} ({}s)",
        started.elapsed().as_secs()
    )
}

/// Render the shared searchable, selectable list pane used by both TUIs.
fn render_choice_pane(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    pane: &Pane,
    show_info: bool,
) {
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);
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
    let mut items = Vec::new();
    let mut selected = None;
    let mut previous_section = None;
    for index in visible {
        let choice = &pane.choices[index];
        let section = pane.sections.get(index).copied().flatten();
        if let Some(section) = section {
            if previous_section != Some(section) {
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("── {section} ──"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ))));
            }
        }
        let is_selected = index == pane.cursor;
        if is_selected {
            selected = Some(items.len());
        }
        let marker = if pane.selected[index] { "[x]" } else { "[ ]" };
        let mut row = vec![Span::raw(format!("{marker} "))];
        if show_info {
            row.push(Span::styled(
                INFO_GLYPH,
                availability_style(choice.availability),
            ));
            row.push(Span::raw("  ".to_owned()));
        }
        row.push(Span::raw(choice.label.clone()));
        items.push(ListItem::new(Line::from(row)).style(choice_row_style(is_selected)));
        previous_section = section;
    }
    let mut state = ListState::default();
    state.select(selected.or_else(|| (!items.is_empty()).then_some(0)));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Available choices"),
        ),
        body[1],
        &mut state,
    );
}

fn choice_row_style(selected: bool) -> Style {
    if selected {
        // Style the row itself instead of List::highlight_style: Ratatui's
        // list highlight layer overrides child spans and would turn the
        // readiness glyph white/blue. The glyph span remains green/yellow/red.
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn availability_style(availability: Availability) -> Style {
    match availability {
        Availability::Available => Style::default().fg(Color::Green),
        Availability::Missing => Style::default().fg(Color::Red),
        Availability::Unknown => Style::default().fg(Color::Yellow),
    }
}

fn install_setup_selection(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<(), Error> {
    let Some(choice) = app.panes[app.step].choices.get(app.panes[app.step].cursor) else {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "Nothing is highlighted to install.",
            Color::Red,
            true,
        );
        return Ok(());
    };
    let label = choice.label.clone();
    if !matches!(app.step, 0 | 3 | 4) {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "This item is discovered or configured, not installed by ASB.",
            Color::Red,
            true,
        );
        return Ok(());
    }
    set_message(
        &mut app.message,
        &mut app.message_color,
        &mut app.message_until,
        format!("Installing `{label}`..."),
        Color::Cyan,
        false,
    );
    let step = app.step;
    let install_label = label.clone();
    let progress_state = std::sync::Arc::new(std::sync::Mutex::new((8_u8, "preparing".to_owned())));
    let worker_progress = std::sync::Arc::clone(&progress_state);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut progress = |percent: u8, stage: &str| {
            if let Ok(mut state) = worker_progress.lock() {
                *state = (percent, stage.to_owned());
            }
        };
        let result = match step {
            0 | 4 => install::tool_with_progress(&[install_label], &mut output, &mut progress),
            3 => {
                let workload = install_label
                    .strip_suffix(" [trial set]")
                    .unwrap_or(&install_label);
                install::workload_with_progress(&[workload.to_owned()], &mut output, &mut progress)
            }
            _ => unreachable!("installable setup steps were checked above"),
        };
        let _ = sender.send(result);
    });
    let started = Instant::now();
    let mut tick = 0_u64;
    let result = loop {
        if let Ok(result) = receiver.try_recv() {
            break result;
        }
        let (percent, stage) = progress_state
            .lock()
            .map(|state| state.clone())
            .unwrap_or((8, "installing".to_owned()));
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            install_status_message(percent, &stage, started, tick),
            Color::Cyan,
            false,
        );
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|error| Error::Io(format!("cannot draw setup install status: {error}")))?;
        tick = tick.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    app.refresh_availability();
    match result {
        Ok(()) => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!("Installed or refreshed `{label}`; status is ready."),
            Color::Green,
            true,
        ),
        Err(error) => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!("Cannot install `{label}`: {error}"),
            Color::Red,
            true,
        ),
    };
    Ok(())
}

fn workload_choices(root: &Path, directory: &str) -> Vec<Choice> {
    let path = project::resolve(root, directory);
    let mut names = fs::read_dir(&path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            if path.extension().and_then(|value| value.to_str()) == Some("txt") {
                return Some(file_name.trim_end_matches(".txt").to_owned());
            }
            file_name
                .strip_suffix(".workload.toml")
                .map(|name| format!("{name} [trial set]"))
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
    names.extend(install::external_workload_names().map(str::to_owned));
    let external_names = install::external_workload_names()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut choices = names
        .into_iter()
        .map(|label| {
            let name = label.strip_suffix(" [trial set]").unwrap_or(&label);
            let availability = if external_names.contains(name) {
                install::workload_available(name, &path)
            } else {
                workloads::load(name, &path).is_ok()
            };
            Choice {
                label,
                provider: None,
                model: None,
                availability: if availability {
                    Availability::Available
                } else {
                    Availability::Missing
                },
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|choice| {
        let name = choice
            .label
            .strip_suffix(" [trial set]")
            .unwrap_or(&choice.label);
        (external_names.contains(name), choice.label.clone())
    });
    choices
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
                for workload_choice in &workloads {
                    let (workload, trial_set, trials) = selected_trials(app, workload_choice)?;
                    for trial in trials {
                        let name = format!("asb-test-{stamp}-{index:03}.toml");
                        let path = app.root.join(&name);
                        let config = BenchmarkConfig {
                            agent: agent.clone(),
                            workload: workload.clone(),
                            provider: provider.clone(),
                            model: model.clone(),
                            trial_set: trial_set.clone(),
                            trial,
                            workspace: app.project.workspace_dir.clone(),
                            log_file: format!("{}.log", name.trim_end_matches(".toml")),
                            measurements: Some(MeasurementConfig {
                                enabled: true,
                                directory: "measurements".into(),
                                collectors: measures.clone(),
                            }),
                        };
                        let text = toml::to_string_pretty(&config).map_err(|error| {
                            Error::Input(format!("cannot render {name}: {error}"))
                        })?;
                        fs::write(&path, text).map_err(|error| {
                            Error::Io(format!("cannot write {}: {error}", path.display()))
                        })?;
                        paths.push(name);
                        index += 1;
                    }
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

fn selected_trials(
    app: &App,
    choice: &str,
) -> Result<(String, Option<String>, Vec<Option<String>>), Error> {
    let Some(workload) = choice.strip_suffix(" [trial set]") else {
        return Ok((choice.to_owned(), None, vec![None]));
    };
    let filename = format!("{workload}.workload.toml");
    let path = project::resolve(&app.root, &app.project.workloads_dir).join(&filename);
    let trial_set = workloads::read_trial_set(&path).map_err(Error::Input)?;
    if trial_set.trials.is_empty() {
        return Err(Error::Input(format!(
            "workload trial set {} does not enable any trials; run `asb workload tui`",
            path.display()
        )));
    }
    Ok((
        workload.to_owned(),
        Some(filename),
        trial_set
            .trials
            .into_iter()
            .map(|trial| Some(trial.id))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        App, Availability, Choice, Generated, Pane, default_model, generate_configs,
        generated_summary, install_status_message,
    };
    use crate::install;
    use crate::project::ProjectConfig;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Style};
    use std::collections::BTreeSet;
    use std::time::Instant;

    #[test]
    fn pane_filters_and_toggles_visible_choices() {
        let mut pane = Pane::new(vec![
            Choice {
                label: "alpha".into(),
                provider: None,
                model: None,
                availability: Availability::Unknown,
            },
            Choice {
                label: "beta".into(),
                provider: None,
                model: None,
                availability: Availability::Unknown,
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
    fn app_marks_missing_agents_and_internal_collectors() {
        let root = std::env::temp_dir().join(format!("asb-tui-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App::new(root.clone(), ProjectConfig::default());
        assert!(
            app.panes[0]
                .choices
                .iter()
                .all(|choice| choice.availability == Availability::Missing)
        );
        assert!(
            app.panes[4]
                .choices
                .iter()
                .filter(|choice| matches!(
                    choice.label.as_str(),
                    "token_usage" | "steps" | "success_rate"
                ))
                .all(|choice| choice.availability == Availability::Available)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setup_tui_lists_uninstalled_external_workloads() {
        let root = std::env::temp_dir().join(format!("asb-setup-workloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App::new(root.clone(), ProjectConfig::default());
        let workload = app.panes[3]
            .choices
            .iter()
            .find(|choice| choice.label == "terminal-bench")
            .expect("external workload catalog entry");
        assert_eq!(workload.availability, Availability::Missing);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_shortcut_accepts_terminal_ctrl_i_encodings() {
        assert!(super::is_install_key(&KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::CONTROL,
        )));
        assert!(super::is_install_key(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::CONTROL,
        )));
        assert!(!super::is_install_key(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn legacy_bare_tab_falls_back_to_install_for_unavailable_setup_items() {
        let root = std::env::temp_dir().join(format!("asb-tui-shortcut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App::new(root.clone(), ProjectConfig::default());
        assert!(super::legacy_setup_install_key(
            &KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &app
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn selected_row_style_is_visible_without_overriding_status_foreground() {
        let style = super::choice_row_style(true);
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::Gray));
        assert_eq!(super::choice_row_style(false), Style::default());
    }

    #[test]
    fn install_status_message_shows_live_indeterminate_activity() {
        let started = Instant::now();
        let first = install_status_message(15, "installing npm package", started, 0);
        let second = install_status_message(15, "installing npm package", started, 1);
        assert!(first.contains("["));
        assert!(first.contains("] 15%: installing npm package"));
        assert_ne!(
            first, second,
            "the activity marker must move while a stage runs"
        );
    }

    #[test]
    fn readiness_indicator_uses_text_glyph_not_emoji_glyph() {
        assert_eq!(super::INFO_GLYPH, "ⓘ");
        assert_ne!(super::INFO_GLYPH, "ℹ");
    }

    #[test]
    fn workload_tui_lists_uninstalled_external_workloads_as_missing() {
        let root =
            std::env::temp_dir().join(format!("asb-workload-choices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let choices = super::external_workload_choices(&root);
        let terminal = choices
            .iter()
            .find(|choice| choice.label == "terminal-bench")
            .expect("catalog workload is listed");
        assert_eq!(terminal.availability, super::Availability::Missing);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setup_workloads_group_builtins_before_sorted_external_entries() {
        let root =
            std::env::temp_dir().join(format!("asb-setup-workload-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let choices = super::workload_choices(&root, ".");
        let external_names = install::external_workload_names().collect::<BTreeSet<_>>();
        let external_start = choices
            .iter()
            .position(|choice| {
                let name = choice
                    .label
                    .strip_suffix(" [trial set]")
                    .unwrap_or(&choice.label);
                external_names.contains(name)
            })
            .expect("external catalog is present");
        assert!(choices[..external_start].iter().all(|choice| {
            let name = choice
                .label
                .strip_suffix(" [trial set]")
                .unwrap_or(&choice.label);
            !external_names.contains(name)
        }));
        assert!(
            choices[external_start..]
                .windows(2)
                .all(|window| { window[0].label <= window[1].label })
        );
        let _ = std::fs::remove_dir_all(root);
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

    #[test]
    fn setup_summary_prints_the_exact_sweep_command() {
        let summary = generated_summary(&Generated {
            tests: 2,
            sweep: std::path::PathBuf::from("asb-sweep-123.toml"),
        });
        assert!(summary.contains("Run it with: asb sweep asb-sweep-123.toml"));
    }
}
