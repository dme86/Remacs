<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/remacs-wordmark-dark.svg">
    <img src="assets/remacs-wordmark-light.svg" alt="Remacs" width="540">
  </picture>
</p>

<h1 align="center">Remacs</h1>
<p align="center"><strong>A little editor with room to grow.</strong></p>

**Remacs** is a tiny terminal text editor written in Rust, inspired by the
keyboard-first editing experience of [MicroEmacs](https://github.com/mittelmark/microemacs).
No GUI, no menus, no daemon, and no configuration required to start editing.
The default keymap is **Vim-style modal editing** with a **Doom-inspired Space leader**.

This is an independent implementation, **not** a port of MicroEmacs' C code.
It includes multiple buffers, a shared and reloadable Lua runtime, persistent
jumps, native clipboard integration and asynchronous LSP navigation.

## Get started

Requires a recent stable Rust toolchain and a terminal on Linux or macOS.

```sh
git clone https://github.com/dme86/Remacs.git
cd Remacs
cargo run -- README.md
# or: cargo install --path . && remacs README.md
```

Open an existing file or pass a new filename to create it on first save.
Start with `remacs` to open an empty unnamed buffer.

## Keyboard shortcuts

Remacs starts in **NORMAL** mode. Press **`i`** to enter **INSERT** mode and
**`Esc`** to go back. Commands below use sequential keys: `SPC f f` means
press Space, then `f`, then `f` — not all three together. The minibuffer
shows incomplete sequences.

| Action | Shortcut |
| --- | --- |
| Insert mode | **`i`**, **`a`**, **`I`**, **`A`**, **`o`**, **`O`** |
| Normal mode | **`Esc`** |
| Move | **`h` / `j` / `k` / `l`**, arrows, **`w` / `b`**, optional count (e.g. **`5j`**, **`12k`**) |
| Beginning / end of line | **`0` / `$`** |
| First / last line | **gg / G**, absolute jump via **42G** or **42gg** |
| Go to line number | **`:42`**, then **Enter** (first nonblank on line 42) |
| Scroll down / up half a page | **Ctrl+D / Ctrl+U** (Normal or Visual) |
| Scroll forward / back a page | **Ctrl+F / Ctrl+B** (Normal or Visual), **PageDown / PageUp** (Normal, Insert or Visual) |
| Jump backwards / forwards | **Ctrl+O / Ctrl+I** or **Alt+Left / Alt+Right** |
| Previous / next edit (global) | **g; / g,** |
| Set local / global mark | **ma / mA** |
| Jump to mark | **'a** (line) / **`a** (exact column), uppercase for cross-file |
| Select function (Tree-sitter) | **`vaf`** or **`SPC v f`** |
| LSP definition and hover (rust-analyzer) | **`gd`** and **`K`** |
| Code completion (Insert mode) | Automatic, or **Ctrl+Space**; **↑/↓** choose, **Tab** accept |
| Language-server manager | **`:mason`** or **`SPC l m`** |
| Git plugin manager | **`:plugins`** |
| Visual selection | **`v`**, then **`hjkl`**, **`w`**, **`b`**; **`Esc`** cancels |
| Yank / delete selection | **`y` / `d`** in VISUAL mode (also copies to OS clipboard) |
| Delete character / line | **`x` / `dd`** |
| Yank line / paste | **`yy` / `p`** (system clipboard shared with other apps) |
| Undo / redo | **`u` / `Ctrl+R`** |
| Open file in a buffer | **`SPC f f`** (or select in tree) |
| Switch buffers (searchable picker) | **`Ctrl+P`** or **`SPC b b`** |
| Next / previous buffer | **`Alt+j`** / **`Alt+k`**, or **`Ctrl+PageDown`** / **`Ctrl+PageUp`**; `SPC b n/p` still work |
| Toggle last buffer | **`SPC b l`** |
| Close current buffer (only if saved) | **`SPC b d`** |
| Split vertically (side by side) | **`SPC w v`**, **`Ctrl+W v`** or **`Ctrl+X 3`** |
| Focus left / right window | **`SPC w h` / `SPC w l`**, or **`Ctrl+W h` / `Ctrl+W l`** |
| Next / previous window | **Tab** cycles forward when the tree is hidden; **`SPC w w` / `SPC w W`**, **`Ctrl+W w` / `Ctrl+W W`**; **`Ctrl+X o`** cycles |
| Close window / keep only this window | **`SPC w c` / `SPC w o`**, **`Ctrl+W c` / `Ctrl+W o`**, or **`Ctrl+X 0` / `Ctrl+X 1`** |
| Grow / shrink window; equal widths | **`SPC w >` / `SPC w <`**; **`SPC w =`** (also **`Ctrl+W`** variants) |
| Toggle project file tree | **`SPC o p`**, **`SPC f t`** or **`Ctrl+N`** |
| Floating LazyGit (Lua toggleterm plugin) | **`SPC g`** |
| Floating project shell | **`Ctrl+T`** or **`SPC t`** |
| Save current buffer | **`SPC f s`** |
| Save all modified named buffers | **`SPC b s`** or **`:wa`** / **`:wall`** |
| Quit (prompts if modified) | **`SPC q q`** |
| Command minibuffer | **`:`** then a line number, `w`, `q`, `q!`, `wq`, `e FILE`, `w FILE`, `buffers`, `bn`, `bp`, `b#`, `bd`, `bd!`, `wa` or `wall` |
| Legacy Emacs-style file shortcuts | **`Ctrl+X Ctrl+F`**, **`Ctrl+X Ctrl+S`**, **`Ctrl+X Ctrl+C`** |

The editor supports multiple independent file buffers, a status line, a minibuffer,
UTF-8 text editing, basic horizontal/vertical scrolling, and CRLF/LF preservation.
This is a **small subset** of Vim/Evil, not full compatibility with Doom Emacs.

Line numbers in `:NUM` are one-based: `:0` goes to the first line and a number
past the end goes to the last. Scrolling moves the viewport and cursor together,
keeping the cursor's screen row until a file boundary. Half-page scrolling uses
half the visible text height; in Normal mode, a count such as `3 Ctrl+D` moves
exactly three lines for that command only. Full pages leave two context lines (where space
allows); `3 Ctrl+F` moves three pages. Counts are consumed by the scroll and do
not affect the next motion. `Ctrl+V` / `Alt+V` remain full-page aliases, while
`Ctrl+PageDown` / `Ctrl+PageUp` still switch buffers in Normal mode. Scrolling
in Visual mode extends the selection. Return to a line-jump or scroll origin
with `Ctrl+O` in Normal mode. Prompts, search fields, the file tree and floating
terminals retain their own key handling.

## System clipboard

Remacs shares text with the **desktop clipboard** on macOS and Linux (Wayland/X11).
`yy` and Visual `y` copy directly to the OS clipboard; `dd`, `x`, and Visual `d`
copy the deleted text as well. Normal-mode `p` reads the current system
clipboard, so you can copy text in a browser or another app and paste it into
Remacs. If the clipboard is inaccessible (for example, on a headless machine),
the internal register continues to work. The existing `yy` → `p` workflow
retains its **linewise** behavior when the clipboard has not been replaced.
An empty clipboard or one containing only an image inserts nothing; it never
replays an older yank. The internal register is used when clipboard access fails.

Terminal paste (e.g. **Cmd+V** in macOS terminals, or **Ctrl+Shift+V** on
Linux) uses bracketed paste. Multi-line UTF-8 text is inserted literally,
without triggering Normal-mode commands, and is one undoable edit when in
Insert mode, with separate undo steps for text typed before and after the paste.
In file/command prompts and all search pickers, paste inserts the first line
as literal input; press Enter yourself to submit it. Clipboard access is lazy;
no GUI clipboard connection is needed
until the first copy or `p`. Linux Wayland support uses the compositor's
`ext-data-control-v1` or `wlr-data-control` protocol (or XWayland fallback).

### macOS

Remacs runs directly in Terminal.app, iTerm2, Alacritty, or `rst` on macOS.
With a recent Rust toolchain installed:

```sh
cargo install --path .
remacs ~/Documents/notes.txt
```

From an open editor, use `SPC f f` to enter a filename, or `:e path/to/file`.
Opening another file does **not** require saving the current one: unsaved changes
stay in their own buffer. Start an empty document with `remacs` and save it
via `SPC f s`.

## Lua configuration and plugins

Commands, default keymaps, hooks and plugins run in **one persistent Lua 5.4
runtime**. Rust provides fast text operations, rendering, I/O, PTY sessions and
asynchronous LSP services. The tree, toggleterm, LSP and Mason plugins share the
same public API and module cache.

Try a command immediately with `:lua remacs.message("Hello")`, load a plugin with
`:luafile /path/to/plugin.lua`, then edit it and use `:reload /path/to/plugin.lua`.
`:reload tree`, `:reload toggleterm`, `:reload lsp` and `:reload mason` update the
bundled modules without rebuilding or restarting. `:reload` reloads your user
configuration. Registered Lua commands are available directly from `:`.

User configuration lives in `~/.config/remacs/init.lua` (or
`$XDG_CONFIG_HOME/remacs/init.lua`; `$REMACS_CONFIG_DIR` overrides the directory).
Place modified bundled plugin files in that directory's `plugins/` folder,
such as `~/.config/remacs/plugins/mason.lua`, then reload the module.

The [Lua API guide](docs/lua-api.md) documents configuration discovery, commands,
multi-key mappings, buffers, editor events and reload behavior.
[examples/init.lua](examples/init.lua) is a usable configuration example;
[lua/defaults.lua](lua/defaults.lua) contains the existing default editor policy.
Embedded Lua files remain available as installation fallbacks.

## Git plugin manager

Open **`:plugins`** to search and manage external Remacs Lua plugins. Git must
be available on PATH. To add one, replace this example's placeholder URL with
a repository you trust:

```text
:plugins add notes https://example.com/you/remacs-notes.git
```

**`add` registers and installs the plugin explicitly.** Optional arguments select
the branch and repository-relative entry file:
`:plugins add NAME REPO [BRANCH] [ENTRY]` (defaults: `main`, `plugin.lua`).
Installations and updates run in the background and become active **after
restarting Remacs**. They do not reload the running plugin.

| Action | Command / popup keys |
| --- | --- |
| Open searchable manager | **`:plugins`** |
| Install a registered plugin | **`:plugins install NAME`** or **Enter** on its row |
| Install missing registered plugins | **`:plugins sync`** or **Ctrl+S** |
| Update one installed plugin | **`:plugins update NAME`** or **Ctrl+U** on its row |
| Update all installed plugins | **`:plugins update`** |
| Check remote branch hashes | **`:plugins check`** or **Ctrl+R** |
| Remove a registration | **`:plugins remove NAME`** |

Type to filter; arrows or Tab select a row, **Ctrl+L** clears the filter, and
**Esc** closes the popup. Update availability appears as a badge and a
notification. Remacs also checks remote branch hashes asynchronously at startup;
checks do not install updates. `update` without a name updates all installed
plugins regardless of a previous check result. `sync` installs only missing
plugins. Removal takes effect after restart and retains cached snapshots.

Installed code is stored as immutable Git commit snapshots with a manifest:

- macOS: `~/Library/Application Support/Remacs/plugins`.
- Linux: `${XDG_DATA_HOME:-~/.local/share}/remacs/plugins`.
- Set **`REMACS_PLUGIN_HOME`** to an absolute directory to override that location.

At startup, installed entries load alphabetically in the shared Lua interpreter,
after bundled plugins/defaults and before `init.lua`, so user configuration can
override them. A repository's `plugin.lua` must use the **Remacs Lua API**;
Neovim plugins are not automatically compatible. Namespaced helper modules can
live under `lua/`, for example `lua/notes/helpers.lua` loaded with
`require("notes.helpers")`. Configuration-directory Lua modules have priority.
This version does not manage plugin dependencies or build hooks. See the
[managed plugin API and repository layout](docs/lua-api.md#managed-git-plugins)
for authoring details.

## Mason-like language-server manager (Lua plugin)

Remacs includes its own **`plugins/mason.lua`** for declaring the languages and
language servers you want available. It is inspired by the Mason ecosystem
in Neovim, but **is not Mason.nvim or its full package registry**. The same
Lua catalog is consumed by `plugins/lsp.lua`: enabling a language makes
the LSP client recognize it, and the manager knows how to install it.

The initial desired list is **Rust, Lua, Go, Bash, JSON and YAML**. Python,
TypeScript/JavaScript and TOML recipes are included but commented out. Edit
`ensure_installed` in your runtime copy of `plugins/mason.lua`, then use
`:reload mason` to change it.
New languages can be added with an executable command, file extensions, root
markers and a typed installation recipe.

| Action | Command / keys |
| --- | --- |
| Open searchable server manager | **`:mason`** or **`SPC l m`** |
| Install highlighted server | **Enter** in the manager |
| Update highlighted server | **Ctrl+U** in the manager |
| Install all missing desired servers | **`:mason sync`** or **Ctrl+S** |
| Update all desired servers | **`:mason update`** or **Ctrl+R** |
| Install one server | **`:mason install gopls`** |
| Update one server | **`:mason update gopls`** |
| View installer log location | **`:mason log`** |

The popup is the same fuzzy UI used for files and buffers. Server entries
show whether they're installed locally, found on the system PATH, missing,
queued, installing or failed. Install/update jobs run **asynchronously and
one at a time**, so editing remains responsive. After installing a server,
Remacs retries a previously unavailable LSP without requiring a restart.

**No surprise downloads:** `auto_install = false` and
`auto_update_days = 0` are the defaults. The single explicit
`:mason sync` command installs missing enabled tools. Optionally set
`auto_install = true` or a positive `auto_update_days = 7` in the Lua file
to enable startup installation or periodic refresh (based on
successful managed installs/updates). Automatic updates only cover entries
with a recorded successful installation/update; nothing checks network
versions in the background otherwise. Updates may fetch latest package
versions and execute package-manager install scripts.

**Storage:** Go, npm and Cargo servers install into Remacs' private directory
(`~/Library/Application Support/Remacs/mason` on macOS or
`${XDG_DATA_HOME:-~/.local/share}/remacs/mason` on Linux). The LSP client
uses managed executables in preference to the system PATH.

Rust uses **`rustup component add rust-analyzer rust-src`** rather than
bundling a second compiler. LuaLS uses **Homebrew** (`brew install
lua-language-server`), which changes the regular Homebrew environment
rather than the private Remacs directory; on Linux, install LuaLS via your
system package manager if `brew` is unavailable. Running
`:mason update` on Rust ensures its components are present in the current
toolchain; **`rustup update` remains a separate, explicit operation**.
Other recipes use local `go install`, `npm install --global --prefix`, or
`cargo install --root`. The required package-manager binary must already be
available on PATH. No arbitrary shell command strings are evaluated.

This first version manages **LSP servers**, not DAP adapters, linters or
formatters, and does not implement Mason.nvim's online registry or version
pinning. Installation logs are kept at `mason/install.log`. The manager's
local package list is intentionally small and auditable.

## Language server support (Lua plugin)

Remacs ships **`plugins/lsp.lua`** for LSP navigation shortcuts; the
desired servers and install recipes are configured in **`plugins/mason.lua`**.
The Rust host **`src/lsp.rs`** handles asynchronous LSP 3.17 JSON-RPC
over stdio; it starts **one server per language and workspace only when a
matching file is opened**, never one server per buffer.

Servers enabled in the Lua catalog provide navigation and completion; Rust
files use **rust-analyzer**:

| Action | Shortcut |
| --- | --- |
| Go to definition | **`gd`** in Normal mode |
| Hover documentation | **`K`** in Normal mode; Esc dismisses |
| Return from definition | **`Ctrl+O`** (existing global jumplist) |
| Request completion | **Ctrl+Space** in Insert mode |
| Choose / accept completion | **↑/↓** or **Shift+Tab** / **Tab** |
| Dismiss completion | **Ctrl+G** stays in Insert; **Esc** returns to Normal |

Install rust-analyzer using your Rust toolchain:

```sh
rustup component add rust-analyzer rust-src
```

Open a Rust project and run `remacs src/main.rs`. The server starts in the
background, initializes, opens the current file and processes changes without
blocking the editor. `gd` works across files and reuses already-open buffers.
`K` displays Markdown text in the existing centered Remacs popup.

Completion suggestions appear in a small dropdown at the cursor after a
150 ms typing pause: after two identifier characters, a dot, or `::`.
**Ctrl+Space** explicitly requests suggestions at any position. **Tab** accepts
the highlighted entry; without a visible menu it still inserts a tab.
**Enter always inserts a newline**, so ordinary typing cannot accidentally
accept a suggestion. Up/Down and Shift+Tab navigate the list.

The editor synchronizes the current document before requesting suggestions
and discards results after edits, cursor movements, dismissal or buffer
switches. Server-provided replacement ranges and additional edits, such as
imports, are applied together as **one undoable change**, using the negotiated
Unicode position encoding. Invalid or overlapping edits leave the document
unchanged. Suggestions never block typing; rapid input cancels earlier work.
While a server is still indexing, Ctrl+Space can request a fresh list.

This first version supports plain-text completion and handles common snippet
placeholders defensively, but does not advertise full snippet support or offer
linked placeholder navigation, item-resolution requests or completion commands.

**Memory/CPU design:** One rust-analyzer process is shared by all open Rust
buffers within a workspace. Remacs uses document revision numbers so ordinary
cursor movement never sends text to the server. Changes are sent as minimal
Unicode-safe edits with the server's negotiated position encoding (UTF-16 by
default); no LSP server is started for files outside the configured languages.
After closing a buffer,
Remacs informs the language server. External tools such as rust-analyzer are
not bundled with Remacs.

Repeated navigation requests are deduplicated; superseded requests are canceled
and outstanding requests have a timeout and a fixed queue limit. Failed or
unresponsive initialization is reported instead of staying stuck at startup.
Workspace configuration is cached, so idle ticks and cursor motions do not
repeat filesystem root discovery. Mason updates invalidate the cache.

**Roadmap:** LSP Core covers definition, hover, completion and synchronization.
Diagnostics and refactoring actions are follow-up milestones. Language servers and
keybindings remain configured in Lua; add more languages to your runtime copy of
`plugins/mason.lua` and use `:reload mason`. GitHub Actions exercises a real rust-analyzer
server on both Linux and macOS in addition to protocol unit tests.

## Floating terminal & LazyGit (Lua plugin)

Remacs bundles **`plugins/toggleterm.lua`**, inspired by the configuration
in `dme86/neovim/lua/plugins/toggleterm.lua`. Unlike a shell command that
takes over the whole screen, it opens a centered popup with a real interactive
PTY, an ANSI/VT100 renderer and a curved border.

- **`SPC g`** opens **LazyGit** in the current project's Git root.
- **`Ctrl+T`** (or **`SPC t`**) opens your `$SHELL` in the project root.
- **`Ctrl+T` while a popup is active** hides it and returns to the editor.
  The process continues in the background; reopening resumes its state.
- Inside the popup, keys like arrows, Enter, Escape and Ctrl combinations
  are sent directly to the child program. `q` exits LazyGit normally.

The Lua plugin **owns the bindings, command selection, titles and lifecycle**.
Its adapter uses the shared runtime; Rust supplies PTY sessions, keyboard input
and drawing. Modify your runtime copy of `plugins/toggleterm.lua`, then use
`:reload toggleterm` to customize it. Existing session state survives reload.

LazyGit is an **optional external program**, not bundled with Remacs. On macOS,
for example: `brew install lazygit`. If it is missing, Remacs shows an error
and continues editing normally.

Each popup session keeps its own working directory and stays alive when hidden.
Terminal output is buffered in a bounded channel; Rust repaints only changed
popup lines and restores the editor after the popup closes. The terminal
is deliberately separate from Remacs' file buffers and doesn't alter or
automatically save modified documents. Restarting Remacs ends its PTY sessions.

## Persistent jumps and named marks

Remacs follows Vim's distinction between a **jumplist** (places you jumped
from) and a **changelist** (places you edited). Both histories are global
across buffers in Remacs. Edit one file, switch to another, and return to
either location with just a shortcut.

