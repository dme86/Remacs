-- Copy this file to your Remacs configuration directory, or try it immediately:
--   :luafile /absolute/path/to/Remacs/examples/init.lua
-- Edit the file and use :reload /absolute/path/to/Remacs/examples/init.lua.

-- This table survives reloading the configuration in the same editor session.
remacs.state.example = remacs.state.example or { greetings = 0 }
local state = remacs.state.example

remacs.commands.add("hello", function(args, context)
  state.greetings = state.greetings + 1
  local who = args ~= "" and args or "Remacs"
  remacs.message(string.format("Hello %s! (%d, buffer %s)",
    who, state.greetings, tostring(context.current)))
end)
remacs.keymap.set("normal", "space h", "hello")

-- Read and replace a buffer as one undoable change. Lines contain Unicode text.
remacs.commands.add("uppercase-buffer", function()
  local id = remacs.buffer.current()
  local lines = remacs.buffer.get_lines(id)
  for i, line in ipairs(lines) do lines[i] = string.upper(line) end
  remacs.buffer.set_lines(id, lines)
end)
remacs.keymap.set("normal", "space U", "uppercase-buffer")

-- Register a hook with a stable ID so loading this file repeatedly replaces it.
remacs.hooks.add("buffer_saved", "example.saved", function(event)
  remacs.message("Saved " .. tostring(event.path or "buffer"))
end)

-- Existing editor commands are callable from Lua; no native keys are required.
remacs.keymap.set("normal", "space S", function()
  remacs.commands.run("save_all", "")
end)
