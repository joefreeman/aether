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
use aether_protocol::picker::PickerKind;
use aether_protocol::ClientId;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use std::sync::Arc;
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

/// Spawn an async search for `query` against the workspace. Detached: the returned future is
/// fire-and-forget. The coordinator self-terminates when the picker's generation moves past
/// `generation` (the user typed something new) or when the walker exhausts the file list.
pub fn spawn_search(
    state: SharedState,
    workspace_files: Arc<Vec<CachedFile>>,
    client_id: ClientId,
    query: String,
    generation: u64,
) {
    tokio::spawn(async move {
        if let Err(e) = run_search(state, workspace_files, client_id, query, generation).await {
            tracing::debug!(error = %e, "grep search aborted");
        }
    });
}

async fn run_search(
    state: SharedState,
    workspace_files: Arc<Vec<CachedFile>>,
    client_id: ClientId,
    query: String,
    generation: u64,
) -> Result<(), String> {
    // Smart-case regex matching — the query is a regex, same as buffer search (`/`). Invalid
    // patterns mid-typing (`foo[`, trailing `\`, etc.) are silently treated as "no matches" so
    // the picker stays responsive instead of erroring; the spawned search just emits one final
    // non-ticking, zero-hit update and exits.
    let matcher = match RegexMatcherBuilder::new().case_smart(true).build(&query) {
        Ok(m) => m,
        Err(_) => {
            return finalize_with_no_hits(state, client_id, generation).await;
        }
    };

    let (batch_tx, mut batch_rx) = mpsc::channel::<Vec<GrepHitCandidate>>(CHANNEL_DEPTH);
    let files_for_blocking = workspace_files.clone();
    tokio::task::spawn_blocking(move || {
        walk_and_search(&files_for_blocking, &matcher, &batch_tx);
        // Dropping `batch_tx` here signals end-of-stream to the coordinator.
    });

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
                if let PickerCandidates::Grep(ref mut cands) = picker.candidates {
                    let start_idx = cands.len() as u32;
                    cands.extend(batch);
                    let new_len = cands.len() as u32;
                    picker.ranked.extend(start_idx..new_len);
                }
                let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
                let crate::state::ServerState {
                    pickers, matcher, ..
                } = &mut *s;
                let picker = pickers.get_mut(&key).expect("checked above");
                let mut update = picker_state::build_update(picker, matcher);
                if let Some(ref mut u) = update {
                    u.ticking = true;
                }
                drop(s);
                if let (Some(sender), Some(params)) = (outbound, update) {
                    let _ = sender.send(picker_update_notif(params)).await;
                }
            }
            None => {
                // Walker done — emit the final, not-ticking push and mark the query as cached
                // so a re-query with the same string short-circuits the next picker_query call.
                let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
                let crate::state::ServerState {
                    pickers, matcher, ..
                } = &mut *s;
                let picker = pickers.get_mut(&key).expect("checked above");
                picker.last_completed_query = Some(picker.query.clone());
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
    picker.last_completed_query = Some(picker.query.clone());
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

/// Walk the workspace file list, running the matcher against each file's contents and shipping
/// batches of hits over `batch_tx`. Quits early if the receiver hangs up (the coordinator
/// noticed a generation bump and dropped it).
fn walk_and_search(
    files: &[CachedFile],
    matcher: &RegexMatcher,
    batch_tx: &mpsc::Sender<Vec<GrepHitCandidate>>,
) {
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut batch: Vec<GrepHitCandidate> = Vec::with_capacity(BATCH_SIZE);
    for f in files {
        if batch_tx.is_closed() {
            return;
        }
        let mut sink = HitCollector {
            matcher,
            display_path: &f.display,
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
    display_path: &'a str,
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
            let match_indices = byte_range_to_char_offsets(&preview, byte_start, byte_end);
            self.batch.push(GrepHitCandidate {
                display_path: self.display_path.to_string(),
                abs_path: self.abs_path.to_string(),
                line: line_num,
                col,
                match_byte_len: (byte_end - byte_start) as u32,
                preview: preview.clone(),
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
/// search where the rare invalid byte shouldn't crash the picker.
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
    String::from_utf8_lossy(trimmed).into_owned()
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
