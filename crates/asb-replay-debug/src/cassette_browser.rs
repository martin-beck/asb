//! Cassette navigation using ASB's own cassette source reader.
use crate::{app, diff};
use asb_cli::cassette::{Limits, Source};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

const TRACK_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EVENTS: usize = 50_000;
const MAX_EVENT_CHARS: usize = 200_000;

fn outcome(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "pass",
        Some(false) => "fail",
        None => "unknown",
    }
}

fn decode_entries(bytes: &[u8], stream: &Stream, id: u64) -> Result<Vec<Value>, String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) if matches!(stream, Stream::Artifact(_)) => {
            let preview: String = bytes
                .iter()
                .take(65_536)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            return Ok(vec![
                serde_json::json!({"kind":"binary artifact", "bytes":bytes.len(), "hex_preview":preview, "preview_truncated":bytes.len() > 65_536}),
            ]);
        }
        Err(_) => return Err(format!("track {id} is not UTF-8 JSONL")),
    };
    if matches!(stream, Stream::Artifact(_))
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        return Ok(vec![value]);
    }
    let mut events = Vec::new();
    for (line, value) in text.lines().enumerate() {
        if value.trim().is_empty() {
            continue;
        }
        if events.len() == MAX_EVENTS {
            return Err(format!("track {id} exceeds {MAX_EVENTS} entries"));
        }
        match serde_json::from_str(value) {
            Ok(event) => events.push(event),
            Err(_) if matches!(stream, Stream::Artifact(_)) => {
                return Ok(vec![Value::String(text.to_owned())]);
            }
            Err(e) => return Err(format!("invalid track {id} JSON on line {}: {e}", line + 1)),
        }
    }
    Ok(events)
}

