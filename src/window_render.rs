//! Incremental rendering of the unfocused views in a vertical window layout.
//! Windows retain view coordinates; documents and undo histories stay shared.
use super::*;
use crate::windows::PaneRect;

#[derive(Default)]
pub(super) struct Cache {
    panes: HashMap<u64, PaneCache>,
    syntax: HashMap<u64, BufferSyntax>,
}

#[derive(Default)]
struct PaneCache {
    geometry: Option<(usize, usize, usize, usize, usize)>,
    revision: Option<u64>,
    syntax_enabled: Option<bool>,
    syntax_name: String,
    lines: Vec<Option<PaintedLine>>,
    status: Option<String>,
}

struct BufferSyntax {
    engine: SyntaxEngine,
    revision: u64,
}

impl Cache {
    pub(super) fn invalidate_rows(&mut self, first: usize, height: usize) {
        for pane in self.panes.values_mut() {
            for row in first..first.saturating_add(height).min(pane.lines.len()) {
                pane.lines[row] = None;
            }
        }
    }

    pub(super) fn invalidate_all(&mut self) {
        self.panes.clear();
    }
}

impl Editor {
    /// Paint unfocused windows without moving their document's canonical cursor.
    /// Parser state is retained per buffer, even when several windows show it.
    pub(super) fn draw_other_windows(
        &mut self,
        output: &mut impl Write,
        layout: &[PaneRect],
        height: usize,
        rows: u16,
        repaint_all: bool,
    ) -> io::Result<()> {
        let mut cache = std::mem::take(&mut self.screen.window_render);
        let enabled = self.syntax.enabled();
        cache.syntax.retain(|id, _| self.buffer_ids.contains(id));
        cache
            .panes
            .retain(|id, _| self.windows.iter().any(|window| window.id == *id));
        let result = (|| {
            for rect in layout
                .iter()
                .filter(|rect| rect.window != self.active_window)
            {
                let window = &mut self.windows[rect.window];
                let id = window.id;
                let buffer_id = window.buffer_id;
                let Some(index) = self.buffer_ids.iter().position(|id| *id == buffer_id) else {
                    continue;
                };
                let (doc, syntax): (&Document, &mut SyntaxEngine) = if index == self.active_slot {
                    (&self.doc, &mut self.syntax)
                } else {
                    let doc = &mut self.slots[index].as_mut().expect("inactive buffer").doc;
                    let entry = cache
                        .syntax
                        .entry(buffer_id)
                        .or_insert_with(|| BufferSyntax {
                            engine: SyntaxEngine::new(doc),
                            revision: doc.revision,
                        });
                    let invalid = entry.engine.refresh(doc);
                    // An active view may already have consumed the document's
                    // invalidation since this buffer was last painted here.
                    if entry.revision != doc.revision && invalid.is_none() {
                        entry.engine = SyntaxEngine::new(doc);
                    }
                    entry.revision = doc.revision;
                    entry.engine.set_enabled(enabled);
                    (&*doc, &mut entry.engine)
                };
                let cursor_row = window.row.min(doc.lines.len().saturating_sub(1));
                let cursor_col = window.col.min(doc.lines[cursor_row].len());
                let gutter = gutter_width(rect.width, doc.lines.len());
                let text_width = rect.width.saturating_sub(gutter);
                window.row = cursor_row;
                window.col = cursor_col;
                window.scroll_row = visible_origin(window.scroll_row, cursor_row, height);
                let column = visual_column(&doc.lines[cursor_row][..cursor_col]);
                window.scroll_col = visible_origin(window.scroll_col, column, text_width);
                let pane = cache.panes.entry(id).or_default();
                let touched = paint_pane(
                    output,
                    doc,
                    syntax,
                    pane,
                    View {
                        rect: *rect,
                        height,
                        rows,
                        id,
                        cursor_row,
                        cursor_col,
                        scroll_row: window.scroll_row,
                        scroll_col: window.scroll_col,
                        repaint_all,
                    },
                )?;
                for y in touched {
                    self.screen
                        .finder
                        .invalidate_underpaint(rect.x, y, rect.width);
                    self.screen
                        .terminal
                        .invalidate_underpaint(rect.x, y, rect.width);
                    self.screen
                        .completion
                        .invalidate_underpaint(rect.x, y, rect.width);
                }
            }
            Ok(())
        })();
        self.screen.window_render = cache;
        result
    }
}

fn visible_origin(origin: usize, cursor: usize, size: usize) -> usize {
    let origin = origin.min(cursor);
    if size > 0 && cursor >= origin.saturating_add(size) {
        cursor - size + 1
    } else {
        origin
    }
}

