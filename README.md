<img src="logo.svg" alt="Aether" width="100" />
<br />

# Aether

An experimental modal text editor with a client–server architecture and tree-sitter integration.

Aether splits editing across two processes: a long-lived server, running locally, holds all
text state — buffer contents, cursors, selections, the undo stack, per-viewport soft wrap — while
thin clients render what the server sends and forward keystrokes. Multiple clients can share a
buffer, see each other's cursors, and share a single undo stack.

## Features

- Modal editing (normal/insert mode)
- Tree-sitter integration (highlighting, indentation, navigation)
- Surround/unsurround, toggle-comment, join and move lines
- Undo and redo stacks for edits and cursor/selection motions
- Fuzzy pickers for files/buffers/projects, file explorer, project-wide grep
- Mouse support, soft wrap, system-clipboard integration
- Git integration (gutter, inline diff, blame, hunk staging)
- LSP (diagnostics, hover, go-to-definition, format)
- Terminal and web clients

## Keybindings

Type `Space ?` for the in-app overlay. Holding the Shift key extends the selection (e.g., `Shift-w`); a leading
**count** repeats a motion (e.g., `3w`). `Space` is the leader for app/file/git/code commands, and `Tab` reveals
hover info at the cursor.

### Motions (normal mode)

| Key | Action |
| --- | --- |
| `h`/`l` | Character left/right |
| `j`/`Alt-j` | Logical/visual line down |
| `k`/`Alt-k` | Logical/visual line up |
| `w`/`Alt-w` | Small/big word forward |
| `b`/`Alt-b` | Small/big word backward |
| `e`/`Alt-e` | Small/big word end |
| `0`, `Home` | Logical line start |
| `Alt-l`, `End` | Logical line end |
| `Alt-h` | First non-blank of line |
| `g`/`Alt-g` | Go to line (count, default 1)/last line |
| `v`/`Alt-v` | Cursor down/up half a page |
| `f`/`Alt-f` | Find character forward/backward (next key is the target) |
| `t`/`Alt-t` | Till character forward/backward |
| `m`/`Alt-m` | Matching bracket/inner matching bracket |
| `p`/`Alt-p` | Next/previous navigation unit |
| `Shift-p`/`Shift-Alt-p` | Select to end/start of unit |
| `Backspace`/`Alt-Backspace` | Jump back/forward (cross-file history) |

### Selection & history (normal mode)

| Key | Action |
| --- | --- |
| `,` | Collapse selection |
| `%` | Select whole buffer |
| `o` | Swap cursor and anchor |
| `y`/`Alt-y` | Expand/contract selection to syntax node |
| `x`/`Alt-x` | Select line downward/upward |
| `u`/`Alt-u` | Undo/redo cursor motion |
| `.` | Repeat last motion |
| `;` | Center cursor in window |

### Search & grep (normal mode)

| Key | Action |
| --- | --- |
| `/` | Search |
| `?` | Search, selecting from the cursor to the match |
| `Alt-/` | Search for current selection |
| `n`/`Alt-n` | Next/previous match |
| `Space n`/`Space Alt-n` | Next/previous grep result |
| `Esc` | Clear the active search |

### Editing (Ctrl — shared by normal and insert)

Every Ctrl edit works in both modes. The clipboard/edit keys are selection-scoped in
normal and line-scoped in insert (since insert has no selection), on the same key; the rest are
identical in both.

| Key | Normal | Insert |
| --- | --- | --- |
| `Ctrl-a` | Change selection | Change line |
| `Ctrl-d` | Delete selection | Delete line |
| `Ctrl-c` | Copy selection | Copy line |
| `Ctrl-x` | Cut selection | Cut line |
| `Ctrl-v` | Paste before selection | Paste at cursor |
| `Ctrl-Alt-v` | Replace selection with clipboard | Replace line with clipboard |
| `Ctrl-s` | Surround selection (next key = delimiter) | Surround line |
| `Ctrl-Alt-s` | Unsurround selection | Unsurround line |
| `Ctrl-u`/`Ctrl-Alt-u` | Undo/redo | Undo/redo |
| `Ctrl-l`/`Ctrl-h` | Indent/dedent | Indent/dedent |
| `Ctrl-j`/`Ctrl-k` | Move line(s) down/up | Move line(s) down/up |
| `Ctrl-g` | Join lines | Join lines |
| `Ctrl-y` | Toggle comment | Toggle comment |
| `Ctrl-f` | Format document | Format document |
| `Ctrl-o`/`Ctrl-Alt-o` | Open line below/above | Open line below/above |

