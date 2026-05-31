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
//! [`ResolvedConfig`] for the active profile: profile fields override
//! the corresponding `[global]` fields when set, with anything unset
//! falling through to the global value.
//!
//! Profile resolution order: explicit `--profile <name>` CLI flag,
//! `$SHOKA_PROFILE` environment variable, `global.default_profile`. If
//! none matches, profile-less mode is used (the global values are taken
//! verbatim and no profile-only fields like `git_config` apply).
//!
//! **Clone destination routing** runs on top of the resolved config:
//! [`ResolvedConfig::resolve_target`] takes a `host/owner/name` spec
//! and returns a [`CloneTarget`]. Precedence is
//! `profile.root` > first matching `[[routes]]` entry > `[global].root`
//! (so an explicit profile always wins over auto-routing). See [`Route`]
//! / [`compile_pattern`] for the pattern syntax (`host:<host>`,
//! `host:<host>/<owner>`, or `/<regex>/`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
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
    /// Clone-destination routing rules. Evaluated top-to-bottom at
    /// clone time; the first matching entry wins. See [`Route`] and
    /// [`compile_pattern`] for pattern syntax.
    #[serde(default)]
    pub routes: Vec<Route>,
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
    pub cache: CacheConfig,
    pub new: NewConfig,
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
            cache: CacheConfig::default(),
            new: NewConfig::default(),
        }
    }
}

/// `[global.new]` — defaults for the `shoka new` scaffolding command.
///
/// Kept under `[global]` (not profile-overridable) because the
/// scaffolding preset is a workflow-wide choice, not a per-workspace
/// one — the same `pj-presets` template applies regardless of which
/// `root` a profile points at.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NewConfig {
    /// kata preset spec applied after create + clone, e.g.
    /// `github.com/yukimemi/pj-presets:rust-cli`. When unset *and* no
    /// `--preset` is passed, `shoka new` skips the kata step and just
    /// creates + clones. The `--preset` CLI flag overrides this for a
    /// single invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
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
    /// Owners considered "yours" (e.g. your GitHub username plus any
    /// orgs you belong to). The TUI starts in mine-only mode when this
    /// is non-empty and offers `m` to toggle back to the full shelf;
    /// empty = no default filter and the toggle is hidden.
    pub own_owners: Vec<String>,
    /// Number of candidate rows the `shoka cd` fuzzy picker shows at
    /// once (inquire's "page size"). Larger values fill more of the
    /// terminal, so fewer scrolls to find a repo. Floored at 1 by the
    /// loader — inquire panics on a zero page size.
    pub cd_page_size: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            status_cache_ttl_secs: 60,
            tui_refresh_ms: 250,
            own_owners: Vec::new(),
            // inquire's own default is 7, which barely fills a modern
            // terminal. 15 is a roomier baseline that still fits an
            // 80x24 window with the prompt + help line.
            cd_page_size: 15,
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

/// `[global.cache]` — controls per-repo cache refresh behaviour.
///
/// Refresh runs explicitly via `shoka cache refresh` and (once the
/// background-refresh PR lands) implicitly at the tail of other
/// subcommands. `background_refresh = false` disables the implicit
/// path entirely for users who don't want their shell sessions
/// spawning detached refresh processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Spawn a detached background refresh after each command. When
    /// `false`, only `shoka cache refresh` updates the cache.
    pub background_refresh: bool,

    /// Skip the per-repo refresh if `last_refreshed` is within this
    /// many seconds of "now". `shoka cache refresh --force` bypasses
    /// the check.
    pub refresh_threshold_secs: u64,

    /// Cap on concurrent per-repo refresh tasks. Floored at 1 by
    /// the loader (same reasoning as `exec_concurrency`).
    pub parallel_repos: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            background_refresh: true,
            refresh_threshold_secs: 60,
            parallel_repos: 8,
        }
    }
}

