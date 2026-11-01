//! Interactive, modular setup wizard built with Ratatui.

use crate::benchmark::{BenchmarkConfig, MeasurementConfig};
use crate::{Error, agents, install, measurements, models, project, providers, sweep, workloads};
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
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LAST_SETUP_TUI_FILE: &str = "setup-tui-last.toml";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LastSetupTui {
    schema_version: u32,
    config: String,
}

const DESCRIPTIONS: &[&str] = &[
    "Choose one or more agents that will execute each benchmark.",
    "Choose the provider endpoint used by the selected agents.",
    "Choose tool-capable models available in the selected providers.",
    "Choose one or more workload prompts installed in this project.",
    "Choose the measurement collectors; host tools are installed project-locally when possible.",
    "Choose how many identical executions run in parallel for each generated benchmark.",
];

// U+2139 is commonly rendered as a blue/white emoji. Use the text-style
// circled glyph so the terminal honors the readiness foreground color.
const INFO_GLYPH: &str = "ⓘ";

#[cfg(test)]
#[path = "tui/coverage_tests.rs"]
mod coverage_tests;
#[cfg(test)]
#[path = "tui/identity_tests.rs"]
mod identity_tests;

mod choice;
use choice::{ChoiceKey, PaneKind};
mod workload_state;
use workload_state::{WorkloadAction, WorkloadStep, WorkloadWizard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Availability {
    Available,
    Missing,
    Unknown,
}

#[derive(Clone, Debug)]
struct Choice {
    label: String,
    key: ChoiceKey,
    availability: Availability,
}

#[derive(Clone, Debug)]
struct Pane {
    choices: Vec<Choice>,
    selected: Vec<bool>,
    cursor: usize,
    search: String,
    sections: Vec<Option<String>>,
    // Ratatui adjusts this on each render to keep the cursor visible. Retain
    // it between frames so moving within the viewport does not scroll it.
    viewport_offset: Cell<usize>,
}

impl Pane {
    fn new(choices: Vec<Choice>) -> Self {
        let mut seen = BTreeSet::new();
        let choices = choices
            .into_iter()
            .filter(|choice| choice.key.valid() && seen.insert(choice.key.clone()))
            .collect::<Vec<_>>();
        Self {
            selected: vec![false; choices.len()],
            sections: vec![None; choices.len()],
            choices,
            cursor: 0,
            search: String::new(),
            viewport_offset: Cell::new(0),
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
        if self.visible().is_empty() {
            return;
        }
        if let Some(value) = self.selected.get_mut(self.cursor) {
            *value = !*value;
        }
    }

    fn active_index(&mut self) -> Option<usize> {
        self.search_changed();
        (!self.visible().is_empty() && self.cursor < self.selected.len()).then_some(self.cursor)
    }

    fn toggle_single(&mut self) {
        self.search_changed();
        if self.visible().is_empty() || self.cursor >= self.selected.len() {
            return;
        }
        let enabled = !self.selected[self.cursor];
        self.selected.fill(false);
        self.selected[self.cursor] = enabled;
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
        self.selected_choices()
            .any(|choice| !choice.key.is_control())
    }
}

struct App {
    root: PathBuf,
    project: project::ProjectConfig,
    panes: [Pane; 8],
    step: PaneKind,
    extension_tab: String,
    extension_description: String,
    collector_tab: String,
    collector_description: String,
    configured_collectors: BTreeSet<String>,
    collector_selections: BTreeMap<String, BTreeSet<String>>,
    message: String,
    message_color: Color,
    message_until: Option<std::time::Instant>,
}

impl App {
    fn pane(&self, kind: PaneKind) -> &Pane {
        &self.panes[kind.index()]
    }
    fn pane_mut(&mut self, kind: PaneKind) -> &mut Pane {
        &mut self.panes[kind.index()]
    }

    fn navigation_order(&self) -> Vec<PaneKind> {
        let mut order = vec![
            PaneKind::Agents,
            PaneKind::Providers,
            PaneKind::Models,
            PaneKind::Workloads,
        ];
        if !self.pane(PaneKind::Extensions).choices.is_empty() {
            order.push(PaneKind::Extensions);
        }
        order.push(PaneKind::Measurements);
        if !self.pane(PaneKind::CollectorOptions).choices.is_empty() {
            order.push(PaneKind::CollectorOptions);
        }
        order.push(PaneKind::Concurrency);
        order
    }

    fn step_name(&self, step: PaneKind) -> &str {
        if step == PaneKind::Extensions {
            &self.extension_tab
        } else if step == PaneKind::CollectorOptions {
            &self.collector_tab
        } else {
            step.name()
        }
    }

    fn step_description(&self) -> &str {
        if self.step == PaneKind::Extensions {
            &self.extension_description
        } else if self.step == PaneKind::CollectorOptions {
            &self.collector_description
        } else {
            self.step.description()
        }
    }

    fn next_step(&self) -> Option<PaneKind> {
        let order = self.navigation_order();
        order
            .iter()
            .position(|step| *step == self.step)
            .and_then(|index| order.get(index + 1).copied())
    }

    fn previous_step(&self) -> Option<PaneKind> {
        let order = self.navigation_order();
        order
            .iter()
            .position(|step| *step == self.step)
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| order.get(index).copied())
    }

    fn new(root: PathBuf, project: project::ProjectConfig) -> Self {
        let agents = std::iter::once(Choice {
            label: "[all]".into(),
            key: ChoiceKey::AllAgents,
            availability: Availability::Available,
        })
        .chain(agents::names().map(|name| Choice {
            label: (*name).into(),
            key: ChoiceKey::Agent((*name).into()),
            availability: Availability::Unknown,
        }))
        .collect();
        let provider_choices = providers::names()
            .iter()
            .copied()
            .chain(std::iter::once("local"))
            .map(|name| Choice {
                label: name.into(),
                key: ChoiceKey::Provider(name.into()),
                availability: Availability::Unknown,
            })
            .collect();
        let workloads = std::iter::once(Choice {
            label: "[all]".into(),
            key: ChoiceKey::AllWorkloads,
            availability: Availability::Available,
        })
        .chain(workload_choices(&root, &project.workloads_dir))
        .collect();
        let mut workload_pane = Pane::new(workloads);
        let external_names = install::external_workload_names()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for (index, choice) in workload_pane.choices.iter().enumerate() {
            let Some(name) = choice.key.workload() else {
                continue;
            };
            workload_pane.sections[index] = Some(
                if external_names.contains(name) {
                    "External workloads"
                } else {
                    "Built-in workloads"
                }
                .into(),
            );
        }
        let measures = std::iter::once(Choice {
            label: "[all]".into(),
            key: ChoiceKey::AllCollectors,
            availability: Availability::Available,
        })
        .chain(
            measurements::available_collectors()
                .into_iter()
                .map(|name| Choice {
                    key: ChoiceKey::Collector(name.clone()),
                    label: name,
                    availability: Availability::Available,
                }),
        )
        .collect();
        let measure_pane = Pane::new(measures);
        let concurrency_choices = (1..=crate::benchmark::MAX_CONCURRENCY)
            .map(|value| Choice {
                label: value.to_string(),
                key: ChoiceKey::Concurrency(value),
                availability: Availability::Available,
            })
            .collect();
        let mut concurrency_pane = Pane::new(concurrency_choices);
        concurrency_pane.selected[0] = true;
        let mut app = Self {
            root,
            project,
            panes: [
                Pane::new(agents),
                Pane::new(provider_choices),
                Pane::new(Vec::new()),
                workload_pane,
                measure_pane,
                concurrency_pane,
                Pane::new(Vec::new()),
                Pane::new(Vec::new()),
            ],
            step: PaneKind::Agents,
            extension_tab: "workload options".into(),
            extension_description: PaneKind::Extensions.description().into(),
            collector_tab: "collector options".into(),
            collector_description: PaneKind::CollectorOptions.description().into(),
            configured_collectors: BTreeSet::new(),
            collector_selections: BTreeMap::new(),
            message: "Space selects; arrows move; Tab or Enter advances; Esc cancels.".into(),
            message_color: Color::Yellow,
            message_until: None,
        };
        app.refresh_availability();
        // Missing tools and their dependants are not selected implicitly.
        for index in 0..app.panes[PaneKind::Measurements.index()].choices.len() {
            let choice = &app.panes[PaneKind::Measurements.index()].choices[index];
            let Some(collector) = choice.key.collector() else {
                continue;
            };
            let dependencies = crate::collectors::expand_dependencies(&[collector.to_owned()]);
            app.panes[PaneKind::Measurements.index()].selected[index] =
                dependencies.iter().all(|name| {
                    app.panes[PaneKind::Measurements.index()]
                        .choices
                        .iter()
                        .any(|item| {
                            item.key.collector() == Some(name.as_str())
                                && item.availability == Availability::Available
                        })
                });
        }
        sync_all_sentinel(&mut app.panes[PaneKind::Measurements.index()]);
        app
    }

    /// Restore every wizard selection representable by the supplied benchmark
    /// or sweep TOML. A sweep becomes the TUI's normal selection matrix, so
    /// deliberately paired configurations become editable selections.
    fn preselect(&mut self, input: &Path) -> Result<(), Error> {
        let configs = preselection_configs(input)?;
        let concurrency = configs
            .first()
            .map(|config| config.concurrency)
            .ok_or_else(|| {
                Error::Input(format!(
                    "{0} contains no benchmark configurations",
                    input.display()
                ))
            })?;
        if configs
            .iter()
            .any(|config| config.concurrency != concurrency)
        {
            return Err(Error::Input(format!(
                "{} contains multiple concurrency values; the setup TUI edits one shared concurrency selection",
                input.display()
            )));
        }

        for config in &configs {
            select_choice(
                self.pane_mut(PaneKind::Agents),
                ChoiceKey::Agent(config.agent.clone()),
                input,
            )?;
            select_choice(
                self.pane_mut(PaneKind::Providers),
                ChoiceKey::Provider(config.provider.clone()),
                input,
            )?;
        }
        sync_all_sentinel(self.pane_mut(PaneKind::Agents));

        let model_choices = configs
            .iter()
            .map(|config| Choice {
                label: format!("{}:{}", config.provider, config.model),
                key: ChoiceKey::Model {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                },
                availability: Availability::Unknown,
            })
            .collect();
        self.panes[PaneKind::Models.index()] = Pane::new(model_choices);
        for config in &configs {
            select_choice(
                self.pane_mut(PaneKind::Models),
                ChoiceKey::Model {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                },
                input,
            )?;
        }

        for config in &configs {
            let key = config.trial_set.as_ref().map_or_else(
                || ChoiceKey::Prompt(config.workload.clone()),
                |file| ChoiceKey::TrialSet {
                    workload: config.workload.clone(),
                    file: PathBuf::from(file),
                },
            );
            select_choice(self.pane_mut(PaneKind::Workloads), key, input)?;
        }
        sync_all_sentinel(self.pane_mut(PaneKind::Workloads));
        self.refresh_extensions()?;

        for config in &configs {
            if config.trial_set.is_some() {
                continue;
            }
            let Some(extension) = workloads::setup_extension(&config.workload) else {
                continue;
            };
            let trial = config.trial.as_ref().ok_or_else(|| {
                Error::Input(format!(
                    "{} selects adapter workload `{}` without its required {} value",
                    input.display(),
                    config.workload,
                    extension.tab
                ))
            })?;
            let key = ChoiceKey::ExtensionValue {
                workload: config.workload.clone(),
                extension: extension.id.into(),
                value: trial.clone(),
            };
            let missing = self.pane(PaneKind::Workloads).choices.iter().any(|choice| {
                choice.key.workload() == Some(config.workload.as_str())
                    && choice.availability == Availability::Missing
            });
            if missing {
                restore_extension_choice(self.pane_mut(PaneKind::Extensions), key);
            } else {
                select_choice(self.pane_mut(PaneKind::Extensions), key, input)?;
            }
        }
        for index in 0..self.pane(PaneKind::Extensions).choices.len() {
            if matches!(
                self.pane(PaneKind::Extensions).choices[index].key,
                ChoiceKey::AllExtension { .. }
            ) {
                update_extension_sentinels(self.pane_mut(PaneKind::Extensions), index);
            }
        }

        let mut collector_options = BTreeSet::new();
        for config in &configs {
            if let Some(measurements) = &config.measurements {
                for collector in &measurements.collectors {
                    let adapter =
                        crate::collectors::adapter_for_selection(collector).ok_or_else(|| {
                            Error::Input(format!(
                                "{} selects unknown collector option `{collector}`",
                                input.display()
                            ))
                        })?;
                    select_choice(
                        self.pane_mut(PaneKind::Measurements),
                        ChoiceKey::Collector(adapter.id().into()),
                        input,
                    )?;
                    if adapter.id() != collector {
                        if let Some(extension) = adapter.selection_extension()
                            && extension.all_selection.as_deref() == Some(collector)
                        {
                            collector_options.extend(
                                extension
                                    .options
                                    .into_iter()
                                    .map(|option| (adapter.id().to_owned(), option.selection)),
                            );
                        } else {
                            collector_options.insert((adapter.id().to_owned(), collector.clone()));
                        }
                    }
                }
            }
        }
        self.sync_collector_dependencies();
        self.refresh_collector_options();
        for (collector, selection) in &collector_options {
            if self.configured_collectors.insert(collector.clone()) {
                for (choice, selected) in self.panes[PaneKind::CollectorOptions.index()]
                    .choices
                    .iter()
                    .zip(
                        self.panes[PaneKind::CollectorOptions.index()]
                            .selected
                            .iter_mut(),
                    )
                {
                    if choice
                        .key
                        .collector_option()
                        .is_some_and(|(owner, _)| owner == collector)
                    {
                        *selected = false;
                    }
                }
            }
            select_choice(
                self.pane_mut(PaneKind::CollectorOptions),
                ChoiceKey::CollectorOption {
                    collector: collector.clone(),
                    selection: selection.clone(),
                },
                input,
            )?;
        }
        update_collector_sentinels(self.pane_mut(PaneKind::CollectorOptions));
        select_choice(
            self.pane_mut(PaneKind::Concurrency),
            ChoiceKey::Concurrency(concurrency),
            input,
        )?;
        self.step = PaneKind::Agents;
        self.message = format!(
            "Loaded selections from {}; review or refine them.",
            input.display()
        );
        self.message_color = Color::Yellow;
        self.message_until = None;
        Ok(())
    }

    fn refresh_availability(&mut self) {
        let tools = project::resolve(&self.root, &self.project.tools_dir);
        for choice in &mut self.panes[PaneKind::Agents.index()].choices {
            if choice.key.is_all() {
                continue;
            }
            choice.availability = if install::tool_available(
                choice.key.agent().expect("agent pane identity"),
                &tools,
            ) {
                Availability::Available
            } else {
                Availability::Missing
            };
        }
        let workloads = project::resolve(&self.root, &self.project.workloads_dir);
        for choice in &mut self.panes[PaneKind::Workloads.index()].choices {
            if choice.key.is_all() {
                continue;
            }
            let name = choice.key.workload().expect("workload pane identity");
            let external = install::external_workload_names().any(|external| external == name);
            choice.availability = if install::workload_available(name, &workloads)
                || (!external && workloads::load(name, &workloads).is_ok())
            {
                Availability::Available
            } else {
                Availability::Missing
            };
        }
        for choice in &mut self.panes[PaneKind::Measurements.index()].choices {
            if choice.key.is_all() {
                continue;
            }
            choice.availability = if crate::collectors::find(
                choice.key.collector().expect("collector pane identity"),
            )
            .is_some_and(|adapter| {
                adapter
                    .tool_id()
                    .is_none_or(|tool| install::tool_available(tool, &tools))
            }) {
                Availability::Available
            } else {
                Availability::Missing
            };
        }
    }

    /// Rebuild extension choices exclusively from currently selected
    /// workloads.  State is retained only for an adapter that remains active;
    /// deselecting a workload removes every associated value immediately.
    fn refresh_extensions(&mut self) -> Result<(), Error> {
        let previous = self
            .pane(PaneKind::Extensions)
            .selected_choices()
            .filter_map(|choice| match &choice.key {
                ChoiceKey::ExtensionValue {
                    workload,
                    extension,
                    value,
                } => Some((workload.clone(), extension.clone(), value.clone())),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let directory = project::resolve(&self.root, &self.project.workloads_dir);
        let selected = self
            .pane(PaneKind::Workloads)
            .selected_choices()
            .filter_map(|choice| choice.key.workload().map(str::to_owned))
            .collect::<Vec<_>>();
        let mut choices = Vec::new();
        let mut tabs = BTreeSet::new();
        let mut descriptions = BTreeSet::new();
        for workload in selected {
            let Some(extension) = workloads::setup_extension(&workload) else {
                continue;
            };
            tabs.insert(extension.tab);
            descriptions.insert(extension.description);
            let missing = self.pane(PaneKind::Workloads).choices.iter().any(|choice| {
                choice.key.workload() == Some(workload.as_str())
                    && choice.availability == Availability::Missing
            });
            let values = match workloads::setup_extension_values(&workload, &extension, &directory)
            {
                Ok(values) => values,
                Err(_) if missing => Vec::new(),
                Err(error) => return Err(Error::Input(error)),
            };
            choices.push(Choice {
                label: format!("[all] {workload}"),
                key: ChoiceKey::AllExtension {
                    workload: workload.clone(),
                    extension: extension.id.into(),
                },
                availability: if missing {
                    Availability::Missing
                } else {
                    Availability::Available
                },
            });
            choices.extend(values.into_iter().map(|value| Choice {
                label: value.clone(),
                key: ChoiceKey::ExtensionValue {
                    workload: workload.clone(),
                    extension: extension.id.into(),
                    value,
                },
                availability: if missing {
                    Availability::Missing
                } else {
                    Availability::Available
                },
            }));
        }
        let mut pane = Pane::new(choices);
        for (index, choice) in pane.choices.iter().enumerate() {
            match &choice.key {
                ChoiceKey::AllExtension { workload, .. }
                | ChoiceKey::ExtensionValue { workload, .. } => {
                    pane.sections[index] = Some(format!("{workload} trials"));
                }
                _ => {}
            }
            if let ChoiceKey::ExtensionValue {
                workload,
                extension,
                value,
            } = &choice.key
            {
                pane.selected[index] =
                    previous.contains(&(workload.clone(), extension.clone(), value.clone()));
            }
        }
        for index in 0..pane.choices.len() {
            if matches!(pane.choices[index].key, ChoiceKey::AllExtension { .. }) {
                update_extension_sentinels(&mut pane, index);
            }
        }
        self.panes[PaneKind::Extensions.index()] = pane;
        self.extension_tab = if tabs.len() == 1 {
            tabs.into_iter().next().unwrap_or("workload options").into()
        } else {
            "workload options".into()
        };
        self.extension_description = if descriptions.len() == 1 {
            descriptions
                .into_iter()
                .next()
                .unwrap_or(PaneKind::Extensions.description())
                .into()
        } else if descriptions.is_empty() {
            PaneKind::Extensions.description().into()
        } else {
            "Choose adapter-owned options for every selected workload.".into()
        };
        if self.step == PaneKind::Extensions && self.pane(PaneKind::Extensions).choices.is_empty() {
            self.step = PaneKind::Workloads;
        }
        Ok(())
    }

    fn refresh_collector_options(&mut self) {
        self.refresh_collector_options_with(crate::collectors::all());
    }

    fn refresh_collector_options_with(
        &mut self,
        adapters: &[&dyn crate::collectors::CollectorAdapter],
    ) {
        let pane = self.pane(PaneKind::CollectorOptions);
        let mut previous = BTreeMap::<String, BTreeSet<String>>::new();
        for (choice, enabled) in pane.choices.iter().zip(&pane.selected) {
            if let Some((collector, selection)) = choice.key.collector_option() {
                let values = previous.entry(collector.to_owned()).or_default();
                if *enabled {
                    values.insert(selection.to_owned());
                }
            }
        }
        for (collector, values) in previous {
            if self.configured_collectors.contains(&collector) {
                self.collector_selections.insert(collector, values);
            }
        }
        let selected = self
            .pane(PaneKind::Measurements)
            .selected_choices()
            .filter_map(|choice| choice.key.collector())
            .collect::<BTreeSet<_>>();
        let mut choices = Vec::new();
        let mut defaults = BTreeSet::new();
        let mut tabs = BTreeSet::new();
        let mut descriptions = BTreeSet::new();
        for adapter in adapters {
            if !selected.contains(adapter.id()) {
                continue;
            }
            let Some(extension) = adapter.selection_extension() else {
                continue;
            };
            tabs.insert(extension.tab);
            descriptions.insert(extension.description);
            if extension.all_selection.is_some() {
                choices.push(Choice {
                    label: "[all]".into(),
                    key: ChoiceKey::AllCollectorOptions(adapter.id().into()),
                    availability: Availability::Available,
                });
            }
            for option in extension.options {
                let identity = (adapter.id().to_owned(), option.selection.clone());
                if option.default {
                    defaults.insert(identity);
                }
                choices.push(Choice {
                    label: format!("{} — {}", option.label, option.description),
                    key: ChoiceKey::CollectorOption {
                        collector: adapter.id().into(),
                        selection: option.selection,
                    },
                    availability: Availability::Available,
                });
            }
        }
        let mut pane = Pane::new(choices);
        for (index, choice) in pane.choices.iter().enumerate() {
            if tabs.len() > 1 {
                pane.sections[index] = match &choice.key {
                    ChoiceKey::AllCollectorOptions(collector) => Some(collector.clone()),
                    ChoiceKey::CollectorOption { collector, .. } => Some(collector.clone()),
                    _ => None,
                };
            }
            if let Some((collector, selection)) = choice.key.collector_option() {
                let identity = (collector.to_owned(), selection.to_owned());
                pane.selected[index] = if self.configured_collectors.contains(collector) {
                    self.collector_selections
                        .get(collector)
                        .is_some_and(|values| values.contains(selection))
                } else {
                    defaults.contains(&identity)
                };
            }
        }
        update_collector_sentinels(&mut pane);
        self.collector_tab = if tabs.len() == 1 {
            tabs.into_iter().next().unwrap().into()
        } else {
            "collector options".into()
        };
        self.collector_description = if descriptions.len() == 1 {
            descriptions.into_iter().next().unwrap().into()
        } else {
            PaneKind::CollectorOptions.description().into()
        };
        self.panes[PaneKind::CollectorOptions.index()] = pane;
    }

    fn sync_collector_dependencies(&mut self) -> Vec<String> {
        let selected = self.panes[PaneKind::Measurements.index()]
            .selected_choices()
            .filter_map(|choice| choice.key.collector().map(str::to_owned))
            .collect::<Vec<_>>();
        let expanded = crate::collectors::expand_dependencies(&selected);
        let pane = &mut self.panes[PaneKind::Measurements.index()];
        for (choice, enabled) in pane.choices.iter().zip(pane.selected.iter_mut()) {
            if choice
                .key
                .collector()
                .is_some_and(|id| expanded.iter().any(|name| name == id))
            {
                *enabled = true;
            }
        }
        sync_all_sentinel(pane);
        expanded
    }

    fn refresh_models(&mut self) -> Option<String> {
        self.refresh_models_with(models::discover_for_setup)
    }

    fn refresh_models_with(
        &mut self,
        mut discover: impl FnMut(&str) -> Result<Vec<String>, Error>,
    ) -> Option<String> {
        let selected_providers = self.panes[PaneKind::Providers.index()]
            .selected_choices()
            .filter_map(|choice| choice.key.provider().map(str::to_owned))
            .collect::<Vec<_>>();
        let selected = self.panes[PaneKind::Models.index()]
            .selected_choices()
            .filter_map(|choice| {
                choice
                    .key
                    .model()
                    .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
            })
            .collect::<BTreeSet<_>>();
        let mut choices = Vec::new();
        let mut warnings = Vec::new();
        for provider in &selected_providers {
            let mut is_fallback = false;
            let models = match discover(provider) {
                Ok(models) if models.is_empty() => {
                    warnings.push(format!("{provider}: no tool-capable models returned. Check `asb provider models {provider}`."));
                    Vec::new()
                }
                Ok(models) => models,
                Err(error) => {
                    is_fallback = true;
                    warnings.push(format!("{provider}: lookup failed; fallback only. Run `asb provider models {provider}`. {error}"));
                    providers::known_models(provider)
                        .map(|values| values.iter().map(|value| (*value).to_owned()).collect())
                        .unwrap_or_else(|| vec![default_model(provider).to_owned()])
                }
            };
            for model in models {
                choices.push(Choice {
                    label: format!(
                        "{provider}:{model}{}",
                        if is_fallback { " [fallback]" } else { "" }
                    ),
                    key: ChoiceKey::Model {
                        provider: provider.clone(),
                        model,
                    },
                    availability: Availability::Unknown,
                });
            }
        }
        for (provider, model) in &selected {
            if !selected_providers.contains(provider)
                || choices.iter().any(|choice| {
                    choice
                        .key
                        .model()
                        .is_some_and(|(candidate_provider, candidate_model)| {
                            candidate_provider == provider && candidate_model == model
                        })
                })
            {
                continue;
            }
            choices.push(Choice {
                label: format!("{provider}:{model} [configured]"),
                key: ChoiceKey::Model {
                    provider: provider.clone(),
                    model: model.clone(),
                },
                availability: Availability::Unknown,
            });
        }
        let mut pane = Pane::new(choices);
        for (index, choice) in pane.choices.iter().enumerate() {
            if choice.key.model().is_some_and(|(provider, model)| {
                selected.contains(&(provider.to_owned(), model.to_owned()))
            }) {
                pane.selected[index] = true;
            }
        }
        self.panes[PaneKind::Models.index()] = pane;
        (!warnings.is_empty()).then(|| warnings.join(" | "))
    }

    fn advance(&mut self) -> Result<Option<Generated>, Error> {
        if self.step == PaneKind::CollectorOptions {
            let selected = self
                .pane(PaneKind::Measurements)
                .selected_choices()
                .filter_map(|choice| choice.key.collector())
                .collect::<BTreeSet<_>>();
            if let Some((collector, tab)) = missing_required_collector_options(
                self.pane(PaneKind::CollectorOptions),
                crate::collectors::all(),
                &selected,
            ) {
                self.message = format!("Select at least one {tab} value for {collector}.");
                self.message_color = Color::Yellow;
                return Ok(None);
            }
        }
        let optional_collector_options = self.step == PaneKind::CollectorOptions
            && self
                .pane(PaneKind::Measurements)
                .selected_choices()
                .filter_map(|choice| choice.key.collector())
                .filter_map(crate::collectors::find)
                .filter_map(|adapter| adapter.selection_extension())
                .all(|extension| !extension.required);
        if !self.pane(self.step).has_selection() && !optional_collector_options {
            self.message = "Select at least one item before continuing.".into();
            return Ok(None);
        }
        let model_warning = (self.step == PaneKind::Providers)
            .then(|| self.refresh_models())
            .flatten();
        if self.step == PaneKind::Workloads {
            self.refresh_extensions()?;
        }
        if self.step == PaneKind::Measurements {
            self.refresh_collector_options();
        }
        if let Some(next) = self.next_step() {
            self.step = next;
            self.message = model_warning.unwrap_or_else(|| self.step_description().into());
            self.message_color = Color::Yellow;
            self.message_until = None;
            Ok(None)
        } else {
            generate_configs(self).map(Some)
        }
    }

    fn back(&mut self) {
        if let Some(previous) = self.previous_step() {
            self.step = previous;
            self.message = self.step_description().into();
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
pub fn setup(input: Option<&Path>, last: bool, output: &mut dyn Write) -> Result<(), Error> {
    let (root, project) = project::load(Path::new("."))?;
    let selected = if last {
        Some(last_setup_tui_config(&root)?)
    } else {
        input.map(Path::to_path_buf)
    };
    let mut app = App::new(root.clone(), project);
    if let Some(input) = selected.as_deref() {
        app.preselect(input)?;
    }
    let mut terminal =
        start_terminal().map_err(|error| Error::Io(format!("cannot start setup TUI: {error}")))?;
    let result = run_loop(&mut terminal, &mut app);
    let cleanup = stop_terminal(terminal);
    let generated = with_terminal_cleanup(result, cleanup)?;
    record_last_setup_tui(&root, &generated)?;
    writeln!(output, "{}", generated_summary(&generated)).map_err(crate::io_error)?;
    output.flush().map_err(crate::io_error)
}

fn last_setup_tui_path(root: &Path) -> PathBuf {
    root.join(".asb").join(LAST_SETUP_TUI_FILE)
}

fn last_setup_tui_config(root: &Path) -> Result<PathBuf, Error> {
    let marker = last_setup_tui_path(root);
    let text = fs::read_to_string(&marker).map_err(|error| {
        Error::Input(format!(
            "cannot load the last setup TUI configuration at {}: {error}; run `asb setup tui` first",
            marker.display()
        ))
    })?;
    let record: LastSetupTui = toml::from_str(&text).map_err(|error| {
        Error::Input(format!(
            "last setup TUI record {} is malformed: {error}; run `asb setup tui` again",
            marker.display()
        ))
    })?;
    if record.schema_version != 1 {
        return Err(Error::Input(format!(
            "last setup TUI record {} has unsupported schema version {}; run `asb setup tui` again",
            marker.display(),
            record.schema_version
        )));
    }
    let relative = Path::new(&record.config);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::Input(format!(
            "last setup TUI record {} has an unsafe configuration path; run `asb setup tui` again",
            marker.display()
        )));
    }
    let config = root.join(relative);
    if !config.is_file() {
        return Err(Error::Input(format!(
            "last setup TUI configuration {} is missing; run `asb setup tui` again",
            config.display()
        )));
    }
    Ok(config)
}

fn record_last_setup_tui(root: &Path, generated: &Generated) -> Result<(), Error> {
    let relative = generated.sweep.strip_prefix(root).map_err(|_| {
        Error::Io("setup TUI generated a configuration outside its project root".into())
    })?;
    let record = LastSetupTui {
        schema_version: 1,
        config: relative.to_string_lossy().into_owned(),
    };
    let text = toml::to_string(&record)
        .map_err(|error| Error::Io(format!("cannot serialize last setup TUI record: {error}")))?;
    crate::config::matrix::publish_atomic(&last_setup_tui_path(root), text.as_bytes())
        .map_err(|error| Error::Io(format!("cannot save last setup TUI record: {error}")))
}

fn preselection_configs(input: &Path) -> Result<Vec<BenchmarkConfig>, Error> {
    let text = sweep::read_manifest_text(input)?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| Error::Input(format!("cannot parse TOML {}: {error}", input.display())))?;
    if value
        .as_table()
        .is_some_and(|table| table.contains_key("configs"))
    {
        let manifest = sweep::parse_manifest(input, value)?;
        let parent = input.parent().unwrap_or_else(|| Path::new("."));
        return manifest
            .configs
            .into_iter()
            .map(|reference| {
                let path = parent.join(reference);
                let text = crate::read_config(&path)?;
                toml::from_str(&text).map_err(|error| {
                    Error::Input(format!(
                        "cannot parse benchmark configuration {}: {error}",
                        path.display()
                    ))
                })
            })
            .collect();
    }
    let text = crate::read_config(input)?;
    toml::from_str(&text)
        .map(|config| vec![config])
        .map_err(|error| {
            Error::Input(format!(
                "cannot parse benchmark configuration {}: {error}",
                input.display()
            ))
        })
}

fn select_choice(pane: &mut Pane, key: ChoiceKey, input: &Path) -> Result<(), Error> {
    let index = pane.choices.iter().position(|choice| choice.key == key).ok_or_else(|| {
        Error::Input(format!(
            "{} selects unavailable {} `{}`; install or register it before reopening the setup TUI",
            input.display(),
            match &key {
                ChoiceKey::Agent(_) => "agent",
                ChoiceKey::Provider(_) => "provider",
                ChoiceKey::Model { .. } => "model",
                ChoiceKey::Prompt(_) | ChoiceKey::TrialSet { .. } => "workload",
                ChoiceKey::Collector(_) => "collector",
                ChoiceKey::CollectorOption { .. } => "collector option",
                ChoiceKey::Concurrency(_) => "concurrency",
                ChoiceKey::AllAgents
                | ChoiceKey::AllWorkloads
                | ChoiceKey::AllCollectors
                | ChoiceKey::AllExtension { .. }
                | ChoiceKey::AllCollectorOptions(_)
                | ChoiceKey::ExtensionValue { .. }
                | ChoiceKey::Trial(_) => "selection",
            },
            choice_key_label(&key)
        ))
    })?;
    pane.selected[index] = true;
    Ok(())
}

/// Restore a saved extension value even when its external workload is not
/// installed yet.  The missing choice keeps the persisted configuration
/// editable and directs the user to install the owning workload from its tab.
fn restore_extension_choice(pane: &mut Pane, key: ChoiceKey) {
    if let Some(index) = pane.choices.iter().position(|choice| choice.key == key) {
        pane.selected[index] = true;
        return;
    }
    let ChoiceKey::ExtensionValue {
        workload,
        extension,
        value,
    } = &key
    else {
        unreachable!("only extension values are restored as unavailable");
    };
    let label = value.clone();
    let section = format!("{workload} {extension}s");
    pane.choices.push(Choice {
        label,
        key,
        availability: Availability::Missing,
    });
    pane.selected.push(true);
    pane.sections.push(Some(section));
}

fn choice_key_label(key: &ChoiceKey) -> String {
    match key {
        ChoiceKey::Agent(value)
        | ChoiceKey::Provider(value)
        | ChoiceKey::Prompt(value)
        | ChoiceKey::Collector(value)
        | ChoiceKey::Trial(value) => value.clone(),
        ChoiceKey::CollectorOption { selection, .. } => selection.clone(),
        ChoiceKey::Model { provider, model } => format!("{provider}:{model}"),
        ChoiceKey::TrialSet { workload, file } => format!("{}:{}", workload, file.display()),
        ChoiceKey::Concurrency(value) => value.to_string(),
        ChoiceKey::AllAgents | ChoiceKey::AllWorkloads | ChoiceKey::AllCollectors => "[all]".into(),
        ChoiceKey::AllCollectorOptions(collector) => format!("{collector}: [all]"),
        ChoiceKey::AllExtension {
            workload,
            extension,
        } => format!("[all] {workload}:{extension}"),
        ChoiceKey::ExtensionValue {
            workload,
            extension,
            value,
        } => format!("{workload}:{extension}:{value}"),
    }
}

fn sync_all_sentinel(pane: &mut Pane) {
    let Some(all) = pane.choices.iter().position(|choice| choice.key.is_all()) else {
        return;
    };
    pane.selected[all] = pane
        .choices
        .iter()
        .zip(pane.selected.iter())
        .filter(|(choice, _)| !choice.key.is_all())
        .all(|(_, selected)| *selected);
}

/// Launch the tree-shaped workload/trial selector. It reuses the setup TUI's
/// Pane, terminal lifecycle, search, selection, and keyboard conventions.
pub fn workload_tui(output: &mut dyn Write) -> Result<(), Error> {
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
        wizard: WorkloadWizard::default(),
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
    let count = with_terminal_cleanup(result, cleanup)?;
    writeln!(output, "Wrote {count} workload trial-set configurations").map_err(crate::io_error)
}

struct WorkloadApp {
    root: PathBuf,
    project: project::ProjectConfig,
    workloads: Pane,
    wizard: WorkloadWizard,
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
                key: ChoiceKey::Prompt(label.clone()),
                label,
                availability,
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|choice| {
        let name = choice.key.workload().expect("workload pane identity");
        (!external_names.contains(name), choice.label.clone())
    });
    choices
}

fn refresh_workload_availability(app: &mut WorkloadApp) {
    let directory = project::resolve(&app.root, &app.project.workloads_dir);
    for choice in &mut app.workloads.choices {
        let name = choice.key.workload().expect("workload pane identity");
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
            key if is_install_key(&key) => {
                install_workload_selection(terminal, app)?;
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::SHIFT) => workload_back(app),
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => workload_back(app),
            KeyEvent {
                code: code @ (KeyCode::Tab | KeyCode::Enter),
                ..
            } => {
                if app.wizard.editing() {
                    match app.wizard.reduce(WorkloadAction::Save) {
                        Ok(_) => app.message = "Trial set saved in memory. Enter finishes; Tab edits; browse to configure another workload.".into(),
                        Err(message) => app.message = message.into(),
                    }
                } else if matches!(app.wizard.step, WorkloadStep::Saved { .. })
                    && code == KeyCode::Enter
                {
                    if app.workloads.selected_choices().any(|choice| {
                        !choice
                            .key
                            .workload()
                            .is_some_and(|name| app.wizard.selections.contains_key(name))
                    }) {
                        app.message = "Save trials for every selected workload before finishing. Browse to open another workload.".into();
                    } else if app.wizard.reduce(WorkloadAction::Finish).unwrap_or(false) {
                        return write_trial_sets(app);
                    }
                } else {
                    if !app.workloads.has_selection() {
                        app.message = "Select at least one external workload first.".into();
                        continue;
                    }
                    let Some(index) = app.workloads.active_index() else {
                        app.message =
                            "No workload matches the search. Clear the filter to continue.".into();
                        continue;
                    };
                    if !app.workloads.selected.get(index).copied().unwrap_or(false) {
                        app.message =
                            "Select the highlighted workload before opening its trials.".into();
                        continue;
                    }
                    let current = app.workloads.choices[index]
                        .key
                        .workload()
                        .expect("workload pane identity")
                        .to_owned();
                    let trials = Pane::new(
                        workloads::installed_trials(
                            &current,
                            &project::resolve(&app.root, &app.project.workloads_dir),
                        )
                        .map_err(Error::Input)?
                        .into_iter()
                        .map(|trial| Choice {
                            key: ChoiceKey::Trial(trial.id.clone()),
                            label: trial.id,
                            availability: Availability::Available,
                        })
                        .collect(),
                    );
                    app.wizard
                        .reduce(WorkloadAction::Open {
                            workload: current,
                            trials,
                        })
                        .map_err(|message| Error::Input(message.into()))?;
                    app.message = "Select enabled trials; Enter saves in memory. Shift-Tab discards edits; Esc cancels.".into();
                }
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } if !app.wizard.editing() => {
                workload_back(app);
                toggle_workload_choice(app, |app| install_workload_selection(terminal, app))?;
            }
            key => {
                if let WorkloadStep::Editing { trials, .. } = &mut app.wizard.step {
                    handle_pane_key(trials, key);
                } else if handle_pane_key(&mut app.workloads, key) {
                    app.wizard.reduce(WorkloadAction::Browse).ok();
                    app.message =
                        "Space selects; Enter/Tab opens highlighted trials; Esc cancels.".into();
                }
            }
        }
    }
}

fn workload_back(app: &mut WorkloadApp) {
    app.wizard.reduce(WorkloadAction::Back).ok();
    app.message =
        "Space selects; Enter/Tab opens highlighted trials; Esc cancels. Unsaved edits discarded."
            .into();
}

fn install_workload_selection(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut WorkloadApp,
) -> Result<bool, Error> {
    if app.wizard.editing() {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "Trials are configured locally; return to workloads before installing.",
            Color::Red,
            true,
        );
        return Ok(false);
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
        return Ok(false);
    };
    let name = choice
        .key
        .workload()
        .expect("workload pane identity")
        .to_owned();
    set_message(
        &mut app.message,
        &mut app.message_color,
        &mut app.message_until,
        format!("Installing `{name}` (8%)..."),
        Color::Cyan,
        false,
    );
    let directory = project::resolve(&app.root, &app.project.workloads_dir);
    let result = drive_install(
        app.root.clone(),
        install::operation::Request::Workload {
            name: name.clone(),
            directory,
        },
        |status| {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                status,
                Color::Cyan,
                false,
            );
            terminal
                .draw(|frame| draw_workload(frame, app))
                .map_err(|error| {
                    Error::Io(format!("cannot draw workload install status: {error}"))
                })?;
            Ok(())
        },
    )?;
    refresh_workload_availability(app);
    let ready = result.is_ok()
        && app
            .workloads
            .choices
            .get(app.workloads.cursor)
            .is_some_and(|choice| choice.availability == Availability::Available);
    match result {
        Ok(()) if !ready => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!(
                "Installation of `{name}` finished but it is not ready; check .asb/install.log."
            ),
            Color::Red,
            false,
        ),
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
            false,
        ),
    };
    Ok(ready)
}

