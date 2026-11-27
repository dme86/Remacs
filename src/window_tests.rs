//! Behavioral checks for shared buffers and independent window views.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "remacs-window-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn editor(text: &str) -> Editor {
    let mut editor = Editor::new(Document::from_text(text));
    // A deterministic terminal geometry, independent of the test runner's TTY.
    editor.screen.size = Some((160, 30));
    editor
}

#[test]
fn same_buffer_windows_keep_independent_cursor_and_viewport_without_copying_buffer() {
    let text = (0..100)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut editor = editor(&text);
    editor.doc.row = 70;
    editor.doc.col = 2;
    editor.scroll_row = 65;
    editor.scroll_col = 1;
    let left = editor.windows[0].id;
    assert!(editor.window_split());
    let right = editor.active_window_id();
    assert_ne!(left, right);
    assert_eq!(
        editor.slots.len(),
        1,
        "splits must reference the existing buffer"
    );
    assert_eq!(editor.windows[0].buffer_id, editor.windows[1].buffer_id);

    editor.doc.row = 5;
    editor.doc.col = 1;
    editor.scroll_row = 3;
    editor.scroll_col = 0;
    assert!(editor.window_focus(left));
    assert_eq!(
        (
            editor.doc.row,
            editor.doc.col,
            editor.scroll_row,
            editor.scroll_col
        ),
        (70, 2, 65, 1)
    );
    assert!(editor.window_focus(right));
    assert_eq!(
        (
            editor.doc.row,
            editor.doc.col,
            editor.scroll_row,
            editor.scroll_col
        ),
        (5, 1, 3, 0)
    );
}

#[test]
fn editing_and_undo_are_shared_between_views_of_the_same_buffer() {
    let mut editor = editor("abc\ndef");
    let left = editor.active_window_id();
    assert!(editor.window_split());
    let right = editor.active_window_id();
    editor.doc.row = 1;
    editor.doc.col = 1;
    editor.command("lua remacs.buffer.insert(remacs.buffer.current(), 'X')");
    assert_eq!(editor.doc.lsp_text(), "abc\ndXef");
    assert!(editor.doc.dirty);
    assert!(editor.window_focus(left));
    assert_eq!(editor.doc.lsp_text(), "abc\ndXef");
    editor.command("undo");
    assert_eq!(editor.doc.lsp_text(), "abc\ndef");
    assert!(editor.window_focus(right));
    assert_eq!(editor.doc.lsp_text(), "abc\ndef");
    editor.command("redo");
    assert_eq!(editor.doc.lsp_text(), "abc\ndXef");
    assert_eq!(editor.slots.len(), 1);
}

#[test]
fn opening_another_file_changes_only_the_focused_window() {
    let project = Project::new();
    let one = project.file("one.txt", "one\nsecond");
    let two = project.file("two.txt", "two");
    let mut editor = Editor::new(Document::open(one.clone()).unwrap());
    editor.screen.size = Some((160, 30));
    let left = editor.active_window_id();
    let first_buffer = editor.buffer_ids[editor.active_slot];
    assert!(editor.window_split());
    let right = editor.active_window_id();
    editor.open(two.to_string_lossy().into_owned());
    let second_buffer = editor.buffer_ids[editor.active_slot];
    assert_ne!(first_buffer, second_buffer);
    assert_eq!(editor.windows[0].buffer_id, first_buffer);
    assert_eq!(editor.windows[1].buffer_id, second_buffer);
    assert!(editor.window_focus(left));
    assert_eq!(editor.doc.path, Some(one));
    assert!(editor.window_focus(right));
    assert_eq!(editor.doc.path, Some(two));
}

