//! Window views are separate from buffer ownership. Every buffer still has one
//! document and undo history; any number of windows may display that document.
use super::*;

pub(super) const MIN_WINDOW_WIDTH: usize = 20;
const MAX_WINDOWS: usize = 16;

#[derive(Clone, Debug)]
pub(super) struct Window {
    pub id: u64,
    pub buffer_id: u64,
    pub row: usize,
    pub col: usize,
    pub scroll_row: usize,
    pub scroll_col: usize,
    /// Relative share of columns beyond the minimum pane width.
    pub weight: u32,
}

impl Window {
    pub fn initial(buffer_id: u64) -> Self {
        Self {
            id: 1,
            buffer_id,
            row: 0,
            col: 0,
            scroll_row: 0,
            scroll_col: 0,
            weight: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PaneRect {
    pub window: usize,
    pub x: usize,
    pub width: usize,
}

impl Editor {
    pub(super) fn active_window_id(&self) -> u64 {
        self.windows[self.active_window].id
    }

    pub(super) fn save_active_window(&mut self) {
        if self.suppress_window_sync {
            return;
        }
        let window = &mut self.windows[self.active_window];
        window.buffer_id = self.buffer_ids[self.active_slot];
        window.row = self.doc.row;
        window.col = self.doc.col;
        window.scroll_row = self.scroll_row;
        window.scroll_col = self.scroll_col;
    }

    pub(super) fn repair_window_buffers(&mut self) {
        if self.suppress_window_sync {
            return;
        }
        self.save_active_window();
        let fallback = self.windows[self.active_window].clone();
        for window in &mut self.windows {
            let index = match self
                .buffer_ids
                .iter()
                .position(|id| *id == window.buffer_id)
            {
                Some(index) => index,
                None => {
                    window.buffer_id = fallback.buffer_id;
                    window.row = fallback.row;
                    window.col = fallback.col;
                    window.scroll_row = fallback.scroll_row;
                    window.scroll_col = fallback.scroll_col;
                    self.active_slot
                }
            };
            let doc = if index == self.active_slot {
                &self.doc
            } else {
                &self.slots[index].as_ref().expect("inactive buffer").doc
            };
            window.row = window.row.min(doc.lines.len().saturating_sub(1));
            window.col = window.col.min(doc.lines[window.row].len());
            window.scroll_row = window.scroll_row.min(window.row);
            window.scroll_col = window
                .scroll_col
                .min(visual_column(&doc.lines[window.row][..window.col]));
        }
    }

    fn reset_window_input(&mut self) {
        self.dismiss_completion();
        self.lsp.cancel_navigation();
        self.hover_popup = None;
        self.mode = Mode::Normal;
        self.selection = None;
        self.visual_anchor = None;
        self.sequence = Sequence::None;
        self.count = None;
        self.insert_changed = false;
        let _ = self.lua.cancel_keys();
        self.buffer_picker = None;
        self.mason_picker = None;
        self.return_to_editor_after_switch();
    }

    fn invalidate_window_view(&mut self) {
        self.screen.lines.iter_mut().for_each(|line| *line = None);
        self.screen.origin = None;
        self.screen.status = None;
    }

    pub(super) fn window_focus(&mut self, id: u64) -> bool {
        let Some(index) = self.windows.iter().position(|window| window.id == id) else {
            self.message = format!("Unknown window: {id}");
            return false;
        };
        if index == self.active_window {
            self.return_to_editor_after_switch();
            return true;
        }
        self.repair_window_buffers();
        let target = self.windows[index].clone();
        let slot = self
            .buffer_ids
            .iter()
            .position(|id| *id == target.buffer_id)
            .expect("window references an open buffer");
        // take_current/activate normally update the selected window. A focus
        // transfer restores a different view after moving canonical ownership.
        self.suppress_window_sync = true;
        self.switch_buffer_internal(slot, false);
        self.suppress_window_sync = false;
        self.active_window = index;
        self.doc.row = target.row.min(self.doc.lines.len() - 1);
        self.doc.col = target.col.min(self.doc.line().len());
        self.scroll_row = target.scroll_row.min(self.doc.row);
        self.scroll_col = target.scroll_col;
        self.reset_window_input();
        self.save_active_window();
        self.invalidate_window_view();
        self.message = format!(
            "Window {}/{}: {}",
            index + 1,
            self.windows.len(),
            self.active_name()
        );
        true
    }

    pub(super) fn window_cycle(&mut self, forward: bool) {
        let count = self.windows.len();
        let index = if forward {
            (self.active_window + 1) % count
        } else {
            (self.active_window + count - 1) % count
        };
        self.window_focus(self.windows[index].id);
    }

    pub(super) fn window_direction(&mut self, right: bool) {
        let index = if right {
            (self.active_window + 1).min(self.windows.len() - 1)
        } else {
            self.active_window.saturating_sub(1)
        };
        self.window_focus(self.windows[index].id);
    }

    fn window_dimensions(&self) -> (usize, usize) {
        let (cols, _) = self
            .screen
            .size
            .or_else(|| terminal::size().ok())
            .unwrap_or((120, 30));
        let cols = cols as usize;
        let sidebar = tree_sidebar_width(cols, self.tree.as_ref().is_some_and(|tree| tree.visible));
        (cols, sidebar)
    }

    pub(super) fn window_layout(&self, columns: usize, sidebar_width: usize) -> Vec<PaneRect> {
        let available = columns.saturating_sub(sidebar_width);
        let count = self.windows.len();
        if count == 1 || available < MIN_WINDOW_WIDTH.saturating_mul(count) + count - 1 {
            // Terminal resizing never destroys views; temporarily give the
            // selected window all available space and restore siblings later.
            return vec![PaneRect {
                window: self.active_window,
                x: sidebar_width,
                width: available,
            }];
        }
        let extra = available - (MIN_WINDOW_WIDTH * count + count - 1);
        let total: u64 = self
            .windows
            .iter()
            .map(|window| u64::from(window.weight))
            .sum();
        let mut widths: Vec<usize> = self
            .windows
            .iter()
            .map(|window| {
                MIN_WINDOW_WIDTH
                    + (extra as u64 * u64::from(window.weight))
                        .checked_div(total)
                        .map_or(extra / count, |share| share as usize)
            })
            .collect();
        let spare = available - (widths.iter().sum::<usize>() + count - 1);
        for width in widths.iter_mut().take(spare) {
            *width += 1;
        }
        let mut x = sidebar_width;
        widths
            .into_iter()
            .enumerate()
            .map(|(window, width)| {
                let pane = PaneRect { window, x, width };
                x += width + 1;
                pane
            })
            .collect()
    }

    pub(super) fn window_split(&mut self) -> bool {
        if self.windows.len() >= MAX_WINDOWS {
            self.message = format!("At most {MAX_WINDOWS} windows can be open");
            return false;
        }
        let (columns, sidebar) = self.window_dimensions();
        let layout = self.window_layout(columns, sidebar);
        let Some(pane) = layout.iter().find(|pane| pane.window == self.active_window) else {
            return false;
        };
        if layout.len() != self.windows.len() || pane.width < MIN_WINDOW_WIDTH * 2 + 1 {
            self.message =
                "Not enough width to split this window (need at least 41 columns)".into();
            return false;
        }
        let width = pane.width;
        self.save_active_window();
        for pane in &layout {
            self.windows[pane.window].weight = (pane.width - MIN_WINDOW_WIDTH) as u32;
        }
        let left = (width - 1) / 2;
        let right = width - 1 - left;
        self.windows[self.active_window].weight = (left - MIN_WINDOW_WIDTH) as u32;
        let mut next = self.windows[self.active_window].clone();
        next.id = self.next_window_id;
        self.next_window_id += 1;
        next.weight = (right - MIN_WINDOW_WIDTH) as u32;
        let id = next.id;
        self.windows.insert(self.active_window + 1, next);
        self.window_focus(id);
        true
    }

    pub(super) fn window_close(&mut self) -> bool {
        self.window_close_id(self.active_window_id())
    }

    pub(super) fn window_close_id(&mut self, id: u64) -> bool {
        let Some(index) = self.windows.iter().position(|window| window.id == id) else {
            self.message = format!("Unknown window: {id}");
            return false;
        };
        if self.windows.len() == 1 {
            self.message = "Cannot close the last window; use :q to quit".into();
            return false;
        }
        self.save_active_window();
        if index == self.active_window {
            let next = if index + 1 < self.windows.len() {
                index + 1
            } else {
                index - 1
            };
            self.window_focus(self.windows[next].id);
        }
        self.windows.remove(index);
        if self.active_window > index {
            self.active_window -= 1;
        }
        self.invalidate_window_view();
        self.message = "Window closed; its buffer remains open".into();
        true
    }

    pub(super) fn window_only(&mut self) {
        self.window_only_id(self.active_window_id());
    }

    pub(super) fn window_only_id(&mut self, id: u64) -> bool {
        if !self.window_focus(id) {
            return false;
        }
        self.save_active_window();
        let mut window = self.windows[self.active_window].clone();
        window.weight = 1;
        self.windows = vec![window];
        self.active_window = 0;
        self.invalidate_window_view();
        self.message = "Only this window; other buffers remain open".into();
        true
    }

    pub(super) fn window_equalize(&mut self) {
        for window in &mut self.windows {
            window.weight = 1;
        }
        self.invalidate_window_view();
    }

    pub(super) fn window_resize(&mut self, delta: isize) {
        if self.windows.len() == 1 {
            self.message = "Only one window is open".into();
            return;
        }
        let (columns, sidebar) = self.window_dimensions();
        let layout = self.window_layout(columns, sidebar);
        if layout.len() != self.windows.len() {
            self.message = "Terminal too narrow to resize windows".into();
            return;
        }
        let neighbor = if self.active_window + 1 < self.windows.len() {
            self.active_window + 1
        } else {
            self.active_window - 1
        };
        for pane in &layout {
            self.windows[pane.window].weight = (pane.width - MIN_WINDOW_WIDTH) as u32;
        }
        let donor = if delta >= 0 {
            neighbor
        } else {
            self.active_window
        };
        let receiver = if delta >= 0 {
            self.active_window
        } else {
            neighbor
        };
        let amount = delta
            .unsigned_abs()
            .min(self.windows[donor].weight as usize) as u32;
        self.windows[donor].weight -= amount;
        self.windows[receiver].weight += amount;
        self.invalidate_window_view();
    }
}
