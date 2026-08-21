//! Workspace selection. The server hosts many workspaces; each client has a single active workspace
//! at a time. The client picks one with `workspace/activate` (also used to switch). `workspace/list`
//! enumerates the workspaces the server has configured on disk.

use crate::buffer::BufferOpenResult;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// Enumerate configured workspaces (the `*.toml` files under `$XDG_CONFIG_HOME/aether/workspaces/`).
/// Does not indicate which one — if any — the calling client has active; the client tracks that
/// locally. Issued directly by the web client's bootstrap chooser (the native shells reach the
/// workspace list through the workspaces picker instead).
pub struct WorkspaceList;
impl RpcMethod for WorkspaceList {
    const NAME: &'static str = "workspace/list";
    type Params = WorkspaceListParams;
    type Result = WorkspaceListResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceListParams {}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub name: String,
}

/// Activate a workspace for this client. Used both for the initial selection (the client has just
/// connected and has no active workspace) and for switching (already active, picking a different
/// one). Switching tears down the client's per-buffer state for the previously-active workspace.
/// The buffers themselves stay in the server, available to other clients.
pub struct WorkspaceActivate;
impl RpcMethod for WorkspaceActivate {
    const NAME: &'static str = "workspace/activate";
    type Params = WorkspaceActivateParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceActivateParams {
    pub name: String,
    /// Which **context** of the workspace to enter: repo → the admin name of the worktree to
    /// resolve its roots against. A workspace is `(configured roots, bindings)`, and the **base is
    /// the empty map** — not a different kind of thing, which is what removes every base-versus-
    /// bound case from this call.
    ///
    /// Keys are [`crate::git::RepoId`]s — the workdirs picker rows already carry, echoed back
    /// opaquely. The server normalises each to its repo *family* (common dir) on the way in, so a
    /// binding sent from inside a worktree lands on the same key as one sent from the main
    /// checkout instead of writing a second entry for the same repo.
    ///
    /// Omitted means **"wherever I was"**: the server enters the workspace's most recently
    /// activated context. That is how a cold-started window returns to the worktree it was in
    /// without windows needing durable identity across restarts — they have none. Send an explicit
    /// empty map to mean the base regardless of history.
    ///
    /// A binding whose worktree no longer resolves does not fail the activation: those roots fall
    /// back to their configured paths. The configured roots are the workspace's own definition, so
    /// there is no way for a binding to strand you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees: Option<std::collections::BTreeMap<crate::git::RepoId, String>>,
    /// Also open the landing buffer — the workspace's `last_buffer_id` when there is one, a fresh
    /// *transient* scratch otherwise — and return it in `opened`. The bootstrap convention
    /// (activate, then land somewhere) folded into one round-trip.
    #[serde(default)]
    pub open_last: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceActivateResult {
    pub workspace: WorkspaceInfo,
    /// The most-recently-used buffer in this workspace for the calling client, if any. Populated
    /// from the server's per-client MRU. `None` means the client has no history in this workspace
    /// (first visit, or every prior buffer has been closed). The client should attach to this
    /// buffer rather than spawn a fresh scratch, so switching back to a workspace lands you on the
    /// buffer you last had open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_buffer_id: Option<BufferId>,
    /// With `open_last`: the landing buffer, fully opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<BufferOpenResult>,
    /// The server instance's start time (unix ms) — its identity for restart detection. A client
    /// caches it on activation and compares across reconnects: a changed value means the daemon
    /// restarted (so unsaved buffer state died with it), distinct from a connection that merely
    /// blipped. Carried on the wire rather than read from a discovery file, so it's authoritative
    /// for the instance you're actually talking to.
    #[serde(default)]
    pub server_started_at: u64,
}

/// Describes the active workspace: its name and absolute root paths. Returned by
/// `workspace/activate`. Paths are server-canonicalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub paths: Vec<String>,
    /// The worktree bindings this context is resolved against — empty in the base. Carried so a
    /// client can hand the same context to a **new window** without asking the server what it is
    /// standing in. It also drove an `aether @ feature-auth` label in the status bar and title;
    /// that was removed — a binding is per *repo*, so where you are is said on the git cluster's
    /// `⧉ branch`, at the grain it actually applies to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<WorkspaceWorktree>,
    /// Projects declared by this workspace, resolved for display. Absent on the wire when empty, so
    /// a workspace that declares none costs nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<WorkspaceProject>,
}

/// One repo of a workspace bound to a worktree.
///
/// Carries the branch as well as the admin name because the two **drift**: `git worktree add` names
/// a tree once, and a checkout inside it later moves HEAD without renaming anything, so a tree
/// admin-named `feature-auth` can be sitting on `main`. A label wants both — admin name primary,
/// branch secondary — for the same reason the picker's rows show both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWorktree {
    /// The repo this binds, as the [`crate::git::RepoId`] of its **main** checkout — the form
    /// clients already speak, and the one to echo back to `workspace/activate`.
    pub repo_id: crate::git::RepoId,
    /// Admin name of the bound worktree. Never empty here: an empty name is an unbind, which is the
    /// absence of an entry rather than an entry saying nothing.
    pub worktree: String,
    /// Branch currently checked out in that tree, when it is on one. Empty for a detached HEAD, or
    /// when the tree no longer resolves.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
}

