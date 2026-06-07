//! Spacezed agent cockpit actions (Phase E). NewAgentTerminal spawns a
//! plain center-pane terminal running the agent launcher -- task terminals
//! misbehave with streaming TUIs, so the launcher is typed into a fresh
//! login shell exactly like a human would. FocusNextBlocked jumps to the
//! agent most deserving of attention, across ALL windows.

use crate::zed::agent_registry::{AgentRegistry, AgentState, AgentStatus};
use crate::zed::spaceline;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task, WeakEntity,
    actions,
};
use picker::{Picker, PickerDelegate};
use std::path::Path;
use std::sync::Arc;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{Context, HighlightedLabel, ListItem, ListItemSpacing, Window, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Pane, Workspace};

actions!(agents, [NewAgentTerminal, FocusNextBlocked, ListAgents]);

/// Resolved via the shell's PATH, so login-shell setup (nvm, ~/.local/bin)
/// applies as usual.
const AGENT_COMMAND: &str = "cccp";

pub fn new_agent_terminal(
    workspace: &mut Workspace,
    _: &NewAgentTerminal,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let working_directory = workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .map(|path| path.to_path_buf());
    let terminal =
        TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
            project.create_terminal_shell(working_directory, cx)
        });
    cx.spawn(async move |_, cx| {
        let terminal = terminal.await?;
        terminal.update(cx, |terminal, _| {
            terminal.input(format!("{AGENT_COMMAND}\n").into_bytes());
        })
    })
    .detach_and_log_err(cx);
}

pub fn focus_next_blocked(
    _workspace: &mut Workspace,
    _: &FocusNextBlocked,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(registry) = AgentRegistry::global(cx) else {
        return;
    };
    let Some(term_id) = registry
        .read(cx)
        .attention_target()
        .map(|state| state.term_id.clone())
    else {
        return;
    };
    // Deferred so other windows can be updated outside this window's cycle.
    cx.defer(move |cx| focus_agent_terminal(&term_id, cx));
}

fn focus_agent_terminal(term_id: &str, cx: &mut App) {
    for window_handle in cx.windows() {
        let Some(workspace_window) = window_handle.downcast::<Workspace>() else {
            continue;
        };
        let found = workspace_window
            .update(cx, |workspace, window, cx| {
                // Center panes first, then the terminal dock's panes.
                let center_panes: Vec<Entity<Pane>> = workspace.panes().to_vec();
                let dock_panes: Vec<Entity<Pane>> = workspace
                    .panel::<TerminalPanel>(cx)
                    .map(|panel| {
                        panel
                            .read(cx)
                            .panes()
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                for (pane, in_dock) in center_panes
                    .iter()
                    .map(|pane| (pane, false))
                    .chain(dock_panes.iter().map(|pane| (pane, true)))
                {
                    let target_index = pane.read(cx).items().enumerate().find_map(
                        |(index, item)| {
                            let terminal_view = item.downcast::<TerminalView>()?;
                            (terminal_view.read(cx).terminal().read(cx).spacezed_term_id()
                                == Some(term_id))
                            .then_some(index)
                        },
                    );
                    if let Some(index) = target_index {
                        window.activate_window();
                        if in_dock {
                            workspace.open_panel::<TerminalPanel>(window, cx);
                        }
                        pane.update(cx, |pane, cx| {
                            pane.activate_item(index, true, true, window, cx);
                        });
                        return true;
                    }
                }
                false
            })
            .unwrap_or(false);
        if found {
            return;
        }
    }
}

pub fn list_agents(
    workspace: &mut Workspace,
    _: &ListAgents,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(registry) = AgentRegistry::global(cx) else {
        return;
    };
    let agents = registry.read(cx).agents_by_urgency();
    if agents.is_empty() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    workspace.toggle_modal(window, cx, move |window, cx| {
        AgentPicker::new(agents, now, window, cx)
    });
}

struct AgentEntry {
    state: AgentState,
    label: String,
}

pub struct AgentPicker {
    picker: Entity<Picker<AgentPickerDelegate>>,
}

impl AgentPicker {
    fn new(
        agents: Vec<AgentState>,
        now: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries: Vec<AgentEntry> = agents
            .into_iter()
            .map(|state| {
                let directory = state
                    .cwd
                    .as_deref()
                    .and_then(|cwd| Path::new(cwd).file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let status_word = match state.status {
                    AgentStatus::Blocked => "blocked",
                    AgentStatus::Working => "working",
                    AgentStatus::Idle => "idle",
                };
                let age = state
                    .ts
                    .map(|ts| format_age(now.saturating_sub(ts)))
                    .unwrap_or_default();
                let label = format!("{directory}  {status_word}  {age}");
                AgentEntry { state, label }
            })
            .collect();
        let delegate = AgentPickerDelegate {
            agent_picker: cx.entity().downgrade(),
            matches: Vec::new(),
            entries,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for AgentPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AgentPicker")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl Focusable for AgentPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for AgentPicker {}
impl ModalView for AgentPicker {}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

pub struct AgentPickerDelegate {
    agent_picker: WeakEntity<AgentPicker>,
    matches: Vec<StringMatch>,
    entries: Vec<AgentEntry>,
    selected_index: usize,
}

impl PickerDelegate for AgentPickerDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Jump to agent…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index)
            && let Some(entry) = self.entries.get(mat.candidate_id)
        {
            let term_id = entry.state.term_id.clone();
            window.defer(cx, move |_, cx| focus_agent_terminal(&term_id, cx));
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.agent_picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.label))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };
            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let entry = self.entries.get(mat.candidate_id)?;
        let (glyph, color) = match entry.state.status {
            AgentStatus::Blocked => ("◉", spaceline::agent_blocked_color()),
            AgentStatus::Working => ("●", spaceline::agent_working_color()),
            AgentStatus::Idle => ("○", spaceline::agent_idle_color()),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(div().text_color(color).child(glyph))
                .child(HighlightedLabel::new(
                    entry.label.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}
