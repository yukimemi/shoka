//! `shoka cd` — resolve a shelf entry to its on-disk path.
//!
//! Doesn't actually change the parent shell's cwd — a child process
//! can't. Instead, the chosen repo's clone path is printed verbatim
//! to **stdout**, and a thin shell wrapper installed via
//! `shoka init-shell <shell>` runs `cd "$(shoka cd $args)"` so the
//! parent shell does the actual cd.
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
//! being printed. A stale shelf entry (repo moved / deleted)
//! produces a clear shoka error rather than the shell's confusing
//! `cd: No such file or directory`.

use anyhow::{Context, Result, bail};
use inquire::Select;

use crate::cli::CdArgs;
use crate::commands::ShokaContext;
use crate::config::ShokaConfig;
use crate::state::{Repo, Shelf};

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

    // The shell wrapper consumes stdout, so be strict: only the path,
    // no decoration, no trailing color codes.
    println!("{}", path.display());
    Ok(())
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
/// for the TUI). Items are labelled by [`Repo::slug`] so the picker
/// shows the same identifier `shoka list` does.
fn fuzzy_pick<'a>(candidates: &[&'a Repo], prompt: &str) -> Result<&'a Repo> {
    if candidates.is_empty() {
        // Defensive: callers already filter to non-empty, but in case
        // a future caller forgets, surface the empty-case explicitly.
        bail!("nothing to pick — candidate list is empty");
    }
    let labels: Vec<String> = candidates.iter().map(|r| r.slug()).collect();
    let chosen = Select::new(prompt, labels.clone())
        .prompt()
        .context("repo selection cancelled")?;
    let idx = labels
        .iter()
        .position(|l| l == &chosen)
        .context("picker returned an unknown label")?;
    Ok(candidates[idx])
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
