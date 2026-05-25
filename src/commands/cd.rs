//! `shoka cd` — resolve a shelf entry to its on-disk path.
//!
//! Doesn't actually change the parent shell's cwd — a child process
//! can't. Instead, the chosen repo's clone path is emitted via one
//! of two channels:
//!
//! - **`$SHOKA_CD_OUT` (wrapper contract)** — when this env var is
//!   set, the path is written to the named file and nothing goes to
//!   stdout. The shell wrapper installed by `shoka init-shell`
//!   creates a temp file, points the env var at it, redirects shoka
//!   cd's stdout to stderr (so `inquire`'s prompt UI is *visible* to
//!   the user instead of being captured), and finally reads the temp
//!   file to do the `cd`. This sidechannel is the only safe way to
//!   pair an interactive picker with a captured path — `inquire`
//!   0.9 writes its UI to stdout and exposes no public switch to
//!   stderr, so the wrapper must give the path its own channel.
//! - **stdout (manual contract)** — when the env var is unset, the
//!   path is printed to stdout. Useful when invoking `shoka cd`
//!   directly to feed a script or copy a path.
//!
//! Matching:
//!
//! - Arg omitted → fuzzy select over the entire shelf (optionally
//!   filtered by `--tag`).
//! - Arg given → filter candidates whose slug contains the hint as a
//!   substring (case-insensitive). One match ⇒ use it. Multiple ⇒
//!   fuzzy pick among them. Zero ⇒ error out instead of silently
//!   falling back to the full shelf — surprising "I typed the wrong
//!   thing and ended up somewhere else" beats a clear "no match".
//!
//! Sanity: the resolved path is verified to exist on disk before
//! being emitted. A stale shelf entry (repo moved / deleted)
//! produces a clear shoka error rather than the shell's confusing
//! `cd: No such file or directory`.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use inquire::Select;

use crate::cli::CdArgs;
use crate::commands::ShokaContext;
use crate::config::ShokaConfig;
use crate::state::{Repo, Shelf};

/// Env var the shell wrapper uses to receive the resolved path
/// out-of-band from stdout. See module docs.
pub const CD_OUT_ENV: &str = "SHOKA_CD_OUT";

pub async fn run(ctx: &ShokaContext, args: CdArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        bail!(
            "shelf is empty — nothing to cd into. `shoka clone <url>` or \
             `shoka import <dir>` first"
        );
    }

    // Tag filter first, then hint filter — mirrors `shoka list`'s
    // order so the two commands stay predictable together.
    let tag_filtered: Vec<&Repo> = if args.tags.is_empty() {
        shelf.repos.iter().collect()
    } else {
        shelf
            .repos
            .iter()
            .filter(|r| args.tags.iter().all(|t| r.tags.iter().any(|rt| rt == t)))
            .collect()
    };
    if tag_filtered.is_empty() {
        bail!(
            "no repos matched the tag filter ({} on the shelf total)",
            shelf.len()
        );
    }

    let chosen = match args.repo.as_deref() {
        Some(hint) => choose_by_hint(&tag_filtered, hint)?,
        None => fuzzy_pick(&tag_filtered, "cd to:")?,
    };

    let path = resolved.clone_path_for_one(chosen)?;
    if !path.is_dir() {
        bail!(
            "{} resolves to {}, but that path doesn't exist — \
             the repo was probably moved or deleted; \
             try `shoka prune` to clean up the shelf",
            chosen.slug(),
            path.display()
        );
    }

    emit_path(&path)?;
    Ok(())
}

/// Emit the resolved path. When [`CD_OUT_ENV`] is set, write to that
/// file (and nothing to stdout — the wrapper rendered the prompt UI
/// on stdout-redirected-to-stderr, and reads the path back from this
/// file). When unset, write to stdout for the direct-invocation case.
///
/// Public so `shoka tui` can reuse the exact same sidechannel
/// contract for its Enter-to-cd flow: the same shell wrapper that
/// services `shoka cd` also picks up the path that `shoka tui`
/// writes when the user picks a repo.
pub fn emit_path(path: &Path) -> Result<()> {
    let rendered = path.to_string_lossy();
    match std::env::var_os(CD_OUT_ENV) {
        Some(out) if !out.is_empty() => {
            std::fs::write(&out, rendered.as_bytes()).with_context(|| {
                format!(
                    "writing path to ${CD_OUT_ENV}={}",
                    Path::new(&out).display()
                )
            })
        }
        // Trailing newline only on the stdout path: command substitution
        // strips it, while the sidechannel reader doesn't expect one.
        _ => {
            println!("{rendered}");
            Ok(())
        }
    }
}

