//! A visible, centered fuzzy-file dialog that overlays the editor.
//! The Lua tree owns query/ranking; this module draws only the small popup,
//! avoiding full-screen redraws while typing or moving between results.
use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::plugins::FinderView;

const BG: Color = Color::Rgb { r: 34, g: 35, b: 44 };
const INPUT_BG: Color = Color::Rgb { r: 49, g: 46, b: 64 };
const HIGHLIGHT_BG: Color = Color::Rgb { r: 77, g: 65, b: 99 };
const BORDER_FG: Color = Color::Rgb { r: 153, g: 139, b: 183 };
const TEXT_FG: Color = Color::Rgb { r: 220, g: 221, b: 231 };
const MUTED_FG: Color = Color::Rgb { r: 155, g: 160, b: 177 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: usize,
    pub height: usize,
}

pub fn rect_for(cols: u16, editor_rows: usize) -> Option<Rect> {
    let cols = cols as usize;
    if cols < 22 || editor_rows < 7 {
        return None;
    }
    let width = (cols * 3 / 4).clamp(42, 100).min(cols);
    let height = (editor_rows * 2 / 3).clamp(8, 24).min(editor_rows);
    Some(Rect {
        x: ((cols - width) / 2) as u16,
        y: ((editor_rows - height) / 2) as u16,
        width,
        height,
    })
}

fn pad(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut cells = 0;
    for ch in text.chars() {
        let size = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + size > width { break; }
        result.push(ch);
        cells += size;
    }
    result.extend(std::iter::repeat_n(' ', width.saturating_sub(cells)));
    result
}

// Keep the newest portion of long searches visible near the input cursor.
fn trailing_query(query: &str, cells: usize) -> String {
    let mut current = 0usize;
    let mut chars = Vec::new();
    for ch in query.chars().rev() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current + width > cells { break; }
        chars.push(ch);
        current += width;
    }
    chars.iter().rev().collect()
}

