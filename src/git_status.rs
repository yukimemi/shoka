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

        let merge_base = repo.merge_base(head_id, upstream_id).ok()?.detach();
        let ahead = count_first_ancestor(repo, merge_base, head_id).ok()?;
        let behind = count_first_ancestor(repo, merge_base, upstream_id).ok()?;
        Some((ahead, behind))
    };
    match inner() {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    }
}

/// Count commits walked back from `tip` (inclusive) until reaching
/// `boundary` (exclusive). When `boundary == tip`, returns `0`.
/// Assumes `boundary` is an ancestor of `tip` (which is true when
/// the caller passed `merge_base(tip, other)`); if not, returns
/// the count of *all* commits walked (i.e., the whole reachable
/// set), which is a reasonable fallback for diverged histories.
fn count_first_ancestor(
    repo: &gix::Repository,
    boundary: gix::ObjectId,
    tip: gix::ObjectId,
) -> Result<usize> {
    if boundary == tip {
        return Ok(0);
    }
    let walk = repo.rev_walk([tip]).all()?;
    let mut count = 0usize;
    for item in walk {
        let info = item?;
        if info.id == boundary {
            return Ok(count);
        }
        count += 1;
    }
    Ok(count)
}
