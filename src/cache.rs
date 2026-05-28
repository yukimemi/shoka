//! Volatile per-repo cache.
//!
//! Lives at `$XDG_DATA_HOME/shoka/cache.toml` (or the platform
//! equivalent — see [`crate::paths::ShokaPaths`]) and holds
//! information that's expensive to re-derive on every command but
//! reproducible from the working tree / remote state. As of phase 1
//! that's just `last_refreshed` per repo; phase 2 will add
//! [`git_status`] and [`gh`] snapshot fields populated by background
//! refresh.
//!
//! Deliberately excluded from `shoka export` — by definition the
//! cache is reproducible, so it would just bloat the portable
//! shelf ledger ([`crate::state`]).
//!
//! Storage layout mirrors [`crate::state::Shelf`]: a `version` field
//! up top, then `[[repos]]` records keyed by the
//! `(host, owner, name, path?)` four-tuple (CACHE_VERSION 4+) — the
//! same shape the shelf uses to keep multi-clone checkouts of one
//! remote distinct from each other. Save is atomic via temp-file +
//! rename so a crashed refresh can't corrupt the cache.
//!
//! [`git_status`]: RepoCache::git_status
//! [`gh`]: RepoCache::gh

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::gh::GhSnapshot;
use crate::git_status::GitStatusSnapshot;
use crate::paths::ShokaPaths;

/// Current on-disk schema version. Bumped to **4** when the per-repo
/// `path` field landed alongside the shelf's path-aware identity —
/// multi-clone support means two checkouts of the same remote at
/// different on-disk paths must hold independent `git_status`
/// snapshots. The `gh` field stays shared by triple (remote-derived,
/// same value for every checkout). Readers built against an older
/// version see the new optional `path` as default-`None` and
/// self-heal on the next `cache refresh` (see [`Cache::upsert`]).
pub const CACHE_VERSION: u32 = 4;

/// Cache file contents. Mirrors [`crate::state::Shelf`]'s shape
/// intentionally — both files are walked by the same patterns
/// (`find`, `find_mut`, `upsert`, atomic save) so subcommands can
/// reach for the same mental model regardless of which file they're
/// touching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub repos: Vec<RepoCache>,
}

fn default_version() -> u32 {
    CACHE_VERSION
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            repos: Vec::new(),
        }
    }
}

/// Cache entry for one repo. Identity is
/// `(host, owner, name, path?)` — the same four-tuple
/// [`crate::state::Repo`] uses since v0.12.0. Two checkouts of the
/// same remote at different paths are distinct rows and hold
/// independent `git_status` snapshots; the `gh` field is
/// remote-derived and shared across siblings via
/// [`Cache::find_gh_by_triple`], which walks the row list and
/// returns the first populated snapshot so a half-refreshed cache
/// still surfaces the upstream value.
///
/// Optional fields are `skip_serializing_if = "Option::is_none"` so
/// a freshly-created entry serialises to its minimal shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCache {
    pub host: String,
    pub owner: String,
    pub name: String,

    /// On-disk path of the checkout this entry caches. `None` only
    /// during the transition window: cache files from CACHE_VERSION
    /// 3 had no `path` field, so legacy entries load with
    /// `path = None` and get promoted to path-pinned on the next
    /// `cache refresh` (see [`Cache::upsert`]'s promotion branch).
    /// Post-migration every entry should have a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,

    /// Unix epoch seconds. `None` means the entry has never been
    /// refreshed (e.g. just created by `upsert`). [`is_stale`]
    /// treats `None` as always-stale.
    ///
    /// [`is_stale`]: RepoCache::is_stale
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed: Option<u64>,

    /// Last captured per-repo git state — branch + dirty + ahead /
    /// behind. Populated by [`crate::git_status::capture`] during
    /// `cache refresh`. `None` when a refresh hasn't run yet, or
    /// when the repo couldn't be opened (the refresher logs a warn
    /// and leaves the previous snapshot untouched). The TUI renders
    /// `?` for repos in the `None` state so users can tell "haven't
    /// checked yet" apart from "clean".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<GitStatusSnapshot>,

    /// Cached GitHub snapshot — open PR count + most-recent CI
    /// conclusion. Populated by [`crate::gh::capture_snapshot`]
    /// during `cache refresh` when a token is reachable and the
    /// repo's host is `github.com`. `None` for non-github hosts,
    /// missing tokens, or rate-limit / network errors — the TUI
    /// renders `-` in those cells so users can tell "no data" apart
    /// from a definite zero PRs / no CI runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh: Option<GhSnapshot>,
}

