//! Editor integration for Git-managed Lua plugins. Git work stays in the backend;
//! installed versions are loaded once, before the user's configuration.
use super::*;
use mlua::Table;
use plugin_manager::{Event as PluginEvent, Spec};

const HELP: &str = "Plugins: Enter install · Ctrl+U update · Ctrl+R check · Ctrl+S install missing";
const USAGE: &str = "Usage: plugins [add NAME REPO [BRANCH] [ENTRY]|install NAME|sync|update [NAME]|check|remove NAME]";

fn clean_message(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(500)
        .collect()
}

impl Editor {
    pub(super) fn plugin_status_hint(&self) -> String {
        let count = self.managed_plugins.update_count();
        if count > 0 {
            format!(
                " | Plugins ↑{count}{}",
                if self.plugin_restart_required {
                    " · restart"
                } else {
                    ""
                }
            )
        } else if self.plugin_restart_required {
            " | Plugins: restart".into()
        } else if self.managed_plugins.is_busy() {
            " | Plugins …".into()
        } else {
            String::new()
        }
    }

    pub(super) fn plugin_context(&self) -> mlua::Result<Table> {
        let lua = self.lua.lua();
        let result = lua.create_table()?;
        let installed = self.managed_plugins.installed();
        for (index, (name, status)) in self.managed_plugins.entries("").into_iter().enumerate() {
            let info = lua.create_table()?;
            info.set("name", name.as_str())?;
            info.set("status", clean_message(&status))?;
            if let Some(plugin) = installed.iter().find(|plugin| plugin.name == name) {
                info.set("startup_sha", plugin.sha.as_str())?;
            }
            if let Some(error) = self.plugin_load_errors.get(&name) {
                info.set("load_error", error.as_str())?;
            }
            result.set(index + 1, info)?;
        }
        Ok(result)
    }

    pub(super) fn load_managed_plugins(&mut self) {
        let mut installed = self.managed_plugins.installed();
        installed.sort_by(|a, b| a.name.cmp(&b.name));
        self.plugin_load_errors.clear();
        for plugin in &installed {
            if let Err(error) = self.lua.register_managed_plugin_path(&plugin.root) {
                self.plugin_load_errors
                    .insert(plugin.name.clone(), clean_message(&error.to_string()));
            }
        }
        for plugin in installed {
            if self.plugin_load_errors.contains_key(&plugin.name) {
                continue;
            }
            match self.with_lua(10, |runtime| {
                runtime.load_managed_plugin(&plugin.name, &plugin.root, &plugin.entry)
            }) {
                Ok(()) => self.drain_lua_actions(),
                Err(error) => {
                    let _ = self.lua.take_actions();
                    self.plugin_load_errors
                        .insert(plugin.name, clean_message(&error.to_string()));
                }
            }
        }
        if !self.plugin_load_errors.is_empty() {
            self.message = format!(
                "{} plugin(s) could not load; :plugins shows details",
                self.plugin_load_errors.len()
            );
        }
    }

    pub(super) fn poll_plugins(&mut self) {
        for event in self.managed_plugins.poll() {
            self.message = match event {
                PluginEvent::Changed { name, updated } => {
                    self.plugin_restart_required = true;
                    format!(
                        "Plugin {} {name}; restart Remacs to activate",
                        if updated { "updated" } else { "installed" }
                    )
                }
                PluginEvent::UpdatesAvailable(names) => {
                    format!(
                        "Plugins: {} update(s) available · :plugins to review",
                        names.len()
                    )
                }
                PluginEvent::Message(message) => clean_message(&message),
            };
        }
    }

    pub(super) fn open_plugin_picker(&mut self) {
        self.dismiss_completion();
        self.buffer_picker = None;
        self.mason_picker = None;
        self.hover_popup = None;
        self.sequence = Sequence::None;
        self.count = None;
        let _ = self.lua.cancel_keys();
        self.plugin_picker = Some(BufferPicker::default());
        self.message = HELP.into();
    }

