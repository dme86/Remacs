//! Explicit Git plugin installation with immutable, commit-pinned snapshots.
//! Network/process work runs off the editor thread. Registering, removing and
//! promoting versions use short, locked, atomic manifest transactions.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_PLUGINS: usize = 128;
const MAX_MANIFEST: u64 = 1024 * 1024;
const MAX_ENTRY: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT: u64 = 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(45);
const MANIFEST_NAME: &str = "manifest.json";
const SNAPSHOT_MARKER: &str = "remacs-plugin.json";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn main_branch() -> String {
    "main".into()
}
fn default_entry() -> String {
    "plugin.lua".into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub name: String,
    pub repo: String,
    #[serde(default = "main_branch")]
    pub branch: String,
    #[serde(default = "default_entry")]
    pub entry: String,
}

#[derive(Clone, Debug)]
pub struct Installed {
    pub name: String,
    pub root: PathBuf,
    pub entry: String,
    pub sha: String,
}

pub enum Event {
    Changed { name: String, updated: bool },
    UpdatesAvailable(Vec<String>),
    Message(String),
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    spec: Spec,
    #[serde(default)]
    current: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    plugins: BTreeMap<String, Record>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    spec: Spec,
    sha: String,
}

#[derive(Clone)]
enum Job {
    Install { spec: Spec, update: bool },
    Check(Vec<Record>),
}

impl Job {
    fn name(&self) -> Option<&str> {
        match self {
            Self::Install { spec, .. } => Some(&spec.name),
            Self::Check(_) => None,
        }
    }
}

enum Completed {
    Install {
        name: String,
        result: Result<Promotion, String>,
    },
    Check {
        available: BTreeMap<String, String>,
        checked: Vec<String>,
        errors: Vec<(String, String)>,
    },
}

struct Promotion {
    sha: String,
    changed: bool,
    updated: bool,
}

struct Active {
    job: Job,
    receiver: Receiver<Completed>,
    cancel: Cancellation,
}

#[derive(Clone, Default)]
struct Cancellation {
    cancelled: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Cancellation {
    fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Acquire) {
            Err("Plugin job cancelled".into())
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut slot = self.child.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(child) = slot.as_mut() {
            terminate(child);
        }
    }
}

pub struct Manager {
    root: PathBuf,
    manifest: Manifest,
    // The running Lua VM keeps its startup snapshots even after update/remove.
    active_versions: Vec<Installed>,
    pending: VecDeque<Job>,
    active: Option<Active>,
    failures: BTreeMap<String, String>,
    check_failures: BTreeMap<String, String>,
    available: BTreeMap<String, String>,
    events: VecDeque<Event>,
    timeout: Duration,
    git: OsString,
}

pub fn default_root() -> PathBuf {
    if let Some(path) = env::var_os("REMACS_PLUGIN_HOME").map(PathBuf::from) {
        if path.is_absolute() {
            return path;
        }
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Remacs/plugins")
    }
    #[cfg(not(target_os = "macos"))]
    {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"))
            .join("remacs/plugins")
    }
}

