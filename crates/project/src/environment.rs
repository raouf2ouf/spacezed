use anyhow::{Context as _, bail};
use futures::{FutureExt, StreamExt as _, channel::mpsc, future::Shared};
use language::Buffer;
use remote::RemoteClient;
use rpc::proto::{self, REMOTE_SERVER_PROJECT_ID};
use std::{collections::VecDeque, path::Path, sync::Arc};
use task::{Shell, shell_to_proto};
use terminal::terminal_settings::TerminalSettings;
use util::{ResultExt, command::new_command, rel_path::RelPath};
use worktree::Worktree;

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, WeakEntity};
use settings::Settings as _;

use crate::{
    project_settings::{DirenvSettings, ProjectEnvironmentSettings, ProjectSettings},
    worktree_store::WorktreeStore,
};

pub struct ProjectEnvironment {
    cli_environment: Option<HashMap<String, String>>,
    local_environments: HashMap<(Shell, Arc<Path>), Shared<Task<Option<HashMap<String, String>>>>>,
    remote_environments: HashMap<(Shell, Arc<Path>), Shared<Task<Option<HashMap<String, String>>>>>,
    probe_directory_remaps: HashMap<(ProjectEnvironmentSettings, Arc<Path>), Arc<Path>>,
    environment_error_messages: VecDeque<String>,
    environment_error_messages_tx: mpsc::UnboundedSender<String>,
    worktree_store: WeakEntity<WorktreeStore>,
    remote_client: Option<WeakEntity<RemoteClient>>,
    is_remote_project: bool,
    _tasks: Vec<Task<()>>,
}

pub enum ProjectEnvironmentEvent {
    ErrorsUpdated,
}

impl EventEmitter<ProjectEnvironmentEvent> for ProjectEnvironment {}

