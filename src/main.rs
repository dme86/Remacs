//! Remacs: a minimal terminal editor with Vim-style modes and Doom-inspired leader keys.
mod document;
mod finder_ui;
mod function_select;
mod plugins;
mod syntax;

use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use document::Document;
use finder_ui::{rect_for as finder_rect_for, search_cursor as finder_cursor, Cache as FinderCache, PickerKind};
use function_select::{around_function, Position, TextRange};
use plugins::{FinderView, TreeAction, TreePlugin};
use syntax::{Span, SyntaxEngine};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TAB_WIDTH: usize = 4;
const MAX_UNDO: usize = 100;
const MAX_UNDO_BYTES: usize = 32 * 1024 * 1024; // per-buffer snapshot budget
// Subtle current-line tint. The normal terminal background is unchanged.
const CURRENT_LINE_BG: Color = Color::Rgb { r: 49, g: 50, b: 57 };
const GUTTER_BG: Color = Color::Rgb { r: 35, g: 36, b: 42 };
const GUTTER_FG: Color = Color::Rgb { r: 116, g: 119, b: 132 };
const CURRENT_NUMBER_FG: Color = Color::Rgb { r: 223, g: 211, b: 255 };
const VISUAL_SELECTION_BG: Color = Color::Rgb { r: 83, g: 69, b: 111 };

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
    Leader,
    File,
    Panel,
    Buffer,
    Visual,
    TextObjectAround,
    Quit,
    Delete,
    Yank,
    Go,
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
}

impl Snapshot {
    fn capture(doc: &Document) -> Self {
        Self {
            lines: doc.lines.clone(),
            row: doc.row,
            col: doc.col,
        }
    }
    fn approx_bytes(&self) -> usize {
        self.lines.iter().map(|line| {
            std::mem::size_of::<Vec<char>>()
                + line.capacity() * std::mem::size_of::<char>()
        }).sum()
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

#[derive(Default)]
struct ScreenCache {
    size: Option<(u16, u16)>,
    origin: Option<(usize, usize)>,
    syntax_enabled: Option<bool>,
    sidebar_width: usize,
    sidebar_lines: Vec<Option<SidebarPaint>>,
    finder: FinderCache,
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

// Only path and filename are compared for fuzzy matching. No filesystem scan
// is needed: a buffer picker typically only contains a handful of entries.
fn matches_buffer(label: &str, query: &str) -> bool {
    let mut letters = label.to_lowercase().chars().collect::<Vec<_>>().into_iter();
    query.to_lowercase().chars().all(|wanted| letters.by_ref().any(|actual| actual == wanted))
}

fn path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
    };
    // Normalise the parent even for a newly created, not-yet-existing file.
    absolute.parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .and_then(|parent| absolute.file_name().map(|file| parent.join(file)))
        .unwrap_or(absolute)
}

struct Editor {
    doc: Document,
    syntax: SyntaxEngine,
    slots: Vec<Option<StoredBuffer>>,
    active_slot: usize,
    last_slot: Option<usize>,
    buffer_picker: Option<BufferPicker>,
    tree: Option<TreePlugin>,
    screen: ScreenCache,
    scroll_row: usize,
    scroll_col: usize,
    mode: Mode,
    selection: Option<TextRange>,
    visual_anchor: Option<Position>,
    sequence: Sequence,
    control_x: bool,
    count: Option<usize>,
    prompt: Option<Prompt>,
    message: String,
    register: Option<Register>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    insert_changed: bool,
    running: bool,
}

impl Editor {
    fn new(doc: Document) -> Self {
        let syntax = SyntaxEngine::new(&doc);
        Self {
            doc,
            syntax,
            slots: vec![None],
            active_slot: 0,
            last_slot: None,
            buffer_picker: None,
            tree: None,
            screen: ScreenCache::default(),
            scroll_row: 0,
            scroll_col: 0,
            mode: Mode::Normal,
            selection: None,
            visual_anchor: None,
            sequence: Sequence::None,
            control_x: false,
            count: None,
            prompt: None,
            message: "NORMAL | i Insert | Space f f Open | Space f s Save | Space q q Quit".into(),
            register: None,
            undo: Vec::new(),
            redo: Vec::new(),
            insert_changed: false,
            running: true,
        }
    }

    fn active_name(&self) -> String {
        self.doc.path.as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[No Name]".into())
    }

    fn buffer_name(doc: &Document) -> String {
        doc.path.as_ref().map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[No Name]".into())
    }

    fn dirty_buffers(&self) -> usize {
        usize::from(self.doc.dirty)
            + self.slots.iter().flatten().filter(|buffer| buffer.doc.dirty).count()
    }

    fn named_slot(&self, path: &Path) -> Option<usize> {
        let target = path_identity(path);
        if self.doc.path.as_ref().is_some_and(|p| path_identity(p) == target) {
            return Some(self.active_slot);
        }
        self.slots.iter().position(|entry| entry.as_ref().is_some_and(|buffer| {
            buffer.doc.path.as_ref().is_some_and(|path| path_identity(path) == target)
        }))
    }

