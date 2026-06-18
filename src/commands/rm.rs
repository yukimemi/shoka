//! `shoka rm` — remove a repo from the shelf, deleting its working tree.
//!
//! The destructive counterpart to `shoka clone`: select a repo (by
//! hint or fuzzy pick), delete its on-disk clone, and drop the entry
//! from the shelf ledger. Where `shoka prune` only reaps entries whose
//! clone path has *already* vanished, `rm` is the deliberate "I'm done
//! with this one" verb that does the deletion for you.
//!
//! Safety:
//!
//! - The confirmation prompt defaults to **no** — `rm` deletes a
//!   working tree, so an accidental Enter must not nuke a repo.
//! - `--dry-run` prints the resolved slug + path and stops, so you can
//!   eyeball exactly what would go before committing to it.
//! - `--keep-files` drops the shelf entry but leaves the directory on
//!   disk — the shelf-only "forget this repo" mode, useful when you've
//!   already relocated the clone by hand.
//!
//! Selection reuses `shoka cd`'s hint / fuzzy machinery so `rm <hint>`
//! and `cd <hint>` resolve a repo identically — the same substring +
//! tag rules, the same picker.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use inquire::Confirm;
use owo_colors::OwoColorize;

use crate::actions::{VcsKind, detect_vcs};
use crate::cli::RmArgs;
use crate::commands::ShokaContext;
use crate::commands::cd::{choose_by_hint, fuzzy_pick};
use crate::config::ShokaConfig;
use crate::git_status;
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: RmArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let mut shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        bail!("shelf is empty — nothing to remove");
    }

    // Tag filter first, then hint filter — same order as `shoka cd` /
    // `shoka list` so the three commands narrow candidates identically.
    let tag_filtered = shelf.filter_by_tags(&args.tags);
    if tag_filtered.is_empty() {
        bail!(
            "no repos matched the tag filter ({} on the shelf total)",
            shelf.len()
        );
    }

    let page_size = resolved.ui.cd_page_size;
    let chosen = match args.repo.as_deref() {
        Some(hint) => choose_by_hint(&tag_filtered, hint, page_size)?,
        None => fuzzy_pick(&tag_filtered, "remove:", page_size)?,
    };

    // Capture the chosen repo's identity + resolved path as owned
    // values so the immutable borrow of `shelf` (via `chosen`) ends
    // here, freeing us to mutate the shelf further down.
    let id = RepoId::of(chosen);
    let path = resolved.clone_path_for_one(chosen)?;
    let slug = id.slug();

    if args.dry_run {
        let fate = if args.keep_files {
            "drop from shelf (keep files)"
        } else {
            "delete working tree + drop from shelf"
        };
        println!(
            "{} would {} {} {} {}",
            "rm:".bold(),
            fate,
            slug.bold(),
            "→".dimmed(),
            path.display().dimmed()
        );
        println!(
            "{} dry run — re-run without `--dry-run` to remove",
            "rm:".bold()
        );
        return Ok(());
    }

    // Pre-delete safety gate: refuse to nuke a working tree that has
    // uncommitted git / jj changes. Skipped when we aren't deleting
    // files (`--keep-files`) or the user explicitly opted out
    // (`--force`). Only a *confirmed* dirty state activates the guard —
    // an inconclusive check (no VCS marker, jj not installed, a gix
    // read error) must not block a legitimate removal, so it falls
    // through to the normal confirmation flow.
    if !args.keep_files && !args.force {
        match cleanliness(&path).await {
            Cleanliness::Dirty => {
                println!(
                    "{} {} has uncommitted changes",
                    "rm:".bold().yellow(),
                    slug.bold()
                );
                if args.yes {
                    // Non-interactive: there's no prompt to catch
                    // this, so a scripted `rm -y` must not silently
                    // drop the changes.
                    bail!(
                        "{slug} has uncommitted changes — commit or stash them, \
                         or re-run with --force to delete anyway"
                    );
                }
                // Interactive: the warning above plus the default-"no"
                // prompt below is the catch; an explicit "yes" proceeds.
            }
            Cleanliness::Clean => {}
            Cleanliness::Unknown(why) => {
                // Couldn't tell — never block a legitimate removal on
                // an inconclusive probe; just note why for debugging.
                tracing::debug!(
                    target: "shoka",
                    "cleanliness check for {slug} inconclusive: {why}"
                );
            }
        }
    }

    if !args.yes {
        let question = if args.keep_files {
            format!(
                "Drop {slug} from the shelf (leaving {} on disk)?",
                path.display()
            )
        } else {
            format!(
                "Delete {slug} at {} (working tree + shelf entry)?",
                path.display()
            )
        };
        let confirmed = Confirm::new(&question)
            .with_default(false)
            .prompt()
            .context("rm confirmation cancelled")?;
        if !confirmed {
            println!("{} aborted — shelf unchanged", "rm:".bold().yellow());
            return Ok(());
        }
    }

    let outcome = remove_entry(&mut shelf, &id, &path, args.keep_files)?;
    shelf.save(&ctx.paths)?;

    let what = match outcome {
        Outcome::Deleted => "removed (working tree deleted)",
        Outcome::AlreadyGone => "removed (working tree was already missing)",
        Outcome::KeptFiles => "removed from shelf (files kept on disk)",
    };
    println!(
        "{} {} {} ({} on shelf now)",
        "rm:".bold().green(),
        slug.bold(),
        what,
        shelf.len()
    );
    Ok(())
}