| Action | Keys |
| --- | --- |
| Older / newer jump | **Ctrl+O / Ctrl+I**, or **Alt+Left / Alt+Right** |
| Older / newer edit | **g; / g,** |
| Set local / global mark | **ma / mA** (lowercase scoped to file, uppercase global) |
| Jump to exact marked position | **`a / `A** |
| Jump to first nonblank character on marked line | **'a / 'A** |

**Example:** edit main.rs, switch to document.rs and edit again. Repeating
**g;** visits the most recent edit locations even across files. **Ctrl+O**
moves back through major navigation positions, **Alt+Right** moves forward.
When a target file is closed, Remacs automatically reopens it. Missing or
unreadable files are skipped when traversing the jumplist. If a file is
already open, Remacs reuses that exact buffer, including unsaved edits.

Locations persist as a small JSON file:

- macOS: **~/Library/Application Support/Remacs/jumps.json**
- Linux: **${XDG_STATE_HOME:-~/.local/state}/remacs/jumps.json**

Only absolute file paths, line/column coordinates, and named marks are
saved—not unsaved text, document buffers, or undo snapshots. The history
holds at most **200 jumps and 200 edits**; repeated changes on the same
line are collapsed. Distinct jump columns on the same line remain separate,
and new jumps after going backward discard the previous forward branch.
Traversing existing history does not rewrite the history file.
Removed files do not cause placeholder files to be
created.

