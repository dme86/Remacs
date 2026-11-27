//! The editor side of the Lua boundary: short-lived read access, queued native
//! mutations and revision-based events. No Lua callback retains an Editor borrow.
use super::*;
use mlua::{Table, Value};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BufferObservation {
    path: Option<PathBuf>,
    revision: u64,
    dirty: bool,
    row: usize,
    col: usize,
}

impl Editor {
    fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual => "visual",
        }
    }

    fn buffer_document(&self, id: u64) -> Option<&Document> {
        let index = self
            .buffer_ids
            .iter()
            .position(|candidate| *candidate == id)?;
        if index == self.active_slot {
            Some(&self.doc)
        } else {
            self.slots.get(index)?.as_ref().map(|buffer| &buffer.doc)
        }
    }

    fn lua_context(&self, page_height: usize) -> mlua::Result<Table> {
        let lua = self.lua.lua();
        let context = lua.create_table()?;
        context.set("current", self.buffer_ids[self.active_slot])?;
        context.set("mode", self.mode_name())?;
        context.set(
            "count",
            self.count.map(|count| count.min(i64::MAX as usize)),
        )?;
        context.set("page_height", page_height.max(1))?;
        let buffers = lua.create_table()?;
        for (index, id) in self.buffer_ids.iter().enumerate() {
            let doc = if index == self.active_slot {
                &self.doc
            } else {
                &self.slots[index].as_ref().expect("inactive buffer").doc
            };
            let buffer = lua.create_table()?;
            buffer.set("id", *id)?;
            buffer.set(
                "path",
                doc.path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            )?;
            buffer.set("dirty", doc.dirty)?;
            buffer.set("revision", doc.revision)?;
            buffer.set("row", doc.row + 1)?;
            buffer.set("col", doc.col + 1)?;
            buffer.set("line_count", doc.lines.len())?;
            buffers.set(index + 1, buffer)?;
        }
        context.set("buffers", buffers)?;
        context.set("window", self.active_window_id())?;
        let windows = lua.create_table()?;
        for (index, window) in self.windows.iter().enumerate() {
            let active = index == self.active_window;
            let metadata = lua.create_table()?;
            metadata.set("id", window.id)?;
            metadata.set(
                "buffer",
                if active {
                    self.buffer_ids[self.active_slot]
                } else {
                    window.buffer_id
                },
            )?;
            metadata.set("active", active)?;
            metadata.set("row", if active { self.doc.row } else { window.row } + 1)?;
            metadata.set("col", if active { self.doc.col } else { window.col } + 1)?;
            metadata.set(
                "scroll_row",
                if active {
                    self.scroll_row
                } else {
                    window.scroll_row
                } + 1,
            )?;
            metadata.set(
                "scroll_col",
                if active {
                    self.scroll_col
                } else {
                    window.scroll_col
                } + 1,
            )?;
            metadata.set("weight", window.weight)?;
            windows.set(index + 1, metadata)?;
        }
        context.set("windows", windows)?;
        context.set("plugins", self.plugin_context()?)?;
        Ok(context)
    }

    pub(super) fn with_lua<T>(
        &self,
        page_height: usize,
        operation: impl FnOnce(&LuaRuntime) -> mlua::Result<T>,
    ) -> mlua::Result<T> {
        let runtime = self.lua.clone();
        runtime.set_context(self.lua_context(page_height)?)?;
        let api: Table = runtime.lua().globals().get("remacs")?;
        let result = runtime.lua().scope(|scope| {
            let provider = scope.create_function(|lua, id: u64| {
                let doc = self
                    .buffer_document(id)
                    .ok_or_else(|| mlua::Error::external("buffer no longer exists"))?;
                lua.create_sequence_from(
                    doc.lines.iter().map(|line| line.iter().collect::<String>()),
                )
            })?;
            api.set("_get_lines", provider)?;
            operation(&runtime)
        });
        // Do not leave an expired scoped callback reachable between dispatches.
        api.set("_get_lines", Value::Nil)?;
        result
    }

    pub(super) fn dispatch_lua_key(&mut self, key: KeyEvent, page_height: usize) -> bool {
        let Some(name) = key_notation(key) else {
            return false;
        };
        if !self.lua.has_key(self.mode_name(), &name).unwrap_or(true) {
            return false;
        }
        let result = self.with_lua(page_height, |runtime| {
            runtime.dispatch_key(self.mode_name(), &name)
        });
        match result {
            Ok(handled) => {
                if handled {
                    if !self.lua.key_pending() {
                        self.count = None;
                    }
                    self.drain_lua_actions();
                }
                handled
            }
            Err(error) => {
                let _ = self.lua.take_actions();
                let _ = self.lua.cancel_keys();
                self.count = None;
                self.message = format!("Lua key error: {error}");
                true
            }
        }
    }

    pub(super) fn lua_command(&mut self, text: &str) {
        let (name, args) = text
            .split_once(char::is_whitespace)
            .map_or((text, ""), |(name, args)| (name, args.trim()));
        let result = self.with_lua(10, |runtime| match name {
            "lua" => runtime.eval(args).map(|()| true),
            "luafile" => runtime.load_file(Path::new(args)).map(|()| true),
            "reload" if args.is_empty() => runtime.load_config(),
            "reload" => runtime.reload(args).map(|()| true),
            _ => runtime.execute(name, args),
        });
        match result {
            Ok(true) => {
                self.drain_lua_actions();
                self.refresh_lua_catalog();
            }
            Ok(false) if name == "reload" => {
                self.message = "No init.lua configuration found".into()
            }
            Ok(false) => self.message = format!("Unknown command: {text}"),
            Err(error) => {
                let _ = self.lua.take_actions();
                self.message = format!("Lua error: {error}");
            }
        }
    }

    fn refresh_lua_catalog(&mut self) {
        if self.lua_generation == self.lua.generation() {
            return;
        }
        self.lua_generation = self.lua.generation();
        let result = MasonPlugin::new(self.lua.clone())
            .and_then(|plugin| self.mason.reload_catalog(&plugin));
        if let Err(error) = result {
            self.message = format!("Mason catalog: {error}");
        }
        self.lsp_plugin.clear_cache();
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let resolved: Vec<_> = std::iter::once(&self.doc)
            .chain(self.slots.iter().flatten().map(|buffer| &buffer.doc))
            .filter_map(|doc| doc.path.as_ref())
            .map(|path| self.with_lua(10, |_| self.lsp_plugin.server_for(path, &cwd)))
            .collect();
        let configurations: Vec<_> = resolved
            .into_iter()
            .filter_map(|result| match result {
                Ok(config) => config,
                Err(error) => {
                    self.message = format!("LSP configuration: {error}");
                    None
                }
            })
            .collect();
        self.lsp.reconfigure(&configurations);
        self.lsp_document_versions = None;
        self.lsp_present.clear();
        self.hover_popup = None;
        self.dismiss_completion();
    }

    pub(super) fn drain_lua_actions(&mut self) {
        let actions = match self.lua.take_actions() {
            Ok(actions) => actions,
            Err(error) => {
                self.message = format!("Lua actions: {error}");
                return;
            }
        };
        for action in actions {
            if let Err(error) = self.apply_lua_action(action) {
                self.message = format!("Lua action: {error}");
            }
        }
        self.refresh_lua_catalog();
    }

    fn apply_lua_action(&mut self, action: Table) -> mlua::Result<()> {
        let kind: String = action.get("kind")?;
        match kind.as_str() {
            "core" => {
                let name: String = action.get("name")?;
                let args: Option<Table> = action.get("args")?;
                self.native_command(&name, args)?;
            }
            "message" => self.message = action.get("text")?,
            "plugin" => self.plugin_lua_action(action)?,
            "open" => self.open(action.get("path")?),
            "switch" => {
                let id: u64 = action.get("id")?;
                let index = self
                    .buffer_ids
                    .iter()
                    .position(|value| *value == id)
                    .ok_or_else(|| mlua::Error::external("buffer no longer exists"))?;
                self.switch_buffer(index);
            }
            "window" => {
                let op: String = action.get("op")?;
                match op.as_str() {
                    "split" => {
                        let path: Option<String> = action.get("path")?;
                        if self.window_split() {
                            if let Some(path) = path {
                                self.open(path);
                            }
                        }
                    }
                    "focus" | "close" | "only" => {
                        let id: u64 = action.get("id")?;
                        if !self.windows.iter().any(|window| window.id == id) {
                            return Err(mlua::Error::external("window no longer exists"));
                        }
                        match op.as_str() {
                            "focus" => {
                                self.window_focus(id);
                            }
                            "close" => {
                                self.window_close_id(id);
                            }
                            _ => {
                                self.window_only_id(id);
                            }
                        }
                    }
                    "resize" => self.window_resize(action.get("delta")?),
                    "equalize" => self.window_equalize(),
                    _ => return Err(mlua::Error::external("unknown window operation")),
                }
            }
            "close" => {
                let id: u64 = action.get("id")?;
                let index = self
                    .buffer_ids
                    .iter()
                    .position(|value| *value == id)
                    .ok_or_else(|| mlua::Error::external("buffer no longer exists"))?;
                let force = action.get::<Option<bool>>("force")?.unwrap_or(false);
                if index == self.active_slot {
                    self.close_buffer(force);
                } else {
                    let buffer = self.slots[index].as_ref().expect("inactive buffer");
                    if buffer.doc.dirty && !force {
                        self.message = "Buffer modified; save it or use force to discard".into();
                        return Ok(());
                    }
                    self.slots.remove(index);
                    self.buffer_ids.remove(index);
                    if index < self.active_slot {
                        self.active_slot -= 1;
                    }
                    self.last_slot = self.last_slot.and_then(|last| {
                        if last == index {
                            None
                        } else {
                            Some(if last > index { last - 1 } else { last })
                        }
                    });
                    self.repair_window_buffers();
                    self.message = "Buffer closed".into();
                }
            }
            "set_lines" | "insert" | "cursor" => {
                let id: u64 = action.get("id")?;
                let index = self
                    .buffer_ids
                    .iter()
                    .position(|value| *value == id)
                    .ok_or_else(|| mlua::Error::external("buffer no longer exists"))?;
                // Validate every value before taking snapshots or touching text.
                let lines = if kind == "set_lines" {
                    let lines = action
                        .get::<Table>("lines")?
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    if lines.iter().any(|line| line.contains(['\r', '\n'])) {
                        return Err(mlua::Error::external(
                            "buffer lines must not contain newlines",
                        ));
                    }
                    Some(if lines.is_empty() {
                        vec![Vec::new()]
                    } else {
                        lines.iter().map(|line| line.chars().collect()).collect()
                    })
                } else {
                    None
                };
                let text = if kind == "insert" {
                    Some(normalize_pasted_text(&action.get::<String>("text")?))
                } else {
                    None
                };
                let position = if kind == "cursor" {
                    Some((
                        action.get::<usize>("row")?.saturating_sub(1),
                        action.get::<usize>("col")?.saturating_sub(1),
                    ))
                } else {
                    None
                };
                if index == self.active_slot {
                    if kind != "cursor" {
                        self.remember();
                        self.insert_changed = false;
                    }
                    mutate_document(&mut self.doc, lines, text, position);
                    self.dismiss_completion();
                    if self.mode == Mode::Visual {
                        self.leave_visual();
                    }
                } else {
                    let buffer = self.slots[index].as_mut().expect("inactive buffer");
                    if kind != "cursor" {
                        push_snapshot(&mut buffer.undo, Snapshot::capture(&buffer.doc));
                        buffer.redo.clear();
                    }
                    mutate_document(&mut buffer.doc, lines, text, position);
                }
            }
            _ => {
                return Err(mlua::Error::external(format!(
                    "unknown editor action: {kind}"
                )))
            }
        }
        Ok(())
    }

    fn native_command(&mut self, name: &str, args: Option<Table>) -> mlua::Result<()> {
        let args = args.unwrap_or(self.lua.lua().create_table()?);
        let explicit: Option<usize> = args.get("count")?;
        let count = explicit.unwrap_or(1);
        let height = args
            .get::<Option<usize>>("page_height")?
            .unwrap_or(10)
            .max(1);
        match name {
            "save" => match args
                .get::<Option<String>>("path")?
                .filter(|path| !path.is_empty())
            {
                Some(path) => self.set_path_and_save(PathBuf::from(path)),
                None => self.save(),
            },
            "save_all" => self.save_all(),
            "quit" => self.quit(),
            "quit_force" => self.running = false,
            "write_quit" => {
                if self.doc.path.is_some() {
                    self.save();
                    if !self.doc.dirty {
                        self.quit();
                    }
                } else {
                    self.message = "Choose a filename with :w or Space f s first".into();
                }
            }
            "open" => self.open(args.get("path")?),
            "open_prompt" => self.prompt = Some(Prompt::Open(String::new())),
            "command_prompt" => self.prompt = Some(Prompt::Command(String::new())),
            "buffer_picker" => {
                if self.buffer_picker.is_some() {
                    self.buffer_picker = None;
                } else {
                    self.open_buffer_picker();
                }
            }
            "buffer_next" => self.cycle_buffer(true),
            "buffer_previous" => self.cycle_buffer(false),
            "buffer_last" => self.last_buffer(),
            "buffer_close" => {
                self.close_buffer(args.get::<Option<bool>>("force")?.unwrap_or(false))
            }
            "window_split" => {
                let path: Option<String> = args.get("path")?;
                if self.window_split() {
                    if let Some(path) = path {
                        self.open(path);
                    }
                }
            }
            "window_next" => self.window_cycle(true),
            "window_previous" => self.window_cycle(false),
            "window_left" => self.window_direction(false),
            "window_right" => self.window_direction(true),
            "window_close" => {
                self.window_close();
            }
            "window_only" => self.window_only(),
            "window_grow" => {
                self.window_resize(explicit.unwrap_or(5).min(isize::MAX as usize) as isize)
            }
            "window_shrink" => {
                self.window_resize(-(explicit.unwrap_or(5).min(isize::MAX as usize) as isize))
            }
            "window_equalize" => self.window_equalize(),
            "tree.toggle" | "tree_toggle" => self.toggle_tree(),
            "terminal" | "terminal.toggle" => self.toggle_terminal(&args.get::<String>("kind")?),
            "lsp" => self.lsp_action(&args.get::<String>("action")?),
            "lsp.definition" => self.lsp_action("definition"),
            "lsp.hover" => self.lsp_action("hover"),
            "mason" => self.mason_command(&args.get::<Option<String>>("args")?.unwrap_or_default()),
            "plugins" => {
                self.plugins_command(&args.get::<Option<String>>("args")?.unwrap_or_default())
            }
            "select_function" => self.select_function(),
            "undo" => self.undo(),
            "redo" => self.redo(),
            "insert_mode" => self.begin_insert(),
            "append" => {
                if self.doc.col < self.doc.line().len() {
                    self.doc.col += 1;
                }
                self.begin_insert();
            }
            "insert_home" => {
                self.doc.home();
                self.begin_insert();
            }
            "insert_end" => {
                self.doc.end();
                self.begin_insert();
            }
            "open_below" | "open_above" => {
                self.remember();
                if name == "open_below" {
                    self.doc.insert_line_below(Vec::new());
                } else {
                    self.doc.insert_line_above(Vec::new());
                }
                self.begin_insert();
                self.insert_changed = true;
            }
            "left" if self.mode == Mode::Normal => {
                self.doc.col = self.doc.col.saturating_sub(count)
            }
            "right" if self.mode == Mode::Normal => {
                if !self.doc.line().is_empty() {
                    self.doc.col = self
                        .doc
                        .col
                        .saturating_add(count)
                        .min(self.doc.line().len() - 1);
                }
            }
            "left" => self.doc.left(),
            "right" => self.doc.right(),
            "down" if count == 1 => self.doc.down(),
            "up" if count == 1 => self.doc.up(),
            "down" => {
                self.doc.row = self
                    .doc
                    .row
                    .saturating_add(count)
                    .min(self.doc.lines.len() - 1);
                self.doc.col = self.doc.col.min(self.doc.line().len());
            }
            "up" => {
                self.doc.row = self.doc.row.saturating_sub(count);
                self.doc.col = self.doc.col.min(self.doc.line().len());
            }
            "home" => self.doc.home(),
            "end" => {
                self.doc.end();
                if self.mode == Mode::Normal {
                    self.doc.col = self.doc.col.saturating_sub(1);
                }
            }
            "word_forward" | "word_backward" => {
                for _ in 0..count.min(100_000) {
                    if name == "word_forward" {
                        self.doc.word_forward();
                    } else {
                        self.doc.word_backward();
                    }
                }
            }
            "first_line" | "last_line" => {
                self.record_current_jump();
                self.doc.row = explicit.map_or_else(
                    || {
                        if name == "first_line" {
                            0
                        } else {
                            self.doc.lines.len() - 1
                        }
                    },
                    |line| line.saturating_sub(1).min(self.doc.lines.len() - 1),
                );
                self.doc.home();
            }
            "delete_char" => {
                if self.doc.col < self.doc.line().len() {
                    self.remember();
                    let text = self.doc.line()[self.doc.col].to_string();
                    self.doc.delete();
                    self.store_register(Register::Text(text), "Character deleted");
                }
            }
            "delete_line" => {
                self.remember();
                let line = self.doc.delete_line();
                self.store_register(Register::Line(line), "Line deleted");
            }
            "yank_line" => {
                self.store_register(Register::Line(self.doc.line().to_vec()), "Line copied")
            }
            "paste" => self.paste_clipboard(),
            "visual_mode" => self.enter_visual(),
            "visual_yank" | "visual_delete" => {
                if let Some(range) = self
                    .selection
                    .filter(|range| name == "visual_yank" || range.start < range.end)
                {
                    let text = self.doc.text_in_range(range);
                    if name == "visual_delete" && range.start < range.end {
                        self.remember();
                        self.doc.delete_text_range(range);
                    }
                    self.store_register(
                        Register::Text(text),
                        if name == "visual_yank" {
                            "Selection copied"
                        } else {
                            "Selection deleted (u to undo)"
                        },
                    );
                }
                self.leave_visual();
            }
            "scroll_down_half" | "scroll_up_half" | "scroll_down_page" | "scroll_up_page" => {
                let half = name.ends_with("half");
                let amount = if half {
                    explicit.unwrap_or((height / 2).max(1))
                } else {
                    height.saturating_sub(2).max(1).saturating_mul(count)
                };
                self.scroll_lines(name.starts_with("scroll_down"), amount, height);
            }
            "jump_back" => self.jump_back(),
            "jump_forward" => self.jump_forward(),
            "change_back" => self.jump_to_change(true),
            "change_forward" => self.jump_to_change(false),
            "normal_mode" => {
                match self.mode {
                    Mode::Insert => self.leave_insert(),
                    Mode::Visual => self.leave_visual(),
                    _ => {}
                }
                self.sequence = Sequence::None;
                self.count = None;
                self.lua.cancel_keys()?;
            }
            "cancel" => {
                self.sequence = Sequence::None;
                self.count = None;
                self.lua.cancel_keys()?;
                self.message = "Cancelled".into();
            }
            "syntax" => match args.get::<Option<String>>("args")?.as_deref() {
                Some("on") => {
                    self.syntax.set_enabled(true);
                    self.message = "Syntax highlighting enabled".into();
                }
                Some("off") => {
                    self.syntax.set_enabled(false);
                    self.message = "Syntax highlighting disabled".into();
                }
                _ => self.message = "Usage: syntax on|off".into(),
            },
            "jumps" => {
                self.message = format!(
                    "{} jump locations (Ctrl+O older, Alt+Right newer)",
                    self.jumps.entries().len()
                )
            }
            "changes" => {
                self.message = format!(
                    "{} recent edits (g; older, g, newer)",
                    self.jumps.changes().len()
                )
            }
            _ => {
                return Err(mlua::Error::external(format!(
                    "unknown native command: {name}"
                )))
            }
        }
        if self.mode == Mode::Visual
            && matches!(
                name,
                "left"
                    | "right"
                    | "up"
                    | "down"
                    | "home"
                    | "end"
                    | "word_forward"
                    | "word_backward"
            )
        {
            self.update_visual_selection();
        }
        Ok(())
    }

    fn mason_command(&mut self, args: &str) {
        match args.trim() {
            "" => self.open_mason_picker(),
            "sync" | "install" => {
                let count = self.mason.queue_all(false);
                self.message = format!("Mason queued {count} missing servers");
            }
            "update" => {
                let count = self.mason.queue_all(true);
                self.message = format!("Mason queued {count} server updates");
            }
            "log" => {
                self.message = format!(
                    "Mason installation log: {}",
                    self.mason.log_path().display()
                )
            }
            args => {
                if let Some((command, id)) = args.split_once(' ') {
                    if matches!(command, "install" | "update") {
                        self.message = self
                            .mason
                            .queue(id.trim(), command == "update")
                            .unwrap_or_else(|error| error);
                        return;
                    }
                }
                self.message = "Usage: mason [sync|install ID|update [ID]|log]".into();
            }
        }
    }

    pub(super) fn emit_lua(&mut self, name: &str, fields: impl FnOnce(&Table) -> mlua::Result<()>) {
        let result = self.with_lua(10, |runtime| {
            let data = runtime.lua().create_table()?;
            fields(&data)?;
            runtime.emit(name, data)
        });
        if let Err(error) = result {
            self.message = format!("Lua hook ({name}): {error}");
        }
    }

    pub(super) fn sync_lua_events(&mut self) {
        // Hooks may edit text or switch buffers. Publish the next generation
        // after applying queued actions, with a fixed bound on recursive hooks.
        for _ in 0..16 {
            self.repair_window_buffers();
            let window_snapshot: Vec<_> = self
                .windows
                .iter()
                .map(|window| (window.id, window.buffer_id, window.weight))
                .collect();
            let window_changed = self.lua_windows != window_snapshot
                || self.lua_window != Some(self.active_window_id());
            let previous_window = self.lua_window.replace(self.active_window_id());
            self.lua_windows = window_snapshot;
            let observed: HashMap<_, _> = self
                .buffer_ids
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    let doc = if index == self.active_slot {
                        &self.doc
                    } else {
                        &self.slots[index].as_ref().expect("inactive buffer").doc
                    };
                    (
                        *id,
                        BufferObservation {
                            path: doc.path.clone(),
                            revision: doc.revision,
                            dirty: doc.dirty,
                            row: doc.row,
                            col: doc.col,
                        },
                    )
                })
                .collect();
            let previous = std::mem::replace(&mut self.lua_observed, observed.clone());
            let current = self.buffer_ids[self.active_slot];
            let entered = self.lua_current.replace(current) != Some(current);
            let mode_changed = self.lua_mode.replace(self.mode) != Some(self.mode);
            let mut emitted = false;
            for id in previous.keys().filter(|id| !observed.contains_key(id)) {
                self.emit_lua("buffer_close", |data| {
                    data.set("id", *id)?;
                    data.set(
                        "path",
                        previous[id]
                            .path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                    )
                });
                emitted = true;
            }
            for (id, value) in &observed {
                let event = match previous.get(id) {
                    None => Some("buffer_open"),
                    Some(old) if old.revision != value.revision => Some("buffer_change"),
                    _ => None,
                };
                if let Some(event) = event {
                    self.emit_lua(event, |data| {
                        data.set("id", *id)?;
                        data.set(
                            "path",
                            value
                                .path
                                .as_ref()
                                .map(|p| p.to_string_lossy().into_owned()),
                        )?;
                        data.set("revision", value.revision)
                    });
                    emitted = true;
                }
                if previous
                    .get(id)
                    .is_some_and(|old| (old.row, old.col) != (value.row, value.col))
                {
                    self.emit_lua("cursor_move", |data| {
                        data.set("id", *id)?;
                        data.set("row", value.row + 1)?;
                        data.set("col", value.col + 1)
                    });
                    emitted = true;
                }
            }
            for id in std::mem::take(&mut self.lua_saved) {
                let path = observed
                    .get(&id)
                    .and_then(|value| value.path.as_ref())
                    .map(|p| p.to_string_lossy().into_owned());
                self.emit_lua("buffer_saved", |data| {
                    data.set("id", id)?;
                    data.set("path", path)
                });
                emitted = true;
            }
            if entered {
                let path = observed[&current]
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned());
                self.emit_lua("buffer_enter", |data| {
                    data.set("id", current)?;
                    data.set("path", path)
                });
                emitted = true;
            }
            if mode_changed {
                let mode = self.mode_name();
                self.emit_lua("mode_changed", |data| data.set("mode", mode));
                emitted = true;
            }
            if window_changed {
                let current = self.active_window_id();
                let count = self.windows.len();
                self.emit_lua("window_change", |data| {
                    data.set("id", current)?;
                    data.set("current", current)?;
                    data.set("previous", previous_window)?;
                    data.set("count", count)
                });
                emitted = true;
            }
            self.drain_lua_actions();
            if !emitted {
                return;
            }
        }
        let _ = self.lua.take_actions();
        self.message = "Lua hooks stopped: recursive editor changes exceeded 16 rounds".into();
    }

    pub(super) fn load_lua_config(&mut self) {
        self.load_managed_plugins();
        let config_name = self.lua.config_dir().join("init.lua").display().to_string();
        if let Ok(errors) = self.lua.startup_errors() {
            if !errors.is_empty() {
                self.message = format!("Lua plugin fallback: {}", errors.join("; "));
            }
        }
        let result = self.with_lua(10, LuaRuntime::load_config);
        match result {
            Ok(_) => self.drain_lua_actions(),
            Err(error) => {
                let _ = self.lua.take_actions();
                self.message = format!("Lua configuration ({config_name}): {error}");
            }
        }
        if !cfg!(test) {
            self.managed_plugins.check_all();
            // User init.lua can opt into Mason's existing startup policy.
            let result = MasonPlugin::new(self.lua.clone())
                .and_then(|plugin| self.mason.auto_queue(&plugin));
            if let Err(error) = result {
                self.message = format!("Mason startup policy: {error}");
            }
        }
        self.sync_lua_events();
        self.emit_lua("editor_ready", |_| Ok(()));
        self.drain_lua_actions();
    }
}