/// One declared project — a marker file whose language server is pinned open while the workspace is
/// active.
///
/// Rendered as the canonical `[root]: [path]` buffer-location format (`aether-client/labels.rs`),
/// like every other in-workspace file reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceProject {
    /// Index into [`WorkspaceInfo::paths`] of the root this project is declared under. Derived per
    /// response and matched against the `paths` in the same message — never persisted, where a
    /// positional reference would be fragile (the config file nests projects under their root
    /// instead, so it has no index at all).
    pub path_index: u32,
    /// Project directory relative to that root (`.`, `web`, `crates/server`).
    pub relative_path: String,
    /// The language whose server this project pins — inferred from the marker's file name, or
    /// declared explicitly for markers that don't imply one. Empty when the entry doesn't resolve.
    #[serde(default)]
    pub language: String,
    /// Why this project is unusable, when it is: a deleted marker (branch switch), an unrecognised
    /// file name, a path outside every root. Recomputed on every read rather than cached, so a
    /// project that comes back stops erroring without reactivating the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Create a fresh workspace with no roots. The client uses the workspace picker's "create new" row
/// to invoke this; the server writes an empty-`paths` TOML to
/// `$XDG_CONFIG_HOME/aether/workspaces/<name>.toml`, registers the workspace in memory, and
/// activates it for the calling client. The follow-up is normally `workspace/add_root` (via the
/// workspace settings overlay, which the TUI auto-opens after create).
///
/// Refuses if a workspace of that name already exists on disk. Name must be non-empty and contain
/// no path separators.
pub struct WorkspaceCreate;
impl RpcMethod for WorkspaceCreate {
    const NAME: &'static str = "workspace/create";
    type Params = WorkspaceCreateParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    pub name: String,
}

