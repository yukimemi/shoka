//! In-process git status capture via `gix`.
//!
//! Used by `shoka cache refresh` to populate the per-repo status
//! snapshot the TUI displays as columns (branch / ahead-behind /
//! dirty). All work is in-process — no `git status` subprocess fan-
//! out, which matters most on Windows where `CreateProcess` dominates
//! the cost over a few-hundred-repo shelf.
//!
//! What's captured (Phase 2a):
//!
//! - `branch` — HEAD's shorthand name, or `HEAD` for a detached
//!   head. `None` when the repo has no commits yet (just-cloned, or
//!   an unborn branch).
//! - `dirty` — `true` when the working tree has any modifications
//!   (tracked or untracked) relative to HEAD. The TUI uses a single
//!   ●/✓ glyph rather than per-status counts; the bool is what we
//!   actually need.
//! - `ahead` / `behind` — commit counts of HEAD relative to its
//!   upstream (`origin/<branch>` by convention). `None` when there
//!   is no remote ref to compare against (offline, fresh clone with
//!   no fetch yet, …).
//!
//! Phase 2b adds GitHub PR / CI counts via `octocrab` on top of this.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Captured snapshot of a repo's local git state. Stored on the
/// per-repo cache entry; serialised verbatim into `cache.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusSnapshot {
    /// HEAD shorthand (e.g. `main`, `feat/foo`). `None` for unborn
    /// branches (repo with no commits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// `true` when the working tree differs from HEAD (tracked
    /// modifications, untracked files, anything that would show up
    /// in `git status --porcelain`).
    pub dirty: bool,

    /// Commits HEAD has that the upstream doesn't. `None` when no
    /// upstream ref exists locally (no fetch yet, or no remote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,

    /// Commits the upstream has that HEAD doesn't. Same `None`
    /// semantics as `ahead`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
}

/// Capture the status snapshot for the repo rooted at `repo_root`.
///
/// Errors propagate when the path can't be opened as a git repo
/// (caller treats them as "skip this entry, leave the previous
/// snapshot intact"). A successful capture with `branch = None`
/// means the repo is valid but has no commits yet.
pub fn capture(repo_root: &Path) -> Result<GitStatusSnapshot> {
    let repo = gix::open(repo_root)?;
    let branch = head_branch(&repo);
    let dirty = is_dirty(&repo)?;
    let (ahead, behind) = ahead_behind(&repo, branch.as_deref());
    Ok(GitStatusSnapshot {
        branch,
        dirty,
        ahead,
        behind,
    })
}

/// Read HEAD's shorthand. Returns `None` when the repo has no
/// commits yet (head() errs with an unborn-branch case); returns
/// `"HEAD"` literally for a detached HEAD so the TUI can render
/// "you're not on a branch" visibly rather than as a blank cell.
fn head_branch(repo: &gix::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    match head.referent_name() {
        Some(name) => Some(name.shorten().to_string()),
        // Detached HEAD: still a valid state, just not a branch.
        None => Some("HEAD".to_string()),
    }
}

/// Best-effort working-tree dirtiness for the repo at `repo_root`.
///
/// A focused entry point for callers (e.g. `shoka rm`'s pre-delete
/// safety gate) that need only the clean/dirty bit, not the full
/// [`capture`] snapshot with its ahead/behind walk. Mirrors
/// [`capture`]'s `dirty` semantics exactly — it shares the same
/// [`is_dirty`] core — so the two never drift. Errors propagate so the
/// caller can decide how cautious to be when the repo can't be read.
pub fn is_dirty_at(repo_root: &Path) -> Result<bool> {
    let repo = gix::open(repo_root)?;
    is_dirty(&repo)
}

/// Is the working tree dirty? Errors are treated as "couldn't
/// tell" — gix's status path can fail on weird repo states (broken
/// symlinks, permission issues on a single file) and we'd rather
/// surface a clean snapshot with `dirty = false` than abort the
/// whole refresh because of one repo.
fn is_dirty(repo: &gix::Repository) -> Result<bool> {
    Ok(repo.is_dirty()?)
}

/// Count commits HEAD has vs. its upstream, and vice versa. Both
/// sides `None` when there's no upstream ref (no fetch yet, or no
/// remote configured); both `Some(0)` when HEAD == upstream.
///
/// Convention: upstream is `refs/remotes/origin/<branch>`. shoka's
/// scope doesn't yet track per-branch upstreams (the `branch.<name>
/// .remote` config gix could read) — `origin` covers the
/// overwhelming majority of yukimemi/* setups, and the right answer
/// for "what should I show by default" is the same one most TUIs
/// pick. Real per-branch upstream resolution is a Phase 2b polish.
fn ahead_behind(repo: &gix::Repository, branch: Option<&str>) -> (Option<usize>, Option<usize>) {
    // Inner closure returns Option<(usize, usize)> so `?` short-
    // circuits on any missing piece (no upstream, can't read head,
    // …) while the outer signature stays as the two-Option tuple
    // the cache schema expects.
    let inner = || -> Option<(usize, usize)> {
        let branch = branch?;
        if branch == "HEAD" {
            // Detached: there's no "upstream" to compare against in
            // any useful sense.
            return None;
        }
        let head_id = repo.head_id().ok()?.detach();
        let upstream_ref = format!("refs/remotes/origin/{branch}");
        let upstream_id = repo
            .find_reference(upstream_ref.as_str())
            .ok()?
            .id()
            .detach();

        // Set-difference via `with_hidden`: walk from one tip,
        // hide everything reachable from the other. No merge_base
        // needed — gix prunes side branches at the boundary so we
        // don't traverse into the shared history, which fixes the
        // "merge / diverged history walks back to root" perf bug
        // the boundary-on-direct-match implementation had.
        if head_id == upstream_id {
            return Some((0, 0));
        }
        let ahead = count_reachable_excluding(repo, head_id, upstream_id).ok()?;
        let behind = count_reachable_excluding(repo, upstream_id, head_id).ok()?;
        Some((ahead, behind))
    };
    match inner() {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    }
}

/// Count commits reachable from `tip` but **not** from `hide`.
/// Equivalent to `git rev-list --count <tip> ^<hide>`.
///
/// Uses gix's [`with_hidden`] so the walker treats `hide` *and its
/// entire ancestor set* as off-limits. Without that, a diverged
/// history (merge commits, side branches) would walk back to the
/// root commit chasing parents that don't share an obvious linear
/// path; performance scales with diverge distance instead of total
/// repo history.
///
/// [`with_hidden`]: gix::revision::walk::Platform::with_hidden
fn count_reachable_excluding(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    hide: gix::ObjectId,
) -> Result<usize> {
    let walk = repo.rev_walk([tip]).with_hidden([hide]).all()?;
    let mut count = 0usize;
    for item in walk {
        item?;
        count += 1;
    }
    Ok(count)
}
