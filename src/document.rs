//! A small, Unicode-aware editable document. No upstream MicroEmacs code is used.
use std::{fs, io, path::PathBuf};

use crate::function_select::TextRange;

pub struct Document {
    pub lines: Vec<Vec<char>>,
    pub path: Option<PathBuf>,
    pub row: usize,
    pub col: usize,
    pub dirty: bool,
    pub revision: u64,
    newline: &'static str,
    syntax_invalid_from: Option<usize>,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text("")
    }

    pub fn from_text(text: &str) -> Self {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let lines = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).chars().collect())
            .collect();
        Self {
            lines,
            path: None,
            row: 0,
            col: 0,
            dirty: false,
            revision: 0,
            newline,
            syntax_invalid_from: None,
        }
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
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no filename specified"))?;
        fs::write(path, self.lsp_text())?;
        self.dirty = false;
        Ok(())
    }

    /// Serialize only when the LSP client observes a changed revision.
    pub fn lsp_text(&self) -> String {
        let mut text = String::new();
        for (row, line) in self.lines.iter().enumerate() {
            if row > 0 {
                text.push_str(self.newline);
            }
            text.extend(line.iter());
        }
        text
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

    pub fn home(&mut self) {
        self.col = 0;
    }
    pub fn end(&mut self) {
        self.col = self.line().len();
    }

    pub fn invalidate_syntax_from(&mut self, row: usize) {
        self.revision = self.revision.wrapping_add(1);
        self.syntax_invalid_from = Some(self.syntax_invalid_from.map_or(row, |old| old.min(row)));
    }

    pub fn take_syntax_invalidation(&mut self) -> Option<usize> {
        self.syntax_invalid_from.take()
    }

    pub fn insert(&mut self, ch: char) {
        self.invalidate_syntax_from(self.row);
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
            self.invalidate_syntax_from(self.row);
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            self.invalidate_syntax_from(self.row - 1);
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line().len();
            self.lines[self.row].extend(tail);
        } else {
            return;
        }
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        if self.col < self.line().len() {
            self.invalidate_syntax_from(self.row);
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            self.invalidate_syntax_from(self.row);
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(tail);
        } else {
            return;
        }
        self.dirty = true;
    }

    pub fn delete_line(&mut self) -> Vec<char> {
        self.invalidate_syntax_from(self.row.min(self.lines.len().saturating_sub(2)));
        let deleted = if self.lines.len() > 1 {
            let line = self.lines.remove(self.row);
            self.row = self.row.min(self.lines.len() - 1);
            line
        } else {
            std::mem::take(&mut self.lines[0])
        };
        self.col = 0;
        self.dirty = true;
        deleted
    }

    pub fn insert_line_below(&mut self, line: Vec<char>) {
        self.invalidate_syntax_from(self.row + 1);
        self.row += 1;
        self.lines.insert(self.row, line);
        self.col = 0;
        self.dirty = true;
    }

    pub fn insert_line_above(&mut self, line: Vec<char>) {
        self.invalidate_syntax_from(self.row);
        self.lines.insert(self.row, line);
        self.col = 0;
        self.dirty = true;
    }

    pub fn word_forward(&mut self) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut row = self.row;
        let mut col = self.col;
        if col < self.lines[row].len() && is_word(self.lines[row][col]) {
            while col < self.lines[row].len() && is_word(self.lines[row][col]) {
                col += 1;
            }
        }
        loop {
            while col < self.lines[row].len() {
                if is_word(self.lines[row][col]) {
                    self.row = row;
                    self.col = col;
                    return;
                }
                col += 1;
            }
            if row + 1 >= self.lines.len() {
                self.row = row;
                self.col = self.lines[row].len().saturating_sub(1);
                return;
            }
            row += 1;
            col = 0;
        }
    }

    pub fn word_backward(&mut self) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut row = self.row;
        let mut col = self.col;
        loop {
            if col == 0 {
                if row == 0 {
                    self.row = row;
                    self.col = col;
                    return;
                }
                row -= 1;
                col = self.lines[row].len();
                continue;
            }
            col -= 1;
            while col > 0 && !is_word(self.lines[row][col]) {
                col -= 1;
            }
            if is_word(self.lines[row][col]) {
                while col > 0 && is_word(self.lines[row][col - 1]) {
                    col -= 1;
                }
                self.row = row;
                self.col = col;
                return;
            }
            if row == 0 {
                self.row = 0;
                self.col = 0;
                return;
            }
            col = 0;
        }
    }

    pub fn text_in_range(&self, range: TextRange) -> String {
        if range.start >= range.end || range.end.row >= self.lines.len() {
            return String::new();
        }
        let mut output = String::new();
        for row in range.start.row..=range.end.row {
            if row > range.start.row {
                output.push('\n');
            }
            let line = &self.lines[row];
            let from = if row == range.start.row {
                range.start.col
            } else {
                0
            };
            let to = if row == range.end.row {
                range.end.col
            } else {
                line.len()
            };
            if from <= to && to <= line.len() {
                output.extend(line[from..to].iter());
            }
        }
        output
    }

    pub fn delete_text_range(&mut self, range: TextRange) {
        if range.start >= range.end
            || range.end.row >= self.lines.len()
            || range.start.col > self.lines[range.start.row].len()
            || range.end.col > self.lines[range.end.row].len()
        {
            return;
        }
        let mut joined = self.lines[range.start.row][..range.start.col].to_vec();
        joined.extend_from_slice(&self.lines[range.end.row][range.end.col..]);
        self.lines
            .splice(range.start.row..=range.end.row, std::iter::once(joined));
        self.row = range.start.row;
        self.col = range.start.col;
        self.dirty = true;
        self.invalidate_syntax_from(range.start.row);
    }

    /// Insert a paste as one document change. Splice the surrounding text only
    /// once so inserting a large paste before a long suffix stays linear.
    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut inserted = text
            .split('\n')
            .map(|line| line.chars().collect::<Vec<_>>());
        let first = inserted.next().expect("split always yields a first line");
        let first_len = first.len();
        let mut following: Vec<Vec<char>> = inserted.collect();
        let start_row = self.row;
        if following.is_empty() {
            self.lines[self.row].splice(self.col..self.col, first);
            self.col += first_len;
        } else {
            let tail = self.lines[self.row].split_off(self.col);
            self.lines[self.row].extend(first);
            self.row += following.len();
            let last = following.last_mut().expect("following lines are nonempty");
            self.col = last.len();
            last.extend(tail);
            self.lines.splice(start_row + 1..start_row + 1, following);
        }
        self.dirty = true;
        self.invalidate_syntax_from(start_row);
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
    fn bulk_insert_preserves_unicode_prefix_suffix_and_cursor() {
        let mut doc = Document::from_text("α世界ω\nnext");
        doc.col = 1;
        doc.insert_text("hé\nλ\n");
        assert_eq!(doc.lsp_text(), "αhé\nλ\n世界ω\nnext");
        assert_eq!((doc.row, doc.col), (2, 0));
        assert_eq!(doc.revision, 1);
        assert_eq!(doc.take_syntax_invalidation(), Some(0));
        assert!(doc.dirty);
    }

    #[test]
    fn bulk_insert_matches_character_edits() {
        for text in ["hello", "世界", "\n", "a\nb", "\n\nlast", "end\n"] {
            let mut bulk = Document::from_text("start\nprefixsuffix\nend");
            bulk.row = 1;
            bulk.col = 6;
            let mut individual = Document::from_text("start\nprefixsuffix\nend");
            individual.row = 1;
            individual.col = 6;
            bulk.insert_text(text);
            for ch in text.chars() {
                individual.insert(ch);
            }
            assert_eq!(bulk.lines, individual.lines, "{text:?}");
            assert_eq!(
                (bulk.row, bulk.col),
                (individual.row, individual.col),
                "{text:?}"
            );
            assert_eq!(bulk.revision, 1);
        }
    }

    #[test]
    fn empty_paste_leaves_document_unchanged() {
        let mut doc = Document::from_text("abc");
        doc.col = 1;
        doc.insert_text("");
        assert_eq!(doc.lsp_text(), "abc");
        assert_eq!((doc.row, doc.col), (0, 1));
        assert!(!doc.dirty);
        assert_eq!(doc.revision, 0);
        assert_eq!(doc.take_syntax_invalidation(), None);
    }

    #[test]
    fn delete_joins_lines_and_removes_characters() {
        let mut doc = Document::from_text("abc\ndef");
        doc.end();
        doc.delete();
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
        assert_eq!(
            doc.lines
                .iter()
                .map(|line| line.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join(doc.newline),
            "one\r\ntwo\r\n"
        );
    }
}