/// Open a file by absolute path, resolving the workspace context for it. This is the workspace-agnostic
/// entry point used by `ae /path/to/file` and the `Space Alt-w` open-from-path overlay — the cases
/// that may need to *activate* a workspace (an ephemeral one when none is active). The path must be
/// absolute (a leading `~/` is fine): the server will **not** resolve it against its own working
/// directory, which isn't the user's. (`ae path` resolves its arg client-side before sending.)
/// Goto-definition into a file outside the active workspace doesn't go through here: it already has an
/// active workspace to host the guest, so it opens the external buffer directly via `buffer/open`'s
/// `absolute_path` (same external-buffer machinery, no workspace activation). The server:
///
/// - canonicalizes the path;
/// - if the calling client has an active workspace whose roots **contain** the path, opens it there
///   as an ordinary (internal) buffer;
/// - if a workspace is active but the path is **outside** its roots, opens it there as an *external*
///   buffer (the workspace hosts it as a guest — no git, trust-restricted LSP) — it is **not**
///   re-homed into whichever other configured workspace might contain it;
/// - if **no** workspace is active, synthesizes a fresh *ephemeral* workspace (no name, no config on
///   disk, auto-removed when its last buffer closes — and superseded by the next one, so throwaway
///   contexts don't accumulate), activates it, and opens the file there. Such a workspace is rooted
///   at the **directory of the file that created it** (each further directory opened into it adds a
///   root), so its file-oriented pickers have something to work over. That root is bookkeeping, not
///   trust: an ephemeral workspace still gets **no language server**, however its files sit relative
///   to it.
///
/// Returns the (possibly newly-activated) workspace alongside the opened buffer, so the client adopts
/// the workspace id exactly as it does after `workspace/activate`.
pub struct WorkspaceOpenPath;
impl RpcMethod for WorkspaceOpenPath {
    const NAME: &'static str = "workspace/open_path";
    type Params = WorkspaceOpenPathParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceOpenPathParams {
    /// File to open. Must be absolute; a leading `~/` is expanded server-side and also counts as
    /// absolute. A relative path is rejected (the server won't resolve it against its own cwd).
    /// Must exist on disk unless `create_if_missing` is set.
    pub path: String,
    /// Open the buffer as transient (auto-closes once hidden) — used when the open is a preview.
    /// Defaults to a permanent open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
    /// When `path` doesn't exist, open an empty buffer bound to it instead of failing (the file
    /// is written at the first save) — same semantics as `buffer/open`'s flag, which this
    /// delegates to. Powers `ae path/to/new-file`; ignored for existing files.
    #[serde(default)]
    pub create_if_missing: bool,
}

/// Add a root path to an existing workspace. Server canonicalizes the path, refuses duplicates,
/// updates the TOML, watches the new path for external changes, and invalidates the workspace's
/// workspace index (so the next picker open re-walks).
pub struct WorkspaceAddRoot;
impl RpcMethod for WorkspaceAddRoot {
    const NAME: &'static str = "workspace/add_root";
    type Params = WorkspaceAddRootParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceAddRootParams {
    /// Workspace to modify. Doesn't have to be the caller's active workspace (the TUI only uses it
    /// for the active workspace today, but the protocol stays general).
    pub workspace: String,
    /// Path on disk. Must exist and be canonicalizable. Leading `~/` is expanded server-side.
    pub path: String,
}

/// Remove a root path from a workspace. The server closes any file-backed buffers under this root
/// that aren't covered by another remaining root, and refuses the whole operation if any such
/// buffer is dirty (with error code `DIRTY_BUFFERS_PREVENT_REMOVE`). Scratch buffers in the
/// workspace are unaffected (they have no path and aren't tied to any root).
///
/// The `next_buffer_id` field follows the same convention as `buffer/close`: when the client's
/// currently-displayed buffer is one of the closed ones, attach to this next id (or spawn a
/// scratch if `None`).
pub struct WorkspaceRemoveRoot;
impl RpcMethod for WorkspaceRemoveRoot {
    const NAME: &'static str = "workspace/remove_root";
    type Params = WorkspaceRemoveRootParams;
    type Result = WorkspaceRemoveRootResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRemoveRootParams {
    pub workspace: String,
    /// The root to remove. Server matches against the workspace's stored canonical paths after
    /// canonicalizing this value too — so callers can pass either the canonical form (what they
    /// got back from `WorkspaceInfo.paths`) or a user-typed equivalent.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRemoveRootResult {
    pub workspace: WorkspaceInfo,
    /// File-backed buffers that were closed as part of this remove. Scratch buffers and buffers
    /// still covered by other roots are not in this list.
    #[serde(default)]
    pub closed_buffer_ids: Vec<crate::BufferId>,
    /// Buffer for the requesting client to attach to if its current buffer was one of the
    /// closed ones. `None` means "no buffers left for you in this workspace — spawn a scratch."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_buffer_id: Option<crate::BufferId>,
}

/// Declare a project in a workspace: a *directory* whose language server is pinned open while the
/// workspace is active. The server validates it (relative, inside its root, exists, and its
/// language either inferable from the build manifests inside or given explicitly), appends it to
/// the TOML under that root, and launches the server straight away rather than waiting for the next
/// activation.
///
/// Refuses duplicates and anything that fails to resolve — unlike activation, which skips bad
/// entries and carries on, a *new* declaration should fail loudly while the user is looking at it.
pub struct WorkspaceAddProject;
impl RpcMethod for WorkspaceAddProject {
    const NAME: &'static str = "workspace/add_project";
    type Params = WorkspaceAddProjectParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceAddProjectParams {
    pub workspace: String,
    /// Which root to declare the project under — an index into the workspace's root list. Chosen by
    /// the user in multi-root workspaces (the path editor's root field); always `0` when there's
    /// only one root.
    pub path_index: u32,
    /// Project directory relative to that root; empty (or `"."`) is the root itself.
    pub relative_path: String,
    /// Overrides the language inferred from the directory's build manifests. Needed when it has
    /// several kinds, or none at all — a Python tree with no `pyproject.toml`, say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Undeclare a project. Drops it from the TOML and unpins its server, which then reaps unless
/// buffers are open against it (in which case it reverts to the ordinary reap-on-last-buffer
/// lifetime). A no-op error if the workspace doesn't declare that path.
pub struct WorkspaceRemoveProject;
impl RpcMethod for WorkspaceRemoveProject {
    const NAME: &'static str = "workspace/remove_project";
    type Params = WorkspaceRemoveProjectParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRemoveProjectParams {
    pub workspace: String,
    /// The project to drop, as it appears in [`WorkspaceProject`].
    pub path_index: u32,
    pub relative_path: String,
}

/// What language would declaring this directory as a project pin? The settings dialog's add-project
/// row asks as the user types, so the language segment can be pre-filled with the answer (still
/// editable — a typed language overrides, and committing with the field empty re-infers
/// server-side anyway).
///
/// Read-only: the same manifest scan `workspace/add_project`'s validation runs, with one addition —
/// languages the workspace *already declares* for this directory are excluded, because a directory
/// holding several projects in several languages is declared once per language and the useful
/// suggestion is the one not yet added. `language` comes back `None` whenever the directory doesn't
/// (yet) resolve or its manifests don't single out exactly one language; never an RPC error, since
/// half-typed paths are this method's normal input.
pub struct WorkspaceInferLanguage;
impl RpcMethod for WorkspaceInferLanguage {
    const NAME: &'static str = "workspace/infer_language";
    type Params = WorkspaceInferLanguageParams;
    type Result = WorkspaceInferLanguageResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceInferLanguageParams {
    pub workspace: String,
    /// Root the prospective project would be declared under (index into the workspace's roots) —
    /// the add-project row's root segment.
    pub path_index: u32,
    /// Directory relative to that root, as typed. A trailing `/` and the empty/`.` forms (the root
    /// itself) are normalized server-side exactly as `workspace/add_project` normalizes them.
    pub relative_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceInferLanguageResult {
    /// The single language the directory's own build manifests identify, after excluding languages
    /// already declared for it. Absent when there is no such single language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Rename a workspace. Moves the on-disk config (`<old>.toml` → `<new>.toml`) and re-keys the
/// workspace's in-memory state: the server's workspace map, every open buffer's workspace association,
/// and every client's active-workspace pointer. Open buffers are untouched — they keep their ids
/// and paths — so a rename is safe even with dirty buffers in the workspace, and nothing is closed
/// or reloaded.
///
/// Refuses if a workspace named `new_name` already exists, or if `new_name` is empty / contains
/// path separators (same constraints as `workspace/create`). Renaming a workspace to its current
/// name is a no-op that returns the current info.
pub struct WorkspaceRename;
impl RpcMethod for WorkspaceRename {
    const NAME: &'static str = "workspace/rename";
    type Params = WorkspaceRenameParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    /// The workspace to rename (its current name).
    pub workspace: String,
    /// The desired new name.
    pub new_name: String,
}

/// Delete a workspace: remove its on-disk config (`<name>.toml`) and drop its in-memory state,
/// closing any buffers that belonged to it. This forgets the workspace *definition* — it does NOT
/// touch the source files under the workspace's roots.
///
/// Refuses if the workspace is any connected client's active workspace (`ACTIVE_WORKSPACE_PREVENTS_-
/// DELETE`) — the caller must switch away first — or if any buffer in the workspace has unsaved
/// changes (`DIRTY_BUFFERS_PREVENT_DELETE`). Invoked from the workspace switcher; you can't delete
/// a workspace you're not looking at the list of, and you can't delete the one you're in.
pub struct WorkspaceDelete;
impl RpcMethod for WorkspaceDelete {
    const NAME: &'static str = "workspace/delete";
    type Params = WorkspaceDeleteParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceDeleteParams {
    pub name: String,
}

// ---- workspace/renamed (notification) -------------------------------------------------------------

/// Pushed to a client when its active workspace is renamed by *another* client. The server has
/// already re-keyed the receiver's server-side state (active workspace, buffers) to the new name; this
/// tells the client so it can update its local name — which drives both the display and the
/// reconnect baseline (reconnect is by name). Only sent to clients whose active workspace was renamed;
/// the renaming client learns the new name from its `workspace/rename` RPC result instead.
pub struct WorkspaceRenamed;
impl NotificationMethod for WorkspaceRenamed {
    const NAME: &'static str = "workspace/renamed";
    type Params = WorkspaceRenamedParams;
}

/// Pushed to a client when its active workspace's **shape** changes under it: a root added or
/// removed, a project declared or undeclared, or a worktree rebound (which moves every root at
/// once). Anything, in short, that changes the [`WorkspaceInfo`] the workspace would report.
///
/// The client's own RPC results already carry a fresh [`WorkspaceInfo`] whenever *it* changes the
/// workspace; this is the same payload for changes it didn't make. Without it a second client keeps
/// the old roots and every path it renders is resolved against a shape the workspace no longer has —
/// wrong labels, wrong `path_index`, and a `buffer/closed` successor that lands somewhere it can't
/// describe.
///
/// Sent before the `buffer/closed` pushes that accompany a rebind, so the roots are already current
/// when the client opens the successor.
pub struct WorkspaceChanged;
impl NotificationMethod for WorkspaceChanged {
    const NAME: &'static str = "workspace/changed";
    type Params = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRenamedParams {
    pub old_name: String,
    pub new_name: String,
}

// ---- workspace/bind_worktree + unbind_worktree ---------------------------------------------------

/// Point one of a workspace's repos at one of its worktrees, and re-activate the workspace.
///
/// **One rule: this always adjusts the workspace you are in.** It never creates a workspace, never
/// switches to another, and never asks which one you meant. A workspace is its configured roots plus
/// a set of worktree bindings; this call edits that set, the roots re-materialise around it, and your open buffers follow to the same relative paths.
///
/// So there are exactly two outcomes, and the bindings alone decide which:
///
/// - **A binding added or changed** → those roots resolve into a worktree instead of the main
///   checkout. Every other binding, the name, and the session are untouched.
/// - **A binding removed** (empty `worktree`) → that repo goes back to its main checkout. The
///   configured roots are the fallback, which is why this can never strand you: the worktree
///   bindings are machine state, the roots are the workspace's own config.
///
/// The rules dropped along the way were all dropped for one reason — they turned invisible state
/// into a discriminator. "First-bound repo is special" made the same gesture behave differently on
/// one repo than another. "A set another variant holds moves you there" made it depend on what
/// *other* contexts contained. And **worktree variants** — a second workspace id `<base>/<name>`
/// spawned automatically the first time you bound from an unbound workspace — made the same
/// keypress mean "adjust this" or "create and leave" depending on where you already were. All three
/// are gone; binding is the same act everywhere.
///
/// Having two trees of one repo open at once is therefore no longer this call's job. It is an
/// ordinary second workspace, created deliberately.
///
/// The worktree must already exist — [`crate::git::GitWorktreeAdd`] makes it. Splitting the two
/// keeps a filesystem checkout and a state-file write as separate operations, which is the rule for
/// when to add a method rather than extend one.
pub struct WorkspaceBindWorktree;
impl RpcMethod for WorkspaceBindWorktree {
    const NAME: &'static str = "workspace/bind_worktree";
    type Params = WorkspaceBindWorktreeParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceBindWorktreeParams {
    /// Workspace to bind in. Defaults to the caller's active workspace, which is what the picker
    /// sends — there is no case where you bind in a workspace you are not standing in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The repo to bind, as its workdir. Defaults to the one the active buffer resolves to, the
    /// same rule every `Space g` surface uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<crate::git::RepoId>,
    /// The buffer the caller is looking at, so the rebind can land it on the *same file* on the new
    /// tree ("the active buffer always follows"). The server cannot work this out for itself —
    /// a client may hold several viewports — and without it the landing falls back to the
    /// workspace's MRU head, which is the right file only by coincidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<crate::BufferId>,
    /// Admin name of the worktree to bind this repo to. **Empty means unbind** — send the repo back
    /// to its main checkout, which is what selecting the `main` row does.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree: String,
    /// Open the resulting workspace's landing buffer, like `workspace/activate { open_last }`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_last: bool,
}
