//! Remacs: a minimal terminal editor with Vim-style modes and Doom-inspired leader keys.
mod document;

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
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;
const MAX_UNDO: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Sequence {
    #[default]
    None,
    Leader,
    File,
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
    }
}

#[derive(Clone)]
enum Register {
    Text(String),
    Line(Vec<char>),
}

struct Editor {
    doc: Document,
    scroll_row: usize,
    scroll_col: usize,
    mode: Mode,
    sequence: Sequence,
    control_x: bool,
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
        Self {
            doc,
            scroll_row: 0,
            scroll_col: 0,
            mode: Mode::Normal,
            sequence: Sequence::None,
            control_x: false,
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
                self.scroll_row = 0;
                self.scroll_col = 0;
                self.mode = Mode::Normal;
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
        if key.code == KeyCode::Esc {
            if self.mode == Mode::Insert {
                self.leave_insert();
            }
            self.sequence = Sequence::None;
            self.control_x = false;
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
                KeyCode::Char('g') => { self.sequence = Sequence::None; self.message = "Cancelled".into(); }
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
            match (sequence, key.code) {
                (Sequence::Leader, KeyCode::Char('f')) => self.sequence = Sequence::File,
                (Sequence::Leader, KeyCode::Char('q')) => self.sequence = Sequence::Quit,
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
                (Sequence::Go, KeyCode::Char('g')) => { self.doc.row = 0; self.doc.home(); }
                _ => self.message = "Unknown key sequence".into(),
            }
            return;
        }
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
                self.doc.col = self.doc.col.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.doc.col + 1 < self.doc.line().len() { self.doc.col += 1; }
            }
            KeyCode::Char('j') | KeyCode::Down => self.doc.down(),
            KeyCode::Char('k') | KeyCode::Up => self.doc.up(),
            KeyCode::Char('0') | KeyCode::Home => self.doc.home(),
            KeyCode::Char('$') | KeyCode::End => {
                self.doc.end();
                self.doc.col = self.doc.col.saturating_sub(1);
            }
            KeyCode::Char('w') => self.doc.word_forward(),
            KeyCode::Char('b') => self.doc.word_backward(),
            KeyCode::Char('g') => self.sequence = Sequence::Go,
            KeyCode::Char('G') => {
                self.doc.row = self.doc.lines.len() - 1;
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
        let width = cols as usize;
        let height = rows.saturating_sub(2) as usize;
        self.ensure_visible(width, height);
        queue!(output, Hide, ResetColor, MoveTo(0, 0), Clear(ClearType::All))?;
        for y in 0..height {
            let index = y + self.scroll_row;
            if let Some(line) = self.doc.lines.get(index) {
                queue!(output, MoveTo(0, y as u16), Print(render_line(line, self.scroll_col, width)))?;
            }
        }
        if rows >= 2 {
            let filename = self.doc.path.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[No Name]".into());
            let mark = if self.doc.dirty { "**" } else { "--" };
            let mode = match self.mode { Mode::Normal => "NORMAL", Mode::Insert => "INSERT" };
            let status = format!(" {mark} Remacs [{mode}] | {filename} | Ln {}, Col {} ", self.doc.row + 1, self.doc.col + 1);
            queue!(output,
                MoveTo(0, rows - 2),
                SetBackgroundColor(Color::DarkGrey), SetForegroundColor(Color::White),
                Print(fit_cells(&status, width)), ResetColor
            )?;
        }
        if rows >= 1 {
            let message = match &self.prompt {
                Some(Prompt::SaveAs(name)) => format!("Save file: {name}"),
                Some(Prompt::Open(name)) => format!("Find file: {name}"),
                Some(Prompt::Command(command)) => format!(":{command}"),
                Some(Prompt::Quit) => "Discard changes and quit? (y/n)".into(),
                None if self.control_x => "C-x".into(),
                None => match self.sequence {
                    Sequence::Leader => "SPC → f Files | q Quit".into(),
                    Sequence::File => "SPC f → f Find file | s Save".into(),
                    Sequence::Quit => "SPC q → q Quit".into(),
                    Sequence::Delete => "d → d Delete line".into(),
                    Sequence::Yank => "y → y Yank line".into(),
                    Sequence::Go => "g → g First line".into(),
                    Sequence::None => self.message.clone(),
                },
            };
            queue!(output, MoveTo(0, rows - 1), Print(fit_cells(&message, width)))?;
        }
        let (x, y) = match &self.prompt {
            Some(Prompt::SaveAs(name)) => ("Save file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Open(name)) => ("Find file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Command(command)) => (1 + command.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Quit) => (0, rows.saturating_sub(1)),
            None => (
                visual_column(&self.doc.line()[..self.doc.col]).saturating_sub(self.scroll_col),
                self.doc.row.saturating_sub(self.scroll_row) as u16,
            ),
        };
        if cols > 0 && rows > 0 {
            queue!(output, MoveTo((x.min(width - 1)) as u16, y.min(rows - 1)), Show)?;
        }
        output.flush()
    }
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
    fn unicode_width() {
        assert_eq!(visual_column(&"世界".chars().collect::<Vec<_>>()), 4);
        assert_eq!(render_line(&"世界".chars().collect::<Vec<_>>(), 1, 3), " 界");
    }
}