struct Track {
    id: u64,
    label: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stream {
    Provider,
    Native,
    Artifact(String),
}
impl Stream {
    fn label(&self) -> String {
        match self {
            Self::Provider => "provider".into(),
            Self::Native => "native tools".into(),
            Self::Artifact(kind) => format!("artifact: {kind}"),
        }
    }
}
struct Browser {
    source: Source,
    tracks: Vec<Track>,
    selected: usize,
    stream: Stream,
    events: Vec<Value>,
    event: usize,
    scroll: u16,
    horizontal: u16,
    wrap: bool,
    focus_tracks: bool,
}

impl Browser {
    fn load(path: &Path, requested: Option<u64>, stream: Stream) -> Result<Self, String> {
        let source = Source::open(path, Limits::default())
            .map_err(|e| format!("cannot open cassette {}: {e}", path.display()))?;
        let tracks: Vec<Track> = if let Some(manifest) = source.manifest() {
            manifest
                .tracks
                .iter()
                .map(|track| Track {
                    id: track.id,
                    label: format!(
                        "#{}  batch {} replica {}  {}  execution={} grade={}",
                        track.id,
                        track.batch,
                        track.replica,
                        if track.complete {
                            "complete"
                        } else {
                            "incomplete"
                        },
                        outcome(track.process_success),
                        outcome(track.workload_success)
                    ),
                })
                .collect()
        } else {
            source
                .legacy_tracks()
                .iter()
                .map(|track| Track {
                    id: track.id,
                    label: format!("#{}  legacy (completion unknown)", track.id),
                })
                .collect()
        };
        if tracks.is_empty() {
            return Err("cassette has no tracks".into());
        }
        let selected = match requested {
            Some(id) => tracks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(|| format!("cassette has no track {id}"))?,
            None => 0,
        };
        let mut browser = Self {
            source,
            tracks,
            selected,
            stream: Stream::Provider,
            events: Vec::new(),
            event: 0,
            scroll: 0,
            horizontal: 0,
            wrap: true,
            focus_tracks: true,
        };
        browser.select_stream(selected, stream)?;
        Ok(browser)
    }
    fn select_track(&mut self, index: usize) -> Result<(), String> {
        self.select_stream(index, Stream::Provider)
    }
    fn streams(&self, index: usize) -> Vec<Stream> {
        let id = self.tracks[index].id;
        let mut streams = vec![Stream::Provider];
        if let Some(track) = self
            .source
            .manifest()
            .and_then(|manifest| manifest.tracks.iter().find(|track| track.id == id))
        {
            if track.native.is_some() {
                streams.push(Stream::Native);
            }
            streams.extend(
                track
                    .artifacts
                    .iter()
                    .map(|artifact| Stream::Artifact(artifact.kind.clone())),
            );
        }
        streams
    }
    fn select_stream(&mut self, index: usize, stream: Stream) -> Result<(), String> {
        let id = self.tracks[index].id;
        let track = self
            .source
            .manifest()
            .and_then(|manifest| manifest.tracks.iter().find(|track| track.id == id));
        let size = match &stream {
            Stream::Provider => track.map(|track| track.bytes).or_else(|| {
                self.source
                    .legacy_tracks()
                    .iter()
                    .find(|track| track.id == id)
                    .map(|track| track.bytes)
            }),
            Stream::Native => {
                track.and_then(|track| track.native.as_ref().map(|native| native.bytes))
            }
            Stream::Artifact(kind) => track.and_then(|track| {
                track
                    .artifacts
                    .iter()
                    .find(|artifact| &artifact.kind == kind)
                    .map(|artifact| artifact.bytes)
            }),
        };
        let bytes = size.ok_or_else(|| format!("track {id} has no {} stream", stream.label()))?;
        if bytes > TRACK_BYTES {
            return Err(format!("track {id} exceeds the 32 MiB browser limit"));
        }
        let bytes = match &stream {
            Stream::Provider => self.source.read_track(id),
            Stream::Native => self.source.read_native(id),
            Stream::Artifact(kind) => self.source.read_artifact(id, kind),
        }
        .map_err(|e| format!("cannot read {} from track {id}: {e}", stream.label()))?;
        let events = decode_entries(&bytes, &stream, id)?;
        self.selected = index;
        self.stream = stream;
        self.events = events;
        self.event = 0;
        self.scroll = 0;
        self.horizontal = 0;
        Ok(())
    }
    fn event_text(&self) -> String {
        let Some(event) = self.events.get(self.event) else {
            return "No recorded interactions in this track.".into();
        };
        let raw = serde_json::to_string_pretty(event).unwrap_or_default();
        let mut chars = raw.chars();
        let text: String = chars.by_ref().take(MAX_EVENT_CHARS).collect();
        let suffix = if chars.next().is_some() {
            "\n[preview truncated at 200000 characters]"
        } else {
            ""
        };
        format!(
            "{}{}",
            text.lines()
                .map(diff::visible)
                .collect::<Vec<_>>()
                .join("\n"),
            suffix
        )
    }
    fn view_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                self.horizontal = 0;
            }
            KeyCode::Left if !self.wrap => self.horizontal = self.horizontal.saturating_sub(4),
            KeyCode::Right if !self.wrap => self.horizontal = self.horizontal.saturating_add(4),
            _ => {}
        }
    }
    fn print(&self, mut out: impl Write) -> io::Result<()> {
        writeln!(
            out,
            "{}; {}; {} recorded entries",
            diff::visible(&self.tracks[self.selected].label),
            self.stream.label(),
            self.events.len()
        )?;
        for event in &self.events {
            let line = serde_json::to_string(event).map_err(io::Error::other)?;
            writeln!(out, "{}", diff::visible(&line))?;
        }
        Ok(())
    }
}