    fn set_path_and_save(&mut self, path: PathBuf) {
        if self.named_slot(&path).is_some_and(|index| index != self.active_slot) {
            self.message = "That file is already open in another buffer".into();
            return;
        }
        let before = self.doc.path.replace(path);
        match self.doc.save() {
            Ok(()) => self.message = "File saved".into(),
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
        self.doc = next.doc;
        self.undo = next.undo;
        self.redo = next.redo;
        self.scroll_row = next.scroll_row;
        self.scroll_col = next.scroll_col;
        self.active_slot = index;
        let enabled = self.syntax.enabled();
        self.syntax = SyntaxEngine::new(&self.doc);
        self.syntax.set_enabled(enabled);
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
        self.count = None;
        self.control_x = false;
        self.insert_changed = false;
        // Only visible text rows are invalidated; tree and terminal geometry
        // are unchanged, so there is no destructive whole-screen clear.
        for line in &mut self.screen.lines { *line = None; }
        self.screen.status = None;
        self.screen.origin = None;
        self.message = format!("Buffer {}/{}: {}", index + 1, self.slots.len(), self.active_name());
    }

    fn switch_buffer(&mut self, index: usize) {
        if index == self.active_slot || index >= self.slots.len() {
            return;
        }
        let Some(next) = self.slots[index].take() else { return; };
        let previous = self.active_slot;
        let current = self.take_current();
        self.slots[previous] = Some(current);
        self.last_slot = Some(previous);
        self.activate(next, index);
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
    }

    fn last_buffer(&mut self) {
        if let Some(index) = self.last_slot {
            self.switch_buffer(index);
        } else {
            self.message = "No previous buffer".into();
        }
    }

    fn open_buffer_picker(&mut self) {
        self.buffer_picker = Some(BufferPicker::default());
        self.sequence = Sequence::None;
        self.message = "Buffers: type to filter, ↑↓ select, Enter switch, Esc cancel".into();
    }

    fn buffer_candidates(&self, query: &str) -> Vec<(usize, String)> {
        let mut entries: Vec<(usize, String, bool)> = self.slots.iter()
            .enumerate().filter_map(|(index, slot)| {
                let (doc, active) = if index == self.active_slot {
                    (&self.doc, true)
                } else {
                    (&slot.as_ref()?.doc, false)
                };
                let name = Self::buffer_name(doc);
                // Filename comes first so long absolute paths never hide
                // the useful part of an entry in a narrower terminal.
                let basename = doc.path.as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "[No Name]".into());
                let file_match = matches_buffer(&basename, query);
                if !file_match && !matches_buffer(&name, query) { return None; }
                let parent = doc.path.as_ref().and_then(|path| path.parent())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some((index, format!(
                    "{} {}{}  {}  {}",
                    if active { "▶" } else { " " },
                    if doc.dirty { "● " } else { "  " },
                    index + 1, basename, parent
                ), file_match))
            }).collect();
        // Filename hits take precedence over unrelated matches created by
        // combining characters across the absolute parent directory.
        if !query.is_empty() && entries.iter().any(|(_, _, file_match)| *file_match) {
            entries.retain(|(_, _, file_match)| *file_match);
        }
        entries.into_iter().map(|(index, label, _)| (index, label)).collect()
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
        let Some(picker) = self.buffer_picker.as_ref() else { return; };
        let count = self.buffer_candidates(&picker.query).len();
        match key.code {
            KeyCode::Esc => { self.buffer_picker = None; }
            KeyCode::Enter => {
                let results = self.buffer_candidates(&picker.query);
                let index = results.get(picker.selected).map(|(index, _)| *index);
                self.buffer_picker = None;
                if let Some(index) = index { self.switch_buffer(index); }
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
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
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
        if self.slots.len() == 1 {
            let enabled = self.syntax.enabled();
            self.doc = Document::empty();
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
            for line in &mut self.screen.lines { *line = None; }
            self.screen.status = None;
            self.message = "Empty buffer".into();
            return;
        }
        let closed = self.active_slot;
        let next = (closed + 1) % self.slots.len();
        let replacement = self.slots[next].take().expect("inactive buffer in slot");
        self.slots.remove(closed);
        // Adjust target index because removing an earlier slot shifts it.
        let adjusted_next = if next > closed { next - 1 } else { next };
        self.last_slot = None;
        self.activate(replacement, adjusted_next);
        self.message = "Buffer closed".into();
    }

    fn remember(&mut self) {
        let snapshot = Snapshot::capture(&self.doc);
        let next_bytes = snapshot.approx_bytes();
        let mut used = self.undo.iter().map(Snapshot::approx_bytes).sum::<usize>();
        while !self.undo.is_empty()
            && (self.undo.len() >= MAX_UNDO
                || used.saturating_add(next_bytes) > MAX_UNDO_BYTES)
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
            match TreePlugin::new(&root) {
                Ok(tree) => self.tree = Some(tree),
                Err(error) => {
                    self.message = format!("Cannot load Lua tree plugin: {error}");
                    return;
                }
            }
        }
        if let Some(tree) = &mut self.tree {
            if let Err(error) = tree.toggle() {
                self.message = format!("Tree plugin error: {error}");
            } else if tree.visible {
                self.message = "Tree: / or f fuzzy find  j/k move  Enter open  Tab/Esc editor".into();
            }
        }
        self.sequence = Sequence::None;
    }

    fn tree_key(&mut self, key: KeyEvent) {
        let name = match key.code {
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                c.to_string(),
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
        let action = match self.tree.as_ref().map(|tree| tree.on_key(&name)) {
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
                if let Some(tree) = &mut self.tree { tree.leave(); }
            }
            TreeAction::Close => {
                if let Some(tree) = &mut self.tree { tree.close(); }
            }
            TreeAction::Open(path) => {
                self.open(path.to_string_lossy().into_owned());
                if self.doc.path.as_ref().is_some_and(|current| {
                    path_identity(current) == path_identity(&path)
                }) {
                    if let Some(tree) = &mut self.tree { tree.leave(); }
                }
            }
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
        self.visual_anchor = Some(Position { row: self.doc.row, col: self.doc.col });
        self.update_visual_selection();
        self.message = "VISUAL: a f Function | y Yank | d Delete | Esc Cancel".into();
    }

    fn update_visual_selection(&mut self) {
        let Some(anchor) = self.visual_anchor else { return };
        let here = Position { row: self.doc.row, col: self.doc.col };
        let inclusive_end = |position: Position, doc: &Document| Position {
            row: position.row,
            col: (position.col + 1).min(doc.lines[position.row].len()),
        };
        self.selection = Some(if anchor <= here {
            TextRange { start: anchor, end: inclusive_end(here, &self.doc) }
        } else {
            TextRange { start: here, end: inclusive_end(anchor, &self.doc) }
        });
    }

    fn select_function(&mut self) {
        self.sequence = Sequence::None;
        match around_function(&self.doc) {
            Ok(Some(range)) => {
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

    fn visual_key(&mut self, key: KeyEvent) {
        if self.sequence == Sequence::TextObjectAround {
            self.sequence = Sequence::None;
            if key.code == KeyCode::Char('f') {
                self.select_function();
            } else {
                self.message = "Unknown Visual text object (use a f)".into();
            }
            return;
        }
        match key.code {
            KeyCode::Char('a') => {
                self.sequence = Sequence::TextObjectAround;
                return;
            }
            KeyCode::Char('v') => {
                self.leave_visual();
                return;
            }
            KeyCode::Char('y') => {
                if let Some(range) = self.selection {
                    let text = self.doc.text_in_range(range);
                    self.register = Some(Register::Text(text));
                    self.message = "Selection yanked into Remacs register".into();
                }
                self.leave_visual();
                return;
            }
            KeyCode::Char('d') => {
                if let Some(range) = self.selection {
                    if range.start < range.end {
                        self.remember();
                        self.register = Some(Register::Text(self.doc.text_in_range(range)));
                        self.doc.delete_text_range(range);
                        self.message = "Selection deleted (u to undo)".into();
                    }
                }
                self.leave_visual();
                return;
            }
            KeyCode::Char('h') | KeyCode::Left => self.doc.left(),
            KeyCode::Char('j') | KeyCode::Down => self.doc.down(),
            KeyCode::Char('k') | KeyCode::Up => self.doc.up(),
            KeyCode::Char('l') | KeyCode::Right => self.doc.right(),
            KeyCode::Char('0') | KeyCode::Home => self.doc.home(),
            KeyCode::Char('$') | KeyCode::End => self.doc.end(),
            KeyCode::Char('w') => self.doc.word_forward(),
            KeyCode::Char('b') => self.doc.word_backward(),
            _ => return,
        }
        self.update_visual_selection();
    }

    fn save(&mut self) {
        if self.doc.path.is_none() {
            self.prompt = Some(Prompt::SaveAs(String::new()));
            return;
        }
        match self.doc.save() {
            Ok(()) => self.message = "File saved".into(),
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

        let mut save_if_dirty = |doc: &mut Document| {
            if !doc.dirty {
                return;
            }
            let Some(path) = doc.path.as_ref() else {
                unnamed += 1;
                return;
            };
            let name = path.display().to_string();
            match doc.save() {
                Ok(()) => saved += 1,
                Err(error) => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("{name}: {error}"));
                    }
                }
            }
        };

        save_if_dirty(&mut self.doc);
        for slot in self.slots.iter_mut().flatten() {
            save_if_dirty(&mut slot.doc);
        }

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
                let previous = self.active_slot;
                let current = self.take_current();
                self.slots[previous] = Some(current);
                let index = self.slots.len();
                self.slots.push(None);
                self.last_slot = Some(previous);
                self.activate(StoredBuffer {
                    doc, undo: Vec::new(), redo: Vec::new(),
                    scroll_row: 0, scroll_col: 0,
                }, index);
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
        match command.trim() {
            "w" => self.save(),
            "wa" | "wall" => self.save_all(),
            "q" => self.quit(),
            "q!" => self.running = false,
            "syntax on" => {
                self.syntax.set_enabled(true);
                self.message = "Syntax highlighting enabled".into();
            }
            "syntax off" => {
                self.syntax.set_enabled(false);
                self.message = "Syntax highlighting disabled".into();
            }
            "tree" | "tree toggle" => self.toggle_tree(),
            "buffers" | "ls" => self.open_buffer_picker(),
            "bn" | "bnext" => self.cycle_buffer(true),
            "bp" | "bprev" => self.cycle_buffer(false),
            "b#" => self.last_buffer(),
            "bd" => self.close_buffer(false),
            "bd!" => self.close_buffer(true),
            "wq" | "x" => {
                if self.doc.path.is_some() {
                    self.save();
                    if !self.doc.dirty {
                        self.quit(); // Other unsaved buffers still require confirmation.
                    }
                } else {
                    self.message = "Choose a filename with :w or Space f s first".into();
                }
            }
            value if value.starts_with("e ") => self.open(value[2..].trim().to_owned()),
            value if value.starts_with("w ") => {
                let name = value[2..].trim();
                if !name.is_empty() {
                    self.set_path_and_save(PathBuf::from(name));
                }
            }
            "" => {},
            other => self.message = format!("Unknown command: {other}"),
        }
    }

    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.take() else { return; };
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
                    if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
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
                    if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
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
                    if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    command.push(c);
                    self.prompt = Some(Prompt::Command(command));
                }
                _ => self.prompt = Some(Prompt::Command(command)),
            },
        }
    }