/// Match `hint` against the candidate slugs (case-insensitive
/// substring). One hit ⇒ return it directly. Multiple ⇒ open a
/// fuzzy picker pre-seeded with the narrowed set. Zero ⇒ error out.
fn choose_by_hint<'a>(candidates: &[&'a Repo], hint: &str) -> Result<&'a Repo> {
    let hint_lc = hint.to_lowercase();
    let matches: Vec<&'a Repo> = candidates
        .iter()
        .copied()
        .filter(|r| r.slug().to_lowercase().contains(&hint_lc))
        .collect();

    match matches.len() {
        0 => bail!(
            "no repos on the shelf match `{hint}` — \
             try `shoka list` to see what's there"
        ),
        1 => Ok(matches[0]),
        _ => fuzzy_pick(&matches, &format!("multiple matches for `{hint}`:")),
    }
}

/// Fuzzy-select among `candidates` via [`inquire::Select`] (which
/// uses its own internal fuzzy match algorithm; nucleo only lands
/// for the TUI). Items are wrapped in a thin `Display` adapter so
/// the picker can return the chosen [`Repo`] reference directly —
/// no string round-trip + linear scan on the way back.
fn fuzzy_pick<'a>(candidates: &[&'a Repo], prompt: &str) -> Result<&'a Repo> {
    if candidates.is_empty() {
        // Defensive: callers already filter to non-empty, but in case
        // a future caller forgets, surface the empty-case explicitly.
        bail!("nothing to pick — candidate list is empty");
    }

    /// `inquire::Select` requires its options to be `Display` and
    /// returns the chosen option by value. A reference-carrying
    /// wrapper lets us hand the picker borrowed `&Repo`s and pull the
    /// reference back out without reallocating slug strings.
    #[derive(Clone)]
    struct RepoItem<'r>(&'r Repo);
    impl fmt::Display for RepoItem<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0.slug())
        }
    }

    let items: Vec<RepoItem<'a>> = candidates.iter().copied().map(RepoItem).collect();
    let chosen = Select::new(prompt, items)
        .prompt()
        .context("repo selection cancelled")?;
    Ok(chosen.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GlobalConfig, VcsDefault};
    use std::collections::BTreeMap;

    fn r(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    fn r_owned(owner: &str, name: &str) -> Repo {
        Repo::new("github.com", owner, name)
    }

    #[test]
    fn hint_filters_to_substring_case_insensitive() {
        let a = r("shoka");
        let b = r("renri");
        let c = r("kanade");
        let candidates: Vec<&Repo> = vec![&a, &b, &c];

        // Unique substring match → returns the unique candidate.
        let picked = choose_by_hint(&candidates, "ren").unwrap();
        assert_eq!(picked.name, "renri");

        // Case-insensitive — uppercase hint still matches.
        let picked = choose_by_hint(&candidates, "RENRI").unwrap();
        assert_eq!(picked.name, "renri");
    }

    #[test]
    fn hint_with_zero_matches_errors_cleanly() {
        let a = r("shoka");
        let candidates: Vec<&Repo> = vec![&a];
        let err = choose_by_hint(&candidates, "no-such-thing").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no repos on the shelf match"),
            "expected no-match error, got: {msg}"
        );
    }

    #[test]
    fn hint_substring_matches_owner_or_host() {
        // The hint matches against the full slug, not just the name —
        // so a hint that matches by owner works too. Useful when the
        // user remembers "the rust-org one" but not the project name.
        let a = r_owned("rust-org", "alpha");
        let b = r_owned("other-org", "beta");
        let candidates: Vec<&Repo> = vec![&a, &b];
        let picked = choose_by_hint(&candidates, "rust-org").unwrap();
        assert_eq!(picked.name, "alpha");
    }

    fn resolved_with_layout(layout: &str, root: &str) -> crate::config::ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some(root.into()),
                layout: layout.into(),
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn clone_path_uses_layout_so_cd_lands_where_clone_left_it() {
        // Sanity: cd and clone share the same path-resolution machine.
        // If they ever drift apart, this test fails with a path
        // mismatch — easier to debug than a runtime cd-to-empty-dir.
        let r = r("shoka");
        let resolved = resolved_with_layout("{{ root }}/{{ name }}", "/data");
        let p = resolved.clone_path_for_one(&r).unwrap();
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(
            s.ends_with("/data/shoka"),
            "cd path should follow the configured layout, got {s:?}"
        );
        // Touch the unused VcsDefault import so this stays a no-op
        // compile dependency rather than a dead import.
        let _ = VcsDefault::Auto;
    }
}
