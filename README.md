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
| Move | **`h` / `j` / `k` / `l`**, arrows, **`w` / `b`** |
| Beginning / end of line | **`0` / `$`** |
| First / last line | **`gg` / `G`** |
| Delete character / line | **`x` / `dd`** |
| Yank line / paste | **`yy` / `p`** |
| Undo / redo | **`u` / `Ctrl+R`** |
| Open file | **`SPC f f`** |
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

## Scope

**Included in v0.1:** opening, editing, and saving text; Vim-style Normal/Insert
modes, basic motions, line yank/delete/paste, snapshot-based undo/redo,
Doom-inspired leader shortcuts, status line, and minibuffer.

**Not yet included:** Lua, LSP, tree view, syntax highlighting, search,
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