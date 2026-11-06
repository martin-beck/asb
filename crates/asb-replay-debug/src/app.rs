//! Ratatui evidence presentation and navigation.

use crate::artifacts::{ArtifactSet, Evidence};
use crate::diff::{self, Field, Segment, Status, TextDiff};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Fields,
    Text,
}

pub(crate) struct App {
    set: ArtifactSet,
    selected_case: usize,
    evidence: Evidence,
    fields: Vec<Field>,
    filtered: Vec<usize>,
    field_cursor: usize,
    list_state: ListState,
    text: TextDiff,
    displayed: Vec<Option<usize>>,
    scroll: usize,
    horizontal: u16,
    page: usize,
    all_fields: bool,
    all_context: bool,
    focus: Focus,
    help: bool,
    search: String,
    searching: bool,
    error: Option<String>,
    project: Option<std::path::PathBuf>,
    workload: Option<String>,
    manage_normalizers: bool,
}

fn state(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "passed",
        Some(false) => "failed",
        None => "unknown",
    }
}

impl App {
    pub(crate) fn new(set: ArtifactSet, replica: Option<u64>, all: bool) -> Result<Self, String> {
        let selected_case = match replica {
            Some(replica) => set.cases.iter().position(|case| case.outcome.replica == Some(replica))
                .ok_or_else(|| format!("no mismatch attributed to replica {replica}; open the campaign/batch with its run logs"))?,
            None => 0,
        };
        let evidence = set.cases[selected_case].load()?;
        let fields = diff::compare(&evidence.expected, &evidence.actual)?;
        let mut app = Self {
            set,
            selected_case,
            evidence,
            fields,
            filtered: Vec::new(),
            field_cursor: 0,
            list_state: ListState::default(),
            text: TextDiff {
                rows: Vec::new(),
                notes: Vec::new(),
            },
            displayed: Vec::new(),
            scroll: 0,
            horizontal: 0,
            page: 10,
            all_fields: all,
            all_context: all,
            focus: Focus::Fields,
            help: false,
            search: String::new(),
            searching: false,
            error: None,
            project: None,
            workload: None,
            manage_normalizers: false,
        };
        app.refilter();
        Ok(app)
    }

    pub(crate) fn configure_normalizers(
        &mut self,
        project: Option<std::path::PathBuf>,
        workload: Option<String>,
    ) {
        self.project = project.or_else(|| crate::normalizers::find_project(&self.set.root));
        self.workload = workload.or_else(|| {
            crate::normalizers::workload_from_evidence(&self.set.cases[self.selected_case].path)
        });
    }

    fn field(&self) -> Option<&Field> {
        self.filtered
            .get(self.field_cursor)
            .map(|index| &self.fields[*index])
    }

