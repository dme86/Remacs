-- Remacs' first bundled Lua plugin: a minimal nvim-tree-like explorer.
-- Only filesystem metadata is exposed by the host; opening files is an action
-- returned to Rust. The plugin owns the selection and expansion state.

local tree = {
  root = ".",
  nodes = {},
  expanded = {},
  selected = 1,
  top = 1,
  show_hidden = false,
}

local function name_of(path)
  return path:match("([^/]+)/*$") or path
end

local function refresh()
  local selected_path = tree.nodes[tree.selected] and tree.nodes[tree.selected].path
  local nodes = {}
  local max_depth = 32
  local max_nodes = 10000

  local function walk(directory, depth)
    if depth > max_depth or #nodes >= max_nodes then return end
    local ok, entries = pcall(remacs.fs.list, directory)
    if not ok then return end
    for _, entry in ipairs(entries) do
      if #nodes >= max_nodes then return end
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
  if selected_path then
    for i, node in ipairs(nodes) do
      if node.path == selected_path then
        tree.selected = i
        break
      end
    end
  end
  tree.top = math.max(1, math.min(tree.top, math.max(#nodes, 1)))
end

function tree.setup(root)
  tree.root = root
  tree.nodes = {}
  tree.expanded = {}
  tree.selected = 1
  tree.top = 1
  tree.show_hidden = false
  refresh()
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
  refresh()
end

function tree.on_key(key)
  local node = selected_node()

  if key == "j" or key == "down" then
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
    refresh()
  elseif key == "." or key == "H" then
    tree.show_hidden = not tree.show_hidden
    refresh()
  elseif key == "tab" or key == "esc" then
    return { action = "editor" }
  elseif key == "q" then
    return { action = "close" }
  end
  return { action = "none" }
end

function tree.render(height)
  height = math.max(0, height)
  if tree.selected < tree.top then
    tree.top = tree.selected
  end
  if tree.selected >= tree.top + height and height > 0 then
    tree.top = tree.selected - height + 1
  end

  local rows = {}
  for row = 1, height do
    local node = tree.nodes[tree.top + row - 1]
    if node then
      local indent = string.rep("  ", math.min(node.depth, 24))
      local symbol
      if node.dir then
        symbol = tree.expanded[node.path] and "▾ " or "▸ "
      else
        symbol = "  "
      end
      rows[row] = indent .. symbol .. node.name .. (node.dir and "/" or "")
    else
      rows[row] = ""
    end
  end

  local selected_row = tree.selected - tree.top + 1
  if not tree.nodes[tree.selected] or selected_row < 1 or selected_row > height then
    selected_row = 0
  end
  return {
    rows = rows,
    selected = selected_row,
    root_name = name_of(tree.root),
    count = #tree.nodes,
  }
end

return tree
