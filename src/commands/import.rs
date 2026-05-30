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
//! `owner = <parent dir name>`, `name = <repo dir name>`.
//!
//! Either way, the repo's absolute on-disk path is pinned via
//! [`Repo::with_path`] so `cd` / `tui` resolve to the exact location
//! we just walked to rather than running the entry through
//! `[global].layout`. That matters because `git clone <url> <other-name>`
//! is officially supported (and a local rename of the working tree
//! is just as legitimate) — the on-disk dir name can legitimately
//! differ from the URL-derived `name`, and the importer must honour
//! that. Repos stay where they are — shoka doesn't move local
//! checkouts.
//!
//! Shelf identity is `(host, owner, name, path?)`. Same triple with
//! different paths is allowed — that's exactly the
//! `git clone <url> <other-name>` / local-rename / multi-checkout
//! case. Re-running `shoka import` distinguishes three sub-cases:
//!
//! - **exact (triple, path) match** → skip ("already on shelf")
//! - **triple matches, existing entry has `path = None`** → fill in
//!   the path (self-heal for shelves imported before the always-pin
//!   behaviour landed, or `shoka clone`-laid-out entries that the
//!   user has since moved on disk)
//! - **triple matches, existing entry has a different `path`** →
//!   add as a new entry (a second checkout of the same remote)
//! - **triple not on shelf** → add
//!
//! No `git` subprocess is spawned — gix does everything in-process.
//! That matters most on Windows, where `CreateProcess` is slow enough
//! to dominate the import for any non-trivial tree.
//!
//! When the source path is omitted, the command falls back to an
//! [`inquire`] picker over common candidate dirs (`~/ghq`, `~/src`,
//! `~/dev`, …) that actually exist on disk. If none exist, the
//! command errors out asking for an explicit path.

