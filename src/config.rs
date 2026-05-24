//! Configuration schema + layered loader.
//!
//! `config.toml` lives at [`crate::paths::ShokaPaths::config_dir`] together
//! with any sibling `config.*.toml` files. They are loaded via
//! [`teravars::load_merged`] in canonical order (`config.toml` first,
//! alphabetical `config.*.toml` next, `config.local.toml` always last so
//! a local override wins), with per-file Tera rendering and a single
//! authoritative `[vars]` resolution at the end.
//!
//! The post-merge [`ShokaConfig`] is then collapsed into a
//! [`ResolvedConfig`] for the active profile / host: profile fields
//! override the corresponding `[global]` fields when set, with anything
//! unset falling through to the global value.
//!
//! Profile resolution order: explicit `--profile <name>` CLI flag,
//! `$SHOKA_PROFILE` environment variable, `global.default_profile`. If
//! none matches, profile-less mode is used (the global values are taken
//! verbatim and no profile-only fields like `git_config` apply).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use teravars::{Engine, discover_config_files, load_merged, system_context};

use crate::paths::ShokaPaths;

/// Top-level shoka configuration as written by the user.
///
/// `[vars]` is intentionally absent here: teravars strips it from the
/// merged config table after resolving cross-references, so it never
/// shows up in the deserialised struct.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShokaConfig {
    pub global: GlobalConfig,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// Filesystem root under which repositories are cloned.
    /// Required: must be set in either `[global]` or the active profile.
    pub root: Option<String>,
    /// Path layout template. Rendered at clone time with the per-repo
    /// `host` / `owner` / `name` context injected on top of vars.
    pub layout: String,
    pub default_vcs: VcsDefault,
    pub default_protocol: Protocol,
    pub default_host: String,
    pub default_profile: Option<String>,
    pub exec_concurrency: usize,
    pub ui: UiConfig,
    pub shell: ShellConfig,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            root: None,
            layout: "{{ root }}/{{ host }}/{{ owner }}/{{ name }}".into(),
            default_vcs: VcsDefault::Auto,
            default_protocol: Protocol::Https,
            default_host: "github.com".into(),
            default_profile: None,
            exec_concurrency: 8,
            ui: UiConfig::default(),
            shell: ShellConfig::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsDefault {
    #[default]
    Auto,
    Git,
    Jj,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Https,
    Ssh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub status_cache_ttl_secs: u64,
    pub tui_refresh_ms: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            status_cache_ttl_secs: 60,
            tui_refresh_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Default function name emitted by `shoka init-shell <shell>`.
    pub cd_command_name: String,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            cd_command_name: "s".into(),
        }
    }
}

/// Per-host overrides — used at clone time / when displaying URLs.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    pub protocol: Option<Protocol>,
    pub layout: Option<String>,
    pub default_vcs: Option<VcsDefault>,
}

/// Per-profile overrides on top of `[global]`.
///
/// Profile fields that are `None` fall through to the corresponding
/// [`GlobalConfig`] value during [`ShokaConfig::resolve`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    pub root: Option<String>,
    pub layout: Option<String>,
    pub default_vcs: Option<VcsDefault>,
    pub default_protocol: Option<Protocol>,
    pub default_host: Option<String>,
    pub exec_concurrency: Option<usize>,
    /// `git config` key/value pairs to inject when operating in this
    /// profile (applied per-repo by `clone` / `exec`).
    pub git_config: BTreeMap<String, String>,
}

/// Profile-resolved view of the config.
///
/// Subcommands consume this. The `raw` field exposes the underlying
/// [`ShokaConfig`] for callers that need to introspect other profiles.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub root: PathBuf,
    pub layout: String,
    pub default_vcs: VcsDefault,
    pub default_protocol: Protocol,
    pub default_host: String,
    pub exec_concurrency: usize,
    pub ui: UiConfig,
    pub shell: ShellConfig,
    pub hosts: BTreeMap<String, HostConfig>,
    pub git_config: BTreeMap<String, String>,
    pub active_profile: Option<String>,
    pub raw: ShokaConfig,
}