impl Manager {
    /// Read existing state only. No directories, lock files or network activity
    /// are created until an explicit add/install/update/check operation.
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or_else(|_| {
            if root.is_absolute() {
                root
            } else {
                env::current_dir().unwrap_or_default().join(root)
            }
        });
        let mut events = VecDeque::new();
        let manifest = match read_manifest(&root) {
            Ok(manifest) => manifest,
            Err(error) => {
                events.push_back(Event::Message(format!("Plugin manifest: {error}")));
                Manifest::default()
            }
        };
        let mut active_versions = Vec::new();
        for record in manifest.plugins.values() {
            if let Some(sha) = &record.current {
                let path = snapshot_path(&root, &record.spec.name, sha);
                match validate_snapshot(&path, &record.spec, sha, false) {
                    Ok(()) => active_versions.push(Installed {
                        name: record.spec.name.clone(),
                        root: path,
                        entry: record.spec.entry.clone(),
                        sha: sha.clone(),
                    }),
                    Err(error) => events.push_back(Event::Message(format!(
                        "Plugin {} not loaded: {error}",
                        record.spec.name
                    ))),
                }
            }
        }
        Self {
            root,
            manifest,
            active_versions,
            pending: VecDeque::new(),
            active: None,
            failures: BTreeMap::new(),
            check_failures: BTreeMap::new(),
            available: BTreeMap::new(),
            events,
            timeout: GIT_TIMEOUT,
            git: "git".into(),
        }
    }

    pub fn len(&self) -> usize {
        self.manifest.plugins.len()
    }
    pub fn update_count(&self) -> usize {
        self.available.len()
    }
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.pending.is_empty()
    }
    pub fn installed(&self) -> Vec<Installed> {
        self.active_versions.clone()
    }

    fn queued(&self, name: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.job.name() == Some(name))
            || self.pending.iter().any(|job| job.name() == Some(name))
    }

    fn reload(&mut self) -> Result<(), String> {
        self.manifest = read_manifest(&self.root)?;
        self.available.retain(|name, sha| {
            self.manifest.plugins.get(name).is_some_and(|record| {
                record
                    .current
                    .as_ref()
                    .is_some_and(|current| current != sha)
            })
        });
        Ok(())
    }

    pub fn add(&mut self, spec: Spec) -> Result<String, String> {
        let spec = normalize_spec(spec)?;
        let manifest = transaction(&self.root, false, |manifest| {
            if let Some(existing) = manifest.plugins.get(&spec.name) {
                if existing.spec != spec {
                    return Err(format!(
                        "Plugin {} is already registered with another source; remove it first",
                        spec.name
                    ));
                }
                return Ok(false);
            }
            if manifest.plugins.len() >= MAX_PLUGINS {
                return Err(format!(
                    "At most {MAX_PLUGINS} Git plugins can be registered"
                ));
            }
            manifest.plugins.insert(
                spec.name.clone(),
                Record {
                    spec: spec.clone(),
                    current: None,
                },
            );
            Ok(true)
        })?;
        self.manifest = manifest;
        self.enqueue(&spec.name, false)
    }

    pub fn remove(&mut self, name: &str) -> Result<String, String> {
        validate_name(name)?;
        if self.queued(name) {
            return Err(format!("Plugin {name} has a queued or running job"));
        }
        // Also protect against an installer in another Remacs instance.
        let _plugin_lock = plugin_lock(&self.root, name)?;
        let manifest = transaction(&self.root, false, |manifest| {
            if manifest.plugins.remove(name).is_none() {
                return Err(format!("Unknown plugin: {name}"));
            }
            Ok(true)
        })?;
        self.manifest = manifest;
        self.available.remove(name);
        self.failures.remove(name);
        Ok(format!("Removed {name}; takes effect after restart"))
    }

    pub fn queue(&mut self, name: &str, update: bool) -> Result<String, String> {
        self.reload()?;
        self.enqueue(name, update)
    }

    fn enqueue(&mut self, name: &str, update: bool) -> Result<String, String> {
        let record = self
            .manifest
            .plugins
            .get(name)
            .ok_or_else(|| format!("Unknown plugin: {name}"))?;
        if self.queued(name) {
            return Ok(format!("{name} is already queued or running"));
        }
        if record.current.is_some() && !update {
            return Ok(format!("{name} is installed"));
        }
        if record.current.is_none() && update {
            return Err(format!("{name} is not installed; install it first"));
        }
        self.failures.remove(name);
        self.pending.push_back(Job::Install {
            spec: record.spec.clone(),
            update,
        });
        Ok(format!(
            "Queued {name} for {}",
            if update { "update" } else { "installation" }
        ))
    }

    pub fn queue_all(&mut self, update: bool) -> usize {
        if let Err(error) = self.reload() {
            self.events.push_back(Event::Message(error));
            return 0;
        }
        let names: Vec<_> = self
            .manifest
            .plugins
            .iter()
            .filter(|(_, record)| record.current.is_some() == update)
            .map(|(name, _)| name.clone())
            .collect();
        let mut queued = 0;
        for name in names {
            let before = self.pending.len();
            if let Err(error) = self.enqueue(&name, update) {
                self.events.push_back(Event::Message(error));
            }
            queued += usize::from(self.pending.len() > before);
        }
        queued
    }

    pub fn check_all(&mut self) -> usize {
        if self
            .active
            .as_ref()
            .is_some_and(|active| matches!(active.job, Job::Check(_)))
            || self.pending.iter().any(|job| matches!(job, Job::Check(_)))
        {
            return 0;
        }
        if let Err(error) = self.reload() {
            self.events.push_back(Event::Message(error));
            return 0;
        }
        let records: Vec<_> = self
            .manifest
            .plugins
            .values()
            .filter(|record| record.current.is_some())
            .cloned()
            .collect();
        let count = records.len();
        if count > 0 {
            self.pending.push_back(Job::Check(records));
        }
        count
    }

    pub fn entries(&self, query: &str) -> Vec<(String, String)> {
        let query = query.to_lowercase();
        self.manifest
            .plugins
            .iter()
            .filter(|(name, record)| {
                let text =
                    format!("{} {} {}", name, record.spec.repo, record.spec.branch).to_lowercase();
                let mut letters = text.chars();
                query
                    .chars()
                    .all(|ch| letters.by_ref().any(|actual| actual == ch))
            })
            .map(|(name, record)| {
                let status = if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.job.name() == Some(name.as_str()))
                {
                    "installing…".into()
                } else if self
                    .pending
                    .iter()
                    .any(|job| job.name() == Some(name.as_str()))
                {
                    "queued".into()
                } else if let Some(error) = self.failures.get(name) {
                    format!("failed: {error}")
                } else if self.available.contains_key(name) {
                    "update available".into()
                } else if let Some(sha) = &record.current {
                    let restart = !self
                        .active_versions
                        .iter()
                        .any(|active| active.name == *name && active.sha == *sha);
                    format!(
                        "{}{}",
                        &sha[..sha.len().min(12)],
                        if restart {
                            " · restart required"
                        } else {
                            " · installed"
                        }
                    )
                } else {
                    "not installed".into()
                };
                (
                    name.clone(),
                    format!("{name}  {} · {status}", record.spec.branch),
                )
            })
            .collect()
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let completed = self
            .active
            .as_ref()
            .map(|active| active.receiver.try_recv());
        match completed {
            Some(Ok(result)) => {
                self.active = None;
                match result {
                    Completed::Install { name, result } => match result {
                        Ok(promotion) => {
                            self.available.remove(&name);
                            self.failures.remove(&name);
                            if let Err(error) = self.reload() {
                                self.events.push_back(Event::Message(error));
                            }
                            if promotion.changed {
                                self.events.push_back(Event::Changed {
                                    name,
                                    updated: promotion.updated,
                                });
                            } else {
                                self.events.push_back(Event::Message(format!(
                                    "{name} is up to date ({})",
                                    &promotion.sha[..12]
                                )));
                            }
                        }
                        Err(error) => {
                            self.failures.insert(name.clone(), error.clone());
                            self.events
                                .push_back(Event::Message(format!("Plugin {name}: {error}")));
                        }
                    },
                    Completed::Check {
                        available,
                        checked,
                        errors,
                    } => {
                        let previous = self.available.clone();
                        for name in checked {
                            self.available.remove(&name);
                            self.check_failures.remove(&name);
                        }
                        for (name, sha) in available {
                            if self.manifest.plugins.get(&name).is_some_and(|record| {
                                record
                                    .current
                                    .as_ref()
                                    .is_some_and(|current| current != &sha)
                            }) {
                                self.available.insert(name, sha);
                            }
                        }
                        for (name, error) in errors {
                            if self.check_failures.get(&name) != Some(&error) {
                                self.events.push_back(Event::Message(error.clone()));
                                self.check_failures.insert(name, error);
                            }
                        }
                        if self.available != previous && !self.available.is_empty() {
                            self.events.push_back(Event::UpdatesAvailable(
                                self.available.keys().cloned().collect(),
                            ));
                        }
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.active = None;
                self.events
                    .push_back(Event::Message("Plugin worker stopped unexpectedly".into()));
            }
            _ => {}
        }
        if self.active.is_none() {
            if let Some(job) = self.pending.pop_front() {
                let (sender, receiver) = mpsc::sync_channel(1);
                let root = self.root.clone();
                let timeout = self.timeout;
                let git = self.git.clone();
                let cancel = Cancellation::default();
                let worker_cancel = cancel.clone();
                let worker_job = job.clone();
                match thread::Builder::new()
                    .name("remacs-plugin-git".into())
                    .spawn(move || {
                        let result = execute_job(&root, worker_job, &git, timeout, &worker_cancel);
                        let _ = sender.send(result);
                    }) {
                    Ok(_) => {
                        self.active = Some(Active {
                            job,
                            receiver,
                            cancel,
                        })
                    }
                    Err(error) => self.events.push_back(Event::Message(format!(
                        "Cannot start plugin worker: {error}"
                    ))),
                }
            }
        }
        self.events
            .drain(..)
            .map(|event| match event {
                Event::Message(text) => Event::Message(display_text(&text, 1200)),
                event => event,
            })
            .collect()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.cancel();
        }
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err("Plugin names must start with a lowercase letter/digit and contain only a-z, 0-9, _ or - (64 characters maximum)".into());
    }
    Ok(())
}

