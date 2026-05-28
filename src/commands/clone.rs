//! `shoka clone` — clone a repo onto the shelf.
//!
//! Input shapes (see [`crate::remote::parse_clone_input`]):
//!
//! - Full URL — `https://github.com/foo/bar.git`, `git@host:foo/bar.git`,
//!   `ssh://git@host/foo/bar.git`, …
//! - `owner/name` — combined with `default_host` + `default_protocol`
//!   from the resolved config.
//! - `host/owner/name` — embedded host overrides `default_host`.
//!
//! Argument omitted ⇒ interactive [`inquire::Text`] prompt. A
//! gh-API-backed fuzzy picker over the user's own repos lands in a
//! follow-up PR (needs `octocrab` + token wiring); the prompt is the
//! phase-1 stand-in.
//!
//! Backend selection:
//!
//! - `vcs = git` (or `auto`) → in-process via [`gix::prepare_clone`]
//!   to avoid the `git` subprocess spawn cost on Windows. Runs inside
//!   [`tokio::task::spawn_blocking`] because gix's clone API is sync.
//! - `vcs = jj` → spawn `jj git clone` subprocess. No Rust crate
//!   exposes jj's plumbing, so the CLI is the only option.
//!
//! Auto ↦ git intentionally: at clone time the `.git/` doesn't exist
//! yet, so the auto-detection that distinguishes the two backends
//! (looking at on-disk markers) can't run. Git is the default because
//! it's the universal substrate; users who want jj declare it
//! explicitly via routes / profile / per-repo `set --vcs jj`.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use inquire::Text;
use owo_colors::OwoColorize;

use crate::cli::CloneArgs;
use crate::commands::ShokaContext;
use crate::config::{ShokaConfig, VcsDefault};
use crate::remote::parse_clone_input;
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: CloneArgs) -> Result<()> {
    let input = match args.url {
        Some(s) => s,
        None => prompt_for_input()?,
    };

    let cfg = ShokaConfig::load(&ctx.paths)?.resolve(ctx.profile_override.as_deref())?;
    let (parts, url) = parse_clone_input(&input, &cfg.default_host, cfg.default_protocol)?;

    let repo = Repo::new(parts.host, parts.owner, parts.name);
    let dest = cfg.clone_path_for_one(&repo)?;
    let target = cfg.resolve_target(&repo.slug());

    if dest_is_occupied(&dest)? {
        bail!(
            "destination {} already exists and is not empty — \
             refusing to clone over existing content",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }

    println!("{} {} → {}", "clone:".bold(), repo.slug(), dest.display());

    match target.default_vcs {
        VcsDefault::Jj => {
            let url_str = url.to_string();
            spawn_jj_clone(&url_str, &dest).await?;
        }
        // Auto routes to git at clone time — see module docs.
        VcsDefault::Git | VcsDefault::Auto => {
            let dest_owned = dest.clone();
            let url_owned = url.clone();
            tokio::task::spawn_blocking(move || gix_clone(url_owned, &dest_owned))
                .await
                .context("gix clone task panicked")?
                .with_context(|| format!("cloning {} via gix", url))?;
        }
    }

    let mut shelf = Shelf::load(&ctx.paths)?;
    if shelf.find(&repo.host, &repo.owner, &repo.name).is_some() {
        // Path didn't exist (we checked) but the shelf already
        // recorded the triple — probably someone deleted the working
        // dir without `shoka prune`. Surface it instead of double-
        // counting, but the clone itself succeeded so don't fail.
        tracing::warn!(
            target: "shoka",
            "shelf already had {} — leaving the existing entry's metadata",
            repo.slug()
        );
    } else {
        shelf.add(repo).context("recording cloned repo on shelf")?;
        shelf.save(&ctx.paths)?;
    }

    println!("{} done", "clone:".bold());
    Ok(())
}

/// Phase-1 fallback when the user calls `shoka clone` with no arg:
/// pop up an [`inquire::Text`] prompt asking for a URL / shorthand.
///
/// The fuzzy gh-API picker over the user's own repos lands in a
/// follow-up (needs `octocrab` + `gh auth token` resolution); this
/// keeps the no-arg path usable in the meantime.
fn prompt_for_input() -> Result<String> {
    let answer = Text::new("URL or owner/name to clone:")
        .with_help_message("e.g. https://github.com/foo/bar or foo/bar")
        .prompt()
        .context("clone input prompt cancelled")?;
    if answer.trim().is_empty() {
        bail!("empty clone input");
    }
    Ok(answer)
}

/// True when `dest` exists *and* contains at least one entry. A
/// completely-empty leftover dir (left by an aborted clone, say) is
/// fine: gix can reuse it, and any other tooling treats it the same.
fn dest_is_occupied(dest: &Path) -> Result<bool> {
    match std::fs::read_dir(dest) {
        Ok(mut it) => Ok(it.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", dest.display())),
    }
}

/// Blocking gix clone. Caller wraps in [`spawn_blocking`] so the
/// async runtime isn't held up by network IO + checkout.
///
/// [`spawn_blocking`]: tokio::task::spawn_blocking
fn gix_clone(url: gix::Url, dest: &Path) -> Result<()> {
    // Per-clone interrupt flag. Phase 1 doesn't wire a Ctrl-C
    // handler into gix (the global signal handler still kills the
    // process — gix just won't be able to clean up partial state).
    // The hook lands when the upcoming `gix::interrupt::init_handler`
    // wiring is added across the whole CLI.
    let interrupt = AtomicBool::new(false);

    let mut prepare_fetch = gix::prepare_clone(url, dest).context("preparing clone")?;
    let (mut prepare_checkout, _outcome) = prepare_fetch
        .fetch_then_checkout(gix::progress::Discard, &interrupt)
        .context("fetching from remote")?;
    let (_repo, _outcome) = prepare_checkout
        .main_worktree(gix::progress::Discard, &interrupt)
        .context("checking out main worktree")?;
    Ok(())
}

async fn spawn_jj_clone(url: &str, dest: &Path) -> Result<()> {
    let jj = which::which("jj").context("`jj` not found on PATH (required for vcs = jj)")?;
    let mut cmd = tokio::process::Command::new(&jj);
    cmd.arg("git").arg("clone").arg(url).arg(dest);
    // See `crate::silent_creation_flags` for the rationale. The
    // foreground `clone` path normally has a console (no flash), but
    // applying the flag uniformly keeps the spawn pattern consistent
    // and protects against future callers that fold this into a
    // detached refresh.
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning `{} git clone`", jj.display()))?;
    if !status.success() {
        bail!("jj git clone exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_is_occupied_missing_dir_is_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp.path().join("not-there");
        assert!(!dest_is_occupied(&absent).unwrap());
    }

    #[test]
    fn dest_is_occupied_empty_dir_is_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!dest_is_occupied(&empty).unwrap());
    }

    #[test]
    fn dest_is_occupied_nonempty_dir_is_blocked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let occupied = tmp.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("leftover.txt"), "hi").unwrap();
        assert!(dest_is_occupied(&occupied).unwrap());
    }
}