#[test]
fn closing_dirty_window_keeps_buffer_text_and_undo_history_available() {
    let project = Project::new();
    let one = project.file("one.txt", "one");
    let two = project.file("two.txt", "two");
    let mut editor = Editor::new(Document::open(one).unwrap());
    editor.screen.size = Some((160, 30));
    assert!(editor.window_split());
    editor.open(two.to_string_lossy().into_owned());
    editor.command("lua remacs.buffer.insert(remacs.buffer.current(), 'X')");
    let edited_buffer = editor.buffer_ids[editor.active_slot];
    assert!(editor.window_close());
    assert_eq!(editor.windows.len(), 1);
    assert_eq!(
        editor.slots.len(),
        2,
        "closing a pane must not kill its buffer"
    );
    let index = editor
        .buffer_ids
        .iter()
        .position(|id| *id == edited_buffer)
        .unwrap();
    editor.switch_buffer(index);
    assert_eq!(editor.doc.lsp_text(), "Xtwo");
    assert!(editor.doc.dirty);
    editor.command("undo");
    assert_eq!(editor.doc.lsp_text(), "two");
    assert!(
        editor.doc.dirty,
        "undo keeps the existing conservative dirty semantics"
    );
}

#[test]
fn killing_a_buffer_repairs_every_window_that_displayed_it() {
    let project = Project::new();
    let one = project.file("one.txt", "one");
    let two = project.file("two.txt", "two");
    let mut editor = Editor::new(Document::open(one).unwrap());
    editor.screen.size = Some((160, 30));
    let retained = editor.buffer_ids[editor.active_slot];
    assert!(editor.window_split());
    editor.open(two.to_string_lossy().into_owned());
    let removed = editor.buffer_ids[editor.active_slot];
    assert!(editor.window_split());
    editor.close_buffer(false);
    assert!(!editor.buffer_ids.contains(&removed));
    assert_eq!(editor.windows.len(), 3);
    assert!(editor
        .windows
        .iter()
        .all(|window| window.buffer_id == retained));
    let ids = editor
        .windows
        .iter()
        .map(|window| window.id)
        .collect::<Vec<_>>();
    for id in ids {
        assert!(editor.window_focus(id));
        assert_eq!(editor.doc.lsp_text(), "one");
    }
}

#[test]
fn killing_the_last_buffer_replaces_its_identity_in_all_views() {
    let mut editor = editor("clean");
    let removed = editor.buffer_ids[0];
    assert!(editor.window_split());
    editor.close_buffer(false);
    let replacement = editor.buffer_ids[0];
    assert_ne!(removed, replacement);
    assert!(editor
        .windows
        .iter()
        .all(|window| window.buffer_id == replacement));
    let left = editor.windows[0].id;
    assert!(editor.window_focus(left));
    assert_eq!(editor.doc.lsp_text(), "");
}

#[test]
fn window_focus_clamps_cursor_after_shared_buffer_is_shortened() {
    let mut editor = editor("abcdef\nsecond\nthird\nfourth");
    editor.doc.row = 3;
    editor.doc.col = 5;
    editor.scroll_row = 3;
    let left = editor.active_window_id();
    assert!(editor.window_split());
    editor.command("lua remacs.buffer.set_lines(remacs.buffer.current(), {'x'})");
    assert!(
        editor
            .windows
            .iter()
            .all(|window| window.row == 0 && window.col <= 1),
        "view metadata must be valid before hooks or rendering inspect it"
    );
    assert!(editor.window_focus(left));
    assert_eq!(editor.doc.row, 0);
    assert!(editor.doc.col <= 1);
    assert_eq!(editor.scroll_row, 0);
    let mut output = Vec::new();
    editor.draw_at(&mut output, 80, 18).unwrap();
}

#[test]
fn narrow_terminal_temporarily_hides_views_without_removing_them() {
    let mut editor = editor("text");
    assert!(editor.window_split());
    assert!(editor.window_split());
    let active = editor.active_window_id();
    let ids = editor
        .windows
        .iter()
        .map(|window| window.id)
        .collect::<Vec<_>>();
    let narrow = editor.window_layout(12, 0);
    assert_eq!(narrow.len(), 1);
    assert_eq!(editor.windows[narrow[0].window].id, active);
    assert!(narrow[0].x + narrow[0].width <= 12);
    assert_eq!(
        editor
            .windows
            .iter()
            .map(|window| window.id)
            .collect::<Vec<_>>(),
        ids
    );
    let wide = editor.window_layout(160, 0);
    assert_eq!(wide.len(), 3);
}

