//! `shoka doctor` — diagnose paths + config resolution.
//!
//! Phase 1's first real command. Prints the resolved [`ShokaPaths`],
//! loads [`ShokaConfig`] (auto-creating a starter file on first run),
//! and reports the effective [`ResolvedConfig`] under the active
//! profile. Useful as a smoke test for new installs and as the
//! rendering target while paths / config code is in flux.
//!
//! [`ShokaConfig`]: crate::config::ShokaConfig
//! [`ResolvedConfig`]: crate::config::ResolvedConfig

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::cache::Cache;
use crate::commands::ShokaContext;
use crate::config::ShokaConfig;
use crate::state::Shelf;

pub async fn run(ctx: &ShokaContext) -> Result<()> {
    let p = &ctx.paths;
    println!("{}", "shoka doctor".bold());
    println!();
    println!("{}", "paths".underline());
    println!("  config file : {}", p.config_file().display());
    println!("  config dir  : {}", p.config_dir().display());
    println!("  state dir   : {}", p.state_dir().display());
    println!("  state file  : {}", p.state_file().display());
    println!("  cache dir   : {}", p.cache_dir().display());
    println!("  cache file  : {}", p.cache_file().display());
    println!();

    let pre_existed = p.config_file().exists();

    // Propagate load / resolve errors via `?` so the process exits
    // non-zero on broken config. Without this, `doctor` would print
    // a red "load failed" line then return Ok and a healthy exit
    // code — masking real problems from scripts / CI.
    let cfg = ShokaConfig::load(p)?;

    println!(
        "{}  ({})",
        "config".underline(),
        if pre_existed {
            "found".green().to_string()
        } else {
            "starter just written — edit `root = ...` to customize"
                .cyan()
                .to_string()
        }
    );

    let profile_name = cfg.resolve_profile_name(ctx.profile_override.as_deref());
    println!(
        "  active profile : {}",
        profile_name.as_deref().unwrap_or("(none)")
    );

    let r = cfg.resolve(ctx.profile_override.as_deref())?;
    println!("  root           : {}", r.root.display());
    println!("  layout         : {}", r.layout);
    println!("  default vcs    : {:?}", r.default_vcs);
    println!("  default proto  : {:?}", r.default_protocol);
    println!("  default host   : {}", r.default_host);
    println!("  exec concur.   : {}", r.exec_concurrency);
    if !r.routes.is_empty() {
        println!("  routes:");
        for (idx, route) in r.routes.iter().enumerate() {
            let override_summary = [
                route.raw.root.as_deref().map(|v| format!("root={v}")),
                route.raw.layout.as_deref().map(|v| format!("layout={v}")),
                route.raw.default_vcs.map(|v| format!("vcs={v:?}")),
                route
                    .raw
                    .default_protocol
                    .map(|v| format!("protocol={v:?}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            // Skip the arrow when there's no overrides to show — a
            // bare `→ ` line is just noise. A future route may exist
            // purely for matching (e.g., to opt into a flag that's
            // not config-shaped); render it without the trailing
            // arrow in that case.
            if override_summary.is_empty() {
                println!("    [{idx}] {}", route.raw.pattern);
            } else {
                println!("    [{idx}] {} → {override_summary}", route.raw.pattern);
            }
        }
    }
    if r.profile_provided_root {
        println!(
            "  {} (profile pinned `root`; routes won't run for clones in this profile)",
            "note:".dimmed()
        );
    }
    if !r.git_config.is_empty() {
        println!("  git_config (profile):");
        for (k, v) in &r.git_config {
            println!("    {k} = {v:?}");
        }
    }

    // Shelf summary. Don't make doctor fail when the shelf is
    // unparseable — surface the error inline and keep going. Doctor
    // is a diagnostic, so a corrupt shelf is *information* (the
    // user wants to know), not a hard stop.
    println!();
    println!("{}", "shelf".underline());
    match Shelf::load(p) {
        Ok(s) if s.is_empty() => println!(
            "  {} (no repos yet — `shoka clone <url>` to add one)",
            "0 repos".dimmed()
        ),
        Ok(s) => println!("  {} repos on the shelf (schema v{})", s.len(), s.version),
        Err(e) => println!("  {} {e:#}", "shelf load failed:".red()),
    }

    // Cache summary. Same informational stance as the shelf section
    // — render what we can, surface load errors inline, don't bail.
    println!();
    println!("{}", "cache".underline());
    println!("  threshold        : {}s", r.cache.refresh_threshold_secs);
    println!("  background_refresh: {}", r.cache.background_refresh);
    println!("  parallel_repos   : {}", r.cache.parallel_repos);
    match Cache::load(p) {
        Ok(c) if c.is_empty() => println!(
            "  {} (run `shoka cache refresh` to populate)",
            "0 entries".dimmed()
        ),
        Ok(c) => {
            let (oldest, newest) = c
                .repos
                .iter()
                .filter_map(|r| r.last_refreshed)
                .fold((u64::MAX, 0u64), |(o, n), ts| (o.min(ts), n.max(ts)));
            print!("  {} entries (schema v{})", c.len(), c.version);
            if newest > 0 {
                let now = crate::cache::current_unix_secs();
                println!(
                    " — freshest {}s ago, oldest {}s ago",
                    now.saturating_sub(newest),
                    now.saturating_sub(oldest)
                );
            } else {
                println!(" (none refreshed yet)");
            }
        }
        Err(e) => println!("  {} {e:#}", "cache load failed:".red()),
    }

    Ok(())
}