impl ProjectEnvironment {
    pub fn new(
        cli_environment: Option<HashMap<String, String>>,
        worktree_store: WeakEntity<WorktreeStore>,
        remote_client: Option<WeakEntity<RemoteClient>>,
        is_remote_project: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded();
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                this.update(cx, |this, cx| {
                    this.environment_error_messages.push_back(message);
                    cx.emit(ProjectEnvironmentEvent::ErrorsUpdated);
                })
                .ok();
            }
        });
        Self {
            cli_environment,
            local_environments: Default::default(),
            remote_environments: Default::default(),
            probe_directory_remaps: Default::default(),
            environment_error_messages: Default::default(),
            environment_error_messages_tx: tx,
            worktree_store,
            remote_client,
            is_remote_project,
            _tasks: vec![task],
        }
    }

    /// Returns the inherited CLI environment, if this project was opened from the Zed CLI.
    pub(crate) fn get_cli_environment(&self) -> Option<HashMap<String, String>> {
        if cfg!(any(test, feature = "test-support")) {
            return Some(HashMap::default());
        }
        if let Some(mut env) = self.cli_environment.clone() {
            set_origin_marker(&mut env, EnvironmentOrigin::Cli);
            Some(env)
        } else {
            None
        }
    }

    pub fn buffer_environment(
        &mut self,
        buffer: &Entity<Buffer>,
        worktree_store: &Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        let Some(worktree) = buffer
            .read(cx)
            .file()
            .map(|f| f.worktree_id(cx))
            .and_then(|worktree_id| worktree_store.read(cx).worktree_for_id(worktree_id, cx))
        else {
            return Task::ready(None).shared();
        };
        self.worktree_environment(worktree, cx)
    }

    pub fn worktree_environment(
        &mut self,
        worktree: Entity<Worktree>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        let worktree = worktree.read(cx);
        let mut abs_path = worktree.abs_path();
        if worktree.is_single_file() {
            let Some(parent) = abs_path.parent() else {
                return Task::ready(None).shared();
            };
            abs_path = parent.into();
        }

        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment(
                    &Shell::Program(shell),
                    abs_path,
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => {
                Some(self.local_directory_environment(&Shell::System, abs_path, cx))
            }
            None => Some({
                let shell = TerminalSettings::get(
                    Some(settings::SettingsLocation {
                        worktree_id: worktree.id(),
                        path: RelPath::empty(),
                    }),
                    cx,
                )
                .shell
                .clone();

                self.local_directory_environment(&shell, abs_path, cx)
            }),
        }
        .unwrap_or_else(|| Task::ready(None).shared())
    }

    pub fn directory_environment(
        &mut self,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment(
                    &Shell::Program(shell),
                    abs_path,
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => {
                Some(self.local_directory_environment(&Shell::System, abs_path, cx))
            }
            None => self
                .worktree_store
                .read_with(cx, |worktree_store, cx| {
                    worktree_store.find_worktree(&abs_path, cx)
                })
                .ok()
                .map(|worktree| {
                    let shell = terminal::terminal_settings::TerminalSettings::get(
                        worktree
                            .as_ref()
                            .map(|(worktree, path)| settings::SettingsLocation {
                                worktree_id: worktree.read(cx).id(),
                                path: &path,
                            }),
                        cx,
                    )
                    .shell
                    .clone();

                    self.local_directory_environment(&shell, abs_path, cx)
                }),
        }
        .unwrap_or_else(|| Task::ready(None).shared())
    }

    /// Returns the project environment, if possible.
    /// If the project was opened from the CLI, then the inherited CLI environment is returned.
    /// If it wasn't opened from the CLI, and an absolute path is given, then a shell is spawned in
    /// that directory, to get environment variables as if the user has `cd`'d there.
    pub fn local_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        let abs_path = match ProjectSettings::get_global(cx).project_environment {
            ProjectEnvironmentSettings::All => abs_path,
            ProjectEnvironmentSettings::Off => {
                return Task::ready(Some(HashMap::default())).shared();
            }
            mode @ (ProjectEnvironmentSettings::TopLevel
            | ProjectEnvironmentSettings::RootOnly) => {
                self.probe_directory(mode, abs_path, cx)
            }
        };

        self.local_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let load_direnv = ProjectSettings::get_global(cx).load_direnv.clone();
                let shell = shell.clone();
                let tx = self.environment_error_messages_tx.clone();
                cx.spawn(async move |cx| {
                    let mut shell_env = match cx
                        .background_spawn(load_directory_shell_environment(
                            shell,
                            abs_path.clone(),
                            load_direnv,
                            tx,
                        ))
                        .await
                    {
                        Ok(shell_env) => Some(shell_env),
                        Err(e) => {
                            log::error!(
                                "Failed to load shell environment for directory {abs_path:?}: {e:#}"
                            );
                            None
                        }
                    };

                    if let Some(shell_env) = shell_env.as_mut() {
                        let path = shell_env
                            .get("PATH")
                            .map(|path| path.as_str())
                            .unwrap_or_default();
                        log::debug!(
                            "using project environment variables shell launched in {:?}. PATH={:?}",
                            abs_path,
                            path
                        );

                        set_origin_marker(shell_env, EnvironmentOrigin::WorktreeShell);
                    }

                    shell_env
                })
                .shared()
            })
            .clone()
    }

    pub fn remote_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        remote_client: Entity<RemoteClient>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if cfg!(any(test, feature = "test-support")) {
            return Task::ready(Some(HashMap::default())).shared();
        }

        self.remote_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let response =
                    remote_client
                        .read(cx)
                        .proto_client()
                        .request(proto::GetDirectoryEnvironment {
                            project_id: REMOTE_SERVER_PROJECT_ID,
                            shell: Some(shell_to_proto(shell.clone())),
                            directory: abs_path.to_string_lossy().to_string(),
                        });
                cx.background_spawn(async move {
                    let environment = response.await.log_err()?;
                    Some(environment.environment.into_iter().collect())
                })
                .shared()
            })
            .clone()
    }

    /// Maps a requested directory to the directory whose environment should
    /// be probed, per the `project_environment` setting. Coalescing nested
    /// directories onto one path lets `local_environments` deduplicate the
    /// expensive login-shell probes (e.g. one per nested git submodule).
    fn probe_directory(
        &mut self,
        mode: ProjectEnvironmentSettings,
        abs_path: Arc<Path>,
        cx: &App,
    ) -> Arc<Path> {
        if let Some(remapped) = self.probe_directory_remaps.get(&(mode, abs_path.clone())) {
            return remapped.clone();
        }

        let worktree_root = self
            .worktree_store
            .read_with(cx, |worktree_store, cx| {
                worktree_store
                    .find_worktree(&abs_path, cx)
                    .and_then(|(worktree, _)| {
                        let worktree = worktree.read(cx);
                        if worktree.is_single_file() {
                            worktree.abs_path().parent().map(Arc::from)
                        } else {
                            Some(worktree.abs_path())
                        }
                    })
            })
            .ok()
            .flatten();

        let remapped = match (mode, worktree_root) {
            (_, None) => abs_path.clone(),
            (ProjectEnvironmentSettings::RootOnly, Some(root)) => root,
            (ProjectEnvironmentSettings::TopLevel, Some(root)) => {
                outermost_repository_in(&root, &abs_path).unwrap_or_else(|| abs_path.clone())
            }
            (ProjectEnvironmentSettings::All | ProjectEnvironmentSettings::Off, Some(_)) => {
                abs_path.clone()
            }
        };

        self.probe_directory_remaps
            .insert((mode, abs_path), remapped.clone());
        remapped
    }

    pub fn peek_environment_error(&self) -> Option<&String> {
        self.environment_error_messages.front()
    }

    pub fn pop_environment_error(&mut self) -> Option<String> {
        self.environment_error_messages.pop_front()
    }
}

