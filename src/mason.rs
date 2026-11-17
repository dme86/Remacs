//! Nonblocking installer backend for plugins/mason.lua.
//! Only typed package-manager argv is executed; no shell evaluation, no
//! arbitrary remote archive extraction and no background network on startup
//! unless explicitly enabled in Lua. Managed binaries live under Remacs data.
use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use serde::{Deserialize, Serialize};

use crate::mason_plugin::{MasonPlugin, Recipe, Tool};

#[derive(Clone)]
struct Task {
    id: String,
    update: bool,
}

struct Active {
    task: Task,
    child: Child,
}

#[derive(Default, Serialize, Deserialize)]
struct History {
    #[serde(default)]
    updated_at: HashMap<String, u64>,
}

pub enum Event {
    Done { id: String, language: String, updated: bool },
    Message(String),
}

pub struct Manager {
    tools: Vec<Tool>,
    root: PathBuf,
    pending: VecDeque<Task>,
    active: Option<Active>,
    failures: HashMap<String, String>,
    history: History,
}

pub fn default_root() -> PathBuf {
    if let Some(value) = env::var_os("REMACS_MASON_HOME") {
        let path = PathBuf::from(value);
        if path.is_absolute() { return path; }
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    { home.join("Library/Application Support/Remacs/mason") }
    #[cfg(not(target_os = "macos"))]
    {
        env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty()).map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("remacs/mason")
    }
}

fn executable_on_path(program: &str) -> Option<PathBuf> {
    // No absolute binary names from Lua may be silently rewritten to the
    // managed root; explicitly configured custom executables are respected.
    let name = Path::new(program);
    if name.is_absolute() {
        return name.is_file().then(|| name.to_path_buf());
    }
    if name.components().count() != 1 { return None; }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path).map(|dir| dir.join(program)).find(|file| file.is_file())
    })
}

pub fn managed_executable(program: &str) -> Option<PathBuf> {
    let name = Path::new(program);
    if name.components().count() != 1 { return None; }
    let path = default_root().join("bin").join(program);
    path.is_file().then_some(path)
}

pub fn resolve_command(program: &str) -> String {
    managed_executable(program)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_owned())
}

impl Manager {
    pub fn new(plugin: &MasonPlugin) -> mlua::Result<Self> {
        let root = default_root();
        let history = fs::read(root.join("history.json")).ok()
            .and_then(|bytes| serde_json::from_slice::<History>(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            tools: plugin.tools()?, root, pending: VecDeque::new(),
            active: None, failures: HashMap::new(), history,
        })
    }

    pub fn tools(&self) -> &[Tool] { &self.tools }
    pub fn log_path(&self) -> PathBuf { self.root.join("install.log") }
    pub fn root(&self) -> &Path { &self.root }

    pub fn has(&self, tool: &Tool) -> bool {
        self.root.join("bin").join(&tool.command[0]).is_file()
            || executable_on_path(&tool.command[0]).is_some()
    }

    pub fn status(&self, tool: &Tool) -> String {
        if let Some(job) = &self.active {
            if job.task.id == tool.id {
                return if job.task.update { "updating…".into() } else { "installing…".into() };
            }
        }
        if self.pending.iter().any(|job| job.id == tool.id) {
            return "queued".into();
        }
        if let Some(error) = self.failures.get(&tool.id) {
            return format!("failed: {error}");
        }
        if self.root.join("bin").join(&tool.command[0]).is_file() {
            return "installed · managed".into();
        }
        if executable_on_path(&tool.command[0]).is_some() {
            return "installed · system".into();
        }
        "missing".into()
    }

    pub fn entries(&self, query: &str) -> Vec<(String, String)> {
        let query = query.to_lowercase();
        self.tools.iter().filter(|tool| {
            // Fuzzy subsequence on tool ID and visible language label.
            let haystack = format!("{} {}", tool.id, tool.label).to_lowercase();
            let mut letters = haystack.chars();
            query.chars().all(|c| letters.by_ref().any(|ch| ch == c))
        }).map(|tool| (
            tool.id.clone(),
            format!("{}  {} · {}", tool.id, tool.label, self.status(tool))
        )).collect()
    }

    pub fn queue(&mut self, id: &str, update: bool) -> Result<String, String> {
        let Some(tool) = self.tools.iter().find(|tool| tool.id == id) else {
            return Err(format!("Unknown language server: {id}"));
        };
        let exists = self.has(tool);
        if exists && !update {
            return Ok(format!("{} is already available", tool.id));
        }
        if self.active.as_ref().is_some_and(|job| job.task.id == id)
            || self.pending.iter().any(|job| job.id == id) {
            return Ok(format!("{id} already queued or installing"));
        }
        self.failures.remove(id);
        self.pending.push_back(Task { id: id.to_string(), update });
        Ok(format!("Queued {id} for {}", if update { "update" } else { "installation" }))
    }

