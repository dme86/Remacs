//! Lua-driven popup terminal controller. This is deliberately separate from
//! the Rust PTY renderer: plugins/toggleterm.lua owns bindings, session
//! visibility and commands; the host only executes actions returned by Lua.
use std::path::Path;
use mlua::{Function, Lua, RegistryKey, Table};

pub struct ToggleTermPlugin {
    lua: Lua,
    plugin: RegistryKey,
}

pub enum Action {
    Open { kind: String, command: String, cwd: String, title: String },
    Show { kind: String },
    Hide,
    Error(String),
}

impl ToggleTermPlugin {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let plugin: Table = lua.load(include_str!("../plugins/toggleterm.lua"))
            .set_name("plugins/toggleterm.lua").eval()?;
        let plugin = lua.create_registry_value(plugin)?;
        Ok(Self { lua, plugin })
    }

    fn table(&self) -> mlua::Result<Table> {
        self.lua.registry_value(&self.plugin)
    }

    pub fn active(&self) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("active")?.call(())
    }

    pub fn binding(&self, key: &str) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("binding")?.call(key)
    }

    pub fn toggle(&self, kind: &str, cwd: &Path) -> mlua::Result<Action> {
        let result: Table = self.table()?.get::<Function>("toggle")?
            .call((kind, cwd.to_string_lossy().into_owned()))?;
        match result.get::<String>("action")?.as_str() {
            "open" => Ok(Action::Open {
                kind: result.get("kind")?,
                command: result.get("command")?,
                cwd: result.get("cwd")?,
                title: result.get("title")?,
            }),
            "show" => Ok(Action::Show { kind: result.get("kind")? }),
            "hide" => Ok(Action::Hide),
            _ => Ok(Action::Error(result.get::<String>("message")
                .unwrap_or_else(|_| "Invalid toggleterm action".into()))),
        }
    }

    pub fn hide(&self) -> mlua::Result<()> {
        self.table()?.get::<Function>("hide")?.call::<Option<String>>(())?;
        Ok(())
    }

    pub fn exited(&self, kind: &str) -> mlua::Result<()> {
        self.table()?.get::<Function>("exited")?.call(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_plugin_owns_keybindings_and_sessions() {
        let plugin = ToggleTermPlugin::new().unwrap();
        assert_eq!(plugin.binding("space g").unwrap().as_deref(), Some("lazygit"));
        assert_eq!(plugin.binding("ctrl-t").unwrap().as_deref(), Some("shell"));
        assert_eq!(plugin.binding("space t").unwrap().as_deref(), Some("shell"));
        assert_eq!(plugin.binding("not-a-binding").unwrap(), None);
        let cwd = std::env::temp_dir();
        match plugin.toggle("lazygit", &cwd).unwrap() {
            Action::Open { command, kind, .. } => {
                assert_eq!(command, "lazygit");
                assert_eq!(kind, "lazygit");
            }
            _ => panic!("first toggle must create PTY"),
        }
        assert_eq!(plugin.active().unwrap().as_deref(), Some("lazygit"));
        assert!(matches!(plugin.toggle("lazygit", &cwd).unwrap(), Action::Hide));
        assert_eq!(plugin.active().unwrap(), None);
        assert!(matches!(plugin.toggle("lazygit", &cwd).unwrap(), Action::Show { .. }));
        assert!(matches!(plugin.toggle("shell", &cwd).unwrap(), Action::Open { .. }));
        assert_eq!(plugin.active().unwrap().as_deref(), Some("shell"));
        plugin.exited("shell").unwrap();
        assert_eq!(plugin.active().unwrap(), None);
        plugin.exited("lazygit").unwrap();
        assert!(matches!(plugin.toggle("lazygit", &cwd).unwrap(), Action::Open { .. }));
    }
}
