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

    /// Recent weekly commit counts, oldest → newest, for the TUI
    /// activity sparkline (at most [`ACTIVITY_WEEKS`] entries). `None`
    /// means "not captured" — non-github host, no token, an API
    /// error, or GitHub still computing the stat (the
    /// `/stats/commit_activity` endpoint returns `202` with an empty
    /// body on a cold cache). The TUI renders that as `-`, distinct
    /// from a genuine all-zero (dormant) history.
    ///
    /// Additive `Option` field: an older cache loads it as `None` and
    /// self-heals on the next `cache refresh`, so no `CACHE_VERSION`
    /// bump is needed — that rule is for new `CiStatus` variants an
    /// older reader couldn't parse, not for new optional fields it
    /// can default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_commits: Option<Vec<u32>>,
}

/// How many trailing weeks of commit activity [`capture_snapshot`]
/// keeps for the sparkline. Twelve weeks (~3 months) reads a repo's
/// recent rhythm without widening the dashboard column — and it's the
/// width the TUI renders.
pub const ACTIVITY_WEEKS: usize = 12;

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
///
/// `async` because the CLI-subprocess fallback runs through
/// [`tokio::process::Command`] — calling a blocking
/// [`std::process::Command`] here would stall the runtime worker
/// while `gh auth token` does its filesystem reads on
/// auth-status, which is rude inside the refresh's tokio context.
pub async fn resolve_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    gh_cli_token().await
}

