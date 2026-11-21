//! Lua-driven popup terminal controller. This is deliberately separate from
//! the Rust PTY renderer: plugins/toggleterm.lua owns bindings, session
//! visibility and commands; the host only executes actions returned by Lua.
use mlua::{Function, Table};

use crate::lua_runtime::LuaRuntime;
use std::path::Path;

pub struct ToggleTermPlugin {
    runtime: LuaRuntime,
}

pub enum Action {
    Open {
        kind: String,
        command: String,
        cwd: String,
        title: String,
    },
    Show {
        kind: String,
    },
    Hide,
    Error(String),
}

impl ToggleTermPlugin {
    pub fn new(runtime: LuaRuntime) -> mlua::Result<Self> {
        runtime.table("toggleterm")?;
        Ok(Self { runtime })
    }

    fn table(&self) -> mlua::Result<Table> {
        self.runtime.table("toggleterm")
    }

    pub fn active(&self) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("active")?.call(())
    }

    #[cfg(test)]
    pub fn binding(&self, key: &str) -> mlua::Result<Option<String>> {
        self.table()?.get::<Function>("binding")?.call(key)
    }

    pub fn dismiss(&self, key: &str) -> mlua::Result<bool> {
        self.table()?.get::<Function>("dismiss")?.call(key)
    }

    pub fn toggle(&self, kind: &str, cwd: &Path) -> mlua::Result<Action> {
        let result: Table = self
            .table()?
            .get::<Function>("toggle")?
            .call((kind, cwd.to_string_lossy().into_owned()))?;
        match result.get::<String>("action")?.as_str() {
            "open" => Ok(Action::Open {
                kind: result.get("kind")?,
                command: result.get("command")?,
                cwd: result.get("cwd")?,
                title: result.get("title")?,
            }),
            "show" => Ok(Action::Show {
                kind: result.get("kind")?,
            }),
            "hide" => Ok(Action::Hide),
            _ => Ok(Action::Error(
                result
                    .get::<String>("message")
                    .unwrap_or_else(|_| "Invalid toggleterm action".into()),
            )),
        }
    }

    pub fn hide(&self) -> mlua::Result<()> {
        self.table()?
            .get::<Function>("hide")?
            .call::<Option<String>>(())?;
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
    fn hot_reload_keeps_active_and_hidden_terminal_sessions() {
        let root = std::env::current_dir().unwrap();
        let runtime = LuaRuntime::new(&root).unwrap();
        let plugin = ToggleTermPlugin::new(runtime.clone()).unwrap();
        assert!(matches!(
            plugin.toggle("shell", &root).unwrap(),
            Action::Open { .. }
        ));
        runtime.reload("toggleterm").unwrap();
        assert_eq!(plugin.active().unwrap().as_deref(), Some("shell"));
        assert!(matches!(
            plugin.toggle("shell", &root).unwrap(),
            Action::Hide
        ));
        runtime.reload("toggleterm").unwrap();
        assert!(matches!(
            plugin.toggle("shell", &root).unwrap(),
            Action::Show { .. }
        ));
        plugin.exited("shell").unwrap();
        assert!(matches!(
            plugin.toggle("shell", &root).unwrap(),
            Action::Open { .. }
        ));
    }

    #[test]
    fn lua_plugin_owns_keybindings_and_sessions() {
        let plugin =
            ToggleTermPlugin::new(LuaRuntime::new(&std::env::current_dir().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            plugin.binding("space g").unwrap().as_deref(),
            Some("lazygit")
        );
        assert_eq!(plugin.binding("ctrl-t").unwrap().as_deref(), Some("shell"));
        assert_eq!(plugin.binding("space t").unwrap().as_deref(), Some("shell"));
        assert_eq!(plugin.binding("not-a-binding").unwrap(), None);
        assert!(plugin.dismiss("ctrl-t").unwrap());
        assert!(!plugin.dismiss("ctrl-n").unwrap());
        let cwd = std::env::temp_dir();
        match plugin.toggle("lazygit", &cwd).unwrap() {
            Action::Open { command, kind, .. } => {
                assert_eq!(command, "lazygit");
                assert_eq!(kind, "lazygit");
            }
            _ => panic!("first toggle must create PTY"),
        }
        assert_eq!(plugin.active().unwrap().as_deref(), Some("lazygit"));
        assert!(matches!(
            plugin.toggle("lazygit", &cwd).unwrap(),
            Action::Hide
        ));
        assert_eq!(plugin.active().unwrap(), None);
        assert!(matches!(
            plugin.toggle("lazygit", &cwd).unwrap(),
            Action::Show { .. }
        ));
        assert!(matches!(
            plugin.toggle("shell", &cwd).unwrap(),
            Action::Open { .. }
        ));
        assert_eq!(plugin.active().unwrap().as_deref(), Some("shell"));
        plugin.exited("shell").unwrap();
        assert_eq!(plugin.active().unwrap(), None);
        plugin.exited("lazygit").unwrap();
        assert!(matches!(
            plugin.toggle("lazygit", &cwd).unwrap(),
            Action::Open { .. }
        ));
    }
}
