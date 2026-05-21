//! Authoritative in-memory state owned by the server.

use crate::syntax::{self, LanguageConfig};
use aether_protocol::cursor::CursorState;
use aether_protocol::envelope::Notification;
use aether_protocol::viewport::WrapMode;
use aether_protocol::{BufferId, ClientId, LogicalPosition, Revision, ViewportId};
use std::time::{Duration, Instant};
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// Edits within this window join the active undo group.
const GROUP_TIME_WINDOW: Duration = Duration::from_millis(500);
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub type SharedState = Arc<Mutex<ServerState>>;

pub struct ServerState {
    pub project_name: String,
    /// Canonicalized project paths. Each is either a file or a directory.
    pub project_paths: Vec<PathBuf>,
    pub token: String,
    pub buffers: HashMap<BufferId, Buffer>,
    pub clients: HashMap<ClientId, ClientSession>,
    pub viewports: HashMap<ViewportId, Viewport>,
    pub cursors: HashMap<(ClientId, BufferId), CursorState>,
    /// Per-`(client, buffer)` history of cursor states for motion undo/redo. Distinct from the
    /// buffer's own undo stack: this rewinds *only* the client's own cursor moves and is cleared
    /// by any buffer mutation (since prior positions may no longer be valid).
    pub motion_history: HashMap<(ClientId, BufferId), MotionHistory>,
    /// The cursor's "intended" *visual* column for vertical motions — preserved across repeated
    /// `Motion::VisualLine` presses so that landing on rows with different prefixes (continuation
    /// marker + indent) doesn't cause the visual column to drift. Cleared by any non-vertical
    /// motion, explicit cursor set, or buffer mutation. Only meaningful for `VisualLine`; logical
    /// `j/k` clears it (mixing motion kinds resets intent).
    pub virtual_col: HashMap<(ClientId, BufferId), u32>,
    /// Per-`(client, buffer)` active search. Set by `search/set`, cleared by `search/clear` or
    /// when the client disconnects / the buffer closes. Re-run whenever the buffer mutates.
    pub searches: HashMap<(ClientId, BufferId), SearchEntry>,
    next_buffer_id: u64,
    next_viewport_id: u64,
}

/// Server-side state for one client's active search on a specific buffer.
#[derive(Debug, Clone)]
pub struct SearchEntry {
    pub query: String,
    /// Sorted by start position. Each match is `(start_inclusive, end_exclusive)` in
    /// buffer-line / byte-col coords.
    pub matches: Vec<(LogicalPosition, LogicalPosition)>,
    /// `true` when the server hit its match cap (`SEARCH_MAX_MATCHES`) and the real count is
    /// higher. `matches.len()` is then a prefix.
    pub truncated: bool,
}

/// Cap on each direction's stack. Bounds memory in pathological cases (e.g. holding down a
/// motion key), and matches the "cursor undo is per-client transient state, not an audit log"
/// framing.
pub const MOTION_HISTORY_CAP: usize = 100;

#[derive(Default)]
pub struct MotionHistory {
    pub undo: VecDeque<CursorState>,
    pub redo: Vec<CursorState>,
}

impl MotionHistory {
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

impl ServerState {
    pub fn new(project_name: String, project_paths: Vec<PathBuf>, token: String) -> Self {
        Self {
            project_name,
            project_paths,
            token,
            buffers: HashMap::new(),
            clients: HashMap::new(),
            viewports: HashMap::new(),
            cursors: HashMap::new(),
            motion_history: HashMap::new(),
            virtual_col: HashMap::new(),
            searches: HashMap::new(),
            next_buffer_id: 1,
            next_viewport_id: 1,
        }
    }

    pub fn allocate_buffer_id(&mut self) -> BufferId {
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;
        id
    }

    pub fn allocate_viewport_id(&mut self) -> ViewportId {
        let id = self.next_viewport_id;
        self.next_viewport_id += 1;
        id
    }

    /// Remove all viewports owned by the given client. Used on disconnect.
    pub fn drop_viewports_for_client(&mut self, client_id: ClientId) {
        self.viewports.retain(|_, v| v.client_id != client_id);
    }