#[test]
fn resizing_respects_minimum_width_and_preserves_total_columns() {
    let mut editor = editor("text");
    assert!(editor.window_split());
    let initial = editor.window_layout(160, 0);
    let focused = editor.active_window;
    let initial_width = initial
        .iter()
        .find(|rect| rect.window == focused)
        .unwrap()
        .width;
    editor.window_resize(8);
    let grown = editor.window_layout(160, 0);
    assert_eq!(
        grown
            .iter()
            .find(|rect| rect.window == focused)
            .unwrap()
            .width,
        initial_width + 8
    );
    editor.window_resize(isize::MAX);
    let maximal = editor.window_layout(160, 0);
    assert!(maximal
        .iter()
        .all(|rect| rect.width >= windows::MIN_WINDOW_WIDTH));
    assert_eq!(
        maximal.iter().map(|rect| rect.width).sum::<usize>() + 1,
        160
    );
    editor.window_resize(isize::MIN);
    let minimal = editor.window_layout(160, 0);
    assert_eq!(
        minimal
            .iter()
            .find(|rect| rect.window == focused)
            .unwrap()
            .width,
        windows::MIN_WINDOW_WIDTH
    );
    editor.window_equalize();
    let equal = editor.window_layout(160, 0);
    assert!(equal[0].width.abs_diff(equal[1].width) <= 1);
}

#[test]
fn closing_last_window_is_rejected_without_losing_unsaved_text() {
    let mut editor = editor("text");
    editor.command("lua remacs.buffer.insert(remacs.buffer.current(), 'X')");
    let window = editor.active_window_id();
    assert!(!editor.window_close());
    assert_eq!(editor.active_window_id(), window);
    assert_eq!(editor.windows.len(), 1);
    assert_eq!(editor.doc.lsp_text(), "Xtext");
    assert!(editor.doc.dirty);
    assert!(!editor.undo.is_empty());
}

#[test]
fn closed_window_ids_are_not_reused() {
    let mut editor = editor("text");
    assert!(editor.window_split());
    let closed = editor.active_window_id();
    assert!(editor.window_close());
    assert!(editor.window_split());
    assert!(editor.active_window_id() > closed);
    assert!(!editor.window_focus(closed));
    assert_eq!(editor.windows.len(), 2);
}

#[test]
fn same_buffer_focus_discards_completion_selection_and_insert_group() {
    let mut editor = editor("text");
    editor.doc.path = Some(PathBuf::from("/tmp/remacs-window-completion.rs"));
    let left = editor.active_window_id();
    assert!(editor.window_split());
    editor.mode = Mode::Insert;
    editor.insert_changed = true;
    let context = CompletionContext {
        origin: editor.doc.path.clone().unwrap(),
        revision: editor.doc.revision,
        cursor: (editor.doc.row, editor.doc.col),
    };
    editor.completion_pending = Some(PendingCompletion {
        context: context.clone(),
        since: Instant::now(),
        manual: true,
    });
    editor.completion_due = Some(Instant::now());
    editor.completion_menu = Some(CompletionMenu {
        context,
        encoding: lsp::Encoding::Utf16,
        items: Vec::new(),
        labels: Vec::new(),
        selected: 0,
        incomplete: false,
    });
    editor.hover_popup = Some("stale docs".into());
    assert!(editor.window_focus(left));
    assert_eq!(editor.mode, Mode::Normal);
    assert!(!editor.insert_changed);
    assert!(editor.completion_menu.is_none());
    assert!(editor.completion_pending.is_none());
    assert!(editor.completion_due.is_none());
    assert!(editor.hover_popup.is_none());
}

#[test]
fn refusing_split_in_narrow_terminal_leaves_model_and_text_intact() {
    let mut editor = editor("text");
    editor.screen.size = Some((40, 20));
    let initial = editor.active_window_id();
    assert!(!editor.window_split());
    assert_eq!(editor.active_window_id(), initial);
    assert_eq!(editor.windows.len(), 1);
    assert_eq!(editor.doc.lsp_text(), "text");
}

#[test]
fn window_prefixes_leave_the_focused_tree_without_closing_it() {
    let mut editor = editor("text");
    editor.toggle_tree();
    assert!(editor.tree.as_ref().unwrap().focused);
    editor.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL), 10);
    editor.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE), 10);
    assert!(!editor.tree.as_ref().unwrap().focused);
    editor.tree.as_mut().unwrap().focus();
    editor.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL), 10);
    editor.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), 10);
    assert_eq!(editor.windows.len(), 2);
    assert!(editor.tree.as_ref().unwrap().visible);
    assert!(!editor.tree.as_ref().unwrap().focused);
    let right = editor.active_window_id();
    editor.tree.as_mut().unwrap().focus();
    editor.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), 10);
    editor.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), 10);
    assert_ne!(editor.active_window_id(), right);
    assert!(!editor.tree.as_ref().unwrap().focused);
}

