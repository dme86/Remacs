-- Remacs Mason-like language-server manager.
-- This file is the single source of truth for enabled languages, LSP commands,
-- root markers and installation recipes. No network activity on editor startup
-- unless auto_install or auto_update_days is explicitly enabled.
--
-- The managed installers (Go/npm/Cargo) write inside Remacs' data directory.
-- Rustup and Homebrew are external toolchain/system package managers.
local M = {
  ensure_installed = {
    "rust_analyzer",
    "lua_ls",
    "gopls",
    "bashls",
    "jsonls",
    "yamlls",
    -- Uncomment to enable additional language servers:
    -- "pyright", "ts_ls", "taplo",
  },

  auto_install = false,       -- true: install missing desired servers on launch
  auto_update_days = 0,      -- 0: manual updates only; 7: weekly refresh

  catalog = {
    rust_analyzer = {
      label = "Rust",
      language = "rust",
      files = {"rs"},
      roots = {"Cargo.toml", "rust-project.json", ".git"},
      cmd = {"rust-analyzer"},
      install = {kind = "rustup", components = {"rust-analyzer", "rust-src"}},
    },
    lua_ls = {
      label = "Lua",
      language = "lua",
      files = {"lua"},
      roots = {".luarc.json", ".luarc.jsonc", ".git"},
      cmd = {"lua-language-server"},
      -- Homebrew installs to its normal prefix, not Remacs' private folder.
      -- Linux users can install this binary with their package manager and
      -- then remove lua_ls from ensure_installed if brew isn't available.
      install = {kind = "brew", package = "lua-language-server"},
    },
    gopls = {
      label = "Go",
      language = "go",
      files = {"go"},
      roots = {"go.work", "go.mod", ".git"},
      cmd = {"gopls"},
      install = {kind = "go", module = "golang.org/x/tools/gopls@latest"},
    },
    bashls = {
      label = "Bash / Shell",
      language = "bash",
      files = {"sh", "bash", "zsh"},
      roots = {".git"},
      cmd = {"bash-language-server", "start"},
      install = {kind = "npm", package = "bash-language-server@latest"},
    },
    jsonls = {
      label = "JSON",
      language = "json",
      files = {"json", "jsonc"},
      roots = {"package.json", ".git"},
      cmd = {"vscode-json-language-server", "--stdio"},
      install = {kind = "npm", package = "vscode-langservers-extracted@latest"},
    },
    yamlls = {
      label = "YAML",
      language = "yaml",
      files = {"yaml", "yml"},
      roots = {".git"},
      cmd = {"yaml-language-server", "--stdio"},
      install = {kind = "npm", package = "yaml-language-server@latest"},
    },
    pyright = {
      label = "Python",
      language = "python",
      files = {"py", "pyi"},
      roots = {"pyproject.toml", "setup.py", ".git"},
      cmd = {"pyright-langserver", "--stdio"},
      install = {kind = "npm", package = "pyright@latest"},
    },
    ts_ls = {
      label = "TypeScript / JavaScript",
      language = "typescript",
      files = {"ts", "tsx", "js", "jsx"},
      roots = {"tsconfig.json", "package.json", ".git"},
      cmd = {"typescript-language-server", "--stdio"},
      install = {kind = "npm", package = "typescript-language-server@latest"},
    },
    taplo = {
      label = "TOML",
      language = "toml",
      files = {"toml"},
      roots = {"Cargo.toml", ".git"},
      cmd = {"taplo", "lsp", "stdio"},
      install = {kind = "cargo", package = "taplo-cli"},
    },
  },
}

function M.list()
  local result = {}
  for _, id in ipairs(M.ensure_installed) do
    local spec = M.catalog[id]
    if not spec then error("Unknown Mason language-server id: " .. tostring(id)) end
    table.insert(result, {
      id = id,
      label = spec.label,
      language = spec.language,
      cmd = spec.cmd,
      files = spec.files,
      roots = spec.roots,
      install = spec.install,
    })
  end
  return result
end

function M.server_for(extension)
  for _, spec in ipairs(M.list()) do
    for _, ext in ipairs(spec.files) do
      if extension == ext then
        return {
          language = spec.language,
          cmd = spec.cmd,
          roots = spec.roots,
        }
      end
    end
  end
end

return M