    /// Remove all cursor records for the given client. Used on disconnect.
    pub fn drop_cursors_for_client(&mut self, client_id: ClientId) {
        self.cursors.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all motion-history records for the given client. Used on disconnect.
    pub fn drop_motion_history_for_client(&mut self, client_id: ClientId) {
        self.motion_history.retain(|(c, _), _| *c != client_id);
    }

    /// Record a user-initiated cursor state transition. The previous state goes on the undo
    /// stack and the redo stack is cleared. No-op if the state didn't change. Called by every
    /// `cursor/*` handler.
    pub fn record_motion(
        &mut self,
        key: (ClientId, BufferId),
        prev: CursorState,
        next: CursorState,
    ) {
        if prev == next {
            return;
        }
        let history = self.motion_history.entry(key).or_default();
        // Skip duplicate top — defensive against compound client ops that touch the cursor more
        // than once via the same intermediate state.
        if history.undo.back() != Some(&prev) {
            history.undo.push_back(prev);
            while history.undo.len() > MOTION_HISTORY_CAP {
                history.undo.pop_front();
            }
        }
        history.redo.clear();
    }

    /// Clear motion history for every client on the given buffer. Called on any buffer mutation
    /// (text, delete, cut, join, undo, redo) — remembered positions could be invalid after the
    /// buffer changes, and the user contract is "motion undo only goes back to the last edit".
    pub fn clear_motion_history_for_buffer(&mut self, buffer_id: BufferId) {
        for ((_, b), h) in self.motion_history.iter_mut() {
            if *b == buffer_id {
                h.clear();
            }
        }
    }

    pub fn drop_virtual_col_for_client(&mut self, client_id: ClientId) {
        self.virtual_col.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all search records for the given client. Used on disconnect.
    pub fn drop_searches_for_client(&mut self, client_id: ClientId) {
        self.searches.retain(|(c, _), _| *c != client_id);
    }

    /// Clear virtual column for every client on the given buffer. Called on any buffer mutation.
    pub fn clear_virtual_col_for_buffer(&mut self, buffer_id: BufferId) {
        self.virtual_col.retain(|(_, b), _| *b != buffer_id);
    }

    /// Locate an already-open buffer for the given canonical path, if any.
    pub fn buffer_for_path(&self, canonical: &Path) -> Option<BufferId> {
        self.buffers
            .iter()
            .find(|(_, b)| b.canonical_path.as_deref() == Some(canonical))
            .map(|(id, _)| *id)
    }

    /// True iff the given canonical path is allowed by the project's access boundary.
    pub fn path_is_in_project(&self, canonical: &Path) -> bool {
        self.project_paths.iter().any(|p| canonical == p || canonical.starts_with(p))
    }
}

pub struct Buffer {
    pub id: BufferId,
    pub canonical_path: Option<PathBuf>,
    pub text: ropey::Rope,
    pub revision: Revision,
    pub language: Option<String>,
    /// Derived: `revision != saved_revision`. Kept as a field for cheap reads.
    pub dirty: bool,
    pub line_ending: LineEnding,
    pub last_modified_unix_ms: Option<u64>,
    pub syntax: Option<BufferSyntax>,

    /// Revision at the most recent successful save. `None` only for a never-saved scratch
    /// buffer in its initial empty state — see `Buffer::scratch`.
    saved_revision: Option<Revision>,
    /// Source of fresh revision ids. Always strictly greater than any revision ever assigned.
    next_revision_id: u64,

    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    active_group: Option<ActiveGroup>,
}

pub struct BufferSyntax {
    pub config: &'static LanguageConfig,
    pub parser: Parser,
    pub tree: Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKindTag {
    Text,
    Delete,
}

struct UndoEntry {
    rope: ropey::Rope,
    revision: Revision,
    cursors: std::collections::HashMap<ClientId, CursorState>,
}

struct ActiveGroup {
    last_edit_at: Instant,
    kind: EditKindTag,
}

pub struct UndoOutcome {
    pub new_revision: Revision,
    /// Cursor positions captured at the start of the rewound group. The undoing client uses
    /// theirs as the post-undo cursor; other clients clamp these or their existing positions
    /// to valid buffer offsets.
    pub restored_cursors: std::collections::HashMap<ClientId, CursorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl Buffer {
    /// Load a buffer from disk. Detects line endings, normalizes to LF in-memory.
    pub fn load_from_file(id: BufferId, canonical: PathBuf) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(&canonical)?;
        let line_ending = if content.contains("\r\n") { LineEnding::Crlf } else { LineEnding::Lf };
        let normalized = if line_ending == LineEnding::Crlf {
            content.replace("\r\n", "\n")
        } else {
            content
        };
        let text = ropey::Rope::from_str(&normalized);
        let metadata = std::fs::metadata(&canonical).ok();
        let last_modified_unix_ms = metadata.and_then(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
        });
        let language = detect_language(&canonical);
        let syntax = language.as_deref().and_then(|name| make_syntax(&text, name));
        Ok(Buffer {
            id,
            canonical_path: Some(canonical),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending,
            last_modified_unix_ms,
            syntax,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
        })
    }

    pub fn scratch(id: BufferId, language: Option<String>) -> Self {
        let text = ropey::Rope::new();
        let syntax = language.as_deref().and_then(|name| make_syntax(&text, name));
        Buffer {
            id,
            canonical_path: None,
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            // Treat empty scratch as "clean"; first edit makes it dirty.
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
        }
    }

    pub fn line_count(&self) -> u32 {
        // ropey counts lines as separated by \n; a trailing empty "line" after a final \n is
        // included. For protocol purposes we report ropey's count directly — clients see what
        // ropey sees.
        self.text.len_lines() as u32
    }

    /// Revision at the last successful save (or `0` for a fresh scratch buffer that's never been
    /// saved). The client uses this together with `revision` to derive `dirty`.
    pub fn saved_revision(&self) -> Revision {
        self.saved_revision.unwrap_or(0)
    }

    pub fn byte_count(&self) -> u64 {
        self.text.len_bytes() as u64
    }

    /// Apply a text replacement: remove `start_char..end_char`, insert `insert_text` at
    /// `start_char`. Bumps `revision`, marks dirty, updates the parse tree incrementally, and
    /// manages the undo group (opening a new entry if grouping conditions broke).
    ///
    /// `cursors_before_edit` is the per-client cursor map captured before this edit; it's
    /// stored in the undo entry when a new group opens, so `Buffer::undo` can restore cursors.
    pub fn apply_edit(
        &mut self,
        start_char: usize,
        end_char: usize,
        insert_text: &str,
        kind: EditKindTag,
        cursors_before_edit: std::collections::HashMap<ClientId, CursorState>,
    ) -> Revision {
        let now = Instant::now();

        // Decide whether to start a new undo group.
        let start_new_group = match &self.active_group {
            None => true,
            Some(g) => now.duration_since(g.last_edit_at) > GROUP_TIME_WINDOW || g.kind != kind,
        };
        if start_new_group {
            self.undo_stack.push(UndoEntry {
                rope: self.text.clone(),
                revision: self.revision,
                cursors: cursors_before_edit,
            });
            self.redo_stack.clear();
        }

        // Capture old byte positions for tree-sitter's InputEdit *before* mutating the rope.
        let edit_info = if self.syntax.is_some() {
            let start_byte = self.text.char_to_byte(start_char);
            let old_end_byte = self.text.char_to_byte(end_char);
            let start_position = rope_byte_to_point(&self.text, start_byte);
            let old_end_position = rope_byte_to_point(&self.text, old_end_byte);
            Some((start_byte, old_end_byte, start_position, old_end_position))
        } else {
            None
        };

        if start_char < end_char {
            self.text.remove(start_char..end_char);
        }
        if !insert_text.is_empty() {
            self.text.insert(start_char, insert_text);
        }
        self.revision = self.next_revision_id;
        self.next_revision_id += 1;
        self.active_group = Some(ActiveGroup { last_edit_at: now, kind });
        self.recompute_dirty();

        if let Some((start_byte, old_end_byte, start_position, old_end_position)) = edit_info {
            let new_end_byte = start_byte + insert_text.len();
            let new_end_position = rope_byte_to_point(&self.text, new_end_byte);

            let text = &self.text;
            let syntax = self.syntax.as_mut().expect("just checked");
            syntax.tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
            let parser = &mut syntax.parser;
            let tree = &mut syntax.tree;
            let new_tree = parser.parse_with(
                &mut |byte_idx: usize, _: Point| -> &[u8] {
                    if byte_idx >= text.len_bytes() {
                        return &[];
                    }
                    let (chunk, chunk_byte_start, _, _) = text.chunk_at_byte(byte_idx);
                    let bytes = chunk.as_bytes();
                    &bytes[byte_idx - chunk_byte_start..]
                },
                Some(&*tree),
            );
            if let Some(t) = new_tree {
                *tree = t;
            }
        }

        self.revision
    }

    /// Write the buffer to disk atomically: write to `<dir>/.aether-tmp-<pid>-<name>`,
    /// fsync, rename onto `target`, fsync the parent directory. Restores CRLF if the buffer
    /// was loaded with CRLF endings. Updates `canonical_path`, `dirty`, `last_modified_unix_ms`.
    ///
    /// Returns the post-save mtime in unix milliseconds.
    pub fn save_to_disk(&mut self, target: PathBuf) -> std::io::Result<u64> {
        use std::io::Write;

        let mut text: String = self.text.chunks().collect();
        if self.line_ending == LineEnding::Crlf {
            text = text.replace('\n', "\r\n");
        }

        let parent = target.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "save target has no parent dir")
        })?;
        let file_name = target.file_name().and_then(|s| s.to_str()).unwrap_or("aether");
        let tmp_path = parent.join(format!(".aether-tmp-{}-{file_name}", std::process::id()));