fn toggle_workload_choice<F>(app: &mut WorkloadApp, mut install: F) -> Result<(), Error>
where
    F: FnMut(&mut WorkloadApp) -> Result<bool, Error>,
{
    let Some(index) = app.workloads.active_index() else {
        return Ok(());
    };
    if app.workloads.selected[index] {
        app.workloads.selected[index] = false;
    } else if app.workloads.choices[index].availability != Availability::Missing || install(app)? {
        app.workloads.selected[index] = true;
    }
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
    let level = usize::from(app.wizard.editing());
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(5),
        ])
        .split(frame.area());
    let steps = ["workloads", "trials"];
    let step_line = steps
        .iter()
        .enumerate()
        .flat_map(|(index, name)| {
            let style = if index == level {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < level {
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
    let description = match &app.wizard.step {
        WorkloadStep::Browsing => {
            "Choose external workloads; selecting a missing workload installs it first.".into()
        }
        WorkloadStep::Editing { workload, .. } => format!("Choose enabled trials for {workload}."),
        WorkloadStep::Saved { workload } => format!(
            "{workload}: trials saved in memory; Enter finishes and writes selected workloads; Tab edits."
        ),
    };
    frame.render_widget(
        Paragraph::new(description).block(Block::default().borders(Borders::ALL).title("Step")),
        vertical[1],
    );
    let pane = match &app.wizard.step {
        WorkloadStep::Editing { trials, .. } => trials,
        _ => &app.workloads,
    };
    render_choice_pane(frame, vertical[2], pane, true);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("Space select/install", Style::default().fg(Color::Cyan)),
                Span::raw(match app.wizard.step {
                    WorkloadStep::Browsing => " | Enter/Tab opens trials | Esc cancels",
                    WorkloadStep::Editing { .. } => {
                        " | Enter/Tab saves | Shift-Tab discards edits | Esc cancels"
                    }
                    WorkloadStep::Saved { .. } => {
                        " | Enter finishes | Tab edits | Arrows/search browse | Esc cancels"
                    }
                }),
            ]),
            Line::from(Span::styled(
                visible_message(&app.message, app.message_until),
                Style::default().fg(app.message_color),
            )),
        ]))
        .wrap(Wrap { trim: true }),
        vertical[3],
    );
}