/// A `[[routes]]` entry. Matches a repository spec (`host/owner/name`)
/// against [`Route::pattern`] and, on hit, overrides the corresponding
/// `[global]` fields when they're `Some`.
///
/// Pattern syntax (see [`compile_pattern`] for the parser):
///
/// - `host:<host>` — host exact match (also matches `<host>/...` prefix).
/// - `host:<host>/<owner>` — host + owner prefix.
/// - `/<regex>/` — full regex, matched against the spec string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vcs: Option<VcsDefault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_protocol: Option<Protocol>,
}

/// A [`Route`] with its [`pattern`](Route::pattern) parsed and
/// [`root`](Route::root) expanded once at [`ShokaConfig::resolve`]
/// time so the per-clone matcher loop doesn't pay a re-compile or
/// re-expansion cost. Multiple `clone` calls in one process (think
/// `shoka exec` or the upcoming TUI) iterate routes hot.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub raw: Route,
    pub pattern: PatternKind,
    /// `expand_home`'d form of [`Route::root`], pre-computed so the
    /// matcher hot path doesn't re-tilde-expand and re-absolutise
    /// the same string every clone.
    pub resolved_root: Option<PathBuf>,
}

/// Compiled form of a [`Route::pattern`].
///
/// Both `host:` variants store a pre-joined string so [`matches`] is
/// allocation-free in the hot path. The shared "prefix-with-`/`-or-equal"
/// matcher is hoisted into [`prefix_or_exact_match`] so the two
/// variants share a single byte-level boundary check.
///
/// [`matches`]: PatternKind::matches
#[derive(Debug, Clone)]
pub enum PatternKind {
    /// `host:<host>` — matches spec when it equals `<host>` or starts
    /// with `<host>/`.
    HostExact(String),
    /// `host:<host>/<owner>` — stored pre-joined as `<host>/<owner>`
    /// so matching is allocation-free.
    HostOwner(String),
    /// `/<regex>/` — full regex match against the spec.
    Regex(Regex),
}

impl PatternKind {
    /// Test the pattern against a `host/owner/name` (or `host/owner`,
    /// or just `host`) spec. Hot path: no allocations.
    pub fn matches(&self, spec: &str) -> bool {
        match self {
            PatternKind::HostExact(h) | PatternKind::HostOwner(h) => prefix_or_exact_match(spec, h),
            PatternKind::Regex(re) => re.is_match(spec),
        }
    }
}