### Mode transitions

| Key | Action |
| --- | --- |
| `i`/`a` | Insert at selection start/end |
| `Alt-i`/`Alt-a` | Insert at first non-blank of line/last line end |
| `Esc` | Leave insert mode |

### Application

| Chord | Action |
| --- | --- |
| `Space f`/`Space Alt-f` | Find files / in buffer's directory |
| `Space b` | Switch buffer |
| `Space Alt-b` | New scratch buffer |
| `Space g`/`Space Alt-g` | Grep workspace / buffer's directory |
| `Space e`/`Space Alt-e` | File explorer / at project root |
| `Space p` | Switch project |
| `Space ,` | Project settings |
| `Space .` | Application settings (soft wrap, …) |
| `Space s`/`Space Alt-s` | Save/save as |
| `Space a` | Reload from disk |
| `Space w` | Close buffer |
| `Space q` | Quit |
| `Space ?` | Show keyboard shortcuts |

### Git

| Chord | Action |
| --- | --- |
| `c`/`Alt-c` | Next/previous change (hunk) |
| `Space y`/`Space Alt-y` | Stage-unstage / revert the change under the cursor (or selected lines) |
| `Space i` | Toggle inline diff |
| `Space m` | Blame commit details for the cursor line |

### Code / LSP

| Chord | Action |
| --- | --- |
| `Tab` | Hover (type & docs) |
| `Enter` | Go to definition |
| `Space r` | Go to references |
| `d`/`Alt-d` | Next/previous diagnostic |
| `Space j` | Diagnostic at cursor |
| `Space d` | Diagnostics list |
| `Space o` | Document symbols |
| `Space l` | LSP servers (status, restart) |
| `Ctrl-f` | Format document |

## Install

Prebuilt binaries for **Linux** and **macOS** (Apple Silicon) are attached to each
[release](https://github.com/joefreeman/aether/releases), in two variants per platform:

- `aether-<version>-<target>.tar.gz` — the **GUI** build (native window + server + terminal/web
  clients); needs a graphical environment at runtime.
- `aether-<version>-<target>-no-gui.tar.gz` — same editor minus the desktop window: server,
  terminal client, and embedded web client, with no graphics libraries required (headless boxes,
  SSH).

Each archive holds the single `ae` binary; unpack it and put `ae` on your `PATH`.

> **macOS:** binaries are unsigned, so clear the quarantine flag once after unpacking:
> `xattr -d com.apple.quarantine ./ae`.

## Building

Aether is a standard Cargo workspace.

```sh
cargo build --release
```

This produces a single binary:

- `ae` — runs the server daemon, the terminal client, and (when built with the `gui` feature, on
  by default) the native GUI client. The build that ships the GUI is the default; dropping it with
  `cargo build --release -p aether-ae --no-default-features` (so `iced`/`winit`/`wgpu` never enter
  the dependency graph) is exactly the `-no-gui` release artifact, for a box with no display libraries.

## Running

1. **Start the server:**

   ```sh
   ae --server
   ```

2. **Start the client**, optionally naming a project and a file/directory to open:

   ```sh
   ae                     # start with the project picker open
   ae aether              # open the "aether" project in a scratch buffer
   ae aether src/main.rs  # open a file
   ae aether src/         # open the file explorer at a directory
   ```

   With no `--gui`/`--tui` flag, `ae` picks a client automatically: a terminal on stdout means the
   terminal client; no terminal but a display set (a desktop launcher) means the GUI. Pass `--gui`
   or `--tui` to force one.

   `path` is resolved against the current working directory and must fall within one of the
   project's roots. A directory opens the file browser there.

   Projects are created and managed from the project picker (`Space p`); running `ae` with no
   arguments opens it.

## Web client

The web client (`web/`, TypeScript) is served by the same server process. Build the bundle once,
then open it in a browser:

```sh
cd web
npm install     # first time only
npm run build   # tsc (typecheck + compile), then Vite bundles to web/dist
```

`npm run build` runs `tsc && vite build`. The server serves `web/dist` directly — the path is baked
from the crate and read at runtime, so a rebuilt bundle is picked up without rebuilding the server.
With the server running, open <http://127.0.0.1:2384>. There's no token to copy: the daemon is
loopback-only and authorizes by `Host`/`Origin`, so a browser on the same machine just connects.

## License

[MIT](LICENSE)