fn write_trial_sets(app: &WorkloadApp) -> Result<usize, Error> {
    let directory = project::resolve(&app.root, &app.project.workloads_dir);
    // Catalogs may change while the wizard is open. Validate and serialize the
    // complete selection before replacing any previously published subset.
    let mut pending = Vec::new();
    for choice in app.workloads.selected_choices() {
        let workload = choice.key.workload().expect("workload pane identity");
        let ids =
            app.wizard.selections.get(workload).ok_or_else(|| {
                Error::Input(format!("save trials for {workload} before finishing"))
            })?;
        if ids.is_empty() {
            return Err(Error::Input(format!(
                "select at least one trial for {workload}"
            )));
        }
        let catalog = workloads::installed_trials(workload, &directory).map_err(Error::Input)?;
        for id in ids {
            if !catalog.iter().any(|trial| &trial.id == id) {
                return Err(Error::Input(format!(
                    "saved trial {id} is no longer available for {workload}; reopen its trials"
                )));
            }
        }
        let trials = catalog
            .into_iter()
            .filter(|trial| ids.contains(&trial.id))
            .collect();
        let config = workloads::TrialSet {
            workload: workload.to_owned(),
            trials,
        };
        let path = directory.join(format!("{workload}.workload.toml"));
        let text =
            toml::to_string_pretty(&config).map_err(|error| Error::Input(error.to_string()))?;
        pending.push((path, text));
    }
    let written = pending.len();
    for (path, text) in pending {
        crate::config::matrix::publish_atomic(&path, text.as_bytes())?;
    }
    Ok(written)
}

struct TerminalCleanup {
    active: bool,
}

impl TerminalCleanup {
    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut errors = Vec::new();
        let mut stdout = io::stdout();
        for result in [
            terminal::disable_raw_mode(),
            execute!(stdout, PopKeyboardEnhancementFlags),
            execute!(stdout, LeaveAlternateScreen),
            execute!(stdout, crossterm::cursor::Show),
        ] {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("; ")))
        }
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    cleanup: TerminalCleanup,
}

impl std::ops::Deref for TerminalSession {
    type Target = Terminal<CrosstermBackend<Stdout>>;
    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl std::ops::DerefMut for TerminalSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

fn start_terminal() -> Result<TerminalSession, io::Error> {
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        return Err(io::Error::other("TUI requires a terminal on stdin"));
    }
    let mut cleanup = TerminalCleanup { active: true };
    let started = (|| {
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
    })();
    match started {
        Ok(terminal) => Ok(TerminalSession { terminal, cleanup }),
        Err(primary) => match cleanup.restore() {
            Ok(()) => Err(primary),
            Err(error) => Err(io::Error::other(format!(
                "{primary}; cannot restore terminal: {error}"
            ))),
        },
    }
}

fn stop_terminal(mut terminal: TerminalSession) -> Result<(), io::Error> {
    let clear = terminal.clear();
    let restore = terminal.cleanup.restore();
    match (clear, restore) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(first), Err(second)) => Err(io::Error::other(format!("{first}; {second}"))),
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn with_terminal_for_test(action: impl FnOnce()) {
    let _terminal = start_terminal().unwrap();
    action();
}

fn with_terminal_cleanup<T>(result: Result<T, Error>, cleanup: io::Result<()>) -> Result<T, Error> {
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(Error::Io(format!("cannot restore terminal: {error}"))),
        (Err(primary), Err(error)) => Err(Error::Io(format!(
            "{primary}; cannot restore terminal: {error}"
        ))),
    }
}

/// All callers share process custody, bounded progress and cancellation policy.
fn drive_install(
    root: PathBuf,
    request: install::operation::Request,
    mut render: impl FnMut(String) -> Result<(), Error>,
) -> Result<Result<(), Error>, Error> {
    let mut operation = install::operation::Operation::start(root, request)?;
    let started = Instant::now();
    let mut tick = 0_u64;
    let result = (|| loop {
        if let Some(result) = operation.poll()? {
            return Ok(result);
        }
        render(install_status_message(
            operation.percent(),
            operation.stage().label(),
            operation.transfer(),
            started,
            tick,
        ))?;
        tick = tick.wrapping_add(1);
        if event::poll(std::time::Duration::from_millis(100))
            .map_err(|error| Error::Io(format!("cannot read installation input: {error}")))?
            && let Event::Key(key) = event::read()
                .map_err(|error| Error::Io(format!("cannot read installation input: {error}")))?
            && key.kind == KeyEventKind::Press
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            return Err(Error::Usage("TUI installation cancelled"));
        }
    })();
    match result {
        Ok(result) => Ok(result),
        Err(primary) => match operation.cancel() {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(Error::Io(format!("{primary}; cleanup: {cleanup}"))),
        },
    }
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
            key if is_install_key(&key) => {
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
                if app.step == PaneKind::Measurements {
                    let selected = app.panes[PaneKind::Measurements.index()]
                        .selected_choices()
                        .filter_map(|choice| choice.key.collector().map(str::to_owned))
                        .collect::<Vec<_>>();
                    if !install_setup_dependencies(terminal, app, &selected)? {
                        continue;
                    }
                    app.sync_collector_dependencies();
                }
                if app.next_step().is_none() {
                    if !install_selected_provider_tools(terminal, app)? {
                        continue;
                    }
                    return generate_configs_with_progress(terminal, app);
                }
                if let Some(generated) = app.advance()? {
                    return Ok(generated);
                }
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } if app.step == PaneKind::Concurrency => {
                app.panes[PaneKind::Concurrency.index()].toggle_single()
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } if app.step == PaneKind::CollectorOptions => {
                toggle_setup_choice(app, |_app, _selected| Ok(true))?;
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } if app.step.installable() => {
                toggle_setup_choice(app, |app, selected| match selected {
                    Some(selected) => install_setup_dependencies(terminal, app, selected),
                    None => install_setup_selection(terminal, app),
                })?;
            }
            key if handle_pane_key(app.pane_mut(app.step), key) => {}
            _ => {}
        }
    }
}

