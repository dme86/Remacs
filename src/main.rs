//! Remacs: a minimal terminal editor with Vim-style modes and Doom-inspired leader keys.
mod clipboard;
mod completion;
mod completion_ui;
mod document;
mod editor_lua;
mod finder_ui;
mod function_select;
mod jumps;
mod lsp;
mod lsp_plugin;
mod lua_runtime;
mod mason;
mod mason_plugin;
mod plugins;
mod popup_terminal;
mod syntax;
mod toggleterm_plugin;

use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clipboard::Clipboard;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use document::Document;
use finder_ui::{
    rect_for as finder_rect_for, search_cursor as finder_cursor, Cache as FinderCache, PickerKind,
};
use function_select::{around_function, Position, TextRange};
use jumps::{JumpManager, Location};
use lsp::{Event as LspEvent, Manager as LspManager, RequestKind};
use lsp_plugin::LspPlugin;
use lua_runtime::LuaRuntime;
use mason::{Event as MasonEvent, Manager as MasonManager};
use mason_plugin::MasonPlugin;
use plugins::{FinderView, TreeAction, TreePlugin};
use popup_terminal::{
    popup_rect as terminal_rect, PopupCache as TerminalPopupCache, Session as TerminalSession,
};
use syntax::{Span, SyntaxEngine};
use toggleterm_plugin::{Action as TerminalAction, ToggleTermPlugin};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TAB_WIDTH: usize = 4;
const MAX_UNDO: usize = 100;
const MAX_UNDO_BYTES: usize = 32 * 1024 * 1024; // per-buffer snapshot budget
const COMPLETION_DELAY: Duration = Duration::from_millis(150);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
// Subtle current-line tint. The normal terminal background is unchanged.
const CURRENT_LINE_BG: Color = Color::Rgb {
    r: 49,
    g: 50,
    b: 57,
};
const GUTTER_BG: Color = Color::Rgb {
    r: 35,
    g: 36,
    b: 42,
};
const GUTTER_FG: Color = Color::Rgb {
    r: 116,
    g: 119,
    b: 132,
};
const CURRENT_NUMBER_FG: Color = Color::Rgb {
    r: 223,
    g: 211,
    b: 255,
};
const VISUAL_SELECTION_BG: Color = Color::Rgb {
    r: 83,
    g: 69,
    b: 111,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Normal,
    Insert,
    Visual,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Sequence {
    #[default]
    None,
    MarkSet,
    MarkLine,
    MarkExact,
}

enum Prompt {
    SaveAs(String),
    Open(String),
    Command(String),
    Quit,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    bytes: usize,
}

impl Snapshot {
    fn capture(doc: &Document) -> Self {
        let lines = doc.lines.clone();
        let bytes = lines
            .iter()
            .map(|line| {
                std::mem::size_of::<Vec<char>>() + line.capacity() * std::mem::size_of::<char>()
            })
            .sum();
        Self {
            lines,
            row: doc.row,
            col: doc.col,
            bytes,
        }
    }
    fn approx_bytes(&self) -> usize {
        self.bytes
    }

    fn restore(self, doc: &mut Document) {
        doc.lines = self.lines;
        doc.row = self.row;
        doc.col = self.col;
        doc.dirty = true; // Never report an undo/redo buffer as safely saved.
        doc.invalidate_syntax_from(0);
    }
}

#[derive(Clone)]
enum Register {
    Text(String),
    Line(Vec<char>),
}

impl Register {
    fn clipboard_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Line(line) => {
                let mut text: String = line.iter().collect();
                text.push('\n');
                text
            }
        }
    }
}

#[derive(Default)]
struct ScreenCache {
    size: Option<(u16, u16)>,
    origin: Option<(usize, usize)>,
    syntax_enabled: Option<bool>,
    sidebar_width: usize,
    sidebar_lines: Vec<Option<SidebarPaint>>,
    finder: FinderCache,
    completion: completion_ui::Cache,
    terminal: TerminalPopupCache,
    lines: Vec<Option<PaintedLine>>,
    status: Option<String>,
    message: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct SidebarPaint {
    text: String,
    active: bool,
    focused: bool,
}

struct PaintedLine {
    index: usize,
    content: Option<Vec<char>>,
    gutter: String,
    active: bool,
    selected: Option<(usize, usize)>,
}

/// Inactive buffers retain their unsaved content and independent undo/redo,
/// cursor and scroll state. Switching moves ownership without cloning text.
struct StoredBuffer {
    doc: Document,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    scroll_row: usize,
    scroll_col: usize,
}

#[derive(Default)]
struct BufferPicker {
    query: String,
    selected: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct CompletionContext {
    origin: PathBuf,
    revision: u64,
    cursor: (usize, usize),
}

struct PendingCompletion {
    context: CompletionContext,
    since: Instant,
    manual: bool,
}

struct CompletionMenu {
    context: CompletionContext,
    encoding: lsp::Encoding,
    items: Vec<completion::Item>,
    labels: Vec<String>,
    selected: usize,
    incomplete: bool,
}

// Only path and filename are compared for fuzzy matching. No filesystem scan
// is needed: a buffer picker typically only contains a handful of entries.
fn matches_buffer(label: &str, query: &str) -> bool {
    let mut letters = label.to_lowercase().chars().collect::<Vec<_>>().into_iter();
    query
        .to_lowercase()
        .chars()
        .all(|wanted| letters.by_ref().any(|actual| actual == wanted))
}

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    // Normalise the parent even for a newly created, not-yet-existing file.
    absolute
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .and_then(|parent| absolute.file_name().map(|file| parent.join(file)))
        .unwrap_or(absolute)
}

struct Editor {
    doc: Document,
    syntax: SyntaxEngine,
    slots: Vec<Option<StoredBuffer>>,
    buffer_ids: Vec<u64>,
    next_buffer_id: u64,
    active_slot: usize,
    last_slot: Option<usize>,
    buffer_picker: Option<BufferPicker>,
    mason_picker: Option<BufferPicker>,
    hover_popup: Option<String>,
    completion_menu: Option<CompletionMenu>,
    completion_pending: Option<PendingCompletion>,
    completion_due: Option<Instant>,
    mason: MasonManager,
    lua: LuaRuntime,
    lua_generation: u64,
    lua_observed: HashMap<u64, editor_lua::BufferObservation>,
    lua_current: Option<u64>,
    lua_mode: Option<Mode>,
    lua_saved: Vec<u64>,
    lsp_plugin: LspPlugin,
    lsp: LspManager,
    lsp_document_versions: Option<Vec<(PathBuf, u64)>>,
    lsp_present: HashSet<PathBuf>,
    jumps: JumpManager,
    toggleterm: ToggleTermPlugin,
    terminals: HashMap<String, TerminalSession>,
    tree: Option<TreePlugin>,
    screen: ScreenCache,
    scroll_row: usize,
    scroll_col: usize,
    mode: Mode,
    selection: Option<TextRange>,
    visual_anchor: Option<Position>,
    sequence: Sequence,
    count: Option<usize>,
    prompt: Option<Prompt>,
    message: String,
    register: Option<Register>,
    clipboard: Clipboard,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    insert_changed: bool,
    running: bool,
}

impl Editor {
    fn new(doc: Document) -> Self {
        let syntax = SyntaxEngine::new(&doc);
        let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let lua = LuaRuntime::new(&root).expect("bundled Lua runtime");
        let mason_plugin = MasonPlugin::new(lua.clone()).expect("bundled plugins/mason.lua");
        let mason = MasonManager::new(&mason_plugin).expect("valid Mason Lua catalog");
        Self {
            doc,
            syntax,
            slots: vec![None],
            buffer_ids: vec![1],
            next_buffer_id: 2,
            active_slot: 0,
            last_slot: None,
            buffer_picker: None,
            mason_picker: None,
            hover_popup: None,
            completion_menu: None,
            completion_pending: None,
            completion_due: None,
            mason,
            lua_generation: lua.generation(),
            lua_observed: HashMap::new(),
            lua_current: None,
            lua_mode: None,
            lua_saved: Vec::new(),
            lsp_plugin: LspPlugin::new(lua.clone()).expect("bundled plugins/lsp.lua"),
            lsp: LspManager::default(),
            lsp_document_versions: None,
            lsp_present: HashSet::new(),
            // Unit tests must not write to the real user's persistent state
            // or race each other while modifying the global jump history.
            jumps: if cfg!(test) {
                JumpManager::memory_only()
            } else {
                JumpManager::load_default()
            },
            toggleterm: ToggleTermPlugin::new(lua.clone()).expect("bundled toggleterm.lua plugin"),
            lua,
            terminals: HashMap::new(),
            tree: None,
            screen: ScreenCache::default(),
            scroll_row: 0,
            scroll_col: 0,
            mode: Mode::Normal,
            selection: None,
            visual_anchor: None,
            sequence: Sequence::None,
            count: None,
            prompt: None,
            message: "NORMAL | i Insert | Space f f Open | Space f s Save | Space q q Quit".into(),
            register: None,
            clipboard: Clipboard::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            insert_changed: false,
            running: true,
        }
    }

    fn project_root(&self) -> PathBuf {
        let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        project_root_for(self.doc.path.as_deref(), &current)
    }

    fn toggle_terminal(&mut self, kind: &str) {
        let root = self.project_root();
        let action = match self.with_lua(10, |_| self.toggleterm.toggle(kind, &root)) {
            Ok(action) => action,
            Err(error) => {
                self.message = format!("toggleterm: {error}");
                return;
            }
        };
        match action {
            TerminalAction::Open {
                kind,
                command,
                cwd,
                title,
            } => {
                let (cols, rows) = terminal::size().unwrap_or((100, 35));
                let Some(rect) = terminal_rect(cols, rows.saturating_sub(2) as usize) else {
                    let _ = self.with_lua(10, |_| self.toggleterm.exited(&kind));
                    self.message = "Terminal is too small for the floating popup".into();
                    return;
                };
                match TerminalSession::spawn(&command, Path::new(&cwd), title, rect) {
                    Ok(session) => {
                        self.terminals.insert(kind, session);
                        self.buffer_picker = None;
                    }
                    Err(error) => {
                        let _ = self.with_lua(10, |_| self.toggleterm.exited(&kind));
                        self.message = format!(
                            "Cannot launch {command} in {}: {error}",
                            Path::new(&cwd).display()
                        );
                    }
                }
            }
            TerminalAction::Show { kind } => {
                if !self.terminals.contains_key(&kind) {
                    let _ = self.with_lua(10, |_| self.toggleterm.exited(&kind));
                    self.message = "Terminal session exited; reopen to start a new one".into();
                } else {
                    self.buffer_picker = None;
                }
            }
            TerminalAction::Hide => {}
            TerminalAction::Error(error) => self.message = error,
        }
    }

    fn poll_terminals(&mut self) {
        let mut ended = Vec::new();
        for (kind, session) in &mut self.terminals {
            match session.poll() {
                Ok(true) => ended.push(kind.clone()),
                Ok(false) => {}
                Err(error) => {
                    self.message = format!("Terminal process error: {error}");
                    ended.push(kind.clone());
                }
            }
        }
        for kind in ended {
            self.terminals.remove(&kind);
            let _ = self.with_lua(10, |_| self.toggleterm.exited(&kind));
            self.message = format!("{kind} terminal exited");
        }
    }

    fn lsp_action(&mut self, action: &str) {
        let Some(path) = self.doc.path.as_ref() else {
            self.message = "LSP requires a named file".into();
            return;
        };
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = match self.with_lua(10, |_| self.lsp_plugin.server_for(path, &cwd)) {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.message = "No language server configured for this file".into();
                return;
            }
            Err(error) => {
                self.message = format!("LSP plugin error: {error}");
                return;
            }
        };
        let kind = match action {
            "definition" => RequestKind::Definition,
            "hover" => RequestKind::Hover,
            _ => {
                self.message = format!("Unsupported LSP action: {action}");
                return;
            }
        };
        match self.lsp.request(
            &config,
            path,
            self.doc.revision,
            self.doc.row,
            self.doc.col,
            kind,
        ) {
            Ok(true) => self.message = format!("{}: {action} requested", config.language),
            Ok(false) => {
                self.message = format!("{} LSP unavailable; check rust-analyzer", config.language)
            }
            Err(error) => self.message = format!("Cannot start {} LSP: {error}", config.language),
        }
    }

    fn completion_eligible(&self) -> bool {
        self.mode == Mode::Insert
            && self.doc.path.is_some()
            && self.prompt.is_none()
            && self.buffer_picker.is_none()
            && self.mason_picker.is_none()
            && self.hover_popup.is_none()
            && !self.tree.as_ref().is_some_and(|tree| tree.focused)
            && self
                .with_lua(10, |_| self.toggleterm.active())
                .ok()
                .flatten()
                .is_none()
    }

    fn completion_prefix(&self) -> String {
        let line = &self.doc.line()[..self.doc.col];
        let start = line
            .iter()
            .rposition(|ch| !ch.is_alphanumeric() && *ch != '_')
            .map_or(0, |index| index + 1);
        line[start..].iter().collect()
    }

    fn completion_trigger(&self) -> bool {
        let line = &self.doc.line()[..self.doc.col];
        line.last() == Some(&'.')
            || line.ends_with(&[':', ':'])
            || self.completion_prefix().chars().count() >= 2
    }

    fn completion_context_matches(&self, context: &CompletionContext) -> bool {
        self.completion_eligible()
            && self.doc.revision == context.revision
            && (self.doc.row, self.doc.col) == context.cursor
            && self
                .doc
                .path
                .as_ref()
                .is_some_and(|path| lsp::absolute(path) == context.origin)
    }

    fn dismiss_completion(&mut self) {
        self.completion_menu = None;
        self.completion_pending = None;
        self.completion_due = None;
        self.lsp.cancel_completion();
    }

    fn request_completion(&mut self, manual: bool) {
        self.dismiss_completion();
        if !self.completion_eligible() {
            return;
        }
        let path = self.doc.path.as_ref().unwrap().clone();
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = match self.with_lua(10, |_| self.lsp_plugin.server_for(&path, &cwd)) {
            Ok(Some(config)) => config,
            Ok(None) => {
                if manual {
                    self.message = "No language server configured for this file".into();
                }
                return;
            }
            Err(error) => {
                if manual {
                    self.message = format!("Completion configuration: {error}");
                }
                return;
            }
        };
        // Synchronize first so the server computes edits against this revision.
        self.sync_lsp_documents();
        match self.lsp.request(
            &config,
            &path,
            self.doc.revision,
            self.doc.row,
            self.doc.col,
            RequestKind::Completion,
        ) {
            Ok(true) => {
                self.completion_pending = Some(PendingCompletion {
                    context: CompletionContext {
                        origin: lsp::absolute(&path),
                        revision: self.doc.revision,
                        cursor: (self.doc.row, self.doc.col),
                    },
                    since: Instant::now(),
                    manual,
                });
                self.lsp.flush(&self.lsp_present);
                if manual {
                    self.message = "Completion requested".into();
                }
            }
            Ok(false) => {
                if manual {
                    self.message = "No LSP completion provider available".into();
                }
            }
            Err(error) => {
                if manual {
                    self.message = format!("Completion: {error}");
                }
            }
        }
    }

    fn poll_completion(&mut self) {
        if self
            .completion_pending
            .as_ref()
            .is_some_and(|pending| pending.since.elapsed() >= COMPLETION_TIMEOUT)
        {
            let manual = self.completion_pending.as_ref().unwrap().manual;
            self.dismiss_completion();
            if manual {
                self.message = "Completion timed out; Ctrl+Space retries".into();
            }
        }
        if self.completion_due.is_some_and(|due| Instant::now() >= due) {
            self.request_completion(false);
        }
    }

    fn show_completions(
        &mut self,
        context: CompletionContext,
        encoding: lsp::Encoding,
        list: completion::List,
    ) {
        if self
            .completion_pending
            .as_ref()
            .map(|pending| &pending.context)
            != Some(&context)
            || !self.completion_context_matches(&context)
        {
            return;
        }
        let manual = self.completion_pending.take().unwrap().manual;
        let incomplete = list.incomplete;
        let items = completion::suggestions(list, &self.doc, encoding);
        if items.is_empty() {
            self.completion_menu = None;
            if manual {
                self.message = "No completions at cursor".into();
            }
            return;
        }
        let labels = items
            .iter()
            .map(|item| {
                let mut label: String = item.label.chars().take(160).collect();
                if let Some(detail) = &item.detail {
                    label.push_str("  ");
                    label.extend(detail.chars().take(120));
                }
                label
            })
            .collect();
        self.completion_menu = Some(CompletionMenu {
            context,
            encoding,
            items,
            labels,
            selected: 0,
            incomplete,
        });
    }

    fn accept_completion(&mut self) {
        let Some(menu) = self.completion_menu.take() else {
            return;
        };
        self.completion_pending = None;
        self.completion_due = None;
        self.lsp.cancel_completion();
        if !self.completion_context_matches(&menu.context) {
            return;
        }
        let Some(item) = menu.items.get(menu.selected) else {
            return;
        };
        let before = Snapshot::capture(&self.doc);
        let change = self.current_location();
        match completion::apply(&mut self.doc, item, menu.encoding) {
            Ok(()) => {
                self.remember_snapshot(before);
                if let Some(location) = change {
                    if let Err(error) = self.jumps.record_change(location) {
                        self.message = format!("Change history could not be saved: {error}");
                    }
                }
                self.insert_changed = false;
            }
            Err(error) => self.message = format!("Cannot apply completion: {error}"),
        }
    }

    fn sync_lsp_documents(&mut self) {
        // Cursor movement and idle ticks do not change document identity or
        // text. Keep polling requests, but avoid filesystem/Lua work entirely.
        let versions: Vec<_> = std::iter::once(&self.doc)
            .chain(self.slots.iter().flatten().map(|buffer| &buffer.doc))
            .filter_map(|doc| doc.path.as_ref().map(|path| (path.clone(), doc.revision)))
            .collect();
        if self.lsp_document_versions.as_ref() == Some(&versions)
            && !self.lsp.has_starting_servers()
        {
            self.lsp.flush(&self.lsp_present);
            return;
        }
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let docs = std::iter::once(&self.doc).chain(
            self.slots
                .iter()
                .filter_map(|entry| entry.as_ref().map(|b| &b.doc)),
        );
        let mut present = HashSet::new();
        let mut synced = true;
        for doc in docs {
            let Some(path) = doc.path.as_ref() else {
                continue;
            };
            let config = match self.with_lua(10, |_| self.lsp_plugin.server_for(path, &cwd)) {
                Ok(Some(config)) => config,
                Ok(None) => continue,
                Err(error) => {
                    self.message = format!("LSP configuration error: {error}");
                    synced = false;
                    continue;
                }
            };
            present.insert(lsp::absolute(path));
            if let Err(error) = self
                .lsp
                .sync(&config, path, doc.revision, || doc.lsp_text())
            {
                self.message = format!("{} LSP: {error}", config.language);
                synced = false;
            }
        }
        self.lsp.flush(&present);
        self.lsp_present = present;
        // A starting server has not received didOpen yet. Retry after its
        // initialize response even when no text has changed in the meantime.
        self.lsp_document_versions =
            (synced && !self.lsp.has_starting_servers()).then_some(versions);
    }

