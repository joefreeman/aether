//! Server-stateful sneak / word-jump (the `s` / `S` motions). The server owns the per-`(client,
//! buffer)` query + candidate list and assigns the on-screen labels; the client streams the query
//! one keystroke at a time and gets back the live label set so it can tell a label keystroke (jump)
//! from a refinement keystroke (narrow). Visible labels ride along with viewport line renders as
//! [`SneakTarget`]s, exactly like search-match highlights.

use crate::cursor::CursorState;
use crate::envelope::RpcMethod;
use crate::{BufferId, ViewportId};
use serde::{Deserialize, Serialize};

// ---- sneak/update -------------------------------------------------------------------------------

/// Set (or refine) the active sneak query for a buffer. The server finds word-starts in the named
/// viewport's visible range whose text starts with `query` (smartcase), assigns single-char labels
/// drawn from an alphabet disjoint from any valid next-refinement char, stores the session, and
/// pushes refreshed viewport renders carrying the labels. An empty `query` clears the labels but
/// keeps the session armed (used right after `s`, before the first char is typed). The result's
/// `labels` is the live set of label chars so the client can classify the next keystroke; when
/// matches exceed the alphabet, as many as fit are labelled and the rest stay highlighted.
pub struct SneakUpdate;
impl RpcMethod for SneakUpdate {
    const NAME: &'static str = "sneak/update";
    type Params = SneakUpdateParams;
    type Result = SneakUpdateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SneakUpdateParams {
    pub buffer_id: BufferId,
    /// The viewport the session is bound to.
    pub viewport_id: ViewportId,
    pub query: String,
    /// The logical-line range actually on screen (`first_line`..`last_line`, last exclusive), which
    /// scopes the candidate search. The client supplies it because the server's viewport carries a
    /// full screen of overscan above/below and the native clients pixel-scroll within that window —
    /// so only the client knows what's truly visible. Labels land on words you can see.
    pub first_line: u32,
    pub last_line: u32,
    /// Match "big" words (whitespace-delimited runs, like `Alt-w`) instead of normal word-starts
    /// (`s`). Omitted on the wire when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub big: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SneakUpdateResult {
    /// The live label characters currently painted on screen. Empty only when there are no matches;
    /// when matches exceed the available alphabet, as many as fit are labelled (the overflow stays
    /// highlighted but unlabelled, reached by narrowing). A keystroke in this set means "jump";
    /// anything else extends the query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<char>,
    /// Total matching word-starts in the viewport (may exceed `labels.len()` when some overflow).
    pub match_count: u32,
}

// ---- sneak/select -------------------------------------------------------------------------------

/// Jump to the word labelled `label` and select it. With `extend`, the selection instead grows to
/// the hull spanning both the current selection and the target word (works whether the target is
/// before or after). Clears the session and pushes a refresh that removes the labels. No-op
/// (returns the unchanged cursor) if there's no session or the label is unknown.
pub struct SneakSelect;
impl RpcMethod for SneakSelect {
    const NAME: &'static str = "sneak/select";
    type Params = SneakSelectParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SneakSelectParams {
    pub buffer_id: BufferId,
    pub label: char,
    #[serde(default, skip_serializing_if = "is_false")]
    pub extend: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

// ---- sneak/cancel -------------------------------------------------------------------------------

/// Abandon the active sneak session (the `Esc` path). Clears the labels and pushes a refresh. The
/// cursor never moved during the session, so there's nothing to restore.
pub struct SneakCancel;
impl RpcMethod for SneakCancel {
    const NAME: &'static str = "sneak/cancel";
    type Params = SneakCancelParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SneakCancelParams {
    pub buffer_id: BufferId,
}

// ---- per-line target (embedded in LogicalLineRender) --------------------------------------------

/// A matched word-start on a logical line. `start`..`end` are byte offsets within the line covering
/// the word (highlighted like a search match). `start`..`prefix_end` is the typed-prefix "chip": a
/// bright run, one cell per character typed so far, that grows as the query narrows — visible
/// feedback that the entered letters still match. The client paints `label` over the chip's first
/// cell and blanks the rest. `label` is `None` (and the chip is empty, `prefix_end == start`) for an
/// overflow word — one beyond the available label alphabet — which stays highlighted but unlabelled.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SneakTarget {
    pub start: u32,
    pub end: u32,
    /// End byte offset of the typed-prefix chip (`>= start`). Equal to `start` for an unlabelled
    /// (deferred) target, which therefore shows no chip. Defaults to `0` (no chip) when absent, so a
    /// client tolerates a server predating this field.
    #[serde(default)]
    pub prefix_end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<char>,
}