impl ShokaConfig {
    /// Load and merge config files from `paths.config_dir()`.
    ///
    /// Returns [`ShokaConfig::default`] when the config dir is missing
    /// or contains no `config*.toml` files.
    pub fn load(paths: &ShokaPaths) -> Result<Self> {
        let dir = paths.config_dir();
        if !dir.exists() {
            return Ok(Self::default());
        }
        let files = discover_config_files(dir)
            .with_context(|| format!("discovering config files in {}", dir.display()))?;
        if files.is_empty() {
            return Ok(Self::default());
        }
        let mut engine = Engine::new();
        let ctx = system_context();
        let merged = load_merged(files.iter(), &mut engine, &ctx)
            .with_context(|| format!("loading config files in {}", dir.display()))?;
        let cfg: ShokaConfig = merged
            .config
            .try_into()
            .context("decoding merged config into ShokaConfig")?;
        Ok(cfg)
    }

    /// Decide which profile is active per the documented order:
    ///   explicit CLI > `$SHOKA_PROFILE` > `global.default_profile`.
    /// Returns `None` when no profile is selected.
    pub fn resolve_profile_name(&self, cli: Option<&str>) -> Option<String> {
        if let Some(n) = cli {
            return Some(n.to_string());
        }
        match std::env::var("SHOKA_PROFILE") {
            Ok(n) if !n.is_empty() => Some(n),
            _ => self.global.default_profile.clone(),
        }
    }

    /// Collapse global + active profile into a [`ResolvedConfig`].
    ///
    /// Errors out if the resolved profile name (CLI / env / default)
    /// names a profile that isn't defined, or if `root` is unset in
    /// both global and the active profile.
    pub fn resolve(self, cli_profile: Option<&str>) -> Result<ResolvedConfig> {
        let active_profile = self.resolve_profile_name(cli_profile);

        let prof = if let Some(name) = active_profile.as_deref() {
            Some(
                self.profiles
                    .get(name)
                    .cloned()
                    .ok_or_else(|| anyhow!("profile `{name}` is not defined in config"))?,
            )
        } else {
            None
        };
        let prof = prof.unwrap_or_default();
        let g = &self.global;

        let root_str = prof.root.as_deref().or(g.root.as_deref()).ok_or_else(|| {
            anyhow!("`global.root` (or `profiles.<name>.root`) must be set in config")
        })?;
        let root = expand_home(root_str);

        Ok(ResolvedConfig {
            root,
            layout: prof.layout.clone().unwrap_or_else(|| g.layout.clone()),
            default_vcs: prof.default_vcs.unwrap_or(g.default_vcs),
            default_protocol: prof.default_protocol.unwrap_or(g.default_protocol),
            default_host: prof
                .default_host
                .clone()
                .unwrap_or_else(|| g.default_host.clone()),
            exec_concurrency: prof.exec_concurrency.unwrap_or(g.exec_concurrency),
            ui: g.ui.clone(),
            shell: g.shell.clone(),
            hosts: self.hosts.clone(),
            git_config: prof.git_config.clone(),
            active_profile,
            raw: self,
        })
    }
}