**Difference from Vim:** Vim's changelist is traditionally per buffer and
its jumplist per window; Remacs deliberately uses a global edit history.
**Tab changes focus and never traverses jump history.** In legacy PTY mode,
Ctrl+I and Tab send the same character, so Ctrl+I also performs Tab's focus
action there. An explicitly reported **Ctrl+I** and **Alt+Right** (Option+Right
in rst) remain forward-jump shortcuts. Use Alt+Right when your terminal cannot
distinguish Ctrl+I from Tab. Like Vim's simple location marks, these
coordinates cannot yet follow arbitrary insertions after a file has
been closed and reopened. Concurrent processes can overwrite one another's
history; this is a first lightweight persistent version.

## Multiple file buffers

Every opened document has its **own editable text, unsaved/dirty flag,
cursor, scroll position, and undo/redo history**. Opening another file
from the tree, the fuzzy finder, `SPC f f`, or `:e FILE` leaves the old
buffer intact. Opening a file already in the buffer list switches back to
that same buffer rather than rereading the file and losing edits.

**`Ctrl+P`** opens the same centered, fuzzy-searchable buffer popup as **`SPC b b`**.
The previously used buffer is preselected, so **`Ctrl+P`, `Enter`** quickly
switches back without typing. Press `Ctrl+P` again or `Esc` to dismiss it.
Start typing
to filter by name/path, use Up/Down (or Tab/Shift+Tab), and Enter to switch.
A `●` denotes unsaved edits and `▶` marks the active buffer.
**`Alt+j`** and **`Alt+k`** cycle through buffers directly in Normal mode;
**`Ctrl+PageDown`** and **`Ctrl+PageUp`** provide an alternative if your
terminal does not transmit Alt/Meta combinations. The original
**`SPC b n`** and **`SPC b p`** also work. These direct shortcuts work even
with the tree focused, returning focus to the editor after switching.
On macOS, your terminal may need **Option as Meta** enabled for `Alt+j/k`.
**`SPC b l`** toggles between the two most recently active buffers.
The footer shows a compact **`B 2/4  ●2  SPC b s`** when you're in
buffer 2 of 4 and two buffers have unsaved edits. The `●` count includes
inactive buffers; when none are dirty it shows `●0` and hides the save hint.
These counters are kept ahead of the filename so they remain visible
even in narrower terminals.

