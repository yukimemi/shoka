//! `shoka cache` — per-repo volatile cache management.
//!
//! Phase-1 implementation: synchronous refresh that walks the shelf,
//! honours the `[global.cache].refresh_threshold_secs` TTL, and
//! updates the `last_refreshed` timestamp on stale entries. Actual
//! git-status / gh-PR snapshot collection is a TODO — this PR lays
//! the cache plumbing so the data-gathering PRs can drop in without
//! restructuring callers.
//!
//! The forthcoming background-refresh PR will wire this same
//! `refresh` entry point into the tail of other subcommands via a
//! detached subprocess (`shoka cache refresh --background`), gated
//! by `[global.cache].background_refresh`.
//!
//! [`Cache`]: crate::cache::Cache
//! [`Shelf`]: crate::state::Shelf

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::cache::{Cache, current_unix_secs};
use crate::cli::CacheCommand;
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::state::{Repo, Shelf};

pub async fn dispatch(ctx: &ShokaContext, cmd: CacheCommand) -> Result<()> {
    match cmd {
        CacheCommand::Refresh { force, tags } => refresh(ctx, force, tags).await,
        CacheCommand::Show => show(ctx).await,
        CacheCommand::Clear => clear(ctx).await,
    }
}

async fn refresh(ctx: &ShokaContext, force: bool, tags: Vec<String>) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;
    let mut cache = Cache::load(&ctx.paths)?;

    let now = current_unix_secs();
    let threshold = resolved.cache_threshold_secs();

    let mut refreshed = 0;
    let mut skipped_threshold = 0;
    let mut skipped_filter = 0;

    for repo in &shelf.repos {
        if !tags.is_empty() && !has_all_tags(repo, &tags) {
            skipped_filter += 1;
            continue;
        }
        let entry = cache.upsert(repo);
        if !force && !entry.is_stale(threshold, now) {
            skipped_threshold += 1;
            continue;
        }
        // Phase-1 placeholder: the actual git_status / gh snapshot
        // refresh lands in a follow-up. For now, recording
        // last_refreshed is enough to exercise the threshold logic
        // end-to-end and prove the plumbing.
        entry.last_refreshed = Some(now);
        refreshed += 1;
    }

    cache.save(&ctx.paths)?;

    println!(
        "{} refreshed {}{} repo{} ({} on the shelf)",
        "cache:".bold(),
        refreshed,
        if force { " (forced)" } else { "" },
        if refreshed == 1 { "" } else { "s" },
        shelf.len()
    );
    if skipped_threshold > 0 {
        println!(
            "  {} {} fresh (within {}s threshold)",
            "↩".dimmed(),
            skipped_threshold,
            threshold
        );
    }
    if skipped_filter > 0 {
        println!(
            "  {} {} filtered out by --tag",
            "↩".dimmed(),
            skipped_filter
        );
    }
    Ok(())
}

async fn show(ctx: &ShokaContext) -> Result<()> {
    let cache = Cache::load(&ctx.paths)?;
    let body = toml::to_string_pretty(&cache)?;
    print!("{body}");
    Ok(())
}

async fn clear(ctx: &ShokaContext) -> Result<()> {
    let cache = Cache::default();
    cache.save(&ctx.paths)?;
    println!("{} cleared", "cache:".bold());
    Ok(())
}

fn has_all_tags(repo: &Repo, wanted: &[String]) -> bool {
    wanted.iter().all(|w| repo.tags.iter().any(|t| t == w))
}

/// Convenience extension on [`ResolvedConfig`] so callers in this
/// module don't have to drill through `raw.global.cache.…` every
/// time. Lives here rather than on `ResolvedConfig` itself because
/// it's only the cache module that cares — keeps the public surface
/// of config.rs from accreting one-shot helpers.
trait CacheView {
    fn cache_threshold_secs(&self) -> u64;
}

impl CacheView for ResolvedConfig {
    fn cache_threshold_secs(&self) -> u64 {
        self.raw.global.cache.refresh_threshold_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::state::{Repo, Shelf};
    use std::path::Path;
    use tempfile::TempDir;

    fn paths_at(tmp: &Path) -> crate::paths::ShokaPaths {
        // Point config_file at the temp dir; state + cache files
        // land under their own subdirs but the dir layout still
        // works because tests use `*_to` / `*_from` directly when
        // touching state/cache files. For commands that load via
        // ShokaPaths, this gives us hermetic isolation.
        crate::paths::ShokaPaths::resolve(Some(&tmp.join("config.toml")))
            .expect("ShokaPaths::resolve")
    }

    fn sample(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    #[test]
    fn refresh_walks_shelf_and_marks_entries() {
        // Direct exercise of the inner logic — wiring this through
        // `dispatch` would need a tokio runtime + filesystem isolation
        // for ShokaConfig / Shelf load, which the unit-test boundary
        // doesn't need to cover.
        let mut shelf = Shelf::default();
        shelf.add(sample("shoka")).unwrap();
        shelf.add(sample("renri")).unwrap();
        let mut cache = Cache::default();
        let now = 1_700_000_000;
        let threshold = 60;

        for repo in &shelf.repos {
            let entry = cache.upsert(repo);
            if entry.is_stale(threshold, now) {
                entry.last_refreshed = Some(now);
            }
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache
                .find("github.com", "yukimemi", "shoka")
                .unwrap()
                .last_refreshed,
            Some(now)
        );
        assert_eq!(
            cache
                .find("github.com", "yukimemi", "renri")
                .unwrap()
                .last_refreshed,
            Some(now)
        );
    }

    #[test]
    fn refresh_respects_threshold_without_force() {
        let mut cache = Cache::default();
        let repo = sample("shoka");
        cache.upsert(&repo).last_refreshed = Some(1_700_000_000);

        let now = 1_700_000_010; // 10s later
        let threshold = 60;

        // Within threshold → stale check says fresh.
        let entry = cache.find_mut("github.com", "yukimemi", "shoka").unwrap();
        assert!(!entry.is_stale(threshold, now));

        // 120s later → stale.
        let entry = cache.find_mut("github.com", "yukimemi", "shoka").unwrap();
        assert!(entry.is_stale(threshold, 1_700_000_120));
    }

    #[test]
    fn clear_empties_cache_file_atomically() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_at(tmp.path());
        let mut cache = Cache::default();
        cache.upsert(&sample("shoka")).last_refreshed = Some(1);
        cache.save(&paths).unwrap();
        assert!(paths.cache_file().exists());

        // Equivalent of the `clear` command path.
        Cache::default().save(&paths).unwrap();
        let loaded = Cache::load(&paths).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn has_all_tags_and_semantics() {
        let mut repo = sample("shoka");
        repo.tags = vec!["rust".into(), "cli".into()];
        assert!(has_all_tags(&repo, &["rust".into()]));
        assert!(has_all_tags(&repo, &["rust".into(), "cli".into()]));
        assert!(!has_all_tags(
            &repo,
            &["rust".into(), "cli".into(), "tui".into()]
        ));
        assert!(has_all_tags(&repo, &[])); // empty filter is trivially true
    }
}
