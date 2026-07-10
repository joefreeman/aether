//! Workspace-wide content search backing the Grep picker. Uses `grep-searcher` + `grep-regex`
//! (the libraries that power ripgrep) over the same `ignore::WalkBuilder` snapshot the file
//! picker walks, so gitignore / hidden-file rules stay consistent.
//!
//! Lifecycle per query: `spawn_search` kicks off an async coordinator. The coordinator launches
//! a blocking task that walks the file list and runs the matcher, batching hits and shipping
//! them back over an mpsc. The coordinator drains the channel, applies each batch to the
//! picker's candidate vec under the global lock, and emits a `picker/update` push so the client
//! sees results stream in. Cancellation is driven by the picker's `generation`: when the user
//! changes the query, `picker/query` bumps the generation, and the next batch the coordinator
//! tries to apply notices the mismatch, drops the receiver, and exits — which makes the
//! blocking task's `blocking_send` fail and abort its file walk too.

use crate::handlers::picker_update_notif;
use crate::picker::{self as picker_state, GrepHitCandidate, PickerCandidates};
use crate::state::SharedState;
use crate::workspace_index::CachedFile;
use aether_protocol::picker::{CaseMode, PickerFilters, PickerKind};
use aether_protocol::ClientId;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Minimum query length that triggers a search. Below this the picker shows an empty result set
/// and doesn't spawn a worker — typing "a" on a large repo would otherwise produce thousands of
/// hits the user is about to throw away with the next keystroke.
pub const MIN_QUERY_LEN: usize = 2;

/// Per-batch flush threshold (in hits) inside the blocking walker. Tuned so the streaming push
/// feels responsive without locking the global mutex on every match.
const BATCH_SIZE: usize = 64;

/// Channel depth between the blocking walker and the async coordinator. A few batches of
/// backpressure lets the walker stay ahead of the apply path without unbounded memory.
const CHANNEL_DEPTH: usize = 8;

/// How often, at most, a count-only progress tick is pushed while the visible window is full and
/// unchanged. The hits keep accumulating server-side every batch; the client only needs the count
/// to climb smoothly, not on every 64-hit batch (a broad query streams thousands). Window-content
/// changes and the final result still push immediately.
const COUNT_THROTTLE: Duration = Duration::from_millis(60);

/// Hard cap on a hit's preview, in chars. Without it a hit on a single-line minified file (or
/// any long line) ships the whole line per match — multiplied across a result window that's
/// enough to blow the websocket frame limit and drop the connection, and the candidate cache
/// holds every hit's preview for the search's lifetime. 256 chars comfortably overfills any
/// picker row.
const MAX_PREVIEW_CHARS: usize = 256;

/// How many chars of left context a windowed preview keeps before the match. Enough to read
/// the surroundings; small enough that the match never scrolls out of a normal picker row.
const PREVIEW_LEAD_CHARS: usize = 32;

/// Spawn an async search for `query` against the workspace. Detached: the returned future is
/// fire-and-forget. The coordinator self-terminates when the picker's generation moves past
/// `generation` (the user typed something new) or when the walker exhausts the file list.
/// `roots` are the workspace's root paths — needed when `filters` require a relaxed re-walk
/// (`+ignored`/`+hidden`) or a per-root Git status pass (`changed`).
pub fn spawn_search(
    state: SharedState,
    workspace_files: Arc<Vec<CachedFile>>,
    roots: Vec<PathBuf>,
    client_id: ClientId,
    query: String,
    filters: PickerFilters,
    generation: u64,
) {
    tokio::spawn(async move {
        if let Err(e) = run_search(
            state,
            workspace_files,
            roots,
            client_id,
            query,
            filters,
            generation,
        )
        .await
        {
            tracing::debug!(error = %e, "grep search aborted");
        }
    });
}

