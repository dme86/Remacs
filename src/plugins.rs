//! Small Lua plugin host. The first bundled plugin is a file tree.
//!
//! Plugins communicate through a narrow API: read-only filesystem metadata,
//! a render result, and action messages. Rust owns editor buffers and TUI I/O.
use std::{
    fs,
    path::{Path, PathBuf},
};

use mlua::{Function, Lua, RegistryKey, Table};

pub enum TreeAction {
    None,
    Open(PathBuf),
    Editor,
    Close,
}

pub struct TreeView {
    pub title: String,
    pub lines: Vec<String>,
    /// 1-based index in the *visible* rows, or 0 when nothing is selected.
    pub selected: usize,
}

pub struct TreePlugin {
    lua: Lua,
    key: RegistryKey,
    pub visible: bool,
    pub focused: bool,
}

impl TreePlugin {
    pub fn new(root: &Path) -> mlua::Result<Self> {
        let lua = Lua::new();
        let remacs = lua.create_table()?;
        let filesystem = lua.create_table()?;

        // One deliberately small host API for the plugin. Never expose the
        // Rust Editor directly or make document writes from Lua callbacks.
        filesystem.set(
            "list",
            lua.create_function(|lua, path: String| {
                let mut entries = Vec::new();
                for entry in fs::read_dir(&path).map_err(mlua::Error::external)? {
                    let entry = entry.map_err(mlua::Error::external)?;
                    let file_type = entry.file_type().map_err(mlua::Error::external)?;
                    entries.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path().to_string_lossy().into_owned(),
                        file_type.is_dir(),
                    ));
                }
                entries.sort_by(|a, b| {
                    b.2.cmp(&a.2)
                        .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
                });
                let table = lua.create_table()?;
                for (index, (name, path, is_dir)) in entries.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("name", name)?;
                    row.set("path", path)?;
                    row.set("is_dir", is_dir)?;
                    table.set(index + 1, row)?;
                }
                Ok(table)
            })?,
        )?;
        remacs.set("fs", filesystem)?;
        lua.globals().set("remacs", remacs)?;

        // Embedded source keeps this working after cargo install --path .
        // Lua owns navigation/tree expansion, not terminal drawing.
        let plugin: Table = lua
            .load(include_str!("../plugins/tree.lua"))
            .set_name("plugins/tree.lua")
            .eval()?;
        let setup: Function = plugin.get("setup")?;
        setup.call::<()>(root.to_string_lossy().into_owned())?;
        let key = lua.create_registry_value(plugin)?;
        Ok(Self {
            lua,
            key,
            visible: false,
            focused: false,
        })
    }

    fn plugin(&self) -> mlua::Result<Table> {
        self.lua.registry_value(&self.key)
    }

    pub fn toggle(&mut self) -> mlua::Result<()> {
        if !self.visible {
            // Pick up newly created/deleted files when reopening.
            let _: TreeAction = self.key_action("r")?;
        }
        self.visible = !self.visible;
        self.focused = self.visible;
        Ok(())
    }

    pub fn focus(&mut self) {
        if self.visible {
            self.focused = true;
        }
    }

    pub fn leave(&mut self) {
        self.focused = false;
    }

    pub fn close(&mut self) {
        self.focused = false;
        self.visible = false;
    }

    fn key_action(&self, key: &str) -> mlua::Result<TreeAction> {
        let plugin = self.plugin()?;
        let on_key: Function = plugin.get("on_key")?;
        let value: Table = on_key.call(key)?;
        let action: String = value.get("action")?;
        match action.as_str() {
            "open" => Ok(TreeAction::Open(PathBuf::from(value.get::<String>("path")?))),
            "editor" => Ok(TreeAction::Editor),
            "close" => Ok(TreeAction::Close),
            _ => Ok(TreeAction::None),
        }
    }

    pub fn on_key(&self, key: &str) -> mlua::Result<TreeAction> {
        self.key_action(key)
    }

    pub fn view(&self, height: usize) -> mlua::Result<TreeView> {
        let plugin = self.plugin()?;
        let render: Function = plugin.get("render")?;
        let result: Table = render.call(height)?;
        let rows: Table = result.get("rows")?;
        let lines = rows.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?;
        let root: String = result.get("root_name")?;
        Ok(TreeView {
            title: format!(" FILES  {root}"),
            lines,
            selected: result.get("selected")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_tree_navigates_folders_and_returns_file_action() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(temp.join("src")).unwrap();
        fs::write(temp.join("README.md"), "# Remacs").unwrap();
        fs::write(temp.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(temp.join(".hidden"), "secret").unwrap();
        let mut tree = TreePlugin::new(&temp).unwrap();
        assert!(!tree.visible);
        tree.toggle().unwrap();
        assert!(tree.visible && tree.focused);
        let top = tree.view(10).unwrap();
        assert_eq!(top.selected, 1);
        assert!(top.lines.iter().any(|line| line.contains("src/")));
        assert!(!top.lines.iter().any(|line| line.contains(".hidden")));
        tree.on_key("l").unwrap();
        assert!(tree.view(10).unwrap().lines.iter().any(|line| line.contains("main.rs")));
        tree.on_key("j").unwrap();
        match tree.on_key("enter").unwrap() {
            TreeAction::Open(path) => assert!(path.ends_with("main.rs")),
            _ => panic!("expected the selected file"),
        }
        tree.on_key(".").unwrap();
        assert!(tree.view(10).unwrap().lines.iter().any(|line| line.contains(".hidden")));
        tree.toggle().unwrap();
        assert!(!tree.visible);
        let _ = fs::remove_dir_all(temp);
    }
}