    fn refilter(&mut self) {
        let previous = self.filtered.get(self.field_cursor).copied();
        let search = self.search.to_lowercase();
        self.filtered = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| {
                (self.all_fields || field.status != Status::Equal)
                    && field.path.to_lowercase().contains(&search)
            })
            .map(|(index, _)| index)
            .collect();
        self.field_cursor = previous
            .and_then(|previous| self.filtered.iter().position(|index| *index == previous))
            .or_else(|| {
                self.filtered
                    .iter()
                    .position(|index| self.fields[*index].status == Status::Changed)
            })
            .unwrap_or(0);
        self.list_state = ListState::default();
        self.reset_text();
    }

    fn reset_text(&mut self) {
        self.text = self
            .field()
            .map(|field| TextDiff::new(&field.text(true), &field.text(false)))
            .unwrap_or_else(|| TextDiff::new("", ""));
        self.displayed = self.text.displayed(self.all_context);
        self.scroll = self
            .displayed
            .iter()
            .position(|row| row.is_some_and(|index| self.text.rows[index].changed))
            .unwrap_or(0)
            .saturating_sub(3);
        self.horizontal = 0;
    }

    fn select_case(&mut self, index: usize) -> Result<(), String> {
        let evidence = self.set.cases[index].load()?;
        let fields = diff::compare(&evidence.expected, &evidence.actual)?;
        self.selected_case = index;
        self.evidence = evidence;
        self.fields = fields;
        self.error = None;
        self.filtered.clear();
        self.refilter();
        Ok(())
    }

    fn reload(&mut self) -> Result<(), String> {
        let selected = &self.set.cases[self.selected_case].path;
        let set = ArtifactSet::open(&self.set.root)?;
        let index = set
            .cases
            .iter()
            .position(|case| &case.path == selected)
            .unwrap_or(0);
        let mut refreshed = Self::new(set, None, self.all_fields)?;
        refreshed.all_context = self.all_context;
        refreshed.search = self.search.clone();
        refreshed.configure_normalizers(self.project.clone(), self.workload.clone());
        refreshed.select_case(index)?;
        *self = refreshed;
        Ok(())
    }

    fn move_field(&mut self, delta: isize) {
        self.field_cursor = self
            .field_cursor
            .saturating_add_signed(delta)
            .min(self.filtered.len().saturating_sub(1));
        self.reset_text();
    }

    fn next_difference(&mut self, backwards: bool) {
        let count = self.filtered.len();
        for offset in 1..=count {
            let index = if backwards {
                (self.field_cursor + count - offset) % count
            } else {
                (self.field_cursor + offset) % count
            };
            if self.fields[self.filtered[index]].status == Status::Changed {
                self.field_cursor = index;
                self.reset_text();
                break;
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search.clear();
                    self.refilter();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.refilter();
                }
                KeyCode::Char(c) => {
                    self.search.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return false;
        }
        if self.help {
            self.help = false;
            return false;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::Char('m') => self.manage_normalizers = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Fields {
                    Focus::Text
                } else {
                    Focus::Fields
                }
            }
            KeyCode::Char('a') => {
                self.all_fields = !self.all_fields;
                self.refilter();
            }
            KeyCode::Char('c') => {
                self.all_context = !self.all_context;
                self.reset_text();
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('n') => self.next_difference(false),
            KeyCode::Char('N') => self.next_difference(true),
            KeyCode::Char(']') | KeyCode::Char('[') => {
                let delta = if key.code == KeyCode::Char(']') {
                    1
                } else {
                    self.set.cases.len() - 1
                };
                let index = (self.selected_case + delta) % self.set.cases.len();
                if let Err(error) = self.select_case(index) {
                    self.error = Some(error);
                }
            }
            KeyCode::Char('r') => {
                if let Err(error) = self.reload() {
                    self.error = Some(error);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Fields {
                    self.move_field(1);
                } else {
                    self.scroll = (self.scroll + 1).min(self.displayed.len().saturating_sub(1));
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Fields {
                    self.move_field(-1);
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + self.page).min(self.displayed.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page),
            KeyCode::Right | KeyCode::Char('l') => {
                self.horizontal = self.horizontal.saturating_add(8)
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.horizontal = self.horizontal.saturating_sub(8)
            }
            KeyCode::Home => {
                self.scroll = 0;
                if self.focus == Focus::Fields {
                    self.field_cursor = 0;
                    self.reset_text();
                }
            }
            KeyCode::End => {
                if self.focus == Focus::Fields {
                    self.field_cursor = self.filtered.len().saturating_sub(1);
                    self.reset_text();
                } else {
                    self.scroll = self.displayed.len().saturating_sub(self.page);
                }
            }
            _ => {}
        }
        false
    }

    fn counts(&self) -> (usize, usize, usize) {
        let count = |status| {
            self.fields
                .iter()
                .filter(|field| field.status == status)
                .count()
        };
        (
            count(Status::Changed),
            count(Status::Equal),
            count(Status::Unavailable),
        )
    }

    pub(crate) fn print(&self, out: &mut impl Write) -> io::Result<()> {
        let case = &self.set.cases[self.selected_case];
        let (changed, equal, unknown) = self.counts();
        writeln!(
            out,
            "{}\nEvidence: {}",
            diff::visible(&case.label()),
            diff::visible(&case.path.display().to_string())
        )?;
        writeln!(
            out,
            "Scope: retained normalized/redacted request snapshots; original raw inputs unavailable"
        )?;
        writeln!(
            out,
            "Request: {} | execution: {} | official grade: {} | cleanup: {}",
            self.evidence
                .sequence
                .map_or("unknown".into(), |n| n.to_string()),
            state(case.outcome.execution),
            state(case.outcome.grade),
            state(case.outcome.cleanup)
        )?;
        writeln!(
            out,
            "Fields: {changed} changed, {equal} equal in evidence, {unknown} redacted/unknown"
        )?;
        for warning in &self.set.warnings {
            writeln!(out, "Warning: {}", diff::visible(warning))?;
        }
        for index in &self.filtered {
            let field = &self.fields[*index];
            writeln!(
                out,
                "\n{} {} ({} -> {})",
                field.status.label(),
                diff::visible(&field.path),
                field.kind(true),
                field.kind(false)
            )?;
            let text = TextDiff::new(&field.text(true), &field.text(false));
            for note in &text.notes {
                writeln!(out, "Note: {note}")?;
            }
            for index in text.displayed(self.all_context) {
                let Some(index) = index else {
                    writeln!(out, "  ... unchanged lines folded ...")?;
                    continue;
                };
                let row = &text.rows[index];
                let join =
                    |parts: &[Segment]| parts.iter().map(|s| s.text.as_str()).collect::<String>();
                if row.changed {
                    if let Some(line) = row.expected_number {
                        writeln!(out, "- {line:>4} {}", join(&row.expected))?;
                    }
                    if let Some(line) = row.actual_number {
                        writeln!(out, "+ {line:>4} {}", join(&row.actual))?;
                    }
                } else {
                    writeln!(
                        out,
                        "  {:>4} {}",
                        row.expected_number.unwrap_or(0),
                        join(&row.expected)
                    )?;
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < 64 || area.height < 20 {
            frame.render_widget(
                Paragraph::new("Resize to at least 64 columns x 20 rows. Press q to quit."),
                area,
            );
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(6),
                Constraint::Length(4),
                Constraint::Min(6),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "ASB Replay Debugger",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(diff::visible(&self.set.root.display().to_string())),
            ]),
            chunks[0],
        );
        let cases: Vec<_> = self
            .set
            .cases
            .iter()
            .map(|case| {
                ListItem::new(diff::visible(&format!(
                    "{} | execution {} | grade {}",
                    case.label(),
                    state(case.outcome.execution),
                    state(case.outcome.grade)
                )))
            })
            .collect();
        let mut selection = ListState::default().with_selected(Some(self.selected_case));
        frame.render_stateful_widget(
            List::new(cases)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Cases {}/{}   [ / ] previous / next",
                    self.selected_case + 1,
                    self.set.cases.len()
                )))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            chunks[1],
            &mut selection,
        );
        let (changed, equal, unknown) = self.counts();
        let outcome = &self.set.cases[self.selected_case].outcome;
        let summary = vec![
            Line::from(format!("Request {} | track {} | {changed} changed / {equal} equal / {unknown} unknown fields | {} ms",self.evidence.sequence.map_or("?".into(),|n|n.to_string()),outcome.track.map_or("?".into(),|n|n.to_string()),outcome.elapsed_ms.map_or("?".into(),|n|n.to_string()))),
            Line::from(Span::styled("Normalized/redacted snapshots; raw inputs and normalization history unavailable.",Style::default().fg(Color::Yellow))),
            Line::from(diff::visible(&format!("Reported: {} at {} | cleanup {}",self.evidence.category,self.evidence.reported_path,state(outcome.cleanup)))),
            Line::from(Span::styled(diff::visible(self.error.as_deref().or_else(|| self.set.warnings.first().map(String::as_str)).or_else(|| self.text.notes.first().map(String::as_str)).unwrap_or("= equal evidence   ! changed   ? redacted/unknown; matching fields are hidden until 'a'")),Style::default().fg(Color::Yellow))),
        ];
        frame.render_widget(Paragraph::new(summary), chunks[2]);
        let panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(chunks[3]);
        let field_items: Vec<_> = self
            .filtered
            .iter()
            .map(|index| {
                let field = &self.fields[*index];
                ListItem::new(diff::visible(&format!(
                    "{} {}",
                    field.status.label(),
                    field.path
                )))
                .style(Style::default().fg(match field.status {
                    Status::Changed => Color::Yellow,
                    Status::Equal => Color::DarkGray,
                    Status::Unavailable => Color::Magenta,
                }))
            })
            .collect();
        self.list_state
            .select((!self.filtered.is_empty()).then_some(self.field_cursor));
        frame.render_stateful_widget(
            List::new(field_items)
                .block(panel(
                    "Fields: / filter, a show equal",
                    self.focus == Focus::Fields,
                ))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            panels[0],
            &mut self.list_state,
        );
        let title = self
            .field()
            .map(|field| diff::visible(&field.path))
            .unwrap_or("No visible fields; press a or clear / filter".into());
        let block = panel(&title, self.focus == Focus::Text);
        let inner = block.inner(panels[1]);
        frame.render_widget(block, panels[1]);
        let sides = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        self.page = sides[0].height.saturating_sub(2).max(1) as usize;
        for (expected, area) in [(true, sides[0]), (false, sides[1])] {
            let lines: Vec<_> = self
                .displayed
                .iter()
                .skip(self.scroll)
                .take(self.page)
                .map(|index| {
                    let Some(index) = index else {
                        return Line::from(Span::styled(
                            "... unchanged lines folded ...",
                            Style::default().fg(Color::DarkGray),
                        ));
                    };
                    let row = &self.text.rows[*index];
                    let (number, segments) = if expected {
                        (row.expected_number, &row.expected)
                    } else {
                        (row.actual_number, &row.actual)
                    };
                    let mut spans = vec![Span::styled(
                        number.map_or("   - ".into(), |n| format!("{n:>4} ")),
                        Style::default().fg(Color::DarkGray),
                    )];
                    if number.is_none() {
                        spans.push(Span::styled(
                            "<no line>",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    for segment in segments {
                        let style = if segment.changed {
                            Style::default()
                                .fg(if expected { Color::Red } else { Color::Green })
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(if row.changed {
                                Color::White
                            } else {
                                Color::DarkGray
                            })
                        };
                        spans.push(Span::styled(segment.text.clone(), style));
                    }
                    if number.is_some() && segments.iter().all(|s| s.text.is_empty()) {
                        spans.push(Span::styled(
                            "<empty>",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).scroll((0, self.horizontal)).block(
                    Block::default().borders(Borders::ALL).title(format!(
                        "{} ({})",
                        if expected { "Expected" } else { "Actual" },
                        self.field()
                            .map(|field| field.kind(expected))
                            .unwrap_or("none")
                    )),
                ),
                area,
            );
        }
        let footer = if self.searching {
            format!(
                "Filter path: /{}   Enter accept | Esc clear",
                diff::visible(&self.search)
            )
        } else {
            "Tab focus | arrows/jk move | n/N difference | m normalizers | a equal | c context | r reload | ? help | q quit".into()
        };
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), chunks[4]);
        if self.help {
            draw_help(frame, area);
        }
    }
}

fn panel(title: &str, active: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }))
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = Rect::new(
        area.x + 2,
        area.y + 2,
        area.width.saturating_sub(4),
        area.height.saturating_sub(4),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(
        "Offline viewer. No agent, provider, container or replay is started.\n\n[ / ]       Previous / next retained mismatch case\nTab         Switch fields / text focus (cyan border)\nUp/Down jk  Select field or scroll text\nPgUp/PgDn   Scroll both text panes together\nHome/End    First/last field or text page\nh/l arrows  Horizontal text scrolling\nn / N       Next / previous differing field\na           Show/hide equal fields\nc           Expand/fold unchanged text (three context lines)\n/           Filter field paths; Enter accepts, Esc clears\nr           Reload this input from disk\nm           Manage project normalizers\nq / Esc     Quit; Ctrl-C also restores the terminal\n\nRed = expected changed characters. Green = actual changed characters.\nGray = matching context. ? = redacted/unknown, not proven equal.\n\nComparisons describe normalized/redacted evidence, not original raw requests.\nOnly the first captured mismatch per proxy is available. Snapshot files alone\ndo not establish the complete request timeline or all retry contents.\n\nPress any key to close help.")
        .wrap(Wrap { trim:false }).block(Block::default().borders(Borders::ALL).title("Help")),popup);
}

pub(crate) struct ScreenGuard;
impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    }
}