    pub(super) fn plugin_view(&self) -> Option<FinderView> {
        let picker = self.plugin_picker.as_ref()?;
        let entries = self.managed_plugins.entries(&picker.query);
        let total = entries.len();
        Some(FinderView {
            query: picker.query.clone(),
            selected: if total == 0 {
                0
            } else {
                picker.selected.min(total - 1) + 1
            },
            matches: entries
                .into_iter()
                .map(|(name, status)| {
                    if let Some(error) = self.plugin_load_errors.get(&name) {
                        clean_message(&format!("{status} · load failed: {error}"))
                    } else {
                        clean_message(&status)
                    }
                })
                .collect(),
            total_matches: total,
            scanned: self.managed_plugins.len(),
        })
    }

    pub(super) fn plugins_command(&mut self, arguments: &str) {
        let args: Vec<_> = arguments.split_whitespace().collect();
        self.message = match args.as_slice() {
            [] => {
                self.open_plugin_picker();
                return;
            }
            ["add", name, repo] | ["add", name, repo, _] | ["add", name, repo, _, _] => {
                let spec = Spec {
                    name: (*name).into(),
                    repo: (*repo).into(),
                    branch: args.get(3).unwrap_or(&"main").to_string(),
                    entry: args.get(4).unwrap_or(&"plugin.lua").to_string(),
                };
                self.managed_plugins.add(spec).unwrap_or_else(|error| error)
            }
            ["install", name] => self
                .managed_plugins
                .queue(name, false)
                .unwrap_or_else(|error| error),
            ["update", name] => self
                .managed_plugins
                .queue(name, true)
                .unwrap_or_else(|error| error),
            ["update"] => format!(
                "Plugins: queued {} updates",
                self.managed_plugins.queue_all(true)
            ),
            ["sync"] => format!(
                "Plugins: queued {} missing installations",
                self.managed_plugins.queue_all(false)
            ),
            ["check"] => format!(
                "Plugins: checking {} repositories",
                self.managed_plugins.check_all()
            ),
            ["remove", name] => match self.managed_plugins.remove(name) {
                Ok(message) => {
                    self.plugin_restart_required = true;
                    message
                }
                Err(error) => error,
            },
            _ => USAGE.into(),
        };
        self.message = clean_message(&self.message);
    }

    pub(super) fn plugin_lua_action(&mut self, action: Table) -> mlua::Result<()> {
        let operation: String = action.get("op")?;
        self.message = match operation.as_str() {
            "add" => self
                .managed_plugins
                .add(Spec {
                    name: action.get("name")?,
                    repo: action.get("repo")?,
                    branch: action.get("branch")?,
                    entry: action.get("entry")?,
                })
                .unwrap_or_else(|error| error),
            "install" => self
                .managed_plugins
                .queue(&action.get::<String>("name")?, false)
                .unwrap_or_else(|error| error),
            "update" => {
                if let Some(name) = action.get::<Option<String>>("name")? {
                    self.managed_plugins
                        .queue(&name, true)
                        .unwrap_or_else(|error| error)
                } else {
                    format!(
                        "Plugins: queued {} updates",
                        self.managed_plugins.queue_all(true)
                    )
                }
            }
            "check" => format!(
                "Plugins: checking {} repositories",
                self.managed_plugins.check_all()
            ),
            "remove" => match self.managed_plugins.remove(&action.get::<String>("name")?) {
                Ok(message) => {
                    self.plugin_restart_required = true;
                    message
                }
                Err(error) => error,
            },
            _ => return Err(mlua::Error::external("unknown plugin operation")),
        };
        self.message = clean_message(&self.message);
        Ok(())
    }