/// What happened to the on-disk working tree, for the success line +
/// tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Working tree existed and was deleted.
    Deleted,
    /// Working tree was already gone — only the shelf entry was reaped.
    AlreadyGone,
    /// `--keep-files`: the directory was deliberately left in place.
    KeptFiles,
}

/// Owned `(host, owner, name, path)` identity of the chosen repo,
/// captured so the shelf borrow can be released before mutation.
struct RepoId {
    host: String,
    owner: String,
    name: String,
    path: Option<PathBuf>,
}

impl RepoId {
    fn of(repo: &Repo) -> Self {
        Self {
            host: repo.host.clone(),
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            path: repo.path.clone(),
        }
    }

    fn slug(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// Drop the entry from the shelf and delete the working tree (unless
/// `keep_files`). Returns what happened to the directory.
///
/// Order matters: the shelf entry is removed from the in-memory shelf
/// **first**, before any destructive disk I/O. The chosen repo came
/// straight off this shelf, so a miss is a logic bug — surfacing it
/// here means we bail *before* deleting anything rather than after, so
/// a (should-never-happen) disagreement can't leave files gone while
/// the entry lingers. And because the caller only `save()`s the shelf
/// on `Ok`, a failed or partial `delete_tree` leaves the on-disk shelf
/// untouched — the entry survives and the user can simply retry.
///
/// A missing directory is **not** an error: the user's goal is "this
/// repo is gone", and a clone someone already `rm -rf`'d by hand
/// satisfies that — we just reap the now-orphaned shelf entry and say
/// so. Any other I/O error (permission denied, busy, …) propagates so
/// we never silently report success while the files are still there.
fn remove_entry(shelf: &mut Shelf, id: &RepoId, path: &Path, keep_files: bool) -> Result<Outcome> {
    if shelf
        .remove_by_path(&id.host, &id.owner, &id.name, id.path.as_deref())
        .is_none()
    {
        bail!(
            "internal: {} vanished from the shelf before removal",
            id.slug()
        );
    }

    if keep_files {
        Ok(Outcome::KeptFiles)
    } else {
        delete_tree(path)
    }
}

/// Delete whatever sits at the clone path. Almost always a directory
/// (`remove_dir_all`); a plain file / symlink at the path (a corrupted
/// or hand-mangled clone) is unlinked instead so `rm` still does the
/// obviously-right thing. `symlink_metadata` (not `metadata`) so a
/// symlink is removed as the link, never followed into its target.
fn delete_tree(path: &Path) -> Result<Outcome> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            std::fs::remove_dir_all(path)
                .with_context(|| format!("deleting working tree {}", path.display()))?;
            Ok(Outcome::Deleted)
        }
        Ok(_) => {
            std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
            Ok(Outcome::Deleted)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                target: "shoka",
                "clone path {} already missing — removing the shelf entry only",
                path.display()
            );
            Ok(Outcome::AlreadyGone)
        }
        Err(e) => Err(e).with_context(|| format!("inspecting clone path {}", path.display())),
    }
}

