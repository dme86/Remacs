//! A small Lua LSP configuration host: servers and keymaps belong to
//! plugins/lsp.lua; Rust owns process management and JSON-RPC.
use std::path::{Path, PathBuf};

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
}

impl LspPlugin {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let plugin: Table = lua.load(include_str!("../plugins/lsp.lua"))
            .set_name("plugins/lsp.lua").eval()?;
        let plugin = lua.create_registry_value(plugin)?;
        Ok(Self { lua, plugin })
    }

    fn table(&self) -> mlua::Result<Table> {
        self.lua.registry_value(&self.plugin)
    }

    pub fn binding(&self, keys: &str) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("binding")?.call(keys)
    }

    pub fn server_for(&self, path: &Path, cwd: &Path) -> mlua::Result<Option<ServerConfig>> {
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { return Ok(None) };
        let Some(server): Option<Table> = self.table()?
            .get::<Function>("server_for")?.call(ext)? else { return Ok(None) };
        let command: Vec<String> = server.get::<Table>("cmd")?
            .sequence_values::<String>().collect::<mlua::Result<_>>()?;
        if command.is_empty() || command[0].is_empty() {
            return Err(mlua::Error::external("LSP command cannot be empty"));
        }
        let roots = server.get::<Table>("roots")?
            .sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?;
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
    let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    let start = path.parent().and_then(|p| p.canonicalize().ok())
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
        let spec = config.server_for(Path::new("src/main.rs"), &root).unwrap().unwrap();
        assert_eq!(spec.language, "rust");
        assert_eq!(spec.command, ["rust-analyzer"]);
        assert!(spec.root.is_dir());
        assert!(config.server_for(Path::new("README.md"), &root).unwrap().is_none());
    }

    #[test]
    fn root_of_relative_rust_file_is_absolute_and_git_worktrees_work() {
        let root = std::env::temp_dir().join(format!(
            "remacs-lsp-root-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()
        ));
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /tmp/fixture\n").unwrap();
        let spec = LspPlugin::new().unwrap()
            .server_for(Path::new("src/nested/lib.rs"), &root)
            .unwrap().unwrap();
        assert_eq!(spec.root, root.canonicalize().unwrap());
        assert!(spec.root.is_absolute());
        std::fs::remove_dir_all(root).unwrap();
    }
}