async fn run_search(
    state: SharedState,
    workspace_files: Arc<Vec<CachedFile>>,
    roots: Vec<PathBuf>,
    client_id: ClientId,
    query: String,
    filters: PickerFilters,
    generation: u64,
) -> Result<(), String> {
    // The query is a regex, same as buffer search (`/`), unless the `lit` filter makes it a
    // literal. Case handling defaults to smart-case; the `case` filter overrides. Invalid
    // patterns mid-typing (`foo[`, trailing `\`, etc.) are silently treated as "no matches" so
    // the picker stays responsive instead of erroring; the spawned search just emits one final
    // non-ticking, zero-hit update and exits.
    let mut builder = RegexMatcherBuilder::new();
    match filters.case {
        CaseMode::Smart => builder.case_smart(true),
        CaseMode::Sensitive => builder.case_smart(false),
        CaseMode::Insensitive => builder.case_insensitive(true),
    };
    builder
        .word(filters.whole_word)
        // Literal by default (ripgrep `-F`); the `.*` chip opts into regex.
        .fixed_strings(!filters.regex);
    let matcher = match builder.build(&query) {
        Ok(m) => m,
        Err(_) => {
            return finalize_with_no_hits(state, client_id, generation).await;
        }
    };

    // Compile the glob filter up front — an invalid glob is treated like an invalid regex
    // (zero hits; the chip stays visible so the user can see what to fix).
    let overrides = match picker_state::build_overrides(&filters.globs) {
        Ok(o) => o,
        Err(_) => {
            return finalize_with_no_hits(state, client_id, generation).await;
        }
    };

    let (batch_tx, mut batch_rx) = mpsc::channel::<Vec<GrepHitCandidate>>(CHANNEL_DEPTH);
    let files_for_blocking = workspace_files.clone();
    tokio::task::spawn_blocking(move || {
        // The shared snapshot is hidden-*inclusive* (gitignore + `.git` excluded); `FileFilter`
        // drops hidden files unless `+hidden`, so `+hidden` needs no re-walk. Only `+ignored` pulls
        // in files the snapshot never contains, so it alone triggers a one-shot relaxed walk —
        // carrying `include_hidden` through so the relaxed walk's own `.hidden` flag matches.
        let relaxed: Vec<CachedFile>;
        let files: &[CachedFile] = if filters.include_ignored {
            relaxed = crate::workspace_index::walk_with(&roots, true, filters.include_hidden);
            &relaxed
        } else {
            &files_for_blocking
        };
        let file_filter = FileFilter::new(&filters, &roots, overrides);
        walk_and_search(files, &matcher, &batch_tx, &file_filter);
        // Dropping `batch_tx` here signals end-of-stream to the coordinator.
    });

    // Throttle clock for count-only ticks (see `COUNT_THROTTLE`). Window-content changes ignore it.
    let mut last_send = Instant::now() - COUNT_THROTTLE;
    loop {
        let batch = batch_rx.recv().await;
        let mut s = state.lock().await;
        let key = (client_id, PickerKind::Grep);
        // If the picker was hidden+reopened with reset, or the user moved past us, drop out.
        let Some(picker) = s.pickers.get_mut(&key) else {
            return Ok(());
        };
        if picker.generation != generation {
            return Ok(());
        }
        match batch {
            Some(batch) => {
                let mut start_idx = 0;
                if let PickerCandidates::Grep(ref mut cands) = picker.candidates {
                    start_idx = cands.len() as u32;
                    cands.extend(batch);
                    let new_len = cands.len() as u32;
                    picker.ranked.extend(start_idx..new_len);
                }
                // Did this batch touch the subscribed window? Grep ranks in insertion order, so the
                // window only changes while it's still filling (`start_idx` falls inside it); once
                // full it's stable. An unchanged window is a count-only tick — throttle it and don't
                // re-serialize the items. (No window subscribed → treat as count-only.)
                let window_end = picker.subscribed.map_or(0, |w| w.offset + w.limit);
                let window_changed = start_idx < window_end;
                let now = Instant::now();
                if !window_changed && now.duration_since(last_send) < COUNT_THROTTLE {
                    continue; // skip this tick; the count rides the next send / the final push
                }
                last_send = now;
                let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
                let crate::state::ServerState {
                    pickers, matcher, ..
                } = &mut *s;
                let picker = pickers.get_mut(&key).expect("checked above");
                let mut update = picker_state::build_update(picker, matcher);
                if let Some(ref mut u) = update {
                    u.ticking = true;
                    if !window_changed {
                        u.items = None; // count-only tick: keep the client's (stable) window
                        u.groups.clear(); // spans describe `items`; meaningless without them
                    }
                }
                drop(s);
                if let (Some(sender), Some(params)) = (outbound, update) {
                    let _ = sender.send(picker_update_notif(params)).await;
                }
            }
            None => {
                // Walker done — emit the final, not-ticking push and mark the (query, filters)
                // pair as cached so an identical re-query short-circuits the next picker_query.
                let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
                let crate::state::ServerState {
                    pickers, matcher, ..
                } = &mut *s;
                let picker = pickers.get_mut(&key).expect("checked above");
                picker.last_completed_search = Some((picker.query.clone(), picker.filters.clone()));
                let mut update = picker_state::build_update(picker, matcher);
                if let Some(ref mut u) = update {
                    u.ticking = false;
                }
                drop(s);
                if let (Some(sender), Some(params)) = (outbound, update) {
                    let _ = sender.send(picker_update_notif(params)).await;
                }
                return Ok(());
            }
        }
    }
}

