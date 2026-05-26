//! `shoka import` — adopt an existing tree into the shelf.
//!
//! Recursively walks the source directory looking for `.git/` or
//! `.jj/` subdirectories. For each repo root found, shoka first tries
//! the `.git/` `[remote "origin"]` URL via [`gix::open`] +
//! [`crate::remote::parse_remote_url`] — the historic ghq-compatible
//! path, yielding a shelf entry keyed by the parsed `(host, owner,
//! name)` triple.
//!
//! When that path doesn't apply (`.jj/`-only, or `.git/` without a
//! remote, or a remote URL shoka can't parse) the importer falls
//! back to a synthesised local identity: `host = "local"`,
//! `owner = <parent dir name>`, `name = <repo dir name>`, and the
//! repo's path is pinned via [`Repo::with_path`] so `cd` / `tui`
//! resolve to the exact location on disk rather than running the
//! entry through `[global].layout`. Repos stay where they are —
//! shoka doesn't move local checkouts.
//!
//! No `git` subprocess is spawned — gix does everything in-process.
//! That matters most on Windows, where `CreateProcess` is slow enough
//! to dominate the import for any non-trivial tree.
//!
//! When the source path is omitted, the command falls back to an
//! [`inquire`] picker over common candidate dirs (`~/ghq`, `~/src`,
//! `~/dev`, …) that actually exist on disk. If none exist, the
//! command errors out asking for an explicit path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use inquire::Select;
use owo_colors::OwoColorize;
use walkdir::WalkDir;

use crate::cli::ImportArgs;
use crate::commands::ShokaContext;
use crate::remote::parse_remote_url;
use crate::state::{Repo, Shelf};

/// `host` field used for repos that don't have a parseable remote
/// URL (`.git/` without a remote, or `.jj/`-only). Distinct from
/// `github.com` / `gitlab.com` / … so the TUI's PR / CI columns
/// can short-circuit on it without firing a doomed gh API call.
const LOCAL_HOST: &str = "local";

pub async fn run(ctx: &ShokaContext, args: ImportArgs) -> Result<()> {
    let source = match args.path {
        Some(p) => p,
        None => prompt_for_source()?,
    };
    if !source.is_dir() {
        bail!("import source {} is not a directory", source.display());
    }

    let mut shelf = Shelf::load(&ctx.paths)?;

    let mut imported = 0usize;
    let mut skipped_already = 0usize;
    let mut errors = 0usize;

    println!(
        "{} scanning {} for git / jj repos…",
        "import:".bold(),
        source.display()
    );

    // Explicit iterator so `skip_current_dir` can prune the walk the
    // moment we recognise a repo root — without that, walkdir would
    // happily descend into `.git/objects` / `.jj/op_store` and yield
    // tens of thousands of dead-end entries per shelf.
    let mut it = WalkDir::new(&source).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(target: "shoka", "walkdir error: {e}");
                errors += 1;
                continue;
            }
        };

        // We recognise EITHER `.git` or `.jj` as a repo marker.
        // walkdir yields directory contents alphabetically, so for a
        // colocated jj+git checkout `.git` is yielded first; calling
        // `skip_current_dir` after the first hit prunes the *parent*
        // dir's remaining children (including the `.jj` sibling) plus
        // the just-yielded entry's own contents. That gives us a
        // single import per repo even when both markers exist.
        let fname = entry.file_name();
        let is_marker =
            (fname == OsStr::new(".git") || fname == OsStr::new(".jj")) && entry.path().is_dir();
        if !is_marker {
            continue;
        }
        it.skip_current_dir();

        let repo_root = match entry.path().parent() {
            Some(p) => p,
            None => continue,
        };

        let result = match fname {
            // `.git` marker: try the remote URL path first, fall
            // back to synthesised local identity when no remote
            // exists. Both cases keep the repo in place.
            f if f == OsStr::new(".git") => extract_git_repo(repo_root),
            // `.jj` marker (no colocated `.git` got there first):
            // jj has no concept of a single "default remote URL"
            // shoka could parse, so always synthesise local.
            _ => Ok(synthesise_local(repo_root)),
        };

        match result {
            Ok(repo) => {
                let slug = repo.slug();
                if shelf.find(&repo.host, &repo.owner, &repo.name).is_some() {
                    skipped_already += 1;
                } else {
                    match shelf.add(repo) {
                        Ok(()) => {
                            println!("  {} {slug}", "+".green());
                            imported += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "shoka",
                                "failed to add {slug} to shelf: {e:#}"
                            );
                            errors += 1;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "shoka",
                    "failed to read {}: {e:#}",
                    repo_root.display()
                );
                errors += 1;
            }
        }
    }

    shelf.save(&ctx.paths)?;

    println!();
    println!(
        "{} {} imported, {} on shelf total",
        "import:".bold(),
        imported,
        shelf.len()
    );
    if skipped_already > 0 {
        println!("  {} {} already on shelf", "↩".dimmed(), skipped_already);
    }
    if errors > 0 {
        println!(
            "  {} {} read errors (see SHOKA_LOG=warn for details)",
            "!".red(),
            errors
        );
    }
    Ok(())
}