struct View {
    rect: PaneRect,
    height: usize,
    rows: u16,
    cursor_row: usize,
    cursor_col: usize,
    scroll_row: usize,
    scroll_col: usize,
    id: u64,
    repaint_all: bool,
}

fn paint_pane(
    output: &mut impl Write,
    doc: &Document,
    syntax: &mut SyntaxEngine,
    cache: &mut PaneCache,
    view: View,
) -> io::Result<Vec<usize>> {
    let View {
        rect,
        height,
        rows,
        cursor_row,
        cursor_col,
        scroll_row,
        scroll_col,
        id,
        repaint_all,
    } = view;
    let gutter = gutter_width(rect.width, doc.lines.len());
    let text_width = rect.width.saturating_sub(gutter);
    let geometry = (rect.x, rect.width, height, scroll_row, scroll_col);
    let full = repaint_all
        || cache.geometry != Some(geometry)
        || cache.syntax_enabled != Some(syntax.enabled())
        || cache.syntax_name != syntax.name();
    let revision_changed = cache.revision != Some(doc.revision);
    cache.lines.resize_with(height, || None);
    let mut touched = Vec::new();
    for y in 0..height {
        let index = y + scroll_row;
        let line = doc.lines.get(index);
        let content = line.map(Vec::as_slice);
        let active = index == cursor_row && line.is_some();
        let number = gutter_number(index, cursor_row, doc.lines.len(), gutter);
        let old = cache.lines[y].as_ref();
        let paint = full
            || revision_changed
            || old.is_none_or(|old| {
                old.index != index || old.content.as_deref() != content || old.active != active
            });
        let gutter_changed = old.is_none_or(|old| old.gutter != number);
        if !paint && !gutter_changed {
            continue;
        }
        if gutter > 0 {
            queue!(
                output,
                MoveTo(rect.x as u16, y as u16),
                SetBackgroundColor(GUTTER_BG),
                SetForegroundColor(if active { CURRENT_NUMBER_FG } else { GUTTER_FG }),
                Print(&number),
                ResetColor
            )?;
        }
        if paint {
            queue!(
                output,
                MoveTo((rect.x + gutter) as u16, y as u16),
                ResetColor
            )?;
            if active {
                queue!(output, SetBackgroundColor(CURRENT_LINE_BG))?;
            }
            let mut used = 0;
            if let Some(line) = line {
                if let Some(spans) = syntax.spans(doc, index).filter(|spans| !spans.is_empty()) {
                    for (color, text) in render_color_line(line, spans, scroll_col, text_width) {
                        used += UnicodeWidthStr::width(text.as_str());
                        queue!(output, SetForegroundColor(color), Print(text))?;
                    }
                } else {
                    let text = render_line(line, scroll_col, text_width);
                    used = UnicodeWidthStr::width(text.as_str());
                    queue!(output, Print(text))?;
                }
            }
            // Clear only this pane: erase-to-EOL would destroy its neighbours.
            queue!(
                output,
                SetForegroundColor(Color::Reset),
                Print(" ".repeat(text_width.saturating_sub(used))),
                ResetColor
            )?;
        }
        cache.lines[y] = Some(PaintedLine {
            index,
            content: line.cloned(),
            gutter: number,
            active,
            selected: None,
        });
        touched.push(y);
    }
    if rows >= 2 {
        let filename = doc
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[No Name]".into());
        let status = fit_cells(
            &format!(
                " W{id} {}{filename} | Ln {}, Col {} ",
                if doc.dirty { "● " } else { "" },
                cursor_row + 1,
                cursor_col + 1
            ),
            rect.width,
        );
        if cache.status.as_deref() != Some(status.as_str()) || full {
            queue!(
                output,
                MoveTo(rect.x as u16, rows - 2),
                SetBackgroundColor(Color::Rgb {
                    r: 43,
                    g: 44,
                    b: 52
                }),
                SetForegroundColor(Color::Rgb {
                    r: 168,
                    g: 172,
                    b: 184
                }),
                Print(&status),
                ResetColor
            )?;
            cache.status = Some(status);
        }
    }
    cache.geometry = Some(geometry);
    cache.revision = Some(doc.revision);
    cache.syntax_enabled = Some(syntax.enabled());
    cache.syntax_name = syntax.name().to_owned();
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn frame(editor: &mut Editor, cols: u16, rows: u16) -> Vec<u8> {
        crossterm::style::force_color_output(true);
        let mut output = Vec::new();
        editor.draw_at(&mut output, cols, rows).unwrap();
        output
    }

    fn split(text: &str) -> Editor {
        let mut editor = Editor::new(Document::from_text(text));
        frame(&mut editor, 120, 18);
        assert!(editor.window_split());
        editor
    }

    fn terminal(cols: u16, rows: u16) -> vt100::Parser {
        vt100::Parser::new(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
            0,
        )
    }

    #[test]
    fn split_views_render_their_own_scroll_positions_and_active_cursor() {
        let mut editor = split("alpha\nbeta\ngamma\ndelta");
        editor.doc.row = 2;
        editor.doc.col = 1;
        editor.scroll_row = 2;
        let layout = editor.window_layout(120, 0);
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        assert!(parser
            .screen()
            .rows(layout[0].x as u16, layout[0].width as u16)
            .next()
            .unwrap()
            .contains("alpha"));
        assert!(parser
            .screen()
            .rows(layout[1].x as u16, layout[1].width as u16)
            .next()
            .unwrap()
            .contains("gamma"));
        let (row, col) = parser.screen().cursor_position();
        assert_eq!(row, 0);
        assert!(
            usize::from(col) >= layout[1].x + gutter_width(layout[1].width, editor.doc.lines.len())
        );
        assert!(usize::from(col) < layout[1].x + layout[1].width);
        assert_eq!(
            editor.windows[0].row, 0,
            "painting must not move another view's cursor"
        );
    }

    #[test]
    fn shared_buffer_edits_refresh_both_panes_and_idle_does_not_repaint() {
        let mut editor = split("old text\nsecond");
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        editor.command(
            "lua remacs.buffer.set_lines(remacs.buffer.current(), {'new text', 'second'})",
        );
        parser.process(&frame(&mut editor, 120, 18));
        for rect in editor.window_layout(120, 0) {
            let first = parser
                .screen()
                .rows(rect.x as u16, rect.width as u16)
                .next()
                .unwrap();
            assert!(
                first.contains("new text"),
                "both views must use the same document: {first}"
            );
            assert!(!first.contains("old text"));
        }
        let idle = String::from_utf8(frame(&mut editor, 120, 18)).unwrap();
        assert!(!idle.contains("new text"));
        assert!(
            !idle.contains("│"),
            "unchanged dividers should remain cached"
        );
        assert!(!idle.contains("\x1b[2J"));
        assert!(idle.len() < 100);
    }

    #[test]
    fn closing_popup_restores_both_panes_and_the_divider() {
        let mut editor = split(
            &(0..40)
                .map(|row| format!("visible row {row}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let before: Vec<_> = parser.screen().rows(0, 120).take(16).collect();
        editor.open_buffer_picker();
        parser.process(&frame(&mut editor, 120, 18));
        editor.buffer_picker = None;
        parser.process(&frame(&mut editor, 120, 18));
        let after: Vec<_> = parser.screen().rows(0, 120).take(16).collect();
        assert_eq!(
            before, after,
            "closing a global overlay must restore every covered window"
        );
        let divider = editor.window_layout(120, 0)[0].width as u16;
        for row in 0..16 {
            assert_eq!(parser.screen().cell(row, divider).unwrap().contents(), "│");
        }
    }

    #[test]
    fn narrow_terminal_keeps_selected_view_and_restores_siblings_when_widened() {
        let mut editor = split("alpha\nbeta\ngamma\ndelta");
        editor.doc.row = 2;
        editor.scroll_row = 2;
        for cols in [1, 4, 19, 30, 40] {
            let mut parser = terminal(cols, 8);
            parser.process(&frame(&mut editor, cols, 8));
            let (row, col) = parser.screen().cursor_position();
            assert!(row < 6 && col < cols);
            assert_eq!(editor.windows.len(), 2);
            assert_eq!(editor.active_window, 1);
            assert!(!parser.screen().contents().contains('│'));
        }
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let left = editor.window_layout(120, 0)[0];
        assert!(parser
            .screen()
            .rows(left.x as u16, left.width as u16)
            .next()
            .unwrap()
            .contains("alpha"));
        assert_eq!(
            parser
                .screen()
                .cell(0, left.width as u16)
                .unwrap()
                .contents(),
            "│"
        );
    }

    #[test]
    fn wide_unicode_lines_stay_inside_their_window() {
        let mut editor = split(&"界😀abc\t".repeat(50));
        editor.doc.col = 20;
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let layout = editor.window_layout(120, 0);
        assert_eq!(
            parser
                .screen()
                .cell(0, layout[0].width as u16)
                .unwrap()
                .contents(),
            "│"
        );
        assert_eq!(
            parser
                .screen()
                .cell(1, layout[0].width as u16)
                .unwrap()
                .contents(),
            "│"
        );
        let (_, column) = parser.screen().cursor_position();
        assert!(usize::from(column) >= layout[1].x);
        assert!(usize::from(column) < layout[1].x + layout[1].width);
    }
    #[test]
    fn completion_menu_is_anchored_and_clipped_to_the_selected_pane() {
        let mut editor = split("pri\ncovered-row\nthird");
        editor.doc.path = Some(PathBuf::from("split-completion.rs"));
        editor.doc.col = 3;
        editor.begin_insert();
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
        editor.show_completions(context, lsp::Encoding::Utf16,
            completion::parse(&serde_json::json!([{ "label": "printer_with_a_very_long_completion_label_that_fills_the_entire_window" }])));
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let layout = editor.window_layout(120, 0);
        let left_rows: Vec<_> = parser
            .screen()
            .rows(0, layout[0].width as u16)
            .take(16)
            .collect();
        assert!(!left_rows
            .iter()
            .any(|row| row.contains('╭') || row.contains('╰')));
        let right_rows: Vec<_> = parser
            .screen()
            .rows(layout[1].x as u16, layout[1].width as u16)
            .take(16)
            .collect();
        assert!(right_rows.iter().any(|row| row.contains('╭')));
        assert!(right_rows.iter().any(|row| row.contains("printer_")));
        assert_eq!(
            parser
                .screen()
                .cell(2, layout[0].width as u16)
                .unwrap()
                .contents(),
            "│"
        );
        editor.dismiss_completion();
        parser.process(&frame(&mut editor, 120, 18));
        for rect in editor.window_layout(120, 0) {
            assert!(parser
                .screen()
                .rows(rect.x as u16, rect.width as u16)
                .nth(1)
                .unwrap()
                .contains("covered-row"));
        }
    }

    #[test]
    fn tree_and_splits_share_geometry_and_keep_editor_cursor_outside_the_gutters() {
        let mut editor = split("hello\nworld");
        editor.toggle_tree();
        editor.tree.as_mut().unwrap().leave();
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let sidebar = tree_sidebar_width(120, true);
        let layout = editor.window_layout(120, sidebar);
        assert_eq!(layout.len(), 2);
        assert_eq!(
            parser
                .screen()
                .cell(0, (sidebar - 1) as u16)
                .unwrap()
                .contents(),
            "│"
        );
        assert_eq!(
            parser
                .screen()
                .cell(0, (layout[0].x + layout[0].width) as u16)
                .unwrap()
                .contents(),
            "│"
        );
        let (_, col) = parser.screen().cursor_position();
        assert!(
            usize::from(col) >= layout[1].x + gutter_width(layout[1].width, editor.doc.lines.len())
        );
        assert!(usize::from(col) < layout[1].x + layout[1].width);
        let idle = String::from_utf8(frame(&mut editor, 120, 18)).unwrap();
        assert!(!idle.contains("hello") && !idle.contains("world") && !idle.contains('│'));
    }

    #[test]
    fn inactive_syntax_cache_survives_active_document_invalidation_consumption() {
        let mut editor = split("fn main() {}\nlet value = 1;");
        editor.doc.path = Some(PathBuf::from("split-syntax.rs"));
        let left = editor.windows[0].id;
        let right = editor.active_window_id();
        let other =
            env::temp_dir().join(format!("remacs-window-syntax-{}.txt", std::process::id()));
        std::fs::write(&other, "other buffer").unwrap();
        editor.open(other.to_string_lossy().into_owned());
        frame(&mut editor, 120, 18);
        editor.window_focus(left);
        editor.command(
            "lua remacs.buffer.set_lines(remacs.buffer.current(), {'/*', 'let value = 1;'})",
        );
        let mut parser = terminal(120, 18);
        parser.process(&frame(&mut editor, 120, 18));
        let x = gutter_width(
            editor.window_layout(120, 0)[0].width,
            editor.doc.lines.len(),
        ) as u16;
        let active_foreground = parser.screen().cell(1, x).unwrap().fgcolor();
        editor.window_focus(right);
        parser.process(&frame(&mut editor, 120, 18));
        assert_eq!(parser.screen().cell(1, x).unwrap().contents(), "l");
        assert_eq!(parser.screen().cell(1,x).unwrap().fgcolor(), active_foreground,
            "an old inactive parser must observe edits even when the active parser consumed the revision's invalidation");
        std::fs::remove_file(other).unwrap();
    }
}
