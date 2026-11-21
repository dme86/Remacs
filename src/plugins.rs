//! File-tree view adapter for the shared editor Lua runtime.
//!
//! Plugins communicate through a narrow API: read-only filesystem metadata,
//! a render result, and action messages. Rust owns editor buffers and TUI I/O.
use std::path::{Path, PathBuf};

use mlua::{Function, Table};

use crate::lua_runtime::LuaRuntime;

pub enum TreeAction {
    None,
    Open(PathBuf),
    Editor,
    Close,
}

pub struct FinderView {
    pub query: String,
    pub matches: Vec<String>,
    /// 1-based index among matching files, not restricted to terminal height.
    pub selected: usize,
    pub total_matches: usize,
    pub scanned: usize,
}

pub struct TreeView {
    pub title: String,
    pub lines: Vec<String>,
    /// 1-based index in the *visible* rows, or 0 when nothing is selected.
    pub selected: usize,
    /// Column of the search input in the sidebar header, when searching.
    pub search_cursor: Option<usize>,
    pub finder: Option<FinderView>,
}

pub struct TreePlugin {
    runtime: LuaRuntime,
    pub visible: bool,
    pub focused: bool,
}

impl TreePlugin {
    pub fn new(runtime: LuaRuntime, root: &Path) -> mlua::Result<Self> {
        let setup: Function = runtime.table("tree")?.get("setup")?;
        setup.call::<()>(root.to_string_lossy().into_owned())?;
        Ok(Self {
            runtime,
            visible: false,
            focused: false,
        })
    }

    fn plugin(&self) -> mlua::Result<Table> {
        self.runtime.table("tree")
    }

    pub fn toggle(&mut self) -> mlua::Result<()> {
        if self.visible {
            // Cancel an active fuzzy search before hiding the pane; otherwise
            // reopening it would unexpectedly return to the old query.
            let _ = self.key_action("esc")?;
        } else {
            // Pick up newly created/deleted files when reopening.
            let _ = self.key_action("r")?;
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
            "open" => Ok(TreeAction::Open(PathBuf::from(
                value.get::<String>("path")?,
            ))),
            "editor" => Ok(TreeAction::Editor),
            "close" => Ok(TreeAction::Close),
            _ => Ok(TreeAction::None),
        }
    }

    pub fn on_key(&self, key: &str) -> mlua::Result<TreeAction> {
        self.key_action(key)
    }

    /// Append literal, single-line text to an active finder query. Outside
    /// search this is a no-op and never dispatches tree navigation shortcuts.
    /// The host removes controls/newlines; Lua enforces the UTF-8 byte limit.
    pub fn paste_query(&self, text: &str) -> mlua::Result<()> {
        let plugin = self.plugin()?;
        let paste: Function = plugin.get("paste_query")?;
        paste.call(text)
    }

