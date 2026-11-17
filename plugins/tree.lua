-- Remacs file explorer / fuzzy finder (first bundled Lua plugin).
-- Filesystem listing is provided by the Rust host. Tree and search state are
-- managed in Lua; no filesystem writes or editor buffer mutations happen here.

local tree = {
  root = ".",
  nodes = {},
  expanded = {},
  selected = 1,
  top = 1,
  show_hidden = false,
  searching = false,
  query = "",
  files = nil,
  results = {},
  total_matches = 0,
  search_selected = 1,
  search_top = 1,
}

local MAX_NODES = 10000
local MAX_SEARCH_FILES = 20000
local MAX_SEARCH_DEPTH = 32
local MAX_RESULTS = 150
local skipped_directories = {
  [".git"] = true,
  [".hg"] = true,
  [".svn"] = true,
  ["node_modules"] = true,
  ["target"] = true,
  ["dist"] = true,
  ["build"] = true,
  [".next"] = true,
  [".venv"] = true,
  ["venv"] = true,
}

local function name_of(path)
  return path:match("([^/]+)/*$") or path
end

local function refresh_tree()
  local previous = tree.nodes[tree.selected] and tree.nodes[tree.selected].path
  local nodes = {}

  local function walk(directory, depth)
    if depth > MAX_SEARCH_DEPTH or #nodes >= MAX_NODES then return end
    local ok, entries = pcall(remacs.fs.list, directory)
    if not ok then return end
    for _, entry in ipairs(entries) do
      if #nodes >= MAX_NODES then return end
      if tree.show_hidden or entry.name:sub(1, 1) ~= "." then
        nodes[#nodes + 1] = {
          name = entry.name,
          path = entry.path,
          dir = entry.is_dir,
          depth = depth,
        }
        if entry.is_dir and tree.expanded[entry.path] then
          walk(entry.path, depth + 1)
        end
      end
    end
  end

  walk(tree.root, 0)
  tree.nodes = nodes
  tree.selected = math.max(1, math.min(tree.selected, #nodes))
  if previous then
    for i, node in ipairs(nodes) do
      if node.path == previous then
        tree.selected = i
        break
      end
    end
  end
  tree.top = math.max(1, math.min(tree.top, math.max(#nodes, 1)))
end

-- Lazily enumerate project files once when '/' opens the finder.
-- Scans are bounded and skip huge generated/dependency directories.
local function scan_files()
  local files = {}
  local function walk(directory, prefix, depth)
    if depth > MAX_SEARCH_DEPTH or #files >= MAX_SEARCH_FILES then return end
    local ok, entries = pcall(remacs.fs.list, directory)
    if not ok then return end
    for _, entry in ipairs(entries) do
      if #files >= MAX_SEARCH_FILES then return end
      if tree.show_hidden or entry.name:sub(1, 1) ~= "." then
        local relative = prefix .. entry.name
        if entry.is_dir then
          if not skipped_directories[entry.name] then
            walk(entry.path, relative .. "/", depth + 1)
          end
        else
          files[#files + 1] = {
            name = relative,
            path = entry.path,
            dir = false,
            depth = 0,
          }
        end
      end
    end
  end
  walk(tree.root, "", 0)
  tree.files = files
end

-- Ordered-subsequence score, with bonuses for contiguous matches, word
-- boundaries, filename matches and early positions. nil = no match.
local function fuzzy_score(value, query)
  local haystack = value:lower()
  local needle = query:lower()
  local offset = 1
  local score = 0
  local previous = 0
  for i = 1, #needle do
    local ch = needle:sub(i, i)
    local pos = haystack:find(ch, offset, true)
    if not pos then return nil end
    score = score + 10
    if pos == previous + 1 then score = score + 8 end
    if pos == 1 or haystack:sub(pos - 1, pos - 1):find("[/_%.%- ]") then
      score = score + 12
    end
    if pos <= 4 then score = score + 4 end
    previous = pos
    offset = pos + 1
  end
  local basename = name_of(haystack)
  if basename:find(needle, 1, true) then score = score + 24 end
  return score - math.floor(#value / 12)
end

local function update_results()
  if not tree.files then scan_files() end
  local results = {}
  for _, node in ipairs(tree.files) do
    -- An empty query should immediately offer files instead of showing a
    -- blank pane (which previously made the finder seem broken).
    local score = tree.query == "" and 0 or fuzzy_score(node.name, tree.query)
    if score then
      results[#results + 1] = { node = node, score = score }
    end
  end
  table.sort(results, function(a, b)
    if a.score ~= b.score then return a.score > b.score end
    return a.node.name:lower() < b.node.name:lower()
  end)
  tree.results = {}
  tree.total_matches = #results
  for i = 1, math.min(#results, MAX_RESULTS) do
    tree.results[i] = results[i].node
  end
  tree.search_selected = 1
  tree.search_top = 1
end

local function start_search()
  tree.searching = true
  tree.query = ""
  tree.files = nil
  tree.results = {}
  tree.search_selected = 1
  tree.search_top = 1
  scan_files()
  update_results()
end

local function cancel_search()
  tree.searching = false
  tree.query = ""
  tree.results = {}
  tree.total_matches = 0
  tree.files = nil
  tree.search_selected = 1
  tree.search_top = 1
end

function tree.setup(root)
  tree.root = root
  tree.nodes = {}
  tree.expanded = {}
  tree.selected = 1
  tree.top = 1
  tree.show_hidden = false
  cancel_search()
  refresh_tree()
end

local function selected_node()
  return tree.nodes[tree.selected]
end

local function toggle_dir(node, expand)
  if not node or not node.dir then return end
  if expand == nil then
    tree.expanded[node.path] = not tree.expanded[node.path]
  else
    tree.expanded[node.path] = expand
  end
  refresh_tree()
end

-- The host passes sanitized, single-line UTF-8. Append in one operation so
-- a paste ranks matches once, and can never act as tree navigation commands.
function tree.paste_query(text)
  if not tree.searching then return end
  local available = math.max(0, 128 - #tree.query)
  local last = math.min(#text, available)
  if last < #text then
    -- Round a truncated prefix down to a complete Unicode character.
    last = utf8.offset(text, 0, last + 1) - 1
  end
  if last > 0 then
    tree.query = tree.query .. text:sub(1, last)
    update_results()
  end
end

function tree.on_key(key)
  if tree.searching then
    if key == "esc" then
      cancel_search()
    elseif key == "enter" then
      local node = tree.results[tree.search_selected]
      if node then
        cancel_search()
        return { action = "open", path = node.path }
      end
    elseif key == "down" or key == "tab" then
      tree.search_selected = math.min(#tree.results, tree.search_selected + 1)
      tree.search_selected = math.max(1, tree.search_selected)
    elseif key == "up" or key == "backtab" then
      tree.search_selected = math.max(1, tree.search_selected - 1)
    elseif key == "backspace" then
      local last = utf8.offset(tree.query, -1)
      if last then tree.query = tree.query:sub(1, last - 1) end
      update_results()
    elseif key == "ctrl-u" then
      tree.query = ""
      update_results()
    elseif utf8.len(key) == 1 and #tree.query + #key <= 128 then
      tree.query = tree.query .. key
      update_results()
    end
    return { action = "none" }
  end

  local node = selected_node()
  if key == "/" or key == "f" then
    start_search()
  elseif key == "j" or key == "down" then
    tree.selected = math.min(#tree.nodes, tree.selected + 1)
  elseif key == "k" or key == "up" then
    tree.selected = math.max(1, tree.selected - 1)
  elseif key == "g" then
    tree.selected = 1
  elseif key == "G" then
    tree.selected = math.max(1, #tree.nodes)
  elseif key == "l" or key == "right" then
    if node and node.dir then
      if not tree.expanded[node.path] then
        toggle_dir(node, true)
      else
        tree.selected = math.min(#tree.nodes, tree.selected + 1)
      end
    end
  elseif key == "h" or key == "left" or key == "backspace" then
    if node and node.dir and tree.expanded[node.path] then
      toggle_dir(node, false)
    elseif node then
      for i = tree.selected - 1, 1, -1 do
        if tree.nodes[i].depth < node.depth then
          tree.selected = i
          break
        end
      end
    end
  elseif key == "enter" or key == "space" then
    if node then
      if node.dir then
        toggle_dir(node)
      else
        return { action = "open", path = node.path }
      end
    end
  elseif key == "r" then
    refresh_tree()
    tree.files = nil
  elseif key == "." or key == "H" then
    tree.show_hidden = not tree.show_hidden
    refresh_tree()
    tree.files = nil
  elseif key == "tab" or key == "esc" then
    return { action = "editor" }
  elseif key == "q" then
    return { action = "close" }
  end
  return { action = "none" }
end

function tree.render(height)
  height = math.max(0, height)
  local searching = tree.searching
  local nodes = searching and tree.results or tree.nodes
  local selected = searching and tree.search_selected or tree.selected
  local top = searching and tree.search_top or tree.top
  if selected < top then top = selected end
  if selected >= top + height and height > 0 then
    top = selected - height + 1
  end
  if searching then tree.search_top = top else tree.top = top end

  local rows = {}
  for row = 1, height do
    local node = nodes[top + row - 1]
    if node then
      if searching then
        rows[row] = "  " .. node.name
      else
        local indent = string.rep("  ", math.min(node.depth, 24))
        local symbol
        if node.dir then
          symbol = tree.expanded[node.path] and "▾ " or "▸ "
        else
          symbol = "  "
        end
        rows[row] = indent .. symbol .. node.name .. (node.dir and "/" or "")
      end
    else
      rows[row] = ""
    end
  end

  local selected_row = selected - top + 1
  if not nodes[selected] or selected_row < 1 or selected_row > height then
    selected_row = 0
  end
  local title
  if searching then
    title = " FIND / " .. tree.query .. " [" .. #tree.results .. "]"
  else
    title = " FILES  " .. name_of(tree.root)
  end
  local search_matches = {}
  if searching then
    for i, entry in ipairs(tree.results) do
      search_matches[i] = entry.name
    end
  end
  return {
    rows = rows,
    selected = selected_row,
    root_name = name_of(tree.root),
    title = title,
    searching = searching,
    query = tree.query,
    count = #nodes,
    matches = search_matches,
    search_selected = tree.search_selected,
    total_matches = tree.total_matches,
    scanned = tree.files and #tree.files or 0,
  }
end

return tree