/// Returns the outermost directory at or below `root` on the path to
/// `abs_path` that contains a `.git` entry (a repository or submodule
/// boundary; `.git` is a file for submodules, so check existence, not
/// directory-ness).
fn outermost_repository_in(root: &Path, abs_path: &Path) -> Option<Arc<Path>> {
    let relative = abs_path.strip_prefix(root).ok()?;
    let mut current = root.to_path_buf();
    if current.join(".git").exists() {
        return Some(Arc::from(current.as_path()));
    }
    for component in relative.components() {
        current.push(component);
        if current.join(".git").exists() {
            return Some(Arc::from(current.as_path()));
        }
    }
    None
}

fn set_origin_marker(env: &mut HashMap<String, String>, origin: EnvironmentOrigin) {
    env.insert(ZED_ENVIRONMENT_ORIGIN_MARKER.to_string(), origin.into());
}

const ZED_ENVIRONMENT_ORIGIN_MARKER: &str = "ZED_ENVIRONMENT";

enum EnvironmentOrigin {
    Cli,
    WorktreeShell,
}

impl From<EnvironmentOrigin> for String {
    fn from(val: EnvironmentOrigin) -> Self {
        match val {
            EnvironmentOrigin::Cli => "cli".into(),
            EnvironmentOrigin::WorktreeShell => "worktree-shell".into(),
        }
    }
}