/// Emit one final non-ticking, zero-hit update for `client_id`'s grep picker, then exit. Used
/// when we bail out before running the walker — currently the invalid-regex path. Generation
/// check guards against a newer query already being in flight.
async fn finalize_with_no_hits(
    state: SharedState,
    client_id: ClientId,
    generation: u64,
) -> Result<(), String> {
    let mut s = state.lock().await;
    let key = (client_id, PickerKind::Grep);
    let Some(picker) = s.pickers.get_mut(&key) else {
        return Ok(());
    };
    if picker.generation != generation {
        return Ok(());
    }
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let crate::state::ServerState {
        pickers, matcher, ..
    } = &mut *s;
    let picker = pickers.get_mut(&key).expect("checked above");
    // Cache the invalid query too — re-typing the same broken regex shouldn't re-walk the tree.
    picker.last_completed_search = Some((picker.query.clone(), picker.filters.clone()));
    let mut update = picker_state::build_update(picker, matcher);
    if let Some(ref mut u) = update {
        u.ticking = false;
    }
    drop(s);
    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }
    Ok(())
}

/// Path-level filters applied before a file's contents are searched: root / directory scope,
/// include+exclude globs, and the changed-only restriction. Built once per search inside the
/// blocking task (the Git status pass walks the worktree).
struct FileFilter {
    /// rg `-g` semantics via `ignore::overrides`: `!`-globs exclude; with ≥1 plain glob
    /// present, non-matching files are excluded too. `None` when no globs were given.
    overrides: Option<ignore::overrides::Override>,
    /// Union of `(path_index, relative_path, is_file)` path scopes — a file passes when it's under
    /// *any* of them (matching how multiple include globs combine). A directory scope passes its
    /// subtree (empty `relative_path` = whole root); an `is_file` scope passes only that exact
    /// file. An empty vec means no scope narrowing.
    directories: Vec<(u32, String, bool)>,
    /// Per-root repo status, aligned to root index. `Some` whenever a status-dependent filter is
    /// active (`changed_only` and/or `hide_untracked`); a root outside any repo yields `None`
    /// inside. Resolved once because the worktree walk is the expensive part.
    status: Option<Vec<Option<crate::git::RepoStatus>>>,
    changed_only: bool,
    hide_untracked: bool,
    /// The shared file snapshot now *includes* hidden (dot-) files, so the Files picker can surface
    /// them. Grep's default excludes them again here; `+hidden` (`include_hidden`) turns this off.
    /// This also drops whitelisted dotfiles like `.envrc` (a `!`-rule in `.gitignore` that the
    /// `ignore` crate lets override the hidden filter) from grep's default — intentional: grep
    /// without `+hidden` means "no dotfiles", matching ripgrep's `--hidden`-off default. A relaxed
    /// `+ignored` walk sets its own `.hidden` flag, so this stays a no-op there.
    hide_hidden: bool,
}