fn normalize_spec(mut spec: Spec) -> Result<Spec, String> {
    if spec.branch.is_empty() {
        spec.branch = main_branch();
    }
    if spec.entry.is_empty() {
        spec.entry = default_entry();
    }
    validate_name(&spec.name)?;
    let branch = &spec.branch;
    if branch.len() > 256
        || branch.starts_with('-')
        || branch == "@"
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || "~^:?*[\\".contains(ch))
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err("Invalid Git branch name".into());
    }
    let entry = Path::new(&spec.entry);
    if spec.entry.len() > 1024 || spec.entry.contains(['\\', ':']) || spec.entry.chars().any(char::is_control)
        || entry.extension().and_then(|extension| extension.to_str()) != Some("lua")
        || !entry.components().all(|component| matches!(component, Component::Normal(value) if value.to_str().is_some_and(|value| !value.eq_ignore_ascii_case(".git"))))
        || spec.entry.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err("Plugin entry must be a relative .lua file inside the repository".into());
    }
    validate_repo(&spec.repo)?;
    Ok(spec)
}

fn validate_repo(repo: &str) -> Result<(), String> {
    if repo.is_empty()
        || repo.len() > 4096
        || repo.starts_with('-')
        || repo.chars().any(char::is_control)
    {
        return Err("Invalid Git repository".into());
    }
    if Path::new(repo).is_absolute() {
        return Ok(());
    }
    if repo.contains("://") {
        let url = url::Url::parse(repo).map_err(|_| "Invalid Git repository URL")?;
        if !matches!(url.scheme(), "https" | "ssh" | "git" | "file") || url.password().is_some() {
            return Err(
                "Use an https, ssh, git or local file repository URL without a password".into(),
            );
        }
        if url.scheme() == "file" {
            if url
                .to_file_path()
                .ok()
                .is_none_or(|path| !path.is_absolute())
            {
                return Err("Invalid local file repository URL".into());
            }
        } else if url
            .host_str()
            .is_none_or(|host| host.is_empty() || host.starts_with('-'))
        {
            return Err("Invalid Git repository host".into());
        }
        return Ok(());
    }
    // SCP-style SSH URLs: intentionally exclude shell metacharacters even
    // though the repository remains one argument to Git.
    let Some((host, path)) = repo.split_once(':') else {
        return Err("Use an absolute local repository path or an explicit Git URL".into());
    };
    if host.is_empty()
        || host.starts_with('-')
        || path.is_empty()
        || path.starts_with('-')
        || !host
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"@._-".contains(&ch))
        || !path
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"/._~-".contains(&ch))
    {
        return Err("Invalid SCP-style Git repository".into());
    }
    Ok(())
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && sha
            .bytes()
            .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))
}