        // Write to tmp.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // Atomic rename.
        if let Err(e) = std::fs::rename(&tmp_path, &target) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        // Best-effort: fsync the parent directory so the rename is durable.
        #[cfg(unix)]
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        let canonical = std::fs::canonicalize(&target).unwrap_or(target);
        let mtime_ms = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        self.canonical_path = Some(canonical);
        self.last_modified_unix_ms = Some(mtime_ms);
        self.saved_revision = Some(self.revision);
        self.active_group = None;
        self.recompute_dirty();
        Ok(mtime_ms)
    }

    pub fn undo(
        &mut self,
        current_cursors: std::collections::HashMap<ClientId, CursorState>,
    ) -> Option<UndoOutcome> {
        let entry = self.undo_stack.pop()?;
        self.redo_stack.push(UndoEntry {
            rope: self.text.clone(),
            revision: self.revision,
            cursors: current_cursors,
        });
        self.text = entry.rope;
        self.revision = entry.revision;
        self.active_group = None;
        self.recompute_dirty();
        self.reparse_full();
        Some(UndoOutcome { new_revision: self.revision, restored_cursors: entry.cursors })
    }

    pub fn redo(
        &mut self,
        current_cursors: std::collections::HashMap<ClientId, CursorState>,
    ) -> Option<UndoOutcome> {
        let entry = self.redo_stack.pop()?;
        self.undo_stack.push(UndoEntry {
            rope: self.text.clone(),
            revision: self.revision,
            cursors: current_cursors,
        });
        self.text = entry.rope;
        self.revision = entry.revision;
        self.active_group = None;
        self.recompute_dirty();
        self.reparse_full();
        Some(UndoOutcome { new_revision: self.revision, restored_cursors: entry.cursors })
    }

