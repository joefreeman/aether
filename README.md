<img src="packaging/uk.joef.Aether.svg" alt="Aether" width="100" />

# Aether

A modal text editor with a client–server architecture for Linux and macOS. Native, terminal and web clients connect to a shared server process.

![screenshot](./screenshot.png)

## Features

- Selection-first motions, sneak, surround, transforms, motion undo/redo
- Tree-sitter integration (highlighting, indentation, selection expand/contract)
- LSP support (diagnostics, hover, go-to-definition, references, document/workspace symbols, formatting)
- Git integration (gutter, inline diff, blame, hunk staging, remotes, commit, branch switching, worktrees, history, stashes)
- Markdown reader mode
- Fuzzy pickers (files, views, symbols, diagnostics, git changes), workspace grep
- File explorer, cross-file jump history, workspace switching
- Native, terminal and web clients with consistent keymaps and behaviour

## Install

Prebuilt binaries for **Linux** and **macOS** (Apple Silicon) are attached to each
[release](https://github.com/joefreeman/aether/releases).

- `aether-<version>-<target>.tar.gz` — the GUI build. Unpack it and put `ae` on your `PATH`; needs
  a graphical environment at runtime.
- `aether-<version>-<target>-no-gui.tar.gz` — as above, but terminal/web only.
- `aether-<version>-x86_64.AppImage` (**Linux**) — the GUI build as one self-contained executable:
  `chmod +x` and run, nothing to unpack. Symlink it onto your `PATH`
  (`ln -s /path/to/aether-<version>-x86_64.AppImage ~/.local/bin/ae`) and every `ae` command works
  through it; an AppImage integration tool can add the app-menu entry and icon.
- `aether-<version>-<target>.dmg` (**macOS**) — the GUI build as a drag-install `Aether.app`. For
  the command line, symlink the binary it wraps
  (`ln -s /Applications/Aether.app/Contents/MacOS/ae /usr/local/bin/ae`).

> **macOS:** downloads are unsigned, so clear the quarantine flag once —
> `xattr -d com.apple.quarantine ./ae` for a `.tar.gz` binary,
> `xattr -dr com.apple.quarantine /Applications/Aether.app` for the app bundle.

## Keybindings

Type `Space y` for the in-app searchable list. Holding the Shift key extends the selection (e.g.
`Shift-w`); a leading **count** repeats a motion (e.g. `3w`). `Space` is the leader for
app/file/code commands, `Space g` the sub-leader for git operations, and `Space t` reveals hover info
at the cursor.

### Motions (normal mode)

| Key | Action |
| --- | --- |
| `h`/`l` | Character left/right |
| `j`/`Alt-j` | Logical/visual line down |
| `k`/`Alt-k` | Logical/visual line up |
| `w`/`Alt-w` | Select small/big word |
| `b`/`Alt-b` | Small/big word backward |
| `e`/`Alt-e` | Small/big word end |
| `0`, `Home` | Logical line start |
| `Alt-l`, `End` | Logical line end |
| `Alt-h` | First non-blank of line |
| `f`/`Alt-f` | Find character forward/backward (next key is the target) |
| `t`/`Alt-t` | Till character forward/backward |
| `s`/`Alt-s` | Sneak to small/big word (type a prefix, then the label on the word you want) |
| `m`/`Alt-m` | Matching bracket/inner matching bracket |
| `o`/`Alt-o` | Next/previous symbol |
| `p`/`Alt-p` | First non-blank of next/previous line |
| `g`/`Alt-g` | Go to line (count, default 1)/from end (default last) |
| `v`/`Alt-v` | Cursor down/up half a page |
| `Backspace`/`Alt-Backspace` | Jump back/forward (cross-file history) |
| `]`/`[` | Next/previous jumplist entry |
| `}`/`{` | Next/previous jumplist entry in this file |
| `Tab`/`Shift-Tab` | Focus the next/previous editor element (a patch's hunks) |

### Scrolling

These move the view, not the cursor, and work the same in the reading view.

| Key | Action |
| --- | --- |
| `PageUp`/`PageDown` | Scroll a page up/down |
| `Alt-↑`/`Alt-↓` | Scroll half a page up/down |
| `↑`/`↓` | Scroll one line up/down |
| `←`/`→` | Scroll one column left/right (in the reading view, the focused code block) |

### Selection & history (normal mode)

| Key | Action |
| --- | --- |
| `,` | Collapse selection |
| `r`/`Alt-r` | Reverse selection (swap cursor and anchor) / orient it forward |
| `%` | Select all |
| `q`/`Alt-q` | Expand/contract selection to syntax node |
| `x`/`Alt-x` | Select line downward/upward |
| `z`/`Alt-z` | Undo/redo cursor motion |
| `.` | Repeat last motion |
| `;`/`Alt-;` | Cursor near top/bottom of window |

### Search (normal mode)

| Key | Action |
| --- | --- |
| `/` | Search |
| `?` | Search, selecting from the cursor to the match |
| `Alt-/` | Search for current selection |
| `n`/`Alt-n` | Next/previous match |
| `Esc` | Clear the active search |

`Alt-c`/`Alt-w`/`Alt-e` toggle case sensitivity, whole-word and regex matching from the prompt, and
`Alt-Backspace` drops the query's last word. `Up`/`Down` recall earlier queries — here and in every
other overlay input (grep, globs, paths).

### Editing (Ctrl — shared by normal and insert)

Every Ctrl edit works in both modes. The clipboard/edit keys are selection-scoped in
normal and line-scoped in insert (since insert has no selection), on the same key; the rest are
identical in both.

| Key | Normal | Insert |
| --- | --- | --- |
| `Ctrl-e` | Change selection | Change line |
| `Ctrl-d` | Delete selection | Delete line |
| `Delete` | Delete selection | Delete character at cursor |
| `Ctrl-c` | Copy selection | Copy line |
| `Ctrl-x` | Cut selection | Cut line |
| `Ctrl-Alt-x` | Cut selection and insert | — |
| `Ctrl-v` | Paste before selection | Paste at cursor |
| `Ctrl-Alt-v` | Replace selection with clipboard | Replace line with clipboard |
| `Ctrl-s` | Surround selection (next key = delimiter) | Surround line |
| `Ctrl-Alt-s` | Unsurround selection | Unsurround line |
| `Ctrl-r` | Transform selection (next key = transform: case styles, invert, reverse, randomise) | Transform identifier under cursor |
| `Ctrl-z`/`Ctrl-Alt-z` | Undo/redo | Undo/redo |
| `Ctrl-l`/`Ctrl-h` | Indent/dedent | Indent/dedent |
| `Ctrl-j`/`Ctrl-k` | Move line(s) down/up | Move line(s) down/up |
| `Ctrl-Alt-j`/`Ctrl-Alt-k` | Move paragraph down/up | Move paragraph down/up |
| `Ctrl-g` | Join lines | Join lines |
| `Ctrl-Alt-g` | Un-join lines (cursor stays before the break) | Line break at caret, caret stays |
| `Ctrl-a`/`Ctrl-Alt-a` | Increment/decrement number | Increment/decrement number |
| `Ctrl-y`/`Ctrl-Alt-y` | Toggle line/block comment | Toggle line/block comment |
| `Ctrl-f` | Format document | Format document |
| `Ctrl-o`/`Ctrl-Alt-o` | Open line below/above | Open line below/above |

In insert mode, `Tab` indents to the next tab stop and `Backspace` steps back to the previous one,
both following the file's own indent style. `Alt-←`/`Alt-→` move by word, and
`Alt-Backspace`/`Alt-Delete` delete the word before/after the caret.

### Mode transitions

| Key | Action |
| --- | --- |
| `i`/`a` | Insert at selection start/end |
| `Alt-i`/`Alt-a` | Insert at first non-blank of line/last line end |
| `Esc` | Leave insert mode |

### Markdown reading view

`Space u` renders the current Markdown file — headings, tables, images, links and highlighted
code fences — as a read-only view with its own keys. The reading position *is* the cursor, so
toggling back lands where you were reading. A file's editor and its reader are two views, each
keeping its own scroll position; opening the file lands in whichever you used last, and the
`markdown_read` setting (`Space ,`) decides for a file with neither open.

`o`/`Alt-o` step the document outline — the same outline the breadcrumb and `Space o` show, which
comes from the language server. Heading navigation therefore needs a Markdown language server
configured; the rest of the reading view works without one.

| Key | Action |
| --- | --- |
| `Space u` | Toggle the reading view |
| `j`/`k` | Focus next/previous element |
| `l`/`h` | Focus next/previous link in the block |
| `o`/`Alt-o` | Next/previous heading |
| `g`/`Alt-g` | First/last element |
| `z`/`Alt-z` | Undo/redo the reading-position move |
| `Enter` | Follow the link, open the image, jump to the footnote, or toggle a task's checkbox |
| `Ctrl-Enter` | Follow a relative link in a new window |
| `Space t` | Show the link's or image's target |
| `x`/`Alt-x`, `Shift-j`/`Shift-k` | Select blocks — as in the editor, plain `x` walks and Shift extends |
| `r`/`Alt-r` | Reverse the selection / orient it forward |
| `%`/`,` | Select every block / collapse the selection to the cursor's block |
| `Ctrl-c` | Copy the selection, the link URL, or the element's Markdown source |
| `Ctrl-z`/`Ctrl-Alt-z` | Undo / redo |
| `Ctrl-a`/`Ctrl-Alt-a` | Check / uncheck the focused task item |
| `i`/`a` | Edit: insert at block/selection start / end |
| `Ctrl-e` | Edit: rewrite the selected block(s) |
| `Ctrl-o`/`Ctrl-Alt-o` | Edit: open a new block below / above — a list item inside a list, a paragraph elsewhere |
| `Ctrl-j`/`Ctrl-k` | Move block(s) down / up (`Ctrl-Alt-j`/`k` moves paragraphs in the editor) |
| `Ctrl-x`, `Ctrl-v`/`Ctrl-Alt-v` | Cut block(s); paste as block / replace selection |
| `Ctrl-d` | Delete block(s) (no clipboard) |
| `Ctrl-l`/`Ctrl-h` | Deepen/flatten: heading level, list nesting, or blockquote level |

Search, jump history and the scroll/placement keys behave as they do in normal mode.

### Application

| Chord | Action |
| --- | --- |
| `Space f`/`Space Alt-f` | Find files / in this file's directory |
| `Space v`/`Space a` | Switch view / new scratch |
| `Space /`/`Space Alt-/` | Grep workspace / for current selection |
| `Space e`/`Space Alt-e` | File explorer / at workspace root |
| `Space w`/`Space Alt-w` | Switch workspace / open file by absolute path |
| `Space j`/`Space Alt-j` | Jumplist (`Ctrl-j` in any picker captures its results into it) / clear it |
| `Space p`/`Space Alt-p` | Copy relative/absolute path |
| `Space s`/`Space Alt-s` | Save / save as |
| `Space k`/`Space Alt-k` | Keep view (toggle transient) / reload from disk |
| `Space x`/`Space Alt-x` | Close view / save and close it |
| `Space z`/`Space Alt-z` | Open another window / copy this view's web URL |
| `Space ,`/`Space .` | Application settings (soft wrap, font sizes, …) / this workspace's (roots, projects) |
| `Space h`/`Space Alt-h` | Dismiss the current hint / turn hints off |
| `Space q`/`Space Alt-q` | Quit / save and quit |
| `Space y`/`Space ?` | Show keyboard shortcuts / about this build |

### Git

Navigation, reveals and views sit on the plain leader, beside their diagnostics counterparts; the
operations live behind the `Space g` sub-leader. There, a plain/Alt pair names one verb at two
scopes — plain takes the change under the cursor (or the selected lines), Alt the whole file.

| Chord | Action |
| --- | --- |
| `c`/`Alt-c` | Next/previous change (hunk) |
| `Space c`/`Space Alt-c` | Git changes in current file / across the workspace (hunks) |
| `Space m` | Blame commit details for the cursor line |
| `Space i`/`Space Alt-i` | Toggle inline diff, in a file or a patch view / choose what it diffs against |
| `Space g s`/`Space g Alt-s` | Stage the change / the whole file (also marks a conflict resolved) |
| `Space g u`/`Space g Alt-u` | Unstage the change / the whole file |
| `Space g r`/`Space g Alt-r` | Revert the change / the whole file |
| `Space g <`/`Space g >`/`Space g =` | Resolve a conflict: keep the top (`<<<<<<<`), the bottom (`>>>>>>>`), or both sections |
| `Space g c`/`Space g Alt-c` | Commit staged changes / amend the previous commit |
| `Space g z` | Uncommit (keep the changes staged) |
| `Space g w` | Working changes — everything uncommitted, as one patch |
| `Space g l`/`Space g Alt-l` | History / this file's history |
| `Space g b` | Branches and worktrees |
| `Space g f` | Fetch from the remote |
| `Space g p`/`Space g Alt-p` | Pull from / push to the remote |
| `Space g x` | Stop the fetch, push or pull in progress |
| `Space g t`/`Space g Alt-t` | Stash the working tree / just the staged changes |
| `Space g a` | Stashes (preview, pop, apply, drop) |
| `Space g d` | Abandon a stopped merge or rebase (asks first) |

### Code / LSP

| Chord | Action |
| --- | --- |
| `Space t` | Hover (type & docs, or a link's target) |
| `Enter` | Follow what's under the cursor: the definition — or, in a patch, the file that line came from |
| `Space r` | Go to references |
| `d`/`Alt-d` | Next/previous diagnostic |
| `Space n` | Diagnostic at cursor |
| `Space d`/`Space Alt-d` | Diagnostics: this file / workspace |
| `Space o`/`Space Alt-o` | Document / workspace symbols |
| `Space l` | LSP servers (status, restart) |
| `Ctrl-f` | Format document |

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

Just run `ae` — it opens a client and, if no server is already running, auto-starts one in the
background:

```sh
ae                         # open the workspace picker
ae src/main.rs             # open a file (workspace inferred from its path)
ae src/                    # open the file explorer at a directory
ae -w aether               # open the "aether" workspace
ae -w aether src/main.rs   # open a file in a named workspace
```

The first client launches a background server and connects to it; later clients reuse it, and the
server idle-reaps itself once nothing has been connected for a while. To run a persistent server
yourself (e.g. to watch its logs), use `ae server`, and stop it with `ae server stop`.

With no `--gui`/`--tui` flag, `ae` picks a client automatically: a terminal on stdout means the
terminal client; no terminal but a display set (a desktop launcher) means the GUI. Pass `--gui` or
`--tui` to force one.

A `path` is resolved against the current working directory; if it falls outside every configured
workspace it opens as a standalone file. A directory opens the file browser there.

Opening a file that way — `ae file`, with no `-w` — *tethers* the client to that file's view: closing it
(`Space x`, or `Space Alt-x` to save first) exits the client, so `ae` works as an `$EDITOR` for git
and anything else that waits for the process to finish.

Workspaces are created and managed from the workspace picker (`Space w`); running `ae` with no
arguments opens it.

## Web client

The web client is served by the same server process: with a server running, open
<http://127.0.0.1:2384>. There's no token to copy — the daemon is loopback-only and authorizes by
`Host`/`Origin`, so a browser on the same machine just connects.

Building from source needs its bundle built once (`web/`, TypeScript):

```sh
cd web
npm install     # first time only
npm run build   # tsc (typecheck + compile), then Vite bundles to web/dist
```

Release builds embed `web/dist` in the binary, so a released `ae` is self-contained; debug builds
read it from disk, so a rebuilt bundle is served without restarting the server.

## License

[MIT](LICENSE)