fn install_setup_dependencies(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    selected: &[String],
) -> Result<bool, Error> {
    let selected = crate::collectors::expand_dependencies(selected);
    let tools_root = project::resolve(&app.root, &app.project.tools_dir);
    let required = crate::collectors::dependency_tools(&selected);
    if required
        .iter()
        .all(|id| install::tool_installed_locally(id, &tools_root))
    {
        return Ok(true);
    }
    let result = drive_install(
        app.root.clone(),
        install::operation::Request::Dependencies {
            selected,
            directory: tools_root.clone(),
        },
        |status| {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                status,
                Color::Cyan,
                false,
            );
            terminal.draw(|frame| draw(frame, app)).map_err(|error| {
                Error::Io(format!("cannot draw dependency install status: {error}"))
            })?;
            Ok(())
        },
    )?;
    app.refresh_availability();
    finish_dependency_install(app, result, &required, &tools_root)
}

fn finish_dependency_install(
    app: &mut App,
    result: Result<(), Error>,
    required: &[&str],
    tools_root: &Path,
) -> Result<bool, Error> {
    match result {
        Ok(())
            if required
                .iter()
                .all(|id| install::tool_installed_locally(id, tools_root)) =>
        {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                "Required collector tools are installed and selected.",
                Color::Green,
                true,
            );
            Ok(true)
        }
        Ok(()) => {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                "Required collector installation finished but a dependency is not ready; check .asb/install.log.",
                Color::Red,
                false,
            );
            Ok(false)
        }
        Err(error) => {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                format!("Cannot install required collector tool: {error}"),
                Color::Red,
                false,
            );
            Ok(false)
        }
    }
}

fn toggle_setup_choice<F>(app: &mut App, mut install: F) -> Result<(), Error>
where
    F: FnMut(&mut App, Option<&[String]>) -> Result<bool, Error>,
{
    let step = app.step;
    let Some(index) = app.panes[step.index()].active_index() else {
        return Ok(());
    };
    if let ChoiceKey::AllCollectorOptions(collector) =
        app.panes[step.index()].choices[index].key.clone()
    {
        let enable = !app.panes[step.index()].selected[index];
        app.configured_collectors.insert(collector.clone());
        let pane = &mut app.panes[step.index()];
        for (choice, selected) in pane.choices.iter().zip(pane.selected.iter_mut()) {
            if choice
                .key
                .collector_option()
                .is_some_and(|(owner, _)| owner == collector)
            {
                *selected = enable;
            }
        }
        update_collector_sentinels(pane);
        return Ok(());
    }
    if let Some((collector, _)) = app.panes[step.index()].choices[index]
        .key
        .collector_option()
    {
        app.configured_collectors.insert(collector.to_owned());
        app.panes[step.index()].toggle();
        update_collector_sentinels(&mut app.panes[step.index()]);
        return Ok(());
    }
    if let ChoiceKey::AllExtension {
        workload,
        extension,
    } = app.panes[step.index()].choices[index].key.clone()
    {
        let enable = !app.panes[step.index()].selected[index];
        let pane = &mut app.panes[step.index()];
        for (choice, selected) in pane.choices.iter().zip(pane.selected.iter_mut()) {
            if matches!(&choice.key, ChoiceKey::ExtensionValue { workload: candidate, extension: candidate_extension, .. } if candidate == &workload && candidate_extension == &extension)
            {
                *selected = enable;
            }
        }
        update_extension_sentinels(pane, index);
        return Ok(());
    }
    if app.panes[step.index()].choices[index].key.is_all() {
        let enable = !app.panes[step.index()].selected[index];
        if enable {
            let missing = app.panes[step.index()]
                .choices
                .iter()
                .enumerate()
                .filter_map(|(candidate, choice)| {
                    (!choice.key.is_all() && choice.availability == Availability::Missing)
                        .then_some(candidate)
                })
                .collect::<Vec<_>>();
            for candidate in missing {
                app.panes[step.index()].cursor = candidate;
                if !install(app, None)? {
                    return Ok(());
                }
            }
        }
        let pane = &mut app.panes[step.index()];
        for (choice, selected) in pane.choices.iter().zip(pane.selected.iter_mut()) {
            if !choice.key.is_all() {
                *selected = enable;
            }
        }
        pane.selected[index] = enable;
        return Ok(());
    }
    if app.panes[step.index()].selected[index] {
        app.panes[step.index()].selected[index] = false;
        if let Some(all) = app.panes[step.index()]
            .choices
            .iter()
            .position(|choice| choice.key.is_all())
        {
            app.panes[step.index()].selected[all] = false;
        }
        if step == PaneKind::Measurements {
            app.sync_collector_dependencies();
            app.refresh_collector_options();
            if app.panes[step.index()].selected[index] {
                set_message(
                    &mut app.message,
                    &mut app.message_color,
                    &mut app.message_until,
                    "This collector is required by another selected collector.",
                    Color::Yellow,
                    true,
                );
            }
        }
        if step == PaneKind::Workloads {
            app.refresh_extensions()?;
        }
        return Ok(());
    }
    if app.panes[step.index()].choices[index].availability == Availability::Missing
        && !install(app, None)?
    {
        return Ok(());
    }
    if step == PaneKind::Measurements {
        let mut selected = app.panes[PaneKind::Measurements.index()]
            .selected_choices()
            .filter_map(|choice| choice.key.collector().map(str::to_owned))
            .collect::<Vec<_>>();
        selected.push(
            app.panes[PaneKind::Measurements.index()].choices[index]
                .key
                .collector()
                .expect("collector pane identity")
                .to_owned(),
        );
        if !install(app, Some(&selected))? {
            return Ok(());
        }
    }
    app.panes[step.index()].selected[index] = true;
    let pane = &mut app.panes[step.index()];
    if pane.choices.iter().any(|choice| choice.key.is_all()) {
        sync_all_sentinel(pane);
    }
    if step == PaneKind::Measurements {
        app.sync_collector_dependencies();
        app.refresh_collector_options();
    }
    if step == PaneKind::Extensions {
        update_extension_sentinels(&mut app.panes[step.index()], index);
    }
    if step == PaneKind::Workloads {
        app.refresh_extensions()?;
    }
    Ok(())
}

fn update_collector_sentinels(pane: &mut Pane) {
    for index in 0..pane.choices.len() {
        let ChoiceKey::AllCollectorOptions(collector) = &pane.choices[index].key else {
            continue;
        };
        let mut values = pane.choices.iter().enumerate().filter(|(_, choice)| {
            choice
                .key
                .collector_option()
                .is_some_and(|(owner, _)| owner == collector)
        });
        let mut any = false;
        let all = values.all(|(i, _)| {
            any = true;
            pane.selected[i]
        });
        pane.selected[index] = any && all;
    }
}

fn missing_required_collector_options(
    pane: &Pane,
    adapters: &[&dyn crate::collectors::CollectorAdapter],
    selected: &BTreeSet<&str>,
) -> Option<(&'static str, &'static str)> {
    adapters
        .iter()
        .filter(|adapter| selected.contains(adapter.id()))
        .find_map(|adapter| {
            let extension = adapter.selection_extension()?;
            (extension.required
                && !pane.selected_choices().any(|choice| {
                    choice
                        .key
                        .collector_option()
                        .is_some_and(|(owner, _)| owner == adapter.id())
                }))
            .then_some((adapter.id(), extension.tab))
        })
}

fn update_extension_sentinels(pane: &mut Pane, _changed: usize) {
    let groups = pane
        .choices
        .iter()
        .filter_map(|choice| match &choice.key {
            ChoiceKey::AllExtension {
                workload,
                extension,
            }
            | ChoiceKey::ExtensionValue {
                workload,
                extension,
                ..
            } => Some((workload.clone(), extension.clone())),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for (workload, extension) in groups {
        let values = pane
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| matches!(&choice.key, ChoiceKey::ExtensionValue { workload: candidate, extension: candidate_extension, .. } if candidate == &workload && candidate_extension == &extension))
            .collect::<Vec<_>>();
        let all = !values.is_empty() && values.iter().all(|(index, _)| pane.selected[*index]);
        for (index, choice) in pane.choices.iter().enumerate() {
            match &choice.key {
                ChoiceKey::AllExtension {
                    workload: candidate,
                    extension: candidate_extension,
                } if candidate == &workload && candidate_extension == &extension => {
                    pane.selected[index] = all
                }
                _ => {}
            }
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
            Constraint::Length(3),
        ])
        .split(frame.area());
    let steps = app.navigation_order();
    let active_step = steps
        .iter()
        .position(|step| *step == app.step)
        .unwrap_or_default();
    let step_line = steps
        .iter()
        .enumerate()
        .flat_map(|(index, name)| {
            let style = if *name == app.step {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < active_step {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            [
                Span::styled(format!(" {} ", app.step_name(*name)), style),
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
        Paragraph::new(app.step_description())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Step")),
        vertical[1],
    );
    let pane = app.pane(app.step);
    render_choice_pane(frame, vertical[2], pane, app.step.installable());
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                if !app.step.installable() {
                    Span::styled("Discovery only", Style::default().fg(Color::Yellow))
                } else {
                    Span::styled("Space select/install", Style::default().fg(Color::Cyan))
                },
                if !app.step.installable() {
                    Span::raw(" | Enter next")
                } else {
                    Span::raw(" | ⓘ green=ready red=missing yellow=unknown | Enter next")
                },
            ]),
            Line::from(Span::styled(
                visible_message(&app.message, app.message_until),
                Style::default().fg(app.message_color),
            )),
        ]))
        .wrap(Wrap { trim: true }),
        vertical[3],
    );
}

/// Format an honest, live installer indicator. The global bar remains visible
/// while a transfer supplies a second, byte-accurate local bar. Installer
/// subprocesses write their output to `.asb/install.log`; when no transfer is
/// observable, the moving marker still proves a stage is being monitored.
fn install_status_message(
    percent: u8,
    stage: &str,
    transfer: Option<crate::install::transfer::Progress>,
    started: Instant,
    tick: u64,
) -> String {
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
    let overall = format!(
        "Installing [{bar}] {percent}%: {stage} ({}s)",
        started.elapsed().as_secs()
    );
    transfer.map_or(overall.clone(), |transfer| {
        format!(
            "{overall} | {}",
            crate::install::transfer::line("transfer", transfer)
        )
    })
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
        let section = pane
            .sections
            .get(index)
            .and_then(|section| section.as_deref());
        if let Some(section) = section
            && previous_section != Some(section)
        {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("── {section} ──"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))));
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
    // Shrinking the filtered list or expanding the terminal must not leave
    // empty rows below the list or hide a section header unnecessarily.
    let viewport_rows = usize::from(body[1].height.saturating_sub(2));
    *state.offset_mut() = pane
        .viewport_offset
        .get()
        .min(items.len().saturating_sub(viewport_rows));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Available choices"),
        ),
        body[1],
        &mut state,
    );
    pane.viewport_offset.set(state.offset());
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
) -> Result<bool, Error> {
    let Some(choice) = app.panes[app.step.index()]
        .choices
        .get(app.panes[app.step.index()].cursor)
    else {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "Nothing is highlighted to install.",
            Color::Red,
            true,
        );
        return Ok(false);
    };
    let label = choice.label.clone();
    let Some(request) = setup_install_request(app, choice) else {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            "This item is discovered or configured, not installed by ASB.",
            Color::Red,
            true,
        );
        return Ok(false);
    };
    let step = app.step;
    set_message(
        &mut app.message,
        &mut app.message_color,
        &mut app.message_until,
        format!("Installing `{label}`..."),
        Color::Cyan,
        false,
    );
    let result = drive_install(app.root.clone(), request, |status| {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            status,
            Color::Cyan,
            false,
        );
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|error| Error::Io(format!("cannot draw setup install status: {error}")))?;
        Ok(())
    })?;
    app.refresh_availability();
    if step == PaneKind::Workloads {
        app.refresh_extensions()?;
    }
    let ready = result.is_ok()
        && app.panes[step.index()]
            .choices
            .get(app.panes[step.index()].cursor)
            .is_some_and(|choice| choice.availability == Availability::Available);
    match result {
        Ok(()) if !ready => set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!(
                "Installation of `{label}` finished but it is not ready; check .asb/install.log."
            ),
            Color::Red,
            false,
        ),
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
            false,
        ),
    };
    Ok(ready)
}

/// The TUI publishes its selected matrix directly instead of routing through
/// `asb setup`, so it must materialize provider-owned local runtimes itself.
/// Keep this alongside the interactive installer so progress, cancellation and
/// log custody remain identical to Ctrl-I installation.
fn install_selected_provider_tools(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<bool, Error> {
    let tools_root = project::resolve(&app.root, &app.project.tools_dir);
    for provider in selected_missing_provider_tools(app, &tools_root) {
        set_message(
            &mut app.message,
            &mut app.message_color,
            &mut app.message_until,
            format!("Installing required provider runtime `{provider}`..."),
            Color::Cyan,
            false,
        );
        let result = drive_install(
            app.root.clone(),
            install::operation::Request::Tool {
                name: provider.clone(),
                directory: tools_root.clone(),
            },
            |status| {
                set_message(
                    &mut app.message,
                    &mut app.message_color,
                    &mut app.message_until,
                    status,
                    Color::Cyan,
                    false,
                );
                terminal.draw(|frame| draw(frame, app)).map_err(|error| {
                    Error::Io(format!("cannot draw provider install status: {error}"))
                })?;
                Ok(())
            },
        )?;
        if let Err(error) = result {
            set_message(
                &mut app.message,
                &mut app.message_color,
                &mut app.message_until,
                format!("Cannot install required provider runtime `{provider}`: {error}"),
                Color::Red,
                false,
            );
            return Ok(false);
        }
    }
    app.refresh_availability();
    Ok(true)
}

fn selected_missing_provider_tools(app: &App, tools_root: &Path) -> Vec<String> {
    app.panes[PaneKind::Providers.index()]
        .selected_choices()
        .filter_map(|choice| choice.key.provider())
        .filter(|provider| crate::tools::find(provider).is_some())
        .filter(|provider| !install::tool_installed_locally(provider, tools_root))
        .map(str::to_owned)
        .collect()
}

/// Installation uses the typed domain identity, never its rendered annotation.
fn setup_install_request(app: &App, choice: &Choice) -> Option<install::operation::Request> {
    if !app.step.installable() {
        return None;
    }
    let tools = project::resolve(&app.root, &app.project.tools_dir);
    match (&choice.key, app.step) {
        (ChoiceKey::Agent(name), PaneKind::Agents) => Some(install::operation::Request::Tool {
            name: name.clone(),
            directory: tools,
        }),
        (
            ChoiceKey::Prompt(name) | ChoiceKey::TrialSet { workload: name, .. },
            PaneKind::Workloads,
        ) => Some(install::operation::Request::Workload {
            name: name.clone(),
            directory: project::resolve(&app.root, &app.project.workloads_dir),
        }),
        (ChoiceKey::Collector(id), PaneKind::Measurements) => {
            let name = crate::collectors::find(id)?.tool_id()?;
            Some(install::operation::Request::Tool {
                name: name.into(),
                directory: tools,
            })
        }
        _ => None,
    }
}

fn workload_choices(root: &Path, directory: &str) -> Vec<Choice> {
    let directory = project::resolve(root, directory);
    let choices = fs::read_dir(&directory)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let filename = path.file_name()?.to_str()?;
            let (key, label) = if let Some(name) = filename.strip_suffix(".txt") {
                (ChoiceKey::Prompt(name.to_owned()), name.to_owned())
            } else if let Some(name) = filename.strip_suffix(".workload.toml") {
                (
                    ChoiceKey::TrialSet {
                        workload: name.to_owned(),
                        file: PathBuf::from(filename),
                    },
                    name.to_owned(),
                )
            } else {
                return None;
            };
            Some(Choice {
                key,
                label,
                availability: Availability::Unknown,
            })
        })
        .collect::<Vec<_>>();
    let mut choices = choices;
    if choices.is_empty() {
        choices.extend(
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
            .map(|name| Choice {
                key: ChoiceKey::Prompt(name.into()),
                label: name.into(),
                availability: Availability::Unknown,
            }),
        );
    }
    choices.extend(install::external_workload_names().map(|name| Choice {
        key: ChoiceKey::Prompt(name.into()),
        label: name.into(),
        availability: Availability::Unknown,
    }));
    // A persisted trial set is configuration for its workload, not another
    // selectable workload. Keep precisely one row per workload identity and
    // prefer that saved selection so its enabled trials are retained when the
    // setup wizard publishes a benchmark.
    let mut unique = BTreeMap::<String, Choice>::new();
    for choice in choices {
        let Some(workload) = choice.key.workload().map(str::to_owned) else {
            continue;
        };
        match unique.get(&workload) {
            Some(existing)
                if matches!(existing.key, ChoiceKey::TrialSet { .. })
                    || !matches!(choice.key, ChoiceKey::TrialSet { .. }) => {}
            _ => {
                unique.insert(workload, choice);
            }
        }
    }
    let mut choices = unique.into_values().collect::<Vec<_>>();
    let external_names = install::external_workload_names().collect::<BTreeSet<_>>();
    for choice in &mut choices {
        let name = choice.key.workload().expect("workload identity");
        let ready = if external_names.contains(name) {
            install::workload_available(name, &directory)
        } else {
            workloads::load(name, &directory).is_ok()
        };
        choice.availability = if ready {
            Availability::Available
        } else {
            Availability::Missing
        };
    }
    choices.sort_by_key(|choice| {
        (
            external_names.contains(choice.key.workload().expect("workload identity")),
            choice.label.clone(),
            choice.key.clone(),
        )
    });
    choices
}

