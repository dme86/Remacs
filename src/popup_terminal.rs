//! Small PTY host for the Lua toggleterm plugin.
//! The Lua plugin owns mappings, commands and visibility. Rust merely launches
//! the command in a real TTY, decodes VT escape sequences and draws a popup.
use std::{
    io::{self, Read, Write},
    num::NonZeroU16,
    path::Path,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

use crossterm::{
    cursor::MoveTo,
    event::{KeyCode, KeyEvent, KeyModifiers},
    queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize, PtySystem};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const BG: Color = Color::Rgb { r: 33, g: 34, b: 41 };
const BORDER: Color = Color::Rgb { r: 215, g: 153, b: 33 };

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn inner(self) -> (u16, u16) {
        (self.height - 2, self.width - 2)
    }
}

pub fn popup_rect(cols: u16, editor_rows: usize) -> Option<Rect> {
    if cols < 54 || editor_rows < 14 { return None; }
    let width = ((cols as usize * 9 / 10).max(54)).min(cols as usize - 2) as u16;
    let height = ((editor_rows * 4 / 5).max(12)).min(editor_rows - 2) as u16;
    Some(Rect {
        x: (cols - width) / 2,
        y: ((editor_rows - height as usize) / 2) as u16,
        width,
        height,
    })
}

pub struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send>,
    writer: Box<dyn Write + Send>,
    receiver: Receiver<Vec<u8>>,
    pub parser: vt100::Parser,
    pub title: String,
    size: (u16, u16), // rows, columns
}

impl Session {
    pub fn spawn(command: &str, cwd: &Path, title: String, rect: Rect) -> io::Result<Self> {
        if !cwd.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "project directory missing"));
        }
        let (rows, cols) = rect.inner();
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size).map_err(io::Error::other)?;
        // No shell interpolation: the Lua plugin chooses one executable.
        // The "shell" sentinel selects the user's preferred login shell.
        let binary = if command == "shell" {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        } else {
            command.to_owned()
        };
        let mut builder = CommandBuilder::new(binary);
        builder.cwd(cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(builder).map_err(io::Error::other)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        // Bounded channel avoids unbounded memory growth if a hidden shell
        // produces a lot of output. Poll continuously even while hidden.
        let (tx, receiver) = mpsc::sync_channel(64);
        thread::Builder::new().name("remacs-toggleterm-pty".into()).spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() { break; }
            }
        })?;
        Ok(Self {
            master: pair.master,
            child,
            writer,
            receiver,
            parser: vt100::Parser::new(
                NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap(), 1000
            ),
            title,
            size: (rows, cols),
        })
    }

    pub fn resize(&mut self, rect: Rect) -> io::Result<()> {
        let size = rect.inner();
        if self.size == size { return Ok(()); }
        self.master.resize(PtySize {
            rows: size.0, cols: size.1, pixel_width: 0, pixel_height: 0
        }).map_err(io::Error::other)?;
        self.parser.set_size(
            NonZeroU16::new(size.0).unwrap(), NonZeroU16::new(size.1).unwrap()
        );
        self.size = size;
        Ok(())
    }

    /// Process a limited number of chunks to avoid starving keyboard input.
    pub fn poll(&mut self) -> io::Result<bool> {
        for _ in 0..64 {
            match self.receiver.try_recv() {
                Ok(bytes) => self.parser.process(&bytes),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        Ok(self.child.try_wait()?.is_some())
    }

    pub fn input(&mut self, event: KeyEvent) -> io::Result<()> {
        let encoded = encode_key(event, self.parser.screen().application_cursor());
        if !encoded.is_empty() {
            self.writer.write_all(&encoded)?;
            self.writer.flush()?;
        }
        Ok(())
    }

    pub fn cursor(&self, rect: Rect) -> (u16, u16) {
        let (row, col) = self.parser.screen().cursor_position();
        (rect.x + 1 + col.min(rect.width - 3), rect.y + 1 + row.min(rect.height - 3))
    }
}

impl Drop for Session {
    fn drop(&mut self) { let _ = self.child.kill(); }
}

/// Encode the keys needed by interactive full-screen terminal programs.
/// Esc remains a normal child key; Ctrl+T to hide is dispatched by the host
/// before reaching this function.
pub fn encode_key(event: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);
    let code = match event.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() =>
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
        KeyCode::Char(' ') if ctrl => vec![0],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab if shift => b"\x1b[Z".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => match (application_cursor, ctrl, alt) {
            (_, true, _) => b"\x1b[1;5A".to_vec(),
            (_, _, true) => b"\x1b[1;3A".to_vec(),
            (true, _, _) => b"\x1bOA".to_vec(),
            _ => b"\x1b[A".to_vec(),
        },
        KeyCode::Down => match (application_cursor, ctrl, alt) {
            (_, true, _) => b"\x1b[1;5B".to_vec(),
            (_, _, true) => b"\x1b[1;3B".to_vec(),
            (true, _, _) => b"\x1bOB".to_vec(),
            _ => b"\x1b[B".to_vec(),
        },
        KeyCode::Right => if ctrl { b"\x1b[1;5C".to_vec() } else { b"\x1b[C".to_vec() },
        KeyCode::Left => if ctrl { b"\x1b[1;5D".to_vec() } else { b"\x1b[D".to_vec() },
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(n) if (1..=4).contains(&n) => vec![0x1b, b'O', b'P' + n - 1],
        KeyCode::F(n) if (5..=12).contains(&n) => {
            format!("\x1b[{}~", [15,17,18,19,20,21,23,24][(n-5) as usize]).into_bytes()
        }
        _ => Vec::new(),
    };
    if alt && !code.is_empty() && !matches!(event.code, KeyCode::Up | KeyCode::Down) {
        let mut out = vec![0x1b];
        out.extend_from_slice(&code);
        out
    } else { code }
}

