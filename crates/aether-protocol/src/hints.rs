//! Hint learning state (docs/hints.md). The client observes bindings being used and
//! reports increment-shaped events (`hints/record`); the server aggregates them into per-hint
//! records, stamps days, derives retirement, and persists the result (`hints.json`, app-global
//! like `settings.toml`). Clients fetch one snapshot on connect (`hints/state`). The hint
//! *definitions* (curriculum, copy, contexts) live client-side with the keymap — the wire carries
//! only opaque hint ids.

use crate::envelope::RpcMethod;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Uses required to retire a hint ("learned"), together with [`LEARNED_DAYS`]. Shared between the
/// server (which stamps days and derives retirement authoritatively) and the client (which derives
/// it live from its own counters so retirement doesn't wait on a round-trip).
pub const LEARNED_USES: u32 = 3;

/// Distinct calendar days those uses must span — one burst of presses isn't "learned". The cheap
/// substitute for spaced repetition (docs/hints.md §1.5).
pub const LEARNED_DAYS: u32 = 2;

/// How much an explicit dismissal (`Space h`) adds to a hint's shows-without-follow fatigue
/// counter — a deliberate "not now" outweighs a display period that merely lapsed (which adds 1).
/// Shared so the server's fold-in and the client's optimistic mirror agree.
pub const DISMISS_WEIGHT: f32 = 2.0;

/// A wall-clock instant reduced to a UTC day number (days since the Unix epoch). Both sides
/// compute days through this so "distinct day" means the same thing everywhere; the server's
/// stamp is authoritative on apply.
pub fn day_from_unix_ms(unix_ms: u64) -> u32 {
    (unix_ms / 86_400_000) as u32
}

/// Learning state for one *unretired* hint. Retired hints collapse to a bare id in the
/// [`HintsStateResult::retired`] list — no record survives retirement (deliberate: docs/hints.md
/// §1.8). Every field has a serde default so records written by older builds parse forward.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HintRecord {
    /// Times the hint's trigger fired, saturating at [`LEARNED_USES`].
    #[serde(default)]
    pub uses: u32,
    /// Distinct calendar days with at least one use, saturating at [`LEARNED_DAYS`].
    #[serde(default)]
    pub use_days: u32,
    /// The UTC day number ([`day_from_unix_ms`]) of the most recent use. `0` = never used.
    #[serde(default)]
    pub last_used_day: u32,
    /// Unix ms of the most recent use. `0` = never used. Drives the recency down-rank.
    #[serde(default)]
    pub last_used_at: u64,
    /// Fatigue counter: display periods that ended without the hint being followed, decayed
    /// exponentially (fractional after decay). A follow resets it to zero.
    #[serde(default)]
    pub shows_without_follow: f32,
    /// Unix ms the hint last started a display period. `0` = never shown. Anchors the fatigue
    /// decay.
    #[serde(default)]
    pub last_shown_at: u64,
}

/// One increment reported by a client observing the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintEvent {
    /// The hint began a display period while the user was present.
    Shown,
    /// The hint's trigger fired (the binding was used) while the hint was *not* on screen.
    Used,
    /// The trigger fired while its hint was on screen — a use that also resets fatigue.
    Followed,
    /// The user explicitly dismissed the displayed hint (`Space h`): fatigue jumps by
    /// [`DISMISS_WEIGHT`] instead of the lapsed-display 1.
    Dismissed,
}

/// Report one hint event. Fire-and-forget in spirit (the client ignores failures); the result
/// carries the server's retirement verdict so the client can stop observing without re-deriving.
pub struct HintsRecord;
impl RpcMethod for HintsRecord {
    const NAME: &'static str = "hints/record";
    type Params = HintsRecordParams;
    type Result = HintsRecordResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintsRecordParams {
    /// Curriculum id of the hint (an opaque string on the wire).
    pub hint_id: String,
    pub event: HintEvent,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HintsRecordResult {
    /// True when this event crossed the retirement threshold — the hint is now learned and the
    /// client should drop its matcher.
    #[serde(default)]
    pub retired: bool,
}

/// Fetch the full learning-state snapshot. Called once per connection, alongside `settings/get`;
/// concurrent windows drift until their next connect (accepted — docs/hints.md §1.8).
pub struct HintsState;
impl RpcMethod for HintsState {
    const NAME: &'static str = "hints/state";
    type Params = HintsStateParams;
    type Result = HintsStateResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HintsStateParams {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HintsStateResult {
    /// Ids of learned hints — permanently out of the pool, no record kept.
    #[serde(default)]
    pub retired: Vec<String>,
    /// Per-hint learning records, keyed by hint id.
    #[serde(default)]
    pub active: BTreeMap<String, HintRecord>,
}
