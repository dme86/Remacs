-- Remacs' floating terminal plugin, inspired by toggleterm.nvim.
-- Lua owns mappings, which session is visible, commands and session lifetime.
-- The Rust host only supplies PTY I/O and terminal rendering.
--
-- Override this module or use remacs.keymap from init.lua; no rebuild needed.
local M = {}
local state = remacs.state.toggleterm or { sessions = {} }
remacs.state.toggleterm = state
local sessions = state.sessions

M.dismiss_key = "ctrl-t"

M.keys = {
  ["ctrl-t"] = "shell",
  ["space t"] = "shell",
  ["space g"] = "lazygit",
}

M.programs = {
  shell = { command = "shell", title = "dme :: popup terminal" },
  lazygit = { command = "lazygit", title = "dme :: lazygit" },
}

function M.binding(key)
  return M.keys[key]
end

function M.dismiss(key)
  return key == M.dismiss_key
end

function M.active()
  return state.active
end

function M.toggle(kind, cwd)
  local program = M.programs[kind]
  if not program then return { action = "error", message = "Unknown popup: " .. tostring(kind) } end
  if state.active == kind then
    state.active = nil
    return { action = "hide", kind = kind }
  end
  state.active = kind
  local session = sessions[kind]
  if session then
    return { action = "show", kind = kind, title = session.title }
  end
  session = { title = program.title, cwd = cwd }
  sessions[kind] = session
  return {
    action = "open",
    kind = kind,
    command = program.command,
    cwd = cwd,
    title = program.title,
  }
end

function M.hide()
  local previous = state.active
  state.active = nil
  return previous
end

function M.exited(kind)
  sessions[kind] = nil
  if state.active == kind then state.active = nil end
end

for kind in pairs(M.programs) do
  remacs.commands.add(kind, function() remacs.core("terminal", { kind = kind }) end)
end
remacs.commands.add("terminal", function() remacs.core("terminal", { kind = "shell" }) end)
for key, kind in pairs(M.keys) do
  local mode = key:match("^ctrl%-") and "global" or "normal"
  remacs.keymap.set(mode, key, kind)
end

return M
