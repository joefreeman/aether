//! Input history (docs/input-history.md) — the recall list behind `Up`/`Down` in the overlay
//! text inputs: the buffer-search prompt, the grep picker's query, and the glob / path filter-chip
//! editors.
//!
//! Scoped per workspace (a grep term or a path scope is workspace vocabulary) and owned by the
//! server, which persists it to `history.json` and hands each client a snapshot on connect and on
//! every workspace switch. Like hints (docs/hints.md) the server is a dumb aggregator: clients
//! append committed values (`history/record`) and do all the *navigation* — cursor, stashed draft,
//! restore-on-overshoot — locally, so recall never waits on a round-trip.

use crate::envelope::RpcMethod;
use crate::picker::{MatchOptions, PickerFilters};
use serde::{Deserialize, Serialize};

/// Entries kept per list. Old entries fall off the front once a list exceeds this.
pub const HISTORY_MAX: usize = 100;

/// Which input's recall list. One list per *field*, not per overlay: the buffer search and the
/// grep query draw from different corpora (one buffer's words vs the workspace's), and a glob is
/// never a valid path, so mixing them would just make `Up` unpredictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    /// The buffer-search prompt (`/`, `?`), recorded on commit.
    Search,
    /// The grep picker's query, recorded when the picker closes (grep searches per keystroke, so
    /// recording on change would store every prefix).
    Grep,
    /// The glob filter chip (`Alt-g`), recorded when the chip editor commits.
    Glob,
    /// The directory / file path filter chip (`Alt-p`), recorded when the chip editor commits.
    /// The entry is the *path segment* as typed — root-relative, without the root: in a multi-root
    /// workspace the root is a separate typeahead field with its own fixed candidate set, so a
    /// recalled path resolves under whichever root the editor currently has, exactly as if it had
    /// been typed there.
    Path,
}

/// One recalled value together with the configuration it ran under. Recall restores both: a regex
/// query recalled without its `regex` flag isn't merely styled differently, it matches nothing, and
/// the same reasoning extends to the scope a search was run in — an entry reproduces the search you
/// actually did, chips and all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub value: String,
    /// The filter set in effect when this was recorded. Grep records its whole chip row; the
    /// buffer search has no scoping, so it populates only the match-option fields (read back with
    /// [`PickerFilters::match_options`]); the glob and path lists leave it default — for them the
    /// value *is* the configuration. All-default is skipped on the wire, so those lists stay a
    /// plain list of strings in `history.json`.
    #[serde(default, skip_serializing_if = "PickerFilters::is_default")]
    pub filters: PickerFilters,
}

impl HistoryEntry {
    /// An entry with no configuration attached — the glob and path lists.
    pub fn bare(value: impl Into<String>) -> Self {
        HistoryEntry {
            value: value.into(),
            filters: PickerFilters::default(),
        }
    }

    /// An entry carrying only match options — the buffer search, which has no scoping filters.
    pub fn with_options(value: impl Into<String>, options: MatchOptions) -> Self {
        HistoryEntry {
            value: value.into(),
            filters: PickerFilters::from_match_options(options),
        }
    }
}

/// One recall list per [`HistoryKind`], oldest first. Named fields rather than a map keyed by the
/// enum so `history.json` stays legible and a list added later parses forward against old files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryLists {
    #[serde(default)]
    pub search: Vec<HistoryEntry>,
    #[serde(default)]
    pub grep: Vec<HistoryEntry>,
    #[serde(default)]
    pub glob: Vec<HistoryEntry>,
    #[serde(default)]
    pub path: Vec<HistoryEntry>,
}

impl HistoryLists {
    pub fn get(&self, kind: HistoryKind) -> &Vec<HistoryEntry> {
        match kind {
            HistoryKind::Search => &self.search,
            HistoryKind::Grep => &self.grep,
            HistoryKind::Glob => &self.glob,
            HistoryKind::Path => &self.path,
        }
    }

    pub fn get_mut(&mut self, kind: HistoryKind) -> &mut Vec<HistoryEntry> {
        match kind {
            HistoryKind::Search => &mut self.search,
            HistoryKind::Grep => &mut self.grep,
            HistoryKind::Glob => &mut self.glob,
            HistoryKind::Path => &mut self.path,
        }
    }

