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
//! up top, then `[[repos]]` records keyed by the `(host, owner,
//! name)` triple. Save is atomic via temp-file + rename so a
//! crashed refresh can't corrupt the cache.
//!
//! [`git_status`]: RepoCache::git_status
//! [`gh`]: RepoCache::gh

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git_status::GitStatusSnapshot;
use crate::paths::ShokaPaths;

/// Current on-disk schema version. Bumped to **2** when the per-repo
/// `git_status` field landed — readers with the old version on disk
/// see the optional field as default-`None` and keep working.
pub const CACHE_VERSION: u32 = 2;

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

/// Cache entry for one repo. Identity is the `(host, owner, name)`
/// triple, matching [`crate::state::Repo`].
///
/// Optional fields are `skip_serializing_if = "Option::is_none"` so
/// a freshly-created entry serialises to its minimal shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCache {
    pub host: String,
    pub owner: String,
    pub name: String,

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
    // The Phase 2b `gh: Option<GhSnapshot>` (PR / CI counts via
    // octocrab) lands as a sibling field; another version bump
    // accompanies it so a downgraded shoka refuses the unknown shape
    // rather than dropping the new data silently.
}

impl RepoCache {
    /// Construct a minimal entry — identity only, never-refreshed.
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
            last_refreshed: None,
            git_status: None,
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

    pub fn find(&self, host: &str, owner: &str, name: &str) -> Option<&RepoCache> {
        self.repos
            .iter()
            .find(|r| r.host == host && r.owner == owner && r.name == name)
    }

    pub fn find_mut(&mut self, host: &str, owner: &str, name: &str) -> Option<&mut RepoCache> {
        self.repos
            .iter_mut()
            .find(|r| r.host == host && r.owner == owner && r.name == name)
    }

    /// Get the entry for `repo`, inserting a freshly-default one if
    /// it doesn't exist yet. Returns a `&mut` borrow so the caller
    /// can immediately update `last_refreshed` / future status
    /// fields.
    pub fn upsert(&mut self, repo: &crate::state::Repo) -> &mut RepoCache {
        let pos = self
            .repos
            .iter()
            .position(|r| r.host == repo.host && r.owner == repo.owner && r.name == repo.name);
        match pos {
            Some(i) => &mut self.repos[i],
            None => {
                self.repos
                    .push(RepoCache::new(&repo.host, &repo.owner, &repo.name));
                self.repos.last_mut().expect("just pushed")
            }
        }
    }

    /// Drop the entry for `(host, owner, name)`. Returns the removed
    /// entry, or `None` if not present.
    pub fn remove(&mut self, host: &str, owner: &str, name: &str) -> Option<RepoCache> {
        let pos = self
            .repos
            .iter()
            .position(|r| r.host == host && r.owner == owner && r.name == name)?;
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
                .find("github.com", "yukimemi", "shoka")
                .unwrap()
                .last_refreshed,
            Some(1_700_000_000)
        );
        assert!(
            loaded
                .find("github.com", "yukimemi", "renri")
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
            c.find("github.com", "yukimemi", "shoka")
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
        let removed = c.remove("github.com", "yukimemi", "shoka").unwrap();
        assert_eq!(removed.name, "shoka");
        assert_eq!(c.len(), 1);
        assert!(c.remove("github.com", "yukimemi", "ghost").is_none());
    }
}
