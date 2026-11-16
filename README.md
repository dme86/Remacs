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
The first milestone focuses on the essentials; Lua plugins and LSP are future goals.

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
| Delete character / line | **`x` / `dd`** |
| Yank line / paste | **`yy` / `p`** |
| Undo / redo | **`u` / `Ctrl+R`** |
| Open file | **`SPC f f`** |
| Toggle project file tree | **`SPC o p`**, **`SPC f t`** or **`Ctrl+N`** |
| Save file | **`SPC f s`** |
| Quit (prompts if modified) | **`SPC q q`** |
| Command minibuffer | **`:`** then `w`, `q`, `q!`, `wq`, `e FILE`, or `w FILE` |
| Legacy Emacs-style file shortcuts | **`Ctrl+X Ctrl+F`**, **`Ctrl+X Ctrl+S`**, **`Ctrl+X Ctrl+C`** |

The editor has a single file buffer, a status line, a minibuffer, UTF-8 text
editing, basic horizontal/vertical scrolling, and CRLF/LF preservation.
This is a **small subset** of Vim/Evil, not full compatibility with Doom Emacs.

### macOS

Remacs runs directly in Terminal.app, iTerm2, Alacritty, or `rst` on macOS.
With a recent Rust toolchain installed:

```sh
cargo install --path .
remacs ~/Documents/notes.txt
```

From an open editor, use `SPC f f` to enter a filename, or `:e path/to/file`.
Opening another file requires saving or discarding your current unsaved changes
first. Start an empty document with `remacs` and save it via `SPC f s`.

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
to jump to the first/last entry. Unsaved editor changes block opening
a different file so nothing is lost.

The tree is implemented in **plugins/tree.lua** and executed by the
bundled Lua 5.4 runtime (mlua). Rust provides a small host API
(remacs.fs.list), paints the sidebar, and processes file-open actions.
The plugin source is embedded in the executable so it works after cargo
install without copying any plugin files. This is the first plugin host
prototype, not yet a general-purpose plugin manager or configuration loader.

The sidebar is hidden by default. The editor automatically adjusts the
gutter, horizontal scrolling and cursor coordinates when the tree is shown.

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