/// Best-effort subprocess to `gh auth token`. Any failure mode (no
/// `gh` on PATH, not logged in, exit != 0, empty output) yields
/// `None` — the caller treats that the same as "no token".
async fn gh_cli_token() -> Option<String> {
    let gh = which::which("gh").ok()?;
    let mut cmd = tokio::process::Command::new(gh);
    cmd.args(["auth", "token"]);
    // Windows: suppress the new-console-window allocation. The
    // background cache refresh runs detached (no console), so any
    // child spawned from inside it would otherwise get a fresh
    // console allocated by Windows — surfacing as a black-window
    // flash on every `shoka <cmd>` tail. CREATE_NO_WINDOW is a
    // no-op when the parent already has a console (foreground
    // calls inherit it as usual).
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    let output = cmd.output().await.ok()?;
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

/// Capture the per-repo snapshot. Errors **propagate**: either
/// inner call failing (rate limit, network, 404) returns `Err` so
/// the caller's contract — "success = update entry, error = leave
/// the previous snapshot intact" — actually holds.
///
/// The earlier `.ok().flatten()` version silently buried transient
/// errors into a snapshot with `None` fields, which then overwrote
/// the cached snapshot and blanked the dashboard until the next
/// successful refresh. Per-field `None` is now reserved for "API
/// returned no data" (e.g. zero workflow runs) — distinct from
/// "API errored", which is now a hard fail.
pub async fn capture_snapshot(client: &Octocrab, owner: &str, name: &str) -> Result<GhSnapshot> {
    let open_pr_count = open_pr_count(client, owner, name).await?;
    let ci_status = latest_ci_status(client, owner, name).await?;
    // Best-effort, unlike the PR/CI calls above: the activity
    // sparkline is a softer signal, and `/stats/commit_activity` 202s
    // (empty body) while GitHub warms the stat cache. Swallowing the
    // error to `None` keeps a cold-cache 202 from blanking the whole
    // snapshot — the graph fills in on a later refresh once the stat
    // is ready. `.ok().flatten()` collapses both "errored" and "no
    // data yet" to `None`.
    let weekly_commits = recent_weekly_commits(client, owner, name)
        .await
        .ok()
        .flatten();
    Ok(GhSnapshot {
        open_pr_count: Some(open_pr_count),
        ci_status,
        weekly_commits,
    })
}

/// One element of `/stats/commit_activity` — a single week. Only
/// `total` is read (the per-day `days` array and `week` timestamp are
/// ignored), same minimal-struct discipline as [`RunMinimal`].
#[derive(Debug, Clone, serde::Deserialize)]
struct WeekActivity {
    total: u32,
}

/// Trailing [`ACTIVITY_WEEKS`] weeks of commit counts (oldest →
/// newest) from `/repos/{owner}/{name}/stats/commit_activity`.
///
/// GitHub computes this stat asynchronously: a request against a cold
/// cache returns `202 Accepted` with an empty body while it builds,
/// which octocrab surfaces as a deserialise error — the caller
/// swallows that to `None` and retries next refresh. A warm `200`
/// with `[]` (a repo with no commit history yet) likewise maps to
/// `None` so the TUI shows "no data" rather than a flat dormant bar
/// for a repo we simply haven't measured.
async fn recent_weekly_commits(
    client: &Octocrab,
    owner: &str,
    name: &str,
) -> Result<Option<Vec<u32>>> {
    let route = format!("/repos/{owner}/{name}/stats/commit_activity");
    let weeks: Vec<WeekActivity> = client.get(route, None::<&()>).await?;
    if weeks.is_empty() {
        return Ok(None);
    }
    // Keep the last ACTIVITY_WEEKS, preserving oldest → newest order.
    let start = weeks.len().saturating_sub(ACTIVITY_WEEKS);
    let tail = weeks[start..].iter().map(|w| w.total).collect();
    Ok(Some(tail))
}

async fn open_pr_count(client: &Octocrab, owner: &str, name: &str) -> Result<usize> {
    // Bypass octocrab's typed `pulls(...).list().send()` because
    // its [`octocrab::models::pulls::PullRequest`] schema treats
    // several optional fields as required `String` — when GitHub
    // returns `null` for any of them (which it does for several of
    // our own repos, including kanade / kaishin / todoke), the
    // whole snapshot fails with `Serde Error: invalid type: null,
    // expected a string` and the row in the TUI renders `-`. We
    // only need the *count*, so a minimal local struct (`items: Vec<
    // serde_json::Value>`) skips every per-PR field and is immune
    // to the schema drift entirely.
    //
    // `per_page(100)` is the API max; the response is a JSON
    // *array* (not the `Page<T>` envelope octocrab synthesises),
    // so we deserialise straight into a `Vec`. Anything over 100
    // open PRs is unusual enough that under-counting at the cap is
    // acceptable — the TUI clamps the display at "99+" regardless.
    let route = format!("/repos/{owner}/{name}/pulls");
    // `IgnoredAny` accepts any JSON value and discards it, so the
    // per-PR contents never touch a typed deserialiser. Counting
    // the Vec gives us the open-PR total without paying for a
    // single struct field.
    let prs: Vec<serde::de::IgnoredAny> = client
        .get(route, Some(&[("state", "open"), ("per_page", "100")]))
        .await?;
    Ok(prs.len())
}

/// Minimal subset of GitHub's workflow-run object — only the two
/// fields [`latest_ci_status`] needs to classify the result. Avoids
/// pulling in octocrab's [`octocrab::models::workflows::Run`],
/// whose strict-`String` typing on optional fields (notably
/// `previous_attempt_url`) makes the entire response refuse to
/// deserialise when GitHub returns `null` for them.
#[derive(Debug, Clone, serde::Deserialize)]
struct RunMinimal {
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}

/// Wire envelope the `/actions/runs` endpoint returns. We only
/// peel off `workflow_runs`; everything else (`total_count`,
/// pagination links) is ignored, so a future field addition or
/// rename upstream can't break this code path.
///
/// `Option<Vec<…>>` rather than `#[serde(default)] Vec<…>` because
/// the latter only catches a *missing* key — if GitHub ever
/// returns `{"workflow_runs": null}` (rare but documented in some
/// upstream issues), serde would still fail deserialising `null`
/// into a `Vec`. `Option` flattens both shapes into the same
/// "treat as no runs" branch via `unwrap_or_default()` at the
/// call site.
#[derive(Debug, Clone, serde::Deserialize)]
struct CiRunsResponse {
    #[serde(default)]
    workflow_runs: Option<Vec<RunMinimal>>,
}

async fn latest_ci_status(client: &Octocrab, owner: &str, name: &str) -> Result<Option<CiStatus>> {
    // Same minimal-struct rationale as `open_pr_count`. The
    // `/actions/runs` response is sorted newest first by GitHub
    // default, so `per_page=1` is enough to read the most recent
    // visible run. Branch filter is *not* applied: a hand-pushed
    // feature-branch CI run can legitimately precede a
    // default-branch run, and the dashboard's job is "what's the
    // latest visible CI state" not "is main green specifically".
    let route = format!("/repos/{owner}/{name}/actions/runs");
    let response: CiRunsResponse = client.get(route, Some(&[("per_page", "1")])).await?;
    let Some(run) = response
        .workflow_runs
        .unwrap_or_default()
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    Ok(Some(classify_ci(&run)))
}

fn classify_ci(run: &RunMinimal) -> CiStatus {
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

/// One Issue or PR as the TUI picker needs to render and act on it.
/// Same shape for both because the TUI treats them identically —
/// number + title + URL + labels. PRs only show up in the issue
/// listing on GitHub as a special-case (`pull_request` field set),
/// so `list_open_issues` is the only place that filters them out
/// to avoid double-counting in the issues picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    pub number: u64,
    pub title: String,
    pub html_url: String,
    pub labels: Vec<String>,
}

impl PickerItem {
    /// Single line used by the picker's fuzzy index. Combining
    /// number + title + labels means a query like "bug 42" matches
    /// either the title text or the label `bug`, which is what users
    /// expect from a Telescope-style picker.
    pub fn search_key(&self) -> String {
        if self.labels.is_empty() {
            format!("#{} {}", self.number, self.title)
        } else {
            format!(
                "#{} {} [{}]",
                self.number,
                self.title,
                self.labels.join(",")
            )
        }
    }
}

/// Minimal subset of a `/issues` or `/pulls` array element — just
/// the fields the Telescope-style picker actually displays. Same
/// rationale as [`RunMinimal`]: octocrab's typed `Issue` /
/// `PullRequest` schemas mark several optional fields as required
/// `String`, so the entire picker fails on repos where GitHub
/// returns `null` for any of them. The label sub-object likewise
/// uses [`LabelMinimal`] to skip the rest of GitHub's label
/// payload (colour, description, default flag, …).
#[derive(Debug, Clone, serde::Deserialize)]
struct PickerItemRaw {
    number: u64,
    title: String,
    html_url: String,
    #[serde(default)]
    labels: Vec<LabelMinimal>,
    /// Present on the `/issues` endpoint when the item is actually
    /// a pull request (GitHub folds PRs into `/issues`). The picker
    /// filters these out on the issues side. `IgnoredAny` skips the
    /// payload so a future schema change in `pull_request.*` can't
    /// break the filter.
    #[serde(default)]
    pull_request: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LabelMinimal {
    name: String,
}

impl PickerItemRaw {
    fn into_picker_item(self) -> PickerItem {
        PickerItem {
            number: self.number,
            title: self.title,
            html_url: self.html_url,
            labels: self.labels.into_iter().map(|l| l.name).collect(),
        }
    }
}

/// List open issues for `owner/name`. Filters out pull requests —
/// GitHub's REST API folds PRs into the `/issues` listing with a
/// `pull_request` field set, and showing them twice (once in `i`
/// and once in `p`) is the kind of thing nobody asks for but
/// everybody notices.
pub async fn list_open_issues(
    client: &Octocrab,
    owner: &str,
    name: &str,
) -> Result<Vec<PickerItem>> {
    let route = format!("/repos/{owner}/{name}/issues");
    let raw: Vec<PickerItemRaw> = client
        .get(route, Some(&[("state", "open"), ("per_page", "100")]))
        .await?;
    Ok(raw
        .into_iter()
        .filter(|i| i.pull_request.is_none())
        .map(PickerItemRaw::into_picker_item)
        .collect())
}

/// List open pull requests for `owner/name`. The `/pulls` endpoint
/// returns only PRs, so no cross-contamination filter is needed —
/// `pull_request` on the deserialised items is always `None` here.
pub async fn list_open_prs(client: &Octocrab, owner: &str, name: &str) -> Result<Vec<PickerItem>> {
    let route = format!("/repos/{owner}/{name}/pulls");
    let raw: Vec<PickerItemRaw> = client
        .get(route, Some(&[("state", "open"), ("per_page", "100")]))
        .await?;
    Ok(raw
        .into_iter()
        .map(PickerItemRaw::into_picker_item)
        .collect())
}

/// One repo from a `/user/repos` or `/users/{user}/repos` listing,
/// trimmed to the fields the clone picker actually shows. GitHub
/// returns ~40 fields per repo; we deserialise only the four we
/// render so a future schema change in the rest can't break us.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoListItem {
    /// `owner.login` flattened — the picker shows `owner/name` and
    /// the clone path uses the same pair, so keeping them split
    /// saves the caller from re-splitting on `/`.
    #[serde(rename = "owner", deserialize_with = "deserialize_owner_login")]
    pub owner: String,
    pub name: String,
    /// `None` when the repo has no description set. Displayed dim
    /// after a `—` separator in the picker; absent line stays clean.
    #[serde(default)]
    pub description: Option<String>,
    /// True for archived repos. Filtered out at the call site —
    /// archived repos rarely belong on a fresh shelf.
    #[serde(default)]
    pub archived: bool,
}

fn deserialize_owner_login<'de, D>(de: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    struct OwnerLogin {
        login: String,
    }
    OwnerLogin::deserialize(de).map(|o| o.login)
}

