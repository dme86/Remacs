//! Persistent, bounded global jumplist and Vim-style named marks.
//! Only filenames and cursor coordinates are stored; no file contents,
//! unsaved changes, undo snapshots or open-buffer state are persisted.
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

const MAX_JUMPS: usize = 200;
const MAX_CHANGES: usize = 200;
const MAX_MARKS: usize = 1024;
const MAX_STATE_BYTES: u64 = 256 * 1024;
static TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub path: PathBuf,
    pub row: usize,
    pub col: usize,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    history: Vec<Location>,
    cursor: Option<usize>,
    changes: Vec<Location>,
    marks: BTreeMap<String, Location>,
}

pub struct JumpManager {
    saved: Saved,
    state_path: Option<PathBuf>,
    change_cursor: usize,
}

pub fn default_state_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    {
        home.map(|dir| dir.join("Library/Application Support/Remacs/jumps.json"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|dir| dir.join(".local/state")))
            .map(|base| base.join("remacs/jumps.json"))
    }
}

impl JumpManager {
    pub fn memory_only() -> Self {
        Self {
            saved: Saved::default(),
            state_path: None,
            change_cursor: 0,
        }
    }

    pub fn load_default() -> Self {
        match default_state_path() {
            Some(path) => Self::from_path(path),
            None => Self::memory_only(),
        }
    }

    pub fn from_path(path: PathBuf) -> Self {
        let saved = fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= MAX_STATE_BYTES)
            .and_then(|_| fs::read(&path).ok())
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .unwrap_or_default();
        let saved = Saved {
            history: saved
                .history
                .into_iter()
                .filter(|entry| entry.path.is_absolute())
                .rev()
                .take(MAX_JUMPS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            cursor: None,
            changes: saved
                .changes
                .into_iter()
                .filter(|entry| entry.path.is_absolute())
                .rev()
                .take(MAX_CHANGES)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            marks: saved
                .marks
                .into_iter()
                .filter(|(_, loc)| loc.path.is_absolute())
                .take(MAX_MARKS)
                .collect(),
        };
        let cursor = saved.history.len().checked_sub(1);
        let change_cursor = saved.changes.len();
        Self {
            saved: Saved { cursor, ..saved },
            state_path: Some(path),
            change_cursor,
        }
    }

    pub fn entries(&self) -> &[Location] {
        &self.saved.history
    }
    /// Whether a forward jump is possible after Ctrl+O.
    #[cfg(test)]
    pub fn can_forward(&self) -> bool {
        self.saved
            .cursor
            .is_some_and(|index| index + 1 < self.saved.history.len())
    }
    pub fn changes(&self) -> &[Location] {
        &self.saved.changes
    }

    // Explicit jumps preserve distinct columns, including same-line mark
    // and definition jumps. Only consecutive identical origins coalesce.
    pub fn remember(&mut self, location: Location) -> io::Result<()> {
        if !location.path.is_absolute() {
            return Ok(());
        }
        // A new jump branches from the current history entry, even when
        // its origin exactly matches that entry. Otherwise Ctrl+O from the
        // new target would skip its origin and retain an unrelated future.
        let truncated = self.saved.cursor.is_some_and(|index| {
            if index + 1 < self.saved.history.len() {
                self.saved.history.truncate(index + 1);
                true
            } else {
                false
            }
        });
        if self.saved.history.last() == Some(&location) {
            self.saved.cursor = self.saved.history.len().checked_sub(1);
            return if truncated { self.persist() } else { Ok(()) };
        }
        self.saved.history.push(location);
        if self.saved.history.len() > MAX_JUMPS {
            self.saved.history.remove(0);
        }
        self.saved.cursor = self.saved.history.len().checked_sub(1);
        self.persist()
    }

    /// Return older jump; capture the live origin at the newest end so a
    /// later forward jump can return to exactly where the user started.
    // Traversal cursors are session-only (load resets them), so moving
    // through existing entries needs no disk write or synchronous fsync.
    pub fn back(&mut self, current: Option<Location>) -> io::Result<Option<Location>> {
        if self.saved.cursor == self.saved.history.len().checked_sub(1) {
            if let Some(current) = current {
                self.remember(current)?;
            }
        }
        let Some(index) = self.saved.cursor else {
            return Ok(None);
        };
        if index == 0 {
            return Ok(None);
        }
        self.saved.cursor = Some(index - 1);
        Ok(self.saved.history.get(index - 1).cloned())
    }