    pub(super) fn plugin_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.plugin_picker.as_ref() else {
            return;
        };
        let entries = self.managed_plugins.entries(&picker.query);
        let selected = entries
            .get(picker.selected.min(entries.len().saturating_sub(1)))
            .map(|(name, _)| name.clone());
        let count = entries.len();
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('u') => {
                    if let Some(name) = selected {
                        self.plugins_command(&format!("update {name}"));
                    }
                }
                KeyCode::Char('r') => self.plugins_command("check"),
                KeyCode::Char('s') => self.plugins_command("sync"),
                KeyCode::Char('l') => {
                    if let Some(picker) = &mut self.plugin_picker {
                        picker.query.clear();
                        picker.selected = 0;
                    }
                }
                _ => {}
            }
            return;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return;
        }
        match key.code {
            KeyCode::Esc => self.plugin_picker = None,
            KeyCode::Enter => {
                if let Some(name) = selected {
                    self.plugins_command(&format!("install {name}"));
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if let Some(picker) = &mut self.plugin_picker {
                    picker.selected = (picker.selected + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if let Some(picker) = &mut self.plugin_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.plugin_picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(ch) => {
                if let Some(picker) = &mut self.plugin_picker {
                    if !ch.is_control() && picker.query.len() + ch.len_utf8() <= 128 {
                        picker.query.push(ch);
                        picker.selected = 0;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        num::NonZeroU16,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "remacs-plugin-ui-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn editor(&self, text: &str) -> Editor {
            let mut doc = Document::from_text(text);
            doc.path = Some(self.0.join("buffer.rs"));
            let mut editor = Editor::new(doc);
            editor.managed_plugins = plugin_manager::Manager::new(self.0.join("plugins"));
            editor.screen.size = Some((120, 24));
            editor
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn press(editor: &mut Editor, code: KeyCode) {
        editor.key(KeyEvent::new(code, KeyModifiers::NONE), 22);
    }

    fn control(editor: &mut Editor, ch: char) {
        editor.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL), 22);
    }

    fn frame(editor: &mut Editor) -> Vec<u8> {
        crossterm::style::force_color_output(true);
        let mut bytes = Vec::new();
        editor.draw_at(&mut bytes, 120, 24).unwrap();
        bytes
    }

    #[test]
    fn empty_plugins_popup_is_visible_owns_tab_and_restores_split_content_on_escape() {
        let project = Project::new();
        let text = (0..50)
            .map(|row| format!("visible underlying row {row:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = project.editor(&text);
        assert!(editor.window_split());
        let window = editor.active_window_id();
        let mut terminal = vt100::Parser::new(
            NonZeroU16::new(24).unwrap(),
            NonZeroU16::new(120).unwrap(),
            0,
        );
        terminal.process(&frame(&mut editor));
        let before: Vec<_> = terminal.screen().rows(0, 120).take(22).collect();
        let revision = editor.doc.revision;

        editor.command("plugins");
        let view = editor.plugin_view().unwrap();
        assert_eq!((view.total_matches, view.scanned, view.selected), (0, 0, 0));
        terminal.process(&frame(&mut editor));
        let visible = terminal.screen().contents();
        assert!(visible.contains("PLUGINS"));
        assert!(visible.contains("No plugins"));
        assert!(visible.contains(":plugins add NAME REPO"));

        press(&mut editor, KeyCode::Tab);
        press(&mut editor, KeyCode::Enter);
        assert_eq!(editor.active_window_id(), window);
        assert_eq!(editor.plugin_picker.as_ref().unwrap().selected, 0);
        assert!(!editor.managed_plugins.is_busy());
        press(&mut editor, KeyCode::Char('j'));
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query, "j");
        control(&mut editor, 'l');
        assert!(editor.plugin_picker.as_ref().unwrap().query.is_empty());
        press(&mut editor, KeyCode::Esc);
        assert!(editor.plugin_picker.is_none());
        assert_eq!(editor.doc.revision, revision);
        assert_eq!(editor.active_window_id(), window);
        let closed = frame(&mut editor);
        assert!(!String::from_utf8_lossy(&closed).contains("\x1b[2J"));
        terminal.process(&closed);
        let after: Vec<_> = terminal.screen().rows(0, 120).take(22).collect();
        assert_eq!(
            after, before,
            "closing the popup must restore both panes and divider"
        );
        assert!(!terminal.screen().contents().contains("PLUGINS"));
    }

    #[test]
    fn plugin_query_typing_and_paste_are_literal_utf8_and_byte_bounded() {
        let project = Project::new();
        let mut editor = project.editor("untouched text");
        let revision = editor.doc.revision;
        editor.command("plugins");
        for _ in 0..42 {
            press(&mut editor, KeyCode::Char('界'));
        }
        press(&mut editor, KeyCode::Char('é'));
        press(&mut editor, KeyCode::Char('🙂'));
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query.len(), 128);
        assert!(editor.plugin_picker.as_ref().unwrap().query.ends_with('é'));
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query.len(), 126);
        editor.pasted("βmore text\nplugins sync");
        let query = &editor.plugin_picker.as_ref().unwrap().query;
        assert_eq!(query.len(), 128);
        assert!(query.ends_with('β'));

        control(&mut editor, 'l');
        editor.pasted("a\u{1b}\u{7}β\rplugins sync");
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query, "aβ");
        editor.pasted(&"🙂".repeat(100));
        let query = &editor.plugin_picker.as_ref().unwrap().query;
        assert_eq!(query, &format!("aβ{}", "🙂".repeat(31)));
        assert!(!query.chars().any(char::is_control));
        assert_eq!(editor.plugin_picker.as_ref().unwrap().selected, 0);
        assert_eq!(editor.doc.lsp_text(), "untouched text");
        assert_eq!(editor.doc.revision, revision);
        assert_eq!(editor.managed_plugins.len(), 0);
        assert!(!editor.managed_plugins.is_busy());
    }

    #[test]
    fn plugin_picker_dismisses_completion_and_keeps_insert_input_out_of_the_document() {
        let project = Project::new();
        let mut editor = project.editor("pri");
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
        editor.show_completions(
            context,
            lsp::Encoding::Utf16,
            completion::parse(&serde_json::json!([{ "label": "printer" }])),
        );
        assert!(editor.completion_menu.is_some());
        editor.completion_due = Some(Instant::now());
        editor.command("plugins");
        assert!(editor.completion_menu.is_none());
        assert!(editor.completion_pending.is_none() && editor.completion_due.is_none());
        assert!(!editor.completion_eligible());
        assert_eq!(editor.mode, Mode::Insert);
        press(&mut editor, KeyCode::Tab);
        press(&mut editor, KeyCode::Null);
        editor.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL), 22);
        press(&mut editor, KeyCode::Char('é'));
        editor.pasted("filter");
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query, "éfilter");
        assert_eq!(editor.doc.lsp_text(), "pri");
        assert!(editor.completion_menu.is_none() && editor.completion_pending.is_none());
        press(&mut editor, KeyCode::Esc);
        assert!(editor.plugin_picker.is_none());
        assert_eq!(editor.mode, Mode::Insert);
        assert!(editor.completion_eligible());
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.doc.lsp_text(), "pri\t");
    }

    #[test]
    fn plugin_popup_retains_visual_and_tree_focus_until_it_closes() {
        let project = Project::new();
        let mut editor = project.editor("first\nsecond\nthird");
        editor.enter_visual();
        let selection = editor.selection;
        editor.command("plugins");
        press(&mut editor, KeyCode::Char('j'));
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.mode, Mode::Visual);
        assert_eq!(editor.selection, selection);
        assert_eq!(editor.doc.row, 0);
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.mode, Mode::Visual);
        assert_eq!(editor.selection, selection);

        editor.leave_visual();
        let mut tree = editor
            .with_lua(22, |_| TreePlugin::new(editor.lua.clone(), &project.0))
            .unwrap();
        tree.toggle().unwrap();
        editor.tree = Some(tree);
        editor.command("plugins");
        press(&mut editor, KeyCode::Char('j'));
        press(&mut editor, KeyCode::Tab);
        assert_eq!(editor.plugin_picker.as_ref().unwrap().query, "j");
        assert!(editor.tree.as_ref().unwrap().focused);
        press(&mut editor, KeyCode::Esc);
        assert!(editor.plugin_picker.is_none());
        assert!(editor.tree.as_ref().unwrap().focused);
    }

    #[test]
    fn prompt_and_terminal_keep_input_priority_over_plugin_picker() {
        let project = Project::new();
        let mut editor = project.editor("untouched");
        assert!(editor.window_split());
        let window = editor.active_window_id();
        editor.command("plugins");
        editor.prompt = Some(Prompt::Command(String::new()));
        press(&mut editor, KeyCode::Char('x'));
        press(&mut editor, KeyCode::Tab);
        assert!(matches!(&editor.prompt, Some(Prompt::Command(value)) if value == "x"));
        assert!(editor.plugin_picker.as_ref().unwrap().query.is_empty());
        press(&mut editor, KeyCode::Esc);
        assert!(editor.prompt.is_none() && editor.plugin_picker.is_some());

        // Exercise the focused-PTY routing guard without starting a child.
        editor.command("lua remacs.state.toggleterm.active = 'shell'");
        press(&mut editor, KeyCode::Char('q'));
        press(&mut editor, KeyCode::Tab);
        press(&mut editor, KeyCode::Esc);
        assert!(editor.plugin_picker.as_ref().unwrap().query.is_empty());
        assert_eq!(editor.active_window_id(), window);
        assert_eq!(editor.doc.lsp_text(), "untouched");
    }

    #[test]
    fn plugin_commands_report_usage_and_unknown_names_without_starting_work() {
        let project = Project::new();
        let mut editor = project.editor("untouched");
        for command in [
            "plugins unknown",
            "plugins add",
            "plugins install",
            "plugins remove",
        ] {
            editor.command(command);
            assert!(
                editor.message.contains("Usage:"),
                "{}: {}",
                command,
                editor.message
            );
            assert!(editor.plugin_picker.is_none());
        }
        for operation in ["install", "update", "remove"] {
            editor.command(&format!("plugins {operation} missing-plugin"));
            assert!(
                editor.message.contains("missing-plugin"),
                "{}",
                editor.message
            );
            assert!(!editor.message.to_lowercase().contains("queued"));
        }
        editor.command("plugins");
        editor.command("plugins install missing-plugin");
        let mut terminal = vt100::Parser::new(
            NonZeroU16::new(24).unwrap(),
            NonZeroU16::new(120).unwrap(),
            0,
        );
        terminal.process(&frame(&mut editor));
        assert!(
            terminal.screen().contents().contains("missing-plugin"),
            "a plugin error must remain visible while the picker is open"
        );
        press(&mut editor, KeyCode::Esc);
        editor.command("plugins add ../escape ./missing-repository");
        assert_ne!(editor.message, USAGE);
        assert!(!editor.message.is_empty());
        assert_eq!(editor.managed_plugins.len(), 0);
        assert!(!editor.managed_plugins.is_busy());
        for command in ["plugins sync", "plugins update", "plugins check"] {
            editor.command(command);
            assert!(editor.message.contains('0'), "{}", editor.message);
            assert!(!editor.managed_plugins.is_busy());
        }
    }

    #[test]
    fn plugin_list_is_a_snapshot_and_failed_callbacks_discard_management_actions() {
        let project = Project::new();
        let mut editor = project.editor("untouched");
        editor
            .with_lua(22, |runtime| {
                runtime.eval(
                    r#"
                local snapshot = remacs.plugins.list()
                assert(#snapshot == 0)
                snapshot[1] = { name = 'local-only', status = 'changed' }
                assert(#remacs.plugins.list() == 0)
                -- Supply one host-context fixture without registering or
                -- installing a repository; both levels must be copied.
                remacs._context.plugins[1] = { name = 'fixture', status = 'registered' }
                local populated = remacs.plugins.list()
                populated[1].name, populated[1].status = 'changed', 'changed'
                assert(remacs.plugins.list()[1].name == 'fixture')
                assert(remacs.plugins.list()[1].status == 'registered')
                remacs._context.plugins[1] = nil
                remacs.commands.add('broken-plugin-management', function()
                    remacs.plugins.add({ name = 'demo', repo = 'https://example.invalid/demo.git' })
                    remacs.plugins.install('demo')
                    remacs.plugins.check()
                    remacs.message('must not escape')
                    error('plugin management callback failed')
                end)
            "#,
                )
            })
            .unwrap();
        editor.drain_lua_actions();
        editor.command("broken-plugin-management");
        assert!(editor.message.contains("plugin management callback failed"));
        assert_eq!(editor.managed_plugins.len(), 0);
        assert!(!editor.managed_plugins.is_busy());
        assert!(editor.lua.take_actions().unwrap().is_empty());
        assert_eq!(editor.doc.lsp_text(), "untouched");
        assert!(!project.0.join("plugins").exists());
    }
}