Use **`SPC b s`** in Normal mode (or **`:wa`** / **`:wall`**) to save
**all modified, named buffers**, without switching files or losing their
cursor, scroll, or undo state. If one write fails, Remacs continues saving
the others and reports the failure; the affected buffer remains dirty.
Unnamed unsaved buffers are skipped safely and reported so you can name
and save them explicitly with `:w FILE`.

**`SPC b d`** or `:bd` closes a clean buffer; a dirty buffer is protected
until saved, unless you explicitly use `:bd!` to discard it.
Quit checks **all** open buffers, including inactive ones, and prompts before
discarding unsaved changes. `:q!` still exits without saving.

Buffers are kept **in memory only**, with no session persistence or
automatic disk swap. Switching uses Rust ownership moves (no document clone);
the bundled syntax grammars and theme are loaded once and shared by all
buffers. Undo histories still use snapshots, but the editor now limits retained
history to **100 steps or roughly 32 MiB per buffer**, whichever is smaller.
For very large files, at least one undo snapshot is preserved even if it
exceeds the limit. Incremental/delta-based undo is planned later.

## Vertical split windows

Use **`SPC w v`**, **`Ctrl+W v`** or **`Ctrl+X 3`** to split the current
window into two side-by-side views. The new window starts with the same buffer
and takes focus. Use **`SPC w h/l`** to move left/right, **`SPC w w`** to cycle
forward, or **`SPC w W`** to cycle backward. The active window is marked in its
status line. Existing file-open and buffer-picker commands change the buffer
in the focused window.