fn default_model(provider: &str) -> &'static str {
    providers::default_model(provider)
}

fn generate_configs(app: &App) -> Result<Generated, Error> {
    let (tests, sweep, configs) = prepare_configs(app)?;
    crate::config::matrix::publish_sweep(&sweep, configs, &mut std::io::sink())?;
    Ok(Generated { tests, sweep })
}

struct GenerationProgress {
    completed: usize,
    total: usize,
    started: Instant,
}

impl GenerationProgress {
    fn new(total: usize) -> Self {
        Self {
            completed: 0,
            total,
            started: Instant::now(),
        }
    }

    fn update(&mut self, completed: usize, total: usize) {
        self.total = total.max(1);
        self.completed = completed.min(self.total);
    }

    fn percent(&self) -> u8 {
        (self.completed.saturating_mul(100) / self.total.max(1)) as u8
    }

    fn eta(&self) -> Option<Duration> {
        (self.completed > 0).then(|| {
            let seconds = self.started.elapsed().as_secs_f64()
                * (self.total.saturating_sub(self.completed)) as f64
                / self.completed as f64;
            Duration::from_secs_f64(seconds)
        })
    }

    fn label(&self) -> String {
        let eta = self
            .eta()
            .map(format_duration)
            .unwrap_or_else(|| "estimating".into());
        format!(
            "Writing {}/{} files ({}%) · ETA {}",
            self.completed,
            self.total,
            self.percent(),
            eta
        )
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn draw_generation_progress(frame: &mut ratatui::Frame<'_>, progress: &GenerationProgress) {
    let area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Percentage(40),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("Writing benchmark configurations")
            .block(Block::default().borders(Borders::ALL).title("ASB setup")),
        area[1],
    );
    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL))
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(f64::from(progress.percent()) / 100.0)
            .label(progress.label()),
        area[2],
    );
}

/// Publish the selected configuration matrix without freezing the terminal.
/// Publication itself retains the normal no-replace and manifest-last contract;
/// this loop only coalesces its completed-file notifications for a 10 Hz UI.
fn generate_configs_with_progress(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &App,
) -> Result<Generated, Error> {
    let (tests, sweep, configs) = prepare_configs(app)?;
    let mut progress = GenerationProgress::new(tests.saturating_add(1));
    let worker_sweep = sweep.clone();
    let (progress_tx, progress_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut output = std::io::sink();
        let result = crate::config::matrix::publish_sweep_with_progress(
            &worker_sweep,
            configs,
            &mut output,
            &mut |completed, total| match progress_tx.try_send((completed, total)) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            },
        );
        let _ = result_tx.send(result);
    });
    loop {
        while let Ok((completed, total)) = progress_rx.try_recv() {
            progress.update(completed, total);
        }
        terminal
            .draw(|frame| draw_generation_progress(frame, &progress))
            .map_err(|error| {
                Error::Io(format!("cannot draw setup publication progress: {error}"))
            })?;
        match result_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                worker
                    .join()
                    .map_err(|_| Error::Io("setup publication worker panicked".into()))?;
                result?;
                progress.update(progress.total, progress.total);
                terminal
                    .draw(|frame| draw_generation_progress(frame, &progress))
                    .map_err(|error| {
                        Error::Io(format!("cannot draw completed setup publication: {error}"))
                    })?;
                return Ok(Generated { tests, sweep });
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                worker
                    .join()
                    .map_err(|_| Error::Io("setup publication worker panicked".into()))?;
                return Err(Error::Io(
                    "setup publication worker stopped without a result".into(),
                ));
            }
        }
    }
}

fn prepare_configs(app: &App) -> Result<(usize, PathBuf, Vec<BenchmarkConfig>), Error> {
    let agents = app.panes[PaneKind::Agents.index()]
        .selected_choices()
        .filter_map(|choice| choice.key.agent().map(str::to_owned))
        .collect::<Vec<_>>();
    let providers = app.panes[PaneKind::Providers.index()]
        .selected_choices()
        .filter_map(|choice| choice.key.provider().map(str::to_owned))
        .collect::<Vec<_>>();
    let models = app.panes[PaneKind::Models.index()]
        .selected_choices()
        .filter_map(|choice| {
            choice
                .key
                .model()
                .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
        })
        .collect::<Vec<_>>();
    let workloads = app.panes[PaneKind::Workloads.index()]
        .selected_choices()
        .filter(|choice| !choice.key.is_control())
        .map(|choice| selected_workload_trials(app, &choice.key))
        .collect::<Result<Vec<_>, _>>()?;
    let mut measures = crate::collectors::expand_dependencies(
        &app.panes[PaneKind::Measurements.index()]
            .selected_choices()
            .filter_map(|choice| choice.key.collector().map(str::to_owned))
            .collect::<Vec<_>>(),
    );
    for collector in &measures {
        let Some(extension) =
            crate::collectors::find(collector).and_then(|adapter| adapter.selection_extension())
        else {
            continue;
        };
        if extension.required
            && !app
                .pane(PaneKind::CollectorOptions)
                .selected_choices()
                .any(|choice| {
                    choice
                        .key
                        .collector_option()
                        .is_some_and(|(owner, _)| owner == collector)
                })
        {
            return Err(Error::Input(format!(
                "select at least one {} value for the {collector} collector",
                extension.tab
            )));
        }
    }
    measures.extend(
        app.pane(PaneKind::CollectorOptions)
            .selected_choices()
            .filter_map(|choice| {
                choice
                    .key
                    .collector_option()
                    .map(|(_, selection)| selection.to_owned())
            }),
    );
    let mut concurrency = app.panes[PaneKind::Concurrency.index()].selected_choices();
    let value = concurrency
        .next()
        .and_then(|choice| choice.key.concurrency());
    if value.is_none() || concurrency.next().is_some() {
        return Err(Error::Input("select exactly one concurrency value".into()));
    }
    let concurrency = value.expect("checked numeric concurrency");
    let valid_models = models
        .iter()
        .filter(|(provider, _)| providers.contains(provider))
        .count();
    let total_trials: usize = workloads.iter().map(|(_, _, trials)| trials.len()).sum();
    let planned = agents
        .len()
        .checked_mul(valid_models)
        .and_then(|count| count.checked_mul(total_trials));
    if planned.is_none_or(|count| count > crate::sweep::MAX_CONFIGS) {
        return Err(Error::Input(format!(
            "setup selection exceeds the {}-configuration sweep limit; select a smaller task or agent/model subset",
            crate::sweep::MAX_CONFIGS
        )));
    }
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
    let mut configs = crate::config::matrix::Population::default();
    let mut index = 0usize;
    for agent in &agents {
        for provider in &providers {
            for (model_provider, model) in &models {
                if model_provider != provider {
                    continue;
                }
                for (workload, trial_set, trials) in &workloads {
                    for trial in trials {
                        let name = format!("asb-test-{stamp}-{index:03}.toml");
                        let config = BenchmarkConfig {
                            planning: None,
                            agent: agent.clone(),
                            workload: workload.clone(),
                            provider: provider.clone(),
                            model: model.clone(),
                            concurrency,
                            trial_set: trial_set.clone(),
                            trial: trial.clone(),
                            workspace: app.project.workspace_dir.clone(),
                            log_file: crate::benchmark::default_log_path(&format!(
                                "{}.log",
                                name.trim_end_matches(".toml")
                            )),
                            measurements: Some(MeasurementConfig {
                                enabled: true,
                                directory: "measurements".into(),
                                collectors: measures.clone(),
                            }),
                            retention: crate::benchmark::RetentionConfig::default(),
                            mcp_servers: Vec::new(),
                        };
                        configs.push(config)?;
                        index += 1;
                    }
                }
            }
        }
    }
    if configs.is_empty() {
        return Err(Error::Input(
            "no compatible provider/model selections were made".into(),
        ));
    }
    let sweep_name = format!("asb-sweep-{stamp}.toml");
    let sweep = app.root.join(&sweep_name);
    Ok((index, sweep, configs.into_configs()))
}

type TrialSelection = (String, Option<String>, Vec<Option<String>>);

fn selected_trials(app: &App, choice: &ChoiceKey) -> Result<TrialSelection, Error> {
    let ChoiceKey::TrialSet { workload, file } = choice else {
        return choice
            .workload()
            .map(|name| (name.to_owned(), None, vec![None]))
            .ok_or_else(|| Error::Input("invalid workload selection identity".into()));
    };
    let path = project::resolve(&app.root, &app.project.workloads_dir).join(file);
    let trial_set = workloads::read_trial_set(&path).map_err(Error::Input)?;
    if trial_set.trials.is_empty() {
        return Err(Error::Input(format!(
            "workload trial set {} does not enable any trials; run `asb workload tui`",
            path.display()
        )));
    }
    Ok((
        workload.to_owned(),
        Some(file.to_string_lossy().into_owned()),
        trial_set
            .trials
            .into_iter()
            .map(|trial| Some(trial.id))
            .collect(),
    ))
}

