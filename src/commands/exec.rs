//! `shoka exec` — run a user-supplied command across the shelf.
//!
//! This is the deliberately-transparent escape hatch: pass any
//! command after `--` and shoka will run it once per matching repo,
//! cwd-set to that repo's clone path. Filtering uses the same
//! `--tag` AND-semantics as `shoka list` / `cd`.
//!
//! ```text
//! shoka exec --tag rust -- cargo check
//! shoka exec -- git fetch --prune
//! shoka exec -- jj git fetch
//! ```
//!
//! `--filter <status>` (dirty / behind / …) is reserved for the Phase
//! 2 cache integration — until the cache carries a status snapshot,
//! we'd have to run `git status` per repo just to decide whether to
//! run the *real* command, which makes the flag useless in
//! practice. The flag still parses for forward compatibility but
//! currently errors out with a pointer to the cache work.
//!
//! Output policy: each repo's stdout + stderr are captured into a
//! single buffer and printed as a banner-headed block when the
//! process exits. Phase 1 batches like this so parallel output from
//! many repos doesn't interleave into nonsense; a future
//! `--stream`-style flag could opt into inherited stdio for
//! real-time long-running commands.
//!
//! Exit code: shoka itself exits non-zero (1) when any repo's
//! command failed. Individual exit codes are reported per-repo so
//! the user can tell which subset broke.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;
use teravars::Engine;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::cli::ExecArgs;
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: ExecArgs) -> Result<()> {
    if args.cmd.is_empty() {
        bail!("no command — pass it after `--`, e.g. `shoka exec --tag rust -- cargo check`");
    }
    if args.filter.is_some() {
        bail!(
            "`--filter` requires the cache status snapshot which lands in Phase 2; \
             for now use `--tag` to narrow the run, or filter externally with `shoka list`"
        );
    }

    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        bail!(
            "shelf is empty — nothing to run against. `shoka clone <url>` or \
             `shoka import <dir>` first"
        );
    }

    // Build the matched-repo set once, plus their resolved paths, so
    // the spawn loop doesn't re-touch the Tera engine or re-render
    // layouts per repo (cheap, but avoidable).
    let targets = resolve_targets(&shelf, &resolved, &args)?;
    if targets.is_empty() {
        bail!(
            "no repos matched the tag filter ({} on the shelf total)",
            shelf.len()
        );
    }

    let semaphore = Arc::new(Semaphore::new(resolved.exec_concurrency));
    let prog = Arc::new(args.cmd[0].clone());
    let argv: Arc<Vec<String>> = Arc::new(args.cmd[1..].to_vec());

    let total = targets.len();
    println!(
        "{} running `{}` across {} repo{} (concurrency {})",
        "exec:".bold(),
        format_cmdline(&args.cmd).cyan(),
        total,
        if total == 1 { "" } else { "s" },
        resolved.exec_concurrency
    );

    let mut joinset: JoinSet<Outcome> = JoinSet::new();
    for target in targets {
        let sem = semaphore.clone();
        let prog = prog.clone();
        let argv = argv.clone();
        joinset.spawn(async move {
            // permit is held for the lifetime of the spawn — drop on
            // return propagates the concurrency slot back to the
            // semaphore for the next waiter.
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    return Outcome::with_setup_error(target, "concurrency semaphore was closed");
                }
            };
            run_one(target, &prog, &argv).await
        });
    }

    let mut failures = 0usize;
    while let Some(joined) = joinset.join_next().await {
        let outcome = match joined {
            Ok(o) => o,
            Err(e) => {
                // JoinError covers panic / cancellation — treat the
                // whole repo as failed, but keep the loop going so
                // the user sees the rest of the shelf finish.
                eprintln!("{} exec task panicked: {e}", "!".red());
                failures += 1;
                continue;
            }
        };
        if !outcome.is_success() {
            failures += 1;
        }
        print_outcome(&outcome);
    }

    println!();
    if failures == 0 {
        println!("{} all {total} repo(s) succeeded", "exec:".bold().green());
        Ok(())
    } else {
        // Non-zero exit so scripts wrapping `shoka exec` notice. The
        // per-repo blocks above already showed which ones failed.
        bail!("{failures} of {total} repo(s) failed — see the per-repo blocks above")
    }
}

/// Resolve the matched-repo set into a vector of [`Target`]s, each
/// carrying the repo metadata + pre-rendered clone path. Borrows
/// from `shelf`'s [`Repo`]s by cloning the small bits we need; the
/// spawn loop has to `move` into per-repo tasks, so owned data is
/// required anyway.
fn resolve_targets(
    shelf: &Shelf,
    resolved: &ResolvedConfig,
    args: &ExecArgs,
) -> Result<Vec<Target>> {
    let mut engine = Engine::new();
    let mut out = Vec::new();
    for repo in &shelf.repos {
        if !args.tags.is_empty() && !has_all_tags(repo, &args.tags) {
            continue;
        }
        let path = resolved
            .clone_path_for(repo, &mut engine)
            .with_context(|| format!("resolving clone path for {}", repo.slug()))?;
        out.push(Target {
            slug: repo.slug(),
            path,
        });
    }
    Ok(out)
}

fn has_all_tags(repo: &Repo, wanted: &[String]) -> bool {
    wanted.iter().all(|w| repo.tags.iter().any(|t| t == w))
}