/// Expand a leading `~` in a path-like string to the user's home dir.
///
/// Tera-side users should prefer `{{ home() }}` (the std-helpers
/// function), but a literal `~/...` is a common enough mistake to be
/// worth handling silently.
fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix('~') {
        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
            let rest = rest.trim_start_matches(['/', '\\']);
            if rest.is_empty() {
                return home;
            }
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Build a [`ShokaPaths`] whose config_dir points at the given
    /// directory. Lets tests stage a config tree without polluting the
    /// real `$XDG_CONFIG_HOME/shoka`.
    fn paths_at(dir: &Path) -> ShokaPaths {
        ShokaPaths::resolve(Some(&dir.join("config.toml")))
            .expect("ShokaPaths::resolve with override")
    }

    #[test]
    fn missing_config_dir_yields_defaults() {
        let cfg = ShokaConfig::load(&paths_at(Path::new(
            "/definitely/does/not/exist/shoka-cfg-test",
        )))
        .expect("load tolerates missing dir");
        assert!(cfg.global.root.is_none());
        assert_eq!(cfg.global.default_vcs, VcsDefault::Auto);
        assert_eq!(cfg.global.exec_concurrency, 8);
    }

    #[test]
    fn empty_config_dir_yields_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn single_file_load_parses_root_and_default_host() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/tmp/repos"
default_host = "gitlab.com"
exec_concurrency = 16
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.global.root.as_deref(), Some("/tmp/repos"));
        assert_eq!(cfg.global.default_host, "gitlab.com");
        assert_eq!(cfg.global.exec_concurrency, 16);
    }

    #[test]
    fn layered_local_override_wins() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/base/repos"
default_host = "github.com"
exec_concurrency = 4
"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("config.local.toml"),
            r#"
[global]
exec_concurrency = 32
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        // local wins on overlap …
        assert_eq!(cfg.global.exec_concurrency, 32);
        // … but does not clobber unrelated keys.
        assert_eq!(cfg.global.root.as_deref(), Some("/base/repos"));
        assert_eq!(cfg.global.default_host, "github.com");
    }

    #[test]
    fn vars_self_reference_resolves() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[vars]
base = "/data"
repo_root = "{{ vars.base }}/repos"

[global]
root = "{{ vars.repo_root }}"
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.global.root.as_deref(), Some("/data/repos"));
    }

    #[test]
    fn host_override_is_visible_after_resolve() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/r"
default_protocol = "https"

[hosts."github.com"]
protocol = "ssh"
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        let resolved = cfg.resolve(None).expect("resolve");
        assert_eq!(resolved.default_protocol, Protocol::Https);
        let gh = resolved
            .hosts
            .get("github.com")
            .expect("github.com host config present");
        assert_eq!(gh.protocol, Some(Protocol::Ssh));
    }

    #[test]
    fn profile_overrides_global_root() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/personal"
default_profile = "work"

[profiles.work]
root = "/work"
exec_concurrency = 32
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        let resolved = cfg.resolve(None).expect("resolve");
        assert_eq!(resolved.active_profile.as_deref(), Some("work"));
        assert_eq!(resolved.root, PathBuf::from("/work"));
        assert_eq!(resolved.exec_concurrency, 32);
    }

    #[test]
    fn cli_profile_overrides_default_profile() {
        let cfg = ShokaConfig {
            global: GlobalConfig {
                default_profile: Some("personal".into()),
                ..Default::default()
            },
            profiles: BTreeMap::from([
                ("personal".into(), ProfileConfig::default()),
                ("work".into(), ProfileConfig::default()),
            ]),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_profile_name(Some("work")), Some("work".into()));
        assert_eq!(cfg.resolve_profile_name(None), Some("personal".into()));
    }

    #[test]
    fn resolve_errors_when_profile_is_undefined() {
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = cfg.resolve(Some("ghost")).unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "error should mention the missing profile name: {err}"
        );
    }

    #[test]
    fn resolve_errors_when_root_is_unset() {
        let cfg = ShokaConfig::default();
        let err = cfg.resolve(None).unwrap_err();
        assert!(
            err.to_string().contains("root"),
            "error should mention the missing root: {err}"
        );
    }

    #[test]
    fn expand_home_handles_leading_tilde() {
        let p = expand_home("~/src/repos");
        assert!(
            p.is_absolute(),
            "expected ~ expansion to produce an absolute path, got {p:?}"
        );
        assert!(p.to_string_lossy().contains("src"));
    }

    #[test]
    fn expand_home_passes_through_absolute_paths() {
        assert_eq!(expand_home("/etc/shoka"), PathBuf::from("/etc/shoka"));
    }
}
