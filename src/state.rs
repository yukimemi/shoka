//! Portable shelf ledger.
//!
//! Lives at `$XDG_DATA_HOME/shoka/state.toml` (or the platform
//! equivalent — see [`crate::paths::ShokaPaths`]). Owned by shoka:
//! reads / writes happen through CLI commands (`clone`, `tag`,
//! `set`, `note`, `import`, …); direct hand-editing isn't expected
//! (though it's a plain TOML file, so nothing physically prevents
//! it). `shoka export` writes exactly the bytes shoka stores here
//! and `shoka import` reads them back unchanged.
//!
//! Save is atomic via temp-file + rename so a crash mid-write
//! can't leave the shelf truncated: a partial `state.toml.tmp`
//! survives in the dir as a forensic artefact instead of clobbering
//! the previous good `state.toml`.
//!
//! ## Schema versioning
//!
//! [`Shelf`] carries a [`Shelf::version`] field. Loaders refuse a
//! file whose version is *newer* than the current build's
//! [`SHELF_VERSION`] — that file came from a future shoka and
//! probably has fields this build doesn't understand. Old versions
//! are loaded as-is; migrations should happen on save when a
//! schema bump lands.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use std::path::PathBuf;

use crate::config::VcsDefault;
use crate::paths::ShokaPaths;

/// Current on-disk schema version. Bumped to **2** when the per-repo
/// `path` override field landed. Readers with the old version on disk
/// see the new optional field as default-`None` and keep working.
pub const SHELF_VERSION: u32 = 2;

/// Top-level state on disk.
///
/// Single `[[repos]]` array of [`Repo`] records, plus a leading
/// `version = ...` line. The file is intentionally simple so a
/// human can sanity-check it even if shoka isn't running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shelf {
    /// Schema version. See module docs.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Repos on the shelf. Order is insertion order — callers
    /// shouldn't rely on alphabetical / topological sorting here.
    #[serde(default)]
    pub repos: Vec<Repo>,
}

fn default_version() -> u32 {
    SHELF_VERSION
}

impl Default for Shelf {
    fn default() -> Self {
        Self {
            version: SHELF_VERSION,
            repos: Vec::new(),
        }
    }
}

/// One repo on the shelf. Identity is the `(host, owner, name)`
/// triple; everything else is mutable metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub host: String,
    pub owner: String,
    pub name: String,

    /// Explicit per-repo VCS override. `None` means "auto-detect
    /// at use time" (the resolver inspects `.git/` / `.jj/` on the
    /// working tree). Set this when auto-detection picks the wrong
    /// one or when a colocated checkout exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs: Option<VcsDefault>,

    /// Free-form tag set used by `--tag` filters and TUI grouping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Per-repo annotation. Surfaced by `shoka list` / `tui` /
    /// `note` for the user's own bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    /// Absolute on-disk path override. `None` means "compute the
    /// path from the configured layout + `(host, owner, name)`"
    /// (the normal cloned-by-shoka case); `Some(p)` pins this repo
    /// to `p` regardless of layout (used by `shoka import` for
    /// local-only repos that were never cloned via shoka, so they
    /// stay where the user already has them on disk).
    ///
    /// Treated as opaque by shoka — no expansion, no canonicalisation
    /// at save time. Callers that resolve relative paths or `~/`
    /// should do so before constructing the [`Repo`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Repo {
    /// Convenience constructor for the common "just got cloned"
    /// case: identity only, no metadata.
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
            vcs: None,
            tags: Vec::new(),
            note: None,
            path: None,
        }
    }

    /// Builder-style override: pin this repo to an explicit on-disk
    /// path, bypassing the `[global].layout` template. Used by
    /// [`shoka import`] for local-only / jj-only repos that don't
    /// have a remote URL to derive a `host/owner/name` triple from,
    /// and that the user wants to leave in place rather than move
    /// under the configured `root`.
    ///
    /// [`shoka import`]: crate::commands::import
    pub fn with_path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// `host/owner/name` slug used in CLI output and error messages.
    pub fn slug(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }
}

