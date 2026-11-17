-- Remacs LSP plugin: language servers, project detection and keymaps.
-- Rust hosts the async JSON-RPC transport; Lua owns all policy.
-- Server executables are installed separately and are launched on demand.
local M = {
  servers = {
    rust = {
      files = { "rs" },
      cmd = { "rust-analyzer" },
      roots = { "Cargo.toml", ".git" },
    },
    -- Add another language by providing extensions, command and root markers.
    -- go = { files = { "go" }, cmd = { "gopls" }, roots = { "go.mod", ".git" } },
  },
  keys = {
    ["gd"] = "definition",
    ["K"] = "hover",
  },
}

function M.server_for(extension)
  for language, server in pairs(M.servers) do
    for _, ext in ipairs(server.files) do
      if extension == ext then
        return {
          language = language,
          cmd = server.cmd,
          roots = server.roots,
        }
      end
    end
  end
end

function M.binding(keys)
  return M.keys[keys]
end

return M
