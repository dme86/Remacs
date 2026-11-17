//! A small Lua LSP configuration host: servers and keymaps belong to
//! plugins/lsp.lua; Rust owns process management and JSON-RPC.
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

use mlua::{Function, Lua, RegistryKey, Table};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub language: String,
    pub command: Vec<String>,
    pub root: PathBuf,
}

pub struct LspPlugin {
    lua: Lua,
    plugin: RegistryKey,
    configurations: RefCell<HashMap<(PathBuf, PathBuf), Option<ServerConfig>>>,
}

impl LspPlugin {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        // Load Mason's declarative Lua catalog once per LSP Lua VM. Both
        // plugins use identical embedded source; no duplicated server lists.
        let package: Table = lua.globals().get("package")?;
        let preload: Table = package.get("preload")?;
        let loader = lua.create_function(|lua, _: ()| {
            lua.load(include_str!("../plugins/mason.lua"))
                .set_name("plugins/mason.lua")
                .eval::<Table>()
        })?;
        preload.set("remacs.mason", loader)?;
        let plugin: Table = lua
            .load(include_str!("../plugins/lsp.lua"))
            .set_name("plugins/lsp.lua")
            .eval()?;
        let plugin = lua.create_registry_value(plugin)?;
        Ok(Self {
            lua,
            plugin,
            configurations: RefCell::new(HashMap::new()),
        })
    }

    fn table(&self) -> mlua::Result<Table> {
        self.lua.registry_value(&self.plugin)
    }

    pub fn binding(&self, keys: &str) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("binding")?.call(keys)
    }

    /// Re-resolve executables after Mason changes the installed tools.
    pub fn clear_cache(&self) {
        self.configurations.borrow_mut().clear();
    }

    pub fn server_for(&self, path: &Path, cwd: &Path) -> mlua::Result<Option<ServerConfig>> {
        let key = (path.to_path_buf(), cwd.to_path_buf());
        if let Some(configuration) = self.configurations.borrow().get(&key) {
            return Ok(configuration.clone());
        }
        let configuration = self.resolve_server(path, cwd)?;
        let mut cache = self.configurations.borrow_mut();
        // Bound retained paths when many files are opened over a long session.
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(key, configuration.clone());
        Ok(configuration)
    }

    fn resolve_server(&self, path: &Path, cwd: &Path) -> mlua::Result<Option<ServerConfig>> {
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            return Ok(None);
        };
        let Some(server): Option<Table> = self.table()?.get::<Function>("server_for")?.call(ext)?
        else {
            return Ok(None);
        };
        let mut command: Vec<String> = server
            .get::<Table>("cmd")?
            .sequence_values::<String>()
            .collect::<mlua::Result<_>>()?;
        if command.is_empty() || command[0].is_empty() {
            return Err(mlua::Error::external("LSP command cannot be empty"));
        }
        // Prefer a private, Remacs-managed LSP binary when installed;
        // otherwise use the user's own PATH (rustup, brew, system packages).
        command[0] = crate::mason::resolve_command(&command[0]);
        let roots = server
            .get::<Table>("roots")?
            .sequence_values::<String>()
            .collect::<mlua::Result<Vec<_>>>()?;
        Ok(Some(ServerConfig {
            language: server.get("language")?,
            command,
            root: resolve_root(path, cwd, &roots),
        }))
    }
}

/// Resolve paths before walking parent directories. Relative path ancestors
/// contain an empty Path and must never be returned as a project directory.
pub fn resolve_root(path: &Path, cwd: &Path, markers: &[String]) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let start = path
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| cwd.clone());
    for directory in start.ancestors() {
        if markers.iter().any(|marker| directory.join(marker).exists()) {
            return directory.to_path_buf();
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_config_maps_rust_analyzer_and_navigation_keys() {
        let config = LspPlugin::new().unwrap();
        assert_eq!(config.binding("gd").unwrap().as_deref(), Some("definition"));
        assert_eq!(config.binding("K").unwrap().as_deref(), Some("hover"));
        assert_eq!(config.binding("gg").unwrap(), None);
        let root = std::env::current_dir().unwrap();
        let spec = config
            .server_for(Path::new("src/main.rs"), &root)
            .unwrap()
            .unwrap();
        assert_eq!(spec.language, "rust");
        assert_eq!(spec.command, ["rust-analyzer"]);
        assert!(spec.root.is_dir());
        assert!(config
            .server_for(Path::new("README.md"), &root)
            .unwrap()
            .is_none());
    }

    #[test]
    fn lsp_reads_same_enabled_servers_as_mason_lua_plugin() {
        let plugin = LspPlugin::new().unwrap();
        let mason = crate::mason_plugin::MasonPlugin::new().unwrap();
        let tools = mason.tools().unwrap();
        let cwd = std::env::current_dir().unwrap();
        for (extension, language) in [
            ("go", "go"),
            ("lua", "lua"),
            ("yaml", "yaml"),
            ("json", "json"),
            ("sh", "bash"),
            ("rs", "rust"),
        ] {
            assert!(tools.iter().any(|tool| tool.language == language));
            let file = format!("main.{extension}");
            let spec = plugin.server_for(Path::new(&file), &cwd).unwrap().unwrap();
            assert_eq!(spec.language, language);
        }
        // Catalog-only, unselected languages must not start or install.
        assert!(plugin
            .server_for(Path::new("main.ts"), &cwd)
            .unwrap()
            .is_none());
    }

    #[test]
    fn server_configuration_is_cached_and_can_be_invalidated() {
        let plugin = LspPlugin::new().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let rust = Path::new("src/main.rs");
        let markdown = Path::new("README.md");
        plugin.server_for(rust, &cwd).unwrap();
        plugin.server_for(markdown, &cwd).unwrap();
        assert_eq!(plugin.configurations.borrow().len(), 2);

        // Replacing the Lua resolver makes a second evaluation observable.
        plugin
            .table()
            .unwrap()
            .set(
                "server_for",
                plugin
                    .lua
                    .create_function(|_, _: String| {
                        Err::<Option<Table>, _>(mlua::Error::external("cache miss"))
                    })
                    .unwrap(),
            )
            .unwrap();
        assert!(plugin.server_for(rust, &cwd).unwrap().is_some());
        assert!(plugin.server_for(markdown, &cwd).unwrap().is_none());
        plugin.clear_cache();
        assert!(plugin.server_for(rust, &cwd).is_err());
    }

    #[test]
    fn root_of_relative_rust_file_is_absolute_and_git_worktrees_work() {
        let root = std::env::temp_dir().join(format!(
            "remacs-lsp-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /tmp/fixture\n").unwrap();
        let spec = LspPlugin::new()
            .unwrap()
            .server_for(Path::new("src/nested/lib.rs"), &root)
            .unwrap()
            .unwrap();
        assert_eq!(spec.root, root.canonicalize().unwrap());
        assert!(spec.root.is_absolute());
        std::fs::remove_dir_all(root).unwrap();
    }
}