    pub fn view(&self, height: usize) -> mlua::Result<TreeView> {
        let plugin = self.plugin()?;
        let render: Function = plugin.get("render")?;
        let result: Table = render.call(height)?;
        let rows: Table = result.get("rows")?;
        let lines = rows
            .sequence_values::<String>()
            .collect::<mlua::Result<Vec<_>>>()?;
        let root: String = result.get("root_name")?;
        let searching: bool = result.get("searching")?;
        let search_cursor = if searching {
            let query: String = result.get("query")?;
            // " FIND / " is eight terminal cells; leave the final column for the divider.
            Some(8 + unicode_width::UnicodeWidthStr::width(query.as_str()))
        } else {
            None
        };
        let finder = if searching {
            let matches: Table = result.get("matches")?;
            Some(FinderView {
                query: result.get("query")?,
                matches: matches
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()?,
                selected: result.get("search_selected")?,
                total_matches: result.get("total_matches")?,
                scanned: result.get("scanned")?,
            })
        } else {
            None
        };
        Ok(TreeView {
            title: result
                .get::<String>("title")
                .unwrap_or_else(|_| format!(" FILES  {root}")),
            lines,
            selected: result.get("selected")?,
            search_cursor,
            finder,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hot_reload_preserves_tree_expansion_dotfiles_and_search() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-tree-reload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(temp.join("src")).unwrap();
        fs::write(temp.join("src/answer.rs"), "42").unwrap();
        fs::write(temp.join(".hidden"), "secret").unwrap();
        let runtime = LuaRuntime::new(&temp).unwrap();
        let mut tree = TreePlugin::new(runtime.clone(), &temp).unwrap();
        tree.toggle().unwrap();
        tree.on_key("l").unwrap();
        tree.on_key(".").unwrap();
        let before = tree.view(10).unwrap().lines;
        runtime.reload("tree").unwrap();
        assert_eq!(tree.view(10).unwrap().lines, before);
        tree.on_key("/").unwrap();
        tree.paste_query("ans").unwrap();
        runtime.reload("tree").unwrap();
        let finder = tree.view(10).unwrap().finder.unwrap();
        assert_eq!(finder.query, "ans");
        assert_eq!(finder.matches, ["src/answer.rs"]);
        match tree.on_key("enter").unwrap() {
            TreeAction::Open(path) => assert_eq!(path, temp.join("src/answer.rs")),
            _ => panic!("reloaded finder must still open its selection"),
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn fuzzy_finder_finds_collapsed_nested_files_and_opens_match() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-fuzzy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(temp.join("src").join("deep")).unwrap();
        fs::create_dir_all(temp.join("target")).unwrap();
        fs::write(temp.join("src/deep/render_engine.rs"), "fn draw() {}").unwrap();
        fs::write(temp.join("src/deep/render_readme.md"), "# hi").unwrap();
        fs::write(temp.join("README.md"), "hello").unwrap();
        fs::write(temp.join(".secret.md"), "hidden").unwrap();
        fs::write(temp.join("target/generated.rs"), "excluded").unwrap();

        let mut tree = TreePlugin::new(LuaRuntime::new(&temp).unwrap(), &temp).unwrap();
        tree.toggle().unwrap();
        tree.on_key("/").unwrap();
        assert!(tree.view(10).unwrap().title.contains("FIND /"));
        let initial = tree.view(10).unwrap();
        let finder = initial.finder.as_ref().unwrap();
        assert!(
            !finder.matches.is_empty(),
            "opening the finder must show results"
        );
        assert!(finder.matches.iter().any(|name| name == "README.md"));
        assert_eq!(finder.selected, 1);
        assert!(finder.scanned >= 3);
        // Search the whole project, even before expanding the src folder.
        for c in "renger".chars() {
            tree.on_key(&c.to_string()).unwrap();
        }
        let view = tree.view(10).unwrap();
        assert!(view.title.contains("renger"));
        assert_eq!(view.search_cursor, Some(8 + "renger".len()));
        assert!(view.lines.iter().any(|s| s.contains("render_engine.rs")));
        assert!(!view.lines.iter().any(|s| s.contains("generated.rs")));
        match tree.on_key("enter").unwrap() {
            TreeAction::Open(path) => assert!(path.ends_with("render_engine.rs")),
            _ => panic!("enter must open the best match"),
        }
        assert!(tree.view(10).unwrap().search_cursor.is_none());

        tree.on_key("/").unwrap();
        for c in "secret".chars() {
            tree.on_key(&c.to_string()).unwrap();
        }
        assert!(tree
            .view(10)
            .unwrap()
            .lines
            .iter()
            .all(|s| !s.contains(".secret.md")));
        tree.on_key("esc").unwrap();
        tree.on_key(".").unwrap();
        tree.on_key("/").unwrap();
        for c in "secret".chars() {
            tree.on_key(&c.to_string()).unwrap();
        }
        assert!(tree
            .view(10)
            .unwrap()
            .lines
            .iter()
            .any(|s| s.contains(".secret.md")));
        // Escape cancels finder but keeps the tree focused.
        tree.on_key("esc").unwrap();
        assert!(tree.view(10).unwrap().search_cursor.is_none());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn fuzzy_navigation_backspace_and_unicode_input() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-fuzzy-unicode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("café.txt"), "text").unwrap();
        fs::write(temp.join("camel.txt"), "text").unwrap();
        let tree = TreePlugin::new(LuaRuntime::new(&temp).unwrap(), &temp).unwrap();
        tree.on_key("/").unwrap();
        for c in "ca".chars() {
            tree.on_key(&c.to_string()).unwrap();
        }
        assert_eq!(tree.view(10).unwrap().selected, 1);
        tree.on_key("down").unwrap();
        assert_eq!(tree.view(10).unwrap().selected, 2);
        tree.on_key("up").unwrap();
        assert_eq!(tree.view(10).unwrap().selected, 1);
        tree.on_key("é").unwrap();
        assert!(tree.view(10).unwrap().title.contains("caé"));
        tree.on_key("backspace").unwrap();
        assert!(tree.view(10).unwrap().title.contains("FIND / ca "));
        tree.on_key("ctrl-u").unwrap();
        let cleared = tree.view(10).unwrap();
        assert!(cleared
            .finder
            .as_ref()
            .is_some_and(|finder| finder.query.is_empty()));
        assert!(
            cleared.lines.iter().any(|line| line.contains("camel.txt")),
            "clearing the query should restore initial suggestions"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn finder_paste_is_literal_unicode_and_bounded() {
        let temp = std::env::temp_dir().join(format!(
            "remacs-fuzzy-paste-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("café.txt"), "text").unwrap();
        fs::write(temp.join("qj.txt"), "text").unwrap();
        let mut tree = TreePlugin::new(LuaRuntime::new(&temp).unwrap(), &temp).unwrap();
        tree.toggle().unwrap();
        let selected = tree.view(10).unwrap().selected;
        tree.paste_query("qj/f").unwrap();
        assert!(tree.visible && tree.focused);
        assert_eq!(tree.view(10).unwrap().selected, selected);
        assert!(tree.view(10).unwrap().finder.is_none());

        tree.on_key("/").unwrap();
        tree.paste_query("café").unwrap();
        let finder = tree.view(10).unwrap().finder.unwrap();
        assert_eq!(finder.query, "café");
        assert_eq!(finder.matches, vec!["café.txt"]);
        tree.on_key("ctrl-u").unwrap();
        tree.paste_query("qj").unwrap();
        let finder = tree.view(10).unwrap().finder.unwrap();
        assert_eq!(finder.query, "qj");
        assert_eq!(finder.matches, vec!["qj.txt"]);
        assert!(tree.visible && tree.focused);

        tree.on_key("ctrl-u").unwrap();
        tree.paste_query(&"é".repeat(63)).unwrap();
        tree.paste_query("界x").unwrap();
        assert_eq!(tree.view(10).unwrap().finder.unwrap().query, "é".repeat(63));
        tree.paste_query("é界").unwrap();
        assert_eq!(tree.view(10).unwrap().finder.unwrap().query, "é".repeat(64));
        tree.paste_query("overflow").unwrap();
        assert_eq!(tree.view(10).unwrap().finder.unwrap().query.len(), 128);
        let _ = fs::remove_dir_all(temp);
    }

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
        let mut tree = TreePlugin::new(LuaRuntime::new(&temp).unwrap(), &temp).unwrap();
        assert!(!tree.visible);
        tree.toggle().unwrap();
        assert!(tree.visible && tree.focused);
        let top = tree.view(10).unwrap();
        assert_eq!(top.selected, 1);
        assert!(top.lines.iter().any(|line| line.contains("src/")));
        assert!(!top.lines.iter().any(|line| line.contains(".hidden")));
        tree.on_key("l").unwrap();
        assert!(tree
            .view(10)
            .unwrap()
            .lines
            .iter()
            .any(|line| line.contains("main.rs")));
        tree.on_key("j").unwrap();
        match tree.on_key("enter").unwrap() {
            TreeAction::Open(path) => assert!(path.ends_with("main.rs")),
            _ => panic!("expected the selected file"),
        }
        tree.on_key(".").unwrap();
        assert!(tree
            .view(10)
            .unwrap()
            .lines
            .iter()
            .any(|line| line.contains(".hidden")));
        tree.toggle().unwrap();
        assert!(!tree.visible);
        let _ = fs::remove_dir_all(temp);
    }
}
