//! GitHub integration: token resolution + per-repo snapshot.
//!
//! ## Token resolution
//!
//! Priority order:
//!
//! 1. `$GITHUB_TOKEN` env var.
//! 2. `gh auth token` subprocess — same auth the `gh` CLI uses.
//!    Hybrid by design: the `gh` CLI is *not* a runtime dependency
//!    (snapshot capture is fine without it), but if the user happens
//!    to have it on PATH and logged in, we transparently piggyback.
//! 3. `None` — gh-dependent fields stay empty; non-gh features keep
//!    working unaffected.
//!
//! ## Snapshot
//!
//! What [`capture_snapshot`] populates, per repo:
//!
//! - `open_pr_count` — open pull requests against the default branch.
//! - `ci_status` — most recent workflow run conclusion on the
//!   default branch (Success / Failure / Pending / Skipped / …).
//!   `None` when there are no runs or the Actions API returned no
//!   data.
//!
//! API calls go through [`octocrab::Octocrab`], which is async and
//! tokio-native — refresh fans out across repos via [`JoinSet`].
//! Rate limits matter: 5 000 req/h authenticated vs. 60 unauth, and
//! each repo costs 2 calls (PRs + runs).
//!
//! [`JoinSet`]: tokio::task::JoinSet

use anyhow::Result;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

/// Cached gh snapshot for one repo. `None` fields mean "not
/// captured" / "no data" — the TUI renders those distinctly from
/// a definite zero (no open PRs, no runs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhSnapshot {
    /// Open pull-request count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_pr_count: Option<usize>,

    /// Conclusion of the most recent workflow run. Wire-stable
    /// variants — bumping `CACHE_VERSION` is required when adding
    /// new ones so a downgraded shoka refuses the file rather than
    /// silently dropping an unknown value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_status: Option<CiStatus>,
}

/// Workflow-run conclusion glyph the TUI cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    Skipped,
    /// Anything else (cancelled, timed_out, action_required, …) —
    /// folded into a single bucket because the dashboard glyph would
    /// be the same warning state for all of them.
    Other,
}

/// Resolve a GitHub token via the documented priority chain. Returns
/// `None` when no token is reachable; callers should treat that as
/// "skip gh fields, populate everything else".
pub fn resolve_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    gh_cli_token()
}

/// Best-effort subprocess to `gh auth token`. Any failure mode (no
/// `gh` on PATH, not logged in, exit != 0, empty output) yields
/// `None` — the caller treats that the same as "no token".
fn gh_cli_token() -> Option<String> {
    let gh = which::which("gh").ok()?;
    let output = std::process::Command::new(gh)
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Build an authenticated [`Octocrab`] from a token. Caller owns the
/// client and threads it through the per-repo capture calls so the
/// HTTP connection pool is reused.
pub fn build_client(token: &str) -> Result<Octocrab> {
    Ok(Octocrab::builder()
        .personal_token(token.to_string())
        .build()?)
}

/// Capture the per-repo snapshot. Both inner calls swallow their
/// errors into a `None` field — partial data beats blanking the
/// snapshot, and the per-call timeouts in octocrab keep a flaky
/// rate-limit / 404 from hanging the refresh.
pub async fn capture_snapshot(client: &Octocrab, owner: &str, name: &str) -> Result<GhSnapshot> {
    let open_pr_count = open_pr_count(client, owner, name).await.ok();
    let ci_status = latest_ci_status(client, owner, name).await.ok().flatten();
    Ok(GhSnapshot {
        open_pr_count,
        ci_status,
    })
}

async fn open_pr_count(client: &Octocrab, owner: &str, name: &str) -> Result<usize> {
    // `per_page(100)` is the API max; octocrab doesn't surface
    // `total_count` for /pulls (Search API would, but at 1/30 of the
    // rate-limit budget). Anything over 100 open PRs is unusual
    // enough that under-counting at the cap is acceptable — the TUI
    // clamps the display at "99+" regardless.
    let prs = client
        .pulls(owner, name)
        .list()
        .state(octocrab::params::State::Open)
        .per_page(100)
        .send()
        .await?;
    Ok(prs.items.len())
}

async fn latest_ci_status(client: &Octocrab, owner: &str, name: &str) -> Result<Option<CiStatus>> {
    // List runs sorted newest first (octocrab default). `per_page(1)`
    // is enough — we only care about the most recent visible run.
    // Branch filter is *not* applied: a hand-pushed feature-branch
    // CI run can legitimately precede a default-branch run, and the
    // dashboard's job is "what's the latest visible CI state" not
    // "is main green specifically".
    let runs = client
        .workflows(owner, name)
        .list_all_runs()
        .per_page(1)
        .send()
        .await?;
    let Some(run) = runs.items.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(classify_ci(&run)))
}

fn classify_ci(run: &octocrab::models::workflows::Run) -> CiStatus {
    // `status` is queued / in_progress / completed. `conclusion` is
    // populated only when status == completed.
    match run.status.as_str() {
        "queued" | "in_progress" | "waiting" | "requested" | "pending" => CiStatus::Pending,
        // status == "completed" → look at the conclusion.
        _ => match run.conclusion.as_deref().unwrap_or("") {
            "success" => CiStatus::Success,
            "failure" => CiStatus::Failure,
            "skipped" | "neutral" => CiStatus::Skipped,
            _ => CiStatus::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_token_uses_env_when_set() {
        // Snapshot + restore env so the test doesn't leak.
        let prev = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token-xyz");
        }
        assert_eq!(resolve_token().as_deref(), Some("test-token-xyz"));
        match prev {
            Some(p) => unsafe { std::env::set_var("GITHUB_TOKEN", p) },
            None => unsafe { std::env::remove_var("GITHUB_TOKEN") },
        }
    }

    #[test]
    fn snapshot_round_trips_through_toml() {
        let snap = GhSnapshot {
            open_pr_count: Some(3),
            ci_status: Some(CiStatus::Failure),
        };
        let s = toml::to_string(&snap).unwrap();
        let parsed: GhSnapshot = toml::from_str(&s).unwrap();
        assert_eq!(parsed, snap);
    }

    #[test]
    fn snapshot_skips_none_fields_on_serialize() {
        let empty = GhSnapshot {
            open_pr_count: None,
            ci_status: None,
        };
        let s = toml::to_string(&empty).unwrap();
        let parsed: GhSnapshot = toml::from_str(&s).unwrap();
        assert_eq!(parsed, empty);
        assert!(
            !s.contains("open_pr_count"),
            "None open_pr_count shouldn't serialise, got: {s}"
        );
        assert!(
            !s.contains("ci_status"),
            "None ci_status shouldn't serialise, got: {s}"
        );
    }

    #[test]
    fn ci_status_serialises_snake_case() {
        let s = toml::to_string(&GhSnapshot {
            open_pr_count: None,
            ci_status: Some(CiStatus::Success),
        })
        .unwrap();
        assert!(s.contains("\"success\""), "wire format = lowercase: {s}");
    }
}
