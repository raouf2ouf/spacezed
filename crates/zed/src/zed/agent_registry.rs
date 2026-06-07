//! Spacezed agent registry: app-global view of every agent running in a
//! spacezed terminal, fed by the state files that Claude Code hooks write
//! (see script/spacezed-agent-hook). Tier 1 of the Phase E agent cockpit.

use std::path::PathBuf;
use std::time::Duration;

use collections::HashMap;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use serde::Deserialize;
use std::sync::Arc;
use util::ResultExt as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Blocked,
    Idle,
}

// The state files carry more (session_id, cwd, message, ts) for the Phase E2
// agent picker; only what E1 consumes is deserialized here.
#[derive(Clone, Debug, Deserialize)]
pub struct AgentState {
    pub term_id: String,
    pub status: AgentStatus,
    pub pid: Option<i32>,
}

fn process_is_alive(pid: i32) -> bool {
    // Signal 0 performs error checking only; ESRCH means the process is gone.
    unsafe { libc::kill(pid, 0) == 0 }
}

pub struct AgentRegistry {
    agents: HashMap<String, AgentState>,
    _watch_task: Task<()>,
    _sweep_task: Task<()>,
}

struct GlobalAgentRegistry(Entity<AgentRegistry>);

impl Global for GlobalAgentRegistry {}

pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/state/spacezed/agents"))
}

impl AgentRegistry {
    pub fn init(fs: Arc<dyn Fs>, cx: &mut App) {
        if cx.has_global::<GlobalAgentRegistry>() {
            return;
        }
        let registry = cx.new(|cx| AgentRegistry::new(fs, cx));
        cx.set_global(GlobalAgentRegistry(registry));
    }

    pub fn global(cx: &App) -> Option<Entity<AgentRegistry>> {
        cx.try_global::<GlobalAgentRegistry>()
            .map(|global| global.0.clone())
    }

    fn new(fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let watch_task = cx.spawn(async move |this, cx| {
            let Some(directory) = state_dir() else {
                return;
            };
            std::fs::create_dir_all(&directory).log_err();

            let (mut events, _watcher) = fs.watch(&directory, Duration::from_millis(100)).await;
            if this
                .update(cx, |this, cx| this.rescan(cx))
                .log_err()
                .is_none()
            {
                return;
            }
            while events.next().await.is_some() {
                if this
                    .update(cx, |this, cx| this.rescan(cx))
                    .log_err()
                    .is_none()
                {
                    return;
                }
            }
        });

        // Liveness sweep: fs events only fire on writes, so a dead agent that
        // never writes again would ghost without this.
        let sweep_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(20))
                    .await;
                if this
                    .update(cx, |this, cx| this.rescan(cx))
                    .log_err()
                    .is_none()
                {
                    return;
                }
            }
        });

        Self {
            agents: HashMap::default(),
            _watch_task: watch_task,
            _sweep_task: sweep_task,
        }
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(directory) = state_dir() else {
            return;
        };
        let mut agents = HashMap::default();
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                if let Ok(contents) = std::fs::read_to_string(&path)
                    && let Ok(state) = serde_json::from_str::<AgentState>(&contents)
                {
                    // Ghost guard: a killed pane can race the Drop cleanup
                    // (the dying agent's hooks rewrite the state file), so
                    // dead processes are pruned here, file included.
                    if state.pid.is_some_and(|pid| !process_is_alive(pid)) {
                        std::fs::remove_file(&path).ok();
                        continue;
                    }
                    agents.insert(state.term_id.clone(), state);
                }
            }
        }
        let changed = agents.len() != self.agents.len()
            || agents.iter().any(|(term_id, state)| {
                self.agents
                    .get(term_id)
                    .is_none_or(|existing| existing.status != state.status)
            });
        if changed {
            self.agents = agents;
            cx.notify();
        }
    }

    pub fn count_with_status(&self, status: AgentStatus) -> usize {
        self.agents
            .values()
            .filter(|state| state.status == status)
            .count()
    }
}