fn read_manifest(root: &Path) -> Result<Manifest, String> {
    let path = root.join(MANIFEST_NAME);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST {
        return Err("Manifest is not a regular file or exceeds 1 MiB".into());
    }
    let manifest: Manifest = serde_json::from_slice(&read_bounded(&path, MAX_MANIFEST)?)
        .map_err(|error| error.to_string())?;
    if manifest.version != 1 || manifest.plugins.len() > MAX_PLUGINS {
        return Err("Unsupported or oversized plugin manifest".into());
    }
    for (name, record) in &manifest.plugins {
        if *name != record.spec.name
            || normalize_spec(record.spec.clone())? != record.spec
            || record.current.as_ref().is_some_and(|sha| !valid_sha(sha))
        {
            return Err("Invalid plugin record in manifest".into());
        }
    }
    Ok(manifest)
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!("Expected a regular directory: {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn lock_file(path: &Path) -> Result<File, String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err("Plugin lock path is not a regular file".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => "Plugin state is busy in another Remacs instance".into(),
        TryLockError::Error(error) => error.to_string(),
    })?;
    Ok(file)
}

fn plugin_lock(root: &Path, name: &str) -> Result<File, String> {
    ensure_directory(root)?;
    let directory = root.join("locks");
    ensure_directory(&directory)?;
    lock_file(&directory.join(format!("{name}.lock")))
}