/// Returns `true` when `spec` either equals `prefix` or starts with
/// `prefix` followed by a `/` segment boundary. Byte-indexed so it
/// allocates nothing. Single `starts_with` upfront keeps the common
/// "no match" case fast (it bails immediately on prefix mismatch).
fn prefix_or_exact_match(spec: &str, prefix: &str) -> bool {
    spec.starts_with(prefix)
        && (spec.len() == prefix.len() || spec.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// Parse a [`Route::pattern`] string into a [`PatternKind`].
///
/// Returns a descriptive error for unrecognised forms so a typo'd
/// pattern surfaces at `config load` time rather than the first
/// clone attempt.
pub fn compile_pattern(s: &str) -> Result<PatternKind> {
    if let Some(rest) = s.strip_prefix("host:") {
        if rest.is_empty() {
            bail!("`host:` pattern requires a host name: `{s}`");
        }
        if let Some((host, owner)) = rest.split_once('/') {
            if host.is_empty() || owner.is_empty() {
                bail!("`host:<host>/<owner>` requires both segments: `{s}`");
            }
            if owner.contains('/') {
                bail!(
                    "`host:` shortcut supports `host:<host>` or `host:<host>/<owner>`; \
                     for deeper matches use `/<regex>/`: `{s}`"
                );
            }
            // Pre-join so the matcher's hot path stays allocation-free.
            return Ok(PatternKind::HostOwner(format!("{host}/{owner}")));
        }
        return Ok(PatternKind::HostExact(rest.to_string()));
    }
    if s.len() >= 2 && s.starts_with('/') && s.ends_with('/') {
        let body = &s[1..s.len() - 1];
        if body.is_empty() {
            // `//` would compile to a regex matching every spec — a
            // very effective unintentional catch-all. Force the user
            // to be explicit (write the actual catch-all `/.*/` or
            // drop the route entirely).
            bail!(
                "empty regex pattern `//` would match every repo spec; \
                 write `/.*/` if a catch-all is intended, or remove the route"
            );
        }
        let re = Regex::new(body).with_context(|| format!("compiling regex pattern `{s}`"))?;
        return Ok(PatternKind::Regex(re));
    }
    bail!("unknown route pattern `{s}` — use `host:<host>`, `host:<host>/<owner>`, or `/<regex>/`")
}

/// Resolved clone destination for a given repo spec — the output of
/// [`ResolvedConfig::resolve_target`].
#[derive(Debug, Clone)]
pub struct CloneTarget {
    pub root: PathBuf,
    pub layout: String,
    pub default_vcs: VcsDefault,
    pub default_protocol: Protocol,
    /// Index into [`ResolvedConfig::routes`] that decided the target,
    /// or `None` when either the active profile's `root` won or the
    /// `[global]` fallback applied. Useful for `shoka where`-style
    /// debug subcommands and tracing output.
    pub matched_route: Option<usize>,
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
    /// Resolved `[global.cache]`. `parallel_repos` is floored at 1
    /// here so the upcoming JoinSet-based refresh can't be handed a
    /// zero-sized pool (which would deadlock).
    pub cache: CacheConfig,
    /// Routes compiled at `resolve()` time so per-clone matching
    /// doesn't pay a regex-build cost. Order is preserved from the
    /// source config (first match wins).
    pub routes: Vec<CompiledRoute>,
    pub git_config: BTreeMap<String, String>,
    pub active_profile: Option<String>,
    /// `true` when the active profile supplied its own `root`. Drives
    /// the `profile.root > routes > global.root` precedence in
    /// [`ResolvedConfig::resolve_target`] — with this flag set,
    /// routes don't get a chance to override (the user picked the
    /// profile, so honour it).
    pub profile_provided_root: bool,
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
        // When the user names a non-default file via --config, also drop
        // any `config.toml` that the auto-discovery happened to pick up
        // in the same directory. teravars::load_merged merges in order
        // with later files overriding earlier ones, so leaving the
        // default behind would silently overwrite the user's named
        // config with the un-asked-for defaults.
        let canonical_default_name = std::ffi::OsStr::new("config.toml");
        let is_user_override = explicit.file_name() != Some(canonical_default_name);
        if is_user_override {
            files.retain(|p| p.file_name() != Some(canonical_default_name));
        }

        // `is_ok_and` keeps the dedup conservative: when canonicalize
        // fails for either side, treat as "not equal" so the explicit
        // path is prepended rather than silently dropped.
        if explicit.exists() {
            let already_listed = explicit.canonicalize().is_ok_and(|explicit_can| {
                files
                    .iter()
                    .any(|p| p.canonicalize().is_ok_and(|p_can| p_can == explicit_can))
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

        let profile_provided_root = prof.root.is_some();
        let root_str = prof.root.as_deref().or(g.root.as_deref()).ok_or_else(|| {
            anyhow!("`global.root` (or `profiles.<name>.root`) must be set in config")
        })?;
        let root = expand_home(root_str);

        // Compile all route patterns up-front so a typo'd pattern
        // surfaces here (at config-load time) rather than on the
        // first clone attempt. While we're here, pre-expand each
        // route's `root` so the matcher hot path doesn't re-tilde +
        // re-absolutise the same string every clone.
        let routes: Vec<CompiledRoute> = self
            .routes
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let pattern = compile_pattern(&r.pattern)
                    .with_context(|| format!("compiling routes[{idx}] pattern"))?;
                let resolved_root = r.root.as_deref().map(expand_home);
                Ok::<_, anyhow::Error>(CompiledRoute {
                    raw: r.clone(),
                    pattern,
                    resolved_root,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ResolvedConfig {
            root,
            layout: prof.layout.clone().unwrap_or_else(|| g.layout.clone()),
            default_vcs: prof.default_vcs.unwrap_or(g.default_vcs),
            default_protocol: prof.default_protocol.unwrap_or(g.default_protocol),
            default_host: prof
                .default_host
                .clone()
                .unwrap_or_else(|| g.default_host.clone()),
            // Floor at 1 — a user-provided `exec_concurrency = 0`
            // would otherwise feed straight into thread-pool builders
            // and tokio::JoinSet sizing, where 0 typically panics or
            // deadlocks. 1 is the smallest safe sequential value.
            exec_concurrency: prof.exec_concurrency.unwrap_or(g.exec_concurrency).max(1),
            // Floor cd_page_size at 1 — inquire panics on a zero page
            // size, same defensive flooring as exec_concurrency.
            ui: UiConfig {
                cd_page_size: g.ui.cd_page_size.max(1),
                ..g.ui.clone()
            },
            shell: g.shell.clone(),
            // Mirror the exec_concurrency reasoning: a configured
            // `parallel_repos = 0` would otherwise hand a zero-sized
            // pool to the background-refresh JoinSet. Floor at 1.
            cache: CacheConfig {
                parallel_repos: g.cache.parallel_repos.max(1),
                ..g.cache.clone()
            },
            routes,
            git_config: prof.git_config.clone(),
            active_profile,
            profile_provided_root,
            raw: self,
        })
    }
}

impl ResolvedConfig {
    /// Resolve the clone destination for a `host/owner/name` spec.
    ///
    /// The clone-routing rule is "profile owns this clone fully, or
    /// not at all":
    ///
    /// 1. **Active profile's `root` is set** → profile claims the
    ///    clone. All profile-overlaid values apply (`self.root`,
    ///    `self.layout`, `self.default_vcs`, `self.default_protocol`)
    ///    and routes are skipped entirely. The user picked the
    ///    profile explicitly (CLI / env / default), so that intent
    ///    wins over auto-routing.
    /// 2. **Active profile didn't pin a `root`** → profile bows out
    ///    of clone routing. Routes are evaluated against the raw
    ///    `[global]` baseline; an unset route field falls through to
    ///    the corresponding `[global]` value (NOT the profile's
    ///    value — those don't bleed into routes when the profile
    ///    isn't claiming the clone). First matching route wins.
    /// 3. **No route matches** → raw `[global]` values, same baseline
    ///    as the route branch would have used. Consistent with #2:
    ///    once profile.root isn't pinned, profile values stay out of
    ///    clone routing.
    ///
    /// Profile fields like `git_config` / `exec_concurrency` are
    /// unaffected — those aren't clone-routing concerns and continue
    /// to apply via [`ResolvedConfig`] for the lifetime of the
    /// session.
    pub fn resolve_target(&self, spec: &str) -> CloneTarget {
        if self.profile_provided_root {
            // Case 1: profile claims the clone.
            return CloneTarget {
                root: self.root.clone(),
                layout: self.layout.clone(),
                default_vcs: self.default_vcs,
                default_protocol: self.default_protocol,
                matched_route: None,
            };
        }
        // Profile didn't pin root → use raw [global] as the baseline
        // so profile-set fields like `default_vcs` don't silently
        // bleed into route results. `self.root` here equals
        // `expand_home(global.root)` because !profile_provided_root.
        let g = &self.raw.global;
        for (idx, route) in self.routes.iter().enumerate() {
            if route.pattern.matches(spec) {
                return CloneTarget {
                    // Use the pre-expanded path stored on the
                    // CompiledRoute so the hot path skips
                    // expand_home (which absolutises against cwd and
                    // does tilde substitution).
                    root: route
                        .resolved_root
                        .clone()
                        .unwrap_or_else(|| self.root.clone()),
                    layout: route.raw.layout.clone().unwrap_or_else(|| g.layout.clone()),
                    default_vcs: route.raw.default_vcs.unwrap_or(g.default_vcs),
                    default_protocol: route.raw.default_protocol.unwrap_or(g.default_protocol),
                    matched_route: Some(idx),
                };
            }
        }
        // Case 3: no route — pure [global] baseline.
        CloneTarget {
            root: self.root.clone(),
            layout: g.layout.clone(),
            default_vcs: g.default_vcs,
            default_protocol: g.default_protocol,
            matched_route: None,
        }
    }

    /// Render the on-disk clone path for `repo` by feeding the
    /// resolved [`CloneTarget`]'s `layout` template through Tera with
    /// the per-repo context (`root` / `host` / `owner` / `name` /
    /// `profile` / `vcs` / `protocol`) populated.
    ///
    /// Lives here (rather than on `Repo`) because the resolution
    /// owns the destination context — same repo can land in different
    /// paths under different routes / profiles, and the layout
    /// template is itself a config-level concern.
    ///
    /// The caller owns the [`teravars::Engine`] so multi-repo loops
    /// (e.g. `shoka list`'s shelf walk) construct it once and reuse
    /// it across every repo rather than paying the Tera setup cost
    /// per call. One-off callers can use [`clone_path_for_one`] for
    /// a single-shot engine.
    ///
    /// [`clone_path_for_one`]: Self::clone_path_for_one
    pub fn clone_path_for(
        &self,
        repo: &crate::state::Repo,
        engine: &mut teravars::Engine,
    ) -> Result<PathBuf> {
        use teravars::Context;

        // Per-repo `path` override wins over everything. Set by
        // `shoka import` for local-only / jj-only repos that the
        // user wants left in place (we record the absolute path
        // and refuse to second-guess it via layout). Layout / route
        // / profile resolution is skipped entirely — those concepts
        // don't apply when the repo's location was given to us
        // directly.
        if let Some(p) = &repo.path {
            return Ok(p.clone());
        }

        let spec = repo.slug();
        let target = self.resolve_target(&spec);

        // Per-repo `vcs` override on the Repo itself wins over the
        // route / global default — that's the contract `set --vcs`
        // promises.
        let vcs = repo.vcs.unwrap_or(target.default_vcs);

        let mut ctx = Context::new();
        ctx.insert("root", &target.root.to_string_lossy().to_string());
        ctx.insert("host", &repo.host);
        ctx.insert("owner", &repo.owner);
        ctx.insert("name", &repo.name);
        ctx.insert(
            "profile",
            &self.active_profile.as_deref().unwrap_or("default"),
        );
        // Avoid `format!("{:?}").to_lowercase()` per render: stable
        // wire names are part of the schema, so a match on the enum
        // gives us a &'static str at zero alloc cost.
        ctx.insert("vcs", vcs_str(vcs));
        ctx.insert("protocol", protocol_str(target.default_protocol));

        let rendered = engine
            .render(&target.layout, &ctx)
            .with_context(|| format!("rendering layout `{}` for {spec}", target.layout))?;

        Ok(PathBuf::from(rendered))
    }

    /// Convenience wrapper for callers that resolve exactly one
    /// repo path — builds a single-use [`Engine`] under the hood.
    /// Multi-repo callers should use [`clone_path_for`](Self::clone_path_for)
    /// with their own reusable engine.
    pub fn clone_path_for_one(&self, repo: &crate::state::Repo) -> Result<PathBuf> {
        let mut engine = teravars::Engine::new();
        self.clone_path_for(repo, &mut engine)
    }
}

fn vcs_str(v: VcsDefault) -> &'static str {
    match v {
        VcsDefault::Auto => "auto",
        VcsDefault::Git => "git",
        VcsDefault::Jj => "jj",
    }
}

fn protocol_str(p: Protocol) -> &'static str {
    match p {
        Protocol::Https => "https",
        Protocol::Ssh => "ssh",
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
# # Owners treated as "yours" in the TUI. When non-empty the dashboard
# # launches in mine-only mode and `m` toggles between mine and all.
# own_owners = ["yukimemi"]
# # Rows the `shoka cd` fuzzy picker shows at once (default 15).
# cd_page_size = 15

# [global.shell]
# cd_command_name = "s"

# [global.new]
# # kata preset spec `shoka new` applies after creating + cloning a
# # repo. Overridable per-run with `--preset`; omit (and pass no
# # --preset) to skip the kata step entirely.
# preset = "github.com/yukimemi/pj-presets:rust-cli"

# Routes — evaluated top-to-bottom at clone time; first hit wins.
# Pattern syntax:
#   host:<host>             - host exact (or `<host>/...` prefix)
#   host:<host>/<owner>     - host + owner prefix
#   /<regex>/               - full regex (Rust regex crate)
# A matching route's `root` / `layout` / `default_vcs` / `default_protocol`
# override the corresponding [global] fields. Absent fields fall through.
# Precedence: profile.root (when set) > routes > [global].root.
#
# [[routes]]
# pattern = "host:github.com/mycompany"
# root    = "~/src/work"
#
# [[routes]]
# pattern = "host:gitlab.com"
# root    = "~/src/gitlab"
# default_protocol = "ssh"

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

    // --- routes ---------------------------------------------------

    #[test]
    fn compile_pattern_host_exact() {
        let p = compile_pattern("host:github.com").unwrap();
        assert!(p.matches("github.com"));
        assert!(p.matches("github.com/foo"));
        assert!(p.matches("github.com/foo/bar"));
        assert!(!p.matches("gitlab.com"));
        assert!(!p.matches("github.com.evil"));
    }

    #[test]
    fn compile_pattern_host_owner() {
        let p = compile_pattern("host:github.com/yukimemi").unwrap();
        assert!(p.matches("github.com/yukimemi"));
        assert!(p.matches("github.com/yukimemi/shoka"));
        assert!(!p.matches("github.com/other"));
        assert!(!p.matches("github.com/yukimemi-other"));
        assert!(!p.matches("github.com"));
    }

    #[test]
    fn compile_pattern_regex() {
        let p = compile_pattern(r"/^github\.com/foo-.*$/").unwrap();
        assert!(p.matches("github.com/foo-bar"));
        assert!(p.matches("github.com/foo-bar/baz"));
        assert!(!p.matches("github.com/bar"));
    }

    #[test]
    fn compile_pattern_rejects_garbage() {
        for bad in [
            "github.com",       // missing prefix
            "host:",            // empty host
            "host:/",           // empty parts
            "host:foo/",        // empty owner
            "host:/bar",        // empty host
            "/",                // just one slash
            "//",               // empty regex body — would catch-all
            "host:foo/bar/baz", // deeper than owner — must use regex
        ] {
            assert!(
                compile_pattern(bad).is_err(),
                "pattern `{bad}` should fail to compile"
            );
        }
    }

    #[test]
    fn compile_pattern_explicit_catchall_regex_works() {
        // Empty `//` is rejected, but the explicit `/.*/` catch-all
        // is accepted and matches everything.
        let p = compile_pattern("/.*/").unwrap();
        assert!(p.matches("github.com/foo/bar"));
        assert!(p.matches(""));
    }

    #[test]
    fn compile_pattern_rejects_invalid_regex() {
        let err = compile_pattern("/[/").unwrap_err();
        assert!(
            err.to_string().contains("regex"),
            "error should mention regex: {err}"
        );
    }

    fn route(pattern: &str, root: &str) -> Route {
        Route {
            pattern: pattern.into(),
            root: Some(root.into()),
            layout: None,
            default_vcs: None,
            default_protocol: None,
        }
    }

    fn resolved_with(routes: Vec<Route>) -> ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some("/global".into()),
                ..Default::default()
            },
            routes,
            ..Default::default()
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn resolve_target_falls_back_to_global_when_no_route_matches() {
        let r = resolved_with(vec![route("host:gitlab.com", "/elsewhere")]);
        let t = r.resolve_target("github.com/foo/bar");
        assert!(t.matched_route.is_none());
        assert!(t.root.ends_with("global"));
    }

    #[test]
    fn resolve_target_first_match_wins() {
        let r = resolved_with(vec![
            route("host:github.com/yukimemi", "/personal"),
            route("host:github.com", "/github-catchall"),
        ]);
        let t = r.resolve_target("github.com/yukimemi/shoka");
        assert_eq!(t.matched_route, Some(0));
        assert!(t.root.ends_with("personal"));
    }

    #[test]
    fn resolve_target_route_overrides_pass_through_unset_fields() {
        let r = ShokaConfig {
            global: GlobalConfig {
                root: Some("/global".into()),
                layout: "{{ root }}/L".into(),
                ..Default::default()
            },
            routes: vec![Route {
                pattern: "host:github.com".into(),
                root: Some("/gh".into()),
                layout: None, // should fall through to global
                default_vcs: Some(VcsDefault::Jj),
                default_protocol: None,
            }],
            ..Default::default()
        }
        .resolve(None)
        .expect("resolve");
        let t = r.resolve_target("github.com/foo/bar");
        assert!(t.root.ends_with("gh"));
        assert_eq!(t.layout, "{{ root }}/L"); // fell through
        assert_eq!(t.default_vcs, VcsDefault::Jj);
        assert_eq!(t.default_protocol, Protocol::Https); // global default
    }

    #[test]
    fn resolve_target_profile_root_beats_routes() {
        // When the active profile pinned its own root, routes don't
        // get a chance to override — this is the "explicit user
        // intent beats auto-routing" rule.
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/global".into()),
                ..Default::default()
            },
            routes: vec![route("host:github.com", "/routed")],
            profiles: BTreeMap::from([(
                "work".into(),
                ProfileConfig {
                    root: Some("/work".into()),
                    ..Default::default()
                },
            )]),
        };
        let r = cfg.resolve(Some("work")).expect("resolve");
        assert!(r.profile_provided_root);
        let t = r.resolve_target("github.com/foo/bar");
        assert!(
            t.matched_route.is_none(),
            "profile.root should skip route matching, got {:?}",
            t.matched_route
        );
        assert!(t.root.ends_with("work"));
    }

    #[test]
    fn resolve_target_routes_run_when_profile_lacks_root() {
        // Profile is active but didn't pin a root → routes still run.
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/global".into()),
                ..Default::default()
            },
            routes: vec![route("host:github.com", "/routed")],
            profiles: BTreeMap::from([(
                "vcs-only".into(),
                ProfileConfig {
                    default_vcs: Some(VcsDefault::Jj),
                    ..Default::default()
                },
            )]),
        };
        let r = cfg.resolve(Some("vcs-only")).expect("resolve");
        assert!(!r.profile_provided_root);
        let t = r.resolve_target("github.com/foo/bar");
        assert_eq!(t.matched_route, Some(0));
        assert!(t.root.ends_with("routed"));
        // CodeRabbit semantic: profile's default_vcs must NOT bleed
        // into the route result. Profile bows out of clone routing
        // once it didn't pin root, so an unset route field falls
        // through to raw [global], not the profile's value.
        assert_eq!(
            t.default_vcs,
            VcsDefault::Auto,
            "profile vcs should not leak into routes when profile.root isn't pinned"
        );
    }

    #[test]
    fn resolve_target_no_match_uses_raw_global_not_profile_overlay() {
        // Same "profile owns clone or stays out" rule applied to the
        // no-route-match fallback: if profile has fields but no root,
        // a clone with no matching route still uses raw [global].
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/global".into()),
                default_vcs: VcsDefault::Auto,
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::from([(
                "vcs-only".into(),
                ProfileConfig {
                    default_vcs: Some(VcsDefault::Jj),
                    ..Default::default()
                },
            )]),
        };
        let r = cfg.resolve(Some("vcs-only")).expect("resolve");
        let t = r.resolve_target("github.com/foo/bar");
        assert!(t.matched_route.is_none());
        assert!(t.root.ends_with("global"));
        assert_eq!(
            t.default_vcs,
            VcsDefault::Auto,
            "profile.default_vcs must not apply when profile didn't pin a root"
        );
    }

    #[test]
    fn resolve_errors_on_bad_route_pattern() {
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ..Default::default()
            },
            routes: vec![Route {
                pattern: "not-a-valid-pattern".into(),
                root: None,
                layout: None,
                default_vcs: None,
                default_protocol: None,
            }],
            ..Default::default()
        };
        let err = cfg.resolve(None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("routes[0]") && msg.contains("unknown route pattern"),
            "error should locate the bad pattern: {msg}"
        );
    }

    #[test]
    fn routes_round_trip_through_toml_load() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/global"

