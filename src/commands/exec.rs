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
//! single buffer (line-interleaved via concurrent reads so the
//! arrival order survives — see [`interleave_pipes`]) and printed
//! as a banner-headed block when the process exits. Phase 1
//! batches like this so parallel output from many repos doesn't
//! interleave into nonsense; a future `--stream`-style flag could
//! opt into inherited stdio for real-time long-running commands.
//!
//! Child stdin is nulled. Running N processes in parallel that all
//! want to read the same TTY would race and corrupt the terminal —
//! the predictable failure (clean EOF) beats a frozen shell
//! waiting for input the user can't see anyone is asking for.
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
use tokio::io::{AsyncBufReadExt, BufReader};
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

/// Run the command in one repo. Captures stdout + stderr into a
/// single buffer, preserving arrival order at line granularity by
/// reading both pipes concurrently via `tokio::select!` — a naive
/// `stdout ++ stderr` concat (what `Command::output` returns)
/// would group all stdout then all stderr, breaking the temporal
/// interleave the user actually wants to read.
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
        // Null stdin so multiple parallel processes don't race over a
        // shared TTY — a command that wanted input fails fast (clean
        // EOF) instead of hanging while the user's shell looks
        // mysteriously frozen.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::with_setup_error(target, &format!("spawn failed: {e}")),
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let combined = match interleave_pipes(stdout, stderr).await {
        Ok(buf) => buf,
        Err(e) => {
            // Don't abandon the child just because we couldn't read its
            // pipes — wait it out so we don't leave a zombie. Surface
            // the IO error as the setup-error path.
            let _ = child.wait().await;
            return Outcome::with_setup_error(target, &format!("reading pipes: {e}"));
        }
    };

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => return Outcome::with_setup_error(target, &format!("waiting on child: {e}")),
    };

    Outcome {
        target,
        exit_code: status.code(),
        combined,
        setup_error: None,
    }
}

/// Read `stdout` and `stderr` line-by-line concurrently, appending
/// to a single `Vec<u8>` in arrival order. Within one line the
/// streams are still distinguishable by what the program wrote, but
/// across lines they're interleaved in real time — close enough to
/// OS-level pipe merging without dipping into platform-specific
/// `dup2` / `SetStdHandle`.
async fn interleave_pipes<R1, R2>(stdout: R1, stderr: R2) -> std::io::Result<Vec<u8>>
where
    R1: tokio::io::AsyncRead + Unpin,
    R2: tokio::io::AsyncRead + Unpin,
{
    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut combined = Vec::new();

    loop {
        if out_done && err_done {
            return Ok(combined);
        }
        tokio::select! {
            // `biased` keeps stdout slightly higher priority than
            // stderr — neither is strictly correct for "OS arrival
            // order" (which we can only approximate at line
            // granularity anyway), so a deterministic tiebreaker is
            // easier to reason about than a random one.
            biased;
            res = out.read_until(b'\n', &mut out_buf), if !out_done => match res? {
                0 => out_done = true,
                _ => {
                    combined.extend_from_slice(&out_buf);
                    out_buf.clear();
                }
            },
            res = err.read_until(b'\n', &mut err_buf), if !err_done => match res? {
                0 => err_done = true,
                _ => {
                    combined.extend_from_slice(&err_buf);
                    err_buf.clear();
                }
            },
        }
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
        // stdout (not stderr) so `shoka exec ... > log.txt` captures
        // the whole per-repo report cohesively — banner + body +
        // setup errors stay in one stream.
        println!("  {} {}", "!".red(), err);
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

    #[tokio::test]
    async fn interleave_pipes_preserves_per_stream_ordering() {
        // Each pipe is itself a Vec<u8> via `io::Cursor`. We can't
        // simulate real arrival-order timing inside a unit test (no
        // actual process), but we *can* verify the function: doesn't
        // drop bytes, doesn't double-include, terminates cleanly when
        // both streams hit EOF, and preserves the per-stream line
        // order (within a single stream, the original sequence is
        // intact). Cross-stream interleave isn't asserted here — that
        // depends on tokio's runtime scheduling decisions.
        use std::io::Cursor;
        let out_data: Vec<u8> = b"out-1\nout-2\n".to_vec();
        let err_data: Vec<u8> = b"err-1\nerr-2\n".to_vec();
        let combined = interleave_pipes(Cursor::new(out_data), Cursor::new(err_data))
            .await
            .unwrap();
        let s = String::from_utf8(combined).unwrap();
        // Both halves of each stream survive, in their original order.
        let i_out1 = s.find("out-1").unwrap();
        let i_out2 = s.find("out-2").unwrap();
        let i_err1 = s.find("err-1").unwrap();
        let i_err2 = s.find("err-2").unwrap();
        assert!(i_out1 < i_out2, "stdout order broken: {s}");
        assert!(i_err1 < i_err2, "stderr order broken: {s}");
        // No bytes lost: total length equals the two input buffers.
        assert_eq!(s.len(), "out-1\nout-2\nerr-1\nerr-2\n".len());
    }

    #[tokio::test]
    async fn interleave_pipes_handles_partial_final_line() {
        // A common case: a process writes its last line without a
        // trailing newline. `read_until` returns the leftover bytes
        // on EOF, so they should still land in the combined buffer.
        use std::io::Cursor;
        let combined = interleave_pipes(Cursor::new(b"no-nl".to_vec()), Cursor::new(b"".to_vec()))
            .await
            .unwrap();
        assert_eq!(combined, b"no-nl");
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
