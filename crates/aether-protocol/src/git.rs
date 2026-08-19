//! Git messages.
//!
//! Blame is request/response and cursor-driven: the client asks for the blame of the line its
//! cursor sits on (whenever that line changes) and renders the answer as end-of-line virtual
//! text. The server computes blame against the live buffer (folding in unsaved edits), so a line
//! the user just typed reports as uncommitted rather than misattributing to the previous author.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::ViewportWindowResult;
use crate::{BufferId, ViewportId};
use serde::{Deserialize, Serialize};

// ---- git/navigate_hunk --------------------------------------------------------------------------

pub struct GitNavigateHunk;
impl RpcMethod for GitNavigateHunk {
    const NAME: &'static str = "git/navigate_hunk";
    type Params = GitNavigateHunkParams;
    type Result = GitNavigateHunkResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitNavigateHunkParams {
    pub buffer_id: BufferId,
    /// The cursor's current 0-based line; the search for the next/previous changed region starts
    /// from here.
    pub from_line: u32,
    pub direction: HunkDirection,
    /// How many hunks to skip in `direction`. Defaults to 1; when fewer than `count` remain the
    /// cursor lands on the furthest reachable hunk rather than not moving at all.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
    /// Grow the selection to the landing hunk (Shift) rather than collapsing to a point there: the
    /// anchor is kept and the cursor jumps to the hunk.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkDirection {
    Next,
    Prev,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitNavigateHunkResult {
    /// Cursor after the jump. Equal to the incoming cursor when `moved` is false.
    pub cursor: CursorState,
    /// False when there's no hunk in the requested direction (cursor unchanged).
    pub moved: bool,
}

// ---- change counts (status-bar summary) ---------------------------------------------------------

/// Per-class Git change counts: how many buffer lines fall into each change class for one diff.
/// `added` / `modified` count the new-side lines of Added / Modified hunks; `deleted` counts the
/// lines removed by pure deletions. A clean file (or one with no repo / untracked) reports all
/// zeros. Used as the staged/unstaged halves of [`GitBufferStatus`] (the status bar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChangeCounts {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

impl GitChangeCounts {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.modified == 0 && self.deleted == 0
    }
}

/// How far the current branch has diverged from its configured upstream: commits HEAD has that the
/// upstream doesn't (`ahead`), and commits the upstream has that HEAD doesn't (`behind`).
///
/// **A local ref comparison, not a network read.** It answers "how did things stand as of the last
/// fetch" — nothing here contacts a remote, and a repo that has never been fetched reports zeros
/// however far the remote has moved. That's why the status bar shows the numbers unqualified: they
/// are exactly what `git status` would say in the same working directory at the same moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitUpstreamStatus {
    /// The upstream ref's shorthand, e.g. `origin/main`. Worth carrying rather than assuming
    /// `origin`: a fork workflow tracks `upstream/main`, and a divergence count is misleading
    /// without knowing what it's counting against.
    pub name: String,
    pub ahead: u32,
    pub behind: u32,
}

impl GitUpstreamStatus {
    /// True when HEAD and the upstream are level. Distinct from *absent*: `None` means there is no
    /// upstream to compare with (detached, unborn, or a never-pushed branch), which is a different
    /// thing to say in the status bar than "in sync".
    pub fn is_level(&self) -> bool {
        self.ahead == 0 && self.behind == 0
    }
}

/// Buffer-level Git status for the status bar: the branch, and the change counts split into staged
/// (HEAD → index) and unstaged (index → working buffer). `Some` for any file inside a repo (the
/// counts are zero for a clean / untracked file); `None` outside a repo. The counts here match
/// `git diff --cached` (staged) and `git diff` (unstaged), so the status bar agrees with the
/// terminal. Distinct from the per-line gutter markers, which the viewport window carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBufferStatus {
    /// Branch name, or a short commit hash when HEAD is detached. `None` only when it can't be
    /// resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Staged changes: HEAD → index (`git diff --cached`).
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub staged: GitChangeCounts,
    /// Unstaged changes: index → working buffer (`git diff`).
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub unstaged: GitChangeCounts,
    /// Divergence from the branch's upstream, or `None` when there is no upstream to compare
    /// against — detached HEAD, an unborn branch, or a branch that has never been pushed. See
    /// [`GitUpstreamStatus`]: local refs only, so it reflects the last fetch rather than the
    /// remote's current state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<GitUpstreamStatus>,
    /// Set when the repo is diffed against something other than HEAD ([`GitSetBaseline`]). The
    /// gutter then means "changed since this commit" and `staged` is always empty, so clients
    /// must surface this — an unexplained gutter that disagrees with `git diff` is worse than no
    /// gutter at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<GitBaselineRef>,
    /// A multi-step git operation the repo is stopped in the middle of, if any.
    ///
    /// Surfaced for the same reason `baseline` is: the editor is showing a state that doesn't match
    /// the user's mental model and must say so. A conflicted `pull --rebase` leaves HEAD *detached*,
    /// so without this the status bar silently swaps the branch name for a bare hash and drops the
    /// ahead/behind arrows (there is no upstream to compare a detached HEAD against) — the whole git
    /// surface degrades with no explanation, and the only thing that ever said "rebase" was a toast
    /// that has since faded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<GitRepoOperation>,
}

/// A multi-step git operation a repo is stopped part-way through — what `.git/MERGE_HEAD`,
/// `.git/rebase-merge` and friends record, read via libgit2's `Repository::state`.
///
/// Distinct from [`GitOperation`], which is something *we* are running right now: this is a
/// persistent property of the repo that outlives the process that created it, and it is just as
/// likely to have been started from a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRepoOperation {
    Merge,
    /// Every rebase flavour — plain, interactive and merge-backend — folded together. The
    /// distinction changes how git resumes, and nothing the editor does with this cares.
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    /// `git am`: applying a mailbox of patches.
    ApplyMailbox,
}

impl GitRepoOperation {
    /// Present-continuous label for the status bar — "merging", "rebasing". Lower case: it renders
    /// as an aside next to the branch (`main (rebasing)`), not as a heading.
    pub fn label(self) -> &'static str {
        match self {
            GitRepoOperation::Merge => "merging",
            GitRepoOperation::Rebase => "rebasing",
            GitRepoOperation::CherryPick => "cherry-picking",
            GitRepoOperation::Revert => "reverting",
            GitRepoOperation::Bisect => "bisecting",
            GitRepoOperation::ApplyMailbox => "applying",
        }
    }
}

// ---- git/set_diff_view --------------------------------------------------------------------------