[[routes]]
pattern = "host:github.com/mycompany"
root = "/work"

[[routes]]
pattern = "host:gitlab.com"
root = "/gitlab"
default_protocol = "ssh"
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.routes.len(), 2);
        let r = cfg.resolve(None).expect("resolve");

        let t1 = r.resolve_target("github.com/mycompany/internal-tool");
        assert_eq!(t1.matched_route, Some(0));
        assert!(t1.root.ends_with("work"));

        let t2 = r.resolve_target("gitlab.com/anyone/anything");
        assert_eq!(t2.matched_route, Some(1));
        assert_eq!(t2.default_protocol, Protocol::Ssh);

        let t3 = r.resolve_target("github.com/other/repo");
        assert!(t3.matched_route.is_none());
    }

    // --- end routes -----------------------------------------------

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
    fn cd_page_size_parses_and_floors_at_one() {
        // A user-set value round-trips through resolve …
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/r"

[global.ui]
cd_page_size = 30
"#,
        )
        .unwrap();
        let cfg = ShokaConfig::load(&paths_at(tmp.path())).expect("load");
        assert_eq!(cfg.global.ui.cd_page_size, 30);
        let resolved = cfg.resolve(None).expect("resolve");
        assert_eq!(resolved.ui.cd_page_size, 30);

        // … and a zero is floored to 1 (inquire panics on 0).
        let zeroed = ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ui: UiConfig {
                    cd_page_size: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
        .resolve(None)
        .expect("resolve");
        assert_eq!(zeroed.ui.cd_page_size, 1);
    }

    #[test]
    fn cd_page_size_defaults_to_fifteen() {
        // Guards the documented default so a starter-config user gets
        // the roomier picker without setting anything.
        assert_eq!(UiConfig::default().cd_page_size, 15);
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
        // Guards the contract: resolved `root` must be absolute
        // regardless of caller cwd.
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
    fn user_named_override_displaces_sibling_default_config_toml() {
        // Gemini round 4: when --config names a non-default file and
        // a `config.toml` happens to sit alongside it, the default
        // must NOT silently override the user's named config (which
        // would happen because teravars::load_merged merges files in
        // order with later entries winning).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/from/default"
"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("staging.toml"),
            r#"
[global]
root = "/from/staging"
"#,
        )
        .unwrap();
        let paths = ShokaPaths::resolve(Some(&tmp.path().join("staging.toml")))
            .expect("ShokaPaths::resolve");
        let cfg = ShokaConfig::load(&paths).expect("load");
        // Expect: the user's named file wins. The sibling
        // `config.toml` is excluded from the discovery merge entirely
        // when --config names a different file.
        assert_eq!(cfg.global.root.as_deref(), Some("/from/staging"));
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
