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
        // First-run bootstrap: if the config file is missing entirely,
        // drop a starter at the expected path so shoka is usable
        // immediately after install. Subsequent runs find the file
        // and skip this step. Emitted at info level so users can see
        // it under the default `warn,shoka=info` filter.
        let explicit = paths.config_file();
        if !explicit.exists() {
            write_starter(paths).with_context(|| {
                format!("auto-creating starter config at {}", explicit.display())
            })?;
            tracing::info!(
                target: "shoka",
                "wrote starter config to {} (first-run bootstrap)",
                explicit.display()
            );
        }

        let dir = paths.config_dir();
        let mut files: Vec<PathBuf> = if dir.exists() {
            discover_config_files(dir)
                .with_context(|| format!("discovering config files in {}", dir.display()))?
        } else {
            Vec::new()
        };

        // Ensure the explicit `--config` / `$SHOKA_CONFIG` target is loaded
        // even if its filename doesn't match the canonical `config.toml` /
        // `config.*.toml` discovery pattern. Prepend so it acts as the base
        // and any sibling `config.*.toml` overlays still win in their usual
        // alphabetical order.
        //
        // The canonicalize-based dedup check uses an explicit
        // `(Some, Some)` match — comparing `Option::None` to `None`
        // would otherwise wrongly treat two paths that *both* failed
        // to canonicalize as equal, causing a missed prepend.
        if explicit.exists() {
            let explicit_can = explicit.canonicalize().ok();
            let already_listed = files.iter().any(|p| {
                matches!(
                    (explicit_can.as_ref(), p.canonicalize().ok().as_ref()),
                    (Some(a), Some(b)) if a == b
                )
            });
            if !already_listed {
                files.insert(0, explicit.to_path_buf());
            }
        }

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

/// Starter `config.toml` content written by `shoka doctor --init`.
///
/// Designed to be a usable baseline as soon as the user edits the
/// `root = ...` line: everything else is commented out with examples
/// of the available knobs and a pointer to the project docs.
pub const STARTER_CONFIG: &str = r#"# shoka config — see https://github.com/yukimemi/shoka for the full schema.
#
# Files are layered: this `config.toml` is the base, `config.*.toml`
# siblings merge on top in alphabetical order, and `config.local.toml`
# always wins. Use `--config PATH` or $SHOKA_CONFIG to point at a
# specific file.

# [vars]
# # User-defined variables. Standard helpers home() / env(name=...) /
# # is_windows() etc. are pre-registered. Reference vars in later TOML
# # values via Tera (see docs above for syntax); the example below
# # uses home().
# work_root = "{{ home() }}/work"

[global]
# Filesystem root under which repositories are cloned. Required.
root = "~/src"

# Path layout template. Variables injected at clone time:
# root, host, owner, name, profile, vcs, protocol. Default produces
# a flat <root>/<host>/<owner>/<name> tree.
# (To set, write a Tera template string here — see the project docs.)

# default_vcs = "auto"          # "auto" | "git" | "jj"
# default_protocol = "https"    # "https" | "ssh"
# default_host = "github.com"
# default_profile = "personal"
# exec_concurrency = 8

# [global.ui]
# status_cache_ttl_secs = 60
# tui_refresh_ms = 250

# [global.shell]
# cd_command_name = "s"

# Per-host overrides — merge over the global values.
# [hosts."github.com"]
# protocol = "ssh"

# Profiles — `--profile NAME` / $SHOKA_PROFILE / global.default_profile
# selects one. Profile fields override [global] when set; absent fields
# fall through to the global value.
# [profiles.personal]
# root = "~/src"
#
# [profiles.work]
# root = "~/work"
# default_host = "github.com"
#
# [profiles.work.git_config]
# "user.email" = "work@example.com"
"#;