struct TabWindow {
    id: u64,
    path: PathBuf,
    row: usize,
    col: usize,
}

fn tab_windows(project: &Project, count: usize) -> (Editor, Vec<TabWindow>) {
    let first = project.file(
        "tab-0.txt",
        "first line\nsecond line\nthird line\nfourth line",
    );
    let mut editor = Editor::new(Document::open(first.clone()).unwrap());
    editor.screen.size = Some((160, 30));
    let mut views = Vec::new();
    for index in 0..count {
        let path = if index == 0 {
            first.clone()
        } else {
            assert!(editor.window_split());
            let path = project.file(
                &format!("tab-{index}.txt"),
                "another first\nanother second\nanother third\nanother fourth",
            );
            editor.open(path.to_string_lossy().into_owned());
            path
        };
        editor.doc.row = index;
        editor.doc.col = index + 1;
        views.push(TabWindow {
            id: editor.active_window_id(),
            path,
            row: index,
            col: index + 1,
        });
    }
    (editor, views)
}

fn assert_tab_window(editor: &Editor, view: &TabWindow) {
    assert_eq!(editor.active_window_id(), view.id);
    assert_eq!(editor.doc.path.as_ref(), Some(&view.path));
    assert_eq!((editor.doc.row, editor.doc.col), (view.row, view.col));
}

fn arm_tab_forward_history(editor: &mut Editor, target: &Path) {
    let origin = editor.current_location().unwrap();
    editor.jumps = JumpManager::memory_only();
    editor.jumps.remember(origin.clone()).unwrap();
    editor
        .jumps
        .remember(Location {
            path: path_identity(target),
            row: 2,
            col: 3,
        })
        .unwrap();
    assert_eq!(editor.jumps.back(None).unwrap(), Some(origin));
    assert!(editor.jumps.can_forward());
}

#[test]
fn tab_cycles_two_file_windows_and_restores_each_cursor() {
    let project = Project::new();
    let (mut editor, views) = tab_windows(&project, 2);
    assert!(editor.tree.is_none());
    assert_tab_window(&editor, &views[1]);
    for index in [0, 1, 0, 1] {
        editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
        assert_tab_window(&editor, &views[index]);
    }
    assert_eq!(editor.slots.len(), 2);
    assert!(editor
        .slots
        .iter()
        .flatten()
        .all(|buffer| !buffer.doc.dirty));
    assert!(!editor.doc.dirty);
}

#[test]
fn plain_tab_wraps_across_three_windows_without_changing_shift_tab() {
    let project = Project::new();
    let (mut editor, views) = tab_windows(&project, 3);
    for index in [0, 1, 2, 0, 1, 2] {
        editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
        assert_tab_window(&editor, &views[index]);
    }
    for (code, modifiers) in [
        (KeyCode::BackTab, KeyModifiers::SHIFT),
        (KeyCode::Tab, KeyModifiers::SHIFT),
        (KeyCode::BackTab, KeyModifiers::NONE),
    ] {
        editor.key(KeyEvent::new(code, modifiers), 10);
        assert_tab_window(&editor, &views[2]);
    }
    assert_eq!(editor.slots.len(), 3);
}

#[test]
fn closed_tree_tabs_cycle_windows_before_legacy_forward_history() {
    let project = Project::new();
    let target = project.file("jump-target.txt", "zero\none\ntarget position");
    let (mut editor, views) = tab_windows(&project, 2);
    editor.toggle_tree();
    editor.toggle_tree();
    assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible));
    arm_tab_forward_history(&mut editor, &target);
    let history = editor.jumps.entries().to_vec();
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_tab_window(&editor, &views[0]);
    assert!(editor.jumps.can_forward());
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_tab_window(&editor, &views[1]);
    assert_eq!(editor.jumps.entries(), history.as_slice());
    assert!(editor.jumps.can_forward());
    assert!(editor.tree.as_ref().is_some_and(|tree| !tree.visible));
}

