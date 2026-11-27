-- Public, versioned editor API. Rust executes queued mutations after callbacks
-- return; Lua never retains a reference to the Rust Editor.
local r = remacs
r.api_version = 1
r.state = {}
r._context = { buffers = {}, mode = "normal" }
local commands, mappings, hooks = {}, {}, {}
local preserved_priorities, next_priority = {}, 0
local actions, owner, prefix, prefix_mode = {}, "interactive", "", nil
local prefix_index, prefix_dirty = {}, true

local function copy(t)
  local out = {}
  for k, v in pairs(t) do out[k] = v end
  return out
end

local function queue(action)
  if #actions >= 4096 then error("Lua action queue limit exceeded") end
  actions[#actions + 1] = action
end

-- Mutations queued by a failed callback must never reach editor buffers.
local function invoke(callback, ...)
  local checkpoint = #actions
  local result = table.pack(pcall(callback, ...))
  if not result[1] then
    for i = #actions, checkpoint + 1, -1 do actions[i] = nil end
    error(result[2], 0)
  end
  return table.unpack(result, 2, result.n)
end

local function register(registry, key, entry)
  local kind = registry == commands and "commands" or registry == mappings and "mappings" or "hooks"
  local preserved = preserved_priorities[owner]
  local priorities = preserved and preserved[kind]
  next_priority = next_priority + 1
  entry.owner, entry.priority = owner, priorities and priorities[key] or next_priority
  local layers = {}
  for _, previous in ipairs(registry[key] or {}) do
    if previous.owner ~= owner then layers[#layers + 1] = previous end
  end
  layers[#layers + 1] = entry
  table.sort(layers, function(a, b) return a.priority < b.priority end)
  registry[key] = layers
end

local function current(registry, key)
  local layers = registry[key]
  local entry = layers and layers[#layers]
  return entry and not entry.deleted and entry or nil
end

local function owned_remove(registry, who)
  for key, layers in pairs(registry) do
    local retained = {}
    for _, entry in ipairs(layers) do
      if entry.owner ~= who then retained[#retained + 1] = entry end
    end
    registry[key] = #retained > 0 and retained or nil
  end
end

local function normalize(keys)
  assert(type(keys) == "string" and keys ~= "", "keys must be a nonempty string")
  return keys:gsub("^%s+", ""):gsub("%s+$", ""):gsub("%s+", " ")
end

r.commands = {}
function r.commands.add(name, callback)
  assert(type(name) == "string" and name ~= "", "command name must be nonempty")
  assert(type(callback) == "function", "command callback must be a function")
  register(commands, name, { callback = callback })
end
function r.commands.remove(name) register(commands, name, { deleted = true }) end
function r.commands.list()
  local names = {}
  for name in pairs(commands) do
    if current(commands, name) then names[#names + 1] = name end
  end
  table.sort(names)
  return names
end
function r.commands.run(name, args)
  local command = current(commands, name)
  if not command then return false end
  r._with_owner(command.owner, invoke, command.callback, args or "", r._context)
  return true
end

r.keymap = {}
function r.keymap.set(mode, keys, callback)
  assert(mode == "normal" or mode == "insert" or mode == "visual" or mode == "global",
    "mode must be normal, insert, visual or global")
  assert(type(callback) == "function" or type(callback) == "string",
    "mapping must be a callback or command name")
  register(mappings, mode .. ":" .. normalize(keys), { callback = callback })
  prefix_dirty = true
end
function r.keymap.del(mode, keys)
  register(mappings, mode .. ":" .. normalize(keys), { deleted = true })
  prefix_dirty = true
end
function r.keymap.list()
  local result = {}
  for key in pairs(mappings) do
    local mapping = current(mappings, key)
    if mapping then result[key] = mapping.owner end
  end
  return result
end
function r._cancel_keys()
  prefix, prefix_mode, r._key_pending, r._key_sequence = "", nil, false, ""
end

local function lookup(mode, keys)
  local exact = current(mappings, mode .. ":" .. keys) or current(mappings, "global:" .. keys)
  if exact then return exact, false end
  if prefix_dirty then
    prefix_index = {}
    for mapping in pairs(mappings) do
      if current(mappings, mapping) then
        local scope, sequence = mapping:match("^([^:]+):(.*)$")
        local partial = ""
        for key in sequence:gmatch("%S+") do
          if partial ~= "" then prefix_index[scope .. ":" .. partial] = true end
          partial = partial == "" and key or partial .. " " .. key
        end
      end
    end
    prefix_dirty = false
  end
  return nil, prefix_index[mode .. ":" .. keys] or prefix_index["global:" .. keys] or false
end

function r._has_key(mode, key)
  if prefix ~= "" and prefix_mode == mode then return true end
  local binding, pending = lookup(mode, key)
  return binding ~= nil or pending
end

function r._dispatch_key(mode, key)
  if prefix_mode ~= mode then r._cancel_keys() end
  if key == "esc" or key == "ctrl-g" then r._cancel_keys() end
  -- A complete sequence wins, including C-x C-f despite standalone C-f.
  -- Otherwise direct modified shortcuts preempt an unrelated pending leader.
  local complete, continuing
  if prefix ~= "" then complete, continuing = lookup(mode, prefix .. " " .. key) end
  if prefix ~= "" and not complete and not continuing and (key:match("^ctrl%-") or key:match("^alt%-"))
      and (current(mappings, mode .. ":" .. key) or current(mappings, "global:" .. key)) then
    r._cancel_keys()
  end
  local had_prefix = prefix ~= ""
  local keys = had_prefix and (prefix .. " " .. key) or key
  local binding, pending = lookup(mode, keys)
  if pending then
    prefix, prefix_mode, r._key_pending, r._key_sequence = keys, mode, true, keys
    return true
  end
  r._cancel_keys()
  if binding then
    if type(binding.callback) == "string" then
      assert(r.commands.run(binding.callback), "Unknown command: " .. binding.callback)
    else
      r._with_owner(binding.owner, invoke, binding.callback, r._context)
    end
    return true
  end
  -- Invalid multi-key sequences are consumed, like the original editor.
  return had_prefix
end

r.hooks = {}
function r.hooks.add(event, id, callback)
  assert(type(event) == "string" and event ~= "", "event must be nonempty")
  assert(type(id) == "string" and id ~= "", "hook id must be nonempty")
  assert(type(callback) == "function", "hook callback must be a function")
  register(hooks, event .. ":" .. id, { callback = callback, event = event })
end
function r.hooks.remove(event, id)
  register(hooks, event .. ":" .. id, { deleted = true })
end
function r.emit(event, data)
  -- Snapshot iteration allows hooks to add/remove hooks safely. Stable ids
  -- give deterministic callback order independent of Lua table hash order.
  local names, callbacks, errors = {}, {}, {}
  for id in pairs(hooks) do
    local hook = current(hooks, id)
    if hook and hook.event == event then names[#names + 1], callbacks[id] = id, hook end
  end
  table.sort(names)
  for _, id in ipairs(names) do
    local hook = callbacks[id]
    local ok, err = pcall(r._with_owner, hook.owner, invoke, hook.callback, data or {}, r._context)
    if not ok then errors[#errors + 1] = id .. ": " .. tostring(err) end
  end
  if #errors > 0 then error(table.concat(errors, "\n"), 0) end
end

r.buffer = {}
function r.buffer.current() return r._context.current end
function r.buffer.list()
  local result = {}
  for _, buffer in ipairs(r._context.buffers or {}) do result[#result + 1] = copy(buffer) end
  return result
end
function r.buffer.get(id)
  id = id or r.buffer.current()
  for _, buffer in ipairs(r._context.buffers or {}) do
    if buffer.id == id then return copy(buffer) end
  end
  error("Unknown buffer: " .. tostring(id))
end
function r.buffer.get_lines(id, first, last)
  id = id or r.buffer.current()
  local lines = r._get_lines and r._get_lines(id) or r.buffer.get(id).lines
  assert(lines, "Buffer text is only available during an editor callback")
  first, last = first or 1, last or #lines
  local result = {}
  for i = math.max(first, 1), math.min(last, #lines) do result[#result + 1] = lines[i] end
  return result
end
function r.buffer.set_lines(id, lines)
  assert(type(lines) == "table", "lines must be an array of strings")
  for _, line in ipairs(lines) do
    assert(type(line) == "string" and not line:find("[\r\n]"), "each line must be a string without newlines")
  end
  queue({ kind = "set_lines", id = id or r.buffer.current(), lines = copy(lines) })
end
function r.buffer.insert(id, text)
  assert(type(text) == "string", "insert text must be a string")
  queue({ kind = "insert", id = id or r.buffer.current(), text = text })
end
function r.buffer.set_cursor(id, row, col)
  assert(type(row) == "number" and row >= 1 and row % 1 == 0, "row must be a positive integer")
  assert(type(col) == "number" and col >= 1 and col % 1 == 0, "column must be a positive integer")
  queue({ kind = "cursor", id = id or r.buffer.current(), row = row, col = col })
end
function r.buffer.open(path)
  assert(type(path) == "string" and path ~= "", "path must be nonempty")
  queue({ kind = "open", path = path })
end
function r.buffer.switch(id) queue({ kind = "switch", id = id }) end
function r.buffer.close(id, force) queue({ kind = "close", id = id or r.buffer.current(), force = force == true }) end
-- Windows are views onto buffers, not copies of their text or undo history.
r.window = {}
function r.window.current() return r._context.window end
function r.window.list()
  local result = {}
  for _, window in ipairs(r._context.windows or {}) do result[#result + 1] = copy(window) end
  return result
end
local function window_id(id)
  id = id or r.window.current()
  assert(type(id) == "number" and id >= 1 and id % 1 == 0, "window id must be a positive integer")
  return id
end
function r.window.split(path)
  assert(path == nil or (type(path) == "string" and path ~= ""), "path must be nonempty")
  queue({ kind = "window", op = "split", path = path })
end
function r.window.focus(id) queue({ kind = "window", op = "focus", id = window_id(id) }) end
function r.window.close(id) queue({ kind = "window", op = "close", id = window_id(id) }) end
function r.window.only(id) queue({ kind = "window", op = "only", id = window_id(id) }) end
function r.window.resize(delta)
  assert(type(delta) == "number" and delta % 1 == 0, "resize delta must be an integer")
  queue({ kind = "window", op = "resize", delta = delta })
end
function r.window.equalize() queue({ kind = "window", op = "equalize" }) end

function r.message(text) queue({ kind = "message", text = tostring(text) }) end
function r.core(name, args)
  assert(type(name) == "string", "core operation name must be a string")
  queue({ kind = "core", name = name, args = args or {} })
end
function r._take_actions()
  local result = actions
  actions = {}
  return result
end

-- Module loaders set owner even for nested require calls, so reloading one
-- file removes only the registrations made by that file.
function r._with_owner(who, callback, ...)
  local previous = owner
  owner = who
  local result = table.pack(pcall(callback, ...))
  owner = previous
  if not result[1] then error(result[2], 0) end
  return table.unpack(result, 2, result.n)
end

-- Wrap ordinary Lua searchers too, so a user module loaded with require has
-- the same ownership and reload behavior as the bundled preload modules.
for index, searcher in ipairs(package.searchers) do
  package.searchers[index] = function(name)
    local result = table.pack(searcher(name))
    if type(result[1]) == "function" then
      local loader = result[1]
      result[1] = function(...)
        local loaded = table.pack(r._with_owner(name, invoke, loader, ...))
        local module = loaded[1]
        if type(module) == "table" then
          for key, callback in pairs(module) do
            if type(callback) == "function" then
              module[key] = function(...)
                return r._with_owner(name, invoke, callback, ...)
              end
            end
          end
        end
        return table.unpack(loaded, 1, loaded.n)
      end
    end
    return table.unpack(result, 1, result.n)
  end
end

local function transaction(owners, callback)
  local old_commands, old_mappings, old_hooks = commands, mappings, hooks
  local old_preserved = preserved_priorities
  preserved_priorities = copy(preserved_priorities)
  local old_loaded, checkpoint = copy(package.loaded), #actions
  commands, mappings, hooks = copy(commands), copy(mappings), copy(hooks)
  for _, who in ipairs(owners) do
    -- Preserve an existing owner's position below later user overrides while
    -- reloading it. Outside reload, every explicit registration wins normally.
    local priorities = { commands = {}, mappings = {}, hooks = {} }
    for kind, registry in pairs({ commands = commands, mappings = mappings, hooks = hooks }) do
      for key, layers in pairs(registry) do
        for _, entry in ipairs(layers) do
          if entry.owner == who then priorities[kind][key] = entry.priority end
        end
      end
    end
    preserved_priorities[who] = priorities
    owned_remove(commands, who)
    owned_remove(mappings, who)
    owned_remove(hooks, who)
  end
  prefix_dirty = true
  local result = table.pack(pcall(callback))
  preserved_priorities = old_preserved
  if not result[1] then
    commands, mappings, hooks = old_commands, old_mappings, old_hooks
    for key in pairs(package.loaded) do package.loaded[key] = nil end
    for key, value in pairs(old_loaded) do package.loaded[key] = value end
    for i = #actions, checkpoint + 1, -1 do actions[i] = nil end
    prefix_dirty = true
    error(result[2], 0)
  end
  prefix_dirty = true
  r._cancel_keys()
  return table.unpack(result, 2, result.n)
end

function r._load_module(name, reload)
  local bundled = { tree = true, toggleterm = true, lsp = true, mason = true, defaults = true }
  if bundled[name] then name = "remacs." .. name end
  if not reload and package.loaded[name] then return package.loaded[name] end
  local targets = { name }
  -- The LSP module may retain the shared Mason module in a local variable.
  if name == "remacs.mason" then targets[#targets + 1] = "remacs.lsp" end
  return transaction(targets, function()
    if reload then for _, target in ipairs(targets) do package.loaded[target] = nil end end
    local result = require(name)
    if reload and #targets > 1 then require("remacs.lsp") end
    return result
  end)
end

function r._load_file(path, source)
  local who = "file:" .. path
  return transaction({ who }, function()
    local chunk, err = load(source, "@" .. path, "t", _G)
    if not chunk then error(err, 0) end
    return r._with_owner(who, chunk)
  end)
end

function r._eval(source)
  local chunk, err = load(source, "=(Lua)", "t", _G)
  if not chunk then error(err, 0) end
  return invoke(chunk)
end

r.plugins = {}
function r.plugins.load(name) return r._host_load(name, false) end
function r.plugins.reload(name) return r._host_load(name, true) end
function r.plugins.list()
  local result = {}
  for _, plugin in ipairs(r._context.plugins or {}) do result[#result + 1] = copy(plugin) end
  return result
end
function r.plugins.add(spec)
  assert(type(spec) == "table", "plugin spec must be a table")
  assert(type(spec.name) == "string" and type(spec.repo) == "string", "plugin name and repo are required")
  assert(spec.branch == nil or type(spec.branch) == "string", "plugin branch must be a string")
  assert(spec.entry == nil or type(spec.entry) == "string", "plugin entry must be a string")
  queue({ kind = "plugin", op = "add", name = spec.name, repo = spec.repo,
    branch = spec.branch or "main", entry = spec.entry or "plugin.lua" })
end
function r.plugins.install(name)
  assert(type(name) == "string", "plugin name must be a string")
  queue({ kind = "plugin", op = "install", name = name })
end
function r.plugins.update(name)
  assert(name == nil or type(name) == "string", "plugin name must be a string")
  queue({ kind = "plugin", op = "update", name = name })
end
function r.plugins.check() queue({ kind = "plugin", op = "check" }) end
function r.plugins.remove(name)
  assert(type(name) == "string", "plugin name must be a string")
  queue({ kind = "plugin", op = "remove", name = name })
end
r.config = {}
function r.config.reload() return r._host_config() end