impl FileFilter {
    fn new(
        filters: &PickerFilters,
        roots: &[PathBuf],
        overrides: Option<ignore::overrides::Override>,
    ) -> FileFilter {
        FileFilter {
            overrides,
            directories: filters
                .directories
                .iter()
                .map(|d| (d.path_index, d.relative_path.clone(), d.is_file))
                .collect(),
            status: (filters.changed_only || filters.hide_untracked).then(|| {
                roots
                    .iter()
                    .map(|r| crate::git::repo_status_for_root(r))
                    .collect()
            }),
            changed_only: filters.changed_only,
            hide_untracked: filters.hide_untracked,
            hide_hidden: !filters.include_hidden,
        }
    }

    fn passes(&self, f: &CachedFile) -> bool {
        if self.hide_hidden && crate::picker::path_is_hidden(&f.relative_path) {
            return false;
        }
        if !self.directories.is_empty()
            && !self.directories.iter().any(|(path_index, rel, is_file)| {
                crate::picker::under_scope(
                    f.path_index,
                    &f.relative_path,
                    *path_index,
                    rel,
                    *is_file,
                )
            })
        {
            return false;
        }
        if let Some(ov) = &self.overrides {
            if ov.matched(&f.relative_path, false).is_ignore() {
                return false;
            }
        }
        if let Some(statuses) = &self.status {
            let status = statuses
                .get(f.path_index as usize)
                .and_then(|rs| rs.as_ref())
                .and_then(|rs| rs.status_of(&f.relative_path));
            if self.changed_only && status.is_none() {
                return false;
            }
            if self.hide_untracked && status == Some(aether_protocol::git::GitStatus::Untracked) {
                return false;
            }
        }
        true
    }
}

/// Walk the workspace file list, running the matcher against each file's contents and shipping
/// batches of hits over `batch_tx`. Files failing `file_filter` are skipped without being
/// opened. Quits early if the receiver hangs up (the coordinator noticed a generation bump and
/// dropped it).
fn walk_and_search(
    files: &[CachedFile],
    matcher: &RegexMatcher,
    batch_tx: &mpsc::Sender<Vec<GrepHitCandidate>>,
    file_filter: &FileFilter,
) {
    // Binary detection mirrors ripgrep's CLI default (the *library* default is `none`!): bail
    // out of a file at the first NUL byte. Without it, archives like `.xlsx` get searched as
    // raw bytes — megabyte "lines" of zip data whose control characters (ESC included) would
    // otherwise reach the terminal and corrupt the display.
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut batch: Vec<GrepHitCandidate> = Vec::with_capacity(BATCH_SIZE);
    for f in files {
        if !file_filter.passes(f) {
            continue;
        }
        if batch_tx.is_closed() {
            return;
        }
        let mut sink = HitCollector {
            matcher,
            path_index: f.path_index,
            relative_path: &f.relative_path,
            abs_path: &f.abs,
            batch: &mut batch,
            batch_tx,
            errored: false,
        };
        // Ignore per-file errors (binary file, permission denied, etc.) — they shouldn't poison
        // the whole walk.
        let _ = searcher.search_path(matcher, std::path::Path::new(&f.abs), &mut sink);
        if sink.errored {
            // Receiver closed mid-file; bail out.
            return;
        }
    }
    if !batch.is_empty() {
        let _ = batch_tx.blocking_send(batch);
    }
}

/// `grep-searcher` sink that converts each matching line into one `GrepHitCandidate` per match
/// position on that line. Holds a `&mut Vec` we own; when it grows past `BATCH_SIZE` we ship it.
struct HitCollector<'a> {
    matcher: &'a RegexMatcher,
    path_index: u32,
    relative_path: &'a str,
    abs_path: &'a str,
    batch: &'a mut Vec<GrepHitCandidate>,
    batch_tx: &'a mpsc::Sender<Vec<GrepHitCandidate>>,
    /// Set when a `blocking_send` fails — we then return `Ok(false)` to halt further searching
    /// of this file. `walk_and_search` checks this to bail out of the whole walk.
    errored: bool,
}