In Normal mode, **Tab** focuses the visible file tree; Tab there returns to
the last active window. With the tree hidden, including after closing it,
Tab cycles forward through two or more windows and wraps from last to first.
With one window and no visible tree, Tab leaves the view unchanged. These
focus actions never follow jump history. Insert/Visual mode, prompts, pickers
and terminal popups retain their own input handling.

Each window keeps its own cursor and scroll position. Two windows displaying
the same buffer share text, dirty state and undo history, so edits appear in
both without creating duplicate files or conflicting copies. Several vertical
windows can be opened. Small terminal sizes temporarily show fewer views while
keeping the window layout and buffers available.

**`SPC w >` / `SPC w <`** grow or shrink the focused window by five columns;
a Normal-mode count such as **`10 SPC w >`** changes ten columns. **`SPC w =`**
restores equal widths. **`SPC w c`** closes the current view, and **`SPC w o`**
keeps only that view. Closing a window retains its buffer, including unsaved
edits; closing the final window is protected.

The command minibuffer accepts **`:vsplit`** / **`:vs`**, optionally followed
by a filename, plus **`:close`**, **`:only`** and **`:wincmd h|l|w|W|v|c|o|>|<|=`**.
The existing **`:q`**, **`:q!`**, **`:wq`** and **`SPC q q`** still operate on
the whole editor and retain their buffer-protection behavior. Prompts and active
terminal popups retain their own input handling.

