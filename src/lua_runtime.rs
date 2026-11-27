//! One persistent Lua VM for the entire editor. Clones are handles to the same
//! VM, not new interpreters. All editor writes cross a deferred action queue.
use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Component, Path, PathBuf},
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
    managed_plugin_paths: Rc<RefCell<Vec<String>>>,
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
            managed_plugin_paths: Rc::new(RefCell::new(Vec::new())),
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
        let managed_plugin_paths = self.managed_plugin_paths.clone();
        let load = self
            .lua
            .create_function(move |_, (name, reload): (String, bool)| {
                let runtime = Self {
                    lua: weak.upgrade(),
                    generation: generation.clone(),
                    started: started.clone(),
                    instructions: instructions.clone(),
                    config_dir: config_dir.clone(),
                    managed_plugin_paths: managed_plugin_paths.clone(),
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
        let managed_plugin_paths = self.managed_plugin_paths.clone();
        let config = self.lua.create_function(move |_, ()| {
            let runtime = Self {
                lua: weak.upgrade(),
                generation: generation.clone(),
                started: started.clone(),
                instructions: instructions.clone(),
                config_dir: config_dir.clone(),
                managed_plugin_paths: managed_plugin_paths.clone(),
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

    /// Register installed snapshots in the chosen order before loading entries.
    /// User modules retain precedence, followed by snapshots in registration order.
    pub fn register_managed_plugin_path(&self, root: &Path) -> mlua::Result<()> {
        let root = managed_plugin_root(root)?;
        let root_text = root.to_str().ok_or_else(|| {
            mlua::Error::RuntimeError("Managed plugin snapshot directory must be UTF-8".into())
        })?;
        let patterns = [
            format!("{root_text}/lua/?.lua"),
            format!("{root_text}/lua/?/init.lua"),
            format!("{root_text}/?.lua"),
            format!("{root_text}/?/init.lua"),
        ];
        let package: Table = self.lua.globals().get("package")?;
        let previous: String = package.get("path")?;
        let mut paths: Vec<_> = previous.split(';').map(str::to_owned).collect();
        let config_lua = self.config_dir.join("lua");
        let config_paths = [
            format!("{}/?.lua", config_lua.display()),
            format!("{}/?/init.lua", config_lua.display()),
        ];
        let mut managed = self.managed_plugin_paths.borrow_mut();
        let mut insertion = paths
            .iter()
            .rposition(|path| config_paths.contains(path) || managed.contains(path))
            .map_or(0, |index| index + 1);
        for pattern in &patterns {
            if !paths.contains(pattern) {
                paths.insert(insertion, pattern.clone());
                insertion += 1;
            }
        }
        package.set("path", paths.join(";"))?;
        for pattern in patterns {
            if !managed.contains(&pattern) {
                managed.push(pattern);
            }
        }
        Ok(())
    }

    /// Load an installed snapshot in this VM, before the user's configuration.
    /// Successful snapshot paths remain available to deferred `require` calls.
    pub fn load_managed_plugin(&self, name: &str, root: &Path, entry: &str) -> mlua::Result<()> {
        let error = |reason: String| {
            mlua::Error::RuntimeError(format!("Managed plugin '{name}': {reason}"))
        };
        let relative = Path::new(entry);
        if entry.is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(error(
                "entry must be a relative file path without '..'".into(),
            ));
        }
        let root = managed_plugin_root(root).map_err(|cause| error(cause.to_string()))?;
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(|cause| error(format!("cannot resolve entry '{entry}': {cause}")))?;
        if !path.starts_with(&root) {
            return Err(error(format!(
                "entry '{entry}' escapes the snapshot directory"
            )));
        }
        if !path.is_file() {
            return Err(error(format!("entry '{entry}' must be a file")));
        }
        if path.to_str().is_none() {
            return Err(error("entry must resolve to a UTF-8 path".into()));
        }

        let package: Table = self.lua.globals().get("package")?;
        let previous: String = package.get("path")?;
        let previous_managed = self.managed_plugin_paths.borrow().clone();
        self.register_managed_plugin_path(&root)
            .map_err(|cause| error(cause.to_string()))?;
        if let Err(cause) = self.load_file(&path) {
            package.set("path", previous)?;
            *self.managed_plugin_paths.borrow_mut() = previous_managed;
            return Err(error(format!("cannot load entry '{entry}': {cause}")));
        }
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

fn managed_plugin_root(root: &Path) -> mlua::Result<PathBuf> {
    let error = |reason: String| {
        mlua::Error::RuntimeError(format!("Managed plugin snapshot directory: {reason}"))
    };
    let root = root
        .canonicalize()
        .map_err(|cause| error(format!("cannot resolve path: {cause}")))?;
    if !root.is_dir() {
        return Err(error("must be a directory".into()));
    }
    let text = root
        .to_str()
        .ok_or_else(|| error("must be a UTF-8 path".into()))?;
    if text.contains([';', '?']) {
        return Err(error(
            "cannot contain ';' or '?' in Lua module search paths".into(),
        ));
    }
    Ok(root)
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

    fn plugin_file(root: &Path, path: &str, source: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn package_path(runtime: &LuaRuntime) -> String {
        runtime
            .lua()
            .globals()
            .get::<Table>("package")
            .unwrap()
            .get("path")
            .unwrap()
    }

    #[test]
    fn managed_snapshot_entry_and_deferred_helpers_share_the_runtime() {
        let dir = TestDir::new();
        let root = dir.0.join("snapshots/demo/commit");
        for (path, value) in [
            ("lua/managed_lua_file.lua", "lua file"),
            ("lua/managed_lua_package/init.lua", "lua package"),
            ("managed_root_file.lua", "root file"),
            ("managed_root_package/init.lua", "root package"),
            ("lua/managed_deferred.lua", "deferred helper"),
        ] {
            plugin_file(&root, path, &format!("return '{value}'"));
        }
        plugin_file(
            &root,
            "plugin.lua",
            r#"
                assert(remacs.api_version == 1)
                assert(remacs.keymap.list()['normal:j'] == 'remacs.defaults')
                local values = {
                    require('managed_lua_file'), require('managed_lua_package'),
                    require('managed_root_file'), (require('managed_root_package'))
                }
                remacs.state.managed_values = table.concat(values, ', ')
                remacs.commands.add('managed-demo', function()
                    remacs.message(remacs.state.managed_values)
                end)
                remacs.keymap.set('normal', 'f22', 'managed-demo')
                remacs.hooks.add('managed-event', 'deferred', function()
                    remacs.message(require('managed_deferred'))
                end)
            "#,
        );
        let runtime = dir.runtime();
        let generation = runtime.generation();
        runtime
            .load_managed_plugin("demo", &root, "plugin.lua")
            .unwrap();
        assert_eq!(runtime.generation(), generation + 1);
        runtime
            .clone()
            .eval("assert(remacs.state.managed_values == 'lua file, lua package, root file, root package')")
            .unwrap();
        assert!(runtime.dispatch_key("normal", "f22").unwrap());
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "lua file, lua package, root file, root package"
        );
        let mappings: Table = runtime
            .api()
            .unwrap()
            .get::<Table>("keymap")
            .unwrap()
            .get::<Function>("list")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(
            mappings.get::<String>("normal:f22").unwrap(),
            format!(
                "file:{}",
                root.join("plugin.lua").canonicalize().unwrap().display()
            )
        );
        runtime
            .emit("managed-event", runtime.lua().create_table().unwrap())
            .unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "deferred helper"
        );
    }

    #[test]
    fn managed_search_paths_are_idempotent_ordered_and_follow_user_modules() {
        let dir = TestDir::new();
        let alpha = dir.0.join("snapshots/alpha/commit");
        let beta = dir.0.join("snapshots/beta/commit");
        plugin_file(&dir.0, "lua/managed_priority.lua", "return 'user'");
        for (root, value) in [(&alpha, "alpha"), (&beta, "beta")] {
            plugin_file(
                root,
                "lua/managed_priority.lua",
                &format!("return '{value}'"),
            );
            plugin_file(root, "lua/managed_order.lua", &format!("return '{value}'"));
        }
        plugin_file(&beta, "lua/managed_beta_helper.lua", "return 'beta helper'");
        plugin_file(
            &alpha,
            "plugin.lua",
            "assert(require('managed_priority') == 'user'); assert(require('managed_order') == 'alpha'); assert(require('managed_beta_helper') == 'beta helper')",
        );
        let runtime = dir.runtime();
        runtime.register_managed_plugin_path(&alpha).unwrap();
        runtime.register_managed_plugin_path(&beta).unwrap();
        let registered = package_path(&runtime);
        runtime
            .clone()
            .register_managed_plugin_path(&alpha)
            .unwrap();
        runtime
            .load_managed_plugin("alpha", &alpha, "plugin.lua")
            .unwrap();
        assert_eq!(package_path(&runtime), registered);
        let config = format!("{}/lua/?.lua", dir.0.display());
        let alpha_path = format!("{}/lua/?.lua", alpha.canonicalize().unwrap().display());
        let beta_path = format!("{}/lua/?.lua", beta.canonicalize().unwrap().display());
        assert!(registered.find(&config).unwrap() < registered.find(&alpha_path).unwrap());
        assert!(registered.find(&alpha_path).unwrap() < registered.find(&beta_path).unwrap());
        for root in [&alpha, &beta] {
            let root = root.canonicalize().unwrap();
            for suffix in ["/lua/?.lua", "/lua/?/init.lua", "/?.lua", "/?/init.lua"] {
                let pattern = format!("{}{suffix}", root.display());
                assert_eq!(
                    registered
                        .split(';')
                        .filter(|path| *path == pattern)
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn failed_managed_entry_restores_paths_registrations_helpers_and_actions() {
        let dir = TestDir::new();
        let root = dir.0.join("snapshots/broken/commit");
        plugin_file(
            &root,
            "lua/managed_failed_helper.lua",
            "remacs.commands.add('managed-helper-leak', function() end); remacs.keymap.set('normal', 'f23', 'managed-helper-leak'); remacs.hooks.add('managed-failure', 'helper', function() remacs.message('leaked hook') end); return {}",
        );
        plugin_file(
            &root,
            "plugin.lua",
            "remacs.commands.add('managed-shared', function() remacs.message('broken') end); remacs.commands.add('managed-entry-leak', function() end); remacs.keymap.set('normal', 'f22', 'managed-entry-leak'); require('managed_failed_helper'); remacs.message('discard'); error('entry failed')",
        );
        let runtime = dir.runtime();
        runtime.eval("remacs.commands.add('managed-shared', function() remacs.message('original') end); remacs.keymap.set('normal', 'f22', 'managed-shared')").unwrap();
        let previous = package_path(&runtime);
        let generation = runtime.generation();
        let error = runtime
            .load_managed_plugin("broken", &root, "plugin.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("broken") && error.contains("entry failed"),
            "{error}"
        );
        assert_eq!(package_path(&runtime), previous);
        assert_eq!(runtime.generation(), generation);
        assert!(runtime.take_actions().unwrap().is_empty());
        assert!(!runtime.execute("managed-entry-leak", "").unwrap());
        assert!(!runtime.execute("managed-helper-leak", "").unwrap());
        assert!(!runtime.has_key("normal", "f23").unwrap());
        runtime
            .emit("managed-failure", runtime.lua().create_table().unwrap())
            .unwrap();
        assert!(runtime.take_actions().unwrap().is_empty());
        runtime
            .eval("assert(package.loaded.managed_failed_helper == nil)")
            .unwrap();
        assert!(runtime.dispatch_key("normal", "f22").unwrap());
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "original"
        );
    }

    #[test]
    fn user_configuration_overrides_managed_registrations_and_file_reload_preserves_it() {
        let dir = TestDir::new();
        let root = dir.0.join("snapshots/demo/commit");
        plugin_file(
            &root,
            "plugin.lua",
            "remacs.commands.add('managed-choice', function() remacs.message('plugin') end); remacs.keymap.set('normal', 'j', 'managed-choice'); remacs.hooks.add('managed-choice', 'shared', function() remacs.message('plugin') end)",
        );
        plugin_file(
            &dir.0,
            "init.lua",
            "remacs.commands.add('managed-choice', function() remacs.message('user') end); remacs.keymap.set('normal', 'j', 'managed-choice'); remacs.hooks.add('managed-choice', 'shared', function() remacs.message('user') end)",
        );
        let runtime = dir.runtime();
        runtime
            .load_managed_plugin("demo", &root, "plugin.lua")
            .unwrap();
        assert!(runtime.load_config().unwrap());
        runtime.load_file(&root.join("plugin.lua")).unwrap();
        for action in ["command", "key", "hook"] {
            match action {
                "command" => assert!(runtime.execute("managed-choice", "").unwrap()),
                "key" => assert!(runtime.dispatch_key("normal", "j").unwrap()),
                _ => runtime
                    .emit("managed-choice", runtime.lua().create_table().unwrap())
                    .unwrap(),
            }
            assert_eq!(
                runtime.take_actions().unwrap()[0]
                    .get::<String>("text")
                    .unwrap(),
                "user"
            );
        }
        plugin_file(&dir.0, "init.lua", "-- removed user override");
        runtime.load_config().unwrap();
        assert!(runtime.dispatch_key("normal", "j").unwrap());
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "plugin"
        );
    }

    #[test]
    fn managed_entries_require_relative_files_without_parent_components() {
        let dir = TestDir::new();
        let root = dir.0.join("snapshot");
        plugin_file(&root, "plugin.lua", "remacs.message('loaded')");
        fs::create_dir(root.join("nested")).unwrap();
        let runtime = dir.runtime();
        let previous = package_path(&runtime);
        for entry in [
            "",
            "../plugin.lua",
            "nested/../plugin.lua",
            "nested/../../plugin.lua",
            "nested",
            "missing.lua",
        ] {
            let error = runtime
                .load_managed_plugin("invalid-entry", &root, entry)
                .unwrap_err()
                .to_string();
            assert!(error.contains("invalid-entry"), "{error}");
            assert_eq!(package_path(&runtime), previous);
        }
        assert!(runtime
            .load_managed_plugin(
                "absolute-entry",
                &root,
                root.join("plugin.lua").to_str().unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("relative"));
        for name in ["snapshot;ambiguous", "snapshot?ambiguous"] {
            let ambiguous = dir.0.join(name);
            plugin_file(&ambiguous, "plugin.lua", "remacs.message('loaded')");
            assert!(runtime.register_managed_plugin_path(&ambiguous).is_err());
        }
        assert_eq!(package_path(&runtime), previous);
        assert!(runtime.take_actions().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn managed_entry_symlinks_cannot_escape_the_snapshot() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new();
        let root = dir.0.join("snapshot");
        plugin_file(&root, "plugin.lua", "remacs.message('inside')");
        plugin_file(&dir.0, "outside/plugin.lua", "remacs.message('outside')");
        symlink(dir.0.join("outside/plugin.lua"), root.join("escape.lua")).unwrap();
        symlink(dir.0.join("outside"), root.join("escape-dir")).unwrap();
        symlink(root.join("plugin.lua"), root.join("inside.lua")).unwrap();
        let runtime = dir.runtime();
        let previous = package_path(&runtime);
        for entry in ["escape.lua", "escape-dir/plugin.lua"] {
            let error = runtime
                .load_managed_plugin("symlink", &root, entry)
                .unwrap_err()
                .to_string();
            assert!(error.contains("escapes"), "{error}");
            assert_eq!(package_path(&runtime), previous);
        }
        assert!(runtime.take_actions().unwrap().is_empty());
        runtime
            .load_managed_plugin("symlink", &root, "inside.lua")
            .unwrap();
        assert_eq!(
            runtime.take_actions().unwrap()[0]
                .get::<String>("text")
                .unwrap(),
            "inside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_snapshot_paths_must_be_utf8() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let dir = TestDir::new();
        let root = dir.0.join(OsString::from_vec(b"snapshot-\xff".to_vec()));
        // Some filesystems, including common macOS volumes, reject filenames
        // that are not UTF-8. Only exercise rejection when the fixture exists.
        if let Err(error) = fs::create_dir(&root) {
            eprintln!("Skipping non-UTF-8 path fixture unsupported by this filesystem: {error}");
            return;
        }
        fs::write(root.join("plugin.lua"), "remacs.message('loaded')").unwrap();
        let runtime = dir.runtime();
        let previous = package_path(&runtime);
        let error = runtime
            .load_managed_plugin("non-utf8", &root, "plugin.lua")
            .unwrap_err()
            .to_string();
        assert!(error.contains("UTF-8"), "{error}");
        assert_eq!(package_path(&runtime), previous);
        assert!(runtime.take_actions().unwrap().is_empty());
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