#[derive(Default)]
pub struct PopupCache {
    rect: Option<Rect>,
    rows: Vec<Option<String>>,
}

impl PopupCache {
    pub fn set_rect(&mut self, next: Option<Rect>) -> Option<Rect> {
        if self.rect == next { return None; }
        let old = self.rect;
        self.rect = next;
        self.rows.clear();
        old
    }

    pub fn invalidate_underpaint(&mut self, x: usize, y: usize, width: usize) {
        let Some(rect) = self.rect else { return; };
        if width == 0 || x >= (rect.x + rect.width) as usize
            || x.saturating_add(width) <= rect.x as usize { return; }
        if let Some(local) = y.checked_sub(rect.y as usize) {
            if let Some(row) = self.rows.get_mut(local) { *row = None; }
        }
    }

    pub fn paint(&mut self, out: &mut impl Write, rect: Rect, session: &Session) -> io::Result<()> {
        self.rows.resize_with(rect.height as usize, || None);
        let interior = (rect.width - 2) as usize;
        let title = format!("─ {} ", session.title);
        for i in 0..rect.height as usize {
            let line = if i == 0 {
                let text = fit(&title, interior);
                format!("╭{text}╮")
            } else if i == rect.height as usize - 1 {
                format!("╰{}╯", "─".repeat(interior))
            } else {
                // vt100 provides per-row content with ANSI SGR colors.
                let screen_row = (i - 1) as usize;
                let text = session.parser.screen().rows_formatted(0, rect.width - 2)
                    .nth(screen_row).unwrap_or_default();
                // Keep each cell inside the popup. Rows are exactly the PTY
                // width; redraw the background before emitting colored cells.
                format!("│\x1b[0m{BG_SGR}{text}\x1b[0m{BG_SGR}│")
            };
            if self.rows[i].as_ref() == Some(&line) { continue; }
            let (fg, bg) = if i == 0 || i == rect.height as usize - 1 {
                (BORDER, BG)
            } else {
                (Color::Reset, BG)
            };
            queue!(out,
                MoveTo(rect.x, rect.y + i as u16),
                SetBackgroundColor(bg),
                SetForegroundColor(fg),
                Print(&line),
                ResetColor
            )?;
            self.rows[i] = Some(line);
        }
        Ok(())
    }
}

// ANSI SGR background to match the floating-terminal surface after a
// child row resets its attributes; no cursor-addressing escapes are accepted.
const BG_SGR: &str = "\x1b[48;2;33;34;41m";

fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for ch in text.chars() {
        let cells = UnicodeWidthChar::width(ch).unwrap_or(0);
        if n + cells > width { break; }
        n += cells;
        out.push(ch);
    }
    out.push_str(&"─".repeat(width - n));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn popup_bounds_and_dimensions() {
        let rect = popup_rect(120, 37).unwrap();
        assert!(rect.x > 0 && rect.y > 0);
        assert!(rect.x + rect.width <= 120);
        assert!(rect.y as usize + rect.height as usize <= 37);
        assert!(rect.inner().0 >= 10);
        assert_eq!(popup_rect(50, 38), None);
        assert_eq!(popup_rect(90, 12), None);
    }

    #[test]
    fn arrow_and_control_keys_are_encoded_for_tui_children() {
        let key = |code, mods| encode_key(KeyEvent::new(code, mods), false);
        assert_eq!(key(KeyCode::Char('j'), KeyModifiers::NONE), b"j");
        assert_eq!(key(KeyCode::Char('g'), KeyModifiers::CONTROL), b"\x07");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(key(KeyCode::Esc, KeyModifiers::NONE), b"\x1b");
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Char('j'), KeyModifiers::ALT), b"\x1bj");
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::NONE), b"\x7f");
        assert_eq!(key(KeyCode::Tab, KeyModifiers::SHIFT), b"\x1b[Z");
    }

    #[test]
    fn vt100_handles_lazygit_style_sgr_and_cursor_control() {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(12).unwrap(), NonZeroU16::new(80).unwrap(), 32
        );
        parser.process(b"\x1b[H\x1b[31mStage changes\x1b[0m\r\n");
        assert!(parser.screen().contents().contains("Stage changes"));
        assert!(parser.screen().rows_formatted(0, 80).next().unwrap().contains("Stage changes"));
    }
}