/// Pop up an inquire picker listing the candidate source dirs that
/// actually exist on this machine. Common conventions only — users
/// who want something exotic pass `--path` explicitly.
fn prompt_for_source() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("could not locate home dir for default candidates")?;
    let candidates: Vec<PathBuf> = ["ghq", "src", "dev", "Code", "code", "repos", "Projects"]
        .into_iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect();

    if candidates.is_empty() {
        bail!(
            "no common source dirs found under {} — pass `--path` to specify one",
            home.display()
        );
    }
    let labels: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    let chosen = Select::new("Pick a source dir to import from:", labels.clone())
        .prompt()
        .context("source dir selection cancelled")?;
    let idx = labels
        .iter()
        .position(|l| l == &chosen)
        .context("chosen label not in candidates")?;
    Ok(candidates[idx].clone())
}

/// Resolve a `.git/`-marked `repo_root` into a [`Repo`].
///
/// Tries the remote-URL path first — that's the historic
/// ghq-compatible behaviour. Falls back to [`synthesise_local`]
/// when the repo has no remote / no fetch URL / unparseable URL.
/// Real I/O / corruption errors propagate.
fn extract_git_repo(repo_root: &Path) -> Result<Repo> {
    let repo = gix::open(repo_root)
        .with_context(|| format!("opening {} as a git repo", repo_root.display()))?;

    // The chain of "things that mean we have no usable remote URL"
    // is a few `Option<...>`s deep. Squash to "got URL or didn't" and
    // synthesise local in the latter case.
    let maybe_url = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .and_then(|r| r.ok())
        .and_then(|r| r.url(gix::remote::Direction::Fetch).cloned());

    let Some(url) = maybe_url else {
        return Ok(synthesise_local(repo_root));
    };

    match parse_remote_url(&url) {
        Ok(parts) => Ok(Repo::new(parts.host, parts.owner, parts.name)),
        Err(e) => {
            // URL exists but doesn't conform to host/owner/name (e.g.
            // gitlab subgroups, or a deliberately-weird internal URL).
            // Rather than refusing to import, fall back to local so
            // the repo still ends up on the shelf.
            tracing::warn!(
                target: "shoka",
                "could not parse remote URL `{url}` for {}: {e:#}; importing as local",
                repo_root.display()
            );
            Ok(synthesise_local(repo_root))
        }
    }
}

/// Build a local-identity [`Repo`] for `repo_root`.
///
/// Conventions:
///
/// - `host` = [`LOCAL_HOST`] (`"local"`).
/// - `owner` = the **parent directory's** basename, or `"_"` when
///   `repo_root` is at the filesystem root.
/// - `name`  = `repo_root`'s basename, or the absolute path's last
///   component when the basename is empty (rare; only for paths
///   like `C:\` on Windows).
/// - `path` = `repo_root` made absolute.
///
/// The `(host, owner, name)` triple is just a shelf-side label —
/// `clone_path_for` short-circuits on the `path` override, so the
/// repo's actual location on disk is the *only* truth used by
/// `cd` / `tui`. The triple's job is uniqueness within the shelf;
/// `(local, <parent>, <repo>)` is unique enough for the vast
/// majority of real layouts.
fn synthesise_local(repo_root: &Path) -> Repo {
    let abs = match std::path::absolute(repo_root) {
        Ok(p) => p,
        // Absolutising can fail (no cwd, weird OS state). Fall back
        // to the caller-supplied path verbatim — it's still useful
        // even if not normalised, and shoka doesn't validate
        // absoluteness on save.
        Err(_) => repo_root.to_path_buf(),
    };
    let name = path_component_string(abs.file_name()).unwrap_or_else(|| abs.display().to_string());
    let owner = abs
        .parent()
        .and_then(|p| path_component_string(p.file_name()))
        .unwrap_or_else(|| "_".to_string());
    Repo::new(LOCAL_HOST, owner, name).with_path(abs)
}

fn path_component_string(component: Option<&OsStr>) -> Option<String> {
    let raw = component?.to_string_lossy().to_string();
    if raw.is_empty() { None } else { Some(raw) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn synthesise_local_uses_parent_and_repo_dir_names() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("alpha").join("beta");
        std::fs::create_dir_all(&repo).unwrap();
        let r = synthesise_local(&repo);
        assert_eq!(r.host, LOCAL_HOST);
        assert_eq!(r.owner, "alpha");
        assert_eq!(r.name, "beta");
        // path is absolutised and points at the repo root itself.
        let canonical = std::fs::canonicalize(&repo).unwrap();
        let stored = std::fs::canonicalize(r.path.as_ref().unwrap()).unwrap();
        assert_eq!(stored, canonical);
    }

    #[test]
    fn synthesise_local_falls_back_when_no_parent_name() {
        // A path like `C:\` (Windows) / `/` (Unix) has no parent
        // basename. The synthesiser should pick a placeholder rather
        // than panic / generate an empty owner string.
        //
        // We test the helper directly with a synthesised empty
        // OsStr — actually exercising root-FS paths in tests would
        // need elevated privileges on most CI hosts.
        let empty: Option<&OsStr> = Some(OsStr::new(""));
        assert!(path_component_string(empty).is_none());
        let none: Option<&OsStr> = None;
        assert!(path_component_string(none).is_none());
    }
}