    pub fn forward(&mut self) -> io::Result<Option<Location>> {
        let Some(index) = self.saved.cursor else {
            return Ok(None);
        };
        if index + 1 >= self.saved.history.len() {
            return Ok(None);
        }
        self.saved.cursor = Some(index + 1);
        Ok(self.saved.history.get(index + 1).cloned())
    }

    #[cfg(test)]
    fn at(&mut self, index: usize) -> io::Result<Option<Location>> {
        if index >= self.saved.history.len() {
            return Ok(None);
        }
        self.saved.cursor = Some(index);
        Ok(self.saved.history.get(index).cloned())
    }

    /// Remacs extension of Vim's changelist: edits across *all* open files,
    /// with position recording once per edit command / insert session.
    pub fn record_change(&mut self, location: Location) -> io::Result<()> {
        if !location.path.is_absolute() {
            return Ok(());
        }
        // A new edit discards the forward traversal of the old change list.
        if self.change_cursor < self.saved.changes.len() {
            self.saved.changes.truncate(self.change_cursor + 1);
        }
        if let Some(last) = self.saved.changes.last_mut() {
            if last.path == location.path && last.row == location.row {
                *last = location;
                self.change_cursor = self.saved.changes.len();
                return self.persist();
            }
        }
        self.saved.changes.push(location);
        if self.saved.changes.len() > MAX_CHANGES {
            self.saved.changes.remove(0);
        }
        self.change_cursor = self.saved.changes.len();
        self.persist()
    }

    /// g; starts with the newest edit, then moves to older edit sites.
    pub fn older_change(&mut self) -> io::Result<Option<Location>> {
        if self.change_cursor == 0 {
            return Ok(None);
        }
        self.change_cursor = self.change_cursor.saturating_sub(1);
        Ok(self.saved.changes.get(self.change_cursor).cloned())
    }

    /// g, moves forward through the previous edit sites.
    pub fn newer_change(&mut self) -> io::Result<Option<Location>> {
        if self.change_cursor + 1 >= self.saved.changes.len() {
            return Ok(None);
        }
        self.change_cursor += 1;
        Ok(self.saved.changes.get(self.change_cursor).cloned())
    }

    /// Lowercase marks are file-local; uppercase marks are global.
    fn mark_key(name: char, file: &Path) -> Option<String> {
        if name.is_ascii_uppercase() {
            Some(format!("global:{name}"))
        } else if name.is_ascii_lowercase() {
            Some(format!("local:{}:{name}", file.display()))
        } else {
            None
        }
    }

    pub fn set_mark(&mut self, name: char, location: Location) -> io::Result<bool> {
        let Some(key) = Self::mark_key(name, &location.path) else {
            return Ok(false);
        };
        if !self.saved.marks.contains_key(&key) && self.saved.marks.len() >= MAX_MARKS {
            // Retain a bounded number of persisted marks.
            self.saved.marks.pop_first();
        }
        self.saved.marks.insert(key, location);
        self.persist()?;
        Ok(true)
    }

    pub fn mark(&self, name: char, current_file: &Path) -> Option<Location> {
        let key = Self::mark_key(name, current_file)?;
        self.saved.marks.get(&key).cloned()
    }

