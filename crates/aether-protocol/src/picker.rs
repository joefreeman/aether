//! Pickers — fuzzy-matched selection overlays (files, buffers, grep hits, ...). Server owns
//! the candidate cache, query, and ranked snapshot per `(client_id, kind)`; client owns the
//! highlighted row and the scroll window. Items, not indices, are the stable handle: the client
//! persists the last-highlighted item locally and asks the server to scroll to include it on
//! resume.
//!
//! Lifecycle: `picker/view` attaches/subscribes (with `reset` to wipe persisted state or
//! `center_on` to frame around a remembered item), `picker/query` updates the query, `picker/select`
//! confirms a choice, `picker/hide` unsubscribes. The server pushes `picker/update` whenever the
//! subscribed window's contents change or the matcher snapshot ticks.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

/// Which picker the client is talking about. Keyed `(client_id, kind)` server-side; only one
/// instance per kind per client lives at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerKind {
    /// Project files, fuzzy-matched on path.
    Files,
    /// Open buffers, ordered by most-recently-used. The current buffer sits at position 0 and
    /// selecting it is a no-op switch.
    Buffers,
    /// Workspace-wide content search. Each candidate is a single match on a single line; the
    /// query *is* the search (no fuzzy filtering on a pre-built candidate set), so query changes
    /// throw out the prior candidates and start a fresh scan. Persisted hits stay around across
    /// `hide`/`view` so the user can step through results — they may be stale relative to the
    /// file on disk after editing, and that's accepted (jumps clamp to the current line bounds).
    Grep,
}

