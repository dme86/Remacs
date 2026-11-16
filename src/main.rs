//! Remacs: a tiny, terminal-only, Emacs-inspired text editor.
mod document;

use std::{env, io::{self, Write}, path::PathBuf};
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

enum Prompt {
    SaveAs(String),
    Open(String),
    Quit,
}

struct Editor {
    doc: Document,
    scroll_row: usize,
    scroll_col: usize,
    control_x: bool,
    prompt: Option<Prompt>,
    message: String,
    yank: String,
    running: bool,
}

impl Editor {
    fn new(doc: Document) -> Self {
        Self {
            doc, scroll_row: 0, scroll_col: 0, control_x: false,
            prompt: None, message: "C-x C-f Open  |  C-x C-s Save  |  C-x C-c Quit".into(),
            yank: String::new(), running: true,
        }
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
                self.message = format!("Opened {name}");
            }
            Err(error) => self.message = format!("Open error: {error}"),
        }
    }

    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.take() else { return; };
        if matches!(key.code, KeyCode::Esc)
            || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            self.message = "Cancelled".into();
            return;
        }
        match prompt {
            Prompt::Quit => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.running = false,
                KeyCode::Char('n') | KeyCode::Char('N') => self.message = "Quit cancelled".into(),
                _ => self.prompt = Some(Prompt::Quit),
            },
            Prompt::SaveAs(mut name) => match key.code {
                KeyCode::Enter if !name.is_empty() => {
                    self.doc.path = Some(PathBuf::from(name));
                    self.save();
                }
                KeyCode::Backspace => { name.pop(); self.prompt = Some(Prompt::SaveAs(name)); }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    name.push(c); self.prompt = Some(Prompt::SaveAs(name));
                }
                _ => self.prompt = Some(Prompt::SaveAs(name)),
            },
            Prompt::Open(mut name) => match key.code {
                KeyCode::Enter if !name.is_empty() => self.open(name),
                KeyCode::Backspace => { name.pop(); self.prompt = Some(Prompt::Open(name)); }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    name.push(c); self.prompt = Some(Prompt::Open(name));
                }
                _ => self.prompt = Some(Prompt::Open(name)),
            },
        }
    }

    fn key(&mut self, key: KeyEvent, page_height: usize) {
        if self.prompt.is_some() {
            self.prompt_key(key);
            return;
        }
        if self.control_x {
            self.control_x = false;
            match key.code {
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => self.save(),
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.prompt = Some(Prompt::Open(String::new()));
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if self.doc.dirty { self.prompt = Some(Prompt::Quit); }
                    else { self.running = false; }
                }
                _ => self.message = "Unknown C-x sequence (C-g cancels)".into(),
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('x') => { self.control_x = true; self.message = "C-x".into(); }
                KeyCode::Char('g') => self.message = "Cancelled".into(),
                KeyCode::Char('a') => self.doc.home(),
                KeyCode::Char('e') => self.doc.end(),
                KeyCode::Char('b') => self.doc.left(),
                KeyCode::Char('f') => self.doc.right(),
                KeyCode::Char('p') => self.doc.up(),
                KeyCode::Char('n') => self.doc.down(),
                KeyCode::Char('d') => self.doc.delete(),
                KeyCode::Char('k') => {
                    if let Some(text) = self.doc.kill_to_end() { self.yank = text; }
                }
                KeyCode::Char('y') => self.doc.insert_text(&self.yank.clone()),
                KeyCode::Char('v') => self.page_down(page_height),
                _ => {},
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            if key.code == KeyCode::Char('v') { self.page_up(page_height); }
            return;
        }
        match key.code {
            KeyCode::Left => self.doc.left(),
            KeyCode::Right => self.doc.right(),
            KeyCode::Up => self.doc.up(),
            KeyCode::Down => self.doc.down(),
            KeyCode::Home => self.doc.home(),
            KeyCode::End => self.doc.end(),
            KeyCode::PageUp => self.page_up(page_height),
            KeyCode::PageDown => self.page_down(page_height),
            KeyCode::Backspace => self.doc.backspace(),
            KeyCode::Delete => self.doc.delete(),
            KeyCode::Enter => self.doc.insert('\n'),
            KeyCode::Tab => self.doc.insert('\t'),
            KeyCode::Char(c) => self.doc.insert(c),
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
                let visible = render_line(line, self.scroll_col, width);
                queue!(output, MoveTo(0, y as u16), Print(visible))?;
            }
        }
        if rows >= 2 {
            let filename = self.doc.path.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[No Name]".into());
            let mark = if self.doc.dirty { "**" } else { "--" };
            let status = format!(" {mark} Remacs | {filename} | Ln {}, Col {} ", self.doc.row + 1, self.doc.col + 1);
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
                Some(Prompt::Quit) => "Discard changes and quit? (y/n)".into(),
                None if self.control_x => "C-x".into(),
                None => self.message.clone(),
            };
            queue!(output, MoveTo(0, rows - 1), Print(fit_cells(&message, width)))?;
        }
        let (x, y) = match &self.prompt {
            Some(Prompt::SaveAs(name)) => ("Save file: ".len() + name.chars().count(), rows.saturating_sub(1)),
            Some(Prompt::Open(name)) => ("Find file: ".len() + name.chars().count(), rows.saturating_sub(1)),
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
        println!("Remacs — minimal terminal editor\nUsage: remacs [file]\nSave: C-x C-s | Open: C-x C-f | Quit: C-x C-c");
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