    fn apply_lsp_definition(&mut self, target: lsp::Target) {
        // LSP coordinates are UTF-16 code units (or negotiated UTF-8/32).
        // Remacs stores Unicode scalar indices in Vec<char>.
        let line_in_buffer = if self
            .doc
            .path
            .as_ref()
            .is_some_and(|p| lsp::absolute(p) == lsp::absolute(&target.path))
        {
            self.doc.lines.get(target.row)
        } else {
            self.slots
                .iter()
                .flatten()
                .find(|entry| {
                    entry
                        .doc
                        .path
                        .as_ref()
                        .is_some_and(|path| lsp::absolute(path) == lsp::absolute(&target.path))
                })
                .and_then(|entry| entry.doc.lines.get(target.row))
        };
        let col = if let Some(line) = line_in_buffer {
            lsp::char_index(line, target.character, target.encoding)
        } else {
            std::fs::read_to_string(&target.path)
                .ok()
                .and_then(|text| {
                    text.split('\n')
                        .nth(target.row)
                        .map(|line| line.trim_end_matches('\r').chars().collect::<Vec<_>>())
                })
                .map(|line| lsp::char_index(&line, target.character, target.encoding))
                .unwrap_or(target.character)
        };
        self.record_current_jump();
        self.visit_location(
            Location {
                path: target.path,
                row: target.row,
                col,
            },
            false,
        );
    }

    fn poll_lsp(&mut self) {
        for event in self.lsp.poll() {
            match event {
                LspEvent::Status(text) => self.message = text,
                LspEvent::Definition {
                    origin,
                    revision,
                    cursor,
                    target,
                } => {
                    let still_current = self
                        .doc
                        .path
                        .as_ref()
                        .is_some_and(|path| lsp::absolute(path) == origin)
                        && self.doc.revision == revision
                        && (self.doc.row, self.doc.col) == cursor;
                    if !still_current {
                        continue;
                    }
                    if let Some(target) = target {
                        self.apply_lsp_definition(target);
                    } else {
                        self.message = "No definition found".into();
                    }
                }
                LspEvent::Hover {
                    origin,
                    revision,
                    cursor,
                    text,
                } => {
                    let still_current = self
                        .doc
                        .path
                        .as_ref()
                        .is_some_and(|path| lsp::absolute(path) == origin)
                        && self.doc.revision == revision
                        && (self.doc.row, self.doc.col) == cursor;
                    if !still_current {
                        continue;
                    }
                    if let Some(text) = text {
                        self.hover_popup = Some(text);
                    } else {
                        self.message = "No hover information at cursor".into();
                    }
                }
                LspEvent::Completion {
                    origin,
                    revision,
                    cursor,
                    encoding,
                    list,
                } => {
                    self.show_completions(
                        CompletionContext {
                            origin,
                            revision,
                            cursor,
                        },
                        encoding,
                        list,
                    );
                }
            }
        }
        self.sync_lsp_documents();
        self.poll_completion();
    }

    fn hover_view(&self) -> Option<FinderView> {
        let content = self.hover_popup.as_ref()?;
        let matches = content
            .lines()
            .take(150)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Some(FinderView {
            query: String::new(),
            selected: 0,
            total_matches: matches.len(),
            scanned: matches.len(),
            matches,
        })
    }

    fn open_mason_picker(&mut self) {
        self.mason_picker = Some(BufferPicker::default());
        self.buffer_picker = None;
        self.hover_popup = None;
        self.sequence = Sequence::None;
        self.message =
            "Mason: Enter install · Ctrl+U update · Ctrl+S sync · Ctrl+R update all".into();
    }

    fn mason_view(&self) -> Option<FinderView> {
        let picker = self.mason_picker.as_ref()?;
        let entries = self.mason.entries(&picker.query);
        let total = entries.len();
        Some(FinderView {
            query: picker.query.clone(),
            matches: entries.into_iter().map(|(_, display)| display).collect(),
            selected: if total == 0 {
                0
            } else {
                picker.selected.min(total.saturating_sub(1)) + 1
            },
            total_matches: total,
            scanned: self.mason.tools().len(),
        })
    }

    fn mason_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.mason_picker.as_ref() else {
            return;
        };
        let entries = self.mason.entries(&picker.query);
        let selected = entries.get(picker.selected).map(|(id, _)| id.clone());
        let count = entries.len();
        match key.code {
            KeyCode::Esc => self.mason_picker = None,
            KeyCode::Enter => {
                if let Some(id) = selected {
                    self.message = self.mason.queue(&id, false).unwrap_or_else(|error| error);
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(id) = selected {
                    self.message = self.mason.queue(&id, true).unwrap_or_else(|error| error);
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let n = self.mason.queue_all(false);
                self.message = format!("Mason queued {n} missing servers");
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let n = self.mason.queue_all(true);
                self.message = format!("Mason queued {n} server updates");
            }
            KeyCode::Down | KeyCode::Tab => {
                if let Some(picker) = &mut self.mason_picker {
                    picker.selected = (picker.selected + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if let Some(picker) = &mut self.mason_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.mason_picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = &mut self.mason_picker {
                    picker.query.clear();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = &mut self.mason_picker {
                    if picker.query.len() < 128 {
                        picker.query.push(c);
                        picker.selected = 0;
                    }
                }
            }
            _ => {}
        }
    }

    fn poll_mason(&mut self) {
        for event in self.mason.poll() {
            match event {
                MasonEvent::Done {
                    id,
                    language,
                    updated,
                } => {
                    self.lsp_plugin.clear_cache();
                    self.lsp_document_versions = None;
                    self.lsp.reset_language(&language);
                    self.message = format!(
                        "Mason {} {id}; LSP ready to restart",
                        if updated { "updated" } else { "installed" }
                    );
                }
                MasonEvent::Message(message) => self.message = message,
            }
        }
    }

    fn active_name(&self) -> String {
        self.doc
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[No Name]".into())
    }

    fn buffer_name(doc: &Document) -> String {
        doc.path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[No Name]".into())
    }

    fn current_location(&self) -> Option<Location> {
        self.doc.path.as_ref().map(|path| Location {
            path: path_identity(path),
            row: self.doc.row,
            col: self.doc.col,
        })
    }

    fn record_current_jump(&mut self) {
        if let Some(location) = self.current_location() {
            if let Err(error) = self.jumps.remember(location) {
                self.message = format!("Jump history could not be saved: {error}");
            }
        }
    }

    fn record_current_change(&mut self) {
        if let Some(location) = self.current_location() {
            if let Err(error) = self.jumps.record_change(location) {
                self.message = format!("Change history could not be saved: {error}");
            }
        }
    }

    fn visit_location(&mut self, target: Location, linewise: bool) -> bool {
        let slot = self.named_slot(&target.path);
        // In-memory buffers take precedence: a file may have been deleted
        // on disk while its modified contents remain open in Remacs.
        if slot.is_none() && !target.path.is_file() {
            self.message = format!("Jump target missing: {}", target.path.display());
            return false;
        }
        let Some(slot) = slot else {
            // Open through the standard buffer API but without adding
            // the origin as another jump to this already-traversed history.
            let next = match Document::open(target.path.clone()) {
                Ok(doc) => doc,
                Err(error) => {
                    self.message = format!("Jump target unreadable: {error}");
                    return false;
                }
            };
            let previous = self.active_slot;
            let current = self.take_current();
            self.slots[previous] = Some(current);
            let index = self.slots.len();
            self.slots.push(None);
            self.buffer_ids.push(self.next_buffer_id);
            self.next_buffer_id += 1;
            self.last_slot = Some(previous);
            self.activate(
                StoredBuffer {
                    doc: next,
                    undo: Vec::new(),
                    redo: Vec::new(),
                    scroll_row: 0,
                    scroll_col: 0,
                },
                index,
            );
            self.move_to_location(target, linewise);
            return true;
        };
        if slot != self.active_slot {
            self.switch_buffer_internal(slot, false);
        }
        self.move_to_location(target, linewise);
        true
    }

    fn move_to_location(&mut self, target: Location, linewise: bool) {
        self.doc.row = target.row.min(self.doc.lines.len().saturating_sub(1));
        let line = &self.doc.lines[self.doc.row];
        let col = if linewise {
            line.iter().position(|c| !c.is_whitespace()).unwrap_or(0)
        } else {
            target.col
        };
        self.doc.col = col.min(line.len());
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
        self.return_to_editor_after_switch();
        self.message = format!(
            "Jump: {}:{}:{}",
            self.doc
                .path
                .as_ref()
                .map_or(String::new(), |p| p.display().to_string()),
            self.doc.row + 1,
            self.doc.col + 1
        );
    }

    fn jump_back(&mut self) {
        let current = self.current_location();
        let mut result = self.jumps.back(current);
        let mut skipped = false;
        loop {
            match result {
                Ok(Some(place)) => {
                    if self.visit_location(place, false) {
                        return;
                    }
                    skipped = true;
                    result = self.jumps.back(None);
                }
                Ok(None) => {
                    if !skipped {
                        self.message = "At oldest jump".into();
                    }
                    return;
                }
                Err(error) => {
                    self.message = format!("Jump history error: {error}");
                    return;
                }
            }
        }
    }

    fn jump_forward(&mut self) {
        let mut skipped = false;
        loop {
            match self.jumps.forward() {
                Ok(Some(place)) => {
                    if self.visit_location(place, false) {
                        return;
                    }
                    skipped = true;
                }
                Ok(None) => {
                    if !skipped {
                        self.message = "At newest jump".into();
                    }
                    return;
                }
                Err(error) => {
                    self.message = format!("Jump history error: {error}");
                    return;
                }
            }
        }
    }

    fn jump_to_change(&mut self, older: bool) {
        let result = if older {
            self.jumps.older_change()
        } else {
            self.jumps.newer_change()
        };
        match result {
            Ok(Some(place)) => {
                self.record_current_jump();
                self.visit_location(place, false);
            }
            Ok(None) => {
                self.message = if older {
                    "No older edit location".into()
                } else {
                    "No newer edit location".into()
                }
            }
            Err(error) => self.message = format!("Change history error: {error}"),
        }
    }

    fn set_mark(&mut self, name: char) {
        let Some(location) = self.current_location() else {
            self.message = "Name this file before setting a persistent mark".into();
            return;
        };
        match self.jumps.set_mark(name, location) {
            Ok(true) => self.message = format!("Mark {name} saved"),
            Ok(false) => self.message = "Marks must be letters a-z or A-Z".into(),
            Err(error) => self.message = format!("Could not save mark: {error}"),
        }
    }

    fn jump_to_mark(&mut self, name: char, linewise: bool) {
        let Some(current) = self.current_location() else {
            self.message = "Name this file before jumping to a mark".into();
            return;
        };
        if let Some(place) = self.jumps.mark(name, &current.path) {
            self.record_current_jump();
            self.visit_location(place, linewise);
        } else {
            self.message = format!("Mark {name} not set");
        }
    }

    fn dirty_buffers(&self) -> usize {
        usize::from(self.doc.dirty)
            + self
                .slots
                .iter()
                .flatten()
                .filter(|buffer| buffer.doc.dirty)
                .count()
    }

    fn named_slot(&self, path: &Path) -> Option<usize> {
        let target = path_identity(path);
        if self
            .doc
            .path
            .as_ref()
            .is_some_and(|p| path_identity(p) == target)
        {
            return Some(self.active_slot);
        }
        self.slots.iter().position(|entry| {
            entry.as_ref().is_some_and(|buffer| {
                buffer
                    .doc
                    .path
                    .as_ref()
                    .is_some_and(|path| path_identity(path) == target)
            })
        })
    }

    fn set_path_and_save(&mut self, path: PathBuf) {
        if self
            .named_slot(&path)
            .is_some_and(|index| index != self.active_slot)
        {
            self.message = "That file is already open in another buffer".into();
            return;
        }
        let before = self.doc.path.replace(path);
        match self.doc.save() {
            Ok(()) => {
                self.lua_saved.push(self.buffer_ids[self.active_slot]);
                self.message = "File saved".into();
            }
            Err(error) => {
                // A failed write must not silently rename an unnamed buffer.
                self.doc.path = before;
                self.message = format!("Save error: {error}");
            }
        }
    }

    fn take_current(&mut self) -> StoredBuffer {
        StoredBuffer {
            // An empty placeholder is replaced, never cloned or displayed.
            doc: std::mem::replace(&mut self.doc, Document::empty()),
            undo: std::mem::take(&mut self.undo),
            redo: std::mem::take(&mut self.redo),
            scroll_row: self.scroll_row,
            scroll_col: self.scroll_col,
        }
    }

    fn activate(&mut self, next: StoredBuffer, index: usize) {
        self.dismiss_completion();
        self.doc = next.doc;
        self.undo = next.undo;
        self.redo = next.redo;
        self.scroll_row = next.scroll_row;
        self.scroll_col = next.scroll_col;
        self.active_slot = index;
        let _ = self.lua.cancel_keys();
        let enabled = self.syntax.enabled();
        self.syntax = SyntaxEngine::new(&self.doc);
        self.syntax.set_enabled(enabled);
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
        self.count = None;
        self.insert_changed = false;
        // Only visible text rows are invalidated; tree and terminal geometry
        // are unchanged, so there is no destructive whole-screen clear.
        for line in &mut self.screen.lines {
            *line = None;
        }
        self.screen.status = None;
        self.screen.origin = None;
        self.message = format!(
            "Buffer {}/{}: {}",
            index + 1,
            self.slots.len(),
            self.active_name()
        );
    }

    fn switch_buffer(&mut self, index: usize) {
        self.switch_buffer_internal(index, true);
    }

    fn switch_buffer_internal(&mut self, index: usize, record_jump: bool) {
        if index == self.active_slot || index >= self.slots.len() {
            return;
        }
        if self.slots[index].is_none() {
            return;
        }
        if record_jump {
            self.record_current_jump();
        }
        let Some(next) = self.slots[index].take() else {
            return;
        };
        let previous = self.active_slot;
        let current = self.take_current();
        self.slots[previous] = Some(current);
        self.last_slot = Some(previous);
        self.activate(next, index);
    }

    fn return_to_editor_after_switch(&mut self) {
        if let Some(mut tree) = self.tree.take() {
            if tree.focused {
                // A direct buffer switch should land in the editor, not leave
                // keyboard focus (or a fuzzy-file popup) in the old tree.
                let _ = self.with_lua(10, |_| tree.on_key("esc"));
                tree.leave();
            }
            self.tree = Some(tree);
        }
    }

    fn cycle_buffer(&mut self, forward: bool) {
        let count = self.slots.len();
        if count < 2 {
            self.message = "Only one buffer is open".into();
            return;
        }
        let index = if forward {
            (self.active_slot + 1) % count
        } else {
            (self.active_slot + count - 1) % count
        };
        self.switch_buffer(index);
        self.return_to_editor_after_switch();
    }

    fn last_buffer(&mut self) {
        if let Some(index) = self.last_slot {
            self.switch_buffer(index);
        } else {
            self.message = "No previous buffer".into();
        }
    }

    fn open_buffer_picker(&mut self) {
        // Make the most recently visited buffer the initial selection.
        // Ctrl+P then Enter quickly jumps back without typing a filename.
        let selected = self
            .last_slot
            .unwrap_or(self.active_slot)
            .min(self.slots.len().saturating_sub(1));
        self.buffer_picker = Some(BufferPicker {
            query: String::new(),
            selected,
        });
        self.sequence = Sequence::None;
        self.message = "Buffers: type to filter, ↑↓ select, Enter switch, Esc cancel".into();
    }

    fn buffer_candidates(&self, query: &str) -> Vec<(usize, String)> {
        let mut entries: Vec<(usize, String, bool)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let (doc, active) = if index == self.active_slot {
                    (&self.doc, true)
                } else {
                    (&slot.as_ref()?.doc, false)
                };
                let name = Self::buffer_name(doc);
                // Filename comes first so long absolute paths never hide
                // the useful part of an entry in a narrower terminal.
                let basename = doc
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "[No Name]".into());
                let file_match = matches_buffer(&basename, query);
                if !file_match && !matches_buffer(&name, query) {
                    return None;
                }
                let parent = doc
                    .path
                    .as_ref()
                    .and_then(|path| path.parent())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some((
                    index,
                    format!(
                        "{} {}{}  {}  {}",
                        if active { "▶" } else { " " },
                        if doc.dirty { "● " } else { "  " },
                        index + 1,
                        basename,
                        parent
                    ),
                    file_match,
                ))
            })
            .collect();
        // Filename hits take precedence over unrelated matches created by
        // combining characters across the absolute parent directory.
        if !query.is_empty() && entries.iter().any(|(_, _, file_match)| *file_match) {
            entries.retain(|(_, _, file_match)| *file_match);
        }
        entries
            .into_iter()
            .map(|(index, label, _)| (index, label))
            .collect()
    }

    fn buffer_picker_view(&self) -> Option<FinderView> {
        let picker = self.buffer_picker.as_ref()?;
        let entries = self.buffer_candidates(&picker.query);
        let selected = picker.selected.min(entries.len().saturating_sub(1)) + 1;
        Some(FinderView {
            query: picker.query.clone(),
            matches: entries.into_iter().map(|(_, label)| label).collect(),
            selected,
            total_matches: self.buffer_candidates(&picker.query).len(),
            scanned: self.slots.len(),
        })
    }

    fn buffer_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.buffer_picker.as_ref() else {
            return;
        };
        let count = self.buffer_candidates(&picker.query).len();
        match key.code {
            KeyCode::Esc => {
                self.buffer_picker = None;
            }
            KeyCode::Enter => {
                let results = self.buffer_candidates(&picker.query);
                let index = results.get(picker.selected).map(|(index, _)| *index);
                self.buffer_picker = None;
                if let Some(index) = index {
                    self.switch_buffer(index);
                    self.return_to_editor_after_switch();
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if let Some(picker) = &mut self.buffer_picker {
                    picker.selected = (picker.selected + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if let Some(picker) = &mut self.buffer_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.buffer_picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = &mut self.buffer_picker {
                    picker.query.clear();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = &mut self.buffer_picker {
                    if picker.query.len() < 128 {
                        picker.query.push(c);
                        picker.selected = 0;
                    }
                }
            }
            _ => {}
        }
    }

    fn close_buffer(&mut self, discard: bool) {
        if self.doc.dirty && !discard {
            self.message = "Buffer modified; save it or use :bd! to discard".into();
            return;
        }
        // Closing a buffer is a jump-worthy event. Its last position remains
        // reachable even when the document is no longer held in memory.
        self.record_current_jump();
        if self.slots.len() == 1 {
            let enabled = self.syntax.enabled();
            self.doc = Document::empty();
            self.buffer_ids[0] = self.next_buffer_id;
            self.next_buffer_id += 1;
            self.syntax = SyntaxEngine::new(&self.doc);
            self.syntax.set_enabled(enabled);
            self.undo.clear();
            self.redo.clear();
            self.scroll_row = 0;
            self.scroll_col = 0;
            self.insert_changed = false;
            self.mode = Mode::Normal;
            self.selection = None;
            self.visual_anchor = None;
            for line in &mut self.screen.lines {
                *line = None;
            }
            self.screen.status = None;
            self.message = "Empty buffer".into();
            return;
        }
        let closed = self.active_slot;
        let next = (closed + 1) % self.slots.len();
        let replacement = self.slots[next].take().expect("inactive buffer in slot");
        self.slots.remove(closed);
        self.buffer_ids.remove(closed);
        // Adjust target index because removing an earlier slot shifts it.
        let adjusted_next = if next > closed { next - 1 } else { next };
        self.last_slot = None;
        self.activate(replacement, adjusted_next);
        self.message = "Buffer closed".into();
    }

    fn remember(&mut self) {
        self.record_current_change();
        let snapshot = Snapshot::capture(&self.doc);
        self.remember_snapshot(snapshot);
    }

    fn remember_snapshot(&mut self, snapshot: Snapshot) {
        let next_bytes = snapshot.approx_bytes();
        let mut used = self.undo.iter().map(Snapshot::approx_bytes).sum::<usize>();
        while !self.undo.is_empty()
            && (self.undo.len() >= MAX_UNDO || used.saturating_add(next_bytes) > MAX_UNDO_BYTES)
        {
            used = used.saturating_sub(self.undo.remove(0).approx_bytes());
        }
        // Keep at least the most recent snapshot even if the document
        // exceeds the budget, so undo remains possible for large files.
        self.undo.push(snapshot);
        self.redo.clear();
    }

    fn insert_change(&mut self) {
        if !self.insert_changed {
            self.remember();
            self.insert_changed = true;
        }
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(Snapshot::capture(&self.doc));
            previous.restore(&mut self.doc);
            self.message = "Undo".into();
        } else {
            self.message = "Nothing to undo".into();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(Snapshot::capture(&self.doc));
            next.restore(&mut self.doc);
            self.message = "Redo".into();
        } else {
            self.message = "Nothing to redo".into();
        }
    }

    fn begin_insert(&mut self) {
        self.mode = Mode::Insert;
        self.insert_changed = false;
    }

    fn leave_insert(&mut self) {
        self.mode = Mode::Normal;
        self.doc.col = self.doc.col.saturating_sub(1);
        self.sequence = Sequence::None;
    }

    fn toggle_tree(&mut self) {
        if self.tree.is_none() {
            let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match self.with_lua(10, |_| TreePlugin::new(self.lua.clone(), &root)) {
                Ok(tree) => self.tree = Some(tree),
                Err(error) => {
                    self.message = format!("Cannot load Lua tree plugin: {error}");
                    return;
                }
            }
        }
        if let Some(mut tree) = self.tree.take() {
            if let Err(error) = self.with_lua(10, |_| tree.toggle()) {
                self.message = format!("Tree plugin error: {error}");
            } else if tree.visible {
                self.message =
                    "Tree: / or f fuzzy find  j/k move  Enter open  Tab/Esc editor".into();
            }
            self.tree = Some(tree);
        }
        self.sequence = Sequence::None;
    }

    fn tree_key(&mut self, key: KeyEvent) {
        let name = match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                c.to_string()
            }
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "backtab".into(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => "ctrl-u".into(),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => "ctrl-p".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            _ => return,
        };
        let action = match self
            .tree
            .as_ref()
            .map(|tree| self.with_lua(10, |_| tree.on_key(&name)))
        {
            Some(Ok(action)) => action,
            Some(Err(error)) => {
                self.message = format!("Lua file tree error: {error}");
                return;
            }
            None => return,
        };
        match action {
            TreeAction::None => {}
            TreeAction::Editor => {
                if let Some(tree) = &mut self.tree {
                    tree.leave();
                }
            }
            TreeAction::Close => {
                if let Some(tree) = &mut self.tree {
                    tree.close();
                }
            }
            TreeAction::Open(path) => {
                self.open(path.to_string_lossy().into_owned());
                if self
                    .doc
                    .path
                    .as_ref()
                    .is_some_and(|current| path_identity(current) == path_identity(&path))
                {
                    if let Some(tree) = &mut self.tree {
                        tree.leave();
                    }
                }
            }
        }
    }

    /// Keep Vim's unnamed register and the desktop clipboard in sync.
    /// Clipboard errors never prevent editing or internal yanks.
    fn store_register(&mut self, value: Register, action: &str) {
        let text = value.clipboard_text();
        self.register = Some(value);
        self.message = match self.clipboard.write(&text) {
            Ok(()) => format!("{action} (system clipboard)"),
            Err(error) => format!("{action} (internal only; clipboard: {error})"),
        };
    }

    fn paste_value(&mut self, value: Register) {
        if matches!(&value, Register::Text(text) if text.is_empty()) {
            return;
        }
        self.remember();
        match value {
            Register::Line(line) => self.doc.insert_line_below(line),
            Register::Text(text) => {
                if self.doc.col < self.doc.line().len() {
                    self.doc.col += 1;
                }
                self.doc.insert_text(&normalize_pasted_text(&text));
                self.doc.col = self.doc.col.saturating_sub(1);
            }
        }
    }

    /// Prefer fresh text copied in another application, but keep the linewise
    /// semantics of an unchanged 'yy' register. If the OS clipboard is not
    /// available (e.g. a remote/headless session), use Remacs' own register.
    fn paste_clipboard(&mut self) {
        let value = match self.clipboard.read() {
            Ok(text) => {
                if text.is_empty() {
                    return;
                }
                match self.register.as_ref() {
                    Some(register) if register.clipboard_text() == text => register.clone(),
                    _ => Register::Text(text),
                }
            }
            Err(_) => match self.register.clone() {
                Some(value) => value,
                None => return,
            },
        };
        self.paste_value(value);
    }

    /// Bracketed paste arrives as one event instead of a stream of hotkeys.
    /// In Insert mode this is one undoable edit, even with multiline Unicode.
    fn pasted(&mut self, text: &str) {
        let _ = self.lua.cancel_keys();
        self.pasted_inner(text);
        self.sync_lua_events();
    }

    fn pasted_inner(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.dismiss_completion();
        if let Ok(Some(active)) = self.with_lua(10, |_| self.toggleterm.active()) {
            if let Some(session) = self.terminals.get_mut(&active) {
                if let Err(error) = session.paste(text) {
                    self.message = format!("PTY paste: {error}");
                }
            }
            return;
        }
        // Paste into input fields literally. Newlines never execute commands
        // or confirm a dialog; the user still presses Enter explicitly.
        if let Some(prompt) = &mut self.prompt {
            match prompt {
                Prompt::Open(value) | Prompt::SaveAs(value) | Prompt::Command(value) => {
                    value.extend(
                        text.chars()
                            .take_while(|ch| *ch != '\r' && *ch != '\n')
                            .filter(|ch| !ch.is_control()),
                    );
                }
                Prompt::Quit => {}
            }
            return;
        }
        if let Some(picker) = self.mason_picker.as_mut().or(self.buffer_picker.as_mut()) {
            let remaining = 128usize.saturating_sub(picker.query.len());
            let mut used = 0;
            picker.query.extend(
                text.chars()
                    .take_while(|ch| *ch != '\r' && *ch != '\n')
                    .filter(|ch| !ch.is_control())
                    .take_while(|ch| {
                        used += ch.len_utf8();
                        used <= remaining
                    }),
            );
            picker.selected = 0;
            return;
        }
        if let Some(tree) = self.tree.as_ref().filter(|tree| tree.focused) {
            let query: String = text
                .chars()
                .take_while(|ch| *ch != '\r' && *ch != '\n')
                .filter(|ch| !ch.is_control())
                .collect();
            if let Err(error) = self.with_lua(10, |_| tree.paste_query(&query)) {
                self.message = format!("Tree paste: {error}");
            }
            return;
        }
        self.hover_popup = None;
        self.sequence = Sequence::None;
        self.count = None;
        let text = normalize_pasted_text(text);
        match self.mode {
            Mode::Insert => {
                self.remember();
                self.doc.insert_text(&text);
                // Both the paste and subsequent typing get their own undo
                // group, even when the user was already inserting text.
                self.insert_changed = false;
            }
            Mode::Visual => {
                if let Some(range) = self.selection {
                    self.remember();
                    self.doc.delete_text_range(range);
                    self.doc.insert_text(&text);
                    self.leave_visual();
                }
            }
            Mode::Normal => self.paste_value(Register::Text(text)),
        }
    }

    fn leave_visual(&mut self) {
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
    }

    fn enter_visual(&mut self) {
        self.mode = Mode::Visual;
        self.visual_anchor = Some(Position {
            row: self.doc.row,
            col: self.doc.col,
        });
        self.update_visual_selection();
        self.message = "VISUAL: a f Function | y Yank | d Delete | Esc Cancel".into();
    }

    fn update_visual_selection(&mut self) {
        let Some(anchor) = self.visual_anchor else {
            return;
        };
        let here = Position {
            row: self.doc.row,
            col: self.doc.col,
        };
        let inclusive_end = |position: Position, doc: &Document| Position {
            row: position.row,
            col: (position.col + 1).min(doc.lines[position.row].len()),
        };
        self.selection = Some(if anchor <= here {
            TextRange {
                start: anchor,
                end: inclusive_end(here, &self.doc),
            }
        } else {
            TextRange {
                start: here,
                end: inclusive_end(anchor, &self.doc),
            }
        });
    }

    fn select_function(&mut self) {
        self.sequence = Sequence::None;
        match around_function(&self.doc) {
            Ok(Some(range)) => {
                self.record_current_jump();
                self.mode = Mode::Visual;
                self.visual_anchor = Some(range.start);
                self.selection = Some(range);
                self.doc.row = range.end.row;
                self.doc.col = range.end.col.saturating_sub(1);
                self.message = "VISUAL FUNCTION: y Yank | d Delete | Esc Cancel".into();
            }
            Ok(None) => self.message = "No surrounding function at the cursor".into(),
            Err(error) => self.message = error,
        }
    }

    fn save(&mut self) {
        if self.doc.path.is_none() {
            self.prompt = Some(Prompt::SaveAs(String::new()));
            return;
        }
        match self.doc.save() {
            Ok(()) => {
                self.lua_saved.push(self.buffer_ids[self.active_slot]);
                self.message = "File saved".into();
            }
            Err(error) => self.message = format!("Save error: {error}"),
        }
    }

    /// Save every modified, named document without changing the active buffer,
    /// cursor, viewport or undo history. An unnamed document needs an explicit
    /// filename; failures leave those buffers dirty and don't stop later saves.
    fn save_all(&mut self) {
        let mut saved = 0usize;
        let mut unnamed = 0usize;
        let mut failed = 0usize;
        let mut first_error: Option<String> = None;

        let mut saved_ids = Vec::new();
        let mut save_if_dirty = |doc: &mut Document, id: u64| {
            if !doc.dirty {
                return;
            }
            let Some(path) = doc.path.as_ref() else {
                unnamed += 1;
                return;
            };
            let name = path.display().to_string();
            match doc.save() {
                Ok(()) => {
                    saved += 1;
                    saved_ids.push(id);
                }
                Err(error) => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("{name}: {error}"));
                    }
                }
            }
        };