pub fn search_cursor(rect: Rect, query: &str) -> (u16, u16) {
    let visible = trailing_query(query, rect.width.saturating_sub(7));
    let x = rect.x as usize + 5 + UnicodeWidthStr::width(visible.as_str());
    ((x.min(rect.x as usize + rect.width - 2)) as u16, rect.y + 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Files,
    Buffers,
    Hover,
    Mason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Border,
    Input,
    Normal,
    Selected,
    Footer,
}

#[derive(Default)]
pub struct Cache {
    rect: Option<Rect>,
    painted: Vec<Option<(String, Style)>>,
}

impl Cache {
    /// When the dialog disappears or changes geometry, return its old area so
    /// the ordinary editor/sidebar caches can repaint the cells underneath.
    pub fn set_rect(&mut self, next: Option<Rect>) -> Option<Rect> {
        if self.rect == next { return None; }
        let previous = self.rect;
        self.rect = next;
        self.painted.clear();
        previous
    }

    /// A lower layer repainted part of the popup. Invalidate only the
    /// intersecting popup row so the overlay is composited on top again.
    /// Without this, the file tree can overwrite the left edge of cached
    /// popup rows while typing (notably where the panes overlap).
    pub fn invalidate_underpaint(&mut self, x: usize, y: usize, width: usize) {
        if width == 0 { return; }
        let Some(rect) = self.rect else { return; };
        let left = rect.x as usize;
        let right = left + rect.width;
        if x >= right || x.saturating_add(width) <= left {
            return;
        }
        if let Some(relative_y) = y.checked_sub(rect.y as usize) {
            if let Some(row) = self.painted.get_mut(relative_y) {
                *row = None;
            }
        }
    }

    pub fn paint(
        &mut self,
        output: &mut impl Write,
        rect: Rect,
        finder: &FinderView,
        kind: PickerKind,
    ) -> io::Result<()> {
        self.painted.resize_with(rect.height, || None);
        let interior = rect.width.saturating_sub(2);
        let available = rect.height.saturating_sub(5);

        let selected = finder.selected.max(1);
        let start = selected.saturating_sub(available).min(
            finder.matches.len().saturating_sub(available)
        );

        for row in 0..rect.height {
            let (line, style) = if row == 0 {
                let title = match kind {
                    PickerKind::Files => "─ FIND FILES ",
                    PickerKind::Buffers => "─ SWITCH BUFFER ",
                    PickerKind::Hover => "─ LSP HOVER ",
                    PickerKind::Mason => "─ MASON · LANGUAGE SERVERS ",
                };
                // Terminal geometry is measured in cells, not UTF-8 bytes.
                let filler = rect.width.saturating_sub(2 + UnicodeWidthStr::width(title));
                (format!("╭{title}{}╮", "─".repeat(filler)), Style::Border)
            } else if row == 1 {
                let query = trailing_query(&finder.query, rect.width.saturating_sub(7));
                (format!("│{}│", pad(&if kind == PickerKind::Hover {
                    "  Documentation · Esc to close".to_string()
                } else { format!("  > {query}") }, interior)), Style::Input)
            } else if row == 2 {
                (format!("├{}┤", "─".repeat(interior)), Style::Border)
            } else if row == rect.height - 2 {
                let label = match kind {
                    PickerKind::Files => format!(
                        "  {} matches · {} indexed · ↑↓ choose · Enter open · Esc cancel",
                        finder.total_matches, finder.scanned
                    ),
                    PickerKind::Buffers => format!(
                        "  {} matches · {} buffers · ↑↓ choose · Enter switch · Esc cancel",
                        finder.total_matches, finder.scanned
                    ),
                    PickerKind::Hover => "  Rust language server · Esc close".to_string(),
                    PickerKind::Mason =>
                        "  Enter install · Ctrl+U update · Ctrl+S sync · Ctrl+R update all".to_string(),
                };
                (format!("│{}│", pad(&label, interior)), Style::Footer)
            } else if row == rect.height - 1 {
                (format!("╰{}╯", "─".repeat(interior)), Style::Border)
            } else {
                let index = start + row - 3;
                let entry = finder.matches.get(index);
                let is_selected = kind != PickerKind::Hover
                    && entry.is_some() && index + 1 == selected;
                let label = match entry {
                    Some(path) if kind == PickerKind::Hover => format!("  {path}"),
                    Some(path) => format!("  {} {path}", if is_selected { '>' } else { ' ' }),
                    None if row == 3 && finder.matches.is_empty() && finder.query.is_empty() =>
                        match kind {
                            PickerKind::Files => "  No project files found".to_string(),
                            PickerKind::Buffers => "  No open buffers".to_string(),
                            PickerKind::Hover => "  No hover information".to_string(),
                            PickerKind::Mason => "  No configured language servers".to_string(),
                        },
                    None if row == 3 && finder.matches.is_empty() =>
                        "  No matches — try another query".to_string(),
                    None => String::new(),
                };
                (format!("│{}│", pad(&label, interior)),
                    if is_selected { Style::Selected } else { Style::Normal })
            };
            if self.painted[row].as_ref() == Some(&(line.clone(), style)) {
                continue;
            }
            let (background, foreground) = match style {
                Style::Border => (BG, BORDER_FG),
                Style::Input => (INPUT_BG, TEXT_FG),
                Style::Selected => (HIGHLIGHT_BG, TEXT_FG),
                Style::Footer => (BG, MUTED_FG),
                Style::Normal => (BG, TEXT_FG),
            };
            queue!(
                output,
                MoveTo(rect.x, rect.y + row as u16),
                SetBackgroundColor(background),
                SetForegroundColor(foreground),
                Print(&line),
                ResetColor
            )?;
            self.painted[row] = Some((line, style));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mason_overlay_shows_status_and_action_hints() {
        let view = FinderView {
            query: "".into(),
            matches: vec!["rust_analyzer · Rust · installed · system".into()],
            selected: 1, total_matches: 1, scanned: 6,
        };
        let rect = rect_for(120, 32).unwrap();
        let mut cache = Cache::default();
        cache.set_rect(Some(rect));
        let mut rendered = Vec::new();
        cache.paint(&mut rendered, rect, &view, PickerKind::Mason).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("MASON"));
        assert!(text.contains("rust_analyzer"));
        assert!(text.contains("Ctrl+S sync"));
    }

    #[test]
    fn hover_overlay_reuses_cached_dialog_without_search_controls() {
        let view = FinderView {
            query: String::new(),
            matches: vec!["fn greet(name: &str)".into(), "Rust documentation".into()],
            selected: 0, total_matches: 2, scanned: 2,
        };
        let rect = rect_for(80, 30).unwrap();
        let mut cache = Cache::default();
        cache.set_rect(Some(rect));
        let mut output = Vec::new();
        cache.paint(&mut output, rect, &view, PickerKind::Hover).unwrap();
        let display = String::from_utf8(output).unwrap();
        assert!(display.contains("LSP HOVER"));
        assert!(display.contains("fn greet(name: &str)"));
        assert!(display.contains("Esc to close"));
        assert!(!display.contains("Enter switch"));
    }

    #[test]
    fn popup_is_centered_and_not_in_status_area() {
        let rect = rect_for(120, 38).unwrap();
        assert!(rect.x > 0 && rect.y > 0);
        assert!(rect.y as usize + rect.height <= 38);
        assert_eq!(rect_for(18, 38), None);
        assert_eq!(rect_for(80, 6), None);
    }

    #[test]
    fn popup_is_visible_with_initial_matches_and_cached_on_next_draw() {
        let finder = FinderView {
            query: String::new(),
            matches: vec!["README.md".into(), "src/main.rs".into()],
            selected: 1,
            total_matches: 2,
            scanned: 2,
        };
        let rect = rect_for(80, 30).unwrap();
        let mut cache = Cache::default();
        cache.set_rect(Some(rect));
        let mut output = Vec::new();
        cache.paint(&mut output, rect, &finder, PickerKind::Files).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("FIND FILES"));
        assert!(output.contains("README.md"));
        assert!(output.contains("src/main.rs"));
        assert!(output.contains("indexed"));
        assert!(!output.contains("\x1b[2J"));
        let mut subsequent = Vec::new();
        cache.paint(&mut subsequent, rect, &finder, PickerKind::Files).unwrap();
        assert!(subsequent.is_empty(), "unchanged search should not repaint");
        assert_eq!(cache.set_rect(None), Some(rect));
    }

    #[test]
    fn popup_repairs_rows_overwritten_by_the_underlying_tree() {
        let finder = FinderView {
            query: "p".into(),
            matches: vec!["src/plugins.rs".into(), "plugins/tree.lua".into()],
            selected: 1,
            total_matches: 2,
            scanned: 14,
        };
        // For a wide terminal the centered popup overlaps the tree gutter.
        let rect = rect_for(120, 38).unwrap();
        assert!(rect.x < 34);
        let mut cache = Cache::default();
        cache.set_rect(Some(rect));
        cache.paint(&mut Vec::new(), rect, &finder, PickerKind::Files).unwrap();

        // Ordinary repeated draws still avoid redundant popup output.
        let mut no_update = Vec::new();
        cache.paint(&mut no_update, rect, &finder, PickerKind::Files).unwrap();
        assert!(no_update.is_empty());

        // Simulate an underlying tree repaint crossing the popup's left edge.
        cache.invalidate_underpaint(0, rect.y as usize, 34);
        let mut repaired = Vec::new();
        cache.paint(&mut repaired, rect, &finder, PickerKind::Files).unwrap();
        let text = String::from_utf8(repaired).unwrap();
        assert!(text.contains("FIND FILES"), "popup title must be restored");
        assert!(!text.contains("src/plugins.rs"), "other popup rows stay cached");

        // A repaint at the search prompt must restore that row as well.
        cache.invalidate_underpaint(0, rect.y as usize + 1, 34);
        let mut repaired = Vec::new();
        cache.paint(&mut repaired, rect, &finder, PickerKind::Files).unwrap();
        let text = String::from_utf8(repaired).unwrap();
        assert!(text.contains("  > p"), "search input must not be clipped");

        // Unrelated terminal regions must not invalidate the dialog.
        cache.invalidate_underpaint(0, rect.y as usize + 1, rect.x as usize);
        cache.invalidate_underpaint(0, 0, 34);
        let mut unchanged = Vec::new();
        cache.paint(&mut unchanged, rect, &finder, PickerKind::Files).unwrap();
        assert!(unchanged.is_empty());
    }

    #[test]
    fn unicode_input_keeps_cursor_inside_dialog() {
        let rect = rect_for(60, 20).unwrap();
        let (x, y) = search_cursor(rect, "hällo🦀".repeat(20).as_str());
        assert!(x as usize <= rect.x as usize + rect.width - 2);
        assert_eq!(y, rect.y + 1);
    }
}