#[test]
fn explicit_control_i_and_alt_right_keep_forward_jump_with_hidden_tree_and_splits() {
    for key in [
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
    ] {
        let project = Project::new();
        let target = project.file("explicit-jump.txt", "zero\none\ntarget position");
        let (mut editor, views) = tab_windows(&project, 2);
        arm_tab_forward_history(&mut editor, &target);
        editor.key(key, 10);
        assert_eq!(editor.active_window_id(), views[1].id);
        assert_eq!(
            path_identity(editor.doc.path.as_ref().unwrap()),
            path_identity(&target)
        );
        assert_eq!((editor.doc.row, editor.doc.col), (2, 3));
        assert!(!editor.jumps.can_forward());
    }
}

#[test]
fn visible_tree_keeps_legacy_forward_jump_then_editor_tree_focus_with_splits() {
    let project = Project::new();
    let target = project.file("visible-tree-jump.txt", "zero\none\ntarget position");
    let (mut editor, views) = tab_windows(&project, 2);
    editor.toggle_tree();
    editor.tree.as_mut().unwrap().leave();
    arm_tab_forward_history(&mut editor, &target);
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), views[1].id);
    assert_eq!(
        path_identity(editor.doc.path.as_ref().unwrap()),
        path_identity(&target)
    );
    assert!(!editor.jumps.can_forward());
    assert!(editor
        .tree
        .as_ref()
        .is_some_and(|tree| tree.visible && !tree.focused));
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), views[1].id);
    assert!(editor.tree.as_ref().is_some_and(|tree| tree.focused));
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), views[1].id);
    assert!(editor.tree.as_ref().is_some_and(|tree| !tree.focused));
    editor.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), 10);
    assert_eq!(editor.active_window_id(), views[1].id);
}

#[test]
fn one_window_retains_legacy_tab_forward_jump_and_no_window_cycle() {
    let project = Project::new();
    let target = project.file("one-window-jump.txt", "zero\none\ntarget position");
    let (mut editor, views) = tab_windows(&project, 1);
    arm_tab_forward_history(&mut editor, &target);
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), views[0].id);
    assert_eq!(
        path_identity(editor.doc.path.as_ref().unwrap()),
        path_identity(&target)
    );
    assert!(!editor.jumps.can_forward());
    for key in [
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    ] {
        editor.key(key, 10);
        assert_eq!(editor.active_window_id(), views[0].id);
        assert_eq!((editor.doc.row, editor.doc.col), (2, 3));
    }
}

#[test]
fn split_tab_shortcut_leaves_insert_visual_prompt_picker_and_terminal_input_alone() {
    let project = Project::new();
    let (mut editor, views) = tab_windows(&project, 2);
    let window = views[1].id;
    editor.begin_insert();
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), window);
    assert_eq!(editor.mode, Mode::Insert);
    assert_eq!(editor.doc.line()[2], '\t');
    editor.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10);
    editor.enter_visual();
    let selection = editor.selection;
    for key in [
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    ] {
        editor.key(key, 10);
        assert_eq!(editor.active_window_id(), window);
        assert_eq!(editor.mode, Mode::Visual);
        assert_eq!(editor.selection, selection);
    }
    editor.leave_visual();
    editor.prompt = Some(Prompt::Command("buffers".into()));
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), window);
    assert!(matches!(&editor.prompt, Some(Prompt::Command(value)) if value == "buffers"));
    editor.prompt = None;
    editor.open_buffer_picker();
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    assert_eq!(editor.active_window_id(), window);
    assert!(editor.buffer_picker.is_some());
    assert_eq!(editor.buffer_picker.as_ref().unwrap().selected, 1);
    editor.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), 10);
    assert_eq!(editor.buffer_picker.as_ref().unwrap().selected, 0);
    editor.buffer_picker = None;
    // Mark a terminal as focused without spawning a child: the routing guard
    // must consume its keys even before a PTY session has been attached.
    editor.command("lua remacs.state.toggleterm.active = 'shell'");
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10);
    editor.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), 10);
    assert_eq!(editor.active_window_id(), window);
    assert_eq!(
        editor.toggleterm.active().unwrap().as_deref(),
        Some("shell")
    );
}