impl<'a> Sink for HitCollector<'a> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let line_bytes = mat.bytes();
        // `line_number` is 1-based; we use 0-based internally.
        let line_num = mat.line_number().unwrap_or(1).saturating_sub(1) as u32;
        let preview = preview_from_line(line_bytes);
        let preview_chars = preview.chars().count();

        // Walk every match on this line — one row per match.
        let mut hits_on_line: Vec<(u32, usize, usize)> = Vec::new();
        let _ = self.matcher.find_iter(line_bytes, |m| {
            // Filter zero-width matches (e.g. user-provided pattern like `^`) to avoid emitting
            // a row at every position.
            if m.end() > m.start() {
                hits_on_line.push((m.start() as u32, m.start(), m.end()));
            }
            true
        });

        for (col, byte_start, byte_end) in hits_on_line {
            let match_chars = byte_range_to_char_offsets(&preview, byte_start, byte_end);
            // Each hit gets a *bounded* window of the line around its own match — `col` stays
            // the real byte column (jumps are unaffected; the preview is display-only).
            let (hit_preview, match_indices) =
                windowed_preview(&preview, preview_chars, match_chars);
            self.batch.push(GrepHitCandidate {
                path_index: self.path_index,
                relative_path: self.relative_path.to_string(),
                abs_path: self.abs_path.to_string(),
                line: line_num,
                col,
                match_byte_len: (byte_end - byte_start) as u32,
                preview: hit_preview,
                match_indices,
            });
        }

        if self.batch.len() >= BATCH_SIZE {
            let to_send = std::mem::take(self.batch);
            if self.batch_tx.blocking_send(to_send).is_err() {
                self.errored = true;
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Strip the trailing newline (if any) from a line read by `grep-searcher` and produce a
/// displayable string. Non-UTF-8 bytes are replaced with U+FFFD — practical for source-tree
/// search where the rare invalid byte shouldn't crash the picker. ASCII control bytes (tabs
/// included) become spaces: an embedded ESC would otherwise reach the terminal as a live
/// escape sequence and corrupt the display, and a raw tab breaks the row's width math. The
/// replacement is byte-for-byte, so match byte offsets into the raw line stay valid against
/// the preview.
fn preview_from_line(line: &[u8]) -> String {
    let trimmed = if line.ends_with(b"\n") {
        let cut = if line.ends_with(b"\r\n") {
            line.len() - 2
        } else {
            line.len() - 1
        };
        &line[..cut]
    } else {
        line
    };
    let sanitized: Vec<u8> = trimmed
        .iter()
        .map(|&b| if b < 0x20 || b == 0x7f { b' ' } else { b })
        .collect();
    String::from_utf8_lossy(&sanitized).into_owned()
}

/// Bound a hit's preview to a window of [`MAX_PREVIEW_CHARS`] around its match: the window
/// opens [`PREVIEW_LEAD_CHARS`] before the match's first char (clamped so the window stays
/// full-size at the line's tail), and a leading `…` marks a left cut. `match_chars` are the
/// match's char offsets into the full `preview`; the returned offsets index the window. Lines
/// already within the cap pass through untouched.
fn windowed_preview(
    preview: &str,
    preview_chars: usize,
    match_chars: Vec<u32>,
) -> (String, Vec<u32>) {
    if preview_chars <= MAX_PREVIEW_CHARS {
        return (preview.to_string(), match_chars);
    }
    let first = match_chars.first().copied().unwrap_or(0) as usize;
    let start = first
        .saturating_sub(PREVIEW_LEAD_CHARS)
        .min(preview_chars - MAX_PREVIEW_CHARS);
    let cut_left = start > 0;
    let mut out = String::new();
    if cut_left {
        out.push('…');
    }
    out.extend(preview.chars().skip(start).take(MAX_PREVIEW_CHARS));
    let shift = usize::from(cut_left);
    let indices = match_chars
        .into_iter()
        .filter_map(|i| {
            let i = i as usize;
            (i >= start && i < start + MAX_PREVIEW_CHARS).then(|| (i - start + shift) as u32)
        })
        .collect();
    (out, indices)
}

/// Convert a byte range within `preview` to the char-index offsets the match covers. Char
/// offsets are what the protocol's `match_indices` field uses (consistent with the existing
/// Files/Buffers pickers).
fn byte_range_to_char_offsets(preview: &str, byte_start: usize, byte_end: usize) -> Vec<u32> {
    let mut out = Vec::new();
    for (ci, (bo, _)) in preview.char_indices().enumerate() {
        if bo >= byte_end {
            break;
        }
        if bo >= byte_start {
            out.push(ci as u32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_strips_trailing_newline() {
        assert_eq!(preview_from_line(b"hello\n"), "hello");
        assert_eq!(preview_from_line(b"hello\r\n"), "hello");
        assert_eq!(preview_from_line(b"no-newline"), "no-newline");
    }

    #[test]
    fn preview_sanitizes_control_bytes() {
        // ESC must never reach the terminal as a live escape sequence; tabs would break the
        // row's width math. Both become spaces, byte-for-byte (offsets stay valid).
        assert_eq!(preview_from_line(b"a\x1b[31mred\tx\n"), "a [31mred x");
        assert_eq!(preview_from_line(b"nul\x00mid\x7fend"), "nul mid end");
    }

    #[test]
    fn windowed_preview_passes_short_lines_through() {
        let (p, idx) = windowed_preview("let foo = 1;", 12, vec![4, 5, 6]);
        assert_eq!(p, "let foo = 1;");
        assert_eq!(idx, vec![4, 5, 6]);
    }

    #[test]
    fn windowed_preview_caps_long_lines_around_the_match() {
        // A match deep inside a long line: the window opens PREVIEW_LEAD_CHARS before it and
        // a leading `…` marks the cut; indices shift into the window.
        let long: String = "x".repeat(1000);
        let mut line = long.clone();
        line.replace_range(500..506, "needle");
        let match_chars: Vec<u32> = (500..506).collect();
        let (p, idx) = windowed_preview(&line, 1000, match_chars);
        assert_eq!(
            p.chars().count(),
            MAX_PREVIEW_CHARS + 1,
            "window + ellipsis"
        );
        assert!(p.starts_with('…'));
        let start = 500 - PREVIEW_LEAD_CHARS;
        let expected: Vec<u32> = (500..506).map(|i| (i - start + 1) as u32).collect();
        assert_eq!(idx, expected);
        // The match chars land on "needle" within the window.
        let chars: Vec<char> = p.chars().collect();
        let shown: String = idx.iter().map(|&i| chars[i as usize]).collect();
        assert_eq!(shown, "needle");
    }

    #[test]
    fn windowed_preview_clamps_at_line_start_and_tail() {
        let long: String = "y".repeat(1000);
        // Match at the very start: no left cut, plain prefix window.
        let (p, idx) = windowed_preview(&long, 1000, vec![0, 1]);
        assert_eq!(p.chars().count(), MAX_PREVIEW_CHARS);
        assert!(!p.starts_with('…'));
        assert_eq!(idx, vec![0, 1]);
        // Match at the very end: the window clamps so it stays full-size at the tail and the
        // match remains inside it.
        let (p, idx) = windowed_preview(&long, 1000, vec![998, 999]);
        assert_eq!(p.chars().count(), MAX_PREVIEW_CHARS + 1);
        let start = 1000 - MAX_PREVIEW_CHARS;
        assert_eq!(
            idx,
            vec![(998 - start + 1) as u32, (999 - start + 1) as u32]
        );
        let max_idx = *idx.iter().max().unwrap() as usize;
        assert!(
            max_idx < p.chars().count(),
            "indices stay inside the window"
        );
    }

    #[test]
    fn byte_range_to_char_offsets_ascii() {
        // "let foo = 1;", match "foo" at bytes 4..7.
        let offsets = byte_range_to_char_offsets("let foo = 1;", 4, 7);
        assert_eq!(offsets, vec![4, 5, 6]);
    }

    #[test]
    fn byte_range_to_char_offsets_multibyte() {
        // "λ foo", λ is 2 bytes (chars 0..1 = 'λ', char 1 = ' ', chars 2..5 = "foo").
        // Match "foo" at bytes 3..6 → char offsets 2, 3, 4.
        let offsets = byte_range_to_char_offsets("λ foo", 3, 6);
        assert_eq!(offsets, vec![2, 3, 4]);
    }
}
