//! `shoka prune` — drop shelf entries whose clone path has gone.
//!
//! Walks the shelf, resolves each repo's clone path, and flags any
//! whose path is missing on disk. Those become removal candidates.
//!
//! Modes:
//!
//! - `--dry-run` → just print the candidate list. Default-safe rehearsal.
//! - `--yes` → remove without the interactive confirmation.
//! - Neither → print + prompt; on assent, remove + save.
//!
//! Scope is intentionally narrow for Phase 1: filesystem presence
//! only. "Remote default branch has merged → suggest removal" is a
//! reasonable future addition but needs network access, octocrab,
//! and a conscious "is this what the user wants" UX choice — not
//! implemented now.

use anyhow::{Context, Result, bail};
use inquire::Confirm;
use owo_colors::OwoColorize;
use teravars::Engine;

use crate::cli::PruneArgs;
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::state::Shelf;

pub async fn run(ctx: &ShokaContext, args: PruneArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let mut shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        println!("{} shelf is empty — nothing to prune", "prune:".bold());
        return Ok(());
    }

    let candidates = find_stale(&shelf, &resolved)?;

    println!(
        "{} {} of {} repo(s) have missing clone paths",
        "prune:".bold(),
        candidates.len(),
        shelf.len()
    );

    if candidates.is_empty() {
        return Ok(());
    }

    for stale in &candidates {
        println!(
            "  {} {} {} {}",
            "✗".red(),
            stale.slug.bold(),
            "→".dimmed(),
            stale.path.dimmed()
        );
    }

    if args.dry_run {
        println!();
        println!(
            "{} dry run — re-run without `--dry-run` to remove (or add `--yes` to skip the prompt)",
            "prune:".bold()
        );
        return Ok(());
    }

    if !args.yes {
        let confirmed = Confirm::new(&format!(
            "Remove {} stale entr{} from the shelf?",
            candidates.len(),
            if candidates.len() == 1 { "y" } else { "ies" }
        ))
        .with_default(false)
        .prompt()
        .context("prune confirmation cancelled")?;
        if !confirmed {
            println!("{} aborted — shelf unchanged", "prune:".bold().yellow());
            return Ok(());
        }
    }

    let mut removed = 0usize;
    for stale in &candidates {
        if shelf
            .remove(&stale.host, &stale.owner, &stale.name)
            .is_some()
        {
            removed += 1;
        }
    }
    shelf.save(&ctx.paths)?;

    if removed == 0 {
        // Shouldn't happen — candidates came from this shelf — but
        // surface the surprise rather than silently report success.
        bail!("internal: candidates list and shelf disagreed; no entries removed");
    }
    println!(
        "{} removed {removed} stale entr{} ({} on shelf now)",
        "prune:".bold().green(),
        if removed == 1 { "y" } else { "ies" },
        shelf.len()
    );
    Ok(())
}

/// One stale shelf entry's identity + resolved path, captured for
/// display + removal.
#[derive(Debug, Clone)]
struct Stale {
    host: String,
    owner: String,
    name: String,
    slug: String,
    path: String,
}

/// Walk the shelf, resolve each repo's clone path, return the ones
/// whose path is missing on disk. Resolution shares one Tera engine
/// across the walk — same trick as `shoka list`.
fn find_stale(shelf: &Shelf, resolved: &ResolvedConfig) -> Result<Vec<Stale>> {
    let mut engine = Engine::new();
    let mut out = Vec::new();
    for repo in &shelf.repos {
        let path = resolved
            .clone_path_for(repo, &mut engine)
            .with_context(|| format!("resolving clone path for {}", repo.slug()))?;
        if !path.is_dir() {
            out.push(Stale {
                host: repo.host.clone(),
                owner: repo.owner.clone(),
                name: repo.name.clone(),
                slug: repo.slug(),
                path: path.display().to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalConfig;
    use crate::state::Repo;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_resolved(root: &str) -> ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some(root.into()),
                layout: "{{ root }}/{{ name }}".into(),
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn find_stale_only_flags_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let resolved = make_resolved(tmp.path().to_string_lossy().as_ref());
        let mut shelf = Shelf::default();
        shelf.add(Repo::new("github.com", "u", "alive")).unwrap();
        shelf.add(Repo::new("github.com", "u", "ghost")).unwrap();
        // Only `alive`'s clone path exists on disk.
        std::fs::create_dir_all(tmp.path().join("alive")).unwrap();

        let stale = find_stale(&shelf, &resolved).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "ghost");
    }

    #[test]
    fn find_stale_empty_when_all_paths_present() {
        let tmp = TempDir::new().unwrap();
        let resolved = make_resolved(tmp.path().to_string_lossy().as_ref());
        let mut shelf = Shelf::default();
        shelf.add(Repo::new("github.com", "u", "a")).unwrap();
        shelf.add(Repo::new("github.com", "u", "b")).unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();

        let stale = find_stale(&shelf, &resolved).unwrap();
        assert!(
            stale.is_empty(),
            "all paths exist → no candidates: {stale:?}"
        );
    }
}