async fn load_directory_shell_environment(
    shell: Shell,
    abs_path: Arc<Path>,
    load_direnv: DirenvSettings,
    tx: mpsc::UnboundedSender<String>,
) -> anyhow::Result<HashMap<String, String>> {
    if let DirenvSettings::Disabled = load_direnv {
        return Ok(HashMap::default());
    }

    let meta = smol::fs::metadata(&abs_path).await.with_context(|| {
        tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
            .ok();
        format!("stat {abs_path:?}")
    })?;

    let dir = if meta.is_dir() {
        abs_path.clone()
    } else {
        abs_path
            .parent()
            .with_context(|| {
                tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
                    .ok();
                format!("getting parent of {abs_path:?}")
            })?
            .into()
    };

    let (shell, args) = shell.program_and_args();
    let mut envs = util::shell_env::capture(shell.clone(), args, abs_path)
        .await
        .with_context(|| {
            tx.unbounded_send("Failed to load environment variables".into())
                .ok();
            format!("capturing shell environment with {shell:?}")
        })?;

    if cfg!(target_os = "windows")
        && let Some(path) = envs.remove("Path")
    {
        // windows env vars are case-insensitive, so normalize the path var
        // so we can just assume `PATH` in other places
        envs.insert("PATH".into(), path);
    }
    // If the user selects `Direct` for direnv, it would set an environment
    // variable that later uses to know that it should not run the hook.
    // We would include in `.envs` call so it is okay to run the hook
    // even if direnv direct mode is enabled.
    let direnv_environment = match load_direnv {
        DirenvSettings::ShellHook => None,
        DirenvSettings::Disabled => bail!("direnv integration is disabled"),
        // Note: direnv is not available on Windows, so we skip direnv processing
        // and just return the shell environment
        DirenvSettings::Direct if cfg!(target_os = "windows") => None,
        DirenvSettings::Direct => load_direnv_environment(&envs, &dir)
            .await
            .with_context(|| {
                tx.unbounded_send("Failed to load direnv environment".into())
                    .ok();
                "load direnv environment"
            })
            .log_err(),
    };
    if let Some(direnv_environment) = direnv_environment {
        for (key, value) in direnv_environment {
            if let Some(value) = value {
                envs.insert(key, value);
            } else {
                envs.remove(&key);
            }
        }
    }

    Ok(envs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outermost_repository_in() {
        let temp = tempfile::tempdir().expect("creating temp dir");
        let root = temp.path();
        let nested = root.join("vendored").join("lib").join("submodule");
        std::fs::create_dir_all(nested.join(".git")).expect("creating nested .git");

        assert_eq!(
            outermost_repository_in(root, nested.parent().expect("parent of nested")),
            None,
            "no repository on the chain to the parent directory"
        );
        assert_eq!(
            outermost_repository_in(root, &nested).as_deref(),
            Some(nested.as_path()),
            "the requested directory itself is the outermost repository"
        );

        let intermediate = root.join("vendored");
        std::fs::write(intermediate.join(".git"), "gitdir: elsewhere")
            .expect("creating gitlink file");
        assert_eq!(
            outermost_repository_in(root, &nested).as_deref(),
            Some(intermediate.as_path()),
            "a gitlink file at an intermediate directory shadows deeper repositories"
        );

        std::fs::create_dir(root.join(".git")).expect("creating root .git");
        assert_eq!(
            outermost_repository_in(root, &nested).as_deref(),
            Some(root),
            "a repository at the root shadows everything below it"
        );

        assert_eq!(
            outermost_repository_in(&nested, root),
            None,
            "paths outside the root are not remapped"
        );
    }
}

async fn load_direnv_environment(
    env: &HashMap<String, String>,
    dir: &Path,
) -> anyhow::Result<HashMap<String, Option<String>>> {
    let Some(direnv_path) = which::which("direnv").ok() else {
        return Ok(HashMap::default());
    };

    let args = &["export", "json"];
    let direnv_output = new_command(&direnv_path)
        .args(args)
        .envs(env)
        .env("TERM", "dumb")
        .current_dir(dir)
        .output()
        .await
        .context("running direnv")?;

    if !direnv_output.status.success() {
        bail!(
            "Loading direnv environment failed ({}), stderr: {}",
            direnv_output.status,
            String::from_utf8_lossy(&direnv_output.stderr)
        );
    }

    let output = String::from_utf8_lossy(&direnv_output.stdout);
    if output.is_empty() {
        // direnv outputs nothing when it has no changes to apply to environment variables
        return Ok(HashMap::default());
    }

    serde_json::from_str(&output).context("parsing direnv json")
}
