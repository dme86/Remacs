//! One persistent Lua VM for the entire editor. Clones are handles to the same
//! VM, not new interpreters. All editor writes cross a deferred action queue.
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use mlua::{Function, HookTriggers, Lua, Table, VmState};

const API: &str = include_str!("../lua/api.lua");
const INSTRUCTION_LIMIT: u64 = 20_000_000;
const CALL_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct LuaRuntime {
    lua: Lua,
    generation: Rc<Cell<u64>>,
    started: Rc<Cell<Instant>>,
    instructions: Rc<Cell<u64>>,
    config_dir: Rc<PathBuf>,
}

impl LuaRuntime {
    pub fn new(root: &Path) -> mlua::Result<Self> {
        let config_dir = if cfg!(test) {
            std::env::temp_dir().join(format!("remacs-no-user-config-{}", std::process::id()))
        } else {
            default_config_dir()
        };
        Self::with_config_dir(root, config_dir)
    }

    /// An explicit configuration location also keeps tests independent of the
    /// real user's configuration and environment.
    pub fn with_config_dir(root: &Path, config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let runtime = Self {
            lua,
            generation: Rc::new(Cell::new(0)),
            started: Rc::new(Cell::new(Instant::now())),
            instructions: Rc::new(Cell::new(0)),
            config_dir: Rc::new(config_dir),
        };
        let started = runtime.started.clone();
        let instructions = runtime.instructions.clone();
        runtime.lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(10_000),
            move |_, _| {
                let used = instructions.get().saturating_add(10_000);
                instructions.set(used);
                if used > INSTRUCTION_LIMIT || started.get().elapsed() > CALL_TIMEOUT {
                    return Err(mlua::Error::RuntimeError(
                        "Lua callback exceeded its execution budget".into(),
                    ));
                }
                Ok(VmState::Continue)
            },
        )?;
        runtime.install_host(root)?;
        runtime.lua.load(API).set_name("remacs/api.lua").exec()?;
        runtime.install_loaders()?;
        let mut errors = Vec::new();
        for name in ["mason", "lsp", "toggleterm", "tree", "defaults"] {
            if let Err(error) = runtime.load_module(name, false) {
                errors.push(format!("{name}: {error}"));
                let embedded_only: Table = runtime.api()?.get("_embedded_only")?;
                embedded_only.set(name, true)?;
                runtime.load_module(name, false)?;
                embedded_only.set(name, mlua::Value::Nil)?;
            }
        }
        runtime
            .api()?
            .set("_startup_errors", runtime.lua.create_sequence_from(errors)?)?;
        Ok(runtime)
    }

    fn install_host(&self, root: &Path) -> mlua::Result<()> {
        let remacs = self.lua.create_table()?;
        remacs.set("root", root.to_string_lossy().into_owned())?;
        remacs.set("config_dir", self.config_dir.to_string_lossy().into_owned())?;
        remacs.set("_embedded_only", self.lua.create_table()?)?;
        let filesystem = self.lua.create_table()?;
        filesystem.set(
            "list",
            self.lua.create_function(|lua, path: String| {
                let mut entries = Vec::new();
                for entry in fs::read_dir(path).map_err(mlua::Error::external)? {
                    let entry = entry.map_err(mlua::Error::external)?;
                    let is_dir = entry.file_type().map_err(mlua::Error::external)?.is_dir();
                    entries.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path().to_string_lossy().into_owned(),
                        is_dir,
                    ));
                }
                entries.sort_by(|a, b| {
                    b.2.cmp(&a.2)
                        .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
                });
                let result = lua.create_table()?;
                for (index, (name, path, is_dir)) in entries.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("name", name)?;
                    entry.set("path", path)?;
                    entry.set("is_dir", is_dir)?;
                    result.set(index + 1, entry)?;
                }
                Ok(result)
            })?,
        )?;
        remacs.set("fs", filesystem)?;
        self.lua.globals().set("remacs", remacs)?;
        Ok(())
    }

    fn install_loaders(&self) -> mlua::Result<()> {
        let package: Table = self.lua.globals().get("package")?;
        let preload: Table = package.get("preload")?;
        let config_lua = self.config_dir.join("lua");
        let existing: String = package.get("path")?;
        package.set(
            "path",
            format!(
                "{}/?.lua;{}/?/init.lua;{}",
                config_lua.display(),
                config_lua.display(),
                existing
            ),
        )?;
        let mut paths = vec![self.config_dir.join("plugins")];
        if !cfg!(test) {
            if let Some(value) = std::env::var_os("REMACS_RUNTIME_PATH") {
                paths.extend(std::env::split_paths(&value));
            }
        }
        // Development checkout fallback. Installed executables retain embedded
        // defaults if the checkout no longer exists.
        paths.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"));
        for (name, embedded) in [
            ("mason", include_str!("../plugins/mason.lua")),
            ("lsp", include_str!("../plugins/lsp.lua")),
            ("toggleterm", include_str!("../plugins/toggleterm.lua")),
            ("tree", include_str!("../plugins/tree.lua")),
            ("defaults", include_str!("../lua/defaults.lua")),
        ] {
            let mut paths = paths.clone();
            if name == "defaults" {
                paths.pop();
                paths.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua"));
            }
            let module_name = format!("remacs.{name}");
            let registered_name = module_name.clone();
            let loader = self.lua.create_function(move |lua, _: mlua::MultiValue| {
                let remacs: Table = lua.globals().get("remacs")?;
                let embedded_only: Table = remacs.get("_embedded_only")?;
                let skip = embedded_only.get::<Option<bool>>(name)?.unwrap_or(false);
                let found = if skip {
                    None
                } else {
                    paths
                        .iter()
                        .map(|dir| dir.join(format!("{name}.lua")))
                        .find(|path| path.is_file())
                };
                let (source, label) = match found {
                    Some(path) => (
                        fs::read_to_string(&path).map_err(mlua::Error::external)?,
                        path.display().to_string(),
                    ),
                    None => (embedded.to_owned(), format!("builtin/{name}.lua")),
                };
                let chunk = lua.load(&source).set_name(&label).into_function()?;
                let module: Table = remacs
                    .get::<Function>("_with_owner")?
                    .call((module_name.clone(), chunk))?;
                let functions: &[&str] = match name {
                    "tree" => &["setup", "on_key", "paste_query", "render"],
                    "toggleterm" => &["binding", "dismiss", "active", "toggle", "hide", "exited"],
                    "lsp" => &["binding", "server_for"],
                    _ => &[],
                };
                for function in functions {
                    module.get::<Function>(*function)?;
                }
                if name == "mason" {
                    crate::mason_plugin::MasonPlugin::validate_table(&module)?;
                }
                Ok(module)
            })?;
            preload.set(registered_name, loader)?;
        }
        // Use weak Lua handles in host callbacks to avoid a VM -> callback ->
        // VM reference cycle. The runtime itself owns the generation counters.
        let weak = self.lua.weak();
        let generation = self.generation.clone();
        let started = self.started.clone();
        let instructions = self.instructions.clone();
        let config_dir = self.config_dir.clone();
        let load = self
            .lua
            .create_function(move |_, (name, reload): (String, bool)| {
                let runtime = Self {
                    lua: weak.upgrade(),
                    generation: generation.clone(),
                    started: started.clone(),
                    instructions: instructions.clone(),
                    config_dir: config_dir.clone(),
                };
                if Path::new(&name).is_file() || name.ends_with(".lua") {
                    runtime.load_file(Path::new(&name))?;
                    Ok(())
                } else {
                    runtime.load_module(&name, reload)
                }
            })?;
        let weak = self.lua.weak();
        let generation = self.generation.clone();
        let started = self.started.clone();
        let instructions = self.instructions.clone();
        let config_dir = self.config_dir.clone();
        let config = self.lua.create_function(move |_, ()| {
            let runtime = Self {
                lua: weak.upgrade(),
                generation: generation.clone(),
                started: started.clone(),
                instructions: instructions.clone(),
                config_dir: config_dir.clone(),
            };
            runtime.load_config()
        })?;
        let remacs = self.api()?;
        remacs.set("_host_load", load)?;
        remacs.set("_host_config", config)?;
        Ok(())
    }

    fn reset_budget(&self) {
        self.started.set(Instant::now());
        self.instructions.set(0);
    }

    fn api(&self) -> mlua::Result<Table> {
        self.lua.globals().get("remacs")
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    pub fn config_dir(&self) -> &Path {
        self.config_dir.as_path()
    }

    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    pub fn startup_errors(&self) -> mlua::Result<Vec<String>> {
        self.api()?
            .get::<Table>("_startup_errors")?
            .sequence_values::<String>()
            .collect()
    }

    pub fn table(&self, name: &str) -> mlua::Result<Table> {
        self.reset_budget();
        let name = if name.starts_with("remacs.") {
            name.to_owned()
        } else {
            format!("remacs.{name}")
        };
        self.lua.globals().get::<Function>("require")?.call(name)
    }

    pub fn eval(&self, source: &str) -> mlua::Result<()> {
        self.reset_budget();
        self.api()?.get::<Function>("_eval")?.call(source)
    }

    fn load_module(&self, name: &str, reload: bool) -> mlua::Result<()> {
        self.reset_budget();
        self.api()?
            .get::<Function>("_load_module")?
            .call::<mlua::Value>((name, reload))?;
        self.generation.set(self.generation.get().wrapping_add(1));
        Ok(())
    }

    pub fn load_file(&self, path: &Path) -> mlua::Result<()> {
        let path = path.canonicalize().map_err(mlua::Error::external)?;
        let source = fs::read_to_string(&path).map_err(mlua::Error::external)?;
        self.reset_budget();
        self.api()?
            .get::<Function>("_load_file")?
            .call::<mlua::Value>((path.to_string_lossy().into_owned(), source))?;
        self.generation.set(self.generation.get().wrapping_add(1));
        Ok(())
    }

    pub fn reload(&self, name: &str) -> mlua::Result<()> {
        if Path::new(name).is_file() || name.ends_with(".lua") {
            self.load_file(Path::new(name))
        } else {
            self.load_module(name, true)
        }
    }

    pub fn load_config(&self) -> mlua::Result<bool> {
        let path = self.config_dir.join("init.lua");
        if !path.is_file() {
            return Ok(false);
        }
        self.load_file(&path)?;
        Ok(true)
    }

    pub fn set_context(&self, context: Table) -> mlua::Result<()> {
        self.api()?.set("_context", context)
    }

    pub fn dispatch_key(&self, mode: &str, key: &str) -> mlua::Result<bool> {
        self.reset_budget();
        self.api()?
            .get::<Function>("_dispatch_key")?
            .call((mode, key))
    }

    pub fn has_key(&self, mode: &str, key: &str) -> mlua::Result<bool> {
        self.reset_budget();
        self.api()?.get::<Function>("_has_key")?.call((mode, key))
    }

    pub fn key_pending(&self) -> bool {
        self.api()
            .and_then(|api| api.get("_key_pending"))
            .unwrap_or(false)
    }

    pub fn pending_keys(&self) -> String {
        self.api()
            .and_then(|api| api.get("_key_sequence"))
            .unwrap_or_default()
    }

    pub fn cancel_keys(&self) -> mlua::Result<()> {
        self.api()?.get::<Function>("_cancel_keys")?.call(())
    }

    pub fn execute(&self, name: &str, args: &str) -> mlua::Result<bool> {
        self.reset_budget();
        self.api()?
            .get::<Table>("commands")?
            .get::<Function>("run")?
            .call((name, args))
    }

    pub fn emit(&self, event: &str, data: Table) -> mlua::Result<()> {
        self.reset_budget();
        self.api()?.get::<Function>("emit")?.call((event, data))
    }

    pub fn take_actions(&self) -> mlua::Result<Vec<Table>> {
        self.api()?
            .get::<Function>("_take_actions")?
            .call::<Table>(())?
            .sequence_values::<Table>()
            .collect()
    }
}

