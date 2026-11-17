-- Remacs' floating terminal plugin, inspired by toggleterm.nvim.
-- Lua owns mappings, which session is visible, commands and session lifetime.
-- The Rust host only supplies PTY I/O and terminal rendering.
--
-- Edit these mappings/commands to customize your installed Remacs build.
local M = {}
local sessions = {}
local active = nil

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
  return active
end

function M.toggle(kind, cwd)
  local program = M.programs[kind]
  if not program then return { action = "error", message = "Unknown popup: " .. tostring(kind) } end
  if active == kind then
    active = nil
    return { action = "hide", kind = kind }
  end
  active = kind
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
  local previous = active
  active = nil
  return previous
end

function M.exited(kind)
  sessions[kind] = nil
  if active == kind then active = nil end
end

return M