    pub fn queue_all(&mut self, update: bool) -> usize {
        let ids: Vec<_> = self.tools.iter().map(|tool| tool.id.clone()).collect();
        let mut queued = 0;
        for id in ids {
            let before = self.pending.len();
            let _ = self.queue(&id, update);
            if self.pending.len() > before { queued += 1; }
        }
        queued
    }

    pub fn auto_queue(&mut self, plugin: &MasonPlugin) -> mlua::Result<()> {
        if plugin.auto_install()? { self.queue_all(false); }
        let interval = plugin.auto_update_days()?;
        if interval > 0 {
            let now = now_secs();
            let interval = interval.saturating_mul(86_400);
            let ids: Vec<_> = self.tools.iter().filter(|tool| self.has(tool)
                && self.history.updated_at.get(&tool.id).is_some_and(|last|
                    now.saturating_sub(*last) >= interval))
                .map(|tool| tool.id.clone()).collect();
            for id in ids { let _ = self.queue(&id, true); }
        }
        Ok(())
    }

    fn command_for(&self, tool: &Tool, update: bool) -> Command {
        let mut cmd = match &tool.recipe {
            Recipe::Rustup(components) => {
                let mut command = Command::new("rustup");
                command.arg("component").arg("add").args(components);
                command
            }
            Recipe::Go(module) => {
                let mut command = Command::new("go");
                command.arg("install").arg(module).env("GOBIN", self.root.join("bin"));
                command
            }
            Recipe::Npm(package) => {
                let mut command = Command::new("npm");
                command.args(["install", "--global", "--prefix"])
                    .arg(&self.root)
                    .args(["--no-audit", "--no-fund", package]);
                command
            }
            Recipe::Cargo(package) => {
                let mut command = Command::new("cargo");
                command.arg("install").arg("--locked").arg("--root").arg(&self.root)
                    .arg(package);
                if update { command.arg("--force"); }
                command
            }
            Recipe::Brew(package) => {
                let mut command = Command::new("brew");
                command.arg(if update && self.has(tool) { "upgrade" } else { "install" })
                    .arg("--formula").arg(package);
                command
            }
        };
        // Run package-manager tools from a stable application directory,
        // never from the user's current source tree or untrusted project.
        cmd.current_dir(&self.root);
        cmd
    }