    /// Append a committed entry as the newest, and trim from the front at [`HISTORY_MAX`].
    /// Empty values are dropped, and a value already in the list *moves* rather than repeats — the
    /// list is walked one entry per keypress, so stepping over the same value twice is pure
    /// friction (vim's `:history` rule, not the shell's keep-every-line one).
    ///
    /// Identity is the **value alone**, not value-plus-filters: two entries reading `needle` would
    /// be indistinguishable while walking, so re-running a term under different filters *updates*
    /// its remembered configuration rather than adding a twin. Returns whether anything changed —
    /// `false` for an empty value or an exact repeat of the newest entry — which is how the client
    /// skips the `history/record` round-trip and the server skips the disk write.
    ///
    /// Shared (like the hints constants) so the client's local list and the server's persisted one
    /// can't drift: both sides apply this exact rule, so a client that records locally ends up
    /// with the list the next `history/state` will hand back.
    pub fn record(&mut self, kind: HistoryKind, entry: HistoryEntry) -> bool {
        if entry.value.is_empty() {
            return false;
        }
        let list = self.get_mut(kind);
        if list.last() == Some(&entry) {
            return false;
        }
        list.retain(|e| e.value != entry.value);
        list.push(entry);
        let overflow = list.len().saturating_sub(HISTORY_MAX);
        if overflow > 0 {
            list.drain(..overflow);
        }
        true
    }
}

/// Fetch the active workspace's recall lists. Called on connect (alongside `settings/get` and
/// `hints/state`) and again after every workspace switch, since the lists are workspace-scoped.
/// Returns empty lists when the client has no workspace active yet (the boot chooser) or its
/// workspace is ephemeral — a "(no workspace)" context has no identity to file history under.
pub struct HistoryState;
impl RpcMethod for HistoryState {
    const NAME: &'static str = "history/state";
    type Params = HistoryStateParams;
    type Result = HistoryStateResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HistoryStateParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStateResult {
    #[serde(default)]
    pub lists: HistoryLists,
}

/// Append one committed value to the active workspace's list. Fire-and-forget: the client has
/// already applied the same [`HistoryLists::record`] rule locally, so the reply carries nothing
/// and a failure costs only cross-window/next-restart visibility of that one entry.
pub struct HistoryRecord;
impl RpcMethod for HistoryRecord {
    const NAME: &'static str = "history/record";
    type Params = HistoryRecordParams;
    type Result = HistoryRecordResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecordParams {
    pub kind: HistoryKind,
    #[serde(flatten)]
    pub entry: HistoryEntry,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HistoryRecordResult {}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(list: &[HistoryEntry]) -> Vec<&str> {
        list.iter().map(|e| e.value.as_str()).collect()
    }

    #[test]
    fn record_keeps_one_entry_per_value_ordered_by_recency() {
        let mut lists = HistoryLists::default();
        assert!(lists.record(HistoryKind::Grep, HistoryEntry::bare("foo")));
        // A repeat of the newest entry changes nothing — no traffic, no write.
        assert!(!lists.record(HistoryKind::Grep, HistoryEntry::bare("foo")));
        assert!(lists.record(HistoryKind::Grep, HistoryEntry::bare("bar")));
        // Re-using an older value moves it to newest rather than duplicating it, so walking the
        // list never steps over the same value twice.
        assert!(lists.record(HistoryKind::Grep, HistoryEntry::bare("foo")));
        assert_eq!(values(&lists.grep), ["bar", "foo"]);
        // Empty values never enter, and the lists are independent.
        assert!(!lists.record(HistoryKind::Grep, HistoryEntry::bare("")));
        assert!(lists.search.is_empty());
    }

    /// Identity is the value: the same term under new filters updates the entry in place rather
    /// than adding a second row that looks identical while walking.
    #[test]
    fn re_recording_a_value_updates_its_filters_without_duplicating() {
        let regex = MatchOptions {
            regex: true,
            ..Default::default()
        };
        let mut lists = HistoryLists::default();
        assert!(lists.record(HistoryKind::Search, HistoryEntry::bare("f.o")));
        assert!(lists.record(
            HistoryKind::Search,
            HistoryEntry::with_options("f.o", regex)
        ));
        assert_eq!(values(&lists.search), ["f.o"]);
        assert_eq!(lists.search[0].filters.match_options(), regex);
        // Exactly the same value *and* filters is the no-op case.
        assert!(!lists.record(
            HistoryKind::Search,
            HistoryEntry::with_options("f.o", regex)
        ));
    }

    #[test]
    fn record_trims_the_oldest_entries_at_the_cap() {
        let mut lists = HistoryLists::default();
        for i in 0..HISTORY_MAX + 10 {
            assert!(lists.record(HistoryKind::Path, HistoryEntry::bare(format!("p{i}"))));
        }
        assert_eq!(lists.path.len(), HISTORY_MAX);
        assert_eq!(lists.path.first().unwrap().value, "p10");
        assert_eq!(
            lists.path.last().unwrap().value,
            format!("p{}", HISTORY_MAX + 9)
        );
    }
}
