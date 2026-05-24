//! `shoka doctor` — diagnose paths + config resolution.
//!
//! Phase 1's first real command. Prints the resolved [`ShokaPaths`],
//! attempts to load [`ShokaConfig`] from the layered config tree, and
//! reports the effective [`ResolvedConfig`] under the active profile.
//! Useful both as a smoke test for new installs and as the rendering
//! target while paths / config code is in flux.
//!
//! [`ShokaConfig`]: crate::config::ShokaConfig
//! [`ResolvedConfig`]: crate::config::ResolvedConfig

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::commands::ShokaContext;
use crate::config::ShokaConfig;

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

    let config_exists = p.config_file().exists();
    println!(
        "{}  ({})",
        "config".underline(),
        if config_exists {
            "found".green().to_string()
        } else {
            "no config file yet — defaults in use".dimmed().to_string()
        }
    );

    let cfg = match ShokaConfig::load(p) {
        Ok(c) => c,
        Err(e) => {
            println!("  {} {e:#}", "load failed:".red());
            return Ok(());
        }
    };

    let profile_name = cfg.resolve_profile_name(ctx.profile_override.as_deref());
    println!(
        "  active profile : {}",
        profile_name.as_deref().unwrap_or("(none)")
    );

    match cfg.resolve(ctx.profile_override.as_deref()) {
        Ok(r) => {
            println!("  root           : {}", r.root.display());
            println!("  layout         : {}", r.layout);
            println!("  default vcs    : {:?}", r.default_vcs);
            println!("  default proto  : {:?}", r.default_protocol);
            println!("  default host   : {}", r.default_host);
            println!("  exec concur.   : {}", r.exec_concurrency);
            if !r.hosts.is_empty() {
                println!("  hosts:");
                for (host, h) in &r.hosts {
                    println!("    {host}: {h:?}");
                }
            }
            if !r.git_config.is_empty() {
                println!("  git_config (profile):");
                for (k, v) in &r.git_config {
                    println!("    {k} = {v:?}");
                }
            }
        }
        Err(e) => {
            println!("  {} {e:#}", "resolve failed:".red());
        }
    }

    Ok(())
}
