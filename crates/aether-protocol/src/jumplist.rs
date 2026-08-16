//! The jumplist — a captured, navigable snapshot of picker results (docs/jumplist.md).
//!
//! Vim-quickfix-shaped (vim's *jumplist* is a different thing — Aether's equivalent of that is
//! the nav history, [`crate::nav`], which deliberately doesn't use the term). From a supported
//! picker, `jumplist/capture` snapshots the currently-visible (filtered, ranked) result set
//! server-side; the client then shows it as the Jumplist picker (Enter jumps), and
//! `jumplist/step` walks the captured entries cursor-relative from Normal mode (`]` / `[`),
//! wrapping at the ends. The list is per-client, lives on the server (like nav history), and
//! survives until the next capture, a workspace switch, or disconnect.

use crate::buffer::BufferOpenResult;
use crate::cursor::Direction;
use crate::envelope::RpcMethod;
use crate::picker::{PickerItem, PickerKind};
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

// ---- jumplist/capture ----------------------------------------------------------------------------

/// Snapshot the open picker's filtered results into the client's jumplist. Like
/// `picker/select`, the client sends the actual highlighted item (not an index) so a re-rank
/// between highlight and keypress can't skew which entry `index` reports. The capture doesn't
/// navigate: the client follows up by opening the Jumplist picker framed on `index` (the same
/// row stays highlighted), and Enter there jumps through the ordinary select path.
///
/// Only the jump-shaped kinds capture (`PickerKind::captures_to_jumplist`) — the position-shaped
/// ones (a row is a location *in* a file) plus the file-shaped Files and Buffers (a row is a whole
/// target, captured without a position). Capturing from
/// the Jumplist picker itself replaces the list with the picker's current (typically
/// query-narrowed) subset — iterative narrowing. Returns `None` when the picker has nothing to
/// capture (empty filtered set) — the previously captured list, if any, is left untouched.
pub struct JumplistCapture;
impl RpcMethod for JumplistCapture {
    const NAME: &'static str = "jumplist/capture";
    type Params = JumplistCaptureParams;
    type Result = Option<JumplistCaptureResult>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JumplistCaptureParams {
    /// Which picker to capture from (the client's open one).
    pub kind: PickerKind,
    /// The highlighted row — reported back as `index` so the Jumplist picker can keep it
    /// highlighted.
    pub item: PickerItem,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JumplistCaptureResult {
    /// Entries captured.
    pub total: u32,
    /// 0-based position of the highlighted row within the captured list — feed into
    /// `picker/view { center_on: JumplistEntry { index } }` to keep it highlighted.
    pub index: u32,
}

// ---- jumplist/step -------------------------------------------------------------------------------

/// Step through the jumplist from the cursor's current location — Normal-mode
/// `]` / `[`. Cursor-derived: the server compares against the cursor selection's *outer* edge
/// (max edge stepping forward, min edge backward), so an entry the cursor sits on is "current"
/// and gets skipped — repeated presses always make progress. A *whole-target* entry (one captured
/// without a position, from the Files or Buffers picker) is always "current" for its own buffer, so
/// a step out of it lands on the neighbouring entry: `]`/`[` walk a captured file list one file at
/// a time. When the current file has no entries, it is virtually inserted into the list's target
/// sequence by path comparison and the step lands on the adjacent target's first/last entry; a
/// pathless buffer that isn't itself in the list steps to the first/last entry overall.
///
/// The list does **not** cycle: stepping past the last entry (forward) or before the first
/// (backward) is a no-op, reported as [`JumplistStepResult::AtEnd`] so the client can toast
/// "last/first entry" rather than silently wrapping. A `count` that overshoots clamps to the
/// boundary entry (still a move). [`JumplistStepResult::Empty`] means no list is captured.
///
/// With `scope: CurrentFile` (`}` / `{`) the walk is restricted to entries in the step
/// buffer's own file — no fall-through — and [`JumplistStepResult::NoneInFile`] reports a file
/// that has no entries at all. A whole-target entry is never an in-file step (it has no position
/// to step *to*), so a captured file list answers `NoneInFile` everywhere.
pub struct JumplistStep;
impl RpcMethod for JumplistStep {
    const NAME: &'static str = "jumplist/step";
    type Params = JumplistStepParams;
    type Result = JumplistStepResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JumplistStepParams {
    /// The buffer the cursor is in — the step origin, and the nav-history origin when `open`.
    pub buffer_id: BufferId,
    /// `Forward` = `]`, `Backward` = `[`.
    pub direction: Direction,
    /// Entries to advance (`3]`). No wrap — a count that overshoots the boundary clamps to the
    /// last/first (in-scope) entry.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
    /// Which entries the step ranges over: the whole list (`]` / `[`) or just the ones in the
    /// current buffer's file (`Alt-]` / `Alt-[`).
    #[serde(default, skip_serializing_if = "JumplistStepScope::is_full")]
    pub scope: JumplistStepScope,
    /// Also open the target — transient, jumped to the entry, nav origin recorded — and return
    /// it in `opened`: the whole `]` client chain in one round-trip.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open: bool,
}

/// The range a [`JumplistStep`] walks. `Full` (the default) crosses files — in-file first, then
/// falling through to adjacent file runs; `CurrentFile` restricts to entries in the step buffer's
/// own file (`Alt-]` / `Alt-[`), so you work one file's hits without jumping away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JumplistStepScope {
    #[default]
    Full,
    CurrentFile,
}

impl JumplistStepScope {
    fn is_full(&self) -> bool {
        matches!(self, JumplistStepScope::Full)
    }
}

/// The outcome of a [`JumplistStep`]. Internally tagged (`status`) so the moved target's fields
/// sit alongside the tag on the wire.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JumplistStepResult {
    /// Stepped to (and, with `open`, opened) the target entry. Boxed so the tag-only outcomes
    /// below don't each carry the target's footprint (the wire shape is unchanged).
    Moved(Box<JumplistStepTarget>),
    /// A list is captured but the cursor is already at the boundary in the step direction — no
    /// move. Which end is implied by the requested `direction` (forward = last, backward = first),
    /// so the client picks the toast without extra payload.
    AtEnd,
    /// `CurrentFile` scope only: a list is captured but the step buffer's file has no positioned
    /// entries in it, so there's nothing to walk. (`Full` scope never yields this — it falls
    /// through to other files.)
    NoneInFile,
    /// No list is captured (or it captured empty) — the client toasts how to capture.
    Empty,
}

impl JumplistStepResult {
    /// The moved target, or `None` for the boundary/empty outcomes — the shape most callers want.
    pub fn moved(self) -> Option<JumplistStepTarget> {
        match self {
            JumplistStepResult::Moved(t) => Some(*t),
            JumplistStepResult::AtEnd
            | JumplistStepResult::NoneInFile
            | JumplistStepResult::Empty => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JumplistStepTarget {
    /// Absolute canonical path of the target file — feed into `buffer/open` when not using
    /// the `open` composite. `None` when the entry targets a *buffer* with no path (a scratch
    /// captured from the Buffers picker), where `buffer_id` is the identity instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The target buffer's id, for the pathless entries described on `path`. `None` whenever
    /// `path` is set — exactly one of the two identifies the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Position to land the cursor on. Same semantics as `PickerSelectResult::FileAt`: for an
    /// entry carrying a span this is the span's inclusive end, with `anchor` at its start, so
    /// the jump lands the same selection the source picker's Enter would. `None` for a
    /// whole-target entry (captured from the Files or Buffers picker), which lands on the
    /// cursor position last recorded for that buffer — the top of the file if there isn't one,
    /// exactly as selecting the row in its source picker would.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<LogicalPosition>,
    /// When `Some`, the *other* end of a selection to establish — anchor here, cursor at
    /// `position` (`buffer/open { jump_to_anchor }`). `None` lands a plain point cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<LogicalPosition>,
    /// 1-based position of the target entry within the list (the status `index/total`).
    pub index: u32,
    /// Total captured entries.
    pub total: u32,
    /// With `open`: the target, fully opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<BufferOpenResult>,
}
