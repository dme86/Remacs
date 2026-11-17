//! Read declarative desired language servers and install recipes from Lua.
//! The plugin owns package selection; Rust only executes typed recipes.
use mlua::{Function, Lua, RegistryKey, Table};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recipe {
    Rustup(Vec<String>),
    Go(String),
    Npm(String),
    Cargo(String),
    Brew(String),
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub id: String,
    pub label: String,
    pub language: String,
    pub command: Vec<String>,
    pub recipe: Recipe,
}

pub struct MasonPlugin {
    lua: Lua,
    plugin: RegistryKey,
}

impl MasonPlugin {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let table: Table = lua.load(include_str!("../plugins/mason.lua"))
            .set_name("plugins/mason.lua").eval()?;
        let plugin = lua.create_registry_value(table)?;
        Ok(Self { lua, plugin })
    }

    fn table(&self) -> mlua::Result<Table> {
        self.lua.registry_value(&self.plugin)
    }

    pub fn auto_install(&self) -> mlua::Result<bool> {
        self.table()?.get("auto_install")
    }

    pub fn auto_update_days(&self) -> mlua::Result<u64> {
        self.table()?.get("auto_update_days")
    }

    pub fn tools(&self) -> mlua::Result<Vec<Tool>> {
        let list: Table = self.table()?.get::<Function>("list")?.call(())?;
        let mut result = Vec::new();
        for entry in list.sequence_values::<Table>() {
            let entry = entry?;
            let id: String = entry.get("id")?;
            if id.is_empty() || !id.bytes().all(|ch| ch.is_ascii_lowercase()
                || ch.is_ascii_digit() || ch == b'_') {
                return Err(mlua::Error::external(format!("Invalid LSP id: {id}")));
            }
            let command = entry.get::<Table>("cmd")?
                .sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?;
            if command.first().is_none_or(String::is_empty) {
                return Err(mlua::Error::external(format!("Missing command for {id}")));
            }
            let recipe = entry.get::<Table>("install")?;
            let kind: String = recipe.get("kind")?;
            let recipe = match kind.as_str() {
                "rustup" => Recipe::Rustup(recipe.get::<Table>("components")?
                    .sequence_values::<String>().collect::<mlua::Result<_>>()?),
                "go" => Recipe::Go(recipe.get("module")?),
                "npm" => Recipe::Npm(recipe.get("package")?),
                "cargo" => Recipe::Cargo(recipe.get("package")?),
                "brew" => Recipe::Brew(recipe.get("package")?),
                _ => return Err(mlua::Error::external(
                    format!("Unknown installer kind {kind} for {id}"))),
            };
            // Package-manager arguments must be single, nonempty tokens.
            // They are passed as argv; never interpolated into a shell.
            let tokens: Vec<&str> = match &recipe {
                Recipe::Rustup(args) => args.iter().map(String::as_str).collect(),
                Recipe::Go(value) | Recipe::Npm(value) | Recipe::Cargo(value)
                    | Recipe::Brew(value) => vec![value],
            };
            if tokens.is_empty() || tokens.iter().any(|token| token.is_empty()
                || token.chars().any(char::is_whitespace) || token.starts_with('-')) {
                return Err(mlua::Error::external(format!(
                    "Unsafe or empty install recipe for {id}"
                )));
            }
            result.push(Tool {
                id,
                label: entry.get("label")?,
                language: entry.get("language")?,
                command,
                recipe,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_lua_catalog_defines_installer_policy() {
        let lua = MasonPlugin::new().unwrap();
        let tools = lua.tools().unwrap();
        let ids = tools.iter().map(|tool| tool.id.as_str()).collect::<Vec<_>>();
        assert!(ids.contains(&"rust_analyzer"));
        assert!(ids.contains(&"gopls"));
        assert!(ids.contains(&"lua_ls"));
        assert!(ids.contains(&"jsonls"));
        assert!(!ids.contains(&"ts_ls"));
        let rust = tools.iter().find(|tool| tool.id == "rust_analyzer").unwrap();
        assert_eq!(rust.command, ["rust-analyzer"]);
        assert!(matches!(&rust.recipe, Recipe::Rustup(components)
            if components == &["rust-analyzer", "rust-src"]));
        assert!(!lua.auto_install().unwrap());
        assert_eq!(lua.auto_update_days().unwrap(), 0);
    }
}
