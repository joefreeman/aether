//! Authoritative in-memory state owned by the server.

use aether_protocol::cursor::CursorState;
use aether_protocol::envelope::Notification;
use aether_protocol::viewport::WrapMode;
use aether_protocol::{BufferId, ClientId, Revision, ViewportId};
use std::collections::HashMap;
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
    next_buffer_id: u64,
    next_viewport_id: u64,
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
    pub dirty: bool,
    pub line_ending: LineEnding,
    pub last_modified_unix_ms: Option<u64>,
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
        Ok(Buffer {
            id,
            canonical_path: Some(canonical),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending,
            last_modified_unix_ms,
        })
    }

    pub fn scratch(id: BufferId, language: Option<String>) -> Self {
        Buffer {
            id,
            canonical_path: None,
            text: ropey::Rope::new(),
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
        }
    }

    pub fn line_count(&self) -> u32 {
        // ropey counts lines as separated by \n; a trailing empty "line" after a final \n is
        // included. For protocol purposes we report ropey's count directly — clients see what
        // ropey sees.
        self.text.len_lines() as u32
    }

    pub fn byte_count(&self) -> u64 {
        self.text.len_bytes() as u64
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
        self.dirty = false;
        Ok(mtime_ms)
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
    /// First logical line currently pushed to the client (inclusive).
    pub first_logical_line: u32,
    /// Last logical line currently pushed to the client (exclusive).
    pub last_logical_line_exclusive: u32,
}