fn selected_workload_trials(app: &App, choice: &ChoiceKey) -> Result<TrialSelection, Error> {
    let Some(workload) = choice.workload() else {
        return Err(Error::Input("invalid workload selection identity".into()));
    };
    // An adapter extension is the current wizard's explicit selection. A
    // persisted trial-set only seeds that pane; it must not replace edits made
    // in this invocation merely because workload discovery chose a TrialSet
    // key for the one visible workload row.
    let Some(extension) = workloads::setup_extension(workload) else {
        return selected_trials(app, choice);
    };
    let trials = app
        .pane(PaneKind::Extensions)
        .selected_choices()
        .filter_map(|choice| match &choice.key {
            ChoiceKey::ExtensionValue {
                workload: selected,
                extension: selected_extension,
                value,
            } if selected == workload && selected_extension == extension.id => {
                Some(Some(value.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if trials.is_empty() {
        return Err(Error::Input(format!(
            "select at least one {} value for `{workload}` in the setup TUI",
            extension.tab
        )));
    }
    Ok((workload.to_owned(), None, trials))
}

#[cfg(test)]
#[path = "../tests/unit/tui_generation.rs"]
mod generation_tests;

#[cfg(test)]
mod tests {
    use super::{
        App, Availability, Choice, ChoiceKey, Generated, GenerationProgress, Pane, PaneKind,
        default_model, generate_configs, generated_summary, install_status_message,
        last_setup_tui_config, record_last_setup_tui, selected_missing_provider_tools,
    };
    use crate::install;
    use crate::project::{self, ProjectConfig};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Style};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    #[test]
    fn reserved_all_is_never_a_selectable_tui_entry() {
        let mut reserved_model = choice("provider:all");
        reserved_model.key = ChoiceKey::Model {
            provider: "provider".into(),
            model: "all".into(),
        };
        let pane = Pane::new(vec![
            choice("all"),
            trial_set_choice("all"),
            reserved_model,
            choice("small"),
        ]);
        assert_eq!(pane.choices.len(), 1);
        assert_eq!(pane.choices[0].label, "small");
        assert_eq!(pane.selected, [false]);
    }

    fn choice(name: &str) -> Choice {
        Choice {
            label: name.into(),
            key: ChoiceKey::Prompt(name.into()),
            availability: Availability::Unknown,
        }
    }

    fn agent_choice(name: &str) -> Choice {
        Choice {
            key: ChoiceKey::Agent(name.into()),
            ..choice(name)
        }
    }
    fn provider_choice(name: &str) -> Choice {
        Choice {
            key: ChoiceKey::Provider(name.into()),
            ..choice(name)
        }
    }
    fn trial_set_choice(name: &str) -> Choice {
        Choice {
            key: ChoiceKey::TrialSet {
                workload: name.into(),
                file: format!("{name}.workload.toml").into(),
            },
            ..choice(&format!("{name} [trial set]"))
        }
    }

    #[test]
    fn selecting_collector_selects_and_keeps_required_dependency() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        let flamegraph = app.panes[PaneKind::Measurements.index()]
            .choices
            .iter()
            .position(|c| c.label == "flamegraph")
            .unwrap();
        let perf = app.panes[PaneKind::Measurements.index()]
            .choices
            .iter()
            .position(|c| c.label == "perf")
            .unwrap();
        app.panes[PaneKind::Measurements.index()]
            .selected
            .fill(false);
        app.panes[PaneKind::Measurements.index()].selected[flamegraph] = true;
        app.sync_collector_dependencies();
        assert!(app.panes[PaneKind::Measurements.index()].selected[perf]);
        app.panes[PaneKind::Measurements.index()].selected[perf] = false;
        app.sync_collector_dependencies();
        assert!(app.panes[PaneKind::Measurements.index()].selected[perf]);
        app.panes[PaneKind::Measurements.index()].selected[flamegraph] = false;
        app.panes[PaneKind::Measurements.index()].selected[perf] = false;
        app.sync_collector_dependencies();
        assert!(!app.panes[PaneKind::Measurements.index()].selected[perf]);
    }

    #[test]
    fn filtered_cursor_navigation_clamps_to_visible_choices() {
        let mut pane = Pane::new(vec![choice("alpha"), choice("beta"), choice("bravo")]);
        pane.search = "B".into();
        pane.search_changed();
        assert_eq!(pane.cursor, 1);
        pane.move_cursor(100);
        assert_eq!(pane.cursor, 2);
        pane.move_cursor(100);
        assert_eq!(pane.cursor, 2);
        pane.move_cursor(-100);
        assert_eq!(pane.cursor, 1);
    }

    #[test]
    fn selection_survives_search_and_single_select_replaces_previous_choice() {
        let mut pane = Pane::new(vec![choice("alpha"), choice("beta"), choice("bravo")]);
        pane.toggle();
        pane.search = "br".into();
        pane.search_changed();
        pane.toggle();
        assert_eq!(pane.selected, vec![true, false, true]);
        pane.toggle_single();
        assert_eq!(pane.selected, vec![false, false, false]);
        pane.toggle_single();
        assert_eq!(pane.selected, vec![false, false, true]);
    }

    #[test]
    fn keyboard_search_ignores_control_keys_and_backspace_updates_results() {
        let mut pane = Pane::new(vec![choice("alpha"), choice("beta")]);
        assert!(!super::handle_pane_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
        assert!(super::handle_pane_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
        ));
        assert_eq!(pane.visible(), vec![1]);
        assert!(super::handle_pane_key(
            &mut pane,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
        ));
        assert_eq!(pane.visible(), vec![0, 1]);
    }

    #[test]
    fn wizard_does_not_advance_without_a_selection_and_back_stops_at_start() {
        let root = std::env::temp_dir().join(format!("asb-tui-navigation-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone(), ProjectConfig::default());
        assert!(app.advance().unwrap().is_none());
        assert_eq!(app.step, PaneKind::Agents);
        assert!(app.message.contains("Select at least one"));
        app.back();
        assert_eq!(app.step, PaneKind::Agents);
        app.panes[PaneKind::Agents.index()].selected[1] = true;
        assert!(app.advance().unwrap().is_none());
        assert_eq!(app.step, PaneKind::Providers);
        app.back();
        assert_eq!(app.step, PaneKind::Agents);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_catalog_discovery_warns_that_models_are_fallbacks() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        let index = app.panes[PaneKind::Providers.index()]
            .choices
            .iter()
            .position(|choice| choice.label == "openrouter")
            .unwrap();
        app.panes[PaneKind::Providers.index()].selected[index] = true;
        let warning = app
            .refresh_models_with(|_| {
                Err(crate::Error::Input(
                    "HTTP 407: proxy authentication required".into(),
                ))
            })
            .unwrap();
        assert!(warning.contains("fallback only"));
        assert!(warning.contains("HTTP 407"));
        assert!(warning.contains("asb provider models openrouter"));
        assert_eq!(app.panes[PaneKind::Models.index()].choices.len(), 1);
        assert!(
            app.panes[PaneKind::Models.index()].choices[0]
                .label
                .contains("gpt-4o-mini")
        );
        assert!(
            app.panes[PaneKind::Models.index()].choices[0]
                .label
                .contains("[fallback]")
        );
        assert_eq!(
            app.panes[PaneKind::Models.index()].choices[0]
                .key
                .model()
                .map(|(_, model)| model),
            Some("openai/gpt-4o-mini")
        );
    }

    #[test]
    fn empty_successful_catalog_does_not_invent_fallback_models() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        let index = app.panes[PaneKind::Providers.index()]
            .choices
            .iter()
            .position(|choice| choice.label == "openrouter")
            .unwrap();
        app.panes[PaneKind::Providers.index()].selected[index] = true;
        let warning = app.refresh_models_with(|_| Ok(Vec::new())).unwrap();
        assert!(warning.contains("no tool-capable models"));
        assert!(app.panes[PaneKind::Models.index()].choices.is_empty());
    }

    #[test]
    fn successful_catalog_keeps_all_discovered_models_without_fallback_warning() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        let index = app.panes[PaneKind::Providers.index()]
            .choices
            .iter()
            .position(|choice| choice.label == "openrouter")
            .unwrap();
        app.panes[PaneKind::Providers.index()].selected[index] = true;
        assert!(
            app.refresh_models_with(|_| Ok(vec!["a/model".into(), "b/model".into()]))
                .is_none()
        );
        assert_eq!(app.panes[PaneKind::Models.index()].choices.len(), 2);
    }

    #[test]
    fn wizard_renders_each_step_at_small_and_normal_terminal_sizes() {
        use ratatui::{Terminal, backend::TestBackend};
        let root = std::env::temp_dir().join(format!("asb-tui-render-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone(), ProjectConfig::default());
        for (width, height) in [(1, 1), (20, 5), (40, 12), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for step in PaneKind::ORDER {
                app.step = step;
                terminal.draw(|frame| super::draw(frame, &app)).unwrap();
            }
            if width == 100 {
                app.step = PaneKind::Agents;
                terminal.draw(|frame| super::draw(frame, &app)).unwrap();
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(rendered.contains("ASB setup"));
                assert!(rendered.contains("[ ]"));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn choice_viewport_scrolls_only_when_cursor_leaves_visible_rows() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut pane = Pane::new(
            (0..8)
                .map(|index| choice(&format!("item-{index}")))
                .collect(),
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, pane: &Pane| {
            terminal
                .draw(|frame| super::render_choice_pane(frame, frame.area(), pane, false))
                .unwrap();
            (2..6)
                .map(|y| {
                    (0..40)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        draw(&mut terminal, &pane);
        for _ in 0..5 {
            pane.move_cursor(1);
            draw(&mut terminal, &pane);
        }
        let before = draw(&mut terminal, &pane);
        assert!(before[0].contains("item-2"));
        assert!(before[3].contains("item-5"));
        assert_eq!(
            terminal.backend().buffer()[(2, 5)].bg,
            ratatui::style::Color::Gray
        );
        pane.move_cursor(-1);
        let after = draw(&mut terminal, &pane);
        assert_eq!(
            before, after,
            "moving within the viewport must not scroll it"
        );
        assert_eq!(
            terminal.backend().buffer()[(2, 4)].bg,
            ratatui::style::Color::Gray
        );
        for _ in 0..3 {
            pane.move_cursor(-1);
            draw(&mut terminal, &pane);
        }
        let scrolled = draw(&mut terminal, &pane);
        assert!(scrolled[0].contains("item-1"));
        assert!(scrolled[3].contains("item-4"));
        for _ in 0..3 {
            pane.move_cursor(1);
            assert_eq!(draw(&mut terminal, &pane), scrolled);
        }
        pane.move_cursor(1);
        let scrolled_down = draw(&mut terminal, &pane);
        assert!(scrolled_down[0].contains("item-2"));
        assert!(scrolled_down[3].contains("item-5"));
    }

    #[test]
    fn choice_viewport_handles_sections_filtering_and_resize() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut pane = Pane::new(
            (0..8)
                .map(|index| choice(&format!("item-{index}")))
                .collect(),
        );
        pane.sections = vec![
            Some("first".into()),
            Some("first".into()),
            Some("first".into()),
            Some("first".into()),
            Some("second".into()),
            Some("second".into()),
            Some("second".into()),
            Some("second".into()),
        ];
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, pane: &Pane| {
            terminal
                .draw(|frame| super::render_choice_pane(frame, frame.area(), pane, false))
                .unwrap();
        };
        for _ in 0..6 {
            pane.move_cursor(1);
            draw(&mut terminal, &pane);
        }
        let offset = pane.viewport_offset.get();
        pane.move_cursor(-1);
        draw(&mut terminal, &pane);
        assert_eq!(pane.viewport_offset.get(), offset);

        pane.search = "item-7".into();
        pane.search_changed();
        draw(&mut terminal, &pane);
        assert_eq!(pane.viewport_offset.get(), 0);
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("item-7"));
        pane.search = "not found".into();
        pane.search_changed();
        draw(&mut terminal, &pane);
        assert_eq!(pane.viewport_offset.get(), 0);

        pane.search.clear();
        pane.search_changed();
        pane.cursor = 7;
        let mut small = Terminal::new(TestBackend::new(40, 5)).unwrap();
        draw(&mut small, &pane);
        let rendered = small
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("item-7"));
        let mut large = Terminal::new(TestBackend::new(40, 12)).unwrap();
        draw(&mut large, &pane);
        let rendered = large
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("item-7"));
    }

    #[test]
    fn search_with_no_matches_must_not_toggle_a_hidden_choice() {
        let mut pane = Pane::new(vec![choice("alpha"), choice("beta")]);
        pane.search = "no matches".into();
        pane.search_changed();
        pane.toggle();
        assert!(!pane.has_selection());
        pane.toggle_single();
        assert!(!pane.has_selection());
    }

    #[test]
    fn pane_filters_and_toggles_visible_choices() {
        let mut pane = Pane::new(vec![
            Choice {
                label: "alpha".into(),
                key: ChoiceKey::Prompt("alpha".into()),
                availability: Availability::Unknown,
            },
            Choice {
                label: "beta".into(),
                key: ChoiceKey::Prompt("beta".into()),
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
    fn app_marks_missing_agents_and_lists_only_real_collectors() {
        let root = std::env::temp_dir().join(format!("asb-tui-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App::new(root.clone(), ProjectConfig::default());
        assert!(
            app.panes[PaneKind::Agents.index()]
                .choices
                .iter()
                .filter(|choice| !choice.key.is_control())
                .all(|choice| choice.availability == Availability::Missing)
        );
        assert!(
            app.panes[PaneKind::Measurements.index()]
                .choices
                .iter()
                .all(|choice| !matches!(
                    choice.label.as_str(),
                    "token_usage" | "steps" | "success_rate" | "runtime"
                ))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bpftrace_selection_exposes_program_tab_with_defaults_and_all_control() {
        let root = std::env::temp_dir().join(format!("asb-tui-bpftrace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone(), ProjectConfig::default());
        let index = app.panes[PaneKind::Measurements.index()]
            .choices
            .iter()
            .position(|choice| choice.key.collector() == Some("bpftrace"))
            .expect("bpftrace collector");
        app.panes[PaneKind::Measurements.index()].selected[index] = true;
        app.refresh_collector_options();
        let pane = app.pane(PaneKind::CollectorOptions);
        assert!(matches!(
            pane.choices[0].key,
            ChoiceKey::AllCollectorOptions(_)
        ));
        assert!(pane.choices.iter().any(|choice| {
            choice.key.collector_option() == Some(("bpftrace", "bpftrace:sched-latency"))
        }));
        assert!(pane.selected.iter().skip(1).any(|selected| *selected));
        assert!(app.navigation_order().contains(&PaneKind::CollectorOptions));
        app.step = PaneKind::CollectorOptions;
        super::toggle_setup_choice(&mut app, |_app, _selected| Ok(true)).unwrap();
        assert!(
            app.pane(PaneKind::CollectorOptions)
                .selected
                .iter()
                .all(|selected| *selected)
        );
        super::toggle_setup_choice(&mut app, |_app, _selected| Ok(true)).unwrap();
        assert!(app.advance().unwrap().is_none());
        assert_eq!(app.step, PaneKind::CollectorOptions);
        assert!(app.message.contains("bpftrace"));
        let _ = std::fs::remove_dir_all(root);
    }

    struct FixtureCollector {
        id: &'static str,
        supports_all: bool,
    }

    impl crate::collectors::CollectorAdapter for FixtureCollector {
        fn id(&self) -> &'static str {
            self.id
        }
        fn selection_extension(&self) -> Option<crate::collectors::SelectionExtension> {
            Some(crate::collectors::SelectionExtension {
                tab: "fixture options",
                description: "Select fixture options",
                all_selection: self.supports_all.then(|| format!("{}:all", self.id)),
                all_description: "all fixture options",
                required: true,
                options: ["one", "two"]
                    .into_iter()
                    .map(|label| crate::collectors::SelectionOption {
                        selection: format!("{}:{label}", self.id),
                        label,
                        description: "fixture option",
                        default: false,
                    })
                    .collect(),
            })
        }
    }

    #[test]
    fn collector_option_groups_preserve_edits_and_require_each_selected_owner() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-multiple-collectors-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first = FixtureCollector {
            id: "first",
            supports_all: false,
        };
        let second = FixtureCollector {
            id: "second",
            supports_all: true,
        };
        let adapters: &[&dyn crate::collectors::CollectorAdapter] = &[&first, &second];
        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.panes[PaneKind::Measurements.index()] = Pane::new(vec![
            Choice {
                label: "first".into(),
                key: ChoiceKey::Collector("first".into()),
                availability: Availability::Available,
            },
            Choice {
                label: "second".into(),
                key: ChoiceKey::Collector("second".into()),
                availability: Availability::Available,
            },
        ]);
        app.panes[PaneKind::Measurements.index()]
            .selected
            .fill(true);
        app.refresh_collector_options_with(adapters);
        let pane = app.pane(PaneKind::CollectorOptions);
        assert!(
            !pane
                .choices
                .iter()
                .any(|choice| choice.key == ChoiceKey::AllCollectorOptions("first".into()))
        );
        assert!(
            pane.choices
                .iter()
                .any(|choice| choice.key == ChoiceKey::AllCollectorOptions("second".into()))
        );

        app.step = PaneKind::CollectorOptions;
        app.panes[PaneKind::CollectorOptions.index()].cursor = app
            .pane(PaneKind::CollectorOptions)
            .choices
            .iter()
            .position(|choice| choice.key.collector_option() == Some(("first", "first:one")))
            .unwrap();
        super::toggle_setup_choice(&mut app, |_, _| Ok(true)).unwrap();
        let selected = BTreeSet::from(["first", "second"]);
        assert_eq!(
            super::missing_required_collector_options(
                app.pane(PaneKind::CollectorOptions),
                adapters,
                &selected
            ),
            Some(("second", "fixture options"))
        );

        app.panes[PaneKind::Measurements.index()].selected[0] = false;
        app.refresh_collector_options_with(adapters);
        app.panes[PaneKind::Measurements.index()].selected[0] = true;
        app.refresh_collector_options_with(adapters);
        let selected_first = app
            .pane(PaneKind::CollectorOptions)
            .selected_choices()
            .filter_map(|choice| choice.key.collector_option())
            .filter(|(owner, _)| *owner == "first")
            .collect::<Vec<_>>();
        assert_eq!(selected_first, vec![("first", "first:one")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_collector_options_round_trip_through_the_setup_wizard() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-collector-options-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("benchmark.toml");
        for (requested, expected) in [
            ("bpftrace:io-wait", vec!["bpftrace:io-wait".to_owned()]),
            (
                "bpftrace:all",
                crate::collectors::find("bpftrace")
                    .unwrap()
                    .selection_extension()
                    .unwrap()
                    .options
                    .into_iter()
                    .map(|option| option.selection)
                    .collect(),
            ),
        ] {
            std::fs::write(&input, format!("agent='pi'\nprovider='openrouter'\nmodel='configured-model'\nworkload='bug-fix'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['bpftrace','{requested}']\n")).unwrap();
            let mut app = App::new(root.clone(), ProjectConfig::default());
            app.preselect(&input).unwrap();
            app.refresh_collector_options();
            let selected = app
                .pane(PaneKind::CollectorOptions)
                .selected_choices()
                .filter_map(|choice| {
                    choice
                        .key
                        .collector_option()
                        .map(|(_, value)| value.to_owned())
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(selected, expected.into_iter().collect());
            let (_, _, configs) = super::prepare_configs(&app).unwrap();
            let generated = &configs[0].measurements.as_ref().unwrap().collectors;
            for selection in &selected {
                assert!(generated.contains(selection));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_tui_lists_uninstalled_external_workloads() {
        let root = std::env::temp_dir().join(format!("asb-setup-workloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App::new(root.clone(), ProjectConfig::default());
        let workload = app.panes[PaneKind::Workloads.index()]
            .choices
            .iter()
            .find(|choice| choice.label == "terminal-bench-1.0")
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
    fn setup_selection_installs_missing_items_before_selection_and_retries_failure() {
        for step in [
            PaneKind::Agents,
            PaneKind::Workloads,
            PaneKind::Measurements,
        ] {
            let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
            app.step = step;
            app.panes[step.index()].selected.fill(false);
            let first = app.panes[step.index()]
                .choices
                .iter()
                .position(|choice| !choice.key.is_control())
                .unwrap();
            app.panes[step.index()].cursor = first;
            app.panes[step.index()].choices[first].availability = Availability::Missing;
            let mut calls = 0;
            super::toggle_setup_choice(&mut app, |app, dependency| {
                assert!(dependency.is_none());
                assert!(!app.panes[step.index()].selected[first]);
                calls += 1;
                Ok(false)
            })
            .unwrap();
            assert_eq!(calls, 1);
            assert!(
                !app.panes[step.index()].selected[first],
                "failed install selected step {step:?}"
            );
            super::toggle_setup_choice(&mut app, |app, dependency| {
                assert!(!app.panes[step.index()].selected[first]);
                if dependency.is_none() {
                    calls += 1;
                    app.panes[step.index()].choices[first].availability = Availability::Available;
                }
                Ok(true)
            })
            .unwrap();
            assert_eq!(calls, 2);
            assert!(app.panes[step.index()].selected[first]);
            super::toggle_setup_choice(&mut app, |_, _| panic!("deselection must not install"))
                .unwrap();
            assert!(!app.panes[step.index()].selected[first]);
        }
    }

    #[test]
    #[cfg(unix)]
    fn successful_dependency_process_requires_ready_files_before_selection() {
        use std::os::unix::fs::PermissionsExt;
        for state in ["missing", "nonexec", "ready"] {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/tui-dependency-fixtures")
                .join(crate::workloads::TaskIdentity::local("dependency", None).attempt_id);
            std::fs::create_dir_all(&root).unwrap();
            let mut app = App::new(root.clone(), ProjectConfig::default());
            let tools_root = crate::project::resolve(&root, &app.project.tools_dir);
            let perf = tools_root.join("perf/bin/perf");
            std::fs::create_dir_all(perf.parent().unwrap()).unwrap();
            let flamegraph = tools_root.join("flamegraph/bin/flamegraph");
            std::fs::create_dir_all(flamegraph.parent().unwrap()).unwrap();
            std::fs::write(&flamegraph, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&flamegraph, std::fs::Permissions::from_mode(0o700)).unwrap();
            let installer = root.join("fake-install");
            std::fs::write(&installer, "#!/bin/sh\nif [ \"$1\" != missing ]; then\n printf '#!/bin/sh\\nexit 0\\n' > \"$2\"\n /bin/chmod \"$3\" \"$2\"\nfi\nexit 0\n").unwrap();
            std::fs::set_permissions(&installer, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                std::process::Command::new("/bin/sh")
                    .arg(&installer)
                    .arg(state)
                    .arg(&perf)
                    .arg(if state == "ready" { "700" } else { "600" })
                    .status()
                    .unwrap()
                    .success()
            );
            app.step = PaneKind::Measurements;
            let index = app.panes[PaneKind::Measurements.index()]
                .choices
                .iter()
                .position(|choice| choice.label == "flamegraph")
                .unwrap();
            app.panes[PaneKind::Measurements.index()].cursor = index;
            app.panes[PaneKind::Measurements.index()]
                .selected
                .fill(false);
            app.panes[PaneKind::Measurements.index()].choices[index].availability =
                Availability::Available;
            let selected_step = app.step;
            super::toggle_setup_choice(&mut app, |app, dependencies| {
                assert!(dependencies.is_some());
                app.refresh_availability();
                super::finish_dependency_install(app, Ok(()), &["perf"], &tools_root)
            })
            .unwrap();
            assert_eq!(
                app.panes[PaneKind::Measurements.index()].selected[index],
                state == "ready"
            );
            assert_eq!(app.step, selected_step);
            if state != "ready" {
                assert!(app.message.contains("dependency is not ready"));
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn collector_dependency_install_failure_never_selects_collector() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        app.step = PaneKind::Measurements;
        app.panes[PaneKind::Measurements.index()]
            .selected
            .fill(false);
        let flame = app.panes[PaneKind::Measurements.index()]
            .choices
            .iter()
            .position(|choice| choice.label == "flamegraph")
            .unwrap();
        let perf = app.panes[PaneKind::Measurements.index()]
            .choices
            .iter()
            .position(|choice| choice.label == "perf")
            .unwrap();
        app.panes[PaneKind::Measurements.index()].cursor = flame;
        app.panes[PaneKind::Measurements.index()].choices[flame].availability =
            Availability::Available;
        super::toggle_setup_choice(&mut app, |app, selected| {
            assert!(selected.unwrap().contains(&"flamegraph".to_owned()));
            assert!(!app.panes[PaneKind::Measurements.index()].selected[flame]);
            assert!(!app.panes[PaneKind::Measurements.index()].selected[perf]);
            Ok(false)
        })
        .unwrap();
        assert!(!app.panes[PaneKind::Measurements.index()].selected[flame]);
        assert!(!app.panes[PaneKind::Measurements.index()].selected[perf]);
        super::toggle_setup_choice(&mut app, |_, selected| {
            assert!(selected.is_some());
            Ok(true)
        })
        .unwrap();
        assert!(app.panes[PaneKind::Measurements.index()].selected[flame]);
        assert!(app.panes[PaneKind::Measurements.index()].selected[perf]);
        app.panes[PaneKind::Measurements.index()].cursor = perf;
        super::toggle_setup_choice(&mut app, |_, _| {
            panic!("required dependency is already selected")
        })
        .unwrap();
        assert!(app.panes[PaneKind::Measurements.index()].selected[perf]);
    }

    #[test]
    fn default_measurements_do_not_select_missing_tools_or_dependencies() {
        let app = App::new(std::env::temp_dir(), ProjectConfig::default());
        for choice in app.panes[PaneKind::Measurements.index()].selected_choices() {
            let Some(collector) = choice.key.collector() else {
                continue;
            };
            let dependencies = crate::collectors::expand_dependencies(&[collector.to_owned()]);
            for name in dependencies {
                assert!(
                    app.panes[PaneKind::Measurements.index()]
                        .choices
                        .iter()
                        .any(|item| item.label == name
                            && item.availability == Availability::Available)
                );
            }
        }
    }

    #[test]
    fn measurement_all_control_installs_missing_collectors_and_toggles_every_selection() {
        let mut app = App::new(std::env::temp_dir(), ProjectConfig::default());
        app.step = PaneKind::Measurements;
        app.panes[PaneKind::Measurements.index()] = Pane::new(vec![
            Choice {
                label: "[all]".into(),
                key: ChoiceKey::AllCollectors,
                availability: Availability::Available,
            },
            Choice {
                label: "perf".into(),
                key: ChoiceKey::Collector("perf".into()),
                availability: Availability::Missing,
            },
            Choice {
                label: "mpstat".into(),
                key: ChoiceKey::Collector("mpstat".into()),
                availability: Availability::Missing,
            },
        ]);
        let mut installs = Vec::new();
        super::toggle_setup_choice(&mut app, |app, dependencies| {
            assert!(dependencies.is_none());
            let cursor = app.panes[PaneKind::Measurements.index()].cursor;
            installs.push(
                app.panes[PaneKind::Measurements.index()].choices[cursor]
                    .label
                    .clone(),
            );
            app.panes[PaneKind::Measurements.index()].choices[cursor].availability =
                Availability::Available;
            Ok(true)
        })
        .unwrap();
        assert_eq!(installs, ["perf", "mpstat"]);
        assert!(
            app.panes[PaneKind::Measurements.index()]
                .selected
                .iter()
                .all(|selected| *selected)
        );

        app.panes[PaneKind::Measurements.index()].cursor = 0;
        super::toggle_setup_choice(&mut app, |_, _| {
            panic!("deselecting [all] must not install collectors")
        })
        .unwrap();
        assert!(
            app.panes[PaneKind::Measurements.index()]
                .selected
                .iter()
                .all(|selected| !*selected)
        );
    }

    #[test]
    fn workload_selector_installs_before_selection_and_does_not_install_on_deselection() {
        let mut app = super::WorkloadApp {
            root: std::env::temp_dir(),
            project: ProjectConfig::default(),
            workloads: Pane::new(vec![choice("fixture")]),
            wizard: super::WorkloadWizard::default(),
            message: String::new(),
            message_color: Color::Yellow,
            message_until: None,
        };
        app.workloads.choices[0].availability = Availability::Missing;
        super::toggle_workload_choice(&mut app, |app| {
            assert!(!app.workloads.selected[0]);
            Ok(false)
        })
        .unwrap();
        assert!(!app.workloads.selected[0]);
        super::toggle_workload_choice(&mut app, |app| {
            assert!(!app.workloads.selected[0]);
            app.workloads.choices[0].availability = Availability::Available;
            Ok(true)
        })
        .unwrap();
        assert!(app.workloads.selected[0]);
        super::toggle_workload_choice(&mut app, |_| panic!("deselection must not install"))
            .unwrap();
        assert!(!app.workloads.selected[0]);
        super::toggle_workload_choice(&mut app, |_| {
            panic!("installed workloads must not reinstall")
        })
        .unwrap();
        assert!(app.workloads.selected[0]);
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
        let first = install_status_message(15, "installing npm package", None, started, 0);
        let second = install_status_message(15, "installing npm package", None, started, 1);
        assert!(first.contains("["));
        assert!(first.contains("] 15%: installing npm package"));
        assert_ne!(
            first, second,
            "the activity marker must move while a stage runs"
        );
    }

    #[test]
    fn install_status_message_shows_overall_and_byte_accurate_transfer_progress() {
        let message = install_status_message(
            15,
            "downloading",
            Some(crate::install::transfer::Progress {
                completed_bytes: 512,
                total_bytes: Some(1024),
                bytes_per_second: 256,
                rate_source: crate::install::transfer::RateSource::Network,
            }),
            Instant::now(),
            0,
        );
        assert!(message.contains("Installing ["));
        assert!(message.contains("] 15%: downloading"));
        assert!(message.contains("transfer [======50%-------] 512 B/1.0 KiB 256 B/s"));
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
            .find(|choice| choice.label == "terminal-bench-1.0")
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
                external_names.contains(choice.key.workload().expect("workload pane identity"))
            })
            .expect("external catalog is present");
        assert!(choices[..external_start].iter().all(|choice| {
            let name = choice.key.workload().expect("workload pane identity");
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
    fn final_tui_setup_prepares_selected_provider_owned_tools() {
        let root =
            std::env::temp_dir().join(format!("asb-tui-provider-install-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let project = ProjectConfig::default();
        let mut app = App::new(root.clone(), project.clone());
        app.panes[PaneKind::Providers.index()] = Pane::new(vec![provider_choice("cli2key")]);
        app.panes[PaneKind::Providers.index()].selected[0] = true;
        let tools = project::resolve(&root, &project.tools_dir);
        assert_eq!(selected_missing_provider_tools(&app, &tools), ["cli2key"]);

        app.panes[PaneKind::Providers.index()] = Pane::new(vec![provider_choice("openrouter")]);
        app.panes[PaneKind::Providers.index()].selected[0] = true;
        assert!(selected_missing_provider_tools(&app, &tools).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generation_writes_test_configs_and_sweep_references() {
        let root = std::env::temp_dir().join(format!("asb-tui-generation-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let project = ProjectConfig::default();
        let mut app = App::new(root.clone(), project);
        app.panes[PaneKind::Agents.index()].selected[1] = true;
        app.panes[PaneKind::Providers.index()].selected[0] = true;
        // Use a catalog fixture rather than discovering from the selected
        // provider, which could read a developer key or contact a live API.
        app.panes[PaneKind::Models.index()] = Pane::new(vec![Choice {
            label: "openai:fixture-model".into(),
            key: ChoiceKey::Model {
                provider: "openai".into(),
                model: "fixture-model".into(),
            },
            availability: Availability::Unknown,
        }]);
        app.panes[PaneKind::Models.index()].selected[0] = true;
        app.panes[PaneKind::Workloads.index()].selected[1] = true;
        let generated = generate_configs(&app).expect("configs generated");
        assert_eq!(generated.tests, 1);
        let manifest = crate::sweep::read_manifest(&generated.sweep).unwrap();
        let test_path = root.join(&manifest.configs[0]);
        let config_text = std::fs::read_to_string(test_path).expect("generated config");
        let config: crate::benchmark::BenchmarkConfig =
            toml::from_str(&config_text).expect("generated config parses");
        assert_eq!(config.concurrency, 1);
        let sweep = std::fs::read_to_string(&generated.sweep).expect("sweep written");
        assert!(sweep.contains("benchmark-000.toml"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn full_population_setup_and_compare_keep_every_selected_task() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("population-setup", None).attempt_id);
        std::fs::create_dir_all(root.join("workloads")).unwrap();
        let selected = crate::workloads::TrialSet {
            workload: "swe-bench".into(),
            trials: (0..2294)
                .map(|index| crate::workloads::Trial {
                    id: format!("fixture-instance-{index}"),
                    description: "synthetic selection fixture".into(),
                })
                .collect(),
        };
        std::fs::write(
            root.join("workloads/swe-bench.workload.toml"),
            toml::to_string(&selected).unwrap(),
        )
        .unwrap();
        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.panes[PaneKind::Agents.index()] = Pane::new(vec![agent_choice("pi")]);
        app.panes[PaneKind::Providers.index()] = Pane::new(vec![provider_choice("openrouter")]);
        app.panes[PaneKind::Models.index()] = Pane::new(vec![Choice {
            label: "fixture-model".into(),
            key: ChoiceKey::Model {
                provider: "openrouter".into(),
                model: "fixture-model".into(),
            },
            availability: Availability::Unknown,
        }]);
        app.panes[PaneKind::Workloads.index()] = Pane::new(vec![trial_set_choice("swe-bench")]);
        for pane in &mut app.panes[..4] {
            pane.selected[0] = true;
        }
        // A trial-capable workload publishes the current extension pane, even
        // when the workload row is a persisted trial set. Mirror the explicit
        // all-trials selection made by the setup wizard.
        app.panes[PaneKind::Extensions.index()] = Pane::new(
            (0..2294)
                .map(|index| Choice {
                    label: format!("fixture-instance-{index}"),
                    key: ChoiceKey::ExtensionValue {
                        workload: "swe-bench".into(),
                        extension: "trial".into(),
                        value: format!("fixture-instance-{index}"),
                    },
                    availability: Availability::Available,
                })
                .collect(),
        );
        app.panes[PaneKind::Extensions.index()].selected.fill(true);
        let generated = generate_configs(&app).unwrap();
        assert_eq!(generated.tests, 2294);
        assert!(std::fs::metadata(&generated.sweep).unwrap().len() > 64 * 1024);
        let manifest = crate::sweep::read_manifest(&generated.sweep).unwrap();
        for (index, reference) in manifest.configs.iter().enumerate() {
            let config: crate::benchmark::BenchmarkConfig =
                toml::from_str(&std::fs::read_to_string(root.join(reference)).unwrap()).unwrap();
            assert_eq!(config.trial, Some(format!("fixture-instance-{index}")));
        }
        // No clients/providers/evaluators run: an empty result census must
        // retain all selected tasks rather than truncate the report population.
        crate::compare::run(generated.sweep.to_str().unwrap(), &mut Vec::new()).unwrap();
        let stem = generated.sweep.file_stem().unwrap().to_str().unwrap();
        let report: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(format!("results/{stem}-comparison/comparison.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(report["benchmarks"].as_array().unwrap().len(), 2294);
        assert!(
            report["benchmarks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["status"]
                    .as_str()
                    .unwrap()
                    .contains("missing measurements"))
        );
        // Reject an oversized cross product before creating another config.
        app.panes[PaneKind::Agents.index()] = Pane::new(
            (0..32)
                .map(|index| agent_choice(&format!("fixture-agent-{index}")))
                .collect(),
        );
        app.panes[PaneKind::Agents.index()].selected.fill(true);
        let before = std::fs::read_dir(&root).unwrap().count();
        assert!(
            generate_configs(&app)
                .err()
                .expect("oversized setup must be rejected")
                .to_string()
                .contains("65536-configuration")
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrency_pane_selects_one_positive_value() {
        let root = std::env::temp_dir().join(format!("asb-tui-concurrency-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let app = App::new(root.clone(), ProjectConfig::default());
        assert_eq!(app.panes.len(), 8);
        assert_eq!(
            app.panes[PaneKind::Concurrency.index()]
                .selected_choices()
                .count(),
            1
        );
        assert_eq!(
            app.panes[PaneKind::Concurrency.index()].choices[0].label,
            "1"
        );
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

    #[test]
    fn publication_progress_counts_the_sweep_and_reports_percent_and_eta() {
        let mut progress = GenerationProgress::new(3);
        assert!(
            progress
                .label()
                .contains("Writing 0/3 files (0%) · ETA estimating")
        );
        progress.update(2, 3);
        assert!(progress.label().contains("Writing 2/3 files (66%) · ETA "));
        progress.update(3, 3);
        assert!(
            progress
                .label()
                .contains("Writing 3/3 files (100%) · ETA 00:00")
        );
    }

    #[test]
    fn last_setup_tui_record_prefers_the_generated_sweep() {
        let root = std::env::temp_dir().join(format!("asb-tui-last-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".asb")).unwrap();
        let sweep = root.join("asb-sweep-123.toml");
        let benchmark = root.join("asb-test-123-000.toml");
        std::fs::write(&sweep, "configs=[]\n").unwrap();
        std::fs::write(&benchmark, "agent='pi'\n").unwrap();
        record_last_setup_tui(
            &root,
            &Generated {
                tests: 2,
                sweep: sweep.clone(),
            },
        )
        .unwrap();
        assert_eq!(last_setup_tui_config(&root).unwrap(), sweep);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn last_setup_tui_record_rejects_escape_paths() {
        let root = std::env::temp_dir().join(format!("asb-tui-last-unsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/setup-tui-last.toml"),
            "schema_version=1\nconfig='../outside.toml'\n",
        )
        .unwrap();
        assert!(
            last_setup_tui_config(&root)
                .unwrap_err()
                .to_string()
                .contains("unsafe configuration path")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saved_terminal_bench_2_0_variant_reopens_before_it_is_installed() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-preselect-external-variant-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("saved.toml");
        std::fs::write(
            &input,
            "agent='pi'\nprovider='openrouter'\nmodel='configured-model'\nworkload='terminal-bench-2.0'\ntrial='smoke'\nconcurrency=1\n",
        )
        .unwrap();

        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.preselect(&input).unwrap();
        assert!(
            app.pane(PaneKind::Workloads)
                .selected_choices()
                .any(|choice| choice.key.workload() == Some("terminal-bench-2.0"))
        );
        assert_eq!(
            app.pane(PaneKind::Workloads)
                .choices
                .iter()
                .find(|choice| choice.key.workload() == Some("terminal-bench-2.0"))
                .expect("registered Terminal-Bench 2.0 variant")
                .availability,
            Availability::Missing
        );
        assert!(
            app.pane(PaneKind::Extensions)
                .selected_choices()
                .any(|choice| {
                    matches!(
                        &choice.key,
                        ChoiceKey::ExtensionValue { workload, value, .. }
                            if workload == "terminal-bench-2.0" && value == "smoke"
                    )
                })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_toml_preselects_every_wizard_pane_including_adapter_trials() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-preselect-benchmark-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("benchmark.toml");
        std::fs::write(
            &input,
            "agent='pi'\nprovider='openrouter'\nmodel='configured-model'\nworkload='bug-fix'\ntrial='smoke'\nconcurrency=2\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['system']\n",
        )
        .unwrap();

        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.preselect(&input).unwrap();
        assert!(
            app.pane(PaneKind::Agents)
                .selected_choices()
                .any(|choice| { choice.key.agent() == Some("pi") })
        );
        assert!(
            app.pane(PaneKind::Providers)
                .selected_choices()
                .any(|choice| { choice.key.provider() == Some("openrouter") })
        );
        assert!(
            app.pane(PaneKind::Models)
                .selected_choices()
                .any(|choice| { choice.key.model() == Some(("openrouter", "configured-model")) })
        );
        assert!(
            app.pane(PaneKind::Workloads)
                .selected_choices()
                .any(|choice| { choice.key.workload() == Some("bug-fix") })
        );
        assert!(
            app.pane(PaneKind::Measurements)
                .selected_choices()
                .any(|choice| { choice.key.collector() == Some("system") })
        );
        assert!(
            app.pane(PaneKind::Concurrency)
                .selected_choices()
                .any(|choice| { choice.key.concurrency() == Some(2) })
        );
        assert!(
            app.refresh_models_with(|_| Ok(vec!["discovered-model".into()]))
                .is_none()
        );
        assert!(
            app.pane(PaneKind::Models)
                .selected_choices()
                .any(|choice| { choice.key.model() == Some(("openrouter", "configured-model")) })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_toml_unions_preselections_and_rejects_mixed_concurrency() {
        let root =
            std::env::temp_dir().join(format!("asb-tui-preselect-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("one.toml"),
            "agent='pi'\nprovider='openrouter'\nmodel='first-model'\nworkload='bug-fix'\nconcurrency=3\n",
        )
        .unwrap();
        std::fs::write(
            root.join("two.toml"),
            "agent='opencode'\nprovider='openrouter'\nmodel='second-model'\nworkload='build-repair'\nconcurrency=3\n",
        )
        .unwrap();
        let sweep = root.join("saved-sweep.toml");
        std::fs::write(&sweep, "configs=['one.toml', 'two.toml']\n").unwrap();

        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.preselect(&sweep).unwrap();
        assert_eq!(
            app.pane(PaneKind::Agents)
                .selected_choices()
                .filter_map(|choice| choice.key.agent())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["opencode", "pi"])
        );
        assert_eq!(
            app.pane(PaneKind::Models)
                .selected_choices()
                .filter_map(|choice| choice.key.model())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ("openrouter", "first-model"),
                ("openrouter", "second-model")
            ])
        );
        assert!(
            app.pane(PaneKind::Concurrency)
                .selected_choices()
                .any(|choice| { choice.key.concurrency() == Some(3) })
        );

        std::fs::write(
            root.join("two.toml"),
            "agent='opencode'\nprovider='openrouter'\nmodel='second-model'\nworkload='build-repair'\nconcurrency=4\n",
        )
        .unwrap();
        let error = App::new(root.clone(), ProjectConfig::default())
            .preselect(&sweep)
            .unwrap_err();
        assert!(error.to_string().contains("multiple concurrency values"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extension_all_control_tracks_and_toggles_every_value() {
        let mut pane = Pane::new(vec![
            Choice {
                label: "[all] terminal-bench-1.0".into(),
                key: ChoiceKey::AllExtension {
                    workload: "terminal-bench-1.0".into(),
                    extension: "trial".into(),
                },
                availability: Availability::Available,
            },
            Choice {
                label: "task-a".into(),
                key: ChoiceKey::ExtensionValue {
                    workload: "terminal-bench-1.0".into(),
                    extension: "trial".into(),
                    value: "task-a".into(),
                },
                availability: Availability::Available,
            },
            Choice {
                label: "task-b".into(),
                key: ChoiceKey::ExtensionValue {
                    workload: "terminal-bench-1.0".into(),
                    extension: "trial".into(),
                    value: "task-b".into(),
                },
                availability: Availability::Available,
            },
        ]);
        pane.selected[1] = true;
        pane.selected[2] = true;
        super::update_extension_sentinels(&mut pane, 1);
        assert!(pane.selected[0]);
        pane.selected[1] = false;
        pane.selected[2] = false;
        super::update_extension_sentinels(&mut pane, 1);
        assert!(!pane.selected[0]);
        assert!(!pane.has_selection());
        assert!(
            pane.selected_choices()
                .all(|choice| !matches!(choice.key, ChoiceKey::ExtensionValue { .. }))
        );
    }

    #[test]
    fn selected_adapter_adds_trial_tab_and_deselection_discards_it() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-extension-lifecycle-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        install_agentbench_catalog_fixture(&root);
        let mut app = App::new(root.clone(), ProjectConfig::default());
        let workload = app.panes[PaneKind::Workloads.index()]
            .choices
            .iter()
            .position(|choice| choice.key.workload() == Some("agentbench"))
            .unwrap();
        app.panes[PaneKind::Workloads.index()].selected[workload] = true;
        app.refresh_extensions().unwrap();
        assert!(app.navigation_order().contains(&PaneKind::Extensions));
        let trials = &app.panes[PaneKind::Extensions.index()];
        assert!(
            trials
                .choices
                .iter()
                .any(|choice| matches!(choice.key, ChoiceKey::AllExtension { .. }))
        );
        let value = trials
            .choices
            .iter()
            .position(|choice| matches!(choice.key, ChoiceKey::ExtensionValue { .. }))
            .unwrap();
        app.panes[PaneKind::Extensions.index()].selected[value] = true;
        app.panes[PaneKind::Workloads.index()].selected[workload] = false;
        app.refresh_extensions().unwrap();
        assert!(app.panes[PaneKind::Extensions.index()].choices.is_empty());
        assert!(!app.navigation_order().contains(&PaneKind::Extensions));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn installed_terminal_bench_2_0_trials_are_selectable_in_the_tui() {
        let root =
            std::env::temp_dir().join(format!("asb-tui-terminal-bench-2-0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let installed = root.join("workloads/terminal-bench-2.0");
        for index in 0..89 {
            std::fs::create_dir_all(installed.join(format!("source/fixture-{index}"))).unwrap();
        }
        let tasks = (0..89)
            .map(|index| {
                serde_json::json!({
                    "id": format!("fixture-{index}"),
                    "description": "fixture task",
                    "instruction": "public fixture instruction",
                    "definition": format!("source/fixture-{index}"),
                })
            })
            .collect::<Vec<_>>();
        std::fs::write(installed.join("source-manifest.json"), "{}").unwrap();
        std::fs::write(
            installed.join("catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "workload": "terminal-bench-2.0",
                "dataset": {"source":"harbor-framework/terminal-bench-2", "revision":"2fd12b88aafdd04a52c298e3940bcb189f9766d6", "split":"repository-root-tasks"},
                "sdk": {"package":"harbor", "version":"0.24.0"},
                "source_sha256": "a".repeat(64),
                "archive_sha256": "b".repeat(64),
                "tasks": tasks,
            }))
            .unwrap(),
        )
        .unwrap();
        let mut app = App::new(root.clone(), ProjectConfig::default());
        let workload = app.panes[PaneKind::Workloads.index()]
            .choices
            .iter()
            .position(|choice| choice.key.workload() == Some("terminal-bench-2.0"))
            .unwrap();
        app.panes[PaneKind::Workloads.index()].selected[workload] = true;
        app.refresh_extensions().unwrap();
        assert!(
            app.panes[PaneKind::Extensions.index()]
                .choices
                .iter()
                .any(|choice| {
                    matches!(&choice.key, ChoiceKey::ExtensionValue { workload, value, .. }
                if workload == "terminal-bench-2.0" && value == "fixture-0")
                })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extension_trial_selection_is_published_as_a_workload_trial() {
        let root = std::env::temp_dir().join(format!(
            "asb-tui-extension-publication-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        install_agentbench_catalog_fixture(&root);
        let mut app = App::new(root.clone(), ProjectConfig::default());
        app.panes[PaneKind::Agents.index()] = Pane::new(vec![agent_choice("pi")]);
        app.panes[PaneKind::Agents.index()].selected[0] = true;
        app.panes[PaneKind::Providers.index()] = Pane::new(vec![provider_choice("openai")]);
        app.panes[PaneKind::Providers.index()].selected[0] = true;
        app.panes[PaneKind::Models.index()] = Pane::new(vec![Choice {
            label: "fixture-model".into(),
            key: ChoiceKey::Model {
                provider: "openai".into(),
                model: "fixture-model".into(),
            },
            availability: Availability::Available,
        }]);
        app.panes[PaneKind::Models.index()].selected[0] = true;
        app.panes[PaneKind::Workloads.index()] = Pane::new(vec![choice("agentbench")]);
        app.panes[PaneKind::Workloads.index()].selected[0] = true;
        app.refresh_extensions().unwrap();
        let trial = app.panes[PaneKind::Extensions.index()]
            .choices
            .iter()
            .position(|choice| {
                matches!(&choice.key, ChoiceKey::ExtensionValue { value, .. } if value == "fixture")
            })
            .unwrap();
        app.panes[PaneKind::Extensions.index()].selected[trial] = true;
        let generated = generate_configs(&app).unwrap();
        assert_eq!(generated.tests, 1);
        let manifest = crate::sweep::read_manifest(&generated.sweep).unwrap();
        let config: crate::benchmark::BenchmarkConfig =
            toml::from_str(&std::fs::read_to_string(root.join(&manifest.configs[0])).unwrap())
                .unwrap();
        assert_eq!(config.workload, "agentbench");
        assert_eq!(config.trial.as_deref(), Some("fixture"));
        assert!(config.trial_set.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_external_workload_uses_only_its_explicitly_selected_trial() {
        for workload in install::external_workload_names() {
            let root = std::env::temp_dir().join(format!(
                "asb-tui-selected-trial-{workload}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let mut app = App::new(root.clone(), ProjectConfig::default());
            app.panes[PaneKind::Agents.index()] = Pane::new(vec![agent_choice("pi")]);
            app.panes[PaneKind::Agents.index()].selected[0] = true;
            app.panes[PaneKind::Providers.index()] = Pane::new(vec![provider_choice("openai")]);
            app.panes[PaneKind::Providers.index()].selected[0] = true;
            app.panes[PaneKind::Models.index()] = Pane::new(vec![Choice {
                label: "fixture-model".into(),
                key: ChoiceKey::Model {
                    provider: "openai".into(),
                    model: "fixture-model".into(),
                },
                availability: Availability::Available,
            }]);
            app.panes[PaneKind::Models.index()].selected[0] = true;
            let saved = root.join("workloads/saved.workload.toml");
            std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
            std::fs::write(
                &saved,
                format!("workload = '{workload}'\n[[trials]]\nid = 'saved-trial'\n"),
            )
            .unwrap();
            app.panes[PaneKind::Workloads.index()] = Pane::new(vec![Choice {
                label: workload.into(),
                key: ChoiceKey::TrialSet {
                    workload: workload.into(),
                    file: PathBuf::from("saved.workload.toml"),
                },
                availability: Availability::Available,
            }]);
            app.panes[PaneKind::Workloads.index()].selected[0] = true;
            let Some(extension) = crate::workloads::setup_extension(workload) else {
                // Reference-only adapters deliberately have no fabricated
                // trial population. They remain unavailable until a
                // source-specific catalog adapter is added.
                assert_eq!(
                    crate::workloads::installed_available(workload, &root.join("workloads")),
                    Some(false),
                    "{workload}"
                );
                let _ = std::fs::remove_dir_all(root);
                continue;
            };
            // Do not require a downloaded catalog here: the invariant is that
            // one selected extension value publishes one configuration for
            // every external family, including catalog-backed LiveCodeBench.
            app.panes[PaneKind::Extensions.index()] = Pane::new(vec![Choice {
                label: "fixture-trial".into(),
                key: ChoiceKey::ExtensionValue {
                    workload: workload.into(),
                    extension: extension.id.into(),
                    value: "fixture-trial".into(),
                },
                availability: Availability::Available,
            }]);
            app.panes[PaneKind::Extensions.index()].selected[0] = true;
            assert_eq!(generate_configs(&app).unwrap().tests, 1, "{workload}");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn install_agentbench_catalog_fixture(root: &Path) {
        let workloads = root.join("workloads");
        let source = workloads.join("agentbench/source/configs/tasks");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            workloads.join("agentbench/ASB-BUNDLE"),
            "THUDM/AgentBench@d1e4a10db08c87075c78972e48ecc182be03e2d5\n",
        )
        .unwrap();
        std::fs::write(source.join("fixture.yaml"), "name: fixture\n").unwrap();
        crate::workloads::finalize_install("agentbench", &workloads).unwrap();
    }
}