fn default_config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("REMACS_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("remacs");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/remacs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "remacs-lua-runtime-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn runtime(&self) -> LuaRuntime {
            LuaRuntime::with_config_dir(&self.0, self.0.clone()).unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn action_name(runtime: &LuaRuntime) -> String {
        let actions = runtime.take_actions().unwrap();
        assert_eq!(actions.len(), 1);
        actions[0].get("name").unwrap()
    }

    #[test]
    fn handles_and_modules_share_one_persistent_vm() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        let clone = runtime.clone();
        runtime
            .eval("counter = 41; require('remacs.mason').marker = 'shared'")
            .unwrap();
        clone
            .eval("counter = counter + 1; assert(require('remacs.mason').marker == 'shared')")
            .unwrap();
        assert_eq!(runtime.lua().globals().get::<i64>("counter").unwrap(), 42);
        assert_eq!(
            runtime
                .table("mason")
                .unwrap()
                .get::<String>("marker")
                .unwrap(),
            "shared"
        );
    }

    #[test]
    fn default_sequences_preserve_count_context_and_command_arguments() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        let context = runtime.lua().create_table().unwrap();
        context.set("count", 7).unwrap();
        context.set("page_height", 15).unwrap();
        runtime.set_context(context).unwrap();
        for (mode, keys, action) in [
            ("normal", vec!["g", "g"], "first_line"),
            ("normal", vec!["d", "d"], "delete_line"),
            ("normal", vec!["space", "b", "s"], "save_all"),
            ("insert", vec!["ctrl-x", "ctrl-s"], "save"),
            ("visual", vec!["a", "f"], "select_function"),
        ] {
            let length = keys.len();
            for (index, key) in keys.into_iter().enumerate() {
                assert!(runtime.dispatch_key(mode, key).unwrap());
                assert_eq!(runtime.key_pending(), index + 1 < length);
            }
            let actions = runtime.take_actions().unwrap();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].get::<String>("name").unwrap(), action);
            let args: Table = actions[0].get("args").unwrap();
            assert_eq!(args.get::<usize>("count").unwrap(), 7);
            assert_eq!(args.get::<usize>("page_height").unwrap(), 15);
        }
        assert!(runtime.execute("w", "other file.md").unwrap());
        let actions = runtime.take_actions().unwrap();
        assert_eq!(
            actions[0]
                .get::<Table>("args")
                .unwrap()
                .get::<String>("path")
                .unwrap(),
            "other file.md"
        );
        assert!(!runtime.execute("missing-command", "").unwrap());
    }

    #[test]
    fn custom_keymaps_override_modes_and_cancel_prefixes() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        runtime.eval("remacs.keymap.set('global', 'space z', function() remacs.message('global') end); remacs.keymap.set('normal', 'space z', function() remacs.message('normal') end)").unwrap();
        assert!(runtime.has_key("normal", "space").unwrap());
        assert!(!runtime.has_key("insert", "a").unwrap());
        assert!(runtime.dispatch_key("normal", "space").unwrap());
        assert_eq!(runtime.pending_keys(), "space");
        assert!(runtime.dispatch_key("normal", "z").unwrap());
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "normal"
        );
        runtime.dispatch_key("normal", "space").unwrap();
        runtime.cancel_keys().unwrap();
        assert!(!runtime.key_pending());
        assert!(!runtime.dispatch_key("normal", "z").unwrap());
        runtime.dispatch_key("insert", "space").unwrap();
        runtime.dispatch_key("insert", "z").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "global"
        );
    }

    #[test]
    fn direct_modified_shortcuts_preempt_prefixes_without_preempting_unmapped_custom_sequences() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        for (prefix, key, expected) in [
            ("space", "ctrl-n", "tree_toggle"),
            ("space", "ctrl-p", "buffer_picker"),
            ("space", "ctrl-t", "terminal"),
            ("g", "ctrl-d", "scroll_down_half"),
            ("space", "alt-j", "buffer_next"),
        ] {
            assert!(runtime.dispatch_key("normal", prefix).unwrap());
            assert!(runtime.key_pending());
            assert!(runtime.dispatch_key("normal", key).unwrap());
            assert!(!runtime.key_pending());
            assert_eq!(action_name(&runtime), expected);
        }
        runtime.eval("remacs.keymap.del('global', 'ctrl-n'); remacs.keymap.set('normal', 'space ctrl-n', function() remacs.message('custom sequence') end)").unwrap();
        runtime.dispatch_key("normal", "space").unwrap();
        runtime.dispatch_key("normal", "ctrl-n").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "custom sequence"
        );
        assert!(!runtime.key_pending());
    }

    #[test]
    fn failed_callbacks_discard_actions_and_hooks_continue_in_stable_order() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        runtime.eval("remacs.commands.add('broken', function() remacs.message('discard'); error('failure') end); remacs.hooks.add('test', 'a', function() remacs.message('discard'); error('failure') end); remacs.hooks.add('test', 'b', function() remacs.message('kept') end)").unwrap();
        assert!(runtime.execute("broken", "").is_err());
        assert!(runtime.take_actions().unwrap().is_empty());
        assert!(runtime
            .eval("remacs.message('discard'); error('failure')")
            .is_err());
        assert!(runtime.take_actions().unwrap().is_empty());
        assert!(runtime
            .emit("test", runtime.lua().create_table().unwrap())
            .is_err());
        let actions = runtime.take_actions().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].get::<String>("text").unwrap(), "kept");
    }

    #[test]
    fn file_reload_replaces_owned_registrations_and_rolls_back_errors() {
        let dir = TestDir::new();
        let path = dir.0.join("plugin.lua");
        let runtime = dir.runtime();
        fs::write(&path, "remacs.commands.add('test', function() remacs.message('old') end); remacs.keymap.set('normal', 'space z', 'test'); remacs.hooks.add('saved', 'test', function() remacs.message('hook') end)").unwrap();
        runtime.load_file(&path).unwrap();
        let generation = runtime.generation();
        fs::write(&path, "remacs.commands.add('test', function() remacs.message('broken') end); remacs.commands.add('leaked', function() end); remacs.message('discard'); error('reload failed')").unwrap();
        assert!(runtime.load_file(&path).is_err());
        assert_eq!(runtime.generation(), generation);
        assert!(runtime.take_actions().unwrap().is_empty());
        assert!(!runtime.execute("leaked", "").unwrap());
        runtime.execute("test", "").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "old"
        );
        fs::write(
            &path,
            "remacs.commands.add('new_test', function() remacs.message('new') end)",
        )
        .unwrap();
        runtime.load_file(&path).unwrap();
        assert!(!runtime.execute("test", "").unwrap());
        assert!(!runtime.has_key("normal", "space z").unwrap());
        runtime
            .emit("saved", runtime.lua().create_table().unwrap())
            .unwrap();
        assert!(runtime.take_actions().unwrap().is_empty());
        assert!(runtime.execute("new_test", "").unwrap());
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "new"
        );
    }

    #[test]
    fn reloading_defaults_preserves_user_overrides_and_removed_override_restores_defaults() {
        let dir = TestDir::new();
        let path = dir.0.join("init.lua");
        let runtime = dir.runtime();
        fs::write(
            &path,
            "remacs.keymap.set('normal', 'j', function() remacs.message('custom') end)",
        )
        .unwrap();
        runtime.load_config().unwrap();
        runtime.reload("defaults").unwrap();
        runtime.dispatch_key("normal", "j").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "custom"
        );
        fs::write(&path, "-- removed override").unwrap();
        runtime.load_config().unwrap();
        runtime.dispatch_key("normal", "j").unwrap();
        assert_eq!(action_name(&runtime), "down");
    }

    #[test]
    fn explicit_user_removals_survive_default_reload_until_configuration_removes_them() {
        let dir = TestDir::new();
        let path = dir.0.join("init.lua");
        let runtime = dir.runtime();
        fs::write(
            &path,
            "remacs.keymap.del('normal', 'j'); remacs.commands.remove('save')",
        )
        .unwrap();
        runtime.load_config().unwrap();
        runtime.reload("defaults").unwrap();
        assert!(!runtime.has_key("normal", "j").unwrap());
        assert!(!runtime.execute("save", "").unwrap());
        fs::write(&path, "-- removed deletions").unwrap();
        runtime.load_config().unwrap();
        assert!(runtime.has_key("normal", "j").unwrap());
        assert!(runtime.execute("save", "").unwrap());
        assert_eq!(action_name(&runtime), "save");
    }

    #[test]
    fn explicit_registration_after_nested_require_wins_over_the_required_plugin() {
        let dir = TestDir::new();
        fs::create_dir_all(dir.0.join("lua")).unwrap();
        fs::write(
            dir.0.join("lua/custom.lua"),
            "remacs.keymap.set('normal', 'u', function() remacs.message('plugin') end); return {}",
        )
        .unwrap();
        fs::write(dir.0.join("init.lua"), "remacs.commands.add('init-command', function() end); require('custom'); remacs.keymap.set('normal', 'u', function() remacs.message('init') end)").unwrap();
        let runtime = dir.runtime();
        runtime.load_config().unwrap();
        runtime.dispatch_key("normal", "u").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "init"
        );
        runtime.reload("custom").unwrap();
        runtime.dispatch_key("normal", "u").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "init"
        );
        runtime
            .eval("remacs.keymap.set('normal', 'u', function() remacs.message('interactive') end)")
            .unwrap();
        runtime.dispatch_key("normal", "u").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "interactive"
        );
    }

    #[test]
    fn ordinary_lua_modules_reload_with_their_own_registration_owner() {
        let dir = TestDir::new();
        fs::create_dir_all(dir.0.join("lua")).unwrap();
        let path = dir.0.join("lua/custom.lua");
        fs::write(&path, "remacs.commands.add('old_custom', function() remacs.commands.add('dynamic_custom', function() end) end); return {} ").unwrap();
        let runtime = dir.runtime();
        runtime.eval("remacs.plugins.load('custom')").unwrap();
        runtime.execute("old_custom", "").unwrap();
        assert!(runtime.execute("dynamic_custom", "").unwrap());
        fs::write(
            &path,
            "remacs.commands.add('new_custom', function() end); return {}",
        )
        .unwrap();
        runtime.reload("custom").unwrap();
        assert!(!runtime.execute("old_custom", "").unwrap());
        assert!(!runtime.execute("dynamic_custom", "").unwrap());
        assert!(runtime.execute("new_custom", "").unwrap());
    }

    #[test]
    fn direct_module_callbacks_discard_actions_on_failure_and_keep_their_owner() {
        let dir = TestDir::new();
        fs::create_dir_all(dir.0.join("lua")).unwrap();
        let path = dir.0.join("lua/custom.lua");
        fs::write(&path, "return { fail = function() remacs.message('discard'); error('failed module callback') end, register = function() remacs.commands.add('module_dynamic', function() end) end }").unwrap();
        let runtime = dir.runtime();
        runtime.eval("require('custom')").unwrap();
        let module: Table = runtime
            .lua()
            .globals()
            .get::<Function>("require")
            .unwrap()
            .call("custom")
            .unwrap();
        assert!(module
            .get::<Function>("fail")
            .unwrap()
            .call::<()>(())
            .is_err());
        assert!(runtime.take_actions().unwrap().is_empty());
        module
            .get::<Function>("register")
            .unwrap()
            .call::<()>(())
            .unwrap();
        assert!(runtime.execute("module_dynamic", "").unwrap());
        fs::write(&path, "return {}").unwrap();
        runtime.reload("custom").unwrap();
        assert!(!runtime.execute("module_dynamic", "").unwrap());
    }

    #[test]
    fn loading_cached_module_does_not_remove_its_registrations() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        runtime.eval("remacs.plugins.load('tree')").unwrap();
        assert!(runtime.execute("tree", "").unwrap());
        assert_eq!(action_name(&runtime), "tree_toggle");
    }

    #[test]
    fn invalid_startup_plugin_falls_back_and_failed_reload_retains_working_module() {
        let dir = TestDir::new();
        fs::create_dir_all(dir.0.join("plugins")).unwrap();
        let path = dir.0.join("plugins/mason.lua");
        fs::write(&path, "return {} -- invalid catalog").unwrap();
        let runtime = dir.runtime();
        assert_eq!(runtime.startup_errors().unwrap().len(), 1);
        let mason = runtime.table("mason").unwrap();
        assert!(mason.get::<Function>("list").is_ok());
        let generation = runtime.generation();
        assert!(runtime.reload("mason").is_err());
        assert_eq!(runtime.generation(), generation);
        assert!(runtime
            .table("mason")
            .unwrap()
            .get::<Function>("list")
            .is_ok());
    }

    #[test]
    fn buffer_reads_are_lazy_and_mutations_are_deferred() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        let reads = Rc::new(Cell::new(0));
        let counter = reads.clone();
        runtime
            .api()
            .unwrap()
            .set(
                "_get_lines",
                runtime
                    .lua()
                    .create_function(move |lua, id: usize| {
                        assert_eq!(id, 11);
                        counter.set(counter.get() + 1);
                        lua.create_sequence_from(["first", "café", "last"])
                    })
                    .unwrap(),
            )
            .unwrap();
        let context = runtime.lua().create_table().unwrap();
        context.set("current", 11).unwrap();
        context
            .set("buffers", runtime.lua().create_table().unwrap())
            .unwrap();
        runtime.set_context(context).unwrap();
        runtime
            .eval("assert(remacs.buffer.current() == 11)")
            .unwrap();
        assert_eq!(reads.get(), 0);
        runtime.eval("local lines = remacs.buffer.get_lines(nil, 2, 2); assert(#lines == 1 and lines[1] == 'café'); remacs.buffer.set_lines(nil, {'changed'}); remacs.buffer.set_cursor(nil, 1, 2)").unwrap();
        assert_eq!(reads.get(), 1);
        let actions = runtime.take_actions().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].get::<usize>("id").unwrap(), 11);
        assert_eq!(actions[0].get::<String>("kind").unwrap(), "set_lines");
        assert_eq!(actions[1].get::<String>("kind").unwrap(), "cursor");
    }

    #[test]
    fn runaway_lua_is_interrupted_and_runtime_remains_usable() {
        let dir = TestDir::new();
        let runtime = dir.runtime();
        let error = runtime.eval("while true do end").unwrap_err().to_string();
        assert!(error.contains("execution budget"), "{error}");
        runtime.eval("remacs.message('recovered')").unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "recovered"
        );
    }
}