/// Result of the pre-delete working-tree cleanliness probe. Only
/// [`Cleanliness::Dirty`] activates the safety guard; the other two
/// are treated identically by the caller (proceed normally) but kept
/// distinct so the inconclusive case can log *why* it couldn't tell.
#[derive(Debug)]
enum Cleanliness {
    /// Working tree matches HEAD / the jj parent — safe to delete.
    Clean,
    /// Uncommitted changes present — deletion would lose work.
    Dirty,
    /// Couldn't determine (no VCS marker, jj missing, read error). The
    /// string explains why, for a debug log.
    Unknown(String),
}

/// Probe the working tree at `path` for uncommitted changes, routing
/// to git (in-process via gix) or jj (`jj diff -s`) by the on-disk
/// marker. `detect_vcs` prefers `.jj/` over `.git/` for colocated
/// repos, matching the rest of shoka.
async fn cleanliness(path: &Path) -> Cleanliness {
    match detect_vcs(path) {
        Some(VcsKind::Git) => match git_status::is_dirty_at(path) {
            Ok(true) => Cleanliness::Dirty,
            Ok(false) => Cleanliness::Clean,
            Err(e) => Cleanliness::Unknown(format!("git status read failed: {e}")),
        },
        Some(VcsKind::Jj) => jj_cleanliness(path).await,
        None => Cleanliness::Unknown("no .git or .jj marker at clone path".into()),
    }
}