    fn persist(&self) -> io::Result<()> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid jump state path",
            ));
        };
        // Never chmod an existing parent directory. A custom state path
        // may be directly under /tmp or an XDG shared directory; changing
        // those permissions would be a serious bug.
        let create_parent = !parent.exists();
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        if create_parent {
            use std::os::unix::fs::PermissionsExt;
            // The new app-specific directory is private; existing directories
            // and their permissions are left untouched.
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let bytes = serde_json::to_vec(&self.saved).map_err(io::Error::other)?;
        let nonce = TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), nonce));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(file: &str, row: usize, col: usize) -> Location {
        Location {
            path: std::env::temp_dir().join(file),
            row,
            col,
        }
    }

    #[test]
    fn global_changes_can_be_navigated_across_closed_buffers() {
        let mut store = JumpManager::memory_only();
        store.record_change(place("first.rs", 2, 0)).unwrap();
        store.record_change(place("second.rs", 12, 4)).unwrap();
        store.record_change(place("third.md", 8, 3)).unwrap();
        assert_eq!(store.older_change().unwrap(), Some(place("third.md", 8, 3)));
        assert_eq!(
            store.older_change().unwrap(),
            Some(place("second.rs", 12, 4))
        );
        assert_eq!(store.older_change().unwrap(), Some(place("first.rs", 2, 0)));
        assert_eq!(store.older_change().unwrap(), None);
        assert_eq!(
            store.newer_change().unwrap(),
            Some(place("second.rs", 12, 4))
        );
    }

    #[test]
    fn repeated_changes_on_same_line_coalesce() {
        let mut store = JumpManager::memory_only();
        store.record_change(place("f.rs", 8, 0)).unwrap();
        store.record_change(place("f.rs", 8, 10)).unwrap();
        assert_eq!(store.changes(), &[place("f.rs", 8, 10)]);
    }

    #[test]
    fn backward_forward_revisits_edits_across_files() {
        let mut store = JumpManager::memory_only();
        store.remember(place("one.rs", 10, 2)).unwrap();
        store.remember(place("two.rs", 30, 4)).unwrap();
        assert_eq!(
            store.back(Some(place("two.rs", 30, 4))).unwrap(),
            Some(place("one.rs", 10, 2))
        );
        assert_eq!(store.forward().unwrap(), Some(place("two.rs", 30, 4)));
        assert_eq!(store.forward().unwrap(), None);
    }

    #[test]
    fn forward_available_only_while_retracing_older_jumps() {
        let mut store = JumpManager::memory_only();
        for row in [2, 12, 22] {
            store.remember(place("main.rs", row, 0)).unwrap();
        }
        assert!(!store.can_forward());
        assert_eq!(store.back(None).unwrap(), Some(place("main.rs", 12, 0)));
        assert!(store.can_forward());
        assert_eq!(store.back(None).unwrap(), Some(place("main.rs", 2, 0)));
        assert!(store.can_forward());
        assert_eq!(store.forward().unwrap(), Some(place("main.rs", 12, 0)));
        assert!(store.can_forward());
        assert_eq!(store.forward().unwrap(), Some(place("main.rs", 22, 0)));
        assert!(!store.can_forward());
    }

    #[test]
    fn captures_live_origin_then_uses_forward_history() {
        let mut store = JumpManager::memory_only();
        store.remember(place("one.rs", 10, 2)).unwrap();
        assert_eq!(
            store.back(Some(place("two.rs", 40, 1))).unwrap(),
            Some(place("one.rs", 10, 2))
        );
        assert_eq!(store.forward().unwrap(), Some(place("two.rs", 40, 1)));
    }

    #[test]
    fn editing_after_backward_jump_truncates_forward_history() {
        let mut store = JumpManager::memory_only();
        for row in [1, 12, 22] {
            store.remember(place("main.rs", row, 0)).unwrap();
        }
        assert_eq!(store.back(None).unwrap(), Some(place("main.rs", 12, 0)));
        store.remember(place("other.rs", 5, 0)).unwrap();
        assert_eq!(store.forward().unwrap(), None);
        assert_eq!(store.entries().len(), 3);
    }

    #[test]
    fn new_jump_from_exact_history_entry_discards_future_and_keeps_origin() {
        let mut store = JumpManager::memory_only();
        for row in [1, 12, 22] {
            store.remember(place("main.rs", row, 0)).unwrap();
        }
        assert_eq!(store.back(None).unwrap(), Some(place("main.rs", 12, 0)));
        store.remember(place("main.rs", 12, 0)).unwrap();
        assert_eq!(store.entries().len(), 2);
        assert!(!store.can_forward());
        assert_eq!(
            store.back(Some(place("target.rs", 4, 2))).unwrap(),
            Some(place("main.rs", 12, 0))
        );
        assert_eq!(store.forward().unwrap(), Some(place("target.rs", 4, 2)));
    }

    #[test]
    fn successive_same_line_jumps_retain_every_exact_column() {
        let mut store = JumpManager::memory_only();
        store.remember(place("main.rs", 8, 1)).unwrap();
        store.remember(place("main.rs", 8, 30)).unwrap();
        assert_eq!(
            store.back(Some(place("main.rs", 8, 50))).unwrap(),
            Some(place("main.rs", 8, 30))
        );
        assert_eq!(store.back(None).unwrap(), Some(place("main.rs", 8, 1)));
        assert_eq!(store.forward().unwrap(), Some(place("main.rs", 8, 30)));
        assert_eq!(store.forward().unwrap(), Some(place("main.rs", 8, 50)));
    }

    #[test]
    fn traversal_does_not_rewrite_persisted_history() {
        let file = std::env::temp_dir().join(format!(
            "remacs-jump-traversal-{}-{}.json",
            std::process::id(),
            TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = JumpManager::from_path(file.clone());
        for row in [1, 12, 22] {
            store.remember(place("main.rs", row, 0)).unwrap();
            store.record_change(place("main.rs", row, 0)).unwrap();
        }
        // Removing the file also catches rewrites whose bytes would be
        // unchanged: navigating must neither recreate it nor touch disk.
        fs::remove_file(&file).unwrap();
        assert!(store.back(None).unwrap().is_some());
        assert!(store.forward().unwrap().is_some());
        assert!(store.at(0).unwrap().is_some());
        assert!(store.older_change().unwrap().is_some());
        assert!(store.older_change().unwrap().is_some());
        assert!(store.newer_change().unwrap().is_some());
        assert!(!file.exists());
        // Branching must persist even when the origin exactly matches
        // the historical entry and truncation is the only mutation.
        store.remember(place("main.rs", 1, 0)).unwrap();
        let restored = JumpManager::from_path(file.clone());
        assert_eq!(restored.entries(), &[place("main.rs", 1, 0)]);
        fs::remove_file(file).unwrap();
    }

    #[test]
    fn loading_oversized_history_keeps_the_newest_locations() {
        let file = std::env::temp_dir().join(format!(
            "remacs-jump-bounds-{}-{}.json",
            std::process::id(),
            TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let saved = Saved {
            history: (0..350).map(|row| place("main.rs", row, 0)).collect(),
            ..Saved::default()
        };
        fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();
        let restored = JumpManager::from_path(file.clone());
        assert_eq!(restored.entries().len(), MAX_JUMPS);
        assert_eq!(restored.entries().first().unwrap().row, 150);
        assert_eq!(restored.entries().last().unwrap().row, 349);
        fs::remove_file(file).unwrap();
    }

    #[test]
    fn consecutive_identical_jump_origins_are_deduplicated() {
        let mut store = JumpManager::memory_only();
        store.remember(place("lib.rs", 8, 1)).unwrap();
        store.remember(place("lib.rs", 8, 1)).unwrap();
        store.remember(place("lib.rs", 8, 8)).unwrap();
        store.remember(place("lib.rs", 8, 8)).unwrap();
        assert_eq!(
            store.entries(),
            &[place("lib.rs", 8, 1), place("lib.rs", 8, 8)]
        );
    }

    #[test]
    fn local_and_global_named_marks_have_vim_scope() {
        let mut store = JumpManager::memory_only();
        let source = place("one.rs", 10, 2);
        store.set_mark('a', source.clone()).unwrap();
        store.set_mark('A', source.clone()).unwrap();
        assert_eq!(
            store.mark('a', &place("one.rs", 1, 0).path),
            Some(source.clone())
        );
        assert_eq!(store.mark('a', &place("two.rs", 1, 0).path), None);
        assert_eq!(store.mark('A', &place("two.rs", 1, 0).path), Some(source));
    }

    #[cfg(unix)]
    #[test]
    fn state_save_never_chmods_existing_parent_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "remacs-state-perms-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("jumps.json");
        let mut store = JumpManager::from_path(path.clone());
        store.remember(place("one.rs", 1, 0)).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "existing directory permissions must be preserved"
        );
        let file_mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "state file should be private");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disk_state_restores_closed_file_locations_and_marks() {
        let file = std::env::temp_dir().join(format!(
            "remacs-jumps-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut store = JumpManager::from_path(file.clone());
            store.remember(place("main.rs", 22, 5)).unwrap();
            store.remember(place("document.rs", 3, 4)).unwrap();
            store.set_mark('A', place("main.rs", 22, 5)).unwrap();
        }
        let mut restored = JumpManager::from_path(file.clone());
        assert_eq!(restored.entries().len(), 2);
        assert_eq!(restored.back(None).unwrap(), Some(place("main.rs", 22, 5)));
        assert_eq!(
            restored.mark('A', &place("document.rs", 0, 0).path),
            Some(place("main.rs", 22, 5))
        );
        let _ = fs::remove_file(file);
    }

    #[test]
    fn missing_or_corrupt_state_does_not_block_startup() {
        let file = std::env::temp_dir().join(format!("remacs-corrupt-{}.json", std::process::id()));
        fs::write(&file, "{ not JSON").unwrap();
        assert!(JumpManager::from_path(file.clone()).entries().is_empty());
        let _ = fs::remove_file(file);
    }

    #[test]
    fn long_jumplist_is_bounded() {
        let mut store = JumpManager::memory_only();
        for row in 0..350 {
            store.remember(place("main.rs", row, 0)).unwrap();
        }
        assert_eq!(store.entries().len(), MAX_JUMPS);
        assert_eq!(store.entries().first().unwrap().row, 150);
    }
}