impl Shelf {
    /// Load the shelf from `paths.state_file()`.
    ///
    /// Returns [`Shelf::default`] when the file doesn't exist (the
    /// "fresh install" case). Errors out on TOML parse failure or
    /// when the file's schema version is *newer* than this build's
    /// — that file is from a future shoka and likely carries
    /// fields this build wouldn't preserve on save.
    pub fn load(paths: &ShokaPaths) -> Result<Self> {
        Self::load_from(paths.state_file().as_path())
    }

    /// Lower-level variant used in tests and by `shoka import`.
    pub fn load_from(path: &Path) -> Result<Self> {
        // Attempt the read directly rather than `path.exists()` +
        // `read_to_string`. The two-step variant is a TOCTOU: another
        // process can delete (or create) the file between the check
        // and the read; the single read with a `NotFound` match is
        // both race-free and the idiomatic Rust shape.
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading shelf from {}", path.display()));
            }
        };
        let shelf: Shelf =
            toml::from_str(&raw).with_context(|| format!("parsing shelf at {}", path.display()))?;
        if shelf.version > SHELF_VERSION {
            bail!(
                "shelf at {} has schema version {}, newer than this build's {} — upgrade shoka",
                path.display(),
                shelf.version,
                SHELF_VERSION
            );
        }
        Ok(shelf)
    }

    /// Atomically save the shelf to `paths.state_file()`.
    ///
    /// Writes to a sibling `state.toml.tmp` and renames over the
    /// target so a mid-write crash leaves either the previous
    /// good file or an `.tmp` for forensics — never a half-written
    /// `state.toml`. Creates the parent dir as needed.
    pub fn save(&self, paths: &ShokaPaths) -> Result<()> {
        self.save_to(paths.state_file().as_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state dir {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialising shelf to TOML")?;
        // Atomic write: temp file in the same dir + rename. Same-dir
        // ensures rename(2) is atomic on POSIX (the cross-filesystem
        // case is what makes /tmp → /home risky). Windows MoveFileEx
        // with MOVEFILE_REPLACE_EXISTING (which `std::fs::rename`
        // uses) gives the same guarantee within a volume.
        //
        // PID-suffixed tmp filename so concurrent `shoka` invocations
        // don't trample each other's in-flight writes.
        let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating temp shelf file {}", tmp.display()))?;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing temp shelf file {}", tmp.display()))?;
            // Propagate sync_all failures so we never rename a
            // partially-flushed file over a known-good state.toml.
            // The cost is a stalled save on a degraded filesystem,
            // but that's the right call when the alternative is
            // silent corruption of the user's shelf ledger.
            f.sync_all()
                .with_context(|| format!("syncing temp shelf file {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Look up a repo by `(host, owner, name)`. Returns the first
    /// match — when the shelf has multiple checkouts of the same
    /// remote (e.g. `git clone <url> <other-name>`), only one is
    /// returned here. Callers that need to disambiguate by on-disk
    /// path should use [`find_by_path`](Self::find_by_path).
    pub fn find(&self, host: &str, owner: &str, name: &str) -> Option<&Repo> {
        self.repos
            .iter()
            .find(|r| r.host == host && r.owner == owner && r.name == name)
    }

    /// Mutable variant for callers that want to update metadata
    /// (tags, vcs override, note) in place. First-match semantics
    /// match [`find`](Self::find).
    pub fn find_mut(&mut self, host: &str, owner: &str, name: &str) -> Option<&mut Repo> {
        self.repos
            .iter_mut()
            .find(|r| r.host == host && r.owner == owner && r.name == name)
    }

    /// Look up by the full `(host, owner, name, path)` identity. Used
    /// by `shoka import` to upsert exactly the entry it just walked
    /// to on disk, even when the shelf already carries other clones
    /// of the same remote.
    ///
    /// Matching rule:
    ///
    /// - `path = Some(p)` matches an entry with `path = Some(p)`.
    /// - `path = None` matches an entry with `path = None` (the
    ///   "shoka-clone laid this out, no override" case).
    ///
    /// A mismatched path / None never collapses to "close enough" —
    /// that's exactly the disambiguation we need to keep multiple
    /// clones of the same remote as distinct shelf entries.
    pub fn find_by_path(
        &self,
        host: &str,
        owner: &str,
        name: &str,
        path: Option<&Path>,
    ) -> Option<&Repo> {
        self.repos.iter().find(|r| {
            r.host == host && r.owner == owner && r.name == name && r.path.as_deref() == path
        })
    }

    /// Mutable variant of [`find_by_path`](Self::find_by_path).
    pub fn find_mut_by_path(
        &mut self,
        host: &str,
        owner: &str,
        name: &str,
        path: Option<&Path>,
    ) -> Option<&mut Repo> {
        self.repos.iter_mut().find(|r| {
            r.host == host && r.owner == owner && r.name == name && r.path.as_deref() == path
        })
    }

    /// Insert a new repo. Errors out only if a row with the **full**
    /// `(host, owner, name, path)` identity already exists — same
    /// triple with a different `path` is allowed (and exactly the
    /// case `shoka import` needs when one remote is checked out
    /// under multiple local dir names). Callers that want
    /// upsert-style behaviour for the same path should
    /// `find_mut_by_path` first.
    pub fn add(&mut self, repo: Repo) -> Result<()> {
        if self
            .find_by_path(&repo.host, &repo.owner, &repo.name, repo.path.as_deref())
            .is_some()
        {
            let where_clause = match &repo.path {
                Some(p) => format!(" at {}", p.display()),
                None => String::new(),
            };
            bail!("repo {}{where_clause} already on the shelf", repo.slug());
        }
        self.repos.push(repo);
        Ok(())
    }

    /// Remove and return a repo by identity triple. Returns `None`
    /// if not present.
    pub fn remove(&mut self, host: &str, owner: &str, name: &str) -> Option<Repo> {
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
    use std::fs;
    use tempfile::TempDir;

    fn sample_repo(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    // Tests deliberately use `load_from` / `save_to` with paths under
    // a [`TempDir`] rather than `Shelf::{load,save}` — that keeps the
    // user's real `state_dir` untouched and dodges the
    // `directories::ProjectDirs`-dependent path resolution.

    #[test]
    fn missing_file_yields_empty_shelf() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");
        assert!(!target.exists());
        let shelf = Shelf::load_from(&target).expect("missing file -> default");
        assert_eq!(shelf.version, SHELF_VERSION);
        assert!(shelf.repos.is_empty());
    }

    #[test]
    fn save_then_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested").join("state.toml");

        let mut shelf = Shelf::default();
        shelf
            .add({
                let mut r = sample_repo("shoka");
                r.tags = vec!["rust".into(), "cli".into()];
                r.note = Some("the bookshelf itself".into());
                r
            })
            .unwrap();
        shelf.add(sample_repo("renri")).unwrap();
        shelf
            .add({
                let mut r = sample_repo("kanade");
                r.vcs = Some(VcsDefault::Jj);
                r
            })
            .unwrap();

        shelf.save_to(&target).expect("save");
        assert!(target.exists(), "save should create the file");

        let loaded = Shelf::load_from(&target).expect("load");
        assert_eq!(loaded.version, SHELF_VERSION);
        // Round-trip equality is the strongest check — if a field
        // round-tripped wrong (e.g. None → Some("")), this would
        // catch it.
        assert_eq!(loaded.repos, shelf.repos);
    }

    #[test]
    fn save_skips_optional_fields_for_repos_that_dont_set_them() {
        // Direct check that `#[serde(default, skip_serializing_if =
        // ...)]` actually keeps the file clean: a repo with no note
        // / no tags / no vcs override produces no key for those.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");

        let mut shelf = Shelf::default();
        shelf.add(sample_repo("renri")).unwrap();
        shelf.save_to(&target).unwrap();

        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains("name = \"renri\""));
        assert!(
            !body.contains("note"),
            "no-note repo should not emit a `note =` key, got:\n{body}"
        );
        assert!(
            !body.contains("tags"),
            "no-tags repo should not emit a `tags =` key, got:\n{body}"
        );
        assert!(
            !body.contains("vcs"),
            "no-vcs-override repo should not emit a `vcs =` key, got:\n{body}"
        );
    }

    #[test]
    fn save_uses_atomic_temp_file() {
        // We don't observe the temp file mid-write (would race), but
        // we do verify the post-rename state: target exists, no
        // `.tmp` siblings remain (rename consumed our pid-suffixed
        // temp file), and the content is what we wrote.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");

        let mut shelf = Shelf::default();
        shelf.add(sample_repo("shoka")).unwrap();
        shelf.save_to(&target).unwrap();

        assert!(target.exists());

        let leftover_tmp: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("tmp"))
            .map(|e| e.file_name())
            .collect();
        assert!(
            leftover_tmp.is_empty(),
            "no .tmp siblings should remain after rename, found: {leftover_tmp:?}"
        );

        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains("\"shoka\""));
    }

    #[test]
    fn save_overwrites_existing_file_without_corruption() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");
        fs::write(&target, "version = 1\n\n[[repos]]\nhost = \"github.com\"\nowner = \"old\"\nname = \"obsolete\"\n").unwrap();

        let mut shelf = Shelf::default();
        shelf.add(sample_repo("fresh")).unwrap();
        shelf.save_to(&target).unwrap();

        let loaded = Shelf::load_from(&target).unwrap();
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].name, "fresh");
    }

    #[test]
    fn add_rejects_duplicate_identity_without_path() {
        // Two entries with the same triple AND no path override
        // collide — that's the "shoka clone laid this out" case
        // where the layout-derived destination is by definition
        // unique per triple.
        let mut shelf = Shelf::default();
        shelf.add(sample_repo("shoka")).unwrap();
        let err = shelf.add(sample_repo("shoka")).unwrap_err();
        assert!(
            err.to_string().contains("already on the shelf"),
            "error should mention duplicate: {err}"
        );
        assert_eq!(shelf.len(), 1, "duplicate add must not mutate");
    }

    #[test]
    fn add_allows_same_triple_with_different_paths() {
        // `git clone <url> <other-name>` (or a local rename) lands
        // multiple checkouts under the same remote. They must coexist
        // on the shelf as distinct entries — that's the whole point
        // of moving from a triple-only identity to triple + path.
        let mut shelf = Shelf::default();
        let mut a = sample_repo("admintask");
        a.path = Some(PathBuf::from(
            "/home/u/src/github.com/yukimemi/DeviceManagement",
        ));
        let mut b = sample_repo("admintask");
        b.path = Some(PathBuf::from("/home/u/src/old/admintask-backup"));
        shelf.add(a).expect("first path-pinned admintask");
        shelf
            .add(b)
            .expect("second path-pinned admintask must coexist");
        assert_eq!(shelf.len(), 2);
    }

    #[test]
    fn add_rejects_duplicate_triple_and_path() {
        // Exact (triple, path) duplicate is still a no-op — the
        // path-aware identity is the new uniqueness contract.
        let mut shelf = Shelf::default();
        let path = PathBuf::from("/home/u/src/github.com/yukimemi/shoka");
        let mut a = sample_repo("shoka");
        a.path = Some(path.clone());
        let mut b = sample_repo("shoka");
        b.path = Some(path);
        shelf.add(a).unwrap();
        let err = shelf.add(b).unwrap_err();
        assert!(
            err.to_string().contains("already on the shelf"),
            "error should mention duplicate: {err}"
        );
        assert_eq!(shelf.len(), 1);
    }

    #[test]
    fn find_by_path_distinguishes_clones_of_same_remote() {
        let mut shelf = Shelf::default();
        let mut a = sample_repo("admintask");
        let pa = PathBuf::from("/a/DeviceManagement");
        a.path = Some(pa.clone());
        let mut b = sample_repo("admintask");
        let pb = PathBuf::from("/b/admintask-backup");
        b.path = Some(pb.clone());
        shelf.add(a).unwrap();
        shelf.add(b).unwrap();

        let found_a = shelf.find_by_path("github.com", "yukimemi", "admintask", Some(&pa));
        assert_eq!(found_a.unwrap().path.as_deref(), Some(pa.as_path()));

        let found_b = shelf.find_by_path("github.com", "yukimemi", "admintask", Some(&pb));
        assert_eq!(found_b.unwrap().path.as_deref(), Some(pb.as_path()));

        // path-less lookup against an all-pinned set returns None,
        // even though the triple is present — exactly the property
        // the importer's upsert relies on (no accidental match).
        assert!(
            shelf
                .find_by_path("github.com", "yukimemi", "admintask", None)
                .is_none()
        );
    }

    #[test]
    fn find_and_find_mut_locate_by_triple() {
        let mut shelf = Shelf::default();
        shelf.add(sample_repo("shoka")).unwrap();
        shelf.add(sample_repo("renri")).unwrap();

        let r = shelf.find("github.com", "yukimemi", "renri").unwrap();
        assert_eq!(r.name, "renri");

        let mr = shelf.find_mut("github.com", "yukimemi", "shoka").unwrap();
        mr.tags.push("rust".into());
        let r2 = shelf.find("github.com", "yukimemi", "shoka").unwrap();
        assert_eq!(r2.tags, vec!["rust".to_string()]);

        assert!(shelf.find("github.com", "yukimemi", "ghost").is_none());
    }

    #[test]
    fn remove_returns_the_repo_and_shrinks_shelf() {
        let mut shelf = Shelf::default();
        shelf.add(sample_repo("shoka")).unwrap();
        shelf.add(sample_repo("renri")).unwrap();

        let removed = shelf.remove("github.com", "yukimemi", "shoka").unwrap();
        assert_eq!(removed.name, "shoka");
        assert_eq!(shelf.len(), 1);
        assert!(shelf.find("github.com", "yukimemi", "shoka").is_none());

        assert!(
            shelf.remove("github.com", "yukimemi", "ghost").is_none(),
            "removing a non-existent repo returns None, not an error"
        );
    }

    #[test]
    fn future_version_fails_load() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");
        fs::write(
            &target,
            format!(
                "version = {}\n\n[[repos]]\nhost = \"github.com\"\nowner = \"o\"\nname = \"n\"\n",
                SHELF_VERSION + 1
            ),
        )
        .unwrap();
        let err = Shelf::load_from(&target).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("newer"),
            "error should mention newer schema: {msg}"
        );
    }

    #[test]
    fn load_tolerates_missing_optional_fields() {
        // A minimal `[[repos]]` entry should deserialise: no `vcs`,
        // no `tags`, no `note`. Tests the `#[serde(default)]`
        // annotations.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("state.toml");
        fs::write(
            &target,
            r#"version = 1

[[repos]]
host = "github.com"
owner = "yukimemi"
name = "shoka"
"#,
        )
        .unwrap();
        let shelf = Shelf::load_from(&target).expect("load");
        assert_eq!(shelf.repos.len(), 1);
        let r = &shelf.repos[0];
        assert_eq!(r.name, "shoka");
        assert_eq!(r.vcs, None);
        assert!(r.tags.is_empty());
        assert!(r.note.is_none());
    }
}
