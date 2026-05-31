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
//! Argument omitted ⇒ interactive flow:
//!
//! 1. Prompt for an org/user. Default is `[ui].own_owners[0]` from
//!    the resolved config (reuses PR #81's "self" definition), else
//!    the authenticated user's own login (looked up via
//!    `gh.rs::whoami`).
//! 2. Route to the API endpoint that matches the input —
//!    empty / personal login → `/user/repos` (private visible),
//!    other `own_owners` entry → `/orgs/{org}/repos` (org's
//!    private visible to members),
//!    anything else → `/users/{user}/repos` (public only).
//!    All three are `sort=updated`, archived filtered out.
//! 3. Show the result in an `inquire::Select` with fuzzy filtering
//!    over `owner/name` + description; Enter picks one, which the
//!    rest of `run` then clones.
//!
//! Token resolution falls back to a plain `inquire::Text` prompt
//! (the legacy path) when no `GITHUB_TOKEN` / `gh auth` is
//! available — the no-arg path stays usable without auth.
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
use inquire::{Select, Text};
use owo_colors::OwoColorize;

use crate::cli::CloneArgs;
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig, VcsDefault};
use crate::gh;
use crate::remote::parse_clone_input;
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: CloneArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?.resolve(ctx.profile_override.as_deref())?;
    let input = match args.url {
        Some(s) => s,
        None => prompt_for_input(&cfg.ui.own_owners).await?,
    };

    clone_and_record(ctx, &cfg, &input).await?;

    println!("{} done", "clone:".bold());
    Ok(())
}

/// Parse `input`, clone it into the configured layout path, and record
/// it on the shelf. The reusable core shared by `shoka clone` and
/// `shoka new` — both end up needing the exact same "resolve a spec to
/// a destination, fetch it, remember it" sequence, so it lives in one
/// place to keep their path/routing/shelf behaviour identical.
///
/// Returns the recorded [`Repo`] (its resolved `host/owner/name`) so
/// callers like `new` can report or post-process it (e.g. hand the
/// clone path to `kata init`). Prints the `clone: <slug> → <dest>`
/// progress line but NOT the trailing `done` — that's the caller's to
/// emit, since `new` has its own multi-step completion message.
pub async fn clone_and_record(
    ctx: &ShokaContext,
    cfg: &ResolvedConfig,
    input: &str,
) -> Result<Repo> {
    let (parts, url) = parse_clone_input(input, &cfg.default_host, cfg.default_protocol)?;

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
        shelf
            .add(repo.clone())
            .context("recording cloned repo on shelf")?;
        shelf.save(&ctx.paths)?;
    }

    Ok(repo)
}

/// Interactive flow when the user calls `shoka clone` with no arg.
///
/// Authenticated path:
///   org prompt (default = `own_owners[0]` from config, else the
///   personal login)  →  fuzzy `Select` over:
///     - `/user/repos` (self — personal login or empty input),
///     - `/orgs/{org}/repos` (input matches an `own_owners` entry
///       that *isn't* the personal login → assumed to be an org we
///       belong to, so the private repos show up too),
///     - `/users/{user}/repos` (anyone else — public only).
///
/// Unauthenticated path:
///   falls back to a free-form [`Text`] prompt accepting URL /
///   shorthand, so the no-arg form keeps working without `gh auth`.
async fn prompt_for_input(own_owners: &[String]) -> Result<String> {
    let Some(token) = gh::resolve_token().await else {
        return text_fallback_prompt();
    };
    let client = match gh::build_client(&token) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "shoka", "octocrab client build failed: {e:#}");
            return text_fallback_prompt();
        }
    };

    // Always resolve the personal login upfront. We need it to
    // disambiguate "is the typed owner my own login (→ /user/repos
    // for private) or an org I belong to (→ /orgs/{org}/repos)?"
    // when both possibilities live in `[ui].own_owners`.
    let personal_login = match gh::whoami(&client).await {
        Ok(login) => login,
        Err(e) => {
            tracing::warn!(target: "shoka", "gh /user lookup failed: {e:#}");
            return text_fallback_prompt();
        }
    };

    // Prefer the configured `[ui].own_owners[0]` as the default —
    // that's what the user already declared as "theirs" for the
    // TUI's mine-only filter, so reusing it keeps the two commands'
    // notions of "self" in sync. Fall back to the personal login
    // when `own_owners` isn't configured.
    let default_owner = own_owners
        .first()
        .cloned()
        .unwrap_or_else(|| personal_login.clone());

    // Re-prompt loop: a 404 from `/users/{owner}/repos` (typo /
    // private user / deleted account) shouldn't drop the whole
    // clone session — print "no such owner" inline and ask again.
    // Other errors still propagate as fatal: network / auth /
    // schema breakage are nothing the user can fix by retyping.
    //
    // First prompt uses `default_owner` as the pre-fill. After a
    // miss we clear the default — the bad string is gone, and the
    // user has to actually type a real owner instead of bouncing
    // Enter on a stale pre-fill that we already know fails.
    let mut default: Option<&str> = Some(default_owner.as_str());
    loop {
        let mut prompt = Text::new("org or user (Enter = your own repos):");
        if let Some(d) = default {
            prompt = prompt.with_default(d);
        }
        let owner_input = prompt.prompt().context("clone owner prompt cancelled")?;
        let trimmed = owner_input.trim();
        // Empty input — or input that case-insensitively matches
        // the personal login — routes to `/user/repos` (private
        // repos visible). GitHub logins are case-insensitive, so
        // `Yukimemi` and `yukimemi` are the same account; matching
        // case-sensitively would silently misroute private repos
        // to the public-only `/users/{u}/repos` endpoint.
        let want_mine = trimmed.is_empty() || trimmed.eq_ignore_ascii_case(&personal_login);
        let owner_for_query = if want_mine { None } else { Some(trimmed) };
        // Any non-self owner that the user has declared in
        // `own_owners` is treated as an org they belong to — that
        // routes to `/orgs/{org}/repos`, which (with the OAuth
        // token's `read:org` / repo scopes) surfaces the org's
        // private repos. A non-self, non-`own_owners` owner stays
        // on `/users/{u}/repos` (public).
        let is_org = owner_for_query
            .is_some_and(|o| own_owners.iter().any(|own| own.eq_ignore_ascii_case(o)));

        match gh::list_repos(&client, owner_for_query, is_org).await {
            Ok(repos) if repos.is_empty() => {
                eprintln!(
                    "{} no repos found for {} — try another?",
                    "clone:".bold(),
                    owner_for_query.unwrap_or(default_owner.as_str())
                );
                default = None;
                continue;
            }
            Ok(repos) => {
                let picked = Select::new("pick a repo:", repos)
                    .with_page_size(15)
                    .prompt()
                    .context("clone repo select cancelled")?;
                return Ok(format!("{}/{}", picked.owner, picked.name));
            }
            Err(e) if gh::is_not_found(&e) => {
                eprintln!(
                    "{} no such org or user: {} — try another?",
                    "clone:".bold(),
                    owner_for_query.unwrap_or(default_owner.as_str())
                );
                default = None;
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| match owner_for_query {
                    None => "listing your own repos".to_string(),
                    Some(o) => format!("listing repos for {o}"),
                });
            }
        }
    }
}

fn text_fallback_prompt() -> Result<String> {
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
