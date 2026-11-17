-- LSP navigation keymaps. Language-specific server/installer policy is
-- centralized in plugins/mason.lua (loaded as remacs.mason by the Rust host).
local mason = require("remacs.mason")

local M = {
  keys = {
    ["gd"] = "definition",
    ["K"] = "hover",
  },
}

function M.server_for(extension)
  return mason.server_for(extension)
end

function M.binding(keys)
  return M.keys[keys]
end

return M