Window commands and all default bindings live in Lua and can be replaced in
`init.lua`. The [window API](docs/lua-api.md#windows) exposes stable window IDs,
layout operations and window-change hooks to plugins.

## Line numbers and cursor line

**Enabled by default:** a narrow left gutter displays *hybrid relative line numbers*:
the active line shows its absolute line number, while other lines show their
distance from it. For example, a row labeled `7` below the cursor can be
reached with `7j`, or use `7k` for a row seven lines above. Enter
`245G` or `245gg` to jump to absolute line 245.

The current line has a subtle full-width background tint, including empty
space to the right of the text. Both features are integrated with the
incremental renderer so moving between lines does not clear the screen.

## File tree (Lua plugin)

Open the project explorer using **SPC o p** (Space, o, p), **SPC f t**,
**Ctrl+N**, or **:tree**. The tree appears in a narrow left sidebar and
starts at the working directory you launched Remacs from. You can navigate it
with j/k or arrow keys, expand and collapse directories using l/h, and open
a selected file with Enter. Tab or Esc returns focus to the editor; q
closes the sidebar. Press . to show/hide dotfiles, r to refresh, and g/G
to jump to the first/last entry. Opening a different file preserves
unsaved changes in the original buffer.

The tree is implemented in **plugins/tree.lua** and executed by the shared
Lua 5.4 runtime (mlua). Rust provides filesystem metadata (`remacs.fs.list`),
paints the sidebar, and processes file-open actions. `:reload tree` loads changes
to its runtime file while preserving the project's navigation state.

### Fuzzy file finding in the tree

Focus the file tree, then press **`/`** (or **`f`**) to open a **centered file
finder overlay** over the editor. Matches are visible immediately, even
before typing. Type a filename or any subsequence, for example `rdme` to
find `README.md`, or `rndr` to find `src/render_engine.rs`.
It searches the entire project including collapsed folders and updates the
ranked matches live. The overlay leaves the file tree open in the background.

The finder is currently a built-in, fzf-*style* Lua fuzzy matcher, **not** an
external `fzf` subprocess and does not require `fzf` to be installed.
If desired, an optional native fzf backend can be added later.

In search mode, use **Up/Down** or **Tab/Shift+Tab** to choose a match,
**Enter** to open it, **Backspace** to edit the query, **Ctrl+U** to clear it,
and **Esc** to cancel the search while keeping the tree open. You can type
`j` and `k` as part of the search query; they are ordinary letters *during*
search mode. Outside search mode, `j/k` remain navigation keys.

The project file list is scanned **once when search starts**, cached while
typing, and limited to 20,000 files / 32 levels to avoid uncontrolled
recursion. Generated/dependency directories such as `.git`, `node_modules`
and `target` are skipped in searches. Hidden files follow the tree's
dotfile visibility setting (`.` toggles it *outside* search mode). The
finder is synchronous in this first iteration; very large project trees may
cause a brief pause on the initial scan.

The sidebar is hidden by default. The editor automatically adjusts the
gutter, horizontal scrolling and cursor coordinates when the tree is shown.

## Syntax-aware function selection

Move your cursor **anywhere inside a function**, press **`vaf`** (Visual,
around function) or **`SPC v f`** (Space, v, f). Remacs highlights the whole
function, from its declaration through its closing delimiter, even with
nested braces or multiline definitions. The selection is real: press
**`y`** to yank it into Remacs' internal register, **`d`** to delete it
(undo with **`u`**), or **`Esc`** to cancel. You can also create a
regular Visual selection with **`v`** and cursor motions.

Function boundaries are resolved on demand using [Tree-sitter](https://tree-sitter.github.io/),
not regex matching or brace counting. The first implementation supports
Rust, Python, Go, JavaScript/JSX, TypeScript/TSX, C, C++, Java, Bash,
Lua, Ruby, PHP, C#, and Swift. The innermost surrounding function is
preferred, including methods and common closures. There is no LSP
requirement, and parsing runs **only when selecting a function**.

Unsupported file types (including Markdown itself) report a message rather
than guessing. Embedded languages in Markdown code fences and very large
files (above 12 MiB) are not yet supported for function selection; ordinary
syntax highlighting is unaffected.

## Syntax highlighting

**Enabled by default**, with no plugin installation or configuration. Remacs uses
[Syntect](https://github.com/trishume/syntect) plus the extended language
grammars from [two-face](https://docs.rs/two-face/latest/two_face/) (derived
from bat). This covers Markdown (including fenced code), Rust, C/C++, Python,
Go, JavaScript/TypeScript, Java, Kotlin, Swift, Lua, Shell, Fish, YAML, TOML,
JSON, Dockerfile, Terraform, Nix, SQL, HTML/CSS and many more. The active
language appears in the status line; unknown files use plain text.

Colors use 24-bit ANSI foregrounds with the embedded `base16-ocean.dark`
theme. The terminal's existing background remains unchanged. Use
`:syntax off` and `:syntax on` to toggle highlighting within a session.

Parsing is cached incrementally across lines; editing invalidates the affected
line and subsequent parser state. No external syntax files or language servers
are required. Syntax highlighting is lexical, not semantic LSP highlighting.

The embedded grammars have their own third-party licenses; see
[two-face's acknowledgments](https://docs.rs/two-face/latest/two_face/acknowledgement/index.html).

## Scope

**Included in v0.1:** opening, editing, and saving text; Vim-style Normal/Insert
modes, basic motions, line yank/delete/paste, snapshot-based undo/redo,
Doom-inspired leader shortcuts, hybrid relative line numbers, current-line highlighting,
built-in syntax highlighting, multiple buffers, vertical split windows, persistent jumps and marks,
native clipboard integration, asynchronous LSP definition/hover/completion, a Lua server
manager, a file finder, floating terminals, status line, and minibuffer.

**Not yet included:** in-buffer text search, LSP diagnostics/refactoring,
tabs, horizontal splits, mouse support, or menus. This release is not
intended to replace a full-featured editor yet. Undo currently keeps up to 100 in-memory whole-document snapshots;
some complex Unicode grapheme clusters and control sequences need more work.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

See [MicroEmacs](https://github.com/mittelmark/microemacs) for the historical
inspiration. Remacs does not import MicroEmacs source code.

MIT license.