fn transaction(
    root: &Path,
    wait: bool,
    change: impl FnOnce(&mut Manifest) -> Result<bool, String>,
) -> Result<Manifest, String> {
    ensure_directory(root)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let _lock = loop {
        match lock_file(&root.join("manifest.lock")) {
            Ok(file) => break file,
            Err(error) if wait && error.contains("busy") && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    };
    let mut manifest = read_manifest(root)?;
    if change(&mut manifest)? {
        write_manifest(root, &manifest)?;
    }
    Ok(manifest)
}

fn write_manifest(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = root.join(format!(
        ".manifest-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut created = false;
    let result = (|| -> io::Result<()> {
        let mut file = options.open(&path)?;
        created = true;
        serde_json::to_writer_pretty(&mut file, manifest).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&path, root.join(MANIFEST_NAME))?;
        // Rename is the commit point: a directory-sync failure must not
        // report an unsuccessful transaction after the new state is visible.
        if let Ok(directory) = File::open(root) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(path);
    }
    result.map_err(|error| error.to_string())
}

fn snapshot_path(root: &Path, name: &str, sha: &str) -> PathBuf {
    root.join("packages").join(name).join(sha)
}

fn entry_source(root: &Path, entry: &str) -> Result<String, String> {
    let mut path = root.to_path_buf();
    if !fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err("Snapshot is not a regular directory".into());
    }
    for component in Path::new(entry).components() {
        let Component::Normal(component) = component else {
            return Err("Invalid entry path".into());
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Entry {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err("Plugin entry must not follow symlinks".into());
        }
    }
    let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ENTRY {
        return Err("Plugin entry must be a regular UTF-8 Lua file of at most 2 MiB".into());
    }
    String::from_utf8(read_bounded(&path, MAX_ENTRY)?).map_err(|error| error.to_string())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(format!("Invalid or oversized file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| error.to_string())?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > maximum {
        return Err("Plugin file exceeded its size limit".into());
    }
    Ok(bytes)
}

fn validate_snapshot(root: &Path, spec: &Spec, sha: &str, parse: bool) -> Result<(), String> {
    let git = root.join(".git");
    if !fs::symlink_metadata(&git).is_ok_and(|metadata| metadata.is_dir()) {
        return Err("Invalid snapshot Git metadata".into());
    }
    let marker: Snapshot =
        serde_json::from_slice(&read_bounded(&git.join(SNAPSHOT_MARKER), 16 * 1024)?)
            .map_err(|error| error.to_string())?;
    if marker.spec != *spec || marker.sha != sha {
        return Err("Snapshot source/commit does not match its manifest".into());
    }
    let source = entry_source(root, &spec.entry)?;
    if parse {
        validate_lua(&source, &spec.entry)?;
    }
    Ok(())
}

fn validate_lua(source: &str, entry: &str) -> Result<(), String> {
    mlua::Lua::new()
        .load(source)
        .set_name(entry)
        .set_mode(mlua::ChunkMode::Text)
        .into_function()
        .map(|_| ())
        .map_err(|error| format!("Invalid Lua entry: {error}"))
}

struct Temporary {
    path: PathBuf,
}

impl Temporary {
    fn new(parent: &Path, label: &str) -> Result<Self, String> {
        for _ in 0..8 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = parent.join(format!(
                "remacs-{label}-{}-{nanos}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("Cannot allocate a private plugin workspace".into())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        // Only this invocation's freshly-created workspace is removable.
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_dir()) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

fn configure_process(command: &mut Command) {
    command.stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and runs only between fork/exec.
        // A private session also prevents SSH from prompting on the editor TTY.
        unsafe {
            command.pre_exec(|| {
                if setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: the unreaped child owns a session/process group created by
        // configure_process; the negative PID addresses only that group.
        unsafe {
            kill(-pid, 9);
        }
    }
    let _ = child.kill();
}

struct Output {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct ProcessGuard<'a>(&'a Cancellation);

impl Drop for ProcessGuard<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .0
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(mut child) = slot.take() {
            terminate(&mut child);
            let _ = child.wait();
        }
    }
}

fn run_process(
    command: &mut Command,
    workspace: &Path,
    timeout: Duration,
    cancel: &Cancellation,
) -> Result<Output, String> {
    cancel.check()?;
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut stdout = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(workspace.join(format!("out-{sequence}")))
        .map_err(|error| error.to_string())?;
    let mut stderr = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(workspace.join(format!("err-{sequence}")))
        .map_err(|error| error.to_string())?;
    command
        .stdout(Stdio::from(
            stdout.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            stderr.try_clone().map_err(|error| error.to_string())?,
        ));
    configure_process(command);
    let mut slot = cancel
        .child
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cancel.check()?;
    *slot = Some(command.spawn().map_err(|error| error.to_string())?);
    drop(slot);
    let _guard = ProcessGuard(cancel);
    let deadline = Instant::now() + timeout;
    let status = loop {
        let reason = if cancel.cancelled.load(Ordering::Acquire) {
            Some("Plugin job cancelled")
        } else if Instant::now() >= deadline {
            Some("Git operation timed out")
        } else if stdout.metadata().map_err(|error| error.to_string())?.len() > MAX_OUTPUT
            || stderr.metadata().map_err(|error| error.to_string())?.len() > MAX_OUTPUT
        {
            Some("Git output exceeded 1 MiB")
        } else {
            None
        };
        let mut slot = cancel
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let child = slot.as_mut().ok_or("Plugin process disappeared")?;
        if let Some(reason) = reason {
            terminate(child);
            let _ = child.wait();
            slot.take();
            return Err(reason.into());
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => {
                slot.take();
                break status;
            }
            None => {
                drop(slot);
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    stderr
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout
        .take(MAX_OUTPUT)
        .read_to_end(&mut out)
        .map_err(|error| error.to_string())?;
    stderr
        .take(MAX_OUTPUT)
        .read_to_end(&mut err)
        .map_err(|error| error.to_string())?;
    Ok(Output {
        status,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

fn git_command(git: &OsString) -> Command {
    let mut command = Command::new(git);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_SSH_COMMAND",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_SSH", "ssh")
        .env("GIT_SSH_VARIANT", "ssh")
        .arg("--no-pager");
    for config in [
        "core.hooksPath=/dev/null",
        "init.templateDir=",
        "core.askPass=",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "protocol.ssh.allow=always",
        "protocol.git.allow=always",
        "protocol.file.allow=always",
        "protocol.ext.allow=never",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
        "core.autocrlf=false",
        "core.protectHFS=true",
        "core.protectNTFS=true",
        "advice.detachedHead=false",
    ] {
        command.args(["-c", config]);
    }
    command
}

fn git_output(
    command: &mut Command,
    workspace: &Path,
    timeout: Duration,
    cancel: &Cancellation,
) -> Result<String, String> {
    let output = run_process(command, workspace, timeout, cancel)?;
    if !output.status.success() {
        let detail: String = display_text(&output.stderr, 4000)
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(800)
            .collect();
        return Err(format!("Git failed ({}): {detail}", output.status));
    }
    Ok(output.stdout)
}

fn display_text(text: &str, maximum: usize) -> String {
    let mut characters = text.chars().peekable();
    let mut output = String::new();
    let mut count = 0;
    while let Some(ch) = characters.next() {
        if ch == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for ch in characters.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(ch) = characters.next() {
                        if ch == '\u{7}'
                            || (ch == '\u{1b}' && characters.next_if_eq(&'\\').is_some())
                        {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let ch = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if ch.is_control() {
            continue;
        }
        if count >= maximum {
            break;
        }
        output.push(ch);
        count += 1;
    }
    output
}

fn remote_head(
    spec: &Spec,
    git: &OsString,
    workspace: &Path,
    timeout: Duration,
    cancel: &Cancellation,
) -> Result<String, String> {
    let reference = format!("refs/heads/{}", spec.branch);
    let text = git_output(
        git_command(git).current_dir(workspace).args([
            "ls-remote",
            "--exit-code",
            "--heads",
            "--",
            &spec.repo,
            &reference,
        ]),
        workspace,
        timeout,
        cancel,
    )?;
    let sha = text
        .lines()
        .find_map(|line| {
            let (sha, found) = line.split_once('\t')?;
            (found == reference && valid_sha(sha)).then(|| sha.to_string())
        })
        .ok_or_else(|| format!("Remote branch {} was not found", spec.branch))?;
    Ok(sha)
}

fn execute_job(
    root: &Path,
    job: Job,
    git: &OsString,
    timeout: Duration,
    cancel: &Cancellation,
) -> Completed {
    match job {
        Job::Install { spec, update } => {
            let result = install(root, &spec, update, git, timeout, cancel);
            Completed::Install {
                name: spec.name,
                result,
            }
        }
        Job::Check(records) => {
            let mut available = BTreeMap::new();
            let mut checked = Vec::new();
            let mut errors = Vec::new();
            for record in records {
                if cancel.check().is_err() {
                    break;
                }
                let result =
                    Temporary::new(&env::temp_dir(), "plugin-check").and_then(|workspace| {
                        remote_head(&record.spec, git, &workspace.path, timeout, cancel)
                    });
                match result {
                    Ok(sha) => {
                        checked.push(record.spec.name.clone());
                        if record.current.as_ref() != Some(&sha) {
                            available.insert(record.spec.name, sha);
                        }
                    }
                    Err(error) => errors.push((
                        record.spec.name.clone(),
                        format!("Plugin {} update check: {error}", record.spec.name),
                    )),
                }
            }
            Completed::Check {
                available,
                checked,
                errors,
            }
        }
    }
}

fn install(
    root: &Path,
    spec: &Spec,
    update: bool,
    git: &OsString,
    timeout: Duration,
    cancel: &Cancellation,
) -> Result<Promotion, String> {
    let _plugin_lock = plugin_lock(root, &spec.name)?;
    let registered = read_manifest(root)?;
    if registered
        .plugins
        .get(&spec.name)
        .is_none_or(|record| record.spec != *spec)
    {
        return Err("Plugin registration changed before installation".into());
    }
    let workspace = Temporary::new(&env::temp_dir(), "plugin-git")?;
    let remote = remote_head(spec, git, &workspace.path, timeout, cancel)?;
    let packages = root.join("packages");
    ensure_directory(&packages)?;
    let package = packages.join(&spec.name);
    ensure_directory(&package)?;
    let mut sha = remote;
    let mut target = snapshot_path(root, &spec.name, &sha);
    let existed = fs::symlink_metadata(&target).is_ok();
    if existed {
        validate_snapshot(&target, spec, &sha, true)?;
    } else {
        let staging = Temporary::new(&package, "staging")?;
        let checkout = staging.path.join("source");
        git_output(
            git_command(git)
                .current_dir(&workspace.path)
                .args([
                    "clone",
                    "--no-checkout",
                    "--no-local",
                    "--no-hardlinks",
                    "--single-branch",
                    "--depth",
                    "1",
                    "--branch",
                    &spec.branch,
                    "--",
                    &spec.repo,
                ])
                .arg(&checkout),
            &workspace.path,
            timeout,
            cancel,
        )?;
        let reference = format!("refs/remotes/origin/{}^{{commit}}", spec.branch);
        sha = git_output(
            git_command(git)
                .current_dir(&checkout)
                .args(["rev-parse", "--verify", &reference]),
            &workspace.path,
            timeout,
            cancel,
        )?
        .trim()
        .to_string();
        if !valid_sha(&sha) {
            return Err("Git returned an invalid commit ID".into());
        }
        git_output(
            git_command(git)
                .current_dir(&checkout)
                .args(["checkout", "--detach", &sha, "--"]),
            &workspace.path,
            timeout,
            cancel,
        )?;
        let source = entry_source(&checkout, &spec.entry)?;
        validate_lua(&source, &spec.entry)?;
        let marker = Snapshot {
            spec: spec.clone(),
            sha: sha.clone(),
        };
        let marker_path = checkout.join(".git").join(SNAPSHOT_MARKER);
        let mut marker_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker_path)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut marker_file, &marker).map_err(|error| error.to_string())?;
        marker_file.sync_all().map_err(|error| error.to_string())?;
        cancel.check()?;
        target = snapshot_path(root, &spec.name, &sha);
        if fs::symlink_metadata(&target).is_ok() {
            validate_snapshot(&target, spec, &sha, true)?;
        } else {
            fs::rename(&checkout, &target).map_err(|error| error.to_string())?;
        }
    }
    cancel.check()?;
    let mut changed = !existed;
    let mut was_installed = false;
    transaction(root, true, |manifest| {
        cancel.check()?;
        let record = manifest
            .plugins
            .get_mut(&spec.name)
            .ok_or("Plugin was removed during installation")?;
        if record.spec != *spec {
            return Err("Plugin registration changed during installation".into());
        }
        was_installed = record.current.is_some();
        changed |= record.current.as_ref() != Some(&sha);
        record.current = Some(sha.clone());
        Ok(changed)
    })?;
    Ok(Promotion {
        sha,
        changed,
        updated: update && was_installed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_tests::Project;

    fn spec(project: &Project, name: &str) -> Spec {
        Spec {
            name: name.into(),
            repo: project.0.join("remote.git").to_str().unwrap().into(),
            branch: "main".into(),
            entry: "plugin.lua".into(),
        }
    }

    fn finish(manager: &mut Manager) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut events = Vec::new();
        loop {
            events.extend(manager.poll());
            if !manager.is_busy() {
                break;
            }
            assert!(Instant::now() < deadline, "plugin worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        events
    }

    #[test]
    fn invalid_specs_and_corrupt_manifests_never_overwrite_state() {
        let project = Project::new();
        let root = project.0.join("managed");
        let mut manager = Manager::new(root.clone());
        assert!(!root.exists());
        for invalid in [
            Spec {
                name: "../escape".into(),
                ..spec(&project, "demo")
            },
            Spec {
                repo: "ext::sh -c anything".into(),
                ..spec(&project, "demo")
            },
            Spec {
                repo: "https://user:secret@example.invalid/repo".into(),
                ..spec(&project, "demo")
            },
            Spec {
                branch: "--upload-pack=evil".into(),
                ..spec(&project, "demo")
            },
            Spec {
                entry: "../outside.lua".into(),
                ..spec(&project, "demo")
            },
        ] {
            assert!(manager.add(invalid).is_err());
        }
        assert!(!root.exists());
        fs::create_dir(&root).unwrap();
        fs::write(root.join(MANIFEST_NAME), "not valid json").unwrap();
        assert!(manager.add(spec(&project, "demo")).is_err());
        assert_eq!(
            fs::read_to_string(root.join(MANIFEST_NAME)).unwrap(),
            "not valid json"
        );
        assert_eq!(manager.len(), 0);
    }

    #[test]
    fn invalid_lua_update_preserves_the_previous_installed_snapshot() {
        let project = Project::new();
        project.publish(1);
        let root = project.0.join("managed");
        let mut manager = Manager::new(root.clone());
        manager.add(spec(&project, "demo")).unwrap();
        let events = finish(&mut manager);
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Changed { updated: false, .. })));
        let old = read_manifest(&root).unwrap().plugins["demo"]
            .current
            .clone()
            .unwrap();
        fs::write(
            project.0.join("work/plugin.lua"),
            "this is not valid Lua !!!",
        )
        .unwrap();
        project.git(&["-C", "work", "commit", "-am", "broken plugin entry"]);
        project.git(&[
            "-C",
            "work",
            "push",
            project.0.join("remote.git").to_str().unwrap(),
            "main",
        ]);
        manager.queue("demo", true).unwrap();
        let events = finish(&mut manager);
        assert!(events.iter().any(
            |event| matches!(event, Event::Message(error) if error.contains("Invalid Lua entry"))
        ));
        assert_eq!(
            read_manifest(&root).unwrap().plugins["demo"]
                .current
                .as_ref(),
            Some(&old)
        );
        assert_eq!(Manager::new(root).installed()[0].sha, old);
    }

    #[test]
    fn checks_only_notify_new_shas_and_offline_checks_keep_working_versions() {
        let project = Project::new();
        project.publish(1);
        let root = project.0.join("managed");
        let mut manager = Manager::new(root.clone());
        manager.add(spec(&project, "demo")).unwrap();
        finish(&mut manager);
        let old = read_manifest(&root).unwrap().plugins["demo"]
            .current
            .clone();
        assert_eq!(manager.check_all(), 1);
        assert!(!finish(&mut manager)
            .iter()
            .any(|event| matches!(event, Event::UpdatesAvailable(_))));
        project.publish(2);
        manager.check_all();
        assert!(finish(&mut manager)
            .iter()
            .any(|event| matches!(event, Event::UpdatesAvailable(names) if names == &["demo"])));
        manager.check_all();
        assert!(!finish(&mut manager)
            .iter()
            .any(|event| matches!(event, Event::UpdatesAvailable(_))));
        assert_eq!(manager.update_count(), 1);
        fs::rename(project.0.join("remote.git"), project.0.join("offline.git")).unwrap();
        manager.check_all();
        assert!(finish(&mut manager)
            .iter()
            .any(|event| matches!(event, Event::Message(_))));
        assert_eq!(manager.update_count(), 1);
        assert_eq!(read_manifest(&root).unwrap().plugins["demo"].current, old);
        assert_eq!(Manager::new(root).installed().len(), 1);
    }

    #[test]
    fn registration_transactions_merge_instances_and_reject_source_changes() {
        let project = Project::new();
        let root = project.0.join("managed");
        let mut first = Manager::new(root.clone());
        let mut second = Manager::new(root.clone());
        first.add(spec(&project, "one")).unwrap();
        second.add(spec(&project, "two")).unwrap();
        assert_eq!(read_manifest(&root).unwrap().plugins.len(), 2);
        assert!(first
            .add(Spec {
                branch: "other".into(),
                ..spec(&project, "one")
            })
            .is_err());
        assert!(second.queue("one", true).is_err());
        assert!(
            first.remove("one").is_err(),
            "queued work must not be removed"
        );
        let lock = plugin_lock(&root, "one").unwrap();
        assert!(
            second.remove("one").is_err(),
            "another instance's lock must be respected"
        );
        drop(lock);
        second.remove("one").unwrap();
        assert_eq!(read_manifest(&root).unwrap().plugins.len(), 1);
        assert!(first.queue("one", false).is_err());
    }

    #[test]
    fn timeouts_terminate_git_process_groups_and_error_text_is_terminal_safe() {
        let project = Project::new();
        let workspace = Temporary::new(&project.0, "timeout").unwrap();
        let started = Instant::now();
        let error = run_process(
            Command::new("sleep").arg("5"),
            &workspace.path,
            Duration::from_millis(50),
            &Cancellation::default(),
        )
        .err()
        .expect("sleep should time out");
        assert!(
            error.to_lowercase().contains("timeout") || error.to_lowercase().contains("timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let clean = display_text("error\x1b[2J\x1b]0;title\x07\r\n\tmessage", 100);
        assert!(!clean.chars().any(char::is_control));
        assert!(clean.contains("error") && clean.contains("message"));
        assert!(!clean.contains("title") && !clean.contains("[2J"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_package_directories_are_not_followed_or_deleted() {
        use std::os::unix::fs::symlink;
        let project = Project::new();
        project.publish(1);
        let root = project.0.join("managed");
        let outside = project.0.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "untouched").unwrap();
        symlink(&outside, root.join("packages")).unwrap();
        let mut manager = Manager::new(root);
        manager.add(spec(&project, "demo")).unwrap();
        assert!(finish(&mut manager)
            .iter()
            .any(|event| matches!(event, Event::Message(_))));
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "untouched"
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
    }
}