pub struct GitSetDiffView;
impl RpcMethod for GitSetDiffView {
    const NAME: &'static str = "git/set_diff_view";
    type Params = GitSetDiffViewParams;
    /// The freshly re-rendered window: toggling the diff view changes which virtual rows exist and
    /// therefore the visual-row layout and `max_scroll`, so the whole window is resent (like
    /// `viewport/set_wrap`).
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitSetDiffViewParams {
    pub viewport_id: ViewportId,
    pub enabled: bool,
}

// ---- git/apply_hunk -----------------------------------------------------------------------------

/// Toggle the staged state of — or revert — the change under the cursor or the selected lines.
/// Cursor-relative like the input commands: the server resolves the client's cursor/selection,
/// so no positions ride the wire. A bare cursor (anchor == position) addresses the whole hunk it
/// sits on (a pure deletion belongs to the line its phantom rows render above, or the last line
/// at end-of-buffer); a wider selection is snapped to whole lines and taken at line granularity.
///
/// Toggle writes the repository index and requires a non-dirty buffer (the index must not hold
/// content that exists nowhere on disk — the client tells the user to save first). Revert is an
/// ordinary buffer edit through the undo stack and works on a dirty buffer. The result's
/// [`ApplyHunkStatus`] reports which direction a toggle resolved to.
pub struct GitApplyHunk;
impl RpcMethod for GitApplyHunk {
    const NAME: &'static str = "git/apply_hunk";
    type Params = GitApplyHunkParams;
    type Result = GitApplyHunkResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitApplyHunkParams {
    pub buffer_id: BufferId,
    pub action: HunkAction,
    /// Which region the action applies to. Defaults to [`ApplyScope::Cursor`] — the shape the
    /// method is named for. The method name predates the file scope; the *edit* is identical
    /// either way (index ← buffer for a stage, baseline → buffer for a revert), only the region
    /// differs, which is why this is a parameter rather than a second RPC.
    #[serde(default, skip_serializing_if = "ApplyScope::is_cursor")]
    pub scope: ApplyScope,
}

/// The region [`GitApplyHunk`] acts on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyScope {
    /// Resolve from the client's cursor: a bare cursor addresses the hunk it sits on, a wider
    /// selection the lines it covers.
    #[default]
    Cursor,
    /// The whole file, wherever the cursor is — `git add <file>` / `git restore --staged <file>`
    /// in one keystroke, which is the more common gesture than picking off hunks. Toggling
    /// resolves its direction over the whole file, unstaged-first, exactly as it does for a hunk:
    /// anything unstaged stages, and a file with nothing unstaged unstages entirely.
    File,
}

impl ApplyScope {
    pub fn is_cursor(&self) -> bool {
        matches!(self, ApplyScope::Cursor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkAction {
    /// Flip the addressed change's staged state, unstaged-first (mirroring `Revert`'s layering):
    /// anything unstaged in the region is staged (index ← buffer, `git add -p`-style); when the
    /// region holds nothing unstaged, its staged change is pulled back out (index ← HEAD). The
    /// region's stage is visible in the combined view's colours, so the direction is readable
    /// before pressing — and reported back via [`ApplyHunkStatus`] after.
    Toggle,
    /// Restore baseline content in the buffer for the addressed change (undoable edit). Peels the
    /// top layer of the H→I→B change stack: an unstaged change reverts to the index's content;
    /// a staged-only region (buffer == index ≠ HEAD) reverts to HEAD's — pressing again on a
    /// re-modified region therefore peels unstaged first, then staged. View-independent, like
    /// `Toggle`.
    Revert,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitApplyHunkResult {
    /// Cursor after the action — unchanged for a toggle, clamped into the edited text for
    /// revert. Always echoed so the client can adopt it unconditionally (mirrors `lsp/format`).
    pub cursor: CursorState,
    pub status: ApplyHunkStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyHunkStatus {
    /// A toggle staged the region's unstaged change(s).
    Staged,
    /// A toggle pulled the region's staged change(s) back out of the index.
    Unstaged,
    /// A revert restored baseline content in the buffer.
    Reverted,
    /// No matching change under the cursor / in the selection, in either direction.
    NoChange,
    /// Toggle refused because the buffer has unsaved edits — save first.
    DirtyBuffer,
    /// The buffer isn't in a Git repository (or the index write failed).
    Unavailable,
    /// Staging refused because the repo is diffed against a revision rather than HEAD
    /// ([`GitSetBaseline`]): these hunks have no index relationship to stage into. Reverting
    /// still works. Restore the HEAD baseline to stage.
    NotAgainstHead,
    /// Staging refused because this file is left conflicted by a merge or rebase.
    ///
    /// Not a limitation but a guard against a silent surprise. A conflicted path has no stage-0
    /// index entry, so writing one is `git add`'s "mark resolved" — the user would press *stage
    /// this hunk* and get *resolve this file*, with the other side's changes and possibly the
    /// conflict markers themselves committed to the index. The staged/unstaged split is meaningless
    /// here anyway (see `git.rs`), so the gutter can't show what they'd be acting on.
    Conflicted,
}

// ---- git/set_blame_follow -----------------------------------------------------------------------

/// Toggle server-driven cursor-line blame for one buffer. While enabled, the server watches this
/// client's cursor on the buffer and — after a short settle window — pushes [`GitBlameChanged`]
/// whenever the settled cursor line's blame differs from the last push. This replaces the old
/// client-polled label flow (a `git/blame_line` request per cursor move): the cursor is
/// server-authoritative, so a per-move request told the server nothing it didn't already know.
///
/// The toggle is client-owned because the display gating is modal state the server doesn't have:
/// clients enable it in blame-displaying contexts (Normal mode, file-backed buffer) and disable
/// it on leaving them, which is also what spares the server whole-file blame recomputes during
/// Insert-mode typing. [`GitBlameLine`] remains for the on-demand commit-details popover.
pub struct GitSetBlameFollow;
impl RpcMethod for GitSetBlameFollow {
    const NAME: &'static str = "git/set_blame_follow";
    type Params = GitSetBlameFollowParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitSetBlameFollowParams {
    pub buffer_id: BufferId,
    pub enabled: bool,
}

// ---- git/repos ----------------------------------------------------------------------------------

/// Identity of one Git repository: its canonicalized working directory, as an absolute path.
///
/// A path rather than a server-assigned token so it survives a server restart (a workspace's
/// repo choice persists as something still meaningful next boot), so any code path holding a
/// path can address a repo without a [`GitRepos`] round trip first, and so it reads in wire
/// logs. The server validates incoming ids against the repos it can actually reach and rejects
/// anything else, so clients should treat it as opaque and echo back what [`GitRepos`] gave them.
///
/// Keyed on the **working directory**, not the git dir: two workspace roots inside one repo
/// collapse to one id, while two linked worktrees of the same repo stay distinct — they have
/// separate HEADs and indexes, so checkout and commit mean different things in each. Repos
/// sharing a [`GitRepoInfo::common_dir`] are worktrees of one another.
pub type RepoId = String;

/// The distinct repos reachable from the client's active workspace — via its roots, and via any
/// buffer it has open. The set a repo chooser lists, and the source of every valid [`RepoId`].
pub struct GitRepos;
impl RpcMethod for GitRepos {
    const NAME: &'static str = "git/repos";
    type Params = GitReposParams;
    type Result = GitReposResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitReposParams {}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitReposResult {
    /// Repos reached through a workspace root first (in root order), then repos reached only
    /// through an open buffer (by id). Empty when the workspace touches no repo at all.
    pub repos: Vec<GitRepoInfo>,
}

/// One repo reachable from the active workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepoInfo {
    pub repo_id: RepoId,
    /// This worktree's git dir — `<repo_id>/.git` for an ordinary checkout, but a path inside the
    /// main repo's `.git/worktrees/` for a linked worktree (whose own `.git` is a *file*).
    pub git_dir: String,
    /// The shared object and ref store. Equal to `git_dir` for an ordinary checkout; rows sharing
    /// a `common_dir` are worktrees of one repository, so they see the same branches, tags and
    /// stash entries while keeping their own HEAD and index.
    pub common_dir: String,
    pub head: GitHead,
    /// Active-workspace roots this repo was reached through, as absolute paths. **Empty means the
    /// repo was reached only through an open buffer** — a dependency checkout a goto-definition
    /// landed in, say. Those stay fully readable (diff, blame, log) but are not mutation targets
    /// without the user explicitly saying so, which is what stops a commit going somewhere the
    /// user never opened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

/// Where a repo's HEAD points. Three cases rather than one branch-name string, because they admit
/// different operations: an unborn HEAD can be committed to but has nothing to push without
/// `--set-upstream`, and moving off a detached HEAD abandons commits unless they're on a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitHead {
    /// On a branch with at least one commit.
    Branch {
        name: String,
        /// Configured upstream (`origin/main`), or `None` for a branch that has never been pushed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
    },
    /// Detached: HEAD names a commit directly. `oid` is the short hash.
    Detached { oid: String },
    /// A fresh repo whose branch has no commit yet. `name` is the branch HEAD will create.
    Unborn { name: String },
}

// ---- git/prepare_commit + git/commit -------------------------------------------------------------

/// Write the commit-message template to the repo's `COMMIT_EDITMSG` and report where it is.
///
/// The message is composed in an ordinary buffer on an ordinary file, so the whole editor works on
/// it: undo, search, the jumplist, syntax highlighting. That's why this is two RPCs rather than a
/// `git/commit { message }` — the alternative is reimplementing a text editor inside a dialog, in
/// an editor.
///
/// `docs/todo.md` originally proposed doing this through `GIT_EDITOR` and the tether. That path
/// still exists and is still wanted for operations git itself must drive (`rebase -i` via
/// `GIT_SEQUENCE_EDITOR`), but for a plain commit it spawns a second client process to write a
/// message in the client the user is already sitting in.
pub struct GitPrepareCommit;
impl RpcMethod for GitPrepareCommit {
    const NAME: &'static str = "git/prepare_commit";
    type Params = GitPrepareCommitParams;
    type Result = GitPrepareCommitResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitPrepareCommitParams {
    /// Which repo to commit to. Omit to let the server resolve it — from `buffer_id`'s repo, or
    /// from the workspace when it holds exactly one. Resolution lives server-side because that's
    /// where the buffer→repo mapping already is; a client that had to work it out would need
    /// `git/repos` plus a copy of the rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    /// The buffer the user is looking at, as the resolution hint. Ignored when `repo_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Amend the previous commit: the template is prefilled with its message, and the commit that
    /// follows must pass the same flag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub amend: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPrepareCommitResult {
    /// The repo this resolved to. Echoed so the follow-up [`GitCommit`] targets exactly what was
    /// prepared, even if the user has moved to another buffer in the meantime.
    pub repo_id: RepoId,
    /// Absolute path of the message file to open. Lives in the repo's *own* git dir, so a linked
    /// worktree composes its message independently of the main checkout.
    pub path: String,
    /// What would be committed, for the client to show — and to decide whether it's worth opening
    /// a buffer at all. Empty with `amend: false` means `git commit` would refuse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged: Vec<StagedFile>,
}

/// One path in the index, with the word git would use for it in `git status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFile {
    /// Repo-relative, forward-slash.
    pub path: String,
    /// `new file`, `modified`, `deleted`, `renamed`, `typechange` — git's own vocabulary, so the
    /// comment block reads like `git status` and clients need no mapping table.
    pub status: String,
}

/// Commit what's staged, using the message left in `COMMIT_EDITMSG` by [`GitPrepareCommit`].
///
/// Runs the real `git commit`, so hooks fire (`pre-commit`, `commit-msg`) and signing works —
/// the reason writes shell out at all. `--cleanup=strip` drops the comment lines, and an empty
/// message aborts, exactly as it would in a terminal.
///
/// A refusal is **not** an RPC error: a failing `pre-commit` hook is an ordinary, expected outcome
/// whose output the user needs to read. It comes back as `commit: None` with git's own stderr.
pub struct GitCommit;
impl RpcMethod for GitCommit {
    const NAME: &'static str = "git/commit";
    type Params = GitCommitParams;
    type Result = GitCommitResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitCommitParams {
    pub repo_id: RepoId,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub amend: bool,
}

#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommitResult {
    /// The commit that was created. `None` when git refused — see `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitInfo>,
    /// git's own output when it refused, verbatim and unparsed: a hook's complaint, "nothing to
    /// commit", "empty commit message". Empty on success. Show it as a terminal would.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// The message was empty (comments and blank lines only), so git was never run. Git's own rule
    /// — *"Aborting commit due to empty commit message"* — surfaced as its own field rather than as
    /// a refusal, because the two mean opposite things to the client: an empty message is the user
    /// changing their mind (close the buffer, say nothing much), a refusal is a hook objecting
    /// (keep the message, let them fix it and retry).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub empty_message: bool,
    /// The reconciliation that followed. Rarely interesting for a commit — but `pre-commit` hooks
    /// routinely rewrite files (formatters), and those buffers have to be picked up.
    #[serde(default, skip_serializing_if = "GitRefreshResult::is_empty")]
    pub refreshed: GitRefreshResult,
}

// ---- git/reset -----------------------------------------------------------------------------------

/// Move HEAD to another commit, keeping the index and working tree exactly as they are
/// (`git reset --soft`).
///
/// **Soft only, deliberately.** A soft reset touches no file: the commits it unwinds come back as
/// staged changes, ready to be recommitted. `--mixed` and `--hard` rewrite the index and working
/// tree, which needs the dirty-buffer pre-flight described in `docs/git-phase-2.md` decision 3 —
/// enumerate what would be lost, refuse or stash, never silently discard. That isn't built, so
/// this doesn't pretend to offer it.
///
/// Generic in `rev` rather than a bare "uncommit" so a log picker's "reset to this commit" is the
/// same call with a different revision. `HEAD^` is the uncommit case, and the common one: wrong
/// message, forgotten file.
pub struct GitReset;
impl RpcMethod for GitReset {
    const NAME: &'static str = "git/reset";
    type Params = GitResetParams;
    type Result = GitResetResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitResetParams {
    /// Omit to let the server resolve it, exactly as [`GitPrepareCommit`] does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    /// The buffer the user is looking at, as the resolution hint. Ignored when `repo_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Where HEAD should end up. Anything `git rev-parse` accepts; `HEAD^` to uncommit.
    pub rev: String,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitResetResult {
    /// The commit HEAD now points at, once it moved. `None` when git refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<CommitInfo>,
    /// The commits that were unwound, newest first — what the user just took back, so the client
    /// can name it ("Uncommitted: Add a line") rather than reporting a hash movement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undone: Vec<CommitInfo>,
    /// git's own output when it refused, verbatim. Empty on success.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

// ---- git/checkout -------------------------------------------------------------------------------

/// Switch the repo's working tree to another branch — optionally creating it first
/// (`git checkout -b`).
///
/// Creating rides this RPC rather than getting its own because `git checkout -b` is the *same edit
/// shape*: the tree may move, reconciliation follows, and the refusal surface is identical.
/// Deleting a branch is a different shape entirely (no tree move, nothing to reconcile) and gets
/// [`GitDeleteBranch`].
///
/// **Buffers with unsaved edits block the switch**, and this is the reason the RPC needs a
/// pre-flight at all. Git only knows about files on disk: it refuses a checkout that would clobber
/// a *modified file*, but an unsaved buffer is invisible to it, so git would cheerfully rewrite the
/// file underneath and strand the user's edits on top of the wrong base. The pre-flight enumerates
/// them and refuses before git runs — nothing is stashed, saved or discarded on the user's behalf.
pub struct GitCheckout;
impl RpcMethod for GitCheckout {
    const NAME: &'static str = "git/checkout";
    type Params = GitCheckoutParams;
    type Result = GitCheckoutResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitCheckoutParams {
    /// Omit to let the server resolve it, exactly as [`GitPrepareCommit`] does. Clients that got
    /// the branch from the picker should send the `repo_id` the *row* carried: resolution runs off
    /// the active buffer, which may have moved since the list was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    /// The buffer the user is looking at, as the resolution hint. Ignored when `repo_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Branch to switch to — or, with `create`, the name to create at HEAD.
    pub branch: String,
    /// `git checkout -b`: create `branch` at the current HEAD, then switch to it. Also the way to
    /// get a first branch in a repo whose HEAD is unborn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub create: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCheckoutResult {
    pub status: GitCheckoutStatus,
    /// Where HEAD ended up. `None` unless the switch happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<GitHead>,
    /// With [`GitCheckoutStatus::BlockedByDirtyBuffers`]: the buffers that stopped it. Git was
    /// never run, so the working tree is exactly as it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<BufferId>,
    /// Context for the refusing statuses: the worktree path for `AlreadyCheckedOut`, and git's own
    /// stderr — verbatim and unparsed — for `Refused`. Empty on success.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// The reconciliation that followed a successful switch: which open buffers were re-read, which
    /// diverged, and which files the new ref doesn't have.
    #[serde(default, skip_serializing_if = "GitRefreshResult::is_empty")]
    pub refreshed: GitRefreshResult,
}

/// How a [`GitCheckout`] resolved.
///
/// A discriminated outcome rather than a bare `head: Option` plus a message, following
/// [`ApplyHunkStatus`]: the client picks a different message and a different follow-up for each,
/// and inferring that from which fields happen to be populated is how those get out of step.
///
/// Every variant here is one the *server* determines — from its own buffer state, or from a
/// libgit2 read. Git's stderr is never parsed into these; whatever only git knows arrives as
/// [`Self::Refused`] with its text intact (`docs/git-phase-2.md` decision 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitCheckoutStatus {
    /// HEAD moved to an existing branch.
    #[default]
    Switched,
    /// The branch was created and switched to (`create: true`).
    Created,
    /// Refused before running git: buffers in this repo hold unsaved edits (`blocked`).
    BlockedByDirtyBuffers,
    /// Refused before running git: another worktree of this repo already has the branch checked
    /// out, and git permits that in only one. `message` names the worktree.
    AlreadyCheckedOut,
    /// Git refused. `message` is its stderr — an unmerged path, a would-be-overwritten untracked
    /// file, a name that isn't a valid ref. Show it as a terminal would.
    Refused,
}

// ---- git/delete_branch --------------------------------------------------------------------------

/// Delete a local branch (`git branch -d`, or `-D` with `force`).
///
/// Separate from [`GitCheckout`] rather than folded in as a mode: nothing moves in the working
/// tree, so there is no reconciliation, no watcher suppression and no dirty-buffer pre-flight —
/// none of checkout's machinery applies.
pub struct GitDeleteBranch;
impl RpcMethod for GitDeleteBranch {
    const NAME: &'static str = "git/delete_branch";
    type Params = GitDeleteBranchParams;
    type Result = GitDeleteBranchResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitDeleteBranchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    pub branch: String,
    /// `git branch -D`: delete even though the branch isn't merged. The client is expected to
    /// reach this only by escalating from a [`GitDeleteBranchStatus::NotMerged`] refusal, so the
    /// user has seen what they're discarding.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDeleteBranchResult {
    pub status: GitDeleteBranchStatus,
    /// git's own output when it refused, verbatim. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// How a [`GitDeleteBranch`] resolved. Same discipline as [`GitCheckoutStatus`]: the discriminated
/// cases come from libgit2 reads the server makes itself, never from matching git's wording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDeleteBranchStatus {
    #[default]
    Deleted,
    /// The branch holds commits that are not reachable from HEAD — deleting it loses them. A
    /// merge-base check, not a reading of git's complaint, which is what lets the client offer
    /// `force` as a specific escalation rather than pattern-matching stderr.
    NotMerged,
    /// It's the branch currently checked out here. Distinguished because it's the likeliest
    /// mistake and "switch away first" is more useful than git's phrasing.
    IsCurrentBranch,
    /// Git refused for some other reason; `message` is its stderr.
    Refused,
}

// ---- git/fetch -----------------------------------------------------------------------------------

/// How often the server re-fetches when [`crate::settings::AppSettings::git_auto_fetch`] is on.
///
/// A fixed cadence rather than a configurable one, per CLAUDE.md's preference for deciding in code:
/// far enough apart that the network cost is invisible and a credential prompt can't become a
/// drumbeat, close enough that "3 behind" is a fact about now rather than about this morning.
/// Lives in the protocol crate so the number has one home even though only the server acts on it.
pub const AUTO_FETCH_INTERVAL_MINUTES: u64 = 15;

/// Fetch from the remote — update `refs/remotes/**` so the divergence counts
/// ([`GitUpstreamStatus`]) reflect the remote's current state.
///
/// **The one mutation that doesn't move the working tree.** It writes remote-tracking refs and
/// objects, never HEAD, the index or a file, so it skips the whole dirty-buffer pre-flight that
/// checkout, stash and pull need: there is nothing it could clobber. Everything it changes is
/// invisible until the user asks for it.
///
/// Plain `git fetch` — the current branch's remote, or `origin`. Not `--all` (an unattended fetch
/// of every remote in a fork workflow is a surprise), and not `--prune` (deleting local
/// remote-tracking refs is a decision, not a refresh).
///
/// Reachability-gated like the other mutations: a repo reached only through an open buffer — a
/// dependency checkout a goto-definition landed in — is refused. Not because fetching would harm
/// it, but because the user never opened it and this reaches the network on their behalf.
pub struct GitFetch;
impl RpcMethod for GitFetch {
    const NAME: &'static str = "git/fetch";
    type Params = GitFetchParams;
    type Result = GitFetchResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitFetchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFetchResult {
    pub status: GitFetchStatus,
    /// git's own output when it refused, verbatim. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Divergence **after** the fetch, so the caller can report what arrived ("3 behind") without
    /// a second round trip. `None` when the branch has no upstream to compare against — which is
    /// not the same as level, and a fetch is exactly the moment that distinction gets interesting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<GitUpstreamStatus>,
}

/// How a [`GitFetch`] resolved. Same discipline as [`GitCheckoutStatus`]: every discriminated
/// variant is one the *server* determined from its own reads, never from matching git's wording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFetchStatus {
    #[default]
    Fetched,
    /// The repo has no remote configured. Asked of libgit2 before spawning rather than read out of
    /// git's complaint — and worth distinguishing, because it's the one failure that will never
    /// succeed on retry, which is what stops the periodic fetcher hammering a local-only repo.
    NoRemote,
    /// Git failed; `message` is its stderr. Network down, credentials refused, host unknown — all
    /// deliberately one variant, because the client's response to each is the same (show git's own
    /// words) and telling them apart would mean parsing them.
    Refused,
    /// The user stopped it ([`GitCancel`]). Distinguished from a failure because it isn't one, and
    /// git's stderr on a killed process reads like a crash.
    Cancelled,
}

// ---- git/push ------------------------------------------------------------------------------------

/// Publish the current branch's commits to its remote — the other half of the `↑ahead` count.
///
/// Like [`GitFetch`] and unlike checkout or stash, this never touches the working tree, so it needs
/// no unsaved-work pre-flight. It *does* move `refs/remotes/**` on success, which is what makes the
/// arrows drop back to level without a fetch.
///
/// **A never-pushed branch is pushed with `--set-upstream`**, so the divergence counts start
/// working from then on. That's not a convenience: a branch with no upstream has nothing to be
/// ahead *of*, so until it has one the status bar can't say anything about it at all.
///
/// Never force-pushes. There is no parameter for it and the keymap deliberately leaves `Alt-p`
/// free rather than making it the force variant.
pub struct GitPush;
impl RpcMethod for GitPush {
    const NAME: &'static str = "git/push";
    type Params = GitPushParams;
    type Result = GitPushResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitPushParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPushResult {
    pub status: GitPushStatus,
    /// git's own output when it refused, verbatim. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Divergence as it stands *after* the attempt — level after a successful push, and still
    /// showing the gap after a [`GitPushStatus::Behind`] refusal, which is what lets the client
    /// say how far behind without asking again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<GitUpstreamStatus>,
    /// True when this push established the tracking relationship (`--set-upstream`). Reported
    /// rather than inferred: it's a one-time event worth naming in the toast, and the client can't
    /// reliably know the branch had no upstream a moment ago.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub set_upstream: bool,
}

/// How a [`GitPush`] resolved. Every discriminated variant comes from the server's own libgit2
/// reads — including [`Self::Behind`], which is the interesting one: see its note.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitPushStatus {
    #[default]
    Pushed,
    /// The branch has an upstream and is level with it. Asked before spawning, like the stash's
    /// "nothing to stash" — reporting a push that pushed nothing is a small lie, and this saves a
    /// pointless round trip to the remote.
    NothingToPush,
    /// The push was rejected because the branch is behind its upstream — the fast-forward rule.
    ///
    /// **Classified after the fact, not pre-flighted.** Refusing before spawning would look
    /// cheaper, but the behind count comes from *local* refs, so a stale one would refuse a push
    /// that git would have accepted (the remote was reset since the last fetch). Letting git decide
    /// and then asking libgit2 *why* keeps the good message without inventing a refusal: git really
    /// did say no, and the reason is still our own read rather than a parse of its wording.
    Behind,
    /// HEAD is detached, so there is no branch to push.
    DetachedHead,
    /// The repo has no remote configured.
    NoRemote,
    /// The branch has no upstream and the repo has several remotes, so which one to publish to is
    /// a choice, not a default. Refused rather than guessed: pushing a branch to the wrong remote
    /// in a fork workflow means publishing work somewhere it wasn't meant to go.
    AmbiguousRemote,
    /// Git refused for some other reason; `message` is its stderr. Authentication, a pre-receive
    /// hook, a protected branch — all one variant, because the client's response to each is the
    /// same (show git's own words) and telling them apart would mean parsing them.
    Refused,
    /// The user stopped it ([`GitCancel`]). Distinguished from a failure because it isn't one, and
    /// git's stderr on a killed process reads like a crash.
    Cancelled,
}

// ---- git/pull ------------------------------------------------------------------------------------

/// Bring the current branch up to date with its upstream — the third network operation, and the
/// only one that moves the working tree.
///
/// **This is where the two halves of the git support meet.** [`GitFetch`] and [`GitPush`] are pure
/// network operations and inherit none of checkout's machinery; [`GitCheckout`] rewrites the tree
/// and inherits all of it. A pull is both, so it carries the dirty-buffer pre-flight, the watcher
/// suppression and the reconciliation pass *and* the progress indicator and cancellation.
///
/// **Plain `git pull`** — the user's own `pull.rebase`, `pull.ff`, `rebase.autoStash` and
/// `merge.conflictstyle` decide what happens, exactly as they would in a terminal. That is
/// `docs/git-phase-2.md` decision 1 applied to a strategy rather than to hooks: an editor that
/// forced `--ff-only` would refuse where the user's own git would have rebased. The cost is that a
/// pull can leave the tree mid-merge, which is what [`GitPullStatus::Conflicted`] exists to report.
///
/// Nothing here is pre-flighted that git could decide better. Uncommitted changes on disk, a merge
/// that would overwrite them, a diverged branch with no configured strategy — all left to git, and
/// classified afterwards from our own reads. The four refusals below it *does* answer up front are
/// the ones knowable without the network that no retry could fix.
pub struct GitPull;
impl RpcMethod for GitPull {
    const NAME: &'static str = "git/pull";
    type Params = GitPullParams;
    type Result = GitPullResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitPullParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPullResult {
    pub status: GitPullStatus,
    /// git's own output when it refused, verbatim. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Divergence as it stands *after* the attempt — level after a clean pull, and still showing
    /// the gap after a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<GitUpstreamStatus>,
    /// With [`GitPullStatus::BlockedByDirtyBuffers`]: the buffers that stopped it. Git was never
    /// run, so the working tree is exactly as it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<BufferId>,
    /// The reconciliation that followed. Reported on *every* outcome that ran git, including the
    /// failures: a pull that stopped at a conflict has already rewritten files, and a buffer left
    /// showing pre-merge content would be the worst possible moment to be stale.
    #[serde(default, skip_serializing_if = "GitRefreshResult::is_empty")]
    pub refreshed: GitRefreshResult,
    /// Repo-relative paths left conflicted, read from the index rather than from git's narration.
    /// Populated for [`GitPullStatus::Conflicted`] and for the
    /// [`GitPullStatus::OperationInProgress`] refusal, which is usually the same conflict seen on a
    /// later attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// What the repo is stopped in the middle of. Set on
    /// [`GitPullStatus::OperationInProgress`] (which is *why* it refused) and on
    /// [`GitPullStatus::Conflicted`] (which merge or rebase just stopped), so the client can name
    /// the operation rather than guessing from the user's config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<GitRepoOperation>,
    /// `.git/index.lock` was still present after a cancelled pull.
    ///
    /// Cancelling SIGKILLs git. Fetch and push never write the index so this could not arise for
    /// them, but a pull's merge half does, and a lock left behind makes *every* subsequent git
    /// operation fail until it is removed. Reported and never removed for us: we know we killed a
    /// git, not that we killed *the* holder of this lock, and deleting a live lock corrupts the
    /// index of whatever does hold it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub index_locked: bool,
}

/// How a [`GitPull`] resolved.
///
/// The three success variants are told apart by comparing HEAD before and after — a libgit2 read of
/// our own, never a parse of git's summary line. They are worth distinguishing because they are
/// three different things to have happened to the user's history: nothing moved, the branch caught
/// up, a merge commit appeared, or their commits were rewritten onto new bases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitPullStatus {
    /// HEAD did not move: already up to date.
    #[default]
    UpToDate,
    /// HEAD moved straight onto the upstream tip — no local commits were in the way.
    FastForwarded,
    /// A merge commit was created: the new HEAD has the old one as a parent alongside the upstream.
    Merged,
    /// Local commits were replayed onto the upstream (`pull.rebase`). Recognised by the old HEAD no
    /// longer being an ancestor of the new one, which is precisely what rewriting means.
    Rebased,
    /// The merge or rebase stopped with conflicts; `conflicts` names the files. **The tree has
    /// moved** and the repo is left mid-operation, exactly as a terminal would leave it.
    ///
    /// Aether has no conflict resolution yet (`docs/git-phase-2.md` stage 6), so this reports the
    /// state rather than offering to fix it. It is still a distinct outcome and not a
    /// [`Self::Refused`]: the user's next step is to edit the marked files, not to read git's
    /// stderr and work out what happened.
    Conflicted,
    /// The branch and its upstream have both moved and git declined to guess how to reconcile them
    /// (no `pull.rebase` or `pull.ff` configured — git's own refusal since 2.27).
    ///
    /// **Classified after the fact**, the same trick and for the same reason as
    /// [`GitPushStatus::Behind`]: whether it applies depends on the *user's config*, which the
    /// server would have to read and re-implement git's precedence rules to predict. Letting git
    /// decide and then asking libgit2 whether we are diverged keeps the good message without
    /// inventing a refusal. Nothing moved.
    Diverged,
    /// The repo is already stopped part-way through a merge, rebase, cherry-pick or bisect
    /// ([`GitRepoOperation`]), so there is nothing sensible to pull into. Pre-flighted.
    ///
    /// **This is the answer a conflicted `pull --rebase` needs on the *next* attempt**, and getting
    /// there needs this check to run before [`Self::DetachedHead`] — a stopped rebase detaches HEAD,
    /// so the detached-head refusal would otherwise fire first and report the symptom ("not on a
    /// branch") in place of the cause. A stopped *merge* keeps HEAD on its branch, which is why that
    /// case appeared to work: it reached git, git refused, and the index conflicts were still there
    /// to classify. This covers both, and also the case with no conflicts left — everything
    /// resolved and staged but never committed — which nothing else detects.
    OperationInProgress,
    /// The branch has no upstream, so there is nothing to pull from. Pre-flighted: git's own
    /// "there is no tracking information for the current branch" costs a process spawn to learn
    /// something libgit2 already knows, and the fix — push first, which sets one — is worth naming.
    /// An unborn HEAD folds in here, having no tracking information either.
    NoUpstream,
    /// HEAD is detached, so there is no branch to bring up to date.
    DetachedHead,
    /// The repo has no remote configured.
    NoRemote,
    /// Open buffers in this repo hold unsaved edits; `blocked` lists them. Refused before anything
    /// ran, the same pre-flight [`GitCheckout`] makes and for the same reason — git guards files on
    /// disk and cannot see an unsaved buffer, so it would merge underneath one and leave the user's
    /// edits sitting on a base that no longer exists.
    BlockedByDirtyBuffers,
    /// Git refused for some other reason; `message` is its stderr. Local changes that would be
    /// overwritten, authentication, an unreachable host — one variant, because the client's
    /// response to each is the same (show git's own words).
    Refused,
    /// The user stopped it ([`GitCancel`]). A cancel during the fetch half leaves nothing behind; a
    /// cancel during the merge half can leave the repo mid-operation, which is why `refreshed` is
    /// reported here too.
    Cancelled,
}

// ---- long-running operations ---------------------------------------------------------------------

/// What long-running git operation a repo is currently running, if any.
///
/// Only **user-initiated** operations are announced. The periodic fetcher deliberately says nothing
/// — a spinner appearing every quarter of an hour is the interruption a background refresh exists
/// to avoid, and there is nobody waiting on it to inform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitOperation {
    pub kind: GitOperationKind,
    /// git's most recent progress line, verbatim (`Writing objects:  47% (8/17)`), or empty before
    /// it has said anything. Passed through rather than parsed into a percentage: git's phrasing
    /// already names the phase, and the phases differ per operation and version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitOperationKind {
    Fetch,
    Push,
    Pull,
}

impl GitOperationKind {
    /// Present-continuous label for the status bar — "Fetching", "Pushing", "Pulling".
    pub fn label(self) -> &'static str {
        match self {
            GitOperationKind::Fetch => "Fetching",
            GitOperationKind::Push => "Pushing",
            GitOperationKind::Pull => "Pulling",
        }
    }
}

/// Pushed to every client when a repo starts, advances through, or finishes a long-running git
/// operation. `operation: None` means it has finished — the client clears its indicator without
/// needing to know how it ended, which it learns from the RPC result it is already waiting on.
pub struct GitOperationChanged;
impl NotificationMethod for GitOperationChanged {
    const NAME: &'static str = "git/operation_changed";
    type Params = GitOperationChangedParams;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOperationChangedParams {
    pub repo_id: RepoId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<GitOperation>,
}

/// Stop the long-running git operation running in `repo_id`.
///
/// The reason this exists at all: a push to an unreachable host sits in a TCP connect timeout for
/// minutes, and without this the only way out is to quit the editor. Killing git mid-transfer is
/// safe — a partial push is simply not applied, and a partial fetch leaves unreferenced objects
/// that git's own gc collects.
pub struct GitCancel;
impl RpcMethod for GitCancel {
    const NAME: &'static str = "git/cancel";
    type Params = GitCancelParams;
    type Result = GitCancelResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitCancelParams {
    pub repo_id: RepoId,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCancelResult {
    /// False when nothing was running — the operation finished between the keystroke and this
    /// arriving, which is a race the client should treat as success rather than an error.
    pub cancelled: bool,
}

// ---- git/set_baseline ---------------------------------------------------------------------------

/// Diff a repo against a revision other than HEAD — "what have I changed since I branched?",
/// "what did this file look like at v1.0?".
///
/// Repo-scoped rather than per-buffer: the question is about a body of work, not one file, and
/// having the answer change as you move between files would make the gutter meaningless. Every
/// buffer in the repo re-diffs, and the whole existing stack — gutter, hunk navigation, inline
/// diff, revert — follows without knowing anything changed.
///
/// The staged/unstaged distinction does not survive: there is no index relationship to an
/// arbitrary commit, so the entire change set reads as unstaged and staging is refused
/// ([`ApplyHunkStatus::NotAgainstHead`]). Reverting still means something — restore this hunk to
/// how it was at that commit — and still works.
pub struct GitSetBaseline;
impl RpcMethod for GitSetBaseline {
    const NAME: &'static str = "git/set_baseline";
    type Params = GitSetBaselineParams;
    type Result = GitSetBaselineResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitSetBaselineParams {
    pub repo_id: RepoId,
    /// Anything `git rev-parse` accepts: a branch, tag, hash, `HEAD~3`. `None` restores HEAD.
    /// An unresolvable revision is an error, not a silent fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSetBaselineResult {
    /// The baseline now in force, or `None` when it's back to HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<GitBaselineRef>,
    /// Buffers whose diff was recomputed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buffers: Vec<BufferId>,
}

/// A pinned non-HEAD diff baseline: what the user asked for, and what it resolved to.
///
/// Pinned at set time rather than re-resolved per file. `git diff main` re-resolves, but a gutter
/// is ambient — having it shift because someone pushed to `main` while you were reading is worse
/// than it going slightly stale. `label` is carried so the UI can say `main` rather than a hash
/// the user never typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBaselineRef {
    pub label: String,
    pub commit: String,
}

// ---- git/show ------------------------------------------------------------------------------------

/// Materialise a revision as a read-only **virtual buffer** — a buffer with no file behind it
/// (docs/git-phase-2.md decision 4). Two shapes, one call:
///
/// - **no `path`** — the commit itself: its metadata and message, then the patch against its first
///   parent (against the empty tree for a root commit), which is what `git show` prints.
/// - **with `path`** — that file's content as of the commit, i.e. `git show <rev>:<path>`. The
///   language is detected from the path, so it highlights like the working-tree file does.
///
/// One RPC rather than two because the shape is identical — a revision, materialised read-only —
/// and only the narrowing differs. The server owns the content: there is deliberately no way for a
/// client to seed a buffer with arbitrary text.
///
/// Repeat calls for the same `(repo, rev, path)` return the **same buffer** rather than stacking
/// duplicates, so re-selecting a row in the log picker lands where you were. The buffer opens
/// transient (it's a preview, so it auto-closes once hidden); `Space k` pins it, and since a
/// read-only buffer can never be promoted by an edit or a save, that's the only promotion there is.
pub struct GitShow;
impl RpcMethod for GitShow {
    const NAME: &'static str = "git/show";
    type Params = GitShowParams;
    /// The opened buffer, in the same shape `buffer/open` returns — the client's adopt path is
    /// identical, and `title` + `read_only` are what mark it as virtual.
    type Result = crate::buffer::BufferOpenResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitShowParams {
    pub repo_id: RepoId,
    /// Anything `git rev-parse` accepts: a hash, a branch, a tag, `HEAD~3`. Unresolvable is an
    /// error, not an empty buffer.
    pub rev: String,
    /// Repo-relative path to show *at* `rev`. `None` shows the commit itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

// ---- git/stash_* --------------------------------------------------------------------------------

/// Stash the working tree (`git stash push`). Rewrites the working tree, so it carries the same
/// dirty-buffer pre-flight and reconciliation as a checkout: unsaved buffers would be stranded on a
/// base that no longer exists on disk.
pub struct GitStashPush;
impl RpcMethod for GitStashPush {
    const NAME: &'static str = "git/stash_push";
    type Params = GitStashPushParams;
    type Result = GitStashResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitStashPushParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    /// Resolution hint when `repo_id` is absent, as everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Optional label. `None` takes git's own `WIP on <branch>: <commit> <subject>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Restore a stash into the working tree — `git stash apply`, or `pop` to drop it afterwards.
/// Tree-rewriting, like [`GitStashPush`] and [`GitCheckout`].
pub struct GitStashApply;
impl RpcMethod for GitStashApply {
    const NAME: &'static str = "git/stash_apply";
    type Params = GitStashApplyParams;
    type Result = GitStashResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitStashApplyParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// The stash commit's hash. Addressed by hash rather than by `stash@{n}` because positions
    /// shift as entries are dropped: the server re-resolves the position immediately before
    /// shelling out, and refuses if this entry has since gone.
    pub oid: String,
    /// Drop the entry after a successful restore (`git stash pop`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pop: bool,
}

/// Discard a stash entry (`git stash drop`). **Not** tree-rewriting — nothing to reconcile, the
/// same split [`GitDeleteBranch`] has from [`GitCheckout`].
pub struct GitStashDrop;
impl RpcMethod for GitStashDrop {
    const NAME: &'static str = "git/stash_drop";
    type Params = GitStashDropParams;
    type Result = GitStashResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitStashDropParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<RepoId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// The stash commit's hash — see [`GitStashApplyParams::oid`].
    pub oid: String,
}

/// The outcome of any stash operation. One result type for all three because the client does the
/// same three things with it — toast the outcome, list the buffers it refreshed, surface git's
/// refusal verbatim — and every discriminated variant here is one the *server* determined.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GitStashResult {
    pub status: GitStashStatus,
    /// The reconciliation that followed: which open buffers were re-read, which diverged, which
    /// files the operation removed. Default (all empty) for a drop, which touches no file.
    #[serde(default, skip_serializing_if = "GitRefreshResult::is_empty")]
    pub refreshed: GitRefreshResult,
    /// Unsaved buffers blocking the operation, for `BlockedByDirtyBuffers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<BufferId>,
    /// git's own words on a `Refused`, or the created entry's description on a push. Empty
    /// otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitStashStatus {
    /// `git stash push` created an entry.
    #[default]
    Pushed,
    /// Nothing to stash — a clean working tree. Distinguished from `Pushed` because git exits 0
    /// either way, and "stashed" when nothing was is a lie the user would act on.
    NothingToStash,
    Applied,
    Popped,
    Dropped,
    /// Unsaved buffers in this repo: refused before anything ran, listing them.
    BlockedByDirtyBuffers,
    /// The entry named by `oid` is no longer in `refs/stash` — dropped or popped elsewhere while
    /// the picker was open.
    Gone,
    /// git refused (a conflicting apply, most often); `message` carries its text verbatim.
    Refused,
}

// ---- git/refresh --------------------------------------------------------------------------------

/// Reconcile every open buffer in a repo with the working tree, in one pass.
///
/// The working tree can move *wholesale* — a checkout, a stash pop, a pull, a worktree switch —
/// rewriting hundreds of files at once. `buffer/reload` is the wrong shape for that: it is
/// per-buffer, user-driven, and refuses a dirty buffer. This is the repo-grain counterpart, and
/// it is what an editor-driven tree-rewriting operation calls once when it finishes.
///
/// Deliberately non-destructive, so it is safe to call at any time and can be tested before
/// anything destructive depends on it: a clean buffer is re-read, a **dirty** buffer is never
/// touched — it's flagged as diverged for the user to resolve — and a buffer whose file has
/// vanished stays open and marked missing rather than being closed out from under the user.
pub struct GitRefresh;
impl RpcMethod for GitRefresh {
    const NAME: &'static str = "git/refresh";
    type Params = GitRefreshParams;
    type Result = GitRefreshResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitRefreshParams {
    /// Which repo moved. Must be one `git/repos` reported for the caller's active workspace.
    pub repo_id: RepoId,
}

/// What the pass did, so a caller that has just rewritten the tree can summarise it in one
/// message instead of the user discovering it buffer by buffer. Buffers whose file was unchanged
/// appear in none of these lists; their Git baseline is still recomputed, because a commit moves
/// HEAD without touching any file.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRefreshResult {
    /// Clean buffers whose file changed on disk, re-read in place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reloaded: Vec<BufferId>,
    /// Buffers with unsaved edits whose file *also* changed underneath. Left exactly as they
    /// were and flagged externally-modified — reloading would silently discard the user's work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diverged: Vec<BufferId>,
    /// Buffers whose file no longer exists (checked out a ref that doesn't have it). Still open,
    /// flagged externally-deleted; their content survives in memory and a save recreates the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<BufferId>,
}

impl GitRefreshResult {
    pub fn is_empty(&self) -> bool {
        self.reloaded.is_empty() && self.diverged.is_empty() && self.missing.is_empty()
    }
}

// ---- git/blame_changed (notification) -----------------------------------------------------------

/// Server push: the followed cursor line's blame (see [`GitSetBlameFollow`]). Sent only when the
/// settled `(line, revision)` differs from the last push, so holding `j` produces no blame
/// traffic until the cursor rests.
pub struct GitBlameChanged;
impl NotificationMethod for GitBlameChanged {
    const NAME: &'static str = "git/blame_changed";
    type Params = GitBlameChangedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitBlameChangedParams {
    pub buffer_id: BufferId,
    /// The 0-based buffer line the blame is for — the client's settled cursor line at resolve
    /// time. Echoed so a client that has already moved on can discard the stale label.
    pub line: u32,
    /// `None` when the line has no blame: no repo, untracked file, or past end-of-file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blame: Option<BlameInfo>,
}

// ---- git/blame_line -----------------------------------------------------------------------------

pub struct GitBlameLine;
impl RpcMethod for GitBlameLine {
    const NAME: &'static str = "git/blame_line";
    type Params = GitBlameLineParams;
    type Result = GitBlameLineResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitBlameLineParams {
    pub buffer_id: BufferId,
    /// 0-based buffer line whose blame is wanted.
    pub line: u32,
    /// Also resolve the blamed commit's full details into `commit_info` — the blame-then-
    /// lookup client chain folded into one round-trip (docs/protocol-composites.md, G).
    /// No effect for an uncommitted or unblamed line.
    #[serde(default)]
    pub include_commit_info: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitBlameLineResult {
    /// `None` when there's no blame for the line: no repo, untracked file, or a line past the
    /// end of the file. An uncommitted line is `Some` with `is_uncommitted = true`.
    pub blame: Option<BlameInfo>,
    /// With `include_commit_info`: the blamed commit's full details, when the line blames to
    /// a real commit that still resolves. Best-effort — `None` if the hash doesn't resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_info: Option<CommitInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameInfo {
    /// Abbreviated (7-char) commit hash. Empty when `is_uncommitted`. The full message and metadata
    /// are fetched on demand by re-requesting blame with `include_commit_info` (the blame popover).
    pub commit: String,
    pub author: String,
    /// Author time as Unix seconds. `0` when `is_uncommitted`.
    pub timestamp: i64,
    /// The line is a local, not-yet-committed edit (or a brand-new working-tree line).
    pub is_uncommitted: bool,
}

// ---- commit details -----------------------------------------------------------------------------

/// Full details for a single commit, resolved from a hash the client already has (e.g. the
/// abbreviated hash in a line's [`BlameInfo`]) and returned alongside blame when
/// `include_commit_info` is set. Drives the blame "commit details" popover and is deliberately
/// generic — not blame-specific — so a future log/show view can reuse it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full (40-char) commit hash.
    pub commit: String,
    pub author: String,
    pub email: String,
    /// Author date, pre-formatted by the server in the commit's own timezone
    /// (`YYYY-MM-DD HH:MM:SS ±HHMM`), so both clients render it identically without a date library.
    pub date: String,
    /// The complete commit message (subject + body), trailing whitespace trimmed.
    pub message: String,
}

// ---- file status (explorer colouring) -----------------------------------------------------------

/// The Git status of a single file-explorer entry, used to colour it. Folded from libgit2's
/// per-path status flags into one value per entry (the working-tree + index state vs HEAD, matching
/// the gutter's "vs HEAD" model — staged and unstaged are not distinguished here).
///
/// For a **directory** entry this is the highest-priority status among its descendants, so a folder
/// inherits the colour of whatever changed inside it. The priority order is the declaration order
/// below (`Conflicted` highest, `Ignored` lowest): a real change always wins over an ignored
/// sibling, so a tracked folder holding a build artifact still reads as changed rather than gray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitStatus {
    /// A merge conflict (both sides modified the path).
    Conflicted,
    /// Removed from the working tree and/or staged for deletion.
    Deleted,
    /// Tracked and changed (working-tree and/or staged modification, or a rename).
    Modified,
    /// Newly staged (in the index, not in HEAD).
    Added,
    /// Present in the working tree but not tracked.
    Untracked,
    /// Excluded by a `.gitignore` rule (e.g. `target/`, `node_modules/`).
    Ignored,
}
