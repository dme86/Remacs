# Extending Remacs with Lua

Remacs runs one persistent Lua 5.4 interpreter per editor session. The bundled
tree, terminal, LSP and Mason modules, your configuration and your plugins all
share that interpreter and the public `remacs` API. Loading a plugin does not
create another interpreter. Lua code can add commands, change keymaps, inspect
and edit buffers, and react to editor events without recompiling Rust.

## Configuration and interactive development

Configuration runs after bundled plugins, default keymaps and installed Git
plugins. See
[`examples/init.lua`](../examples/init.lua) for a working starting point.

The configuration directory is the first applicable location:

1. `$REMACS_CONFIG_DIR`, if set.
2. `$XDG_CONFIG_HOME/remacs`, if set.
3. `~/.config/remacs` (on both Linux and macOS).

Remacs loads `init.lua` from that directory. Its `lua/?.lua` and
`lua/?/init.lua` paths are added to Lua's module search path, so reusable modules
can live beside your configuration. `remacs.config_dir` contains the resolved
directory, `remacs.root` the editor's initial working directory, and
`remacs.api_version` is currently `1`.

For example:

```text
~/.config/remacs/
  init.lua
  lua/my_helpers.lua
  plugins/mason.lua
  plugins/tree.lua
```

Bundled module source is searched in this order: the configuration directory's
`plugins/` folder; each directory in the platform-separated
`REMACS_RUNTIME_PATH`; the development checkout's `plugins/` folder; embedded
installation defaults. The development fallback for `defaults.lua` is the
checkout's `lua/` folder. Each override directory contains files directly, such as
`tree.lua`. To customize Mason after installation, copy the repository's
`plugins/mason.lua` to `~/.config/remacs/plugins/mason.lua`, edit it, then run
`:reload mason`. The same override rules apply to `defaults.lua`.

The command minibuffer supports:

```text
:lua remacs.message("Hello from Lua")
:luafile /absolute/path/to/plugin.lua
:reload /absolute/path/to/plugin.lua
:reload tree
:reload mason
:reload
```

`:lua` executes a Lua chunk in the existing interpreter. `:luafile` executes a
file; `:reload PATH` loads its current contents again. `:reload NAME` reloads a
module, including any of the bundled plugins. With no argument, `:reload`
reloads the user configuration. A command you register as `greet` is available
immediately as `:greet arguments`.

Lua files are trusted programs with access to Lua's standard libraries. Do not
load code you would not run as your own user. Editor callbacks run on the editor
thread: keep them short, especially hooks. Rust continues to handle PTY sessions,
asynchronous language servers and installers, filesystem I/O, text storage,
syntax parsing and incremental terminal rendering.

## Managed Git plugins

The **`:plugins`** manager installs external Remacs plugins from Git repositories.
It uses the same Lua interpreter and public API as local modules. Installed
entries load in stable alphabetical order after bundled plugins/defaults and
before `init.lua`. User configuration therefore keeps the final say over
commands, mappings and hooks.

Register and install a plugin with:

```text
:plugins add NAME REPO [BRANCH] [ENTRY]
```

`NAME` identifies the registration; `REPO` is the Git repository URL. Defaults
are branch `main` and entry `plugin.lua` relative to the repository root. For
example, **replace this placeholder URL** before running:

```text
:plugins add notes https://example.com/you/remacs-notes.git main plugin.lua
```

`add` explicitly queues installation. `:plugins install NAME` installs an
already registered plugin; `:plugins sync` installs missing registrations only.
`:plugins update NAME` updates one installed plugin; `:plugins update` updates
all installed plugins, even if their last check did not report an update.
`:plugins check` compares remote branch hashes asynchronously. The same check
runs in the background at startup; availability produces a notification and
badge, without automatically updating installed code.

The popup accepts typing to filter, arrows/Tab to choose, Enter to install,
Ctrl+U to update the selected plugin, Ctrl+R to check, Ctrl+S to install missing
plugins, Ctrl+L to clear the filter and Esc to close.

Installation metadata and immutable Git commit snapshots live under the
absolute `REMACS_PLUGIN_HOME` override, or otherwise:

- macOS: `~/Library/Application Support/Remacs/plugins`.
- Linux: `$XDG_DATA_HOME/remacs/plugins`, falling back to
  `~/.local/share/remacs/plugins`.

Install/update selects code for the **next editor startup**; the current Lua
session keeps running its loaded version. `:plugins remove NAME` removes the
registration, unloads it on next startup and keeps cached snapshots. Local
source-development loaders such as `:luafile` and `:reload` remain available
as described above.

