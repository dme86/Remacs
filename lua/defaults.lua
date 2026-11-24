-- Remacs' default editor policy. Rust implements the named primitives below;
-- commands and keys are ordinary Lua registrations and may be replaced in init.lua.
local M = {}

local function native(name)
  return function(_, context)
    remacs.core(name, context)
  end
end

local function command(name, action)
  remacs.commands.add(name, native(action or name))
end

local function bind(mode, keys, name)
  remacs.keymap.set(mode, keys, name)
end

for _, name in ipairs({
  "save", "save_all", "quit", "open_prompt", "command_prompt",
  "buffer_picker", "buffer_next", "buffer_previous", "buffer_last",
  "select_function", "undo", "redo", "insert_mode", "append",
  "insert_home", "insert_end", "open_below", "open_above",
  "left", "right", "up", "down", "home", "end",
  "word_forward", "word_backward", "first_line", "last_line",
  "delete_char", "delete_line", "yank_line", "paste", "visual_mode",
  "visual_yank", "visual_delete", "scroll_down_half", "scroll_up_half",
  "scroll_down_page", "scroll_up_page", "jump_back", "jump_forward",
  "change_back", "change_forward", "cancel", "normal_mode", "jumps", "changes",
  "window_split", "window_next", "window_previous", "window_left", "window_right",
  "window_close", "window_only", "window_grow", "window_shrink", "window_equalize",
}) do
  command(name)
end

remacs.commands.add("buffer_close", function(_, context)
  remacs.core("buffer_close", context)
end)

-- Command-minibuffer aliases share the same registry as user commands.
remacs.commands.add("w", function(args)
  remacs.core("save", { path = args ~= "" and args or nil })
end)
for _, name in ipairs({ "wa", "wall" }) do command(name, "save_all") end
command("q", "quit")
command("q!", "quit_force")
command("wq", "write_quit")
command("x", "write_quit")
remacs.commands.add("e", function(args)
  if args ~= "" then remacs.core("open", { path = args }) end
end)
for _, name in ipairs({ "buffers", "ls" }) do command(name, "buffer_picker") end
for _, name in ipairs({ "bn", "bnext" }) do command(name, "buffer_next") end
for _, name in ipairs({ "bp", "bprev" }) do command(name, "buffer_previous") end
command("b#", "buffer_last")
remacs.commands.add("bd", function() remacs.core("buffer_close", { force = false }) end)
remacs.commands.add("bd!", function() remacs.core("buffer_close", { force = true }) end)
remacs.commands.add("syntax", function(args) remacs.core("syntax", { args = args }) end)

for _, name in ipairs({ "vsplit", "vs" }) do
  remacs.commands.add(name, function(args)
    remacs.core("window_split", { path = args ~= "" and args or nil })
  end)
end
command("only", "window_only")
command("close", "window_close")
remacs.commands.add("wincmd", function(args, context)
  local operation = {
    v = "window_split", w = "window_next", W = "window_previous",
    h = "window_left", l = "window_right", c = "window_close", o = "window_only",
    [">"] = "window_grow", ["<"] = "window_shrink", ["="] = "window_equalize",
  }
  local action = operation[args]
  if action then remacs.core(action, context)
  else remacs.message("Usage: wincmd v|w|W|h|l|c|o|>|<|=") end
end)


-- Global shortcuts retain their old availability in Insert and Visual modes.
for keys, name in pairs({
  ["ctrl-x ctrl-f"] = "open_prompt",
  ["ctrl-x ctrl-s"] = "save",
  ["ctrl-x ctrl-c"] = "quit",
  ["ctrl-x 3"] = "window_split", ["ctrl-x o"] = "window_next",
  ["ctrl-x 0"] = "window_close", ["ctrl-x 1"] = "window_only",
  ["ctrl-p"] = "buffer_picker",
  ["ctrl-g"] = "cancel",
  ["esc"] = "normal_mode",
  ["pagedown"] = "scroll_down_page",
  ["pageup"] = "scroll_up_page",
}) do bind("global", keys, name) end

for keys, name in pairs({
  ["space f f"] = "open_prompt", ["space f s"] = "save",
  ["space b b"] = "buffer_picker", ["space b n"] = "buffer_next",
  ["space b p"] = "buffer_previous", ["space b l"] = "buffer_last",
  ["space b d"] = "buffer_close", ["space b s"] = "save_all",
  ["space q q"] = "quit", ["space v f"] = "select_function",
  ["alt-j"] = "buffer_next", ["alt-k"] = "buffer_previous",
  ["ctrl-pagedown"] = "buffer_next", ["ctrl-pageup"] = "buffer_previous",
  ["alt-left"] = "jump_back", ["alt-right"] = "jump_forward",
  ["ctrl-o"] = "jump_back", ["ctrl-i"] = "jump_forward",
  ["ctrl-r"] = "redo", ["u"] = "undo",
  ["i"] = "insert_mode", ["a"] = "append", ["I"] = "insert_home",
  ["A"] = "insert_end", ["o"] = "open_below", ["O"] = "open_above",
  ["v"] = "visual_mode", ["G"] = "last_line", ["g g"] = "first_line",
  ["g ;"] = "change_back", ["g ,"] = "change_forward",
  ["d d"] = "delete_line", ["y y"] = "yank_line",
  ["x"] = "delete_char", ["p"] = "paste", [":"] = "command_prompt",
}) do bind("normal", keys, name) end

-- Doom-style window leader and Vim's Ctrl+W use the same Lua commands.
for keys, name in pairs({
  v = "window_split", w = "window_next", W = "window_previous",
  h = "window_left", l = "window_right", c = "window_close", o = "window_only",
  [">"] = "window_grow", ["<"] = "window_shrink", ["="] = "window_equalize",
}) do
  bind("normal", "space w " .. keys, name)
  bind("normal", "ctrl-w " .. keys, name)
end
bind("normal", "ctrl-w ctrl-w", "window_next")

for _, mode in ipairs({ "normal", "visual" }) do
  for keys, name in pairs({
    ["h"] = "left", ["left"] = "left", ["j"] = "down", ["down"] = "down",
    ["k"] = "up", ["up"] = "up", ["l"] = "right", ["right"] = "right",
    ["0"] = "home", ["home"] = "home", ["$"] = "end", ["end"] = "end",
    ["w"] = "word_forward", ["b"] = "word_backward",
    ["ctrl-d"] = "scroll_down_half", ["ctrl-u"] = "scroll_up_half",
    ["ctrl-f"] = "scroll_down_page", ["ctrl-b"] = "scroll_up_page",
    ["ctrl-v"] = "scroll_down_page", ["alt-v"] = "scroll_up_page",
  }) do bind(mode, keys, name) end
end

bind("visual", "v", "normal_mode")
bind("visual", "y", "visual_yank")
bind("visual", "d", "visual_delete")
bind("visual", "a f", "select_function")

-- Insert text itself and completion navigation remain native. Ordinary text
-- needs no Lua command callback; every key can still be overridden at runtime.
for keys, name in pairs({
  left = "left", right = "right", up = "up", down = "down",
  home = "home", ["end"] = "end",
}) do bind("insert", keys, name) end

return M
