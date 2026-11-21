-- LSP navigation keymaps. Language-specific server/installer policy is
-- centralized in plugins/mason.lua (loaded as remacs.mason by the Rust host).

local M = {
  keys = {
    ["gd"] = "definition",
    ["K"] = "hover",
  },
}

function M.server_for(extension)
  return require("remacs.mason").server_for(extension)
end

function M.binding(keys)
  return M.keys[keys]
end

for _, action in ipairs({ "definition", "hover" }) do
  remacs.commands.add("lsp-" .. action, function()
    remacs.core("lsp", { action = action })
  end)
end
for keys, action in pairs(M.keys) do
  -- Multi-key normal-mode sequences use the public, space-separated notation.
  local sequence = keys == "gd" and "g d" or keys
  remacs.keymap.set("normal", sequence, "lsp-" .. action)
end

return M