/// Color only JSON string values of a `stderr` field, after safe rendering.
fn highlight_stderr(text: &str) -> Text<'static> {
    Text::from(
        text.lines()
            .map(|line| {
                let trimmed = line.trim_start();
                let Some(value) = trimmed.strip_prefix("\"stderr\": ") else {
                    return Line::from(line.to_owned());
                };
                let raw = value.strip_suffix(',').unwrap_or(value);
                if serde_json::from_str::<String>(raw).is_ok_and(|value| !value.is_empty()) {
                    let prefix_len = line.len() - value.len();
                    Line::from(vec![
                        Span::raw(line[..prefix_len].to_owned()),
                        Span::styled(value.to_owned(), Style::default().fg(Color::Red)),
                    ])
                } else {
                    Line::from(line.to_owned())
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn detail_widget(text: &str, scroll: u16, horizontal: u16, wrap: bool) -> Paragraph<'static> {
    let mut details = Paragraph::new(highlight_stderr(text))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Selected interaction"),
        )
        .scroll((scroll, if wrap { 0 } else { horizontal }));
    if wrap {
        details = details.wrap(Wrap { trim: false });
    }
    details
}

pub(crate) fn open(
    path: &Path,
    track: Option<u64>,
    native: bool,
    artifact: Option<&str>,
    print: bool,
    project: Option<&Path>,
    workload: Option<&str>,
) -> Result<(), String> {
    let stream = if native {
        Stream::Native
    } else if let Some(kind) = artifact {
        Stream::Artifact(kind.to_owned())
    } else {
        Stream::Provider
    };
    let mut browser = Browser::load(path, track, stream)?;
    if print {
        return browser
            .print(io::stdout().lock())
            .map_err(|e| e.to_string());
    }
    let project = project
        .map(Path::to_owned)
        .or_else(|| crate::normalizers::find_project(path));
    let workload = workload.map(str::to_owned).or_else(|| {
        browser
            .source
            .manifest()
            .map(|m| m.compatibility.workload.clone())
    });
    let (mut guard, mut terminal) = app::start_terminal().map_err(|e| e.to_string())?;
    let mut error = None::<String>;
    loop {
        terminal
            .draw(|frame| {
                let outer = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Min(3),
                    Constraint::Length(3),
                ])
                .split(frame.area());
                let main =
                    Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
                        .split(outer[1]);
                let right =
                    Layout::vertical([Constraint::Percentage(35), Constraint::Percentage(65)])
                        .split(main[1]);
                frame.render_widget(
                    Paragraph::new(
                        "Cassette browser — recorded content may contain sensitive data",
                    )
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("ASB replay debugger"),
                    ),
                    outer[0],
                );
                let mut track_state = ListState::default();
                track_state.select(Some(browser.selected));
                let tracks = browser
                    .tracks
                    .iter()
                    .map(|t| ListItem::new(diff::visible(&t.label)))
                    .collect::<Vec<_>>();
                frame.render_stateful_widget(
                    List::new(tracks)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Tracks")
                                .border_style(Style::default().fg(if browser.focus_tracks {
                                    Color::Cyan
                                } else {
                                    Color::DarkGray
                                })),
                        )
                        .highlight_style(Style::default().fg(Color::Cyan))
                        .highlight_symbol("> "),
                    main[0],
                    &mut track_state,
                );
                let mut event_state = ListState::default();
                if !browser.events.is_empty() {
                    event_state.select(Some(browser.event));
                }
                let events = browser
                    .events
                    .iter()
                    .enumerate()
                    .map(|(i, event)| {
                        let method = event
                            .pointer("/request/method")
                            .and_then(Value::as_str)
                            .unwrap_or("event");
                        ListItem::new(Line::from(format!("{}  {}", i + 1, diff::visible(method))))
                    })
                    .collect::<Vec<_>>();
                frame.render_stateful_widget(
                    List::new(events)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!("{} entries", browser.stream.label()))
                                .border_style(Style::default().fg(if browser.focus_tracks {
                                    Color::DarkGray
                                } else {
                                    Color::Cyan
                                })),
                        )
                        .highlight_style(Style::default().fg(Color::Cyan))
                        .highlight_symbol("> "),
                    right[0],
                    &mut event_state,
                );
                frame.render_widget(detail_widget(&browser.event_text(), browser.scroll, browser.horizontal, browser.wrap), right[1]);
                let footer = error.as_deref().unwrap_or(
                    "Tab: focus  Up/Down: select  s: stream  m: normalizers  w: wrap  Left/Right: scroll  PgUp/PgDn: page  q: quit",
                );
                frame.render_widget(
                    Paragraph::new(diff::visible(footer))
                        .block(Block::default().borders(Borders::ALL)),
                    outer[2],
                );
            })
            .map_err(|e| e.to_string())?;
        if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('q')
            || key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('m') => {
                let Some(root) = project.as_ref() else {
                    error = Some("No ASB project found; reopen with --project PATH".into());
                    continue;
                };
                let sample = browser
                    .events
                    .get(browser.event)
                    .and_then(crate::normalizers::tool_output_example)
                    .map(|s| (s.clone(), s));
                drop(terminal);
                drop(guard);
                let result = crate::normalizers::run(root, workload.clone(), sample);
                (guard, terminal) = app::start_terminal().map_err(|e| e.to_string())?;
                error = result.err();
            }
            KeyCode::Tab => browser.focus_tracks = !browser.focus_tracks,
            KeyCode::Char('s') => {
                let streams = browser.streams(browser.selected);
                let position = streams
                    .iter()
                    .position(|stream| stream == &browser.stream)
                    .unwrap_or(0);
                let next = streams[(position + 1) % streams.len()].clone();
                error = browser.select_stream(browser.selected, next).err();
            }
            KeyCode::Up | KeyCode::Char('k') if browser.focus_tracks => {
                let next = browser.selected.saturating_sub(1);
                if next != browser.selected {
                    error = browser.select_track(next).err();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if browser.focus_tracks => {
                let next = (browser.selected + 1).min(browser.tracks.len() - 1);
                if next != browser.selected {
                    error = browser.select_track(next).err();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                browser.event = browser.event.saturating_sub(1);
                browser.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                browser.event = (browser.event + 1).min(browser.events.len().saturating_sub(1));
                browser.scroll = 0;
            }
            KeyCode::PageUp => browser.scroll = browser.scroll.saturating_sub(10),
            KeyCode::PageDown => browser.scroll = browser.scroll.saturating_add(10),
            KeyCode::Char('w') | KeyCode::Left | KeyCode::Right => browser.view_key(key.code),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use asb_cli::cassette::{
        Artifact, Compatibility, Manifest, NativeTrack, SCHEMA_VERSION, Statistics, Track, digest,
    };
    use std::{fs::File, io::Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn highlights_only_nonempty_string_stderr_values_including_nested_fields() {
        let value = serde_json::json!({"stderr":"top\u{1b}[31m", "nested":{"stderr":"failure"}, "empty":{"stderr":""}, "number":{"stderr":4}, "unrelated":"failure"});
        let safe = serde_json::to_string_pretty(&value)
            .unwrap()
            .lines()
            .map(diff::visible)
            .collect::<Vec<_>>()
            .join("\n");
        let styled = highlight_stderr(&safe);
        let red = styled
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg == Some(Color::Red))
            .map(|span| span.content.to_string())
            .collect::<Vec<_>>();
        assert_eq!(red.len(), 2);
        assert!(red.iter().any(|value| value.contains("failure")));
        assert!(red.iter().any(|value| value.contains("top")));
        assert!(red.iter().all(|value| !value.contains('\u{1b}')));
    }

    #[test]
    fn narrow_detail_view_wraps_and_scrolls_without_terminal_controls() {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(16, 6)).unwrap();
        let draw = |terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
                    scroll,
                    horizontal,
                    wrap| {
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        detail_widget("abcdefghijklmnopqrstuvwxyz", scroll, horizontal, wrap),
                        frame.area(),
                    )
                })
                .unwrap();
            (0..6)
                .map(|y| {
                    (0..16)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let wrapped = draw(&mut terminal, 0, 0, true);
        assert!(wrapped[1].contains("abcdefghijklmn"));
        assert!(wrapped[2].contains("opqrstuvwxyz"));
        let scrolled = draw(&mut terminal, 1, 0, true);
        assert!(scrolled[1].contains("opqrstuvwxyz"));
        let horizontal = draw(&mut terminal, 0, 4, false);
        assert!(horizontal[1].contains("efghijklmnopqr"));
        assert!(!horizontal[2].contains("stuvwxyz"));
    }

    #[test]
    fn browses_multitrack_bundle_using_shared_reader_and_preserves_archive() {
        let fixture = Fixture::new();
        let path = fixture.path.join("fixture.asb.zip");
        let payloads: [&[u8]; 2] = [
            b"{\"request\":{\"method\":\"POST\"},\"response\":\"one\"}\n",
            b"{\"response\":\"two\"}\n",
        ];
        let native = b"{\"kind\":\"tool\"}\n";
        let notes = b"{\"note\":\"fixture\"}\n";
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            recording_id: "fixture".into(),
            concurrency: 1,
            provenance: None,
            compatibility: Compatibility {
                agent: "agent".into(),
                provider: "provider".into(),
                model: "model".into(),
                workload: "workload".into(),
                task: "task".into(),
                dataset_sha256: None,
                prompt_sha256: digest(b"prompt"),
                material_sha256: digest(b"material"),
                protocol_sha256: None,
                normalization_sha256: None,
            },
            tracks: payloads
                .iter()
                .enumerate()
                .map(|(i, bytes)| Track {
                    recorder: None,
                    recorded_max_steps: None,
                    id: i as u64 + 1,
                    batch: i as u64 + 1,
                    replica: 1,
                    complete: true,
                    process_success: Some(true),
                    workload_success: Some(true),
                    bytes: bytes.len() as u64,
                    sha256: digest(bytes),
                    statistics: Statistics::default(),
                    native: (i == 0).then(|| NativeTrack {
                        complete: true,
                        events: 1,
                        bytes: native.len() as u64,
                        sha256: digest(native),
                    }),
                    artifacts: if i == 0 {
                        vec![Artifact {
                            kind: "notes".into(),
                            bytes: notes.len() as u64,
                            sha256: digest(notes),
                        }]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        };
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default();
        zip.start_file("manifest.json", options).unwrap();
        zip.write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        for (track, bytes) in manifest.tracks.iter().zip(payloads) {
            zip.start_file(track.entry_name(), options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.start_file(manifest.tracks[0].native_entry_name(), options)
            .unwrap();
        zip.write_all(native).unwrap();
        zip.start_file(manifest.tracks[0].artifact_entry_name("notes"), options)
            .unwrap();
        zip.write_all(notes).unwrap();
        zip.finish().unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut browser = Browser::load(&path, Some(2), Stream::Provider).unwrap();
        assert!(browser.wrap);
        assert_eq!(browser.horizontal, 0);
        browser.view_key(KeyCode::Right);
        assert_eq!(browser.horizontal, 0);
        browser.view_key(KeyCode::Char('w'));
        browser.view_key(KeyCode::Right);
        assert_eq!(browser.horizontal, 4);
        browser.view_key(KeyCode::Left);
        assert_eq!(browser.horizontal, 0);
        browser.view_key(KeyCode::Char('w'));
        assert!(browser.wrap);
        assert_eq!(browser.events[0]["response"], "two");
        browser.select_track(0).unwrap();
        assert_eq!(browser.events[0]["response"], "one");
        browser.select_stream(0, Stream::Native).unwrap();
        assert_eq!(browser.events[0]["kind"], "tool");
        browser
            .select_stream(0, Stream::Artifact("notes".into()))
            .unwrap();
        assert_eq!(browser.events[0]["note"], "fixture");
        browser.select_track(0).unwrap();
        let mut output = Vec::new();
        browser.print(&mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("one"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(
            Browser::load(&path, Some(3), Stream::Provider)
                .err()
                .unwrap()
                .contains("no track 3")
        );
        let tampered = fixture.path.join("tampered.asb.zip");
        let mut zip = ZipWriter::new(File::create(&tampered).unwrap());
        zip.start_file("manifest.json", options).unwrap();
        zip.write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        for (index, (track, bytes)) in manifest.tracks.iter().zip(payloads).enumerate() {
            zip.start_file(track.entry_name(), options).unwrap();
            if index == 0 {
                let mut changed = bytes.to_vec();
                changed[0] ^= 1;
                zip.write_all(&changed).unwrap();
            } else {
                zip.write_all(bytes).unwrap();
            }
        }
        zip.start_file(manifest.tracks[0].native_entry_name(), options)
            .unwrap();
        zip.write_all(native).unwrap();
        zip.start_file(manifest.tracks[0].artifact_entry_name("notes"), options)
            .unwrap();
        zip.write_all(notes).unwrap();
        zip.finish().unwrap();
        assert!(
            Browser::load(&tampered, Some(1), Stream::Provider)
                .err()
                .unwrap()
                .contains("checksum")
        );
    }

    #[test]
    fn supports_legacy_jsonl_and_rejects_malformed_events() {
        let fixture = Fixture::new();
        let path = fixture.write("legacy.jsonl", "{\"response\":\"ok\"}\n");
        let browser = Browser::load(&path, None, Stream::Provider).unwrap();
        assert!(browser.tracks[0].label.contains("completion unknown"));
        assert_eq!(browser.events[0]["response"], "ok");
        let bad = fixture.write("broken.jsonl", "{bad}\n");
        assert!(
            Browser::load(&bad, None, Stream::Provider)
                .err()
                .unwrap()
                .contains("invalid track")
        );
    }

    #[test]
    fn artifact_text_and_binary_are_browsable_without_weakening_track_parsing() {
        let artifact = Stream::Artifact("fixture".into());
        assert_eq!(
            decode_entries(b"ordinary text\n", &artifact, 1).unwrap()[0],
            "ordinary text\n"
        );
        let binary = decode_entries(&[0xff, 0x00], &artifact, 1).unwrap();
        assert_eq!(binary[0]["hex_preview"], "ff00");
        assert_eq!(binary[0]["preview_truncated"], false);
        assert!(decode_entries(b"ordinary text\n", &Stream::Provider, 1).is_err());
    }
}
