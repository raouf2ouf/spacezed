//! Spacezed agent cockpit actions (Phase E). E1 ships NewAgentTerminal: a
//! plain center-pane terminal running the agent launcher -- task terminals
//! misbehave with streaming TUIs, so the launcher is typed into a fresh
//! login shell exactly like a human would.

use gpui::actions;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Context, Window};
use workspace::Workspace;

actions!(agents, [NewAgentTerminal]);

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