    fn key(&mut self, key: KeyEvent, page_height: usize) {
        if self.prompt.is_some() {
            self.prompt_key(key);
            return;
        }
        if self.buffer_picker.is_some() {
            self.buffer_picker_key(key);
            return;
        }
        // Ctrl+N is a global tree toggle, even when the tree or its fuzzy
        // finder has focus. Handle it before forwarding keys to Lua.
        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_tree();
            return;
        }
        if self.tree.as_ref().is_some_and(|tree| tree.focused) {
            self.tree_key(key);
            return;
        }
        if self.mode == Mode::Normal && key.code == KeyCode::Tab {
            if let Some(tree) = &mut self.tree {
                if tree.visible {
                    tree.focus();
                    return;
                }
            }
        }
        if key.code == KeyCode::Esc {
            if self.mode == Mode::Insert { self.leave_insert(); }
            else if self.mode == Mode::Visual { self.leave_visual(); }
            self.sequence = Sequence::None;
            self.control_x = false;
            self.count = None;
            return;
        }
        if self.control_x {
            self.control_x = false;
            match key.code {
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => self.save(),
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.prompt = Some(Prompt::Open(String::new()));
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit(),
                _ => self.message = "Unknown C-x sequence (Esc cancels)".into(),
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('x') => self.control_x = true,
                KeyCode::Char('g') => { self.sequence = Sequence::None; self.count = None; self.message = "Cancelled".into(); }
                KeyCode::Char('r') if self.mode == Mode::Normal => self.redo(),
                KeyCode::Char('v') if self.mode == Mode::Normal => self.page_down(page_height),
                _ => {},
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            if key.code == KeyCode::Char('v') && self.mode == Mode::Normal {
                self.page_up(page_height);
            }
            return;
        }
        if self.mode == Mode::Insert {
            match key.code {
                KeyCode::Left => self.doc.left(),
                KeyCode::Right => self.doc.right(),
                KeyCode::Up => self.doc.up(),
                KeyCode::Down => self.doc.down(),
                KeyCode::Home => self.doc.home(),
                KeyCode::End => self.doc.end(),
                KeyCode::Backspace => {
                    if self.doc.col > 0 || self.doc.row > 0 {
                        self.insert_change(); self.doc.backspace();
                    }
                }
                KeyCode::Delete => { self.insert_change(); self.doc.delete(); }
                KeyCode::Enter => { self.insert_change(); self.doc.insert('\n'); }
                KeyCode::Tab => { self.insert_change(); self.doc.insert('\t'); }
                KeyCode::Char(c) => { self.insert_change(); self.doc.insert(c); }
                _ => {},
            }
            return;
        }
        if self.mode == Mode::Visual {
            self.visual_key(key);
            return;
        }
        self.normal_key(key, page_height);
    }

