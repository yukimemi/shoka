//! `shoka import` — adopt an existing ghq-style tree into the shelf.
//!
//! Recursively walks the source directory looking for `.git/`
//! subdirectories, opens each one in-process via [`gix::open`], and
//! reads the default remote's fetch URL. The URL is parsed into a
//! `(host, owner, name)` triple and inserted into the shelf
//! ([`crate::state::Shelf`]).
//!
//! No `git` subprocess is spawned — gix does everything in-process.
//! That matters most on Windows, where `CreateProcess` is slow
//! enough to dominate the import for any non-trivial tree.
//!
//! When the source path is omitted, the command falls back to an
//! [`inquire`] picker over common candidate dirs (`~/ghq`, `~/src`,
//! `~/dev`, …) that actually exist on disk. If none exist, the
//! command errors out asking for an explicit path.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use inquire::Select;
use owo_colors::OwoColorize;
use walkdir::WalkDir;

use crate::cli::ImportArgs;
use crate::commands::ShokaContext;
use crate::state::{Repo, Shelf};

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
    let mut skipped_no_remote = 0usize;
    let mut errors = 0usize;

    println!(
        "{} scanning {} for git repos…",
        "import:".bold(),
        source.display()
    );

    // We use the explicit iterator (rather than the implicit
    // `for in IntoIter`) so we can call `it.skip_current_dir()` the
    // moment we recognise a `.git/`. Without that hand-off, walkdir
    // happily descends into `objects/` / `refs/` for every repo —
    // tens of thousands of dead-end entries per shelf. The earlier
    // comment claimed we didn't descend; we now actually don't.
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

        if entry.file_name() != ".git" || !entry.path().is_dir() {
            continue;
        }
        // Found a repo — record it, then prune the walk so the
        // object store doesn't get crawled.
        it.skip_current_dir();

        let repo_root = match entry.path().parent() {
            Some(p) => p,
            None => continue,
        };

        match extract_repo(repo_root) {
            Ok(Some(repo)) => {
                let slug = repo.slug();
                match shelf.add(repo) {
                    Ok(()) => {
                        println!("  {} {slug}", "+".green());
                        imported += 1;
                    }
                    Err(_) => {
                        // Add failed → triple already on shelf.
                        skipped_already += 1;
                    }
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target: "shoka",
                    "no default remote at {}",
                    repo_root.display()
                );
                skipped_no_remote += 1;
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
    if skipped_no_remote > 0 {
        println!(
            "  {} {} repos with no remote (left alone)",
            "↩".dimmed(),
            skipped_no_remote
        );
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

/// Read the default remote's fetch URL from `repo_root` and build a
/// [`Repo`] from it.
///
/// Returns `Ok(None)` when the repo opens fine but has no default
/// remote (e.g. a local-only experiment) — that's intentional, not
/// an error: skip and keep going. Real read errors propagate.
fn extract_repo(repo_root: &Path) -> Result<Option<Repo>> {
    let repo = gix::open(repo_root)
        .with_context(|| format!("opening {} as a git repo", repo_root.display()))?;
    let Some(remote_result) = repo.find_default_remote(gix::remote::Direction::Fetch) else {
        return Ok(None);
    };
    let remote = remote_result.context("resolving default remote")?;
    let Some(url) = remote.url(gix::remote::Direction::Fetch) else {
        return Ok(None);
    };
    let parts = parse_remote_url(url).with_context(|| format!("parsing remote URL `{url}`"))?;
    Ok(Some(Repo::new(parts.host, parts.owner, parts.name)))
}

/// Parsed `(host, owner, name)` triple from a gix [`Url`].
///
/// `Debug` is required by the tests' `unwrap_err` (which formats
/// the `Ok` variant on panic to explain the unexpected success).
///
/// [`Url`]: gix::Url
#[derive(Debug)]
struct RemoteParts {
    host: String,
    owner: String,
    name: String,
}

fn parse_remote_url(url: &gix::Url) -> Result<RemoteParts> {
    let host = url.host().context("remote URL has no host")?.to_string();
    // gix stores paths as bytes; valid SSH / HTTPS git URLs are
    // ASCII in practice, but go through `to_str` for safety.
    //
    // Trim slashes *first*, then strip the `.git` suffix exactly
    // once. Order matters: `owner/repo.git/` would otherwise survive
    // the suffix step ("doesn't end with `.git`") and pass through
    // with the bogus dotfile attached to the name.
    let trimmed = std::str::from_utf8(url.path.as_ref())
        .context("remote URL path is not UTF-8")?
        .trim_matches('/');
    let path = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut iter = path.splitn(2, '/');
    let owner = iter
        .next()
        .filter(|s| !s.is_empty())
        .context("remote URL has no owner segment")?
        .to_string();
    let name = iter
        .next()
        .filter(|s| !s.is_empty())
        .context("remote URL has no name segment")?
        .to_string();
    // Reject deeper paths (`github.com/foo/bar/baz`) — that's not
    // a shape this shelf understands. Better to surface the error
    // than silently lose the trailing segments.
    if name.contains('/') {
        bail!("remote URL path `{path}` has more than two segments");
    }
    Ok(RemoteParts { host, owner, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url_str: &str) -> Result<RemoteParts> {
        let url = gix::url::parse(url_str.into())
            .with_context(|| format!("parsing test URL `{url_str}`"))?;
        parse_remote_url(&url)
    }

    #[test]
    fn ssh_url() {
        let p = parse("git@github.com:foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn ssh_url_without_dot_git() {
        let p = parse("git@github.com:foo/bar").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn https_url() {
        let p = parse("https://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn https_url_without_dot_git() {
        let p = parse("https://github.com/foo/bar").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn ssh_url_with_trailing_slash() {
        let p = parse("https://github.com/foo/bar/").unwrap();
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn trailing_slash_after_dot_git_strips_both() {
        // Regression for the slash-before-strip ordering: with the
        // naive `trim_end(".git").trim_end('/')` pair, this would
        // leave `name = "bar.git"` because `.git` didn't end the
        // string yet. trim-then-strip gets it right.
        let p = parse("https://github.com/foo/bar.git/").unwrap();
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn gitlab_subgroup_rejected_as_too_deep() {
        // gitlab allows `gitlab.com/group/subgroup/repo`. shoka's
        // (host, owner, name) shape can't represent that; surface
        // the mismatch rather than silently keep just the leading
        // segments.
        let err = parse("https://gitlab.com/group/subgroup/repo.git").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("more than two segments") || msg.contains("parsing remote URL"),
            "expected too-deep error, got {msg}"
        );
    }

    #[test]
    fn ssh_alternate_user_segment_drops_user() {
        // `ssh://git@gh.example.com/foo/bar.git` — the user (`git`)
        // is part of the URL's userinfo, not the path. gix exposes
        // host = "gh.example.com", path = "foo/bar.git".
        let p = parse("ssh://git@gh.example.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "gh.example.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }
}