pub(crate) fn start_terminal() -> io::Result<(ScreenGuard, Terminal<CrosstermBackend<io::Stdout>>)>
{
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive mode needs a terminal; use --print for redirected output",
        ));
    }
    terminal::enable_raw_mode()?;
    let guard = ScreenGuard;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    Ok((guard, terminal))
}

pub(crate) fn run(app: &mut App) -> io::Result<()> {
    let (mut guard, mut terminal) = start_terminal()?;
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
        {
            if app.key(key) {
                return Ok(());
            }
            if app.manage_normalizers {
                app.manage_normalizers = false;
                let Some(project) = app.project.clone() else {
                    app.error = Some("No ASB project found; reopen with --project PATH".into());
                    continue;
                };
                let examples =
                    app.field()
                        .and_then(|field| match (&field.expected, &field.actual) {
                            (
                                Some(serde_json::Value::String(a)),
                                Some(serde_json::Value::String(b)),
                            ) if field.status == Status::Changed => Some((a.clone(), b.clone())),
                            _ => None,
                        });
                drop(terminal);
                drop(guard);
                let result = crate::normalizers::run(&project, app.workload.clone(), examples);
                (guard, terminal) = start_terminal()?;
                if let Err(error) = result {
                    app.error = Some(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    #[test]
    fn renders_failure_and_keeps_selected_difference_when_showing_equal_fields() {
        let fixture = Fixture::new();
        fixture.write("batch-1/example.mismatch.json", &json!({
            "schema_version":1,"mismatch":{"sequence":18,"path":"$.input[50].output","category":"json_value"},
            "expected":{"method":"POST","url":"/responses","body":{"input":[{"output":"Ran 6 tests in 0.001s\n\nOK"}],"max_output_tokens":"[redacted]"}},
            "actual":{"method":"POST","url":"/responses","body":{"input":[{"output":"Ran 6 tests in 0.002s\n\nOK"}],"max_output_tokens":"[redacted]"}}
        }).to_string());
        let mut app = App::new(ArtifactSet::open(&fixture.path).unwrap(), None, false).unwrap();
        let selected = app.field().unwrap().path.clone();
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.field().unwrap().path, selected);
        let mut terminal = Terminal::new(TestBackend::new(140, 35)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Normalized/redacted snapshots"));
        assert!(screen.contains("0.001s") && screen.contains("0.002s"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "1" && cell.fg == Color::Red)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "2" && cell.fg == Color::Green)
        );
        let before = std::fs::read_dir(fixture.path.join("batch-1"))
            .unwrap()
            .count();
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app.error.is_none());
        assert_eq!(
            before,
            std::fs::read_dir(fixture.path.join("batch-1"))
                .unwrap()
                .count()
        );
    }
}