/// Run the command in one repo. Captures combined stdout + stderr —
/// we don't separate them because the user typically wants to see
/// the failure context in order, and a separate stream pair would
/// have to be re-interleaved by line for that to make sense.
async fn run_one(target: Target, prog: &str, argv: &[String]) -> Outcome {
    if !target.path.is_dir() {
        return Outcome::with_setup_error(
            target,
            "clone path does not exist (stale shelf entry — try `shoka prune`)",
        );
    }
    let mut cmd = Command::new(prog);
    cmd.args(argv)
        .current_dir(&target.path)
        // Inherit stdin so commands that prompt (rare in exec, but
        // possible) at least *can* read; we'd need a TTY allocation
        // to actually be useful, but inheriting beats /dev/null.
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return Outcome::with_setup_error(target, &format!("spawn failed: {e}"));
        }
    };

    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    Outcome {
        target,
        exit_code: output.status.code(),
        combined,
        setup_error: None,
    }
}

/// One repo's result.
struct Outcome {
    target: Target,
    /// `None` when the process was terminated by a signal (Unix) — we
    /// still call that a failure, but the exit-code line shows
    /// "signal" instead of a number.
    exit_code: Option<i32>,
    combined: Vec<u8>,
    /// Set when shoka itself couldn't even spawn the command (path
    /// missing, OS-level spawn error, …). When `Some`, the
    /// `combined` buffer is empty and `exit_code` is `None`.
    setup_error: Option<String>,
}

impl Outcome {
    fn with_setup_error(target: Target, msg: &str) -> Self {
        Self {
            target,
            exit_code: None,
            combined: Vec::new(),
            setup_error: Some(msg.to_string()),
        }
    }
    fn is_success(&self) -> bool {
        self.setup_error.is_none() && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone)]
struct Target {
    slug: String,
    path: PathBuf,
}

fn print_outcome(o: &Outcome) {
    println!();
    let status = match (&o.setup_error, o.exit_code) {
        (Some(_), _) => "ERR".red().to_string(),
        (None, Some(0)) => "ok".green().to_string(),
        (None, Some(code)) => format!("exit {code}").red().to_string(),
        // Unix signal termination.
        (None, None) => "signal".red().to_string(),
    };
    // Banner format optimised for grep-ability: the slug is the
    // anchor the user is most likely to search for.
    println!("{} {} [{}]", "───".dimmed(), o.target.slug.bold(), status);
    if let Some(err) = &o.setup_error {
        eprintln!("  {} {}", "!".red(), err);
        return;
    }
    if !o.combined.is_empty() {
        // Lossy is safe here — terminal output is already best-effort,
        // and a non-UTF-8 byte in `git diff` shouldn't crash the run.
        let s = String::from_utf8_lossy(&o.combined);
        // Strip trailing newline so we don't end with a blank line —
        // the banner of the next outcome (or the summary) does its
        // own separation.
        let trimmed = s.trim_end_matches('\n');
        if !trimmed.is_empty() {
            println!("{trimmed}");
        }
    }
}

/// Format the cmd vector for the "running `…`" header. Just joins
/// with spaces — shell-quoting would lie about how `Command::new`
/// actually invokes the args (no shell, no interpolation), so a
/// minimal echo is more honest than a fake `bash -c` quote.
fn format_cmdline(cmd: &[String]) -> String {
    cmd.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalConfig;
    use std::collections::BTreeMap;

    fn make_resolved(root: &str) -> ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some(root.into()),
                layout: "{{ root }}/{{ name }}".into(),
                exec_concurrency: 4,
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn resolve_targets_filters_by_tag() {
        let resolved = make_resolved("/data");
        let mut shelf = Shelf::default();
        let mut rust = Repo::new("github.com", "u", "rust-tool");
        rust.tags = vec!["rust".into()];
        let mut go = Repo::new("github.com", "u", "go-tool");
        go.tags = vec!["go".into()];
        shelf.add(rust).unwrap();
        shelf.add(go).unwrap();

        let targets = resolve_targets(
            &shelf,
            &resolved,
            &ExecArgs {
                tags: vec!["rust".into()],
                filter: None,
                cmd: vec!["true".into()],
            },
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].slug, "github.com/u/rust-tool");
    }

    #[test]
    fn resolve_targets_no_tag_returns_everything() {
        let resolved = make_resolved("/data");
        let mut shelf = Shelf::default();
        shelf.add(Repo::new("github.com", "u", "a")).unwrap();
        shelf.add(Repo::new("github.com", "u", "b")).unwrap();
        let targets = resolve_targets(
            &shelf,
            &resolved,
            &ExecArgs {
                tags: vec![],
                filter: None,
                cmd: vec!["true".into()],
            },
        )
        .unwrap();
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn outcome_success_only_when_exit_zero_and_no_setup_error() {
        let t = Target {
            slug: "x".into(),
            path: PathBuf::from("."),
        };
        let mut ok = Outcome {
            target: t.clone(),
            exit_code: Some(0),
            combined: Vec::new(),
            setup_error: None,
        };
        assert!(ok.is_success());
        ok.exit_code = Some(1);
        assert!(!ok.is_success());
        ok.exit_code = Some(0);
        ok.setup_error = Some("nope".into());
        assert!(!ok.is_success());
        ok.setup_error = None;
        ok.exit_code = None;
        assert!(!ok.is_success());
    }
}
