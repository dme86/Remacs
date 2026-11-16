//! A small, Unicode-aware editable document. No upstream MicroEmacs code is used.
use std::{fs, io, path::PathBuf};

pub struct Document {
    pub lines: Vec<Vec<char>>,
    pub path: Option<PathBuf>,
    pub row: usize,
    pub col: usize,
    pub dirty: bool,
    newline: &'static str,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text("")
    }

    pub fn from_text(text: &str) -> Self {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let lines = text.split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).chars().collect())
            .collect();
        Self { lines, path: None, row: 0, col: 0, dirty: false, newline }
    }

    pub fn open(path: PathBuf) -> io::Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut document = Self::from_text(&text);
        document.path = Some(path);
        Ok(document)
    }

    pub fn save(&mut self) -> io::Result<()> {
        let path = self.path.as_ref().ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput, "no filename specified"
        ))?;
        let contents = self.lines.iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(self.newline);
        fs::write(path, contents)?;
        self.dirty = false;
        Ok(())
    }

    pub fn line(&self) -> &[char] {
        &self.lines[self.row]
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line().len();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.line().len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        self.row = self.row.saturating_sub(1);
        self.col = self.col.min(self.line().len());
    }

    pub fn down(&mut self) {
        self.row = (self.row + 1).min(self.lines.len() - 1);
        self.col = self.col.min(self.line().len());
    }

    pub fn home(&mut self) { self.col = 0; }
    pub fn end(&mut self) { self.col = self.line().len(); }

    pub fn insert(&mut self, ch: char) {
        if ch == '\n' {
            let tail = self.lines[self.row].split_off(self.col);
            self.row += 1;
            self.lines.insert(self.row, tail);
            self.col = 0;
        } else {
            self.lines[self.row].insert(self.col, ch);
            self.col += 1;
        }
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line().len();
            self.lines[self.row].extend(tail);
        } else { return; }
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        if self.col < self.line().len() {
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(tail);
        } else { return; }
        self.dirty = true;
    }

    pub fn kill_to_end(&mut self) -> Option<String> {
        if self.col < self.line().len() {
            let text: String = self.lines[self.row].drain(self.col..).collect();
            self.dirty = true;
            Some(text)
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
            self.dirty = true;
            Some("\n".to_string())
        } else { None }
    }

    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars() { self.insert(ch); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_unicode() {
        let mut doc = Document::empty();
        doc.insert_text("héllo\n世界");
        assert_eq!(doc.row, 1);
        assert_eq!(doc.col, 2);
        doc.home();
        doc.backspace();
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].iter().collect::<String>(), "héllo世界");
    }

    #[test]
    fn delete_and_kill() {
        let mut doc = Document::from_text("abc\ndef");
        doc.end();
        assert_eq!(doc.kill_to_end().as_deref(), Some("\n"));
        assert_eq!(doc.lines[0].iter().collect::<String>(), "abcdef");
        doc.home();
        doc.delete();
        assert_eq!(doc.lines[0].iter().collect::<String>(), "bcdef");
    }

    #[test]
    fn newline_roundtrip() {
        let doc = Document::from_text("one\r\ntwo\r\n");
        assert_eq!(doc.lines.len(), 3);
        assert_eq!(doc.newline, "\r\n");
        assert_eq!(doc.lines.iter().map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>().join(doc.newline), "one\r\ntwo\r\n");
    }
}