        save_if_dirty(&mut self.doc, self.buffer_ids[self.active_slot]);
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(slot) = slot {
                save_if_dirty(&mut slot.doc, self.buffer_ids[index]);
            }
        }
        self.lua_saved.extend(saved_ids);

        self.message = if saved == 0 && unnamed == 0 && failed == 0 {
            "All buffers already saved".into()
        } else if unnamed == 0 && failed == 0 {
            format!("Saved {saved} buffer(s)")
        } else {
            let mut message = format!("Saved {saved}");
            if unnamed > 0 {
                message.push_str(&format!("; {unnamed} unnamed (use :w FILE)"));
            }
            if failed > 0 {
                message.push_str(&format!("; {failed} failed"));
                if let Some(error) = first_error {
                    message.push_str(&format!(": {error}"));
                }
            }
            message
        };
    }

    fn open(&mut self, name: String) {
        let path = PathBuf::from(&name);
        if let Some(index) = self.named_slot(&path) {
            if index != self.active_slot {
                self.switch_buffer(index);
            } else {
                self.message = format!("Already open: {name}");
            }
            return;
        }
        match Document::open(path) {
            Ok(doc) => {
                self.record_current_jump();
                let previous = self.active_slot;
                let current = self.take_current();
                self.slots[previous] = Some(current);
                let index = self.slots.len();
                self.slots.push(None);
                self.buffer_ids.push(self.next_buffer_id);
                self.next_buffer_id += 1;
                self.last_slot = Some(previous);
                self.activate(
                    StoredBuffer {
                        doc,
                        undo: Vec::new(),
                        redo: Vec::new(),
                        scroll_row: 0,
                        scroll_col: 0,
                    },
                    index,
                );
            }
            Err(error) => self.message = format!("Open error: {error}"),
        }
    }

    fn quit(&mut self) {
        if self.dirty_buffers() > 0 {
            self.prompt = Some(Prompt::Quit);
        } else {
            self.running = false;
        }
    }

    fn command(&mut self, command: &str) {
        let command = command.trim();
        if !command.is_empty() && command.bytes().all(|byte| byte.is_ascii_digit()) {
            let line = command.bytes().fold(0usize, |line, digit| {
                line.saturating_mul(10)
                    .saturating_add((digit - b'0') as usize)
            });
            self.go_to_line(line);
        } else if !command.is_empty() {
            self.lua_command(command);
        }
        self.sync_lua_events();
    }

    fn go_to_line(&mut self, line: usize) {
        let row = line.saturating_sub(1).min(self.doc.lines.len() - 1);
        let col = self.doc.lines[row]
            .iter()
            .position(|ch| !ch.is_whitespace())
            .unwrap_or(0);
        if (self.doc.row, self.doc.col) != (row, col) {
            self.record_current_jump();
        }
        self.dismiss_completion();
        self.doc.row = row;
        self.doc.col = col;
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
        self.count = None;
        self.return_to_editor_after_switch();
        if let Some((_, rows)) = self.screen.size {
            let height = (rows.saturating_sub(2) as usize).max(1);
            if row < self.scroll_row || row >= self.scroll_row.saturating_add(height) {
                self.scroll_row = row
                    .saturating_sub(height / 2)
                    .min(self.doc.lines.len().saturating_sub(height));
            }
        }
        self.message = format!("Line {} of {}", row + 1, self.doc.lines.len());
    }

    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.message = "Cancelled".into();
            return;
        }
        match prompt {
            Prompt::Quit => match key.code {
                KeyCode::Char('y' | 'Y') => self.running = false,
                KeyCode::Char('n' | 'N') => self.message = "Quit cancelled".into(),
                _ => self.prompt = Some(Prompt::Quit),
            },
            Prompt::SaveAs(mut name) => match key.code {
                KeyCode::Enter if !name.is_empty() => {
                    self.set_path_and_save(PathBuf::from(name));
                }
                KeyCode::Backspace => {
                    name.pop();
                    self.prompt = Some(Prompt::SaveAs(name));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    name.push(c);
                    self.prompt = Some(Prompt::SaveAs(name));
                }
                _ => self.prompt = Some(Prompt::SaveAs(name)),
            },
            Prompt::Open(mut name) => match key.code {
                KeyCode::Enter if !name.is_empty() => self.open(name),
                KeyCode::Backspace => {
                    name.pop();
                    self.prompt = Some(Prompt::Open(name));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    name.push(c);
                    self.prompt = Some(Prompt::Open(name));
                }
                _ => self.prompt = Some(Prompt::Open(name)),
            },
            Prompt::Command(mut command) => match key.code {
                KeyCode::Enter => self.command(&command),
                KeyCode::Backspace => {
                    command.pop();
                    self.prompt = Some(Prompt::Command(command));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    command.push(c);
                    self.prompt = Some(Prompt::Command(command));
                }
                _ => self.prompt = Some(Prompt::Command(command)),
            },
        }
    }

    fn key(&mut self, key: KeyEvent, page_height: usize) {
        if self
            .completion_menu
            .as_ref()
            .is_some_and(|menu| !self.completion_context_matches(&menu.context))
        {
            self.dismiss_completion();
        }
        if self.completion_eligible() {
            // Legacy terminals encode Ctrl+Space as NUL; enhanced keyboard
            // protocols can report Ctrl+Space or Ctrl+@ explicitly.
            let invoke = key.code == KeyCode::Null
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char(' ' | '@')));
            if invoke {
                self.request_completion(true);
                return;
            }
            if let Some(menu) = &mut self.completion_menu {
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    match key.code {
                        KeyCode::Tab => {
                            self.accept_completion();
                            self.sync_lua_events();
                            return;
                        }
                        KeyCode::Down => {
                            menu.selected = (menu.selected + 1) % menu.items.len();
                            return;
                        }
                        KeyCode::Up | KeyCode::BackTab => {
                            menu.selected =
                                (menu.selected + menu.items.len() - 1) % menu.items.len();
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
        let before = (
            self.active_slot,
            self.doc.path.clone(),
            self.doc.revision,
            self.doc.row,
            self.doc.col,
            self.mode,
        );
        self.key_inner(key, page_height);
        let after = (
            self.active_slot,
            self.doc.path.clone(),
            self.doc.revision,
            self.doc.row,
            self.doc.col,
            self.mode,
        );
        let cancel = key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g'));
        if before != after || !self.completion_eligible() || cancel {
            self.dismiss_completion();
            let typing = matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            ) && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            if before.2 != after.2
                && typing
                && self.completion_eligible()
                && self.completion_trigger()
            {
                self.completion_due = Some(Instant::now() + COMPLETION_DELAY);
            }
        }
        self.sync_lua_events();
    }

    fn key_inner(&mut self, key: KeyEvent, page_height: usize) {
        // Focused PTYs and prompts own their input. Their keys must never leak
        // into editor commands, including Lua commands registered at runtime.
        if let Ok(Some(active)) = self.with_lua(10, |_| self.toggleterm.active()) {
            if popup_shortcut(key).as_deref().is_some_and(|name| {
                self.with_lua(10, |_| self.toggleterm.dismiss(name))
                    .unwrap_or(false)
            }) {
                let _ = self.with_lua(10, |_| self.toggleterm.hide());
                return;
            }
            if let Some(session) = self.terminals.get_mut(&active) {
                if let Err(error) = session.input(key) {
                    self.message = format!("PTY input: {error}");
                }
            }
            return;
        }
        if self.hover_popup.take().is_some() && key.code == KeyCode::Esc {
            return;
        }
        if self.prompt.is_some() {
            self.prompt_key(key);
            return;
        }
        // These focus-changing shortcuts retain their availability in pickers
        // and the tree. Their actions and availability are defined in Lua.
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('t' | 'n' | 'p'))
            && self.dispatch_lua_key(key, page_height)
        {
            return;
        }
        if self.mason_picker.is_some() {
            self.mason_picker_key(key);
            return;
        }
        if self.buffer_picker.is_some() {
            self.buffer_picker_key(key);
            return;
        }
        if self.mode == Mode::Normal
            && (key.modifiers == KeyModifiers::ALT
                || (key.modifiers == KeyModifiers::CONTROL
                    && matches!(key.code, KeyCode::PageDown | KeyCode::PageUp)))
            && self.dispatch_lua_key(key, page_height)
        {
            return;
        }
        if self.tree.as_ref().is_some_and(|tree| tree.focused) {
            self.tree_key(key);
            return;
        }
        // Legacy terminals encode Ctrl+I and Tab identically. Preserve the
        // jumplist-before-tree-focus rule without hiding this ambiguity in Lua.
        if self.mode == Mode::Normal
            && key.code == KeyCode::Tab
            && key.modifiers == KeyModifiers::NONE
        {
            if self.jumps.can_forward() {
                self.jump_forward();
                return;
            }
            if let Some(tree) = &mut self.tree {
                if tree.visible {
                    tree.focus();
                    return;
                }
            }
        }
        if key.code == KeyCode::Esc {
            self.sequence = Sequence::None;
            self.count = None;
        }
        if self.sequence != Sequence::None
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.dispatch_lua_key(key, page_height);
            return;
        }
        if self.sequence != Sequence::None {
            let sequence = std::mem::take(&mut self.sequence);
            if let KeyCode::Char(name) = key.code {
                match sequence {
                    Sequence::MarkSet => self.set_mark(name),
                    Sequence::MarkLine => self.jump_to_mark(name, true),
                    Sequence::MarkExact => self.jump_to_mark(name, false),
                    Sequence::None => {}
                }
            }
            self.count = None;
            return;
        }
        // Counts and arbitrary mark names are modal input syntax; the
        // operations they drive and all ordinary keymaps live in Lua.
        if self.mode == Mode::Normal
            && !self.lua.key_pending()
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if let KeyCode::Char(digit @ '0'..='9') = key.code {
                if digit != '0' || self.count.is_some() {
                    self.count = Some(
                        self.count
                            .unwrap_or(0)
                            .saturating_mul(10)
                            .saturating_add((digit as u8 - b'0') as usize),
                    );
                    return;
                }
            }
        }
        if self.dispatch_lua_key(key, page_height) {
            return;
        }
        self.count = None;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return;
        }
        if self.mode == Mode::Normal {
            self.sequence = match key.code {
                KeyCode::Char('m') => Sequence::MarkSet,
                KeyCode::Char('\'') => Sequence::MarkLine,
                KeyCode::Char('`') => Sequence::MarkExact,
                _ => Sequence::None,
            };
        } else if self.mode == Mode::Insert {
            // Literal input is the native hot path. Lua can override any key
            // above, while normal typing keeps the existing undo grouping.
            match key.code {
                KeyCode::Backspace if self.doc.col > 0 || self.doc.row > 0 => {
                    self.insert_change();
                    self.doc.backspace();
                }
                KeyCode::Delete => {
                    self.insert_change();
                    self.doc.delete();
                }
                KeyCode::Enter => {
                    self.insert_change();
                    self.doc.insert('\n');
                }
                KeyCode::Tab => {
                    self.insert_change();
                    self.doc.insert('\t');
                }
                KeyCode::Char(c) => {
                    self.insert_change();
                    self.doc.insert(c);
                }
                _ => {}
            }
        }
    }

    fn scroll_lines(&mut self, down: bool, amount: usize, height: usize) {
        self.dismiss_completion();
        let height = height.max(1);
        let last_row = self.doc.lines.len() - 1;
        let max_top = self.doc.lines.len().saturating_sub(height);
        // Normalize a viewport that has not been painted since a jump/resize.
        let top = self
            .scroll_row
            .min(self.doc.row)
            .min(max_top)
            .max(self.doc.row.saturating_sub(height - 1));
        let row = if down {
            self.doc.row.saturating_add(amount).min(last_row)
        } else {
            self.doc.row.saturating_sub(amount)
        };
        let top = if down {
            top.saturating_add(amount).min(max_top)
        } else {
            top.saturating_sub(amount)
        };
        if row != self.doc.row {
            self.record_current_jump();
        }
        self.doc.row = row;
        // Intermediate short lines must not erase the destination column.
        self.doc.col = self.doc.col.min(self.doc.line().len());
        self.scroll_row = top.min(row).max(row.saturating_sub(height - 1));
        self.sequence = Sequence::None;
        self.count = None;
        if self.mode == Mode::Visual {
            self.update_visual_selection();
        }
    }

    fn ensure_visible(&mut self, width: usize, height: usize) {
        if self.doc.row < self.scroll_row {
            self.scroll_row = self.doc.row;
        }
        if height > 0 && self.doc.row >= self.scroll_row + height {
            self.scroll_row = self.doc.row - height + 1;
        }
        let column = visual_column(&self.doc.line()[..self.doc.col]);
        if column < self.scroll_col {
            self.scroll_col = column;
        }
        if width > 0 && column >= self.scroll_col + width {
            self.scroll_col = column - width + 1;
        }
    }

    fn draw(&mut self, output: &mut impl Write) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        self.draw_at(output, cols, rows)
    }

    // Compare the current viewport with the last painted frame. A simple j/k
    // movement repaints only changed gutter/active rows and the status line,
    // without clearing the screen.
    fn draw_at(&mut self, output: &mut impl Write, cols: u16, rows: u16) -> io::Result<()> {
        let syntax_invalid = self.syntax.refresh(&mut self.doc);
        if cols == 0 || rows == 0 {
            self.dismiss_completion();
            self.screen = ScreenCache::default();
            return Ok(());
        }

        // Painting moves the terminal cursor through the gutter and sidebar.
        // Hide it for every frame, then show it only after the final MoveTo
        // restores its editing position, even when output arrives in chunks.
        queue!(output, Hide)?;

        let width = cols as usize;
        let height = rows.saturating_sub(2) as usize;
        let sidebar_width =
            tree_sidebar_width(width, self.tree.as_ref().is_some_and(|tree| tree.visible));
        let editor_width = width - sidebar_width;
        let gutter_width = gutter_width(editor_width, self.doc.lines.len());
        let text_width = editor_width - gutter_width;
        self.ensure_visible(text_width, height);

        let size_changed = self.screen.size != Some((cols, rows));
        let viewport_changed = self.screen.origin != Some((self.scroll_row, self.scroll_col));
        let syntax_changed = self.screen.syntax_enabled != Some(self.syntax.enabled());
        let sidebar_changed = self.screen.sidebar_width != sidebar_width;
        let repaint_all = size_changed || viewport_changed || syntax_changed || sidebar_changed;

        // Initial frame, resize or a changed split geometry requires a full repaint.
        // Normal navigation in either pane still only changes the affected rows.
        if size_changed || sidebar_changed {
            queue!(output, ResetColor, MoveTo(0, 0), Clear(ClearType::All))?;
            self.screen = ScreenCache::default();
        }
        self.screen.lines.resize_with(height, || None);

        let tree_view = match self
            .tree
            .as_ref()
            .map(|tree| self.with_lua(height, |_| tree.view(height.saturating_sub(1))))
        {
            Some(Ok(view)) => Some(view),
            Some(Err(error)) => {
                self.message = format!("Tree render error: {error}");
                None
            }
            None => None,
        };
        let buffer_view = self.buffer_picker_view();
        let mason_view = self.mason_view();
        let hover_view = self.hover_view();
        let finder_rect = if buffer_view.is_some()
            || mason_view.is_some()
            || hover_view.is_some()
            || tree_view
                .as_ref()
                .and_then(|view| view.finder.as_ref())
                .is_some()
        {
            finder_rect_for(cols, height)
        } else {
            None
        };

        // On closing the dialog, invalidate just the rows it covered.
        // That restores the existing tree/editor content without clearing the
        // whole screen or affecting the incremental renderer's other rows.
        if let Some(old) = self.screen.finder.set_rect(finder_rect) {
            for y in old.y as usize..old.y as usize + old.height {
                if let Some(line) = self.screen.lines.get_mut(y) {
                    *line = None;
                }
                if let Some(line) = self.screen.sidebar_lines.get_mut(y) {
                    *line = None;
                }
            }
        }

        let active_terminal = self
            .with_lua(10, |_| self.toggleterm.active())
            .ok()
            .flatten();
        let popup_rect = active_terminal
            .as_ref()
            .filter(|kind| self.terminals.contains_key(kind.as_str()))
            .and_then(|_| terminal_rect(cols, height));
        if let Some(old) = self.screen.terminal.set_rect(popup_rect) {
            // Repaint only the previously covered region when hiding.
            for y in old.y as usize..old.y as usize + old.height as usize {
                if let Some(line) = self.screen.lines.get_mut(y) {
                    *line = None;
                }
                if let Some(line) = self.screen.sidebar_lines.get_mut(y) {
                    *line = None;
                }
                self.screen
                    .finder
                    .invalidate_underpaint(old.x as usize, y, old.width as usize);
            }
        }

        let completion_rect =
            if finder_rect.is_none() && popup_rect.is_none() && self.completion_eligible() {
                self.completion_menu.as_ref().and_then(|menu| {
                    completion_ui::rect_for(
                        cols,
                        height,
                        (
                            sidebar_width
                                + gutter_width
                                + visual_column(&self.doc.line()[..self.doc.col])
                                    .saturating_sub(self.scroll_col),
                            self.doc.row.saturating_sub(self.scroll_row),
                        ),
                        &menu.labels,
                    )
                })
            } else {
                None
            };
        // Never let Tab accept a suggestion that the terminal cannot show.
        if completion_rect.is_none() && self.completion_menu.is_some() {
            self.dismiss_completion();
        }
        if let Some(old) = self.screen.completion.set_rect(completion_rect) {
            for y in old.y as usize..old.y as usize + old.height {
                if let Some(line) = self.screen.lines.get_mut(y) {
                    *line = None;
                }
                if let Some(line) = self.screen.sidebar_lines.get_mut(y) {
                    *line = None;
                }
            }
        }

        let mut selected_tree_row = 0;
        let mut search_cursor: Option<usize> = None;
        if sidebar_width > 0 {
            if let Some(view) = tree_view.as_ref() {
                selected_tree_row = view.selected;
                search_cursor = view.search_cursor;
                let focused = self.tree.as_ref().is_some_and(|tree| tree.focused);
                self.screen.sidebar_lines.resize_with(height, || None);
                for y in 0..height {
                    let text = if y == 0 {
                        view.title.clone()
                    } else {
                        view.lines.get(y - 1).cloned().unwrap_or_default()
                    };
                    let active = y > 0 && y == view.selected;
                    let cell = SidebarPaint {
                        text,
                        active,
                        focused,
                    };
                    if self.screen.sidebar_lines[y].as_ref() == Some(&cell) {
                        continue;
                    }
                    let background = if active && focused {
                        Color::Rgb {
                            r: 65,
                            g: 64,
                            b: 78,
                        }
                    } else if active {
                        Color::Rgb {
                            r: 49,
                            g: 48,
                            b: 59,
                        }
                    } else {
                        Color::Rgb {
                            r: 34,
                            g: 35,
                            b: 42,
                        }
                    };
                    let foreground = if active {
                        Color::Rgb {
                            r: 226,
                            g: 213,
                            b: 255,
                        }
                    } else {
                        Color::Rgb {
                            r: 174,
                            g: 179,
                            b: 194,
                        }
                    };
                    queue!(
                        output,
                        MoveTo(0, y as u16),
                        SetBackgroundColor(background),
                        SetForegroundColor(foreground),
                        Print(fit_cells(&cell.text, sidebar_width.saturating_sub(1))),
                        ResetColor,
                        SetForegroundColor(Color::Rgb {
                            r: 82,
                            g: 84,
                            b: 96
                        }),
                        Print("│"),
                        ResetColor
                    )?;
                    self.screen.sidebar_lines[y] = Some(cell);
                    // This row may lie behind the centered finder. If the
                    // sidebar just repainted it, the popup must redraw that
                    // portion even when its own text is unchanged.
                    self.screen
                        .finder
                        .invalidate_underpaint(0, y, sidebar_width);
                    self.screen
                        .terminal
                        .invalidate_underpaint(0, y, sidebar_width);
                    self.screen
                        .completion
                        .invalidate_underpaint(0, y, sidebar_width);
                }
            }
        }

        for y in 0..height {
            let index = y + self.scroll_row;
            let line = self.doc.lines.get(index);
            let content = line.map(Vec::as_slice);
            let active = index == self.doc.row && line.is_some();
            let number = gutter_number(index, self.doc.row, self.doc.lines.len(), gutter_width);

            let old = self.screen.lines[y].as_ref();
            let text_stale = old.is_none_or(|painted| {
                painted.index != index || painted.content.as_deref() != content
            });
            let highlight_changed = old.is_none_or(|painted| painted.active != active);
            let selected = self
                .selection
                .and_then(|range| range.columns(index, line.map_or(0, Vec::len)));
            let selection_changed = old.is_none_or(|painted| painted.selected != selected);
            let gutter_changed = old.is_none_or(|painted| painted.gutter != number);
            let syntax_stale =
                self.syntax.enabled() && syntax_invalid.is_some_and(|first| index >= first);
            let paint_text =
                repaint_all || text_stale || highlight_changed || syntax_stale || selection_changed;

            if !paint_text && !gutter_changed {
                continue;
            }
            if gutter_width > 0 && (paint_text || gutter_changed) {
                queue!(
                    output,
                    MoveTo(sidebar_width as u16, y as u16),
                    SetBackgroundColor(GUTTER_BG),
                    SetForegroundColor(if active { CURRENT_NUMBER_FG } else { GUTTER_FG }),
                    Print(&number),
                    ResetColor
                )?;
                self.screen
                    .finder
                    .invalidate_underpaint(sidebar_width, y, gutter_width);
                self.screen
                    .terminal
                    .invalidate_underpaint(sidebar_width, y, gutter_width);
                self.screen
                    .completion
                    .invalidate_underpaint(sidebar_width, y, gutter_width);
            }
            if paint_text {
                queue!(
                    output,
                    MoveTo((sidebar_width + gutter_width) as u16, y as u16),
                    ResetColor
                )?;
                if active {
                    queue!(output, SetBackgroundColor(CURRENT_LINE_BG))?;
                }

                let mut visible_width = 0;
                if let Some(line) = line {
                    if let Some(columns) = selected {
                        let spans = self.syntax.spans(&self.doc, index);
                        for (foreground, background, text) in render_selected_line(
                            line,
                            spans.unwrap_or(&[]),
                            self.scroll_col,
                            text_width,
                            columns,
                            active,
                        ) {
                            visible_width += UnicodeWidthStr::width(text.as_str());
                            queue!(
                                output,
                                SetForegroundColor(foreground),
                                SetBackgroundColor(background),
                                Print(text)
                            )?;
                        }
                        queue!(output, ResetColor)?;
                    } else if let Some(spans) = self
                        .syntax
                        .spans(&self.doc, index)
                        .filter(|s| !s.is_empty())
                    {
                        for (color, text) in
                            render_color_line(line, spans, self.scroll_col, text_width)
                        {
                            visible_width += UnicodeWidthStr::width(text.as_str());
                            queue!(output, SetForegroundColor(color), Print(text))?;
                        }
                    } else {
                        let text = render_line(line, self.scroll_col, text_width);
                        visible_width = UnicodeWidthStr::width(text.as_str());
                        queue!(output, Print(text))?;
                    }
                }
                if active {
                    // Fill to the right edge without clearing the terminal.
                    // This also highlights empty lines and trailing whitespace.
                    queue!(
                        output,
                        SetBackgroundColor(CURRENT_LINE_BG),
                        SetForegroundColor(Color::Reset),
                        Print(" ".repeat(text_width.saturating_sub(visible_width))),
                        ResetColor
                    )?;
                } else {
                    // Remove any tail left by a previously longer line.
                    queue!(output, ResetColor, Clear(ClearType::UntilNewLine))?;
                }
                self.screen.finder.invalidate_underpaint(
                    sidebar_width + gutter_width,
                    y,
                    text_width,
                );
                self.screen.terminal.invalidate_underpaint(
                    sidebar_width + gutter_width,
                    y,
                    text_width,
                );
                self.screen.completion.invalidate_underpaint(
                    sidebar_width + gutter_width,
                    y,
                    text_width,
                );
            }
            self.screen.lines[y] = Some(PaintedLine {
                index,
                content: line.cloned(),
                gutter: number,
                active,
                selected,
            });
        }

        if rows >= 2 {
            let filename = self
                .doc
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[No Name]".into());
            let mark = if self.doc.dirty { "**" } else { "--" };
            let mode = match self.mode {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT",
                Mode::Visual => "VISUAL",
            };
            let language = if self.syntax.enabled() {
                self.syntax.name()
            } else {
                "Highlight off"
            };
            let pane = if self.tree.as_ref().is_some_and(|tree| tree.visible) {
                " | TREE"
            } else {
                ""
            };
            // Keep global buffer counts before long paths so they stay visible
            // even on smaller terminals. The dirty count includes inactive files.
            let dirty = self.dirty_buffers();
            let save_hint = if dirty > 0 { "  SPC b s" } else { "" };
            let status = format!(
                " {mark} Remacs [{mode}] | B {}/{}  ●{dirty}{save_hint} | {filename} | {language}{pane} | Ln {}, Col {} ",
                self.active_slot + 1, self.slots.len(),
                self.doc.row + 1, self.doc.col + 1
            );
            let status = fit_cells(&status, width);
            if self.screen.status.as_deref() != Some(status.as_str()) {
                queue!(
                    output,
                    MoveTo(0, rows - 2),
                    SetBackgroundColor(Color::DarkGrey),
                    SetForegroundColor(Color::White),
                    Print(&status),
                    ResetColor
                )?;
                self.screen.status = Some(status);
            }
        }
        if rows >= 1 {
            let message = match &self.prompt {
                Some(Prompt::SaveAs(name)) => format!("Save file: {name}"),
                Some(Prompt::Open(name)) => format!("Find file: {name}"),
                Some(Prompt::Command(command)) => format!(":{command}"),
                Some(Prompt::Quit) => format!(
                    "Discard unsaved changes in {} buffer(s) and quit? (y/n)", self.dirty_buffers()
                ),
                None if self.mason_picker.is_some() =>
                    "MASON  ↑↓ Select · Enter Install · Ctrl+U Update · Ctrl+S Sync · Ctrl+R Update All".into(),
                None if self.buffer_picker.is_some() =>
                    "BUFFERS  Type to filter · ↑↓ Choose · Enter Switch · Esc Cancel".into(),
                None if self.completion_menu.is_some() => {
                    let more = if self.completion_menu.as_ref().unwrap().incomplete {
                        " · Type for more"
                    } else { "" };
                    format!("COMPLETE  ↑↓ Choose · Tab Accept · Ctrl+G Dismiss · Enter Newline{more}")
                }
                None if self.tree.as_ref().is_some_and(|tree| tree.focused) =>
                    "TREE  / or f Find  j/k Move  l/Enter Open  . Hidden  r Refresh  Tab/Esc Editor  q Close".into(),
                None => match self.sequence {
                    Sequence::MarkSet => "m → a-z File mark | A-Z Global mark".into(),
                    Sequence::MarkLine => "' → mark (first nonblank on line)".into(),
                    Sequence::MarkExact => "` → mark (exact position)".into(),
                    Sequence::None => if self.lua.key_pending() {
                        format!("{} →", self.lua.pending_keys())
                    } else { self.count.map_or_else(|| self.message.clone(), |n| n.to_string()) },
                },
            };
            let message = fit_cells(&message, width);
            if self.screen.message.as_deref() != Some(message.as_str()) {
                queue!(output, MoveTo(0, rows - 1), ResetColor, Print(&message))?;
                self.screen.message = Some(message);
            }
        }
        if let (Some(rect), Some(menu)) = (completion_rect, self.completion_menu.as_ref()) {
            self.screen
                .completion
                .paint(output, rect, &menu.labels, menu.selected)?;
        }

        // Overlay the fully visible fzf-style picker after the normal editor
        // and sidebar have painted; only changed popup rows are output.
        if let Some(rect) = finder_rect {
            if let Some(finder) = mason_view.as_ref() {
                self.screen
                    .finder
                    .paint(output, rect, finder, PickerKind::Mason)?;
            } else if let Some(finder) = hover_view.as_ref() {
                self.screen
                    .finder
                    .paint(output, rect, finder, PickerKind::Hover)?;
            } else if let Some(finder) = buffer_view.as_ref() {
                self.screen
                    .finder
                    .paint(output, rect, finder, PickerKind::Buffers)?;
            } else if let Some(finder) = tree_view.as_ref().and_then(|view| view.finder.as_ref()) {
                self.screen
                    .finder
                    .paint(output, rect, finder, PickerKind::Files)?;
            }
        }

        // The terminal is composed above both editor and fuzzy popups, and
        // never forwards child cursor-control codes to the outer terminal.
        if let (Some(rect), Some(kind)) = (popup_rect, active_terminal.as_ref()) {
            if let Some(session) = self.terminals.get_mut(kind) {
                if let Err(error) = session.resize(rect) {
                    self.message = format!("PTY resize: {error}");
                }
                self.screen.terminal.paint(output, rect, session)?;
            }
        }

        let (x, y) = match &self.prompt {
            _ if popup_rect.is_some() => {
                let rect = popup_rect.unwrap();
                let kind = active_terminal.as_ref().unwrap();
                let (x, y) = self.terminals.get(kind).unwrap().cursor(rect);
                (x as usize, y)
            }
            Some(Prompt::SaveAs(name)) => (
                "Save file: ".len() + name.chars().count(),
                rows.saturating_sub(1),
            ),
            Some(Prompt::Open(name)) => (
                "Find file: ".len() + name.chars().count(),
                rows.saturating_sub(1),
            ),
            Some(Prompt::Command(command)) => (1 + command.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Quit) => (0, rows.saturating_sub(1)),
            None if finder_rect.is_some() => {
                let rect = finder_rect.unwrap();
                let query = mason_view
                    .as_ref()
                    .or(buffer_view.as_ref())
                    .or_else(|| tree_view.as_ref().and_then(|view| view.finder.as_ref()))
                    .map_or("", |finder| finder.query.as_str());
                let (x, y) = finder_cursor(rect, query);
                (x as usize, y)
            }
            None if sidebar_width > 0 && self.tree.as_ref().is_some_and(|tree| tree.focused) => (
                search_cursor.map_or(2, |x| x.min(sidebar_width.saturating_sub(2))),
                if search_cursor.is_some() {
                    0
                } else {
                    selected_tree_row as u16
                },
            ),
            None => (
                sidebar_width
                    + gutter_width
                    + visual_column(&self.doc.line()[..self.doc.col])
                        .saturating_sub(self.scroll_col),
                self.doc.row.saturating_sub(self.scroll_row) as u16,
            ),
        };
        if cols > 0 && rows > 0 {
            queue!(
                output,
                MoveTo((x.min(width - 1)) as u16, y.min(rows - 1)),
                Show
            )?;
        }
        self.screen.size = Some((cols, rows));
        self.screen.origin = Some((self.scroll_row, self.scroll_col));
        self.screen.syntax_enabled = Some(self.syntax.enabled());
        self.screen.sidebar_width = sidebar_width;
        output.flush()
    }
}