use std::collections::HashSet;
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
    let mut updated = 0usize;
    let mut skipped_already = 0usize;
    let mut errors = 0usize;
    // Repo roots already imported in this run, keyed by the parent
    // directory of the marker (`.git` / `.jj`). Used to dedupe
    // colocated checkouts: the first marker yielded (`.git` by
    // walkdir's alphabetical ordering) records the root here, and
    // the sibling `.jj` is then skipped. `skip_current_dir()` only
    // prunes *descendants* of the yielded entry — it has no effect
    // on siblings — so an explicit set is the only reliable way to
    // avoid double-importing the same repo.
    let mut imported_roots: HashSet<PathBuf> = HashSet::new();

    println!(
        "{} scanning {} for git / jj repos…",
        "import:".bold(),
        source.display()
    );

    // Explicit iterator so we can call `skip_current_dir()` to keep
    // the walk from descending into `.git/objects` / `.jj/op_store`
    // etc. — tens of thousands of dead-end entries otherwise. The
    // method *only* prunes descendants of the just-yielded entry,
    // not siblings; colocated-checkout dedup is done via
    // `imported_roots` below.
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
        let fname = entry.file_name();
        let is_marker =
            (fname == OsStr::new(".git") || fname == OsStr::new(".jj")) && entry.path().is_dir();
        if !is_marker {
            continue;
        }
        // Prune the walk into the marker dir's contents. Siblings in
        // the parent dir (e.g. a `.jj` next to a `.git`) are NOT
        // pruned by this call — that's what `imported_roots` is for.
        it.skip_current_dir();

        let repo_root = match entry.path().parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };

        // Colocated `.git` + `.jj`: walkdir yields `.git` first
        // (alphabetical), records the root, then yields `.jj` —
        // here we recognise the duplicate and skip it. Single
        // import per repo even with two markers present.
        if !imported_roots.insert(repo_root.clone()) {
            continue;
        }

        let result = match fname {
            // `.git` marker: try the remote URL path first, fall
            // back to synthesised local identity when no remote
            // exists. Both cases keep the repo in place.
            f if f == OsStr::new(".git") => extract_git_repo(&repo_root),
            // `.jj` marker (no colocated `.git` got there first):
            // jj has no concept of a single "default remote URL"
            // shoka could parse, so always synthesise local.
            _ => Ok(synthesise_local(&repo_root)),
        };

        match result {
            Ok(repo) => {
                let slug = repo.slug();
                let outcome = upsert_into_shelf(&mut shelf, repo);
                match outcome {
                    Outcome::Imported => {
                        println!("  {} {slug}", "+".green());
                        imported += 1;
                    }
                    Outcome::PathFilled => {
                        println!("  {} {slug}", "↻".cyan());
                        updated += 1;
                    }
                    Outcome::AlreadyOnShelf => {
                        skipped_already += 1;
                    }
                    Outcome::AddFailed(e) => {
                        tracing::warn!(
                            target: "shoka",
                            "failed to add {slug} to shelf: {e:#}"
                        );
                        errors += 1;
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
    if updated > 0 {
        println!("  {} {} path refreshed", "↻".cyan(), updated);
    }
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

/// What happened when the importer tried to fold one repo into the
/// shelf. Mirrors the user-visible summary so the main loop just
/// counts + prints.
enum Outcome {
    /// New row added (either a fresh triple, or a second checkout
    /// of an existing remote at a different path).
    Imported,
    /// Existing path-less row got its path filled in. The shelf
    /// grew by zero rows but the entry now resolves correctly via
    /// `clone_path_for`.
    PathFilled,
    /// Exact `(triple, path)` already on the shelf; nothing to do.
    AlreadyOnShelf,
    /// `Shelf::add` returned an error. Wrapped so the caller can
    /// log + count without juggling the enum / Result split.
    AddFailed(anyhow::Error),
}

/// Decide whether to add a new row, fill in a path-less twin, or
/// skip outright. See module docs for the full case analysis.
fn upsert_into_shelf(shelf: &mut Shelf, repo: Repo) -> Outcome {
    // Exact `(host, owner, name, path)` already there → nothing to do.
    if shelf
        .find_by_path(&repo.host, &repo.owner, &repo.name, repo.path.as_deref())
        .is_some()
    {
        return Outcome::AlreadyOnShelf;
    }

    // Self-heal the pre-always-pin shelf: when the new entry brings
    // a `path` but the shelf already has a path-less twin for this
    // triple, fill in the path rather than add a duplicate. A
    // path-less entry is by definition the *single* layout-derived
    // checkout for that triple, so it's safe to refine in place.
    //
    // We look up the path-less twin directly via `find_mut_by_path`
    // with `None` — not `find_mut` (triple-only) — because a shelf
    // with multiple checkouts of the same remote might place a
    // path-pinned row first, and `find_mut` would return *that* row
    // and then fail the `path.is_none()` check, missing the
    // path-less twin that actually needs healing.
    if repo.path.is_some() {
        if let Some(existing) = shelf.find_mut_by_path(&repo.host, &repo.owner, &repo.name, None) {
            existing.path = repo.path;
            return Outcome::PathFilled;
        }
    }

    match shelf.add(repo) {
        Ok(()) => Outcome::Imported,
        Err(e) => Outcome::AddFailed(e),
    }
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
/// when the repo simply has no remote / no fetch URL. Genuine
/// config-load or repo-corruption errors propagate as `Err` so the
/// caller logs + counts them rather than masking them as "no remote".
///
/// The returned [`Repo`] always has its `path` pinned to `repo_root`'s
/// absolute form. Git officially supports `git clone <url> <other-name>`
/// (and locally-renamed working trees are equally legitimate), so
/// the URL-derived `name` and the on-disk dir name can legitimately
/// diverge. Without an explicit `path`, `clone_path_for` would route
/// `cd` / `tui` to the layout-derived location, which simply doesn't
/// exist when the dir was renamed.
fn extract_git_repo(repo_root: &Path) -> Result<Repo> {
    let repo = gix::open(repo_root)
        .with_context(|| format!("opening {} as a git repo", repo_root.display()))?;

    // `find_default_remote` returns `Option<Result<Remote>>`:
    //   - outer `None`  → no remote configured, or HEAD is detached
    //                     (common with jj-colocated checkouts where jj
    //                     always leaves HEAD pointing at a commit hash,
    //                     not a branch ref, so gix can't derive the
    //                     branch's tracking remote this way).
    //   - outer `Some(Err)` → genuine resolution error worth surfacing.
    //   - outer `Some(Ok(Remote))` → the configured default remote.
    //
    // `.transpose()?` turns the inner `Result` into a `?`-able error
    // so config / corruption issues propagate properly, instead of
    // being silently downgraded to a `None` URL and hidden behind
    // the synthesise-local fallback.
    let remote = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .transpose()
        .with_context(|| {
            format!(
                "reading default remote configuration for {}",
                repo_root.display()
            )
        })?;

    // When HEAD is detached (typical for jj-colocated checkouts),
    // `find_default_remote` returns `None` even if remotes exist.
    // `remote_default_name` handles the detached-HEAD case correctly:
    // it picks the sole remote when only one is configured, or falls
    // back to "origin" when multiple remotes include it.
    let remote = match remote {
        Some(r) => Some(r),
        None => repo
            .remote_default_name(gix::remote::Direction::Fetch)
            .and_then(|name| repo.find_remote(name.as_ref()).ok()),
    };

    let maybe_url = remote.and_then(|r| r.url(gix::remote::Direction::Fetch).cloned());

    let Some(url) = maybe_url else {
        return Ok(synthesise_local(repo_root));
    };

    match parse_remote_url(&url) {
        Ok(parts) => {
            Ok(Repo::new(parts.host, parts.owner, parts.name).with_path(absolute(repo_root)))
        }
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

/// Absolutise `p`, falling back to the input verbatim if the OS can't
/// give us a cwd (no current directory, weird state). Shared by
/// [`extract_git_repo`] and [`synthesise_local`] so both paths agree
/// on what "absolute" means.
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
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
    let abs = absolute(repo_root);
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
    fn upsert_self_heals_path_less_twin_even_when_path_pinned_row_is_first() {
        // Regression for the Gemini Code Assist finding on PR #59:
        // the original `find_mut` was triple-only and returned the
        // first match. If a path-pinned row preceded the path-less
        // row for the same triple, the importer would land on the
        // pinned row, fail the `is_none()` check, and miss healing
        // the actual path-less twin. Switching to
        // `find_mut_by_path(..., None)` lets us go straight to the
        // path-less row regardless of insertion order.
        let mut shelf = Shelf::default();
        // Insertion order matters: pinned first, path-less second.
        let mut pinned = Repo::new("github.com", "yukimemi", "admintask");
        pinned.path = Some(PathBuf::from("/elsewhere/admintask"));
        shelf.add(pinned).unwrap();
        shelf
            .add(Repo::new("github.com", "yukimemi", "admintask"))
            .unwrap();
        assert_eq!(shelf.len(), 2, "fixture must start with both rows");

        // A new import lands at /here/admintask with the same
        // triple — the path-less twin should get healed, not a
        // third row added.
        let mut incoming = Repo::new("github.com", "yukimemi", "admintask");
        incoming.path = Some(PathBuf::from("/here/admintask"));
        let outcome = upsert_into_shelf(&mut shelf, incoming);
        assert!(
            matches!(outcome, Outcome::PathFilled),
            "expected PathFilled, got something else"
        );
        assert_eq!(shelf.len(), 2, "no new row should be added");
        // The originally-pinned row is untouched.
        let still_pinned = shelf
            .repos
            .iter()
            .find(|r| r.path.as_deref() == Some(Path::new("/elsewhere/admintask")))
            .expect("original pinned row preserved");
        assert_eq!(still_pinned.name, "admintask");
        // The healed row now carries the imported path.
        let healed = shelf
            .repos
            .iter()
            .find(|r| r.path.as_deref() == Some(Path::new("/here/admintask")))
            .expect("path-less row healed to incoming path");
        assert_eq!(healed.name, "admintask");
    }

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

    /// Build a `.git/`-marked repo at `dir` with the given remote
    /// URL via `gix::init` + a hand-written `[remote "origin"]`
    /// stanza. Mirrors the integration-test helper but lives here so
    /// the unit test stays in-crate.
    fn init_git_with_remote(dir: &Path, url: &str) {
        std::fs::create_dir_all(dir).unwrap();
        gix::init(dir).expect("gix init");
        let cfg = dir.join(".git").join("config");
        let mut body = std::fs::read_to_string(&cfg).unwrap();
        body.push_str(&format!(
            "\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        ));
        std::fs::write(&cfg, body).unwrap();
    }

    #[test]
    fn extract_git_repo_pins_path_when_dirname_differs_from_remote_slug() {
        // The headline regression: `git clone <url> <other-name>` (or
        // a local rename) leaves the working tree at a dir whose name
        // doesn't match the URL-derived `name`. Without a `path`
        // override, layout would route `cd` / `tui` to a nonexistent
        // location. We must pin the actual on-disk path.
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("DeviceManagement");
        init_git_with_remote(&repo_root, "https://github.com/yukimemi/admintask.git");

        let r = extract_git_repo(&repo_root).expect("extract");
        assert_eq!(r.host, "github.com");
        assert_eq!(r.owner, "yukimemi");
        // Identity comes from the URL, not the dir name.
        assert_eq!(r.name, "admintask");
        // Path is pinned to the actual on-disk location.
        let pinned = r.path.as_ref().expect("path must be pinned on import");
        assert_eq!(
            std::fs::canonicalize(pinned).unwrap(),
            std::fs::canonicalize(&repo_root).unwrap(),
        );
    }

    #[test]
    fn extract_git_repo_pins_path_even_when_dirname_matches_slug() {
        // For consistency the path is pinned unconditionally, not
        // just on the rename case. Always-pin means re-running
        // `shoka import` after a root move never silently strands
        // entries at stale layout-derived paths.
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("matching-name");
        init_git_with_remote(&repo_root, "https://github.com/yukimemi/matching-name.git");

        let r = extract_git_repo(&repo_root).expect("extract");
        assert_eq!(r.name, "matching-name");
        assert!(
            r.path.is_some(),
            "path should be pinned even in the matching-name case"
        );
    }

    /// Build a `.git/`-marked repo with a remote and a detached HEAD
    /// (a commit hash written directly into `.git/HEAD`). This is the
    /// state jj always produces for colocated checkouts.
    fn init_git_with_remote_detached(dir: &Path, url: &str) {
        init_git_with_remote(dir, url);
        // Write a fake commit hash into HEAD to simulate detached state.
        std::fs::write(
            dir.join(".git").join("HEAD"),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();
    }

    #[test]
    fn extract_git_repo_resolves_remote_when_head_is_detached() {
        // Regression: jj always leaves HEAD pointing at a bare commit
        // hash (detached). `find_default_remote` returns `None` in that
        // case, which previously triggered the synthesise-local fallback
        // even though "origin" was correctly configured.
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("rvpm");
        init_git_with_remote_detached(&repo_root, "https://github.com/yukimemi/rvpm.git");

        let r = extract_git_repo(&repo_root).expect("extract");
        assert_eq!(r.host, "github.com");
        assert_eq!(r.owner, "yukimemi");
        assert_eq!(r.name, "rvpm");
        assert!(r.path.is_some(), "path must be pinned");
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
