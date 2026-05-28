//! TUI action plumbing — `fetch` / `push` helpers shared by the
//! dashboard's `f` / `P` keybinds.
//!
//! Each row in the dashboard maps to a directory that may be a plain
//! git repo, a jj-colocated repo (`.jj/` + `.git/`), or a jj-only
//! repo. The action helpers probe the filesystem to pick the right
//! CLI — `jj git fetch` / `jj git push` for jj, `git fetch` /
//! `git push` for plain git — and shell out via tokio so callers
//! await without blocking the runtime.
//!
//! Output is captured as piped strings: the TUI renders them in a
//! popup so the user sees what happened without dropping into the
//! parent shell. stdin is closed so a credential prompt fails fast
//! instead of hanging.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::process::Command;

/// Upper bound on a single fetch / push. Long enough for honest
/// repos over slow links, short enough that a stalled remote /
/// handshake / hanging credential helper can't freeze the TUI
/// indefinitely. On expiry the child is killed (via `kill_on_drop`)
/// and the popup renders a `timed out` line.
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsKind {
    Jj,
    Git,
}

impl VcsKind {
    pub fn label(self) -> &'static str {
        match self {
            VcsKind::Jj => "jj",
            VcsKind::Git => "git",
        }
    }
}

/// Probe the filesystem for a VCS marker. Prefers `.jj/` over
/// `.git/` because colocated repos have both, and `jj git fetch`
/// also updates the git refs for any reader who happens to use git
/// directly — so picking jj is strictly more informative.
pub fn detect_vcs(path: &Path) -> Option<VcsKind> {
    if path.join(".jj").is_dir() {
        Some(VcsKind::Jj)
    } else if path.join(".git").exists() {
        // Allow `.git` to be a file (worktree) as well as a dir,
        // hence `exists()` rather than `is_dir()`.
        Some(VcsKind::Git)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Fetch,
    Push,
}

impl ActionKind {
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Fetch => "fetch",
            ActionKind::Push => "push",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub vcs: VcsKind,
    pub command: String,
}

/// Resolve `(program, args)` for a given `(vcs, kind)` pair.
/// Extracted so the test suite can assert the exact command line
/// without spawning a subprocess.
pub fn command_for(vcs: VcsKind, kind: ActionKind) -> (&'static str, &'static [&'static str]) {
    match (vcs, kind) {
        (VcsKind::Jj, ActionKind::Fetch) => ("jj", &["git", "fetch"]),
        (VcsKind::Jj, ActionKind::Push) => ("jj", &["git", "push"]),
        (VcsKind::Git, ActionKind::Fetch) => ("git", &["fetch"]),
        (VcsKind::Git, ActionKind::Push) => ("git", &["push"]),
    }
}

/// Run a fetch or push at `path`. Returns `Ok(outcome)` even when
/// the subprocess exits non-zero — non-zero is "git/jj said no",
/// which is interesting output to surface to the user, not an
/// error in our orchestration. `Err` is reserved for "couldn't even
/// spawn the binary" or "no VCS detected".
pub async fn run_action(path: &Path, kind: ActionKind) -> Result<ActionOutcome> {
    let vcs =
        detect_vcs(path).ok_or_else(|| anyhow!("no .jj or .git found in {}", path.display()))?;
    let (program, args) = command_for(vcs, kind);
    let display_command = format!("{program} {}", args.join(" "));

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(path)
        // Closing stdin makes credential prompts fail fast instead
        // of hanging the TUI thread forever. Real auth should be
        // configured via SSH keys / credential helpers / gh CLI.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `kill_on_drop` is the kill switch for the [`ACTION_TIMEOUT`]
        // path: when the `wait_with_output` future is dropped on
        // timeout, tokio drops the `Child`, which sends the kill
        // signal. Without this the child would linger as an orphan
        // (still holding the network handle, still writing to a now-
        // dead pipe) after we've already moved on.
        .kill_on_drop(true);
    // See `crate::silent_creation_flags` for the rationale. The TUI
    // owns a console while it's running, so the foreground f/P
    // keypress doesn't usually flash — but applying the flag
    // uniformly keeps the spawn pattern consistent with the rest
    // of the codebase.
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn `{display_command}`: {e}"))?;

    match tokio::time::timeout(ACTION_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(ActionOutcome {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            vcs,
            command: display_command,
        }),
        Ok(Err(e)) => Err(anyhow!(
            "failed to capture output of `{display_command}`: {e}"
        )),
        // Timeout: surface as a failed outcome (not an `Err`) so the
        // popup renders red instead of bubbling up into the TUI's
        // generic error path. The user sees what happened *and* the
        // dashboard stays interactive.
        Err(_elapsed) => Ok(ActionOutcome {
            success: false,
            stdout: String::new(),
            stderr: format!(
                "timed out after {}s — child killed",
                ACTION_TIMEOUT.as_secs()
            ),
            vcs,
            command: display_command,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detect_vcs_prefers_jj_when_colocated() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".jj")).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(detect_vcs(dir.path()), Some(VcsKind::Jj));
    }

    #[test]
    fn detect_vcs_picks_git_when_only_git() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(detect_vcs(dir.path()), Some(VcsKind::Git));
    }

    #[test]
    fn detect_vcs_handles_git_worktree_file() {
        // Secondary worktrees have `.git` as a *file* pointing back
        // to the real gitdir, not a directory. `is_dir()` would miss
        // them; `exists()` is intentional.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".git"), "gitdir: /tmp/foo").unwrap();
        assert_eq!(detect_vcs(dir.path()), Some(VcsKind::Git));
    }

    #[test]
    fn detect_vcs_none_when_neither_marker_present() {
        let dir = tempdir().unwrap();
        assert_eq!(detect_vcs(dir.path()), None);
    }

    #[test]
    fn command_for_jj_uses_git_subcommand() {
        assert_eq!(
            command_for(VcsKind::Jj, ActionKind::Fetch),
            ("jj", &["git", "fetch"][..])
        );
        assert_eq!(
            command_for(VcsKind::Jj, ActionKind::Push),
            ("jj", &["git", "push"][..])
        );
    }

    #[test]
    fn command_for_git_uses_plain_subcommand() {
        assert_eq!(
            command_for(VcsKind::Git, ActionKind::Fetch),
            ("git", &["fetch"][..])
        );
        assert_eq!(
            command_for(VcsKind::Git, ActionKind::Push),
            ("git", &["push"][..])
        );
    }

    #[test]
    fn action_kind_label_is_human_readable() {
        assert_eq!(ActionKind::Fetch.label(), "fetch");
        assert_eq!(ActionKind::Push.label(), "push");
    }
}