// Convert terminal key events into Lua plugin binding names. This is a
// generic dispatcher, not a hardcoded terminal-plugin shortcut table.
/// Resolve the opened file relative to the editor's actual working
/// directory *before* traversing ancestors. For e.g. "src/main.rs",
/// Path::ancestors() eventually yields an empty path; returning that as the
/// Git root causes portable-pty to reject its working directory.
fn project_root_for(file: Option<&Path>, cwd: &Path) -> PathBuf {
    let current = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let start = file
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| {
            if parent.is_absolute() {
                parent.to_path_buf()
            } else {
                current.join(parent)
            }
        })
        // The parent of an as-yet-unsaved file may not exist; use the
        // current directory instead of passing an invalid cwd to the PTY.
        .and_then(|parent| parent.canonicalize().ok().filter(|path| path.is_dir()))
        .unwrap_or_else(|| current.clone());

    // Git worktrees may have a .git *file*, not a directory.
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    for dir in start.ancestors() {
        if ["Cargo.toml", "go.mod", "package.json"]
            .iter()
            .any(|marker| dir.join(marker).exists())
        {
            return dir.to_path_buf();
        }
    }
    // Always return a real absolute directory; never the empty path.
    start
}

fn popup_shortcut(key: KeyEvent) -> Option<String> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let modifiers = key.modifiers;
    if modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT) {
        Some(format!("ctrl-{}", c.to_ascii_lowercase()))
    } else if modifiers.contains(KeyModifiers::ALT) && !modifiers.contains(KeyModifiers::CONTROL) {
        Some(format!("alt-{}", c.to_ascii_lowercase()))
    } else {
        None
    }
}

