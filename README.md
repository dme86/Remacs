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
The first milestone focuses on the essentials; the Lua file-tree plugin is bundled and LSP is a future goal.

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
| First / last line | **`gg` / `G`**, absolute jump via **`42G`** or **`42gg`** |
| Select function (Tree-sitter) | **`vaf`** or **`SPC v f`** |
| Visual selection | **`v`**, then **`hjkl`**, **`w`**, **`b`**; **`Esc`** cancels |
| Yank / delete selection | **`y` / `d`** in VISUAL mode |
| Delete character / line | **`x` / `dd`** |
| Yank line / paste | **`yy` / `p`** |
| Undo / redo | **`u` / `Ctrl+R`** |
| Open file in a buffer | **`SPC f f`** (or select in tree) |
| Switch buffers (searchable picker) | **`SPC b b`** |
| Next / previous buffer | **`SPC b n`** / **`SPC b p`** |
| Toggle last buffer | **`SPC b l`** |
| Close current buffer (only if saved) | **`SPC b d`** |
| Toggle project file tree | **`SPC o p`**, **`SPC f t`** or **`Ctrl+N`** |
| Save file | **`SPC f s`** |
| Quit (prompts if modified) | **`SPC q q`** |
| Command minibuffer | **`:`** then `w`, `q`, `q!`, `wq`, `e FILE`, `w FILE`, `buffers`, `bn`, `bp`, `b#`, `bd` or `bd!` |
| Legacy Emacs-style file shortcuts | **`Ctrl+X Ctrl+F`**, **`Ctrl+X Ctrl+S`**, **`Ctrl+X Ctrl+C`** |

The editor supports multiple independent file buffers, a status line, a minibuffer,
UTF-8 text editing, basic horizontal/vertical scrolling, and CRLF/LF preservation.
This is a **small subset** of Vim/Evil, not full compatibility with Doom Emacs.

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

## Multiple file buffers

Every opened document has its **own editable text, unsaved/dirty flag,
cursor, scroll position, and undo/redo history**. Opening another file
from the tree, the fuzzy finder, `SPC f f`, or `:e FILE` leaves the old
buffer intact. Opening a file already in the buffer list switches back to
that same buffer rather than rereading the file and losing edits.

**`SPC b b`** opens a searchable popup listing all buffers. Start typing
to filter by name/path, use Up/Down (or Tab/Shift+Tab), and Enter to switch.
A `●` denotes unsaved edits and `▶` marks the active buffer.
**`SPC b n`** and **`SPC b p`** cycle through buffers;
**`SPC b l`** toggles between the two most recently active buffers.
The status line shows the current buffer index, total count, and modified count.

**`SPC b d`** or `:bd` closes a clean buffer; a dirty buffer is protected
until saved, unless you explicitly use `:bd!` to discard it.
Quit checks **all** open buffers, including inactive ones, and prompts before
discarding unsaved changes. `:q!` still exits without saving.

Buffers are kept **in memory only**, with no session persistence or
automatic disk swap. Switching uses Rust ownership moves (no document clone);
the bundled syntax grammars and theme are loaded once and shared by all
buffers. Individual undo histories still use the current snapshot-based
implementation, so very large edited files can consume additional memory.
Incremental/delta-based undo is planned for a later iteration.

## Line numbers and cursor line

**Enabled by default:** a narrow left gutter displays *hybrid relative line numbers*:
the active line shows its absolute line number, while other lines show their
distance from it. For example, a row labeled `7` below the cursor can be
reached with `7j`, or use `7k` for a row seven lines above. Enter
`245G` or `245gg` to jump to absolute line 245.

The current line has a subtle full-width background tint, including empty
space to the right of the text. Both features are integrated with the
incremental renderer so moving between lines does not clear the screen.

## File tree — first Lua plugin

Open the project explorer using **SPC o p** (Space, o, p), **SPC f t**,
**Ctrl+N**, or **:tree**. The tree appears in a narrow left sidebar and
starts at the working directory you launched Remacs from. You can navigate it
with j/k or arrow keys, expand and collapse directories using l/h, and open
a selected file with Enter. Tab or Esc returns focus to the editor; q
closes the sidebar. Press . to show/hide dotfiles, r to refresh, and g/G
to jump to the first/last entry. Opening a different file preserves
unsaved changes in the original buffer.

The tree is implemented in **plugins/tree.lua** and executed by the
bundled Lua 5.4 runtime (mlua). Rust provides a small host API
(remacs.fs.list), paints the sidebar, and processes file-open actions.
The plugin source is embedded in the executable so it works after cargo
install without copying any plugin files. This is the first plugin host
prototype, not yet a general-purpose plugin manager or configuration loader.

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
built-in syntax highlighting, a bundled Lua file-tree plugin, status line, and minibuffer.

**Not yet included:** LSP, search,
multiple buffers, tabs, splits, mouse support, or menus. This release is not
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