fn push_snapshot(undo: &mut Vec<Snapshot>, snapshot: Snapshot) {
    let incoming = snapshot.approx_bytes();
    let mut retained: usize = undo.iter().map(Snapshot::approx_bytes).sum();
    while !undo.is_empty()
        && (undo.len() >= MAX_UNDO || retained.saturating_add(incoming) > MAX_UNDO_BYTES)
    {
        retained = retained.saturating_sub(undo.remove(0).approx_bytes());
    }
    undo.push(snapshot);
}

fn mutate_document(
    doc: &mut Document,
    lines: Option<Vec<Vec<char>>>,
    text: Option<String>,
    position: Option<(usize, usize)>,
) {
    if let Some(lines) = lines {
        doc.lines = lines;
        doc.row = doc.row.min(doc.lines.len() - 1);
        doc.col = doc.col.min(doc.line().len());
        doc.dirty = true;
        doc.invalidate_syntax_from(0);
    }
    if let Some(text) = text {
        doc.insert_text(&text);
    }
    if let Some((row, col)) = position {
        doc.row = row.min(doc.lines.len() - 1);
        doc.col = col.min(doc.line().len());
    }
}

fn key_notation(key: KeyEvent) -> Option<String> {
    let name = match key.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Null => "ctrl-space".into(),
        _ => return None,
    };
    let mut prefix = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("ctrl-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("alt-");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(key.code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        prefix.push_str("shift-");
    }
    Some(format!("{prefix}{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "remacs-lua-editor-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
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
        editor.sync_lua_events();
        editor
    }

    fn eval(editor: &mut Editor, source: &str) {
        editor.with_lua(10, |runtime| runtime.eval(source)).unwrap();
        editor.drain_lua_actions();
        editor.sync_lua_events();
    }

    fn press(editor: &mut Editor, key: KeyCode) {
        editor.key(KeyEvent::new(key, KeyModifiers::NONE), 10);
    }

    #[test]
    fn registered_keys_and_commands_share_persistent_state_and_counts() {
        let mut editor = editor("text");
        eval(
            &mut editor,
            r#"
            remacs.state.test_calls = 0
            remacs.commands.add("test-count", function(args, context)
                assert(args == "")
                remacs.state.test_calls = remacs.state.test_calls + 1
                remacs.state.test_count = context.count
            end)
            remacs.keymap.set("normal", "space z", "test-count")
            "#,
        );
        for key in ['3', ' ', 'z'] {
            press(&mut editor, KeyCode::Char(key));
        }
        eval(
            &mut editor,
            "assert(remacs.state.test_calls == 1 and remacs.state.test_count == 3)",
        );
        editor.command("test-count");
        eval(
            &mut editor,
            "assert(remacs.state.test_calls == 2 and remacs.state.test_count == nil)",
        );
        assert_eq!(editor.doc.lsp_text(), "text");
    }

    #[test]
    fn lazy_unicode_buffer_edits_preserve_snapshots_cursor_and_undo() {
        let mut editor = editor("héllo\n界");
        eval(
            &mut editor,
            r#"
            local id = remacs.buffer.current()
            local lines = remacs.buffer.get_lines(id)
            assert(lines[1] == "héllo" and lines[2] == "界")
            assert(remacs.buffer.get_lines(id, 2, 2)[1] == "界")
            remacs.buffer.set_lines(id, { "café", "世界" })
            assert(remacs.buffer.get_lines(id)[1] == "héllo")
            "#,
        );
        assert_eq!(editor.doc.lsp_text(), "café\n世界");
        assert!(editor.doc.dirty);
        assert_eq!(editor.undo.len(), 1);
        editor.command("undo");
        assert_eq!(editor.doc.lsp_text(), "héllo\n界");
        eval(
            &mut editor,
            r#"
            local id = remacs.buffer.current()
            remacs.buffer.set_cursor(id, 2, 2)
            remacs.buffer.insert(id, "🙂")
            "#,
        );
        assert_eq!(editor.doc.lsp_text(), "héllo\n界🙂");
        assert_eq!((editor.doc.row, editor.doc.col), (1, 2));
        editor.command("undo");
        assert_eq!(editor.doc.lsp_text(), "héllo\n界");
    }

    #[test]
    fn editing_an_inactive_buffer_preserves_focus_and_its_own_undo() {
        let project = Project::new();
        let first = project.file("one.txt", "first 界");
        let second = project.file("two.txt", "second café");
        let mut editor = Editor::new(Document::open(first).unwrap());
        let first_id = editor.buffer_ids[editor.active_slot];
        editor.open(second.to_string_lossy().into_owned());
        editor.sync_lua_events();
        let active_id = editor.buffer_ids[editor.active_slot];
        editor
            .lua
            .lua()
            .globals()
            .set("target_id", first_id)
            .unwrap();
        eval(
            &mut editor,
            r#"
            assert(remacs.buffer.get_lines(target_id)[1] == "first 界")
            remacs.buffer.set_lines(target_id, { "changed 🙂" })
            "#,
        );
        assert_eq!(editor.buffer_ids[editor.active_slot], active_id);
        assert_eq!(editor.doc.lsp_text(), "second café");
        assert!(editor.undo.is_empty());
        assert_eq!(
            editor.buffer_document(first_id).unwrap().lsp_text(),
            "changed 🙂"
        );
        eval(&mut editor, "remacs.buffer.switch(target_id)");
        editor.command("undo");
        assert_eq!(editor.doc.lsp_text(), "first 界");
        assert_eq!(editor.buffer_ids[editor.active_slot], first_id);
    }

    #[test]
    fn closed_buffer_ids_are_not_reused_or_silently_retargeted() {
        let mut editor = editor("original");
        let original_id = editor.buffer_ids[editor.active_slot];
        editor
            .lua
            .lua()
            .globals()
            .set("old_id", original_id)
            .unwrap();
        eval(&mut editor, "remacs.buffer.close(old_id, false)");
        assert_ne!(editor.buffer_ids[editor.active_slot], original_id);
        assert!(editor.buffer_document(original_id).is_none());
        editor.command("lua remacs.buffer.set_lines(old_id, {'wrong buffer'})");
        assert!(editor.message.contains("buffer no longer exists"));
        assert_eq!(editor.doc.lsp_text(), "");
        eval(
            &mut editor,
            "assert(#remacs.buffer.list() == 1 and remacs.buffer.current() ~= old_id)",
        );
    }

    #[test]
    fn save_hooks_receive_buffer_identity_for_dirty_and_clean_writes() {
        let project = Project::new();
        let path = project.file("saved.txt", "initial");
        let mut editor = Editor::new(Document::open(path.clone()).unwrap());
        editor.sync_lua_events();
        editor
            .lua
            .lua()
            .globals()
            .set("expected_path", path.to_string_lossy().into_owned())
            .unwrap();
        eval(
            &mut editor,
            r#"
            remacs.state.saved_count = 0
            remacs.hooks.add("buffer_saved", "test.saved", function(event)
                assert(event.id == remacs.buffer.current())
                assert(event.path == expected_path)
                remacs.state.saved_count = remacs.state.saved_count + 1
            end)
            remacs.buffer.insert(remacs.buffer.current(), "changed ")
            "#,
        );
        editor.command("w");
        assert!(!editor.doc.dirty);
        editor.command("w");
        eval(&mut editor, "assert(remacs.state.saved_count == 2)");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "changed initial");
    }

    #[test]
    fn failed_plugin_command_discards_queued_text_mutations() {
        let mut editor = editor("keep this 界");
        eval(
            &mut editor,
            r#"
            remacs.commands.add("broken-plugin", function()
                remacs.buffer.set_lines(remacs.buffer.current(), { "discard this" })
                error("plugin failure")
            end)
            "#,
        );
        editor.command("broken-plugin");
        assert!(editor.message.contains("plugin failure"));
        assert_eq!(editor.doc.lsp_text(), "keep this 界");
        assert!(editor.undo.is_empty());
        eval(
            &mut editor,
            "remacs.buffer.insert(remacs.buffer.current(), 'still working ')",
        );
        assert_eq!(editor.doc.lsp_text(), "still working keep this 界");
    }

    #[test]
    fn tree_callbacks_and_following_hooks_have_fresh_lazy_buffer_access() {
        let project = Project::new();
        let first = project.file("old.txt", "old contents");
        let second = project.file("fresh.txt", "fresh 界");
        let mut editor = Editor::new(Document::open(first).unwrap());
        editor.sync_lua_events();
        editor.toggle_tree();
        eval(
            &mut editor,
            r#"
            local tree = require("remacs.tree")
            tree.on_key = function(key)
                assert(key == "j")
                local id = remacs.buffer.current()
                assert(id == expected_id)
                assert(remacs.buffer.get(id).path == expected_path)
                assert(remacs.buffer.get_lines(id)[1] == "fresh 界")
                remacs.state.tree_buffer_id = id
                remacs.buffer.insert(id, "tree ")
                return { action = "editor" }
            end
            remacs.hooks.add("buffer_change", "test.lazy-hook", function(event)
                assert(event.id == remacs.buffer.current())
                remacs.state.hook_line = remacs.buffer.get_lines(event.id)[1]
            end)
            "#,
        );
        // Switch without another Lua command: the adapter must refresh the
        // context instead of reusing metadata from the previous buffer.
        editor.open(second.to_string_lossy().into_owned());
        let current = editor.buffer_ids[editor.active_slot];
        editor
            .lua
            .lua()
            .globals()
            .set("expected_id", current)
            .unwrap();
        editor
            .lua
            .lua()
            .globals()
            .set("expected_path", second.to_string_lossy().into_owned())
            .unwrap();
        editor.tree.as_mut().unwrap().focus();
        press(&mut editor, KeyCode::Char('j'));
        assert_eq!(editor.doc.lsp_text(), "tree fresh 界");
        assert!(!editor.tree.as_ref().unwrap().focused);
        editor.command("lua remacs.buffer.insert(remacs.buffer.current(), 'command ')");
        assert_eq!(editor.doc.lsp_text(), "tree command fresh 界");
        eval(
            &mut editor,
            r#"
            assert(remacs.state.tree_buffer_id == expected_id)
            assert(remacs.state.hook_line == "tree command fresh 界")
            "#,
        );
    }

    #[test]
    fn file_reload_removes_stale_bindings_and_rolls_back_failed_changes() {
        let project = Project::new();
        let plugin = project.file(
            "test-plugin.lua",
            r#"
            remacs.state.reload_test = remacs.state.reload_test or { loads = 0 }
            remacs.state.reload_test.loads = remacs.state.reload_test.loads + 1
            remacs.commands.add("old-plugin", function() remacs.message("old") end)
            remacs.keymap.set("normal", "space z", "old-plugin")
            remacs.keymap.set("normal", "space y", "old-plugin")
            "#,
        );
        let mut editor = editor("unchanged");
        editor.command(&format!("luafile {}", plugin.display()));
        for key in [' ', 'z'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.message, "old");
        std::fs::write(
            &plugin,
            r#"
            remacs.state.reload_test.loads = remacs.state.reload_test.loads + 1
            remacs.commands.add("new-plugin", function() remacs.message("new") end)
            remacs.keymap.set("normal", "space z", "new-plugin")
            "#,
        )
        .unwrap();
        editor.command(&format!("reload {}", plugin.display()));
        eval(
            &mut editor,
            "assert(not remacs.commands.run('old-plugin')); assert(remacs.keymap.list()['normal:space y'] == nil); assert(remacs.state.reload_test.loads == 2)",
        );
        for key in [' ', 'z'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.message, "new");
        std::fs::write(
            &plugin,
            r#"
            remacs.commands.add("new-plugin", function() remacs.message("broken") end)
            remacs.buffer.set_lines(remacs.buffer.current(), { "bad reload" })
            error("reload rejected")
            "#,
        )
        .unwrap();
        editor.command(&format!("reload {}", plugin.display()));
        assert!(editor.message.contains("reload rejected"));
        assert_eq!(editor.doc.lsp_text(), "unchanged");
        for key in [' ', 'z'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.message, "new");
        eval(&mut editor, "assert(remacs.state.reload_test.loads == 2)");
    }
    #[test]
    fn deleting_empty_visual_selection_preserves_register_and_clipboard() {
        let mut editor = Editor::new(Document::empty());
        editor.store_register(Register::Text("keep".into()), "Copied");
        editor.enter_visual();
        editor.command("visual_delete");
        assert_eq!(editor.clipboard.read().unwrap(), "keep");
        assert!(matches!(editor.register, Some(Register::Text(ref text)) if text == "keep"));
        assert!(editor.undo.is_empty());
    }

    #[test]
    fn closing_inactive_buffer_preserves_insert_mode_and_active_cursor() {
        let mut editor = Editor::new(Document::from_text("first"));
        let previous = editor.buffer_ids[0];
        let root =
            std::env::temp_dir().join(format!("remacs-close-inactive-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        editor.open(root.join("second.txt").to_string_lossy().into_owned());
        let active = editor.buffer_ids[editor.active_slot];
        editor.begin_insert();
        editor.doc.insert_text("text");
        editor.command(&format!("lua remacs.buffer.close({previous})"));
        assert_eq!(editor.buffer_ids, [active]);
        assert_eq!(editor.mode, Mode::Insert);
        assert_eq!((editor.doc.row, editor.doc.col), (0, 4));
        assert_eq!(editor.doc.lsp_text(), "text");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lua_window_metadata_and_operations_use_deferred_stable_views() {
        let mut editor = editor("hello\nworld");
        editor.screen.size = Some((120, 24));
        eval(
            &mut editor,
            r#"
            remacs.state.original_window = remacs.window.current()
            local windows = remacs.window.list()
            assert(#windows == 1 and windows[1].active)
            assert(windows[1].buffer == remacs.buffer.current())
            assert(windows[1].row == 1 and windows[1].col == 1)
            windows[1].weight = 999999
            remacs.window.split()
            assert(#remacs.window.list() == 1, "writes are deferred")
        "#,
        );
        assert_eq!(editor.windows.len(), 2);
        assert_eq!(
            editor.buffer_ids.len(),
            1,
            "views must share buffer storage"
        );
        eval(
            &mut editor,
            r#"
            local windows = remacs.window.list()
            assert(#windows == 2 and windows[2].active)
            assert(windows[1].id ~= windows[2].id)
            assert(windows[1].buffer == windows[2].buffer)
            assert(windows[1].weight ~= 999999)
            remacs.window.focus(remacs.state.original_window)
        "#,
        );
        assert_eq!(editor.active_window, 0);
        eval(
            &mut editor,
            "assert(remacs.window.current() == remacs.state.original_window)",
        );
    }

    #[test]
    fn split_views_share_lua_edits_and_buffer_undo() {
        let mut editor = editor("original");
        editor.screen.size = Some((120, 24));
        let first = editor.active_window_id();
        editor.command("vsplit");
        eval(&mut editor, "remacs.buffer.insert(nil, 'new ')");
        assert_eq!(editor.doc.lsp_text(), "new original");
        assert!(editor.window_focus(first));
        assert_eq!(editor.doc.lsp_text(), "new original");
        editor.command("undo");
        assert_eq!(editor.doc.lsp_text(), "original");
        assert_eq!(editor.windows.len(), 2);
        assert_eq!(editor.buffer_ids.len(), 1);
    }

    #[test]
    fn split_file_and_close_window_keep_modified_buffer_and_quit_protection() {
        let project = Project::new();
        let first = project.file("one.txt", "first");
        let second = project.file("two.txt", "second");
        let mut editor = Editor::new(Document::open(first).unwrap());
        editor.screen.size = Some((120, 24));
        editor.command(&format!("vs {}", second.display()));
        assert_eq!(editor.windows.len(), 2);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("two.txt"));
        eval(&mut editor, "remacs.buffer.insert(nil, 'changed ')");
        editor.command("close");
        assert_eq!(editor.windows.len(), 1);
        assert_eq!(editor.buffer_ids.len(), 2);
        assert!(editor.doc.path.as_ref().unwrap().ends_with("one.txt"));
        assert_eq!(editor.dirty_buffers(), 1);
        editor.command("q");
        assert!(matches!(editor.prompt, Some(Prompt::Quit)));
        assert!(editor.running);
    }

    #[test]
    fn window_api_inactive_close_preserves_insert_mode_and_active_view() {
        let mut editor = editor("hello");
        editor.screen.size = Some((120, 24));
        let first = editor.active_window_id();
        assert!(editor.window_split());
        let active = editor.active_window_id();
        editor.begin_insert();
        editor.doc.col = 3;
        editor.command(&format!("lua remacs.window.close({first})"));
        assert_eq!(editor.windows.len(), 1);
        assert_eq!(editor.active_window_id(), active);
        assert_eq!(editor.mode, Mode::Insert);
        assert_eq!((editor.doc.row, editor.doc.col), (0, 3));
        editor.command("close");
        assert_eq!(editor.windows.len(), 1, "last view stays open");
        assert!(editor.running);
    }

    #[test]
    fn window_commands_bindings_and_change_hooks_use_public_lua_registry() {
        let mut editor = editor("hello");
        editor.screen.size = Some((140, 24));
        eval(
            &mut editor,
            r#"
            remacs.state.window_events = {}
            remacs.hooks.add("window_change", "test.windows", function(event)
                assert(event.current == remacs.window.current())
                assert(event.count == #remacs.window.list())
                table.insert(remacs.state.window_events, event.count)
            end)
        "#,
        );
        for key in [' ', 'w', 'v'] {
            press(&mut editor, KeyCode::Char(key));
        }
        assert_eq!(editor.windows.len(), 2);
        let right = editor.active_window_id();
        editor.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), 10);
        press(&mut editor, KeyCode::Char('o'));
        assert_ne!(editor.active_window_id(), right);
        editor.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL), 10);
        press(&mut editor, KeyCode::Char('l'));
        assert_eq!(editor.active_window_id(), right);
        editor.command("wincmd v");
        assert_eq!(editor.windows.len(), 3);
        editor.command("only");
        assert_eq!(editor.windows.len(), 1);
        editor.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), 10);
        press(&mut editor, KeyCode::Char('3'));
        assert_eq!(editor.windows.len(), 2);
        editor.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), 10);
        editor.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 10);
        assert!(
            matches!(editor.prompt, Some(Prompt::Open(_))),
            "legacy C-x C-f remains intact"
        );
        editor.prompt = None;
        eval(&mut editor, "assert(#remacs.state.window_events >= 5)");
    }

    #[test]
    fn failing_lua_window_command_discards_queued_split_and_invalid_id_is_safe() {
        let mut editor = editor("hello");
        editor.screen.size = Some((120, 24));
        eval(
            &mut editor,
            r#"
            remacs.commands.add("bad-window", function()
                remacs.window.split()
                error("do not apply")
            end)
        "#,
        );
        editor.command("bad-window");
        assert_eq!(editor.windows.len(), 1);
        assert!(editor.message.contains("do not apply"));
        editor.command("lua remacs.window.focus(999999)");
        assert_eq!(editor.windows.len(), 1);
        assert!(editor.message.contains("window no longer exists"));
    }
}