/// `inquire::Select` requires `Display` on its options — the line
/// rendered here is also what `inquire`'s default `string_matches`
/// filter scores: `owner/name` plus an em-dash-separated
/// description when present.
impl std::fmt::Display for RepoListItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.description {
            Some(d) if !d.is_empty() => write!(f, "{}/{}  —  {d}", self.owner, self.name),
            _ => write!(f, "{}/{}", self.owner, self.name),
        }
    }
}

/// Authenticated user's login. Used to seed the `shoka clone`
/// org-prompt default — pressing Enter without typing lands on
/// "list my own repos" instead of forcing the user to retype their
/// own handle.
pub async fn whoami(client: &Octocrab) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct User {
        login: String,
    }
    let user: User = client.get("/user", None::<&()>).await?;
    Ok(user.login)
}

/// True when `err` (or anything in its `source` chain) is an
/// octocrab `GitHubError` with HTTP 404. Lets the clone-flow
/// re-prompt on "no such org" instead of bubbling up as a fatal
/// error and forcing the user to retype from scratch.
pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<octocrab::GitHubError>())
        .any(|gh| gh.status_code.as_u16() == 404)
}

/// List repos for one of three audiences:
///
/// - `owner = None` ⇒ the authenticated user's own repos via
///   `/user/repos?affiliation=owner` (includes private).
/// - `owner = Some, is_org = true` ⇒ an organisation's repos via
///   `/orgs/{org}/repos`. When the caller is a member this endpoint
///   includes the org's private repos too; `/users/{org}/repos`
///   would silently show only the public subset.
/// - `owner = Some, is_org = false` ⇒ another user's repos via
///   `/users/{user}/repos` (public only — there's no way to read
///   another user's private repos through an OAuth token).
///
/// Filters out archived repos before returning; forks are kept
/// since users often clone their own forks for upstream-tracking
/// work. Sorted most-recently-updated first. Capped at 100 — the
/// few users with more repos than that can still pass `owner/name`
/// directly.
///
/// A 404 from a named-owner path (typo / private user / deleted
/// account) propagates as an octocrab error; callers can detect
/// it via [`is_not_found`] and re-prompt rather than aborting.
pub async fn list_repos(
    client: &Octocrab,
    owner: Option<&str>,
    is_org: bool,
) -> Result<Vec<RepoListItem>> {
    let (route, params): (String, &[(&str, &str)]) = match owner {
        None => (
            "/user/repos".to_string(),
            &[
                ("affiliation", "owner"),
                ("sort", "updated"),
                ("per_page", "100"),
            ],
        ),
        Some(o) if is_org => (
            format!("/orgs/{o}/repos"),
            &[("sort", "updated"), ("per_page", "100")],
        ),
        Some(o) => (
            format!("/users/{o}/repos"),
            &[("sort", "updated"), ("per_page", "100")],
        ),
    };
    let raw: Vec<RepoListItem> = client.get(route, Some(params)).await?;
    Ok(raw.into_iter().filter(|r| !r.archived).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_token_uses_env_when_set() {
        // Snapshot + restore env so the test doesn't leak.
        let prev = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token-xyz");
        }
        assert_eq!(resolve_token().await.as_deref(), Some("test-token-xyz"));
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
            weekly_commits: Some(vec![0, 1, 4, 2, 7]),
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
            weekly_commits: None,
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
        assert!(
            !s.contains("weekly_commits"),
            "None weekly_commits shouldn't serialise, got: {s}"
        );
    }

    #[test]
    fn repo_list_item_deserialises_owner_login_flat() {
        let json = r#"{
            "owner": { "login": "yukimemi", "id": 1 },
            "name": "shoka",
            "description": "jj-aware ghq successor",
            "archived": false,
            "extra_field_we_dont_care_about": 42
        }"#;
        let item: RepoListItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.owner, "yukimemi");
        assert_eq!(item.name, "shoka");
        assert_eq!(item.description.as_deref(), Some("jj-aware ghq successor"));
        assert!(!item.archived);
    }

    #[test]
    fn repo_list_item_display_includes_description() {
        let item = RepoListItem {
            owner: "yukimemi".into(),
            name: "shoka".into(),
            description: Some("书架".into()),
            archived: false,
        };
        assert_eq!(item.to_string(), "yukimemi/shoka  —  书架");
    }

    #[test]
    fn repo_list_item_display_falls_back_to_slug_when_no_description() {
        let item = RepoListItem {
            owner: "yukimemi".into(),
            name: "shoka".into(),
            description: None,
            archived: false,
        };
        assert_eq!(item.to_string(), "yukimemi/shoka");
        // Empty-string description is treated as absent rather than
        // a stray trailing em-dash.
        let empty_desc = RepoListItem {
            description: Some(String::new()),
            ..item
        };
        assert_eq!(empty_desc.to_string(), "yukimemi/shoka");
    }

    #[test]
    fn ci_status_serialises_snake_case() {
        let s = toml::to_string(&GhSnapshot {
            open_pr_count: None,
            ci_status: Some(CiStatus::Success),
            weekly_commits: None,
        })
        .unwrap();
        assert!(s.contains("\"success\""), "wire format = lowercase: {s}");
    }
}