    fn normal_key(&mut self, key: KeyEvent, page_height: usize) {
        if self.sequence != Sequence::None {
            let sequence = std::mem::take(&mut self.sequence);
            let count = self.count.take();
            match (sequence, key.code) {
                (Sequence::Leader, KeyCode::Char('b')) => self.sequence = Sequence::Buffer,
                (Sequence::Leader, KeyCode::Char('f')) => self.sequence = Sequence::File,
                (Sequence::Leader, KeyCode::Char('o')) => self.sequence = Sequence::Panel,
                (Sequence::Leader, KeyCode::Char('v')) => self.sequence = Sequence::Visual,
                (Sequence::Visual, KeyCode::Char('f')) => self.select_function(),
                (Sequence::Leader, KeyCode::Char('q')) => self.sequence = Sequence::Quit,
                (Sequence::Buffer, KeyCode::Char('b')) => self.open_buffer_picker(),
                (Sequence::Buffer, KeyCode::Char('n')) => self.cycle_buffer(true),
                (Sequence::Buffer, KeyCode::Char('p')) => self.cycle_buffer(false),
                (Sequence::Buffer, KeyCode::Char('l')) => self.last_buffer(),
                (Sequence::Buffer, KeyCode::Char('d')) => self.close_buffer(false),
                (Sequence::Buffer, KeyCode::Char('s')) => self.save_all(),
                (Sequence::Panel, KeyCode::Char('p')) => self.toggle_tree(),
                (Sequence::File, KeyCode::Char('t')) => self.toggle_tree(),
                (Sequence::File, KeyCode::Char('f')) => self.prompt = Some(Prompt::Open(String::new())),
                (Sequence::File, KeyCode::Char('s')) => self.save(),
                (Sequence::Quit, KeyCode::Char('q')) => self.quit(),
                (Sequence::Delete, KeyCode::Char('d')) => {
                    self.remember();
                    self.register = Some(Register::Line(self.doc.delete_line()));
                }
                (Sequence::Yank, KeyCode::Char('y')) => {
                    self.register = Some(Register::Line(self.doc.line().to_vec()));
                    self.message = "Line yanked".into();
                }
                (Sequence::Go, KeyCode::Char('g')) => {
                    self.doc.row = count.unwrap_or(1).saturating_sub(1).min(self.doc.lines.len() - 1);
                    self.doc.home();
                }
                _ => self.message = "Unknown key sequence".into(),
            }
            return;
        }
        // Vim-style numeric prefixes: 5j, 12k, 120G, 42gg.
        // Zero alone still means beginning of line.
        if let KeyCode::Char(digit @ '0'..='9') = key.code {
            if digit != '0' || self.count.is_some() {
                self.count = Some(
                    self.count.unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add((digit as u8 - b'0') as usize),
                );
                return;
            }
        }
        let explicit_count = self.count.take();
        let count = explicit_count.unwrap_or(1);
        match key.code {
            KeyCode::Char(' ') => self.sequence = Sequence::Leader,
            KeyCode::Char('v') => self.enter_visual(),
            KeyCode::Char('i') => self.begin_insert(),
            KeyCode::Char('a') => {
                if self.doc.col < self.doc.line().len() { self.doc.col += 1; }
                self.begin_insert();
            }
            KeyCode::Char('I') => { self.doc.home(); self.begin_insert(); }
            KeyCode::Char('A') => { self.doc.end(); self.begin_insert(); }
            KeyCode::Char('o') => {
                self.remember();
                self.doc.insert_line_below(Vec::new());
                self.begin_insert();
                self.insert_changed = true;
            }
            KeyCode::Char('O') => {
                self.remember();
                self.doc.insert_line_above(Vec::new());
                self.begin_insert();
                self.insert_changed = true;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.doc.col = self.doc.col.saturating_sub(count);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if !self.doc.line().is_empty() { self.doc.col = self.doc.col.saturating_add(count).min(self.doc.line().len() - 1); }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.doc.row = self.doc.row.saturating_add(count).min(self.doc.lines.len() - 1);
                self.doc.col = self.doc.col.min(self.doc.line().len());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.doc.row = self.doc.row.saturating_sub(count);
                self.doc.col = self.doc.col.min(self.doc.line().len());
            },
            KeyCode::Char('0') | KeyCode::Home => self.doc.home(),
            KeyCode::Char('$') | KeyCode::End => {
                self.doc.end();
                self.doc.col = self.doc.col.saturating_sub(1);
            }
            KeyCode::Char('w') => {
                for _ in 0..count.min(100_000) { self.doc.word_forward(); }
            }
            KeyCode::Char('b') => {
                for _ in 0..count.min(100_000) { self.doc.word_backward(); }
            }
            KeyCode::Char('g') => {
                self.count = explicit_count;
                self.sequence = Sequence::Go;
            },
            KeyCode::Char('G') => {
                self.doc.row = explicit_count.map_or(
                    self.doc.lines.len() - 1,
                    |line| line.saturating_sub(1).min(self.doc.lines.len() - 1),
                );
                self.doc.home();
            }
            KeyCode::Char('d') => self.sequence = Sequence::Delete,
            KeyCode::Char('y') => self.sequence = Sequence::Yank,
            KeyCode::Char('x') => {
                if self.doc.col < self.doc.line().len() {
                    self.remember();
                    self.register = Some(Register::Text(self.doc.line()[self.doc.col].to_string()));
                    self.doc.delete();
                }
            }
            KeyCode::Char('p') => {
                if let Some(value) = self.register.clone() {
                    self.remember();
                    match value {
                        Register::Line(line) => self.doc.insert_line_below(line),
                        Register::Text(text) => {
                            if self.doc.col < self.doc.line().len() { self.doc.col += 1; }
                            self.doc.insert_text(&text);
                            self.doc.col = self.doc.col.saturating_sub(1);
                        }
                    }
                }
            }
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char(':') => self.prompt = Some(Prompt::Command(String::new())),
            KeyCode::PageDown => self.page_down(page_height),
            KeyCode::PageUp => self.page_up(page_height),
            _ => {},
        }
    }

    fn page_down(&mut self, height: usize) {
        for _ in 0..height.max(1) { self.doc.down(); }
    }
    fn page_up(&mut self, height: usize) {
        for _ in 0..height.max(1) { self.doc.up(); }
    }

