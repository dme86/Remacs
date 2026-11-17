//! A small completion menu anchored to the editor cursor. The editor keeps
//! keyboard focus and restores covered rows when this cached overlay closes.
use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
};
use unicode_width::UnicodeWidthStr;

pub use crate::finder_ui::Rect;

const MAX_ITEMS: usize = 8;
const MAX_WIDTH: usize = 64;
const MAX_LABEL_CHARS: usize = 512;
const BG: Color = Color::Rgb {
    r: 34,
    g: 35,
    b: 44,
};
const SELECTED_BG: Color = Color::Rgb {
    r: 77,
    g: 65,
    b: 99,
};
const BORDER_FG: Color = Color::Rgb {
    r: 153,
    g: 139,
    b: 183,
};
const TEXT_FG: Color = Color::Rgb {
    r: 220,
    g: 221,
    b: 231,
};

/// Place the menu wholly inside the editor, leaving the cursor line visible.
/// The caller supplies the editor's row count, excluding its status lines.
pub fn rect_for(
    cols: u16,
    editor_rows: usize,
    cursor: (usize, usize),
    labels: &[String],
) -> Option<Rect> {
    let cols = cols as usize;
    if cols < 6 || editor_rows < 3 || labels.is_empty() {
        return None;
    }
    let cursor_y = cursor.1.min(editor_rows - 1);
    let above = cursor_y;
    let below = editor_rows - cursor_y - 1;
    let desired_height = labels.len().min(MAX_ITEMS) + 2;
    let use_below = below >= desired_height || (above < desired_height && below >= above);
    let available = if use_below { below } else { above };
    if available < 3 {
        return None;
    }
    let height = desired_height.min(available);
    let widest = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(sanitize(label).as_str()))
        .max()
        .unwrap_or(0);
    // Two border cells, one selection marker and a following space.
    let width = widest.saturating_add(4).clamp(12, MAX_WIDTH).min(cols);
    let x = cursor.0.min(cols - width);
    let y = if use_below {
        cursor_y + 1
    } else {
        cursor_y - height
    };
    Some(Rect {
        x: x as u16,
        y: y as u16,
        width,
        height,
    })
}

fn sanitize(text: &str) -> String {
    text.chars()
        .take(MAX_LABEL_CHARS)
        .filter(|ch| !ch.is_control())
        .collect()
}

/// Clip at character boundaries and pad in terminal cells. No server-provided
/// control character is allowed into the terminal command stream.
fn fit(text: &str, width: usize) -> String {
    let mut result = String::new();
    for ch in text
        .chars()
        .take(MAX_LABEL_CHARS)
        .filter(|ch| !ch.is_control())
    {
        result.push(ch);
        if UnicodeWidthStr::width(result.as_str()) > width {
            result.pop();
            break;
        }
    }
    // Unicode sequences such as joined emoji can occupy fewer cells than the
    // sum of their scalar widths. Padding uses the resulting string's width.
    let actual = UnicodeWidthStr::width(result.as_str());
    result.extend(std::iter::repeat_n(' ', width.saturating_sub(actual)));
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Border,
    Normal,
    Selected,
}

#[derive(Default)]
pub struct Cache {
    rect: Option<Rect>,
    painted: Vec<Option<(String, Style)>>,
}

impl Cache {
    /// Return the area that the editor must restore when geometry changes.
    pub fn set_rect(&mut self, next: Option<Rect>) -> Option<Rect> {
        if self.rect == next {
            return None;
        }
        let previous = self.rect;
        self.rect = next;
        self.painted.clear();
        previous
    }