    fn ensure_data_directory(&self) -> io::Result<()> {
        fs::create_dir_all(self.root.join("bin"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Do not chmod existing directories (e.g. a user-controlled
            // XDG_DATA_HOME); only newly created private app directories.
            // create_dir_all has already succeeded; keep defaults on existing.
        }
        Ok(())
    }

    fn open_log(&self) -> io::Result<File> {
        let mut opts = OpenOptions::new();
        opts.create(true).append(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(self.log_path())
    }

    fn start_next(&mut self, events: &mut Vec<Event>) {
        if self.active.is_some() { return; }
        while let Some(task) = self.pending.pop_front() {
            let Some(tool) = self.tools.iter().find(|tool| tool.id == task.id) else { continue };
            let setup = self.ensure_data_directory()
                .and_then(|()| self.open_log());
            let mut log = match setup {
                Ok(log) => log,
                Err(error) => {
                    self.failures.insert(task.id.clone(), error.to_string());
                    events.push(Event::Message(format!(
                        "Mason {} failed: {error}", task.id
                    )));
                    continue;
                }
            };
            let _ = writeln!(log, "\n=== {} {} ===",
                if task.update { "Update" } else { "Install" }, tool.id);
            let child = self.command_for(tool, task.update)
                .stdout(Stdio::from(match log.try_clone() {
                    Ok(log) => log,
                    Err(error) => {
                        events.push(Event::Message(format!("Mason log error: {error}")));
                        continue;
                    }
                }))
                .stderr(Stdio::from(log))
                .spawn();
            match child {
                Ok(child) => {
                    events.push(Event::Message(format!(
                        "Mason {} {} (see {})",
                        if task.update { "updating" } else { "installing" },
                        task.id, self.log_path().display()
                    )));
                    self.active = Some(Active { task, child });
                    break;
                }
                Err(error) => {
                    self.failures.insert(task.id.clone(), error.to_string());
                    events.push(Event::Message(format!(
                        "Mason {} failed: {error} (installer unavailable?)", task.id
                    )));
                }
            }
        }
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(active) = &mut self.active {
            match active.child.try_wait() {
                Ok(Some(status)) => {
                    let finished = self.active.take().expect("completed job");
                    let id = finished.task.id;
                    if status.success() {
                        if let Some(tool) = self.tools.iter().find(|tool| tool.id == id) {
                            events.push(Event::Done {
                                id: id.clone(), language: tool.language.clone(),
                                updated: finished.task.update,
                            });
                        }
                        self.history.updated_at.insert(id, now_secs());
                        let _ = self.persist_history();
                    } else {
                        let error = format!("exit {status}; see {}", self.log_path().display());
                        self.failures.insert(id.clone(), error.clone());
                        events.push(Event::Message(format!("Mason {id} failed: {error}")));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let id = active.task.id.clone();
                    self.failures.insert(id.clone(), error.to_string());
                    self.active = None;
                    events.push(Event::Message(format!("Mason {id}: {error}")));
                }
            }
        }
        self.start_next(&mut events);
        events
    }

    fn persist_history(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.history).map_err(io::Error::other)?;
        let target = self.root.join("history.json");
        let temp = self.root.join(format!("history-{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temp, target)
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(active) = &mut self.active {
            let _ = active.child.kill();
            let _ = active.child.wait();
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn fixture() -> Manager {
        let plugin = MasonPlugin::new().unwrap();
        let mut manager = Manager::new(&plugin).unwrap();
        manager.root = std::env::temp_dir().join(format!(
            "remacs-mason-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        manager
    }

    #[test]
    fn no_downloads_or_updates_without_explicit_opt_in() {
        let plugin = MasonPlugin::new().unwrap();
        let mut manager = fixture();
        assert!(manager.pending.is_empty());
        manager.auto_queue(&plugin).unwrap();
        assert!(manager.pending.is_empty());
        assert!(manager.active.is_none());
    }

    #[test]
    fn installer_commands_use_argv_not_shell_and_write_to_managed_root() {
        let manager = fixture();
        let rust = manager.tools.iter().find(|tool| tool.id == "rust_analyzer").unwrap();
        let cmd = manager.command_for(rust, false);
        assert_eq!(cmd.get_program(), OsStr::new("rustup"));
        assert_eq!(cmd.get_args().map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(), ["component", "add", "rust-analyzer", "rust-src"]);
        let go = manager.tools.iter().find(|tool| tool.id == "gopls").unwrap();
        let cmd = manager.command_for(go, false);
        assert_eq!(cmd.get_program(), OsStr::new("go"));
        assert_eq!(cmd.get_args().next(), Some(OsStr::new("install")));
        assert!(cmd.get_envs().any(|(key,val)| key == OsStr::new("GOBIN")
            && val.is_some_and(|val| val == manager.root.join("bin"))));
        let npm = manager.tools.iter().find(|tool| tool.id == "yamlls").unwrap();
        let cmd = manager.command_for(npm, false);
        assert_eq!(cmd.get_program(), OsStr::new("npm"));
        assert!(cmd.get_args().any(|arg| arg == OsStr::new("--prefix")));
        assert!(cmd.get_args().any(|arg| arg == OsStr::new("yaml-language-server@latest")));
    }

    #[test]
    fn queue_rejects_unknown_tools_and_deduplicates() {
        let mut manager = fixture();
        assert!(manager.queue("does_not_exist", false).is_err());
        assert!(manager.queue("jsonls", true).unwrap().contains("Queued"));
        assert!(manager.queue("jsonls", true).unwrap().contains("already queued"));
        assert_eq!(manager.pending.len(), 1);
        assert!(manager.entries("yaml").iter().any(|(id,_)| id == "yamlls"));
    }

    #[test]
    fn managed_binary_is_recognized_and_installed_tools_are_not_reinstalled() {
        let mut manager = fixture();
        fs::create_dir_all(manager.root.join("bin")).unwrap();
        fs::write(manager.root.join("bin/gopls"), b"fixture").unwrap();
        let tool = manager.tools.iter().find(|tool| tool.id == "gopls").unwrap();
        assert!(manager.has(tool));
        assert_eq!(manager.status(tool), "installed · managed");
        assert!(manager.queue("gopls", false).unwrap().contains("already available"));
        assert!(manager.pending.is_empty());
        assert!(manager.queue("gopls", true).unwrap().contains("Queued"));
        let _ = fs::remove_dir_all(&manager.root);
    }
}
