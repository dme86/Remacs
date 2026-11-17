//! Remacs: a minimal terminal editor with Vim-style modes and Doom-inspired leader keys.
mod document;
mod function_select;
mod plugins;
mod syntax;

use std::{
    env,
    io::{self, Write},
    path::PathBuf,
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use document::Document;
use function_select::{around_function, Position, TextRange};
use plugins::{TreeAction, TreePlugin, TreeView};
use syntax::{Span, SyntaxEngine};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TAB_WIDTH: usize = 4;
const MAX_UNDO: usize = 100;
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

struct Editor {
    doc: Document,
    syntax: SyntaxEngine,
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

    fn remember(&mut self) {
        if self.undo.len() == MAX_UNDO {
            self.undo.remove(0);
        }
        self.undo.push(Snapshot::capture(&self.doc));
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
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => "ctrl-n".into(),
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
                if !self.doc.dirty {
                    let filename = path.to_string_lossy().into_owned();
                    self.open(filename);
                    if self.doc.path.as_ref() == Some(&path) {
                        if let Some(tree) = &mut self.tree { tree.leave(); }
                    }
                } else {
                    self.message = "Unsaved changes: save before opening a tree file".into();
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

    fn open(&mut self, name: String) {
        if self.doc.dirty {
            self.message = "Unsaved changes: save before opening another file".into();
            return;
        }
        match Document::open(PathBuf::from(&name)) {
            Ok(doc) => {
                self.doc = doc;
                self.syntax = SyntaxEngine::new(&self.doc);
                self.scroll_row = 0;
                self.scroll_col = 0;
                self.count = None;
                self.mode = Mode::Normal;
                self.selection = None;
                self.visual_anchor = None;
                self.undo.clear();
                self.redo.clear();
                self.message = format!("Opened {name}");
            }
            Err(error) => self.message = format!("Open error: {error}"),
        }
    }

    fn quit(&mut self) {
        if self.doc.dirty {
            self.prompt = Some(Prompt::Quit);
        } else {
            self.running = false;
        }
    }

    fn command(&mut self, command: &str) {
        match command.trim() {
            "w" => self.save(),
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
            "wq" | "x" => {
                if self.doc.path.is_some() {
                    self.save();
                    if !self.doc.dirty {
                        self.running = false;
                    }
                } else {
                    self.message = "Choose a filename with :w or Space f s first".into();
                }
            }
            value if value.starts_with("e ") => self.open(value[2..].trim().to_owned()),
            value if value.starts_with("w ") => {
                let name = value[2..].trim();
                if !name.is_empty() {
                    self.doc.path = Some(PathBuf::from(name));
                    self.save();
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
                    self.doc.path = Some(PathBuf::from(name));
                    self.save();
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
        // The explorer owns its keys when focused, including Ctrl+N for
        // moving through fuzzy results. Outside it, Ctrl+N toggles the pane.
        if self.tree.as_ref().is_some_and(|tree| tree.focused) {
            self.tree_key(key);
            return;
        }
        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_tree();
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
            if self.mode == Mode::Insert {
                self.leave_insert();
            }
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
        self.normal_key(key, page_height);
    }

    fn normal_key(&mut self, key: KeyEvent, page_height: usize) {
        if self.sequence != Sequence::None {
            let sequence = std::mem::take(&mut self.sequence);
            let count = self.count.take();
            match (sequence, key.code) {
                (Sequence::Leader, KeyCode::Char('f')) => self.sequence = Sequence::File,
                (Sequence::Leader, KeyCode::Char('o')) => self.sequence = Sequence::Panel,
                (Sequence::Leader, KeyCode::Char('q')) => self.sequence = Sequence::Quit,
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

        let mut selected_tree_row = 0;
        let mut search_cursor: Option<usize> = None;
        if sidebar_width > 0 {
            let view = match self.tree.as_ref().map(|tree| tree.view(height.saturating_sub(1))) {
                Some(Ok(view)) => Some(view),
                Some(Err(error)) => {
                    self.message = format!("Tree render error: {error}");
                    None
                }
                None => None,
            };
            if let Some(view) = view {
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
            let gutter_changed = old.is_none_or(|painted| painted.gutter != number);
            let syntax_stale = self.syntax.enabled()
                && syntax_invalid.is_some_and(|first| index >= first);
            let paint_text = repaint_all || text_stale || highlight_changed || syntax_stale;

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
            }
            if paint_text {
                queue!(output, MoveTo((sidebar_width + gutter_width) as u16, y as u16), ResetColor)?;
                if active {
                    queue!(output, SetBackgroundColor(CURRENT_LINE_BG))?;
                }

                let mut visible_width = 0;
                if let Some(line) = line {
                    if let Some(spans) = self.syntax.spans(&self.doc, index).filter(|s| !s.is_empty()) {
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
                        SetForegroundColor(Color::Reset),
                        Print(" ".repeat(text_width.saturating_sub(visible_width))),
                        ResetColor
                    )?;
                } else {
                    // Remove any tail left by a previously longer line.
                    queue!(output, ResetColor, Clear(ClearType::UntilNewLine))?;
                }
            }
            self.screen.lines[y] = Some(PaintedLine {
                index,
                content: line.cloned(),
                gutter: number,
                active,
            });
        }

        if rows >= 2 {
            let filename = self.doc.path.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[No Name]".into());
            let mark = if self.doc.dirty { "**" } else { "--" };
            let mode = match self.mode { Mode::Normal => "NORMAL", Mode::Insert => "INSERT" };
            let language = if self.syntax.enabled() { self.syntax.name() } else { "Highlight off" };
            let pane = if self.tree.as_ref().is_some_and(|tree| tree.visible) { " | TREE" } else { "" };
            let status = format!(" {mark} Remacs [{mode}] | {filename} | {language}{pane} | Ln {}, Col {} ", self.doc.row + 1, self.doc.col + 1);
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
                Some(Prompt::Quit) => "Discard changes and quit? (y/n)".into(),
                None if self.control_x => "C-x".into(),
                None if self.tree.as_ref().is_some_and(|tree| tree.focused) =>
                    "TREE  / or f Find  j/k Move  l/Enter Open  . Hidden  r Refresh  Tab/Esc Editor  q Close".into(),
                None => match self.sequence {
                    Sequence::Leader => "SPC → f Files | o Panels | q Quit".into(),
                    Sequence::File => "SPC f → f Find | s Save | t Tree".into(),
                    Sequence::Panel => "SPC o → p Project tree".into(),
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
        let (x, y) = match &self.prompt {
            Some(Prompt::SaveAs(name)) => ("Save file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Open(name)) => ("Find file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Command(command)) => (1 + command.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Quit) => (0, rows.saturating_sub(1)),
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