/// A pickable item. Tagged enum so different pickers can carry the data they need; match-index
/// highlighting rides in `match_indices` (char positions within the display string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerItem {
    /// A file from the workspace walk. `path` is project-relative (forward-slash separated).
    File {
        path: String,
        /// Indices into `path` (char offsets) covered by fuzzy matches. Empty on empty query.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// An open buffer. Identity is `buffer_id` — stable across rename / Save-As, where the
    /// `display` string would change. `dirty` is captured at row-build time and may go stale
    /// between pushes (an active picker re-pushes on dirty transitions).
    Buffer {
        buffer_id: BufferId,
        /// What the row renders: project-relative path for file-backed buffers, `[scratch N]`
        /// for scratch buffers. Also the haystack the matcher scores against.
        display: String,
        dirty: bool,
        /// Indices into `display` (char offsets) covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One match found by the grep picker. Identity is `(path, line, col)`. One row per match
    /// (a line with N matches produces N hits) — keeps `match_indices` a flat list within the
    /// preview, same as the other variants.
    GrepHit {
        /// Project-relative path of the file the match lives in (forward-slash separated).
        path: String,
        /// 0-based line number within the file.
        line: u32,
        /// 0-based byte offset of the match's first byte within the line.
        col: u32,
        /// The full text of the matching line, trimmed of its trailing newline. May be truncated
        /// at the client side to fit the picker pane.
        preview: String,
        /// Char offsets into `preview` covered by the match.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
}

// ---- picker/view --------------------------------------------------------------------------------

/// Attach to a picker, declare the scroll window to be pushed, and start receiving updates. If
/// `reset` is true, any persisted state (query, selection) is wiped first; otherwise the picker
/// resumes from whatever the prior `view`/`query`/`hide` cycle left behind. If `center_on` is
/// provided, the server picks an offset that frames the named item — this is how the client
/// restores its highlight on resume. `offset` and `center_on` are mutually exclusive —
/// `center_on` wins if both are sent.
pub struct PickerView;
impl RpcMethod for PickerView {
    const NAME: &'static str = "picker/view";
    type Params = PickerViewParams;
    type Result = PickerViewResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerViewParams {
    pub kind: PickerKind,
    /// Wipe persisted query and matcher state before attaching.
    #[serde(default)]
    pub reset: bool,
    /// First row of the window the client wants pushed. Ignored when `center_on` is set.
    #[serde(default)]
    pub offset: u32,
    pub limit: u32,
    /// If set, the server picks an `effective_offset` such that this item is inside the returned
    /// window (used on resume to restore the client's prior highlight). If the item is no longer
    /// in the results, the server falls back to `offset: 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on: Option<PickerItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerViewResult {
    /// The current query (may be empty on first open or after `reset`).
    pub query: String,
    /// Server's view of "what query generation is current." On `reset` this resets to 0; otherwise
    /// it's the generation that was active when the persisted state was saved. The client should
    /// adopt this as its `generation` baseline.
    pub generation: u64,
    /// Total candidates in the cache. May still be growing if the walker isn't done.
    pub total_candidates: u32,
    /// The offset the server actually used (matters when the client passed `center_on`). The
    /// follow-up `picker/update` push carries the same offset.
    pub effective_offset: u32,
}

// ---- picker/query -------------------------------------------------------------------------------

/// Update the active query. The client mints `generation` (monotonic per query change); the
/// server tags subsequent `picker/update` pushes with the same generation so the client can
/// discard updates from earlier queries.
pub struct PickerQuery;
impl RpcMethod for PickerQuery {
    const NAME: &'static str = "picker/query";
    type Params = PickerQueryParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerQueryParams {
    pub kind: PickerKind,
    pub query: String,
    pub generation: u64,
}

// ---- picker/select ------------------------------------------------------------------------------

/// Confirm a choice. The client sends the actual item, not an index — so there's no risk of
/// drift if results re-ranked between the user moving the highlight and pressing Enter. The
/// server acts on it (e.g. opens a buffer) and returns whatever the kind's action produces.
pub struct PickerSelect;
impl RpcMethod for PickerSelect {
    const NAME: &'static str = "picker/select";
    type Params = PickerSelectParams;
    type Result = PickerSelectResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerSelectParams {
    pub kind: PickerKind,
    pub item: PickerItem,
}

/// Per-kind action result. For `Files`, the canonical absolute path the client should open
/// (via `buffer/open`). For `Buffers`, the `buffer_id` the client should attach to (via
/// `buffer/open { buffer_id }`). For `Grep`, the canonical absolute path plus the position to
/// jump to (client opens via `buffer/open { jump_to }`). The picker handler doesn't perform the
/// switch itself — that's the client's job, same as the file browser flow.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerSelectResult {
    File {
        /// Absolute canonical path on disk.
        path: String,
    },
    Buffer {
        buffer_id: BufferId,
    },
    FileAt {
        /// Absolute canonical path on disk.
        path: String,
        /// Position to land the cursor on. Coordinates may be stale if the file changed since the
        /// hit was recorded; the server clamps in `buffer/open` when applying.
        position: LogicalPosition,
    },
}

// ---- picker/hide --------------------------------------------------------------------------------

/// Stop pushing updates for this picker. The underlying walker/matcher state stays alive so the
/// next `view` with `reset: false` resumes from where it left off. No payload — the client owns
/// the highlight and persists it locally.
pub struct PickerHide;
impl RpcMethod for PickerHide {
    const NAME: &'static str = "picker/hide";
    type Params = PickerHideParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerHideParams {
    pub kind: PickerKind,
}

// ---- picker/update (notification) ---------------------------------------------------------------

/// Server-pushed window contents. Sent whenever the subscribed window's items change (matcher
/// tick, query update applied, walker progress) or `total_matches` / `total_candidates` move.
///
/// The client discards updates whose `generation` doesn't match its latest query, and whose
/// `offset` doesn't match its current subscribed window — that handles in-flight crossover when
/// query or window changes hit the wire just before a push.
pub struct PickerUpdate;
impl NotificationMethod for PickerUpdate {
    const NAME: &'static str = "picker/update";
    type Params = PickerUpdateParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerUpdateParams {
    pub kind: PickerKind,
    pub generation: u64,
    pub offset: u32,
    pub items: Vec<PickerItem>,
    pub total_matches: u32,
    pub total_candidates: u32,
    /// True while the matcher is still consuming candidates (walk in progress, or matcher hasn't
    /// quiesced after a query change). The client may use this to show a spinner.
    pub ticking: bool,
}