The manager is also accessible from Lua:

| API | Effect |
| --- | --- |
| `remacs.plugins.add({ name = NAME, repo = REPO, branch = "main", entry = "plugin.lua" })` | Register and queue installation; `branch` and `entry` are optional. |
| `remacs.plugins.install(name)` | Queue installation of a registered plugin. |
| `remacs.plugins.update(name)` | Queue an update of one installed plugin. |
| `remacs.plugins.update()` | Queue updates for all installed plugins. |
| `remacs.plugins.check()` | Queue remote hash checks. |
| `remacs.plugins.remove(name)` | Remove a registration; the running plugin remains until restart. |
| `remacs.plugins.list()` | Read registered plugin metadata from the editor snapshot. |

Each list entry contains `name` and `status`, plus `startup_sha` for the
snapshot selected at this startup and `load_error` if its entry failed.
Git must be available on `PATH` for installation and remote update checks.

Use the manager for explicit installation/update actions, rather than adding
an unconditional install or update to `init.lua`.

A minimal repository can contain:

```text
plugin.lua
lua/notes/greeting.lua
```

For example, `lua/notes/greeting.lua`:

```lua
return function(name) return "Hello " .. name end
```

And the root `plugin.lua`:

```lua
local greeting = require("notes.greeting")
remacs.commands.add("notes-hello", function(args)
  remacs.message(greeting(args ~= "" and args or "Remacs"))
end)
```

All installed snapshots' `lua/?.lua` and `lua/?/init.lua` paths are registered
before the first entry runs. Configuration-directory Lua paths take priority.
Namespace helpers by plugin, as above: every plugin shares `package.loaded`, so
generic module names such as `util` can collide. Plugins must target Remacs'
API; arbitrary Neovim plugins are not compatible merely because they use Lua.
This first manager does not resolve dependencies or run build hooks.

## Commands and keys

```lua
remacs.commands.add("greet", function(args, context)
  remacs.message("Hello " .. (args ~= "" and args or "editor"))
end)

remacs.keymap.set("normal", "space h", "greet")
remacs.keymap.set("normal", "space H", function(context)
  remacs.commands.run("greet", "from a keybinding")
end)

remacs.keymap.del("normal", "space h")
remacs.commands.remove("greet")
```

Command callbacks receive the argument string and a context table. Key callbacks
receive the context directly; a string keymap value names a command. Command
and keymap registrations replace that file's earlier registration of the same
name or key. Later-loaded user files can override bundled registrations.
`remacs.commands.list()` lists registered commands.
`remacs.keymap.list()` returns a table of mode/sequence keys and their owners.

Modes are `normal`, `insert`, `visual` and `global`. A mode's own binding takes
precedence over its global binding. Separate sequence keys with spaces:
`space f f`, `g d`, `ctrl-x ctrl-s`. Letters are case-sensitive, so `g` and `G`
are distinct. Special names include `esc`, `enter`, `tab`, `backtab`, `space`,
`backspace`, `delete`, `left`, `right`, `up`, `down`, `home`, `end`, `pageup`,
and `pagedown`. Modifiers use `ctrl-`, `alt-` and `shift-` prefixes, in that
order when combined (for example `ctrl-alt-x`). Shifted letters use their
actual character, such as `A`; shifted special keys use names like
`shift-left`, while Shift+Tab is `backtab`. Availability depends on the
terminal's keyboard protocol.

The context contains `mode`, `current` (the active buffer ID), `buffers`,
`window` (the focused window ID), `windows`,
`page_height` and an optional `count`. For example, `5j` supplies `count = 5` to
the `j` binding; an uncounted motion leaves `count` absent. Prefix sequences are
held in Lua's keymap dispatcher until complete. `Esc` and `Ctrl+G` cancel them.
Prompts, completion menus, pickers, the focused tree and active terminal keep
their own input handling before ordinary editor mappings. `global` applies to
the editor's three modes; it does not intercept every key in a focused popup.
The existing focus-changing shortcuts retain their availability across panes.

The default policy is in [`lua/defaults.lua`](../lua/defaults.lua), using this
same API. It registers named commands such as `save`, `save_all`, `buffer_next`,
`undo`, `insert_mode`, `select_function` and `scroll_down_half`, plus the existing
command aliases (`w`, `q`, `bn`, `bd`, and so on). You can invoke these commands
from your own Lua instead of reproducing native behavior.