    fn recompute_dirty(&mut self) {
        self.dirty = self.saved_revision != Some(self.revision);
    }

    /// Re-parse the entire buffer from scratch. Used after operations (undo/redo) that swap the
    /// whole rope — the incremental InputEdit pathway can't help when the buffer is replaced.
    fn reparse_full(&mut self) {
        if let Some(syntax) = self.syntax.as_mut() {
            let source: String = self.text.chunks().collect();
            if let Some(tree) = syntax.parser.parse(&source, None) {
                syntax.tree = tree;
            }
        }
    }
}

fn detect_language(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "toml" => "toml",
        "md" => "markdown",
        "json" => "json",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        _ => return None,
    }
    .to_string())
}

fn make_syntax(text: &ropey::Rope, language: &str) -> Option<BufferSyntax> {
    let config = syntax::get_config(language)?;
    let mut parser = syntax::make_parser(config);
    let source: String = text.chunks().collect();
    let tree = parser.parse(&source, None)?;
    Some(BufferSyntax { config, parser, tree })
}

fn rope_byte_to_point(rope: &ropey::Rope, byte_idx: usize) -> Point {
    let char_idx = rope.byte_to_char(byte_idx);
    let line = rope.char_to_line(char_idx);
    let line_start_char = rope.line_to_char(line);
    let col_chars = char_idx - line_start_char;
    let line_slice = rope.line(line);
    let col_bytes = line_slice.char_to_byte(col_chars);
    Point { row: line, column: col_bytes }
}

pub struct ClientSession {
    #[allow(dead_code)]
    pub client_id: ClientId,
    /// Channel for sending notifications to this client's connection task.
    pub outbound: mpsc::Sender<Notification>,
}

pub struct Viewport {
    pub id: ViewportId,
    pub buffer_id: BufferId,
    pub client_id: ClientId,
    pub cols: u32,
    pub rows: u32,
    pub overscan_rows: u32,
    pub scroll_logical_line: u32,
    pub scroll_sub_row: f32,
    pub wrap: WrapMode,
    pub continuation_marker_width: u32,
    /// First logical line currently pushed to the client (inclusive).
    pub first_logical_line: u32,
    /// Last logical line currently pushed to the client (exclusive).
    pub last_logical_line_exclusive: u32,
}