/// Write [`STARTER_CONFIG`] to `paths.config_file()`, creating the
/// config directory if needed.
///
/// Errors out (without writing) if the target file already exists —
/// the caller is responsible for asking the user to confirm an
/// overwrite if that's desired.
pub fn write_starter(paths: &ShokaPaths) -> Result<()> {
    let target = paths.config_file();
    if target.exists() {
        anyhow::bail!(
            "config file already exists at {}; not overwriting",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::write(target, STARTER_CONFIG)
        .with_context(|| format!("writing starter config to {}", target.display()))?;
    Ok(())
}

/// Expand a leading `~` in a path-like string to the user's home
/// dir, then absolutise the result against the process working
/// directory so callers always see a stable absolute path.
///
/// Tilde handling: only `~` on its own and `~/…` / `~\…` are
/// expanded. `~foo` (which in POSIX shells refers to user `foo`'s
/// home dir) is left untouched rather than misread as `<home>/foo`.
/// Tera-side users should still prefer `{{ home() }}`; this helper
/// exists for the common literal `~/...` case.
///
/// Absolutisation uses [`std::path::absolute`], which does *not*
/// require the path to exist (so this works during first-run setup
/// before `root` has been mkdir'd). On the rare failure case (e.g.
/// no current directory), the tilde-expanded path is returned
/// as-is.
fn expand_home(s: &str) -> PathBuf {
    let expanded = expand_tilde(s);
    std::path::absolute(&expanded).unwrap_or(expanded)
}

fn expand_tilde(s: &str) -> PathBuf {
    let is_bare = s == "~";
    let is_rooted = s.starts_with("~/") || s.starts_with("~\\");
    if is_bare || is_rooted {
        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
            if is_bare {
                return home;
            }
            // s[2..] skips the `~/` or `~\`, leaving a relative remainder.
            return home.join(&s[2..]);
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
    fn load_auto_writes_starter_when_missing() {
        // First-run bootstrap: load() finds no config file, writes a
        // starter at the canonical path, and proceeds to load it. End
        // state should be the starter's view of the world (root set
        // to `~/src`, no profiles defined yet).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("config.toml");
        assert!(!target.exists(), "preconditions: target must not exist yet");
        let cfg =
            ShokaConfig::load(&paths_at(tmp.path())).expect("load should auto-create starter");
        assert!(
            target.exists(),
            "load must have written the starter at {target:?}"
        );
        assert_eq!(cfg.global.root.as_deref(), Some("~/src"));
        assert!(
            cfg.profiles.is_empty(),
            "starter does not define any profiles"
        );
    }

    #[test]
    fn load_does_not_overwrite_existing_config() {
        // Subsequent runs: load() must NOT clobber an existing config
        // (the user-edited file). It only auto-creates when missing.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/user/edited"
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.global.root.as_deref(), Some("/user/edited"));
        // File body should still be the user's, not the starter.
        let body = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        assert!(body.contains("/user/edited"));
        assert!(
            !body.contains("shoka config — see"),
            "starter header should not have been written over user content"
        );
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
        // expand_home absolutises the result, so on Windows `/work`
        // becomes the equivalent of the current drive's `\work`. Just
        // verify shape: absolute + ends with the intended segment.
        assert!(
            resolved.root.is_absolute(),
            "resolved root must be absolute, got {:?}",
            resolved.root
        );
        assert!(
            resolved.root.ends_with("work"),
            "resolved root should end with `work`, got {:?}",
            resolved.root
        );
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
    fn expand_home_keeps_already_absolute_inputs_absolute() {
        // std::env::temp_dir is always absolute on every platform, so
        // this is a portable stand-in for the previous /etc literal.
        let abs = std::env::temp_dir();
        let expanded = expand_home(abs.to_str().unwrap());
        assert!(
            expanded.is_absolute(),
            "absolute input must stay absolute, got {expanded:?}"
        );
    }

    #[test]
    fn expand_home_makes_relative_input_absolute() {
        // Gemini review on PR #9: resolved `root` should be absolute
        // regardless of caller cwd. This guards the contract.
        let expanded = expand_home("relative/repos");
        assert!(
            expanded.is_absolute(),
            "relative input should be absolutised, got {expanded:?}"
        );
        assert!(
            expanded.ends_with("relative/repos") || expanded.ends_with("relative\\repos"),
            "absolutised path should still end with the original tail, got {expanded:?}"
        );
    }

    #[test]
    fn expand_home_bare_tilde_yields_home() {
        let p = expand_home("~");
        assert!(p.is_absolute(), "bare ~ should expand to home, got {p:?}");
    }

    #[test]
    fn expand_home_does_not_touch_tilde_user_syntax() {
        // POSIX `~<user>` is a deliberate non-target: silently treating
        // `~foo` as `$HOME/foo` would be wrong. The tilde is preserved
        // in the path, though absolutisation still kicks in (anchoring
        // to cwd) to keep the "always absolute" contract.
        let p1 = expand_home("~foo");
        assert!(p1.is_absolute(), "result should be absolute, got {p1:?}");
        assert!(
            p1.to_string_lossy().contains("~foo"),
            "~foo literal should be preserved in the result, got {p1:?}"
        );
        let p2 = expand_home("~bar/baz");
        assert!(p2.is_absolute(), "result should be absolute, got {p2:?}");
        assert!(
            p2.to_string_lossy().contains("~bar"),
            "~bar literal should be preserved, got {p2:?}"
        );
    }

    #[test]
    fn override_with_custom_filename_is_loaded() {
        // `--config custom.toml` (whose name doesn't match the
        // `config.*.toml` discovery pattern) must still be loaded.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("custom.toml"),
            r#"
[global]
root = "/from/custom"
default_host = "gitlab.com"
"#,
        )
        .unwrap();
        let paths = ShokaPaths::resolve(Some(&tmp.path().join("custom.toml")))
            .expect("ShokaPaths::resolve");
        let cfg = ShokaConfig::load(&paths).expect("load");
        assert_eq!(cfg.global.root.as_deref(), Some("/from/custom"));
        assert_eq!(cfg.global.default_host, "gitlab.com");
    }

    #[test]
    fn write_starter_creates_file_when_missing() {
        let tmp = TempDir::new().unwrap();
        // Point config_file at a yet-to-exist subdir to also exercise
        // the `create_dir_all` path.
        let target = tmp.path().join("nested").join("config.toml");
        let paths = ShokaPaths::resolve(Some(&target)).unwrap();
        assert!(!target.exists());

        write_starter(&paths).expect("write_starter on fresh path");
        assert!(target.exists(), "starter file should exist at {target:?}");

        let body = fs::read_to_string(&target).unwrap();
        assert!(
            body.contains("[global]"),
            "starter should contain [global] section, got: {body}"
        );
        assert!(
            body.contains("root ="),
            "starter should set root = ..., got: {body}"
        );
    }

    #[test]
    fn write_starter_refuses_to_overwrite() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("config.toml");
        fs::write(&target, "# existing user config\n").unwrap();
        let paths = ShokaPaths::resolve(Some(&target)).unwrap();

        let err = write_starter(&paths).expect_err("write_starter must refuse overwrite");
        assert!(
            err.to_string().contains("already exists"),
            "error should mention existing file: {err}"
        );

        // Original content must be intact.
        let body = fs::read_to_string(&target).unwrap();
        assert_eq!(body, "# existing user config\n");
    }

    #[test]
    fn starter_content_is_loadable() {
        // Round-trip: write starter, then load it through the regular
        // pipeline. This guards against the starter regressing into
        // syntactically-invalid TOML or unrenderable Tera.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("config.toml");
        let paths = ShokaPaths::resolve(Some(&target)).unwrap();
        write_starter(&paths).expect("write");
        let cfg = ShokaConfig::load(&paths).expect("load starter");
        // The starter sets root = "~/src" (uncommented); everything else
        // is commented out. So we expect root set and defaults elsewhere.
        assert_eq!(cfg.global.root.as_deref(), Some("~/src"));
        assert_eq!(cfg.global.default_vcs, VcsDefault::Auto);
        assert_eq!(cfg.global.default_host, "github.com");
    }

    #[test]
    fn override_does_not_duplicate_when_file_matches_canonical_name() {
        // When `--config config.toml` is passed (same name as the
        // canonical discovery entry), it should still load exactly
        // once — duplicate prepend would re-run Tera and double-merge.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/once"
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.global.root.as_deref(), Some("/once"));
    }
}