`remacs.core(name, arguments)` queues a native primitive. It is the low-level
bridge used by default commands and bundled plugins. Prefer named commands and
the buffer API in user plugins; the core primitive names describe host
implementation operations rather than a second command namespace.

## Buffers

```lua
local id = remacs.buffer.current()
local all = remacs.buffer.list()
local metadata = remacs.buffer.get(id)
local lines = remacs.buffer.get_lines(id)

remacs.buffer.set_lines(id, { "first line", "second line" })
remacs.buffer.set_cursor(id, 2, 1)
remacs.buffer.insert(id, "Hello ")
remacs.buffer.open("notes.txt")
remacs.buffer.switch(id)
remacs.buffer.close(id, false) -- protect unsaved edits
```

Buffer IDs are stable, opaque session identifiers. Closing a buffer does not
reassign its ID to a new document. Buffer-list entries include `id`, `path`
(absent for unnamed documents), `dirty`, `row`, `col`, and `revision`.

Cursor rows and columns are **one-based**. Columns count Unicode scalar values,
matching the editor's text model; they do not count UTF-8 bytes or terminal
cells. `get_lines` returns an array of strings without line endings. Optional
`get_lines(id, first, last)` bounds are one-based and inclusive.
`set_lines` replaces the entire buffer; `insert` inserts text at that buffer's
cursor. Text mutations form undoable changes and refresh the document revision,
syntax state and language-server synchronization. Editing an inactive buffer
does not switch focus to it.

Lua reads the editor snapshot for the current callback. Mutations are queued
and applied after the callback returns, in order. Consequently, reading a buffer
immediately after calling `set_lines` in the same callback still sees its
previous contents. Build a local table if you need to inspect intermediate
edits, or observe the resulting `buffer_change` event. Lazy line reads avoid
copying every open document on each keystroke.

## Windows

A window is a view onto a buffer with its own cursor and scrolling. Text,
modified state and undo history belong to the buffer, so two windows can show
the same document without copying it.

```lua
local current = remacs.window.current()
local windows = remacs.window.list()

remacs.window.split()                  -- same buffer, focus new right-hand view
remacs.window.split("notes.txt")       -- open a file in the new view
remacs.window.focus(current)           -- focus a stable window ID
remacs.window.close(current)           -- close one view, retain its buffer
remacs.window.only()                   -- keep only the focused view
remacs.window.resize(5)                -- grow focused view by five columns
remacs.window.resize(-5)               -- shrink focused view by five columns
remacs.window.equalize()               -- restore equal widths
```

These operations use the same deferred action queue as buffer writes. Reading
`window.current()` or `window.list()` immediately after `split()` in one
callback still returns the pre-split snapshot. `close(id)` and `only(id)` accept
an optional stable window ID; omitted IDs mean the current window. Closing an
inactive window preserves focus and the active view's mode and position.
Closing the last window is protected. Window IDs are never reused during the
session.

List entries contain `id`, `buffer`, `active`, `row`, `col`, `scroll_row`,
`scroll_col` and `weight`. Cursor and scrolling coordinates are one-based.
Cursor columns count Unicode scalar values; horizontal scrolling counts terminal
cells, accounting for wide characters and expanded tabs. `weight` is the
relative layout weight, rather than a fixed pixel width. Terminal resizing
preserves views and shows the focused window when space is limited.

The default commands are `window_split`, `window_next`, `window_previous`,
`window_left`, `window_right`, `window_close`, `window_only`, `window_grow`,
`window_shrink` and `window_equalize`. They are bound under `space w` and
`ctrl-w` in Normal mode. Global Emacs-style aliases are `ctrl-x 3`, `ctrl-x o`,
`ctrl-x 0` and `ctrl-x 1`. The command aliases `vsplit` / `vs` accept an optional
file; `close`, `only` and `wincmd` expose the same operations. Existing `q`,
`q!` and `wq` retain their whole-editor behavior. `Ctrl+W` and `Ctrl+X`
window prefixes also work while the file tree has focus.

## Hooks and editor events

```lua
remacs.hooks.add("buffer_saved", "my-plugin.saved", function(event)
  remacs.message("Saved " .. tostring(event.path))
end)

remacs.hooks.remove("buffer_saved", "my-plugin.saved")
```

A hook has an event name, a stable ID and a callback receiving an event table.
Reusing its ID replaces the previous callback. Use a plugin-specific prefix in
IDs so your hooks remain easy to identify. Available editor events are:

| Event | Trigger |
| --- | --- |
| `editor_ready` | Startup configuration has loaded and editing can begin. |
| `editor_exit` | The editor is shutting down. |
| `buffer_open` | A new document is added to the editor. |
| `buffer_close` | A document is removed from the editor. |
| `buffer_enter` | The active buffer changes. |
| `buffer_change` | Document contents change, including undo/redo and Lua edits. |
| `buffer_saved` | A document is successfully written. |
| `cursor_move` | The active cursor moves. |
| `mode_changed` | Normal, Insert or Visual mode changes. |
| `window_change` | Window layout, focused window or buffer assignment changes; includes `id`, `current`, `previous` and `count`. |
| `resize` | Terminal dimensions change; includes `width` and `height`. |

Plugins can also emit their own events with
`remacs.emit("my-plugin.event", { value = 42 })`. Hook failures are reported in
the editor instead of terminating the process.

## Modules, reload and persistent state

Use `require("remacs.tree")`, `require("remacs.toggleterm")`,
`require("remacs.lsp")` and `require("remacs.mason")` to access the current
bundled modules. They are normal Lua modules in the shared package cache.
Mason's catalog is shared with the LSP plugin, so changes do not need to be
copied between independent configurations.

The plugins register `tree`, `terminal` (also `shell`), `lazygit`,
`lsp-definition`, `lsp-hover` and `mason` commands through the public API.
`remacs.plugins.load(name_or_path)` and
`remacs.plugins.reload(name_or_path)` expose the loader to Lua;
`remacs.config.reload()` reloads `init.lua`. For a custom reusable module, put
`my_plugin.lua` under the configuration's `lua/` directory and load it with
`require("my_plugin")` or `remacs.plugins.load("my_plugin")`. Both track module
ownership; `remacs.plugins.reload("my_plugin")` refreshes its current source.
Alternatively, load a file with `remacs.plugins.load(path)`. Bundled module names
such as `tree` are shorthand for `remacs.tree` in the loader API.

Use `remacs.state` for data that should outlive a reload:

```lua
remacs.state.counter = remacs.state.counter or { value = 0 }
local state = remacs.state.counter

remacs.commands.add("count", function()
  state.value = state.value + 1
  remacs.message(tostring(state.value))
end)
```

Module-local variables are recreated when its file reloads. A cached local
reference to another module still points at that old table; use a fresh
`require` when a dependency's current configuration matters. The bundled LSP
plugin follows this rule for Mason. Tree navigation and terminal lifecycle
state live in `remacs.state`, so reloading their implementations preserves
state within the current session.

The loader tracks the commands, maps and hooks registered by each module or
file. Reloading replaces that owner's registrations, so removed hooks and keys
do not accumulate. Registrations belonging to other files remain available.
Reload preserves each owner's priority: user overrides survive a reload of
bundled defaults, and removing a user override on its next reload reveals the
underlying default again.
If loading fails, the previous working registrations remain installed and the
error is displayed. Queued actions from a failed callback are discarded.
Registration rollback does not restore `remacs.state`, arbitrary globals or
external side effects such as writes performed with Lua's `io` library.

Lua instruction and elapsed-time budgets stop runaway Lua loops, and the editor
bounds queued actions and event processing. These limits help contain accidental
recursion; they do not make blocking standard-library calls asynchronous.

Rust adapters resolve the current module when called. They do not retain an old
Lua function after reload. Language-server configuration caches are invalidated
when the runtime generation changes. Reload preserves unaffected language-server
services and stops services whose executable, language or workspace configuration
changed or was disabled; matching documents reconnect using the new catalog.
Rust continues to own the services and existing PTY sessions.

## Architecture and validation

`src/windows.rs` owns window view state and layout; the renderer paints shared
buffer contents into those views. `src/lua_runtime.rs` owns the interpreter and runtime loading, while
`lua/api.lua` implements registrations, dispatch and ownership. The four plugin
adapters convert Lua module results into typed Rust data. `lua/defaults.lua`
contains editor commands and bindings. The editor publishes small metadata
snapshots, provides lazy line reads and applies queued mutations through its
normal editing, undo and synchronization paths. No Lua callback holds a mutable
Rust editor reference across another callback.

Run the project's regular checks after changing the API or bundled plugins:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Runtime tests cover shared module state, command/key/hook dispatch and reload
behavior. Editor tests exercise Lua-driven editing and its integration with
existing buffer, undo and modal keybinding behavior.