/// jj working-copy cleanliness via `jj diff --summary`. jj snapshots
/// the working copy into the `@` change, so its diff against the
/// parent *is* the set of uncommitted edits: empty output ⇒ clean,
/// any output ⇒ dirty. A missing `jj` binary or a non-zero exit is
/// inconclusive rather than fatal — the caller treats Unknown as
/// "proceed with the normal confirmation".
async fn jj_cleanliness(path: &Path) -> Cleanliness {
    // Spawn `jj` directly and let the OS resolve it on PATH, rather than
    // a synchronous `which::which` lookup — that would do blocking disk
    // I/O on the async executor. A `NotFound` spawn error is the
    // "jj isn't installed" case, mapped to Unknown so it never blocks a
    // removal.
    let mut cmd = tokio::process::Command::new("jj");
    cmd.arg("diff")
        .arg("--summary")
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    match cmd.output().await {
        Ok(out) if out.status.success() => {
            if out.stdout.iter().any(|b| !b.is_ascii_whitespace()) {
                Cleanliness::Dirty
            } else {
                Cleanliness::Clean
            }
        }
        Ok(out) => Cleanliness::Unknown(format!(
            "`jj diff` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Cleanliness::Unknown("jj not found on PATH".into())
        }
        Err(e) => Cleanliness::Unknown(format!("spawning `jj diff` failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed(shelf: &mut Shelf, name: &str) -> RepoId {
        let repo = Repo::new("github.com", "yukimemi", name);
        let id = RepoId::of(&repo);
        shelf.add(repo).unwrap();
        id
    }

    #[test]
    fn remove_entry_deletes_dir_and_drops_entry() {
        let tmp = TempDir::new().unwrap();
        let clone = tmp.path().join("github.com/yukimemi/shoka");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("README.md"), "hi").unwrap();

        let mut shelf = Shelf::default();
        let id = seed(&mut shelf, "shoka");

        let outcome = remove_entry(&mut shelf, &id, &clone, false).unwrap();
        assert_eq!(outcome, Outcome::Deleted);
        assert!(!clone.exists(), "working tree should be gone");
        assert!(shelf.is_empty(), "shelf entry should be dropped");
    }

    #[test]
    fn remove_entry_missing_dir_is_already_gone_not_error() {
        let tmp = TempDir::new().unwrap();
        let clone = tmp.path().join("never-existed");

        let mut shelf = Shelf::default();
        let id = seed(&mut shelf, "ghost");

        let outcome = remove_entry(&mut shelf, &id, &clone, false).unwrap();
        assert_eq!(outcome, Outcome::AlreadyGone);
        assert!(shelf.is_empty(), "orphaned entry still reaped");
    }

    #[test]
    fn remove_entry_keep_files_leaves_dir() {
        let tmp = TempDir::new().unwrap();
        let clone = tmp.path().join("github.com/yukimemi/renri");
        std::fs::create_dir_all(&clone).unwrap();

        let mut shelf = Shelf::default();
        let id = seed(&mut shelf, "renri");

        let outcome = remove_entry(&mut shelf, &id, &clone, true).unwrap();
        assert_eq!(outcome, Outcome::KeptFiles);
        assert!(clone.exists(), "--keep-files must not touch the directory");
        assert!(shelf.is_empty(), "shelf entry still dropped");
    }

    #[test]
    fn remove_entry_only_drops_the_targeted_checkout() {
        // Two checkouts of one remote; removing one by its path must
        // leave the sibling untouched on the shelf.
        let tmp = TempDir::new().unwrap();
        let clone_a = tmp.path().join("a");
        let clone_b = tmp.path().join("b");
        std::fs::create_dir_all(&clone_a).unwrap();
        std::fs::create_dir_all(&clone_b).unwrap();

        let mut shelf = Shelf::default();
        let a = Repo::new("github.com", "yukimemi", "dup").with_path(clone_a.clone());
        let b = Repo::new("github.com", "yukimemi", "dup").with_path(clone_b.clone());
        let id_a = RepoId::of(&a);
        shelf.add(a).unwrap();
        shelf.add(b).unwrap();

        remove_entry(&mut shelf, &id_a, &clone_a, false).unwrap();
        assert_eq!(shelf.len(), 1, "only the targeted checkout is removed");
        assert_eq!(shelf.repos[0].path.as_deref(), Some(clone_b.as_path()));
        assert!(!clone_a.exists());
        assert!(clone_b.exists());
    }

    #[tokio::test]
    async fn cleanliness_unknown_when_no_vcs_marker() {
        // A bare directory with no `.git` / `.jj` can't be probed; the
        // guard must treat that as inconclusive, not dirty.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(cleanliness(&dir).await, Cleanliness::Unknown(_)));
    }

    /// Build a git repo with one commit at `dir` via the `git` CLI.
    /// Returns `false` (so callers can skip) when git isn't installed —
    /// `gix`'s dirty detection needs a real HEAD commit to compare the
    /// working tree against, and scripting a commit through gix in a
    /// test is far more code than shelling out to git.
    fn git_repo_with_commit(dir: &Path) -> bool {
        std::fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
        };
        // Probe availability via the first call; bail out cleanly if
        // git isn't on PATH so the suite still runs on a gix-only box.
        if git(&["init", "-q"]).is_err() {
            return false;
        }
        let _ = git(&["config", "user.email", "t@example.com"]);
        let _ = git(&["config", "user.name", "tester"]);
        // Disable commit signing — a dev/CI box may have `commit.gpgsign`
        // (or a custom `gpg.program`) configured globally, and a failed
        // signing call would abort the commit, leaving a staged-but-
        // uncommitted file that muddies the clean/dirty assertion.
        let _ = git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("tracked.txt"), "v1\n").unwrap();
        let _ = git(&["add", "."]);
        let out = git(&["commit", "-q", "--no-gpg-sign", "-m", "init"]).unwrap();
        // If the commit still didn't take (no HEAD), the dirty/clean
        // distinction this helper promises doesn't hold — skip.
        out.status.success()
    }

    #[tokio::test]
    async fn cleanliness_flags_a_dirty_git_tree() {
        // A committed file, then modified → dirty by the same rule
        // `cache refresh` uses. Routes detect_vcs → git → is_dirty_at.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("repo");
        if !git_repo_with_commit(&dir) {
            return; // git unavailable — skip rather than false-fail.
        }
        std::fs::write(dir.join("tracked.txt"), "v2 — edited\n").unwrap();
        assert!(
            matches!(cleanliness(&dir).await, Cleanliness::Dirty),
            "a modified tracked file should read as dirty"
        );
    }

    #[tokio::test]
    async fn cleanliness_clean_git_tree_is_clean() {
        // Right after a commit, the working tree matches HEAD → clean,
        // so a pristine clone deletes without the guard firing.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("repo");
        if !git_repo_with_commit(&dir) {
            return; // git unavailable — skip.
        }
        assert!(
            matches!(cleanliness(&dir).await, Cleanliness::Clean),
            "an unmodified committed repo should read as clean"
        );
    }

    #[test]
    fn delete_tree_unlinks_a_file_at_the_path() {
        // A clone path that's somehow a plain file (corrupted state)
        // is still removed rather than erroring on `remove_dir_all`.
        let tmp = TempDir::new().unwrap();
        let weird = tmp.path().join("not-a-repo");
        std::fs::write(&weird, "i am a file").unwrap();

        let outcome = delete_tree(&weird).unwrap();
        assert_eq!(outcome, Outcome::Deleted);
        assert!(!weird.exists());
    }
}