impl RepoCache {
    /// Construct a minimal triple-only entry — never-refreshed,
    /// no path. Tests use this; production code should prefer
    /// [`Self::with_path`] (or go through [`Cache::upsert`]) so the
    /// path-aware identity stays consistent end-to-end.
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
            path: None,
            last_refreshed: None,
            git_status: None,
            gh: None,
        }
    }

    /// Constructor with the path baked in. Path-aware
    /// [`Cache::upsert`] uses this when creating a new entry for a
    /// shelf repo that carries a path.
    pub fn with_path(
        host: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
            path,
            last_refreshed: None,
            git_status: None,
            gh: None,
        }
    }

    pub fn slug(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    /// `true` iff the entry hasn't been refreshed within
    /// `threshold_secs` of `now`. Never-refreshed entries
    /// (`last_refreshed: None`) are always stale.
    ///
    /// Uses `saturating_sub` so a clock skew where the cached
    /// timestamp is *ahead* of `now` doesn't wrap to a huge number
    /// and mis-report the entry as stale.
    pub fn is_stale(&self, threshold_secs: u64, now: u64) -> bool {
        match self.last_refreshed {
            None => true,
            Some(ts) => now.saturating_sub(ts) > threshold_secs,
        }
    }
}

/// Process-time Unix epoch seconds. Pulled out so tests can pass
/// `now` explicitly (and so the future TUI / scheduler can plug in
/// a deterministic clock if needed).
pub fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Cache {
    pub fn load(paths: &ShokaPaths) -> Result<Self> {
        Self::load_from(paths.cache_file().as_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        // Direct read + NotFound match — same TOCTOU-free shape as
        // state::Shelf::load_from.
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading cache from {}", path.display()));
            }
        };
        let cache: Cache =
            toml::from_str(&raw).with_context(|| format!("parsing cache at {}", path.display()))?;
        if cache.version > CACHE_VERSION {
            bail!(
                "cache at {} has schema version {}, newer than this build's {} — upgrade shoka",
                path.display(),
                cache.version,
                CACHE_VERSION
            );
        }
        Ok(cache)
    }

    pub fn save(&self, paths: &ShokaPaths) -> Result<()> {
        self.save_to(paths.cache_file().as_path())
    }

    /// Atomic save: write to `<file>.<pid>.tmp` in the same dir,
    /// then rename. Same shape as [`crate::state::Shelf::save_to`]
    /// — the comments there explain the rationale.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialising cache to TOML")?;
        let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating temp cache file {}", tmp.display()))?;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing temp cache file {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("syncing temp cache file {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Path-aware lookup. Returns the entry whose `(host, owner,
    /// name, path)` all match exactly — `path: None` only matches
    /// the legacy path-less entry, `path: Some(p)` only matches an
    /// entry pinned to the same `p`. Use this for fields that are
    /// per-checkout (the headline case being [`RepoCache::git_status`]).
    pub fn find(
        &self,
        host: &str,
        owner: &str,
        name: &str,
        path: Option<&Path>,
    ) -> Option<&RepoCache> {
        self.repos.iter().find(|r| {
            r.host == host && r.owner == owner && r.name == name && r.path.as_deref() == path
        })
    }

    /// Same as [`Self::find`], but `&mut`.
    pub fn find_mut(
        &mut self,
        host: &str,
        owner: &str,
        name: &str,
        path: Option<&Path>,
    ) -> Option<&mut RepoCache> {
        self.repos.iter_mut().find(|r| {
            r.host == host && r.owner == owner && r.name == name && r.path.as_deref() == path
        })
    }

    /// Triple-only lookup. Returns the first entry whose
    /// `(host, owner, name)` matches, regardless of path. Use this
    /// when you need any sibling — e.g. membership tests, not for
    /// reading per-checkout state. For the [`RepoCache::gh`] field
    /// specifically, prefer [`Self::find_gh_by_triple`], which
    /// skips unpopulated siblings so a half-refreshed cache still
    /// surfaces the upstream value.
    pub fn find_any_by_triple(&self, host: &str, owner: &str, name: &str) -> Option<&RepoCache> {
        self.repos
            .iter()
            .find(|r| r.host == host && r.owner == owner && r.name == name)
    }

    /// Resolve the shared GitHub snapshot for `(host, owner, name)`.
    /// Walks every sibling row (each path-pinned clone gets its own
    /// row) and returns the first one with a populated `gh` field.
    /// A naive "first triple match" would lose to row ordering: if
    /// clone A's refresh errored and left `gh = None`, but clone B
    /// succeeded, the TUI must see B's snapshot rather than A's
    /// `None`. The fallback is principled — `gh` is remote-derived
    /// so any populated sibling is the same upstream value.
    pub fn find_gh_by_triple(&self, host: &str, owner: &str, name: &str) -> Option<&GhSnapshot> {
        self.repos
            .iter()
            .filter(|r| r.host == host && r.owner == owner && r.name == name)
            .find_map(|r| r.gh.as_ref())
    }

    /// Get the cache entry for `repo`, inserting a freshly-default
    /// one if it doesn't exist yet. Returns a `&mut` borrow so the
    /// caller can immediately update `last_refreshed` / status
    /// fields.
    ///
    /// Path-aware with two-step self-heal:
    ///
    /// 1. **Strict match** — same triple AND same path: reuse the
    ///    entry (the normal case once migration is complete).
    /// 2. **Promotion** — `repo.path.is_some()` AND a legacy
    ///    path-less entry with the same triple exists: claim it by
    ///    setting `path`. This is how a CACHE_VERSION 3 cache file
    ///    migrates to v4 incrementally — the first refresh after the
    ///    bump fills in the path, the second sees a strict match.
    ///    With multi-clone shelves the promotion claims the legacy
    ///    entry for whichever shelf row gets refreshed first; later
    ///    rows fall through to the new-entry branch below, which is
    ///    the correct behaviour (each path gets its own row).
    /// 3. **New entry** — no match either way: push fresh.
    pub fn upsert(&mut self, repo: &crate::state::Repo) -> &mut RepoCache {
        if let Some(i) = self.repos.iter().position(|r| {
            r.host == repo.host
                && r.owner == repo.owner
                && r.name == repo.name
                && r.path.as_deref() == repo.path.as_deref()
        }) {
            return &mut self.repos[i];
        }

        if repo.path.is_some() {
            if let Some(i) = self.repos.iter().position(|r| {
                r.host == repo.host
                    && r.owner == repo.owner
                    && r.name == repo.name
                    && r.path.is_none()
            }) {
                self.repos[i].path = repo.path.clone();
                return &mut self.repos[i];
            }
        }

        self.repos.push(RepoCache::with_path(
            &repo.host,
            &repo.owner,
            &repo.name,
            repo.path.clone(),
        ));
        self.repos.last_mut().expect("just pushed")
    }

    /// Drop the entry matching `(host, owner, name, path)` exactly.
    /// Returns the removed entry, or `None` if not present. Strict
    /// path equality so a `shoka prune` against one of two
    /// path-pinned clones of the same remote can't accidentally
    /// remove its sibling's row.
    pub fn remove(
        &mut self,
        host: &str,
        owner: &str,
        name: &str,
        path: Option<&Path>,
    ) -> Option<RepoCache> {
        let pos = self.repos.iter().position(|r| {
            r.host == host && r.owner == owner && r.name == name && r.path.as_deref() == path
        })?;
        Some(self.repos.remove(pos))
    }

    pub fn len(&self) -> usize {
        self.repos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Repo;
    use std::fs;
    use tempfile::TempDir;

    fn sample(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cache.toml");
        assert!(!target.exists());
        let c = Cache::load_from(&target).expect("missing -> default");
        assert_eq!(c.version, CACHE_VERSION);
        assert!(c.repos.is_empty());
    }

    #[test]
    fn save_then_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested").join("cache.toml");

        let mut c = Cache::default();
        c.upsert(&sample("shoka")).last_refreshed = Some(1_700_000_000);
        c.upsert(&sample("renri")); // never-refreshed
        c.save_to(&target).unwrap();
        assert!(target.exists());

        let loaded = Cache::load_from(&target).unwrap();
        assert_eq!(loaded.repos.len(), 2);
        assert_eq!(
            loaded
                .find("github.com", "yukimemi", "shoka", None)
                .unwrap()
                .last_refreshed,
            Some(1_700_000_000)
        );
        assert!(
            loaded
                .find("github.com", "yukimemi", "renri", None)
                .unwrap()
                .last_refreshed
                .is_none()
        );
    }

    #[test]
    fn save_uses_pid_suffixed_temp_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cache.toml");
        let mut c = Cache::default();
        c.upsert(&sample("shoka"));
        c.save_to(&target).unwrap();

        let leftover: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("tmp"))
            .map(|e| e.file_name())
            .collect();
        assert!(
            leftover.is_empty(),
            "no .tmp siblings after rename, got: {leftover:?}"
        );
        assert!(target.exists());
    }

    #[test]
    fn future_version_fails_load() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cache.toml");
        fs::write(&target, format!("version = {}\n", CACHE_VERSION + 1)).unwrap();
        let err = Cache::load_from(&target).unwrap_err();
        assert!(
            err.to_string().contains("newer"),
            "error should mention newer schema: {err}"
        );
    }

    #[test]
    fn upsert_creates_then_updates() {
        let mut c = Cache::default();
        let r = sample("shoka");
        let entry1 = c.upsert(&r);
        entry1.last_refreshed = Some(100);
        assert_eq!(c.len(), 1);

        // Second upsert reuses the existing row.
        let entry2 = c.upsert(&r);
        assert_eq!(entry2.last_refreshed, Some(100));
        entry2.last_refreshed = Some(200);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c.find("github.com", "yukimemi", "shoka", None)
                .unwrap()
                .last_refreshed,
            Some(200)
        );
    }

    #[test]
    fn is_stale_treats_unrefreshed_as_stale() {
        let r = RepoCache::new("github.com", "u", "n");
        assert!(r.is_stale(60, 1_700_000_000));
    }

    #[test]
    fn is_stale_within_threshold_is_fresh() {
        let mut r = RepoCache::new("github.com", "u", "n");
        r.last_refreshed = Some(1_700_000_000);
        assert!(!r.is_stale(60, 1_700_000_000 + 30)); // 30s < 60s
        assert!(!r.is_stale(60, 1_700_000_000 + 60)); // boundary: == threshold is fresh
    }

    #[test]
    fn is_stale_beyond_threshold_is_stale() {
        let mut r = RepoCache::new("github.com", "u", "n");
        r.last_refreshed = Some(1_700_000_000);
        assert!(r.is_stale(60, 1_700_000_000 + 61));
        assert!(r.is_stale(60, 1_700_000_000 + 3600));
    }

    #[test]
    fn is_stale_handles_clock_skew_safely() {
        // last_refreshed is *ahead* of `now` (clock went backwards
        // between writes, or two machines disagree). saturating_sub
        // returns 0, which is <= threshold → not stale. Avoid the
        // wrapping-overflow misread that would otherwise flag every
        // such entry as stale.
        let mut r = RepoCache::new("github.com", "u", "n");
        r.last_refreshed = Some(1_700_000_100);
        assert!(!r.is_stale(60, 1_700_000_000));
    }

    #[test]
    fn remove_returns_entry_and_shrinks_cache() {
        let mut c = Cache::default();
        c.upsert(&sample("shoka"));
        c.upsert(&sample("renri"));
        let removed = c.remove("github.com", "yukimemi", "shoka", None).unwrap();
        assert_eq!(removed.name, "shoka");
        assert_eq!(c.len(), 1);
        assert!(c.remove("github.com", "yukimemi", "ghost", None).is_none());
    }

    fn pinned(name: &str, path: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name).with_path(PathBuf::from(path))
    }

    #[test]
    fn path_aware_round_trip_keeps_two_clones_independent() {
        // Two checkouts of the same remote at different paths must
        // serialise as two distinct rows and load back with their
        // own identities intact. This is the core regression
        // scenario from #57 — pre-v4 schema collapsed both into one
        // entry and the second clone's status overwrote the first.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cache.toml");

        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka")).last_refreshed = Some(100);
        c.upsert(&pinned("shoka", "/home/u/b/shoka")).last_refreshed = Some(200);
        assert_eq!(c.len(), 2);
        c.save_to(&target).unwrap();

        let loaded = Cache::load_from(&target).unwrap();
        assert_eq!(loaded.len(), 2);
        let a = loaded
            .find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/a/shoka")),
            )
            .expect("path-a entry survives round trip");
        let b = loaded
            .find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/b/shoka")),
            )
            .expect("path-b entry survives round trip");
        assert_eq!(a.last_refreshed, Some(100));
        assert_eq!(b.last_refreshed, Some(200));
    }

    #[test]
    fn find_with_mismatched_path_returns_none() {
        // Strict equality — `Some(p)` must not match a path-less
        // entry, and `None` must not match a path-pinned entry.
        // This is what makes multi-clone `git_status` reads
        // unambiguous: each row only sees its own snapshot.
        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka"));
        assert!(
            c.find("github.com", "yukimemi", "shoka", None).is_none(),
            "triple-less query must not match a path-pinned entry"
        );
        assert!(
            c.find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/b/shoka")),
            )
            .is_none(),
            "wrong-path query must not match a different-path entry"
        );
    }

    #[test]
    fn find_any_by_triple_returns_first_match_regardless_of_path() {
        // `find_any_by_triple` is the membership-style lookup —
        // returns the first sibling so callers can ask "is there at
        // least one cache row for this triple?". Read-the-gh-field
        // callers should reach for `find_gh_by_triple` instead
        // (see the populated-preference test below).
        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka")).gh = None;
        c.upsert(&pinned("shoka", "/home/u/b/shoka")).gh = None;
        assert!(
            c.find_any_by_triple("github.com", "yukimemi", "shoka")
                .is_some()
        );
        assert!(
            c.find_any_by_triple("github.com", "yukimemi", "ghost")
                .is_none()
        );
    }

    #[test]
    fn find_gh_by_triple_prefers_populated_sibling_over_unpopulated_first_row() {
        // Regression guard for the Gemini PR #62 finding: a
        // half-refreshed cache where clone A's `gh` capture errored
        // (left as `None`) but clone B's succeeded must surface B's
        // snapshot. A naive "first triple match wins" would clobber
        // every TUI row with A's `None`. The walk continues until
        // it lands on a populated `gh`.
        use crate::gh::{CiStatus, GhSnapshot};
        let populated = GhSnapshot {
            open_pr_count: Some(3),
            ci_status: Some(CiStatus::Success),
        };

        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka")).gh = None;
        c.upsert(&pinned("shoka", "/home/u/b/shoka")).gh = Some(populated.clone());

        let found = c
            .find_gh_by_triple("github.com", "yukimemi", "shoka")
            .expect("populated sibling must be reachable");
        assert_eq!(found, &populated);
    }

    #[test]
    fn find_gh_by_triple_returns_none_when_every_sibling_is_unpopulated() {
        // No populated row → genuine `None`. The TUI renders `-`
        // for that case, which is the right "no data yet" cue
        // (distinct from a confirmed-zero `Some(0)`).
        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka")).gh = None;
        c.upsert(&pinned("shoka", "/home/u/b/shoka")).gh = None;
        assert!(
            c.find_gh_by_triple("github.com", "yukimemi", "shoka")
                .is_none()
        );
    }

    #[test]
    fn upsert_promotes_legacy_path_less_entry_to_path_pinned() {
        // Pre-v4 cache file: one path-less entry. After v0.12.0's
        // shelf migration the shelf row carries a path, so on first
        // refresh the upsert should *claim* the legacy entry by
        // setting its path, not push a new row. This is the
        // self-heal path that keeps the cache file from growing an
        // orphan row per repo on the upgrade.
        let mut c = Cache::default();
        c.repos
            .push(RepoCache::new("github.com", "yukimemi", "shoka"));
        c.repos[0].last_refreshed = Some(500);

        let entry = c.upsert(&pinned("shoka", "/home/u/a/shoka"));
        entry.last_refreshed = Some(1000);
        assert_eq!(c.len(), 1, "promotion must reuse the legacy row");
        assert_eq!(
            c.repos[0].path.as_deref(),
            Some(Path::new("/home/u/a/shoka"))
        );
        assert_eq!(c.repos[0].last_refreshed, Some(1000));
    }

    #[test]
    fn upsert_creates_distinct_rows_for_two_clones_of_same_remote() {
        // Second clone of the same remote at a different path can't
        // reuse the first row (its `git_status` would clobber). The
        // promotion branch only applies when a *legacy path-less*
        // row exists; once the first path is claimed, the second
        // shelf row falls through to the new-entry push.
        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka")).last_refreshed = Some(100);
        c.upsert(&pinned("shoka", "/home/u/b/shoka")).last_refreshed = Some(200);
        assert_eq!(c.len(), 2, "different paths -> different rows");
        assert_eq!(
            c.find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/a/shoka")),
            )
            .unwrap()
            .last_refreshed,
            Some(100)
        );
        assert_eq!(
            c.find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/b/shoka")),
            )
            .unwrap()
            .last_refreshed,
            Some(200)
        );
    }

    #[test]
    fn remove_strict_path_does_not_take_siblings() {
        // `shoka prune` removes a single shelf row; the cache's
        // `remove` must mirror that — strict identity so a prune of
        // the `/a/` clone leaves the `/b/` clone's row intact.
        // Without strict equality a prune would silently zero the
        // sibling's `git_status` snapshot.
        let mut c = Cache::default();
        c.upsert(&pinned("shoka", "/home/u/a/shoka"));
        c.upsert(&pinned("shoka", "/home/u/b/shoka"));
        let removed = c
            .remove(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/a/shoka")),
            )
            .expect("targeted row removed");
        assert_eq!(removed.path.as_deref(), Some(Path::new("/home/u/a/shoka")));
        assert_eq!(c.len(), 1);
        assert!(
            c.find(
                "github.com",
                "yukimemi",
                "shoka",
                Some(Path::new("/home/u/b/shoka")),
            )
            .is_some(),
            "sibling clone must survive"
        );
    }

    #[test]
    fn version_4_cache_file_loads_without_path_field_on_legacy_rows() {
        // CACHE_VERSION 3 → 4 has to be a *forward-compatible*
        // schema bump: a freshly-upgraded shoka must be able to read
        // a v3 cache.toml that has no `path` field on any row.
        // `serde(default)` on `RepoCache::path` should pick up
        // `None` and the load should succeed without complaint.
        //
        // Pin the fixture's version field to literal `3` — using
        // `CACHE_VERSION` would silently track future bumps and
        // stop exercising the v3 migration contract this test
        // exists to guard.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cache.toml");
        let v3 = "version = 3\n\n[[repos]]\nhost = \"github.com\"\nowner = \"yukimemi\"\nname = \"shoka\"\nlast_refreshed = 100\n";
        fs::write(&target, v3).unwrap();
        let loaded = Cache::load_from(&target).expect("legacy v3-shape row loads");
        assert_eq!(loaded.version, 3, "version field round-trips as v3");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.repos[0].path, None);
    }
}