    fn ensure_visible(&mut self, width: usize, height: usize) {
        if self.doc.row < self.scroll_row { self.scroll_row = self.doc.row; }
        if height > 0 && self.doc.row >= self.scroll_row + height {
            self.scroll_row = self.doc.row - height + 1;
        }
        let column = visual_column(&self.doc.line()[..self.doc.col]);
        if column < self.scroll_col { self.scroll_col = column; }
        if width > 0 && column >= self.scroll_col + width {
            self.scroll_col = column - width + 1;
        }
    }

    fn draw(&mut self, output: &mut impl Write) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        self.draw_at(output, cols, rows)
    }

    // Compare the current viewport with the last painted frame. A simple j/k
    // movement only updates the cursor and status line: no screen clear or
    // syntax-aware body repaint is sent to the terminal.
    fn draw_at(&mut self, output: &mut impl Write, cols: u16, rows: u16) -> io::Result<()> {
        let syntax_invalid = self.syntax.refresh(&mut self.doc);
        if cols == 0 || rows == 0 {
            self.screen = ScreenCache::default();
            return Ok(());
        }

        let width = cols as usize;
        let height = rows.saturating_sub(2) as usize;
        let sidebar_width = tree_sidebar_width(width, self.tree.as_ref().is_some_and(|tree| tree.visible));
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

        let tree_view = match self.tree.as_ref().map(|tree| tree.view(height.saturating_sub(1))) {
            Some(Ok(view)) => Some(view),
            Some(Err(error)) => {
                self.message = format!("Tree render error: {error}");
                None
            }
            None => None,
        };
        let buffer_view = self.buffer_picker_view();
        let finder_rect = if buffer_view.is_some()
            || tree_view.as_ref().and_then(|view| view.finder.as_ref()).is_some()
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
                if let Some(line) = self.screen.lines.get_mut(y) { *line = None; }
                if let Some(line) = self.screen.sidebar_lines.get_mut(y) { *line = None; }
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
                    let cell = SidebarPaint { text, active, focused };
                    if self.screen.sidebar_lines[y].as_ref() == Some(&cell) {
                        continue;
                    }
                    let background = if active && focused {
                        Color::Rgb { r: 65, g: 64, b: 78 }
                    } else if active {
                        Color::Rgb { r: 49, g: 48, b: 59 }
                    } else {
                        Color::Rgb { r: 34, g: 35, b: 42 }
                    };
                    let foreground = if active {
                        Color::Rgb { r: 226, g: 213, b: 255 }
                    } else {
                        Color::Rgb { r: 174, g: 179, b: 194 }
                    };
                    queue!(output,
                        MoveTo(0, y as u16),
                        SetBackgroundColor(background),
                        SetForegroundColor(foreground),
                        Print(fit_cells(&cell.text, sidebar_width.saturating_sub(1))),
                        ResetColor,
                        SetForegroundColor(Color::Rgb { r: 82, g: 84, b: 96 }),
                        Print("│"),
                        ResetColor
                    )?;
                    self.screen.sidebar_lines[y] = Some(cell);
                    // This row may lie behind the centered finder. If the
                    // sidebar just repainted it, the popup must redraw that
                    // portion even when its own text is unchanged.
                    self.screen.finder.invalidate_underpaint(0, y, sidebar_width);
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
            let selected = self.selection.and_then(|range| range.columns(index, line.map_or(0, Vec::len)));
            let selection_changed = old.is_none_or(|painted| painted.selected != selected);
            let gutter_changed = old.is_none_or(|painted| painted.gutter != number);
            let syntax_stale = self.syntax.enabled()
                && syntax_invalid.is_some_and(|first| index >= first);
            let paint_text = repaint_all || text_stale || highlight_changed || syntax_stale || selection_changed;

            if !paint_text && !gutter_changed {
                continue;
            }
            if gutter_width > 0 && (paint_text || gutter_changed) {
                queue!(output,
                    MoveTo(sidebar_width as u16, y as u16),
                    SetBackgroundColor(GUTTER_BG),
                    SetForegroundColor(if active { CURRENT_NUMBER_FG } else { GUTTER_FG }),
                    Print(&number),
                    ResetColor
                )?;
                self.screen.finder.invalidate_underpaint(sidebar_width, y, gutter_width);
            }
            if paint_text {
                queue!(output, MoveTo((sidebar_width + gutter_width) as u16, y as u16), ResetColor)?;
                if active {
                    queue!(output, SetBackgroundColor(CURRENT_LINE_BG))?;
                }

                let mut visible_width = 0;
                if let Some(line) = line {
                    if let Some(columns) = selected {
                        let spans = self.syntax.spans(&self.doc, index);
                        for (foreground, background, text) in render_selected_line(
                            line, spans.unwrap_or(&[]), self.scroll_col, text_width, columns, active
                        ) {
                            visible_width += UnicodeWidthStr::width(text.as_str());
                            queue!(output, SetForegroundColor(foreground), SetBackgroundColor(background), Print(text))?;
                        }
                        queue!(output, ResetColor)?;
                    } else if let Some(spans) = self.syntax.spans(&self.doc, index).filter(|s| !s.is_empty()) {
                        for (color, text) in render_color_line(line, spans, self.scroll_col, text_width) {
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
                    queue!(output,
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
                    sidebar_width + gutter_width, y, text_width
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
            let filename = self.doc.path.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[No Name]".into());
            let mark = if self.doc.dirty { "**" } else { "--" };
            let mode = match self.mode { Mode::Normal => "NORMAL", Mode::Insert => "INSERT", Mode::Visual => "VISUAL" };
            let language = if self.syntax.enabled() { self.syntax.name() } else { "Highlight off" };
            let pane = if self.tree.as_ref().is_some_and(|tree| tree.visible) { " | TREE" } else { "" };
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
                queue!(output,
                    MoveTo(0, rows - 2),
                    SetBackgroundColor(Color::DarkGrey), SetForegroundColor(Color::White),
                    Print(&status), ResetColor
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
                None if self.control_x => "C-x".into(),
                None if self.buffer_picker.is_some() =>
                    "BUFFERS  Type to filter · ↑↓ Choose · Enter Switch · Esc Cancel".into(),
                None if self.tree.as_ref().is_some_and(|tree| tree.focused) =>
                    "TREE  / or f Find  j/k Move  l/Enter Open  . Hidden  r Refresh  Tab/Esc Editor  q Close".into(),
                None => match self.sequence {
                    Sequence::Leader => "SPC → b Buffers | f Files | o Panels | v Visual | q Quit".into(),
                    Sequence::Buffer => "SPC b → b Pick | n Next | p Previous | l Last | s Save All | d Close".into(),
                    Sequence::File => "SPC f → f Find | s Save | t Tree".into(),
                    Sequence::Panel => "SPC o → p Project tree".into(),
                    Sequence::Visual => "SPC v → f Select function".into(),
                    Sequence::TextObjectAround => "VISUAL a → f Function".into(),
                    Sequence::Quit => "SPC q → q Quit".into(),
                    Sequence::Delete => "d → d Delete line".into(),
                    Sequence::Yank => "y → y Yank line".into(),
                    Sequence::Go => "g → g First line".into(),
                    Sequence::None => self.count.map_or_else(|| self.message.clone(), |n| n.to_string()),
                },
            };
            let message = fit_cells(&message, width);
            if self.screen.message.as_deref() != Some(message.as_str()) {
                queue!(output, MoveTo(0, rows - 1), ResetColor, Print(&message))?;
                self.screen.message = Some(message);
            }
        }
        // Overlay the fully visible fzf-style picker after the normal editor
        // and sidebar have painted; only changed popup rows are output.
        if let Some(rect) = finder_rect {
            if let Some(finder) = buffer_view.as_ref() {
                self.screen.finder.paint(output, rect, finder, PickerKind::Buffers)?;
            } else if let Some(finder) = tree_view.as_ref().and_then(|view| view.finder.as_ref()) {
                self.screen.finder.paint(output, rect, finder, PickerKind::Files)?;
            }
        }

        let (x, y) = match &self.prompt {
            Some(Prompt::SaveAs(name)) => ("Save file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Open(name)) => ("Find file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Command(command)) => (1 + command.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Quit) => (0, rows.saturating_sub(1)),
            None if finder_rect.is_some() => {
                let rect = finder_rect.unwrap();
                let query = buffer_view.as_ref().or_else(|| {
                    tree_view.as_ref().and_then(|view| view.finder.as_ref())
                }).map_or("", |finder| finder.query.as_str());
                let (x, y) = finder_cursor(rect, query);
                (x as usize, y)
            }
            None if sidebar_width > 0 && self.tree.as_ref().is_some_and(|tree| tree.focused) => (
                search_cursor.map_or(2, |x| x.min(sidebar_width.saturating_sub(2))),
                if search_cursor.is_some() { 0 } else { selected_tree_row as u16 },
            ),
            None => (
                sidebar_width + gutter_width
                    + visual_column(&self.doc.line()[..self.doc.col]).saturating_sub(self.scroll_col),
                self.doc.row.saturating_sub(self.scroll_row) as u16,
            ),
        };
        if cols > 0 && rows > 0 {
            queue!(output, MoveTo((x.min(width - 1)) as u16, y.min(rows - 1)), Show)?;
        }
        self.screen.size = Some((cols, rows));
        self.screen.origin = Some((self.scroll_row, self.scroll_col));
        self.screen.syntax_enabled = Some(self.syntax.enabled());
        self.screen.sidebar_width = sidebar_width;
        output.flush()
    }
}


fn tree_sidebar_width(columns: usize, visible: bool) -> usize {
    if !visible || columns < 48 { return 0; }
    (columns / 3).clamp(24, 34).min(columns - 20)
}

fn gutter_width(columns: usize, total_lines: usize) -> usize {
    // Never sacrifice the entire text area when the terminal is narrow.
    if columns < 5 { 0 } else { (total_lines.to_string().len().max(2) + 2).min(columns - 1) }
}

fn gutter_number(index: usize, cursor: usize, total_lines: usize, width: usize) -> String {
    if width == 0 { return String::new(); }
    if index >= total_lines { return " ".repeat(width); }
    let number = if index == cursor { index + 1 } else { index.abs_diff(cursor) };
    let digits = number.to_string();
    let available = width.saturating_sub(2);
    // On exceptionally narrow screens, keep the gutter inside the viewport.
    let display = &digits[digits.len().saturating_sub(available)..];
    format!("{display:>available$}  ")
}

fn char_width(c: char, column: usize) -> usize {
    if c == '\t' { TAB_WIDTH - (column % TAB_WIDTH) }
    else { UnicodeWidthChar::width(c).unwrap_or(0) }
}

fn visual_column(line: &[char]) -> usize {
    let mut column = 0;
    for &c in line { column += char_width(c, column); }
    column
}

fn render_line(line: &[char], start: usize, width: usize) -> String {
    let mut output = String::new();
    let mut column = 0;
    let end = start.saturating_add(width);
    for &c in line {
        let size = char_width(c, column);
        if column >= end { break; }
        if c == '\t' || c.is_control() {
            for i in 0..size.max(usize::from(c.is_control())) {
                if column + i >= start && column + i < end { output.push(' '); }
            }
        } else if size == 0 {
            if column > start && column <= end { output.push(c); }
        } else if column >= start && column + size <= end {
            output.push(c);
        } else {
            for i in 0..size {
                if column + i >= start && column + i < end { output.push(' '); }
            }
        }
        column += size;
    }
    output
}

fn render_color_line(line: &[char], spans: &[Span], start: usize, width: usize) -> Vec<(Color, String)> {
    let mut runs: Vec<(Color, String)> = Vec::new();
    let mut column = 0;
    let end = start.saturating_add(width);
    for span in spans {
        for &c in &line[span.start..span.end] {
            if column >= end { break; }
            let size = char_width(c, column);
            let mut visible = String::new();
            if c == '\t' || c.is_control() {
                for i in 0..size.max(usize::from(c.is_control())) {
                    if column + i >= start && column + i < end { visible.push(' '); }
                }
            } else if size == 0 {
                if column > start && column <= end { visible.push(c); }
            } else if column >= start && column + size <= end {
                visible.push(c);
            } else {
                for i in 0..size {
                    if column + i >= start && column + i < end { visible.push(' '); }
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
    line: &[char], spans: &[Span], scroll: usize, width: usize,
    selection: (usize, usize), active: bool,
) -> Vec<(Color, Color, String)> {
    let mut runs: Vec<(Color, Color, String)> = Vec::new();
    let mut visual_col = 0;
    let mut syntax_index = 0;
    let end = scroll.saturating_add(width);
    for (index, &ch) in line.iter().enumerate() {
        if visual_col >= end { break; }
        while syntax_index < spans.len() && index >= spans[syntax_index].end {
            syntax_index += 1;
        }
        let foreground = spans.get(syntax_index)
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
            if visual_col > scroll && visual_col <= end { visible.push(ch); }
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
        if used + size > width { break; }
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
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, ResetColor, LeaveAlternateScreen);
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
    while editor.running {
        editor.draw(&mut stdout)?;
        if let Event::Key(key) = event::read()? {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                let height = terminal::size()?.1.saturating_sub(2) as usize;
                editor.key(key, height);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, key: KeyCode) {
        editor.key(KeyEvent::new(key, KeyModifiers::NONE), 10);
    }


    fn paint(editor: &mut Editor) -> String {
        let mut output = Vec::new();
        editor.draw_at(&mut output, 80, 18).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn temporary_buffer_project() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "remacs-buffers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("one.rs"), "one\nsecond\nthird\n").unwrap();
        std::fs::write(root.join("two.md"), "# two\n").unwrap();
        root
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
        assert_eq!(editor.undo.len(), before_undo, "save-all must not wipe undo");
        assert!(std::fs::read_to_string(root.join("one.rs")).unwrap().starts_with('X'));
        assert!(std::fs::read_to_string(root.join("two.md")).unwrap().starts_with('Y'));
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
        assert!(std::fs::read_to_string(root.join("one.rs")).unwrap().starts_with('X'));

        editor.switch_buffer(0);
        assert!(editor.doc.path.is_none());
        assert!(editor.doc.dirty);
        assert_eq!(editor.doc.line().first(), Some(&'!'));
        editor.set_path_and_save(root.join("scratch.txt"));
        assert_eq!(editor.dirty_buffers(), 0);
        assert_eq!(std::fs::read_to_string(root.join("scratch.txt")).unwrap(), "!");
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
        assert_eq!(editor.slots[0].as_ref().unwrap().doc.path.as_ref(), Some(&failed_path));
        assert!(editor.slots[0].as_ref().unwrap().doc.dirty);
        assert!(editor.message.contains("Saved 2"));
        assert!(editor.message.contains("1 failed"));
        assert!(editor.message.contains("failed.rs"));
        assert!(std::fs::read_to_string(root.join("one.rs")).unwrap().starts_with('X'));
        assert!(std::fs::read_to_string(root.join("two.md")).unwrap().starts_with('Y'));
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
        assert!(editor.doc.lines[1].iter().collect::<String>().starts_with('X'));
        editor.undo();
        assert_eq!(editor.doc.lines[1].iter().collect::<String>(), "second");
        editor.switch_buffer(1);
        assert!(editor.doc.lines[0].iter().collect::<String>().starts_with('Y'));
        editor.undo();
        assert_eq!(editor.doc.lines[0].iter().collect::<String>(), "# two");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tree_opens_second_file_without_discarding_modified_first_buffer() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        let mut tree = TreePlugin::new(&root).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        // Sorted root: one.rs, two.md.
        editor.tree_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        editor.tree_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(editor.doc.path.as_ref().is_some_and(|p| p.ends_with("two.md")));
        assert_eq!(editor.slots.len(), 2);
        assert!(editor.slots[0].as_ref().unwrap().doc.dirty);
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        editor.switch_buffer(0);
        assert_eq!(editor.doc.lines[0].first(), Some(&'!'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn doom_buffer_navigation_and_last_buffer_are_stable() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        assert_eq!(editor.active_slot, 1);
        for key in [' ', 'b', 'p'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.active_slot, 0);
        for key in [' ', 'b', 'l'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.active_slot, 1);
        for key in [' ', 'b', 'n'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.active_slot, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn buffer_picker_lists_dirty_buffers_and_switches_without_reloading() {
        let root = temporary_buffer_project();
        let mut editor = Editor::new(Document::open(root.join("one.rs")).unwrap());
        editor.doc.insert('!');
        editor.open(root.join("two.md").to_string_lossy().into_owned());
        for key in [' ', 'b', 'b'] { press(&mut editor, KeyCode::Char(key)); }
        assert!(editor.buffer_picker.is_some());
        let popup = paint(&mut editor);
        assert!(popup.contains("SWITCH BUFFER"));
        assert!(popup.contains("one.rs"));
        assert!(popup.contains("two.md"));
        assert!(popup.contains('●'), "dirty buffers are marked");
        for key in ['o', 'n', 'e'] { press(&mut editor, KeyCode::Char(key)); }
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
        assert!(matches!(editor.prompt, Some(Prompt::Quit)),
            "quit must notice an unsaved inactive buffer");
        editor.prompt_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(editor.running);
        editor.close_buffer(false);
        assert_eq!(editor.slots.len(), 1);
        assert!(editor.doc.dirty, "closing clean buffer must restore dirty buffer");
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
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.md"),
            "save-as cannot overwrite a file already open in another buffer");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn moving_up_and_down_never_repaints_the_file() {
        let mut editor = Editor::new(Document::from_text("Unique first line\nUnique second line\nUnique third line"));
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
            &(0..30).map(|i| format!("row number {i}")).collect::<Vec<_>>().join("\n")
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

    #[test]
    fn counted_vertical_jumps_work_on_large_files() {
        let mut editor = Editor::new(Document::from_text(
            &(0..500).map(|i| format!("row {i}")).collect::<Vec<_>>().join("\n")
        ));
        for c in "125j".chars() { press(&mut editor, KeyCode::Char(c)); }
        assert_eq!(editor.doc.row, 125);
        for c in "40k".chars() { press(&mut editor, KeyCode::Char(c)); }
        assert_eq!(editor.doc.row, 85);
        for c in "420G".chars() { press(&mut editor, KeyCode::Char(c)); }
        assert_eq!(editor.doc.row, 419);
        for c in "17gg".chars() { press(&mut editor, KeyCode::Char(c)); }
        assert_eq!(editor.doc.row, 16);
        press(&mut editor, KeyCode::Char('G'));
        assert_eq!(editor.doc.row, 499);
        for c in "gg".chars() { press(&mut editor, KeyCode::Char(c)); }
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
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.visible && tree.focused));

        // From the focused tree: Ctrl+N must close it (regression).
        press_ctrl_n(&mut editor);
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible && !tree.focused));

        // Re-open the tree and enter the fuzzy finder.
        press_ctrl_n(&mut editor);
        editor.tree_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(editor.tree.as_ref().unwrap().view(10).unwrap().search_cursor.is_some());

        // Ctrl+N remains the global toggle even while searching.
        press_ctrl_n(&mut editor);
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible && !tree.focused));

        // Reopening returns to the tree, not to the stale search prompt.
        press_ctrl_n(&mut editor);
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.visible && tree.focused));
        assert!(editor.tree.as_ref().unwrap().view(10).unwrap().search_cursor.is_none());
        assert_eq!(editor.doc.line().iter().collect::<String>(), "editor text");
    }

    #[test]
    fn overlay_is_repainted_after_sidebar_redraw_during_fuzzy_search() {
        let mut editor = Editor::new(Document::from_text(
            &vec!["SOME-EDITOR-CONTENT"; 60].join("\n")
        ));
        let mut tree = TreePlugin::new(&std::env::current_dir().unwrap()).unwrap();
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
        assert!(frame.contains("FIND FILES"),
            "unchanged popup header must be repainted after tree overlap");
        assert!(frame.contains("  > p"),
            "search input must remain intact when the tree repaints");

        // Simulate an underlying editor repaint on the popup header too.
        editor.screen.lines[overlap_y] = None;
        let frame = draw(&mut editor);
        assert!(frame.contains("FIND FILES"),
            "popup border must survive redraws from the editor pane");
        assert!(!frame.contains("\x1b[2J"), "no fullscreen clear needed");
    }

    #[test]
    fn focused_tree_slash_opens_visible_fuzzy_dialog_and_opens_file() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-finder-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(temp.join("src")).unwrap();
        std::fs::write(temp.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(temp.join("README.md"), "# Example").unwrap();

        let mut editor = Editor::new(Document::empty());
        let mut tree = TreePlugin::new(&temp).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        let initial = paint(&mut editor);
        assert!(initial.contains("FILES"));

        press(&mut editor, KeyCode::Char('/'));
        let opened = paint(&mut editor);
        assert!(opened.contains("FIND FILES"), "finder must visibly open");
        assert!(opened.contains("README.md"), "unfiltered project files must be listed");
        assert!(!opened.contains("\x1b[2J"), "search must not erase entire screen");

        // Live fuzzy search should narrow to the nested file.
        for key in "mainrs".chars() {
            press(&mut editor, KeyCode::Char(key));
        }
        let matches = paint(&mut editor);
        assert!(matches.contains("src/main.rs"));
        assert!(matches.contains("mainrs"));
        press(&mut editor, KeyCode::Enter);
        assert_eq!(editor.doc.path.as_deref(), Some(temp.join("src/main.rs").as_path()));
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
        let restored = paint(&mut editor);
        assert!(!restored.contains("FIND FILES"));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn fuzzy_search_esc_closes_only_popup_not_the_tree() {
        let mut editor = Editor::new(Document::from_text(
            &vec!["EDITOR_CONTENT_STAYS_VISIBLE"; 30].join("\n")
        ));
        let mut tree = TreePlugin::new(&std::env::current_dir().unwrap()).unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        paint(&mut editor);
        press(&mut editor, KeyCode::Char('f'));
        assert!(paint(&mut editor).contains("FIND FILES"));
        press(&mut editor, KeyCode::Esc);
        let restored = paint(&mut editor);
        assert!(!restored.contains("FIND FILES"));
        assert!(restored.contains("EDITOR_CONTENT_STAYS_VISIBLE"));
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.visible && tree.focused));
        assert!(!restored.contains("\x1b[2J"));
    }

    #[test]
    fn opening_tree_keeps_editor_buffer_and_renders_sidebar() {
        let mut editor = Editor::new(Document::from_text("UNCHANGED-EDITOR-CONTENT"));
        editor.toggle_tree();
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.visible && tree.focused));
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
        assert!(editor.tree.as_ref().is_some_and(|tree| tree.visible && !tree.focused));
        editor.toggle_tree();
        assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible));
        assert_eq!(editor.doc.line().iter().collect::<String>(), "UNCHANGED-EDITOR-CONTENT");
    }

    #[test]
    fn visual_function_selection_and_yank() {
        let mut doc = Document::from_text("pub fn from_text(text: &str) -> Self {\n  let v = { 42 };\n  Self { v }\n}\n");
        doc.path = Some(PathBuf::from("src/document.rs"));
        doc.row = 1;
        doc.col = 5;
        let mut editor = Editor::new(doc);
        for key in ['v', 'a', 'f'] { press(&mut editor, KeyCode::Char(key)); }
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
        for key in [' ', 'v', 'f'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.mode, Mode::Visual);
        press(&mut editor, KeyCode::Char('d'));
        assert_eq!(editor.mode, Mode::Normal);
        assert!(editor.doc.lines[0].is_empty());
        assert!(editor.doc.lines[1].iter().collect::<String>().contains("fn g"));
        press(&mut editor, KeyCode::Char('u'));
        assert!(editor.doc.lines[0].iter().collect::<String>().starts_with("fn f"));
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
        for key in [' ', 'f', 'f'] { press(&mut editor, KeyCode::Char(key)); }
        assert!(matches!(editor.prompt, Some(Prompt::Open(_))));
        editor.prompt = None;
        for key in [' ', 'f', 's'] { press(&mut editor, KeyCode::Char(key)); }
        assert!(matches!(editor.prompt, Some(Prompt::SaveAs(_))));
    }

    #[test]
    fn yank_delete_paste() {
        let mut editor = Editor::new(Document::from_text("one\ntwo"));
        for key in ['y', 'y', 'j', 'p'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.doc.lines.len(), 3);
        assert_eq!(editor.doc.line().iter().collect::<String>(), "one");
        for key in ['d', 'd'] { press(&mut editor, KeyCode::Char(key)); }
        assert_eq!(editor.doc.lines.len(), 2);
    }

    #[test]
    fn tab_expansion() {
        assert_eq!(visual_column(&"a\tb".chars().collect::<Vec<_>>()), 5);
        assert_eq!(render_line(&"a\tb".chars().collect::<Vec<_>>(), 0, 8), "a   b");
    }

    #[test]
    fn colored_lines_preserve_tabs_and_wide_characters() {
        let line: Vec<char> = "a\t界x".chars().collect();
        let spans = vec![
            Span { start: 0, end: 2, foreground: Color::Blue },
            Span { start: 2, end: 4, foreground: Color::Red },
        ];
        let runs = render_color_line(&line, &spans, 0, 9);
        assert_eq!(runs, vec![
            (Color::Blue, "a   ".into()),
            (Color::Red, "界x".into()),
        ]);
    }

    #[test]
    fn unicode_width() {
        assert_eq!(visual_column(&"世界".chars().collect::<Vec<_>>()), 4);
        assert_eq!(render_line(&"世界".chars().collect::<Vec<_>>(), 1, 3), " 界");
    }
}