    /// Repaint an overlay row if the editor/sidebar painted underneath it.
    pub fn invalidate_underpaint(&mut self, x: usize, y: usize, width: usize) {
        if width == 0 {
            return;
        }
        let Some(rect) = self.rect else { return };
        let left = rect.x as usize;
        if x >= left + rect.width || x.saturating_add(width) <= left {
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
        labels: &[String],
        selected: usize,
    ) -> io::Result<()> {
        if rect.width < 4 || rect.height < 3 {
            return Ok(());
        }
        self.painted.resize_with(rect.height, || None);
        let interior = rect.width - 2;
        let available = rect.height - 2;
        let selected = selected.min(labels.len().saturating_sub(1));
        let start = selected
            .saturating_add(1)
            .saturating_sub(available)
            .min(labels.len().saturating_sub(available));
        for (row, painted) in self.painted.iter_mut().enumerate() {
            let (line, style) = if row == 0 {
                (format!("╭{}╮", "─".repeat(interior)), Style::Border)
            } else if row == rect.height - 1 {
                (format!("╰{}╯", "─".repeat(interior)), Style::Border)
            } else {
                let index = start + row - 1;
                let entry = labels.get(index);
                let active = entry.is_some() && index == selected;
                let label = entry.map_or_else(String::new, |label| {
                    format!("{} {label}", if active { '›' } else { ' ' })
                });
                (
                    format!("│{}│", fit(&label, interior)),
                    if active {
                        Style::Selected
                    } else {
                        Style::Normal
                    },
                )
            };
            if painted.as_ref().is_some_and(|(previous, previous_style)| {
                previous == &line && *previous_style == style
            }) {
                continue;
            }
            let (background, foreground) = match style {
                Style::Border => (BG, BORDER_FG),
                Style::Normal => (BG, TEXT_FG),
                Style::Selected => (SELECTED_BG, TEXT_FG),
            };
            queue!(
                output,
                MoveTo(rect.x, rect.y + row as u16),
                SetBackgroundColor(background),
                SetForegroundColor(foreground),
                Print(&line),
                ResetColor
            )?;
            *painted = Some((line, style));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("suggestion_{index}"))
            .collect()
    }

    #[test]
    fn menu_geometry_stays_inside_editor_at_all_small_boundaries() {
        let labels = labels(20);
        for cols in 0..90 {
            for rows in 0..25 {
                for cursor_y in 0..=rows {
                    if let Some(rect) =
                        rect_for(cols, rows, (cols as usize + 10, cursor_y), &labels)
                    {
                        assert!(rect.x as usize + rect.width <= cols as usize);
                        assert!(rect.y as usize + rect.height <= rows);
                        assert!((3..=10).contains(&rect.height));
                        assert!(rect.width <= MAX_WIDTH);
                        let cursor_y = cursor_y.min(rows - 1);
                        assert!(
                            cursor_y < rect.y as usize || cursor_y >= rect.y as usize + rect.height
                        );
                    }
                }
            }
        }
        assert_eq!(rect_for(80, 20, (0, 0), &[]), None);
    }

    #[test]
    fn menu_prefers_below_and_moves_above_near_bottom() {
        let labels = labels(12);
        let below = rect_for(80, 24, (10, 2), &labels).unwrap();
        assert_eq!((below.x, below.y, below.height), (10, 3, 10));
        let above = rect_for(80, 24, (79, 23), &labels).unwrap();
        assert_eq!(above.y as usize + above.height, 23);
        assert_eq!(above.x as usize + above.width, 80);
        let shortened = rect_for(12, 8, (2, 4), &labels).unwrap();
        assert_eq!((shortened.y, shortened.height), (0, 4));
    }

    #[test]
    fn unicode_labels_are_clipped_and_controls_never_reach_terminal() {
        let padded = fit("界é\n\r\t\x1b\x07x", 6);
        assert_eq!(padded, "界éx  ");
        assert_eq!(UnicodeWidthStr::width(padded.as_str()), 6);
        assert!(!padded.chars().any(char::is_control));
        assert_eq!(fit("世界tail", 3), "世 ");
        let joined = fit("👩\u{200d}💻", 6);
        assert_eq!(UnicodeWidthStr::width(joined.as_str()), 6);
        assert_eq!(UnicodeWidthStr::width(fit("❤️", 1).as_str()), 1);
        let labels = vec!["safe\x1b[2J\nlabel".to_string()];
        let rect = rect_for(80, 20, (0, 0), &labels).unwrap();
        let mut cache = Cache::default();
        cache.set_rect(Some(rect));
        let mut output = Vec::new();
        cache.paint(&mut output, rect, &labels, 0).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("\x1b[2J"));
        assert!(output.contains("safe[2Jlabel"));
    }

    #[test]
    fn selection_scrolls_and_unchanged_menu_emits_nothing() {
        let labels = labels(20);
        let rect = rect_for(80, 20, (0, 0), &labels).unwrap();
        let mut cache = Cache::default();
        assert_eq!(cache.set_rect(Some(rect)), None);
        let mut first = Vec::new();
        cache.paint(&mut first, rect, &labels, 19).unwrap();
        let first = String::from_utf8(first).unwrap();
        assert!(first.contains("› suggestion_19"));
        assert!(first.contains("suggestion_12"));
        assert!(!first.contains("suggestion_11"));
        let mut unchanged = Vec::new();
        cache.paint(&mut unchanged, rect, &labels, 19).unwrap();
        assert!(unchanged.is_empty());
        cache.invalidate_underpaint(rect.x as usize, rect.y as usize + 2, rect.width);
        let mut repaired = Vec::new();
        cache.paint(&mut repaired, rect, &labels, 19).unwrap();
        let repaired = String::from_utf8(repaired).unwrap();
        assert!(repaired.contains("suggestion_13"));
        assert!(!repaired.contains("suggestion_12"));
        assert_eq!(cache.set_rect(None), Some(rect));
        assert_eq!(cache.set_rect(None), None);
    }
}
