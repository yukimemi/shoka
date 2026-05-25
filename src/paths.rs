//! OS-aware path resolution for shoka.
//!
//! [`ShokaPaths`] is resolved once at startup and threaded through to the
//! commands that need it. Resolution honours `$SHOKA_CONFIG` (or `--config
//! <path>` on the CLI, which clap also fills from `$SHOKA_CONFIG`) by
//! pinning the config file path directly; otherwise the OS default from
//! [`directories::ProjectDirs::from("", "yukimemi", "shoka")`][pd] is
//! used. On the platforms we care about that resolves to roughly:
//!
//! | platform | config dir                                     |
//! | :------- | :--------------------------------------------- |
//! | Linux    | `$XDG_CONFIG_HOME/shoka`                       |
//! | macOS    | `~/Library/Application Support/yukimemi.shoka` |
//! | Windows  | `%APPDATA%\yukimemi\shoka\config`              |
//!
//! See the [`directories` crate docs][pd] for the authoritative
//! per-platform behaviour and any future changes — the table above is
//! a quick reference, not a contract.
//!
//! `state.toml` always lives under the state dir; `cache.toml` under
//! the cache dir.
//!
//! State / cache locations honour two env-var overrides:
//!
//! - `SHOKA_STATE_DIR` — directory holding `state.toml`.
//! - `SHOKA_CACHE_DIR` — directory holding `cache.toml`.
//!
//! Both are intended for integration tests that must not write into
//! the user's real OS data directory; they're also documented for any
//! advanced caller who deliberately wants non-default locations.
//!
//! [pd]: https://docs.rs/directories/latest/directories/struct.ProjectDirs.html

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// Resolved on-disk paths shoka uses across commands.
#[derive(Debug, Clone)]
pub struct ShokaPaths {
    config_file: PathBuf,
    config_dir: PathBuf,
    state_dir: PathBuf,
    cache_dir: PathBuf,
}

impl ShokaPaths {
    /// Resolve paths.
    ///
    /// If `config_override` is `Some`, it's taken as the explicit config
    /// file path (the layered `config.*.toml` loader then walks that
    /// file's parent directory). Pass `None` to use the OS default.
    pub fn resolve(config_override: Option<&Path>) -> Result<Self> {
        let project = ProjectDirs::from("", "yukimemi", "shoka")
            .context("could not determine OS data directories for shoka")?;
        let default_config_dir = project.config_dir().to_path_buf();

        let (config_file, config_dir) = match config_override {
            Some(p) => {
                // `std::path::absolute` handles `.` / `..` correctly
                // without requiring the path to exist (helpful when
                // first-run setup is about to create the file).
                let abs = std::path::absolute(p)
                    .with_context(|| format!("absolutising config path {}", p.display()))?;
                let dir = abs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                (abs, dir)
            }
            None => (default_config_dir.join("config.toml"), default_config_dir),
        };

        let state_dir =
            env_override("SHOKA_STATE_DIR").unwrap_or_else(|| project.data_dir().to_path_buf());
        let cache_dir =
            env_override("SHOKA_CACHE_DIR").unwrap_or_else(|| project.cache_dir().to_path_buf());

        Ok(Self {
            config_file,
            config_dir,
            state_dir,
            cache_dir,
        })
    }

    /// Directory the layered loader scans for `config.toml` +
    /// `config.*.toml` siblings.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Canonical config file path. Same as `config_dir().join("config.toml")`
    /// unless overridden via `--config`.
    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    /// Directory holding the portable shelf ledger (state.toml).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.toml")
    }

    /// Directory holding volatile cache (cache.toml).
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn cache_file(&self) -> PathBuf {
        self.cache_dir.join("cache.toml")
    }
}

/// Read a directory-pointing env var. Returns `None` for unset or
/// empty; absolutises against cwd via [`std::path::absolute`] so
/// relative inputs (e.g. tests passing temp paths) are consistent
/// with the `config_override` branch above.
fn env_override(var: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(var)?;
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    std::path::absolute(&p).ok().or(Some(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_default_paths_contain_shoka() {
        let p = ShokaPaths::resolve(None).expect("paths resolve");
        assert!(
            p.config_dir()
                .to_string_lossy()
                .to_lowercase()
                .contains("shoka"),
            "config_dir should contain `shoka`: {:?}",
            p.config_dir()
        );
        assert!(
            p.state_dir()
                .to_string_lossy()
                .to_lowercase()
                .contains("shoka"),
            "state_dir should contain `shoka`: {:?}",
            p.state_dir()
        );
        assert_eq!(
            p.state_file().file_name().and_then(|s| s.to_str()),
            Some("state.toml")
        );
        assert_eq!(
            p.cache_file().file_name().and_then(|s| s.to_str()),
            Some("cache.toml")
        );
    }

    #[test]
    fn resolve_with_absolute_override_uses_explicit_path() {
        let dir = std::env::temp_dir().join("shoka-paths-test-abs");
        let file = dir.join("custom.toml");
        let p = ShokaPaths::resolve(Some(&file)).expect("paths resolve");
        assert_eq!(p.config_file(), file);
        assert_eq!(p.config_dir(), dir);
    }

    #[test]
    fn resolve_with_relative_override_becomes_absolute_via_cwd() {
        let p = ShokaPaths::resolve(Some(Path::new("rel/config.toml"))).expect("paths resolve");
        assert!(p.config_file().is_absolute(), "{:?}", p.config_file());
        assert!(p.config_dir().is_absolute(), "{:?}", p.config_dir());
    }

    #[test]
    fn config_file_default_is_config_toml_in_config_dir() {
        let p = ShokaPaths::resolve(None).expect("paths resolve");
        assert_eq!(p.config_file(), p.config_dir().join("config.toml"));
    }
}