fn tree_sidebar_width(columns: usize, visible: bool) -> usize {
    if !visible || columns < 48 {
        return 0;
    }
    (columns / 3).clamp(24, 34).min(columns - 20)
}

fn gutter_width(columns: usize, total_lines: usize) -> usize {
    // Never sacrifice the entire text area when the terminal is narrow.
    if columns < 5 {
        0
    } else {
        (total_lines.to_string().len().max(2) + 2).min(columns - 1)
    }
}

fn gutter_number(index: usize, cursor: usize, total_lines: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if index >= total_lines {
        return " ".repeat(width);
    }
    let number = if index == cursor {
        index + 1
    } else {
        index.abs_diff(cursor)
    };
    let digits = number.to_string();
    let available = width.saturating_sub(2);
    // On exceptionally narrow screens, keep the gutter inside the viewport.
    let display = &digits[digits.len().saturating_sub(available)..];
    format!("{display:>available$}  ")
}

fn char_width(c: char, column: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - (column % TAB_WIDTH)
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

fn visual_column(line: &[char]) -> usize {
    let mut column = 0;
    for &c in line {
        column += char_width(c, column);
    }
    column
}

fn render_line(line: &[char], start: usize, width: usize) -> String {
    let mut output = String::new();
    let mut column = 0;
    let end = start.saturating_add(width);
    for &c in line {
        let size = char_width(c, column);
        if column >= end {
            break;
        }
        if c == '\t' || c.is_control() {
            for i in 0..size.max(usize::from(c.is_control())) {
                if column + i >= start && column + i < end {
                    output.push(' ');
                }
            }
        } else if size == 0 {
            if column > start && column <= end {
                output.push(c);
            }
        } else if column >= start && column + size <= end {
            output.push(c);
        } else {
            for i in 0..size {
                if column + i >= start && column + i < end {
                    output.push(' ');
                }
            }
        }
        column += size;
    }
    output
}

fn render_color_line(
    line: &[char],
    spans: &[Span],
    start: usize,
    width: usize,
) -> Vec<(Color, String)> {
    let mut runs: Vec<(Color, String)> = Vec::new();
    let mut column = 0;
    let end = start.saturating_add(width);
    for span in spans {
        for &c in &line[span.start..span.end] {
            if column >= end {
                break;
            }
            let size = char_width(c, column);
            let mut visible = String::new();
            if c == '\t' || c.is_control() {
                for i in 0..size.max(usize::from(c.is_control())) {
                    if column + i >= start && column + i < end {
                        visible.push(' ');
                    }
                }
            } else if size == 0 {
                if column > start && column <= end {
                    visible.push(c);
                }
            } else if column >= start && column + size <= end {
                visible.push(c);
            } else {
                for i in 0..size {
                    if column + i >= start && column + i < end {
                        visible.push(' ');
                    }
                }
            }
            if !visible.is_empty() {
                if let Some((last_color, text)) = runs.last_mut() {
                    if *last_color == span.foreground {
                        text.push_str(&visible);
                    } else {
                        runs.push((span.foreground, visible));
                    }
                } else {
                    runs.push((span.foreground, visible));
                }
            }
            column += size;
        }
    }
    runs
}

fn render_selected_line(
    line: &[char],
    spans: &[Span],
    scroll: usize,
    width: usize,
    selection: (usize, usize),
    active: bool,
) -> Vec<(Color, Color, String)> {
    let mut runs: Vec<(Color, Color, String)> = Vec::new();
    let mut visual_col = 0;
    let mut syntax_index = 0;
    let end = scroll.saturating_add(width);
    for (index, &ch) in line.iter().enumerate() {
        if visual_col >= end {
            break;
        }
        while syntax_index < spans.len() && index >= spans[syntax_index].end {
            syntax_index += 1;
        }
        let foreground = spans
            .get(syntax_index)
            .filter(|span| index >= span.start && index < span.end)
            .map_or(Color::Reset, |span| span.foreground);
        let background = if (selection.0..selection.1).contains(&index) {
            VISUAL_SELECTION_BG
        } else if active {
            CURRENT_LINE_BG
        } else {
            Color::Reset
        };
        let size = char_width(ch, visual_col);
        let mut visible = String::new();
        if ch == '\t' || ch.is_control() {
            for offset in 0..size.max(usize::from(ch.is_control())) {
                if visual_col + offset >= scroll && visual_col + offset < end {
                    visible.push(' ');
                }
            }
        } else if size == 0 {
            if visual_col > scroll && visual_col <= end {
                visible.push(ch);
            }
        } else if visual_col >= scroll && visual_col + size <= end {
            visible.push(ch);
        } else {
            for offset in 0..size {
                if visual_col + offset >= scroll && visual_col + offset < end {
                    visible.push(' ');
                }
            }
        }
        if !visible.is_empty() {
            if let Some((fg, bg, text)) = runs.last_mut() {
                if *fg == foreground && *bg == background {
                    text.push_str(&visible);
                } else {
                    runs.push((foreground, background, visible));
                }
            } else {
                runs.push((foreground, background, visible));
            }
        }
        visual_col += size;
    }
    runs
}

fn fit_cells(input: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for c in input.chars() {
        let size = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + size > width {
            break;
        }
        result.push(c);
        used += size;
    }
    result.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    result
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            ResetColor,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let path = args.next();
    if matches!(path.as_deref(), Some("--help") | Some("-h")) {
        println!("Remacs — minimal modal terminal editor\nUsage: remacs [file]\nOpen: SPC f f | Save: SPC f s | Quit: SPC q q\nInsert: i | Normal: Esc | Commands: :w :q :q!");
        return Ok(());
    }
    if matches!(path.as_deref(), Some("--version") | Some("-V")) {
        println!("remacs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.next().is_some() {
        eprintln!("Usage: remacs [file]");
        std::process::exit(2);
    }
    let doc = match path {
        Some(path) => Document::open(PathBuf::from(path))?,
        None => Document::empty(),
    };
    let _terminal = TerminalGuard::enter()?;
    let mut stdout = io::stdout();
    let mut editor = Editor::new(doc);
    editor.load_lua_config();
    while editor.running {
        // A PTY can produce output without key presses. Event polling
        // ensures interactive TUI refreshes while keeping idle CPU low.
        editor.poll_terminals();
        editor.poll_mason();
        editor.poll_lsp();
        editor.sync_lua_events();
        editor.draw(&mut stdout)?;
        let delay = if !editor.terminals.is_empty()
            || editor.completion_due.is_some()
            || editor.completion_pending.is_some()
        {
            Duration::from_millis(24)
        } else {
            Duration::from_millis(90)
        };
        if event::poll(delay)? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    let height = terminal::size()?.1.saturating_sub(2) as usize;
                    editor.key(key, height);
                }
                Event::Paste(text) => editor.pasted(&text),
                Event::Resize(width, height) => {
                    editor.emit_lua("resize", |data| {
                        data.set("width", width)?;
                        data.set("height", height)
                    });
                    editor.drain_lua_actions();
                }
                _ => {}
            }
        }
    }
    editor.emit_lua("editor_exit", |_| Ok(()));
    editor.drain_lua_actions();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, key: KeyCode) {
        editor.key(KeyEvent::new(key, KeyModifiers::NONE), 10);
    }

    fn paint(editor: &mut Editor) -> String {
        // Color assertions must not depend on the test runner's NO_COLOR.
        crossterm::style::force_color_output(true);
        let mut output = Vec::new();
        editor.draw_at(&mut output, 80, 18).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn temporary_buffer_project() -> PathBuf {
        static NEXT_PROJECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "remacs-buffers-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_PROJECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("one.rs"), "one\nsecond\nthird\n").unwrap();
        std::fs::write(root.join("two.md"), "# two\n").unwrap();
        root
    }

    #[test]
    fn popup_root_resolves_relative_files_inside_git_repository() {
        let project = temporary_buffer_project();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src/deep")).unwrap();
        let root = std::fs::canonicalize(&project).unwrap();

        // Regression: traversing ancestors of a relative path reached ""
        // and returned that empty path when .git was in the process cwd.
        for file in ["src/main.rs", "src/deep/module.rs", "one.rs"] {
            let found = project_root_for(Some(Path::new(file)), &project);
            assert_eq!(found, root, "wrong git root for {file}");
            assert!(found.is_absolute() && found.is_dir());
        }
        assert_eq!(project_root_for(None, &project), root);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn popup_root_resolves_absolute_paths_and_git_worktree_files() {
        let cwd = temporary_buffer_project();
        let other = temporary_buffer_project();
        std::fs::create_dir_all(other.join("src/deep")).unwrap();
        // Git linked worktrees use a .git text file with a gitdir: pointer.
        std::fs::write(other.join(".git"), "gitdir: /tmp/fake-worktree\n").unwrap();

        let file = other.join("src/deep/file.rs");
        let root = project_root_for(Some(&file), &cwd);
        assert_eq!(root, std::fs::canonicalize(&other).unwrap());
        assert!(root.is_dir() && root.is_absolute());
        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn popup_root_uses_existing_directory_for_new_or_unnamed_file() {
        let root = temporary_buffer_project();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='test'\n").unwrap();
        let expected = std::fs::canonicalize(&root).unwrap();

        for name in ["not-yet-created/main.rs", "src/file.rs"] {
            let found = project_root_for(Some(Path::new(name)), &root);
            assert_eq!(found, expected);
            assert!(found.is_dir() && found.is_absolute());
        }
        assert_eq!(project_root_for(None, &root), expected);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mason_opens_same_fuzzy_popup_and_filters_language_catalog() {
        let mut editor = Editor::new(Document::from_text("example"));
        editor.command("mason");
        assert!(editor.mason_picker.is_some());
        let out = paint(&mut editor);
        assert!(out.contains("MASON"));
        assert!(out.contains("rust_analyzer"));
        assert!(out.contains("Ctrl+S sync"));
        for key in ['g', 'o', 'p', 'l', 's'] {
            press(&mut editor, KeyCode::Char(key));
        }
        let entries = editor.mason_view().unwrap();
        assert_eq!(entries.total_matches, 1);
        assert!(entries.matches[0].contains("gopls"));
        editor.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10);
        assert!(editor.mason_picker.is_none());
        assert!(!paint(&mut editor).contains("MASON"));
    }

    #[test]
    fn mason_leader_binding_and_commands_queue_tools_without_starting_downloads() {
        let mut editor = Editor::new(Document::from_text("example"));
        for key in [' ', 'l', 'm'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert!(editor.mason_picker.is_some());
        editor.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 10);
        assert!(editor.mason_picker.is_some(), "installer UI stays visible");
        // An explicit update is allowed even if a system server exists.
        editor.command("mason update gopls");
        assert!(editor.message.contains("gopls"));
        editor.command("mason install nonexistent");
        assert!(editor.message.contains("Unknown language server"));
        editor.command("mason log");
        assert!(editor.message.contains("install.log"));
        // Test does not tick the installer. Nothing runs without poll_mason.
    }

    #[test]
    fn save_all_shortcut_persists_dirty_active_and_inactive_buffers() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        press(&mut editor, KeyCode::Char('i'));
        press(&mut editor, KeyCode::Char('X'));
        press(&mut editor, KeyCode::Esc);
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        press(&mut editor, KeyCode::Char('i'));
        press(&mut editor, KeyCode::Char('Y'));
        press(&mut editor, KeyCode::Esc);
        editor.scroll_row = 2;
        editor.scroll_col = 4;
        assert_eq!(editor.dirty_buffers(), 2);
        let before_undo = editor.undo.len();

        let status = paint(&mut editor);
        assert!(status.contains("B 2/2  ●2  SPC b s"));
        // Drawing may normalize scroll offsets for short files. Save-all
        // must preserve the viewport state as it stands at invocation time.
        let before_scroll = (editor.scroll_row, editor.scroll_col);
        for key in [' ', 'b', 's'] {
            press(&mut editor, KeyCode::Char(key));
        }

        assert_eq!(editor.dirty_buffers(), 0);
        assert_eq!(editor.active_slot, 1);
        assert_eq!((editor.scroll_row, editor.scroll_col), before_scroll);
        assert_eq!(
            editor.undo.len(),
            before_undo,
            "save-all must not wipe undo"
        );
        assert!(std::fs::read_to_string(root.join("one.rs"))
            .unwrap()
            .starts_with('X'));
        assert!(std::fs::read_to_string(root.join("two.md"))
            .unwrap()
            .starts_with('Y'));
        assert!(!editor.slots[0].as_ref().unwrap().doc.dirty);
        assert!(editor.message.contains("Saved 2 buffer"));
        assert!(paint(&mut editor).contains("B 2/2  ●0"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_all_skips_unnamed_buffers_without_discarding_changes() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::empty());
        editor.doc.insert('!');
        editor.open(root.join("one.rs").to_string_lossy().into_owned());
        editor.doc.insert('X');

        editor.command("wa");
        assert_eq!(editor.dirty_buffers(), 1);
        assert!(editor.message.contains("Saved 1"));
        assert!(editor.message.contains("1 unnamed"));
        assert!(editor.message.contains(":w FILE"));
        assert!(std::fs::read_to_string(root.join("one.rs"))
            .unwrap()
            .starts_with('X'));

        editor.switch_buffer(0);
        assert!(editor.doc.path.is_none());
        assert!(editor.doc.dirty);
        assert_eq!(editor.doc.line().first(), Some(&'!'));
        editor.set_path_and_save(root.join("scratch.txt"));
        assert_eq!(editor.dirty_buffers(), 0);
        assert_eq!(
            std::fs::read_to_string(root.join("scratch.txt")).unwrap(),
            "!"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_all_continues_after_error_and_reports_failed_buffer() {
        let root = temporary_buffer_project();
        let failed_path = root.join("does-not-exist").join("failed.rs");
        let mut editor = Editor::new(Document::empty());
        editor.doc.path = Some(failed_path.clone());
        editor.doc.insert('!');
        editor.open(root.join("one.rs").to_string_lossy().into_owned());
        editor.doc.insert('X');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.doc.insert('Y');
        assert_eq!(editor.dirty_buffers(), 3);

        editor.command("wall");
        assert_eq!(editor.active_slot, 2);
        assert_eq!(editor.dirty_buffers(), 1);
        assert_eq!(
            editor.slots[0].as_ref().unwrap().doc.path.as_ref(),
            Some(&failed_path)
        );
        assert!(editor.slots[0].as_ref().unwrap().doc.dirty);
        assert!(editor.message.contains("Saved 2"));
        assert!(editor.message.contains("1 failed"));
        assert!(editor.message.contains("failed.rs"));
        assert!(std::fs::read_to_string(root.join("one.rs"))
            .unwrap()
            .starts_with('X'));
        assert!(std::fs::read_to_string(root.join("two.md"))
            .unwrap()
            .starts_with('Y'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn clean_footer_stays_compact_and_save_all_is_noop() {
        let mut editor = Editor::new(Document::from_text("unchanged"));
        let status = paint(&mut editor);
        assert!(status.contains("B 1/1  ●0"));
        assert!(!status.contains("SPC b s"));
        editor.command("wa");
        assert_eq!(editor.message, "All buffers already saved");
        assert_eq!(editor.dirty_buffers(), 0);
    }

    #[test]
    fn ctrl_i_legacy_tab_retraces_entire_jumplist_even_with_tree_visible() {
        let root = temporary_buffer_project();
        std::fs::write(root.join("three.rs"), "three\nfour\n").unwrap();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.row = 2;
        editor.doc.col = 1;
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.open(root.join("three.rs").to_string_lossy().into_owned());

        // The tree can be visible, but we remain in the editor.
        editor.toggle_tree();
        editor.tree.as_mut().unwrap().leave();
        assert!(editor.tree.as_ref().unwrap().visible);

        let back = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        editor.key(back, 12);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        editor.key(back, 12);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("one.rs"));
        assert_eq!((editor.doc.row, editor.doc.col), (2, 1));

        // In legacy PTY input Ctrl+I arrives as KeyCode::Tab *without*
        // a Control modifier. It must go forward, not focus the tree.
        let legacy_ctrl_i = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        editor.key(legacy_ctrl_i, 12);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        editor.key(legacy_ctrl_i, 12);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("three.rs"));
        assert!(!editor.jumps.can_forward());

        // At the newest location ordinary Tab resumes its original
        // editor-to-tree focus behavior.
        editor.key(legacy_ctrl_i, 12);
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.focused));

        // Tab with focus in tree still returns to the editor.
        editor.key(legacy_ctrl_i, 12);
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_control_i_remains_forward_jump_with_enhanced_key_reporting() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.row = 2;
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), 12);
        assert_eq!(editor.active_slot, 0);
        editor.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL), 12);
        assert_eq!(editor.active_slot, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_o_and_alt_right_jump_back_and_forward_across_buffers() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.row = 2;
        editor.doc.col = 1;
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        assert_eq!(editor.active_slot, 1);
        // Exercise the actual keyboard event dispatch.
        editor.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), 12);
        assert_eq!(editor.active_slot, 0);
        assert_eq!((editor.doc.row, editor.doc.col), (2, 1));

        editor.key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), 12);
        assert_eq!(editor.active_slot, 1);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn edit_positions_are_recalled_across_buffers_and_buffer_close() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.row = 2;
        editor.doc.col = 2;
        // Recording happens at the start of each editing command.
        editor.remember();
        editor.doc.insert('A');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.doc.row = 0;
        editor.doc.col = 3;
        editor.remember();
        editor.doc.insert('B');
        assert_eq!(editor.jumps.changes().len(), 2);
        editor.jump_to_change(true);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        editor.jump_to_change(true);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("one.rs"));
        assert_eq!((editor.doc.row, editor.doc.col), (2, 2));

        // Close the now-clean second file, then revisit its edit position.
        editor.switch_buffer(1);
        editor.doc.dirty = false; // simulate file saved
        editor.close_buffer(false);
        assert_eq!(editor.slots.len(), 1);
        editor.jump_to_change(false);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        assert_eq!(editor.doc.row, 0);
        // Files reopened after buffer close are loaded from disk; unsaved
        // content is intentionally not persisted by the jumplist.
        assert_eq!(editor.doc.line().iter().collect::<String>(), "# two");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn marks_persist_across_buffer_closes_and_process_restart() {
        let root = temporary_buffer_project();
        let state = root.join("jump_history.json");
        {
            let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
            editor.jumps = JumpManager::from_path(state.clone());
            editor.doc.row = 2;
            editor.doc.col = 1;
            for key in ['m', 'A'] {
                press(&mut editor, KeyCode::Char(key));
            }
            for key in ['m', 'a'] {
                press(&mut editor, KeyCode::Char(key));
            }
            assert!(editor.message.contains("Mark a"));
            editor.open(root.join("two.md").to_string_lossy().into_owned());
            editor.switch_buffer(0);
            editor.close_buffer(false);
            for key in ['`', 'A'] {
                press(&mut editor, KeyCode::Char(key));
            }
            assert_eq!(editor.doc.row, 2);
            assert_eq!(editor.doc.col, 1);
        }

        let mut editor = Editor::new(Document::open(root.join("two.md")).unwrap());
        editor.jumps = JumpManager::from_path(state);
        for key in ['\'', 'A'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert!(editor.doc.path.as_ref().unwrap().ends_with("one.rs"));
        assert_eq!(editor.doc.row, 2);
        assert_eq!(editor.doc.col, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn closing_clean_buffer_preserves_its_last_jump_position() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.doc.col = 3;
        editor.close_buffer(false);
        assert_eq!(editor.slots.len(), 1);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("one.rs"));
        editor.jump_back();
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"));
        assert_eq!(editor.doc.col, 3);
        assert_eq!(editor.slots.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_jump_target_does_not_create_a_new_file() {
        // macOS temp paths often contain /var -> /private/var symlinks.
        // Use the canonical root so test fixtures match production
        // path_identity() when storing and comparing jump locations.
        let root = std::fs::canonicalize(temporary_buffer_project()).unwrap();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor
            .jumps
            .remember(Location {
                path: root.join("missing.rs"),
                row: 5,
                col: 0,
            })
            .unwrap();
        editor
            .jumps
            .remember(Location {
                path: root.join("one.rs"),
                row: 0,
                col: 0,
            })
            .unwrap();
        editor.jump_back();
        assert!(editor.message.contains("Jump target missing"));
        assert!(!root.join("missing.rs").exists());
        assert_eq!(editor.slots.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn vim_gg_and_g_semicolon_record_and_navigate_jumps_and_edits() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.row = 2;
        for key in ['g', 'g'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.row, 0);
        editor.jump_back();
        assert_eq!(editor.doc.row, 2);
        editor.doc.row = 1;
        editor.remember();
        editor.doc.row = 2;
        editor.remember();
        for key in ['g', ';'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.row, 2);
        for key in ['g', ';'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.row, 1);
        for key in ['g', ','] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.row, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unsaved_file_can_be_left_and_restored_with_independent_undo() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        press(&mut editor, KeyCode::Char('j'));
        press(&mut editor, KeyCode::Char('i'));
        press(&mut editor, KeyCode::Char('X'));
        press(&mut editor, KeyCode::Esc);
        editor.scroll_row = 1;
        editor.scroll_col = 3;
        assert!(editor.doc.dirty);
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        assert_eq!(editor.slots.len(), 2);
        assert_eq!(editor.active_slot, 1);
        assert_eq!(editor.dirty_buffers(), 1);
        assert!(!editor.doc.dirty);
        assert_eq!(editor.doc.row, 0);
        press(&mut editor, KeyCode::Char('i'));
        press(&mut editor, KeyCode::Char('Y'));
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.dirty_buffers(), 2);
        editor.open(root.join("one.rs").to_string_lossy().into_owned());
        assert_eq!(editor.slots.len(), 2, "reopening must reuse the buffer");
        assert_eq!(editor.active_slot, 0);
        assert_eq!(editor.scroll_row, 1);
        assert_eq!(editor.scroll_col, 3);
        assert_eq!(editor.doc.row, 1);
        assert!(editor.doc.lines[1]
            .iter()
            .collect::<String>()
            .starts_with('X'));
        editor.undo();
        assert_eq!(editor.doc.lines[1].iter().collect::<String>(), "second");
        editor.switch_buffer(1);
        assert!(editor.doc.lines[0]
            .iter()
            .collect::<String>()
            .starts_with('Y'));
        editor.undo();
        assert_eq!(editor.doc.lines[0].iter().collect::<String>(), "# two");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tree_opens_second_file_without_discarding_modified_first_buffer() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        let mut tree = TreePlugin::new(editor.lua.clone(), &root).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        // Sorted root: one.rs, two.md.
        editor.tree_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        editor.tree_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(editor
            .doc
            .path
            .as_ref()
            .is_some_and(|p| p.ends_with("two.md")));
        assert_eq!(editor.slots.len(), 2);
        assert!(editor.slots[0].as_ref().unwrap().doc.dirty);
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        editor.switch_buffer(0);
        assert_eq!(editor.doc.lines[0].first(), Some(&'!'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn alt_j_and_alt_k_cycle_buffers_without_leader() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        assert_eq!(editor.active_slot, 1);
        editor.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT), 12);
        assert_eq!(editor.active_slot, 0);
        assert!(
            editor.doc.dirty,
            "modified buffer must survive direct switching"
        );
        assert_eq!(editor.doc.line().first(), Some(&'!'));

        editor.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), 12);
        assert_eq!(editor.active_slot, 1);
        editor.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL), 12);
        assert_eq!(editor.active_slot, 0);
        editor.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL), 12);
        assert_eq!(editor.active_slot, 1);
        assert_eq!(editor.slots.len(), 2);
        assert_eq!(editor.dirty_buffers(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_p_opens_existing_fuzzy_buffer_picker_and_switches_to_last_file() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());

        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        editor.key(ctrl_p, 12);
        assert!(editor.buffer_picker.is_some());
        assert!(paint(&mut editor).contains("SWITCH BUFFER"));
        // Default selection prefers the previous file for rapid back-and-forth.
        assert_eq!(editor.buffer_picker.as_ref().unwrap().selected, 0);
        editor.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 12);
        assert_eq!(editor.active_slot, 0);
        assert!(editor.buffer_picker.is_none());

        editor.key(ctrl_p, 12);
        assert_eq!(editor.buffer_picker.as_ref().unwrap().selected, 1);
        editor.key(ctrl_p, 12);
        assert!(editor.buffer_picker.is_none(), "Ctrl+P toggles the dialog");

        editor.key(ctrl_p, 12);
        for c in "two".chars() {
            editor.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), 12);
        }
        assert_eq!(editor.buffer_picker_view().unwrap().total_matches, 1);
        editor.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 12);
        assert_eq!(editor.active_slot, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_p_works_with_tree_focused_and_does_not_close_it() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.toggle_tree();
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.focused));

        editor.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL), 12);
        assert!(editor.buffer_picker.is_some());
        assert!(paint(&mut editor).contains("SWITCH BUFFER"));
        editor.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 12);
        assert!(editor.buffer_picker.is_none());
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && tree.focused));
        editor.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT), 12);
        assert_eq!(editor.active_slot, 0);
        assert!(
            editor
                .tree
                .as_ref()
                .is_some_and(|tree| tree.visible && !tree.focused),
            "fast buffer switching should return focus to the editor"
        );
        // Opening the chooser must not re-read or discard files.
        assert_eq!(editor.slots.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn alt_keys_do_not_switch_buffers_while_inserting_text() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        press(&mut editor, KeyCode::Char('i'));
        editor.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), 12);
        editor.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL), 12);
        assert_eq!(editor.mode, Mode::Insert);
        assert_eq!(editor.active_slot, 1);
        press(&mut editor, KeyCode::Esc);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn doom_buffer_navigation_and_last_buffer_are_stable() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        assert_eq!(editor.active_slot, 1);
        for key in [' ', 'b', 'p'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.active_slot, 0);
        for key in [' ', 'b', 'l'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.active_slot, 1);
        for key in [' ', 'b', 'n'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.active_slot, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn buffer_picker_lists_dirty_buffers_and_switches_without_reloading() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        for key in [' ', 'b', 'b'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert!(editor.buffer_picker.is_some());
        let popup = paint(&mut editor);
        assert!(popup.contains("SWITCH BUFFER"));
        assert!(popup.contains("one.rs"));
        assert!(popup.contains("two.md"));
        assert!(popup.contains('●'), "dirty buffers are marked");
        for key in ['o', 'n', 'e'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.buffer_picker_view().unwrap().total_matches, 1);
        press(&mut editor, KeyCode::Enter);
        assert!(editor.buffer_picker.is_none());
        assert_eq!(editor.active_slot, 0);
        assert!(editor.doc.dirty);
        assert_eq!(editor.doc.lines[0].first(), Some(&'!'));
        let output = paint(&mut editor);
        assert!(!output.contains("SWITCH BUFFER"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn quit_and_close_protect_all_dirty_buffers() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        editor.quit();
        assert!(
            matches!(editor.prompt, Some(Prompt::Quit)),
            "quit must notice an unsaved inactive buffer"
        );
        editor.prompt_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(editor.running);
        editor.close_buffer(false);
        assert_eq!(editor.slots.len(), 1);
        assert!(
            editor.doc.dirty,
            "closing clean buffer must restore dirty buffer"
        );
        editor.close_buffer(false);
        assert_eq!(editor.slots.len(), 1, "cannot close an unsaved buffer");
        editor.close_buffer(true);
        assert!(!editor.doc.dirty);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_open_and_duplicate_save_do_not_damage_other_buffers() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        std::fs::write(root.join("broken.bin"), [0xff, 0xfe]).unwrap();
        editor.open(root.join("broken.bin").to_string_lossy().into_owned());
        assert_eq!(editor.active_slot, 1);
        assert_eq!(editor.slots.len(), 2);
        editor.set_path_and_save(root.join("one.rs"));
        assert!(
            editor.doc.path.as_ref().unwrap().ends_with("two.md"),
            "save-as cannot overwrite a file already open in another buffer"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_vertical_navigation_never_shows_cursor_in_line_numbers() {
        let root = temporary_buffer_project();
        let text = (0..120)
            .map(|row| format!("line {row:03}: text wide enough to scroll horizontally\n"))
            .collect::<String>();
        for (cols, rows, sidebar) in [(80, 18, false), (100, 24, true), (18, 8, false)] {
            let mut editor = Editor::new(Document::from_text(&text));
            editor.doc.col = 20;
            if sidebar {
                let mut tree = TreePlugin::new(editor.lua.clone(), &root).unwrap();
                tree.toggle().unwrap();
                tree.leave();
                editor.tree = Some(tree);
            }
            let mut parser = vt100::Parser::new(
                std::num::NonZeroU16::new(rows).unwrap(),
                std::num::NonZeroU16::new(cols).unwrap(),
                0,
            );
            let mut initial = Vec::new();
            editor.draw_at(&mut initial, cols, rows).unwrap();
            parser.process(&initial);
            assert!(!parser.screen().hide_cursor());

            for direction in ['j', 'k'] {
                for step in 0..90 {
                    let mut key = KeyEvent::new(KeyCode::Char(direction), KeyModifiers::NONE);
                    key.kind = KeyEventKind::Repeat;
                    editor.key(key, rows.saturating_sub(2) as usize);
                    let mut frame = Vec::new();
                    editor.draw_at(&mut frame, cols, rows).unwrap();
                    let sidebar_width = tree_sidebar_width(cols as usize, sidebar);
                    let gutter =
                        gutter_width(cols as usize - sidebar_width, editor.doc.lines.len());
                    let text_left = sidebar_width + gutter;
                    // A terminal may render between any two bytes/writes.
                    // Even an incomplete frame must never expose the paint
                    // cursor in the relative line numbers or the sidebar.
                    for byte in &frame {
                        parser.process(std::slice::from_ref(byte));
                        if !parser.screen().hide_cursor() {
                            assert!(parser.screen().cursor_position().1 as usize >= text_left,
                                "visible cursor in gutter during {direction} repeat {step} ({cols}x{rows})");
                        }
                    }
                    assert!(
                        !parser.screen().hide_cursor(),
                        "editing cursor must be restored"
                    );
                    assert_eq!(
                        parser.screen().cursor_position(),
                        (
                            (editor.doc.row - editor.scroll_row) as u16,
                            (text_left + visual_column(&editor.doc.line()[..editor.doc.col])
                                - editor.scroll_col) as u16,
                        )
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn moving_up_and_down_never_repaints_the_file() {
        let mut editor = Editor::new(Document::from_text(
            "Unique first line\nUnique second line\nUnique third line",
        ));
        let first = paint(&mut editor);
        assert!(first.contains("Unique first line"));
        // Only old and new active rows repaint. Other text stays untouched.
        press(&mut editor, KeyCode::Char('j'));
        let next = paint(&mut editor);
        assert!(next.contains("Unique first line"));
        assert!(next.contains("Unique second line"));
        assert!(!next.contains("Unique third line"));
        assert!(!next.contains("\x1b[2J"));
        assert!(!next.contains("\x1b[0J"));
        assert!(next.contains("Ln 2"));
        press(&mut editor, KeyCode::Char('k'));
        let previous = paint(&mut editor);
        assert!(!previous.contains("Unique third line"));
        assert!(previous.contains("Ln 1"));
    }

    #[test]
    fn inserting_changes_only_the_edited_row() {
        let mut editor = Editor::new(Document::from_text("first row\nsecond row\nthird row"));
        // With syntax disabled, changing one line must not touch other lines.
        // With syntax enabled, following lines may need a reparse because
        // multi-line string/comment contexts can change.
        editor.syntax.set_enabled(false);
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('j'));
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('i'));
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('X'));
        let output = paint(&mut editor);
        assert!(output.contains("Xsecond row"));
        assert!(!output.contains("first row"));
        assert!(!output.contains("third row"));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn scrolling_and_resize_refresh_visible_content() {
        let mut editor = Editor::new(Document::from_text(
            &(0..30)
                .map(|i| format!("row number {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        paint(&mut editor);
        for _ in 0..20 {
            press(&mut editor, KeyCode::Char('j'));
        }
        let scrolled = paint(&mut editor);
        assert!(scrolled.contains("row number"));
        assert!(!scrolled.contains("\x1b[2J"));
        let mut resized = Vec::new();
        editor.draw_at(&mut resized, 60, 10).unwrap();
        assert!(String::from_utf8(resized).unwrap().contains("\x1b[2J"));
    }

    #[test]
    fn turning_highlighting_off_forces_repaint() {
        let mut editor = Editor::new(Document::from_text("# A heading"));
        editor.doc.path = Some(PathBuf::from("README.md"));
        paint(&mut editor);
        editor.command("syntax off");
        let repainted = paint(&mut editor);
        assert!(repainted.contains("# A heading"));
        assert!(!repainted.contains("\x1b[2J"));
    }

    #[test]
    fn tree_layout_preserves_text_width() {
        assert_eq!(tree_sidebar_width(80, false), 0);
        assert_eq!(tree_sidebar_width(80, true), 26);
        assert_eq!(tree_sidebar_width(40, true), 0);
        assert!(tree_sidebar_width(120, true) + gutter_width(86, 100) < 120);
    }

    #[test]
    fn relative_numbers_keep_absolute_cursor_position() {
        assert_eq!(gutter_width(80, 12_000), 7);
        assert_eq!(gutter_number(42, 42, 12_000, 7), "   43  ");
        assert_eq!(gutter_number(38, 42, 12_000, 7), "    4  ");
        assert_eq!(gutter_number(52, 42, 12_000, 7), "   10  ");
        assert_eq!(gutter_number(12_000, 42, 12_000, 7), "       ");
    }

    #[test]
    fn relative_numbers_and_highlight_are_visible_by_default() {
        let mut editor = Editor::new(Document::from_text("alpha\nbeta\ngamma"));
        let first = paint(&mut editor);
        assert!(first.contains("\x1b[48;2;49;50;57m"));
        assert!(first.contains(" 1  "));
        press(&mut editor, KeyCode::Char('j'));
        let next = paint(&mut editor);
        assert!(next.contains("beta"));
        assert!(next.contains("alpha"));
        assert!(!next.contains("gamma"));
        assert!(!next.contains("\x1b[2J"));
        assert!(next.contains(" 2  "));
    }

    fn scrolling_editor() -> Editor {
        let text = (0..100)
            .map(|row| format!("  line {row:03} with enough columns"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut doc = Document::from_text(&text);
        doc.path = Some(PathBuf::from("navigation-fixture.txt"));
        doc.row = 23;
        doc.col = 6;
        let mut editor = Editor::new(doc);
        editor.scroll_row = 20;
        editor
    }

    #[test]
    fn numeric_command_jumps_through_prompt_and_preserves_jump_origin() {
        let mut editor = scrolling_editor();
        paint(&mut editor);
        press(&mut editor, KeyCode::Char(':'));
        for ch in "52".chars() {
            press(&mut editor, KeyCode::Char(ch));
        }
        press(&mut editor, KeyCode::Enter);
        assert!(editor.prompt.is_none());
        assert_eq!((editor.doc.row, editor.doc.col), (51, 2));
        assert!(editor.doc.row >= editor.scroll_row && editor.doc.row < editor.scroll_row + 16);
        editor.jump_back();
        assert_eq!((editor.doc.row, editor.doc.col), (23, 6));
        editor.jump_forward();
        assert_eq!((editor.doc.row, editor.doc.col), (51, 2));
        assert!(!editor.doc.dirty);
        assert_eq!(editor.doc.revision, 0);
        assert!(editor.undo.is_empty() && editor.redo.is_empty());
    }

    #[test]
    fn numeric_commands_clamp_bounds_and_work_without_a_filename() {
        let mut editor = Editor::new(Document::from_text("  first\n\tsecond\n  last"));
        editor.command("  0002  ");
        assert_eq!((editor.doc.row, editor.doc.col), (1, 1));
        editor.command("0");
        assert_eq!((editor.doc.row, editor.doc.col), (0, 2));
        editor.command(&"9".repeat(400));
        assert_eq!((editor.doc.row, editor.doc.col), (2, 2));
        for command in ["12x", "-1", "", "12 34"] {
            editor.command(command);
            assert_eq!((editor.doc.row, editor.doc.col), (2, 2));
        }
        assert!(!editor.doc.dirty);
        let mut empty = Editor::new(Document::empty());
        empty.command("9999999999999999999999999999999999");
        assert_eq!((empty.doc.row, empty.doc.col), (0, 0));
    }

    #[test]
    fn fast_scroll_keys_move_viewport_and_cursor_together() {
        for (code, modifiers, down, amount) in [
            (KeyCode::Char('d'), KeyModifiers::CONTROL, true, 5),
            (KeyCode::Char('u'), KeyModifiers::CONTROL, false, 5),
            (KeyCode::Char('f'), KeyModifiers::CONTROL, true, 8),
            (KeyCode::Char('b'), KeyModifiers::CONTROL, false, 8),
            (KeyCode::PageDown, KeyModifiers::NONE, true, 8),
            (KeyCode::PageUp, KeyModifiers::NONE, false, 8),
            (KeyCode::Char('v'), KeyModifiers::CONTROL, true, 8),
            (KeyCode::Char('v'), KeyModifiers::ALT, false, 8),
        ] {
            let mut editor = scrolling_editor();
            editor.key(KeyEvent::new(code, modifiers), 10);
            let expected_row = if down { 23 + amount } else { 23 - amount };
            let expected_top = if down { 20 + amount } else { 20 - amount };
            assert_eq!(
                (editor.doc.row, editor.scroll_row, editor.doc.col),
                (expected_row, expected_top, 6)
            );
            editor.jump_back();
            assert_eq!((editor.doc.row, editor.doc.col), (23, 6));
            assert_eq!(editor.doc.revision, 0);
            assert!(!editor.doc.dirty);
            assert!(editor.undo.is_empty());
        }
    }

    #[test]
    fn scroll_counts_are_consumed_and_full_pages_saturate_safely() {
        let mut editor = scrolling_editor();
        press(&mut editor, KeyCode::Char('3'));
        editor.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 10);
        assert_eq!((editor.doc.row, editor.scroll_row), (47, 44));
        assert!(editor.count.is_none());
        press(&mut editor, KeyCode::Char('j'));
        assert_eq!(editor.doc.row, 48);
        press(&mut editor, KeyCode::Char('3'));
        editor.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 10);
        assert_eq!(editor.doc.row, 51);
        editor.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 10);
        assert_eq!(editor.doc.row, 56, "half-page count applies to one command");
        for _ in 0..40 {
            press(&mut editor, KeyCode::Char('9'));
        }
        editor.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 10);
        assert_eq!((editor.doc.row, editor.scroll_row), (99, 90));
        assert!(editor.count.is_none());
        let history = editor.jumps.entries().len();
        editor.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 10);
        assert_eq!(
            editor.jumps.entries().len(),
            history,
            "no jump for a no-op at EOF"
        );
    }

    #[test]
    fn paging_clamps_column_only_at_destination_and_keeps_selection() {
        let mut editor = scrolling_editor();
        for line in &mut editor.doc.lines[24..31] {
            line.clear();
        }
        editor.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 10);
        assert_eq!((editor.doc.row, editor.doc.col), (31, 6));
        editor.doc.lines[39] = "hi".chars().collect();
        editor.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 10);
        assert_eq!((editor.doc.row, editor.doc.col), (39, 2));
        editor.enter_visual();
        let anchor = editor.visual_anchor;
        editor.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 10);
        assert_eq!(editor.mode, Mode::Visual);
        assert_eq!(editor.visual_anchor, anchor);
        let selected = editor.selection.unwrap();
        assert_eq!(selected.start.row, 39);
        assert_eq!(selected.end.row, 44);
        assert!(!editor.doc.dirty);
    }

    #[test]
    fn paging_handles_tiny_viewports_and_single_line_documents() {
        for height in [0, 1, 2, 10, 200] {
            let mut editor = scrolling_editor();
            editor.count = Some(usize::MAX);
            editor.key(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                height,
            );
            assert_eq!(editor.doc.row, 99);
            assert!(editor.scroll_row <= editor.doc.lines.len().saturating_sub(height.max(1)));
            editor.count = Some(usize::MAX);
            editor.key(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
                height,
            );
            assert_eq!((editor.doc.row, editor.scroll_row), (0, 0));
            let history = editor.jumps.entries().len();
            editor.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), height);
            assert_eq!(editor.jumps.entries().len(), history);
            let mut empty = Editor::new(Document::empty());
            empty.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                height,
            );
            empty.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), height);
            assert_eq!((empty.doc.row, empty.doc.col, empty.scroll_row), (0, 0, 0));
        }
    }

    #[test]
    fn scrolling_respects_prompt_picker_tree_and_insert_mode() {
        let mut editor = scrolling_editor();
        editor.prompt = Some(Prompt::Command("52".into()));
        editor.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 10);
        assert!(matches!(&editor.prompt, Some(Prompt::Command(value)) if value == "52"));
        assert_eq!(editor.doc.row, 23);
        editor.prompt = None;
        editor.open_buffer_picker();
        editor.pasted("query");
        editor.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 10);
        assert!(editor.buffer_picker.as_ref().unwrap().query.is_empty());
        assert_eq!(editor.doc.row, 23);
        editor.buffer_picker = None;
        let root = temporary_buffer_project();
        let mut tree = TreePlugin::new(editor.lua.clone(), &root).unwrap();
        tree.toggle().unwrap();
        tree.on_key("/").unwrap();
        tree.paste_query("query").unwrap();
        editor.tree = Some(tree);
        editor.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 10);
        assert!(editor
            .tree
            .as_ref()
            .unwrap()
            .view(10)
            .unwrap()
            .finder
            .unwrap()
            .query
            .is_empty());
        assert_eq!(editor.doc.row, 23);
        editor.tree = None;
        editor.begin_insert();
        editor.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 10);
        assert_eq!(
            editor.doc.row, 23,
            "insert controls must not become normal-mode motions"
        );
        editor.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 10);
        assert_eq!(
            (editor.mode, editor.doc.row, editor.scroll_row),
            (Mode::Insert, 31, 28)
        );
        assert!(!editor.doc.dirty);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn counted_vertical_jumps_work_on_large_files() {
        let mut editor = Editor::new(Document::from_text(
            &(0..500)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        for c in "125j".chars() {
            press(&mut editor, KeyCode::Char(c));
        }
        assert_eq!(editor.doc.row, 125);
        for c in "40k".chars() {
            press(&mut editor, KeyCode::Char(c));
        }
        assert_eq!(editor.doc.row, 85);
        for c in "420G".chars() {
            press(&mut editor, KeyCode::Char(c));
        }
        assert_eq!(editor.doc.row, 419);
        for c in "17gg".chars() {
            press(&mut editor, KeyCode::Char(c));
        }
        assert_eq!(editor.doc.row, 16);
        press(&mut editor, KeyCode::Char('G'));
        assert_eq!(editor.doc.row, 499);
        for c in "gg".chars() {
            press(&mut editor, KeyCode::Char(c));
        }
        assert_eq!(editor.doc.row, 0);
    }

    #[test]
    fn narrow_terminal_keeps_space_for_text() {
        assert_eq!(gutter_width(4, 2_000_000), 0);
        assert!(gutter_width(9, 2_000_000) < 9);
        let mut editor = Editor::new(Document::from_text("hello\nworld"));
        let mut output = Vec::new();
        editor.draw_at(&mut output, 4, 5).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("hell"));
    }

    #[test]
    fn horizontal_movement_does_not_repaint_other_rows() {
        let mut editor = Editor::new(Document::from_text("abcdef\nsecond\nthird"));
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('l'));
        let output = paint(&mut editor);
        assert!(!output.contains("abcdef"));
        assert!(!output.contains("second"));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn ctrl_n_toggles_tree_regardless_of_focus() {
        let mut editor = Editor::new(Document::from_text("editor text"));
        let press_ctrl_n = |editor: &mut Editor| {
            editor.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL), 10);
        };

        // From the editor: open the tree and focus it.
        press_ctrl_n(&mut editor);
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && tree.focused));

        // From the focused tree: Ctrl+N must close it (regression).
        press_ctrl_n(&mut editor);
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| !tree.visible && !tree.focused));

        // Re-open the tree and enter the fuzzy finder.
        press_ctrl_n(&mut editor);
        editor.tree_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(editor
            .tree
            .as_ref()
            .unwrap()
            .view(10)
            .unwrap()
            .search_cursor
            .is_some());

        // Ctrl+N remains the global toggle even while searching.
        press_ctrl_n(&mut editor);
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| !tree.visible && !tree.focused));

        // Reopening returns to the tree, not to the stale search prompt.
        press_ctrl_n(&mut editor);
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && tree.focused));
        assert!(editor
            .tree
            .as_ref()
            .unwrap()
            .view(10)
            .unwrap()
            .search_cursor
            .is_none());
        assert_eq!(editor.doc.line().iter().collect::<String>(), "editor text");
    }

    #[test]
    fn overlay_is_repainted_after_sidebar_redraw_during_fuzzy_search() {
        let mut editor = Editor::new(Document::from_text(
            &vec!["SOME-EDITOR-CONTENT"; 60].join("\n"),
        ));
        let mut tree =
            TreePlugin::new(editor.lua.clone(), &std::env::current_dir().unwrap()).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);

        // Use a 120-column terminal where the centered dialog intersects
        // both the sidebar and the editor (the reported macOS layout).
        let draw = |editor: &mut Editor| {
            let mut output = Vec::new();
            editor.draw_at(&mut output, 120, 40).unwrap();
            String::from_utf8(output).unwrap()
        };
        draw(&mut editor);
        press(&mut editor, KeyCode::Char('/'));
        assert!(draw(&mut editor).contains("FIND FILES"));

        let popup = finder_rect_for(120, 38).unwrap();
        let overlap_y = popup.y as usize;
        assert!((popup.x as usize) < tree_sidebar_width(120, true));

        // Simulate the tree re-rendering a row on the popup's title.
        // Previously this overwrote the left edge of the title, while
        // the popup cache incorrectly skipped repainting that row.
        editor.screen.sidebar_lines[overlap_y] = None;
        press(&mut editor, KeyCode::Char('p'));
        let frame = draw(&mut editor);
        assert!(
            frame.contains("FIND FILES"),
            "unchanged popup header must be repainted after tree overlap"
        );
        assert!(
            frame.contains("  > p"),
            "search input must remain intact when the tree repaints"
        );

        // Simulate an underlying editor repaint on the popup header too.
        editor.screen.lines[overlap_y] = None;
        let frame = draw(&mut editor);
        assert!(
            frame.contains("FIND FILES"),
            "popup border must survive redraws from the editor pane"
        );
        assert!(!frame.contains("\x1b[2J"), "no fullscreen clear needed");
    }

    #[test]
    fn focused_tree_slash_opens_visible_fuzzy_dialog_and_opens_file() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-finder-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(temp.join("src")).unwrap();
        std::fs::write(temp.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(temp.join("README.md"), "# Example").unwrap();

        let mut editor = Editor::new(Document::empty());
        let mut tree = TreePlugin::new(editor.lua.clone(), &temp).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        let initial = paint(&mut editor);
        assert!(initial.contains("FILES"));

        press(&mut editor, KeyCode::Char('/'));
        let opened = paint(&mut editor);
        assert!(opened.contains("FIND FILES"), "finder must visibly open");
        assert!(
            opened.contains("README.md"),
            "unfiltered project files must be listed"
        );
        assert!(
            !opened.contains("\x1b[2J"),
            "search must not erase entire screen"
        );

        // Live fuzzy search should narrow to the nested file.
        for key in "mainrs".chars() {
            press(&mut editor, KeyCode::Char(key));
        }
        let matches = paint(&mut editor);
        assert!(matches.contains("src/main.rs"));
        assert!(matches.contains("mainrs"));
        press(&mut editor, KeyCode::Enter);
        assert_eq!(
            editor.doc.path.as_deref(),
            Some(temp.join("src/main.rs").as_path())
        );
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        let restored = paint(&mut editor);
        assert!(!restored.contains("FIND FILES"));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn fuzzy_search_esc_closes_only_popup_not_the_tree() {
        let mut editor = Editor::new(Document::from_text(
            &vec!["EDITOR_CONTENT_STAYS_VISIBLE"; 30].join("\n"),
        ));
        let mut tree =
            TreePlugin::new(editor.lua.clone(), &std::env::current_dir().unwrap()).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('f'));
        assert!(paint(&mut editor).contains("FIND FILES"));
        press(&mut editor, KeyCode::Esc);
        let restored = paint(&mut editor);
        assert!(!restored.contains("FIND FILES"));
        assert!(restored.contains("EDITOR_CONTENT_STAYS_VISIBLE"));
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && tree.focused));
        assert!(!restored.contains("\x1b[2J"));
    }

    #[test]
    fn opening_tree_keeps_editor_buffer_and_renders_sidebar() {
        let mut editor = Editor::new(Document::from_text("UNCHANGED-EDITOR-CONTENT"));
        editor.toggle_tree();
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && tree.focused));
        let mut output = Vec::new();
        editor.draw_at(&mut output, 90, 18).unwrap();
        let first = String::from_utf8(output).unwrap();
        assert!(first.contains("FILES"));
        assert!(first.contains("UNCHANGED-EDITOR-CONTENT"));
        let mut output = Vec::new();
        editor.draw_at(&mut output, 90, 18).unwrap();
        // No unnecessary repaint of the sidebar or editor content.
        let second = String::from_utf8(output).unwrap();
        assert!(!second.contains("UNCHANGED-EDITOR-CONTENT"));
        editor.tree_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(editor
            .tree
            .as_ref()
            .is_some_and(|tree| tree.visible && !tree.focused));
        editor.toggle_tree();
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible));
        assert_eq!(
            editor.doc.line().iter().collect::<String>(),
            "UNCHANGED-EDITOR-CONTENT"
        );
    }

    #[test]
    fn visual_function_selection_and_yank() {
        let mut doc = Document::from_text(
            "pub fn from_text(text: &str) -> Self {\n  let v = { 42 };\n  Self { v }\n}\n",
        );
        doc.path = Some(PathBuf::from("src/document.rs"));
        doc.row = 1;
        doc.col = 5;
        let mut editor = Editor::new(doc);
        for key in ['v', 'a', 'f'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.mode, Mode::Visual);
        let selected = editor.selection.unwrap();
        assert_eq!(selected.start, Position { row: 0, col: 0 });
        assert_eq!(selected.end, Position { row: 3, col: 1 });
        let output = paint(&mut editor);
        assert!(output.contains("\x1b[48;2;83;69;111m"));
        press(&mut editor, KeyCode::Char('y'));
        assert_eq!(editor.mode, Mode::Normal);
        assert!(editor.selection.is_none());
        match editor.register.as_ref().unwrap() {
            Register::Text(text) => assert!(text.starts_with("pub fn from_text")),
            _ => panic!("expected selected function in register"),
        }
        assert_eq!(editor.doc.lines.len(), 5);
    }

    #[test]
    fn doom_function_shortcut_delete_is_undoable() {
        let mut doc = Document::from_text("fn f() {\n  println!(\"hi\");\n}\nfn g() {}\n");
        doc.path = Some(PathBuf::from("src/lib.rs"));
        doc.row = 1;
        let mut editor = Editor::new(doc);
        for key in [' ', 'v', 'f'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.mode, Mode::Visual);
        press(&mut editor, KeyCode::Char('d'));
        assert_eq!(editor.mode, Mode::Normal);
        assert!(editor.doc.lines[0].is_empty());
        assert!(editor.doc.lines[1]
            .iter()
            .collect::<String>()
            .contains("fn g"));
        press(&mut editor, KeyCode::Char('u'));
        assert!(editor.doc.lines[0]
            .iter()
            .collect::<String>()
            .starts_with("fn f"));
    }

    #[test]
    fn visual_manual_motions_and_escape() {
        let mut editor = Editor::new(Document::from_text("abcd\nsecond\nthird"));
        press(&mut editor, KeyCode::Char('v'));
        press(&mut editor, KeyCode::Char('l'));
        let range = editor.selection.unwrap();
        assert_eq!(range.columns(0, 4), Some((0, 2)));
        paint(&mut editor);
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.mode, Mode::Normal);
        let output = paint(&mut editor);
        assert!(output.contains("abcd"));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn modal_editing() {
        let mut editor = Editor::new(Document::from_text("abc"));
        press(&mut editor, KeyCode::Char('i'));
        assert_eq!(editor.mode, Mode::Insert);
        press(&mut editor, KeyCode::Char('x'));
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.mode, Mode::Normal);
        assert_eq!(editor.doc.line().iter().collect::<String>(), "xabc");
        press(&mut editor, KeyCode::Char('u'));
        assert_eq!(editor.doc.line().iter().collect::<String>(), "abc");
        editor.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL), 10);
        assert_eq!(editor.doc.line().iter().collect::<String>(), "xabc");
    }

    #[test]
    fn leader_open_and_save() {
        let mut editor = Editor::new(Document::empty());
        for key in [' ', 'f', 'f'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert!(matches!(editor.prompt, Some(Prompt::Open(_))));
        editor.prompt = None;
        for key in [' ', 'f', 's'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert!(matches!(editor.prompt, Some(Prompt::SaveAs(_))));
    }

    #[test]
    fn yank_delete_paste() {
        let mut editor = Editor::new(Document::from_text("one\ntwo"));
        for key in ['y', 'y', 'j', 'p'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.lines.len(), 3);
        assert_eq!(editor.doc.line().iter().collect::<String>(), "one");
        for key in ['d', 'd'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.doc.lines.len(), 2);
    }

    #[test]
    fn visual_yank_and_external_clipboard_paste() {
        let mut editor = Editor::new(Document::from_text("ab"));
        press(&mut editor, KeyCode::Char('v'));
        press(&mut editor, KeyCode::Char('y'));
        assert_eq!(editor.clipboard.read().unwrap(), "a");
        // Another application replaces the desktop clipboard.
        editor.clipboard.write("β\r\n世界").unwrap();
        press(&mut editor, KeyCode::Char('p'));
        assert_eq!(editor.doc.lsp_text(), "aβ\n世界b");
        press(&mut editor, KeyCode::Char('u'));
        assert_eq!(editor.doc.lsp_text(), "ab");
    }

    #[test]
    fn line_yank_preserves_linewise_paste_and_exports_newline() {
        let mut editor = Editor::new(Document::from_text("one\ntwo"));
        press(&mut editor, KeyCode::Char('y'));
        press(&mut editor, KeyCode::Char('y'));
        assert_eq!(editor.clipboard.read().unwrap(), "one\n");
        press(&mut editor, KeyCode::Char('p'));
        assert_eq!(editor.doc.lsp_text(), "one\none\ntwo");
        press(&mut editor, KeyCode::Char('d'));
        press(&mut editor, KeyCode::Char('d'));
        assert_eq!(editor.clipboard.read().unwrap(), "one\n");
    }

    #[test]
    fn delete_char_and_visual_delete_update_desktop_clipboard() {
        let mut editor = Editor::new(Document::from_text("abc"));
        press(&mut editor, KeyCode::Char('x'));
        assert_eq!(editor.clipboard.read().unwrap(), "a");
        press(&mut editor, KeyCode::Char('v'));
        press(&mut editor, KeyCode::Char('l'));
        press(&mut editor, KeyCode::Char('d'));
        assert_eq!(editor.clipboard.read().unwrap(), "bc");
        assert_eq!(editor.doc.lsp_text(), "");
    }

    #[test]
    fn bracketed_paste_is_multiline_unicode_and_single_undo() {
        let mut editor = Editor::new(Document::from_text("tail"));
        press(&mut editor, KeyCode::Char('i'));
        editor.pasted("hé\r\n世界");
        assert_eq!(editor.doc.lsp_text(), "hé\n世界tail");
        press(&mut editor, KeyCode::Esc);
        press(&mut editor, KeyCode::Char('u'));
        assert_eq!(editor.doc.lsp_text(), "tail");
    }

    #[test]
    fn bracketed_paste_in_normal_mode_inserts_literal_text() {
        let mut editor = Editor::new(Document::from_text("two"));
        editor.pasted("hello");
        assert_eq!(editor.doc.lsp_text(), "thellowo");
    }

    #[test]
    fn empty_clipboard_does_not_replay_stale_register() {
        let mut editor = Editor::new(Document::from_text("one\ntwo"));
        press(&mut editor, KeyCode::Char('y'));
        press(&mut editor, KeyCode::Char('y'));
        editor.clipboard.write("").unwrap();
        press(&mut editor, KeyCode::Char('p'));
        assert_eq!(editor.doc.lsp_text(), "one\ntwo");
        assert!(!editor.doc.dirty);
        assert!(editor.undo.is_empty());
    }

    #[test]
    fn bracketed_paste_has_undo_boundaries_before_and_after() {
        let mut editor = Editor::new(Document::empty());
        press(&mut editor, KeyCode::Char('i'));
        press(&mut editor, KeyCode::Char('a'));
        editor.pasted("β\r\n世界");
        press(&mut editor, KeyCode::Char('z'));
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.doc.lsp_text(), "aβ\n世界z");
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "aβ\n世界");
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "a");
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "");
        editor.redo();
        editor.redo();
        editor.redo();
        assert_eq!(editor.doc.lsp_text(), "aβ\n世界z");
    }

    #[test]
    fn pasted_prompt_text_never_executes_commands_or_confirms_quit() {
        let mut editor = Editor::new(Document::from_text("unchanged"));
        editor.prompt = Some(Prompt::Command(String::new()));
        editor.pasted("q!\r\nignored");
        assert!(matches!(&editor.prompt, Some(Prompt::Command(value)) if value == "q!"));
        assert!(editor.running);
        editor.prompt = Some(Prompt::Open(String::new()));
        editor.pasted("/tmp/世界.rs\nignored");
        assert!(matches!(&editor.prompt, Some(Prompt::Open(value)) if value == "/tmp/世界.rs"));
        editor.prompt = Some(Prompt::SaveAs(String::new()));
        editor.pasted("/tmp/notes.txt");
        assert!(matches!(&editor.prompt, Some(Prompt::SaveAs(value)) if value == "/tmp/notes.txt"));
        editor.prompt = Some(Prompt::Quit);
        editor.pasted("yes\n");
        assert!(editor.running);
        assert_eq!(editor.doc.lsp_text(), "unchanged");
    }

    #[test]
    fn pasted_search_query_is_literal_bounded_and_selects_first_match() {
        let mut editor = Editor::new(Document::empty());
        editor.buffer_picker = Some(BufferPicker {
            query: "a".into(),
            selected: 4,
        });
        editor.pasted("β\r\nignored");
        let picker = editor.buffer_picker.as_ref().unwrap();
        assert_eq!(picker.query, "aβ");
        assert_eq!(picker.selected, 0);
        editor.buffer_picker = None;
        editor.mason_picker = Some(BufferPicker::default());
        editor.pasted(&"世".repeat(100));
        let picker = editor.mason_picker.as_ref().unwrap();
        assert_eq!(picker.query.len(), 126);
        assert!(!editor.doc.dirty);
    }

    #[test]
    fn paste_cancels_pending_normal_command() {
        let mut editor = Editor::new(Document::from_text("one"));
        press(&mut editor, KeyCode::Char('d'));
        editor.pasted("x");
        press(&mut editor, KeyCode::Char('d'));
        assert_eq!(editor.doc.lsp_text(), "oxne");
        assert!(editor.lua.key_pending());
    }

    #[test]
    fn jump_navigation_skips_missing_files_in_both_directions() {
        let root = std::fs::canonicalize(temporary_buffer_project()).unwrap();
        let mut editor = Editor::new(Document::open(root.join("two.rs")).unwrap());
        for name in ["one.rs", "missing.rs"] {
            editor
                .jumps
                .remember(Location {
                    path: root.join(name),
                    row: 0,
                    col: 0,
                })
                .unwrap();
        }
        editor.jump_back();
        assert_eq!(
            editor.doc.path.as_deref(),
            Some(root.join("one.rs").as_path())
        );
        editor.jump_forward();
        assert_eq!(
            editor.doc.path.as_deref(),
            Some(root.join("two.rs").as_path())
        );
        assert!(!root.join("missing.rs").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_documents_reuse_lsp_sync_state_until_revision_changes() {
        let mut doc = Document::from_text("# title");
        doc.path = Some(PathBuf::from("README.md"));
        let mut editor = Editor::new(doc);
        editor.sync_lsp_documents();
        let versions = editor.lsp_document_versions.clone().unwrap();
        let present = editor.lsp_present.clone();
        editor.doc.right();
        editor.sync_lsp_documents();
        assert_eq!(editor.lsp_document_versions.as_ref(), Some(&versions));
        assert_eq!(editor.lsp_present, present);
        editor.doc.insert('x');
        editor.sync_lsp_documents();
        assert_ne!(editor.lsp_document_versions.as_ref(), Some(&versions));
    }

    #[test]
    fn tab_expansion() {
        assert_eq!(visual_column(&"a\tb".chars().collect::<Vec<_>>()), 5);
        assert_eq!(
            render_line(&"a\tb".chars().collect::<Vec<_>>(), 0, 8),
            "a   b"
        );
    }

    #[test]
    fn colored_lines_preserve_tabs_and_wide_characters() {
        let line: Vec<char> = "a\t界x".chars().collect();
        let spans = vec![
            Span {
                start: 0,
                end: 2,
                foreground: Color::Blue,
            },
            Span {
                start: 2,
                end: 4,
                foreground: Color::Red,
            },
        ];
        let runs = render_color_line(&line, &spans, 0, 9);
        assert_eq!(
            runs,
            vec![(Color::Blue, "a   ".into()), (Color::Red, "界x".into()),]
        );
    }

    fn completion_editor(text: &str) -> Editor {
        let mut doc = Document::from_text(text);
        doc.path = Some(PathBuf::from("completion-fixture.rs"));
        doc.col = doc.line().len();
        let mut editor = Editor::new(doc);
        editor.begin_insert();
        editor
    }

    fn provide_completions(editor: &mut Editor, value: serde_json::Value) -> CompletionContext {
        let context = CompletionContext {
            origin: lsp::absolute(editor.doc.path.as_ref().unwrap()),
            revision: editor.doc.revision,
            cursor: (editor.doc.row, editor.doc.col),
        };
        editor.completion_pending = Some(PendingCompletion {
            context: context.clone(),
            since: Instant::now(),
            manual: true,
        });
        editor.show_completions(
            context.clone(),
            lsp::Encoding::Utf16,
            completion::parse(&value),
        );
        context
    }

    #[test]
    fn completion_acceptance_and_following_typing_have_separate_undo_steps() {
        let mut editor = completion_editor("");
        for ch in "pri".chars() {
            press(&mut editor, KeyCode::Char(ch));
        }
        provide_completions(&mut editor, serde_json::json!([{"label":"println!"}]));
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.doc.lsp_text(), "println!");
        assert_eq!(editor.mode, Mode::Insert);
        assert!(editor.completion_menu.is_none() && editor.completion_due.is_none());
        press(&mut editor, KeyCode::Char('x'));
        press(&mut editor, KeyCode::Esc);
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "println!");
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "pri");
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), "");
    }

    #[test]
    fn completion_selection_uses_arrows_and_tab_while_enter_keeps_newline() {
        let mut editor = completion_editor("pri");
        let choices = serde_json::json!([{"label":"print"},{"label":"println"}]);
        provide_completions(&mut editor, choices.clone());
        press(&mut editor, KeyCode::Down);
        assert_eq!(editor.completion_menu.as_ref().unwrap().selected, 1);
        press(&mut editor, KeyCode::BackTab);
        assert_eq!(editor.completion_menu.as_ref().unwrap().selected, 0);
        press(&mut editor, KeyCode::Up);
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.doc.lsp_text(), "println");
        provide_completions(&mut editor, serde_json::json!([{"label":"println2"}]));
        press(&mut editor, KeyCode::Enter);
        assert_eq!(editor.doc.lsp_text(), "println\n");
        assert!(editor.completion_menu.is_none());
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.doc.lsp_text(), "println\n\t");
    }

    #[test]
    fn completion_import_and_replacement_are_one_undoable_change() {
        let original = "// header\nfn run() { pri }";
        let mut editor = completion_editor(original);
        editor.doc.row = 1;
        editor.doc.col = 14;
        provide_completions(
            &mut editor,
            serde_json::json!([{
                "label":"println!",
                "textEdit":{"range":{"start":{"line":1,"character":11},
                    "end":{"line":1,"character":14}},"newText":"println!"},
                "additionalTextEdits":[{"range":{"start":{"line":0,"character":0},
                    "end":{"line":0,"character":0}},"newText":"use foo::Bar;\n"}]
            }]),
        );
        press(&mut editor, KeyCode::Tab);
        assert_eq!(
            editor.doc.lsp_text(),
            "use foo::Bar;\n// header\nfn run() { println! }"
        );
        assert_eq!((editor.doc.row, editor.doc.col), (2, 19));
        editor.undo();
        assert_eq!(editor.doc.lsp_text(), original);
        editor.redo();
        assert!(editor.doc.lsp_text().contains("use foo::Bar;"));
    }

    #[test]
    fn invalid_completion_preserves_document_and_undo_redo_history() {
        let mut editor = completion_editor("pri");
        editor.remember();
        editor.doc.insert('x');
        editor.undo();
        let before = (
            editor.doc.lsp_text(),
            editor.doc.revision,
            editor.undo.len(),
            editor.redo.len(),
        );
        provide_completions(
            &mut editor,
            serde_json::json!([{
                "label":"broken", "textEdit":{"range":{
                    "start":{"line":0,"character":0},"end":{"line":99,"character":0}
                },"newText":"bad"}
            }]),
        );
        press(&mut editor, KeyCode::Tab);
        assert_eq!(
            (
                editor.doc.lsp_text(),
                editor.doc.revision,
                editor.undo.len(),
                editor.redo.len()
            ),
            before
        );
        assert!(editor.message.contains("Cannot apply completion"));
    }

    #[test]
    fn completion_results_are_ignored_after_edit_movement_dismissal_and_switch() {
        let choices = serde_json::json!([{"label":"print"}]);
        let mut editor = completion_editor("pri");
        let context = provide_completions(&mut editor, choices.clone());
        press(&mut editor, KeyCode::Char('v'));
        editor.show_completions(
            context.clone(),
            lsp::Encoding::Utf16,
            completion::parse(&choices),
        );
        assert!(editor.completion_menu.is_none());
        assert_eq!(editor.doc.lsp_text(), "priv");
        let context = provide_completions(&mut editor, serde_json::json!([{"label":"private"}]));
        press(&mut editor, KeyCode::Left);
        editor.show_completions(context, lsp::Encoding::Utf16, completion::parse(&choices));
        assert!(editor.completion_menu.is_none() && editor.completion_pending.is_none());
        provide_completions(&mut editor, choices.clone());
        editor.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL), 10);
        assert!(editor.completion_menu.is_none());
        assert_eq!(editor.mode, Mode::Insert);
        assert!(editor.completion_due.is_none());
        let root = temporary_buffer_project();
        let context = provide_completions(&mut editor, choices.clone());
        editor.open(root.join("two.md").display().to_string());
        editor.begin_insert();
        editor.show_completions(context, lsp::Encoding::Utf16, completion::parse(&choices));
        assert!(editor.completion_menu.is_none() && editor.completion_pending.is_none());
        assert_eq!(editor.doc.lsp_text(), "# two\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completion_is_debounced_and_cursor_movement_cancels_pending_work() {
        let mut editor = completion_editor("");
        press(&mut editor, KeyCode::Char('p'));
        assert!(editor.completion_due.is_none());
        press(&mut editor, KeyCode::Char('r'));
        assert!(editor.completion_due.is_some());
        press(&mut editor, KeyCode::Left);
        assert!(editor.completion_due.is_none());
        press(&mut editor, KeyCode::Char('.'));
        assert!(editor.completion_due.is_some());
        editor.pasted("literal");
        assert!(editor.completion_due.is_none());
        assert!(editor.completion_menu.is_none());
    }

    #[test]
    fn completion_popup_is_cached_and_restores_covered_editor_rows() {
        let mut editor = completion_editor("pri\ncovered-row\nthird\nfourth\nfifth");
        editor.doc.row = 0;
        editor.doc.col = 3;
        paint(&mut editor);
        provide_completions(&mut editor, serde_json::json!([{"label":"print"}]));
        let popup = paint(&mut editor);
        assert!(popup.contains("› print"));
        assert!(!popup.contains("\x1b[2J"));
        let same = paint(&mut editor);
        assert!(!same.contains("› print"));
        editor.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL), 10);
        let restored = paint(&mut editor);
        assert!(restored.contains("covered-row"));
        assert!(!restored.contains("\x1b[2J"));
    }

    #[test]
    fn legacy_ctrl_space_invokes_completion_without_inserting_nul() {
        let mut editor = completion_editor("text");
        editor.doc.path = Some(PathBuf::from("completion-fixture.md"));
        press(&mut editor, KeyCode::Null);
        assert_eq!(editor.doc.lsp_text(), "text");
        assert!(editor.message.contains("No language server configured"));
        assert!(!editor.doc.dirty);
    }

    #[test]
    fn tiny_terminal_never_accepts_an_invisible_completion() {
        for (cols, rows) in [(0, 0), (5, 20), (80, 4), (80, 5)] {
            let mut editor = completion_editor("pri");
            provide_completions(&mut editor, serde_json::json!([{"label":"print"}]));
            editor.draw_at(&mut Vec::new(), cols, rows).unwrap();
            assert!(editor.completion_menu.is_none());
            press(&mut editor, KeyCode::Tab);
            assert_eq!(editor.doc.lsp_text(), "pri\t");
        }
    }

    #[test]
    #[ignore = "run in CI with rustup component add rust-analyzer rust-src"]
    fn actual_rust_analyzer_editor_completion_can_be_selected_rendered_and_undone() {
        let root = temporary_buffer_project();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"),
            "[package]\nname = \"remacs-editor-completion\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let text = "pub struct Widget { pub width: u32 }\npub fn run() { let widget = Widget { width: 42 }; widget.wid }\n";
        let source = root.join("src/lib.rs");
        std::fs::write(&source, text).unwrap();
        let mut editor = Editor::new(Document::open(source).unwrap());
        editor.begin_insert();
        editor.doc.row = 1;
        editor.doc.col =
            text.lines().nth(1).unwrap().find("widget.wid").unwrap() + "widget.wid".len();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut retry = Instant::now();
        let mut found = false;
        while Instant::now() < deadline {
            editor.poll_lsp();
            if let Some(menu) = &mut editor.completion_menu {
                if let Some(index) = menu
                    .items
                    .iter()
                    .position(|item| item.filter_text == "width" || item.label == "width")
                {
                    menu.selected = index;
                    found = true;
                    break;
                }
            }
            if Instant::now() >= retry {
                // Invoking again after an initially empty result mirrors
                // Ctrl+Space while the workspace finishes indexing.
                editor.request_completion(true);
                retry = Instant::now() + Duration::from_millis(400);
            }
            std::thread::sleep(Duration::from_millis(24));
        }
        if found {
            let output = paint(&mut editor);
            assert!(output.contains("width"));
            press(&mut editor, KeyCode::Tab);
            assert!(editor.doc.lsp_text().contains("widget.width"));
            assert_eq!(editor.mode, Mode::Insert);
            editor.undo();
            assert_eq!(editor.doc.lsp_text(), text);
            // Once indexed, a normal edit must invoke completion through
            // the debounce timer without another manual request.
            press(&mut editor, KeyCode::Backspace);
            press(&mut editor, KeyCode::Char('d'));
            assert!(editor.completion_due.is_some());
            let automatic_deadline = Instant::now() + Duration::from_secs(5);
            let mut automatic = false;
            while Instant::now() < automatic_deadline {
                editor.poll_lsp();
                if let Some(menu) = &mut editor.completion_menu {
                    if let Some(index) = menu
                        .items
                        .iter()
                        .position(|item| item.filter_text == "width" || item.label == "width")
                    {
                        menu.selected = index;
                        automatic = true;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(24));
            }
            assert!(
                automatic,
                "Typing did not automatically show completion: {}",
                editor.message
            );
            paint(&mut editor);
            press(&mut editor, KeyCode::Tab);
            assert!(editor.doc.lsp_text().contains("widget.width"));
        }
        let status = editor.message.clone();
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
        assert!(found, "Editor did not display a real completion: {status}");
    }

    #[test]
    fn unicode_width() {
        assert_eq!(visual_column(&"世界".chars().collect::<Vec<_>>()), 4);
        assert_eq!(
            render_line(&"世界".chars().collect::<Vec<_>>(), 1, 3),
            " 界"
        );
    }
}
