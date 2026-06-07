//! `shoka tui` — Phase 2 dashboard skeleton.
//!
//! A ratatui app over the shelf. Each row is a repo with its slug,
//! tags, and resolved clone path. Navigation is j/k (or arrow keys),
//! `/` opens a filter input that nucleo scores in real time, Enter
//! exits the TUI and emits the chosen repo's path via the same
//! `SHOKA_CD_OUT` sidechannel contract `shoka cd` uses — the shell
//! wrapper that already services `cd` picks up TUI's output without
//! changes.
//!
//! Layout (3 rows):
//!
//! 1. Header — counts + active filter prefix.
//! 2. Table — slug / branch / ↑↓ / ✓ / PR / CI / path / tags,
//!    current selection highlighted. Status columns read the
//!    cached `git_status` + `gh` snapshots off `cache.toml`;
//!    entries that haven't been refreshed yet render `?` (git) /
//!    `-` (gh) so users can tell "unchecked" or "no data" apart
//!    from a definite zero.
//! 3. Footer — mode-specific key hints, or the live filter input.
//!
//! The OSC 7 cwd hint is emitted on Enter-to-cd via the shared
//! [`emit_path`] sidechannel, so a tab/split opened after picking a
//! repo inherits its directory — see [`crate::commands::cd`].
//!
//! What's intentionally **not** here yet:
//!
//! - **Multi-select / bulk ops.** The TUI is currently a fancy `cd`
//!   picker. Bulk `exec --tag X -- ...` would be a natural extension.

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use nucleo::Matcher;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Terminal, prelude::Backend};
use teravars::Engine;

use crate::actions::{ActionKind, ActionOutcome, run_action};
use crate::cache::{Cache, current_unix_secs};
use crate::cli::TuiArgs;

/// Catppuccin Mocha palette, kept as `Color::Rgb` so the dashboard
/// reads the same on every true-color terminal — falling back to
/// indexed colors would lose the soft pastel feel that's the whole
/// point of the theme. The roles below match the upstream palette
/// names so a future refresh can swap in Frappé / Macchiato /
/// Latte by retargeting these constants only.
mod theme {
    use ratatui::style::Color;

    // Background ramp (deepest → lightest).
    pub const MANTLE: Color = Color::Rgb(0x18, 0x18, 0x25);
    pub const BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
    pub const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
    pub const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
    pub const OVERLAY: Color = Color::Rgb(0x6c, 0x70, 0x86);

    // Text ramp.
    pub const SUBTEXT: Color = Color::Rgb(0xba, 0xc2, 0xde);
    pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);

    // Accents. Picked from Catppuccin's named roles so the meaning
    // (success / warning / etc.) is portable across the file.
    pub const LAVENDER: Color = Color::Rgb(0xb4, 0xbe, 0xfe);
    pub const SKY: Color = Color::Rgb(0x89, 0xdc, 0xeb);
    pub const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
    pub const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
    pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
    pub const PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
    pub const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
    pub const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
    pub const PINK: Color = Color::Rgb(0xf5, 0xc2, 0xe7);
}
use crate::commands::ShokaContext;
use crate::commands::cd::emit_path;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::gh::{CiStatus, GhSnapshot};
use crate::git_status::{self, GitStatusSnapshot};
use crate::paths::ShokaPaths;
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: TuiArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        // Print rather than crash into ratatui's alternate-screen
        // setup — an empty shelf would render a blank dashboard with
        // no useful affordance.
        anyhow::bail!(
            "shelf is empty — nothing to dashboard. `shoka clone <url>` \
             or `shoka import <dir>` first"
        );
    }

    // Cache load is best-effort: a missing / corrupt cache still
    // lets the dashboard open (rows just render with `?` in the
    // status columns). The user can recover via `shoka cache clear`
    // + a refresh — the alternative (refusing to open the TUI)
    // would block them from doing it from inside shoka itself.
    let cache = match Cache::load(&ctx.paths) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "shoka", "tui: cache load failed, falling back to no-status mode ({e:#})");
            Cache::default()
        }
    };

    let rows = build_rows(&shelf, &resolved, &cache, &args.tags)?;
    if rows.is_empty() {
        anyhow::bail!(
            "no repos matched the tag filter ({} on the shelf total)",
            shelf.len()
        );
    }

    let own_owners = resolved.ui.own_owners.clone();
    let mut app = App::new(rows, own_owners, ctx.paths.clone());
    let selection = run_app(&mut app)?;

    // After the alternate screen tears down, emit the path so the
    // shell wrapper can `cd`. None = user quit without picking.
    if let Some(idx) = selection {
        emit_path(&app.rows[idx].path)?;
    }
    Ok(())
}

/// Pre-resolved row data the table renders. Building these once
/// up-front lets the per-frame render stay allocation-light.
#[derive(Debug, Clone)]
struct DashRow {
    slug: String,
    /// Host segment (the `<host>` in `<host>/<owner>/<name>`). Kept
    /// alongside `owner` / `name` so a post-action status refresh can
    /// rebuild the `(host, owner, name, path?)` cache identity without
    /// re-splitting `slug`.
    host: String,
    /// Bare owner segment (the `<owner>` in `<host>/<owner>/<name>`).
    /// Cached on construction so the mine-only filter doesn't
    /// re-split `slug` on every refilter pass.
    owner: String,
    /// Name segment (the `<name>` in `<host>/<owner>/<name>`). Other
    /// half of the cache identity, see `host`.
    name: String,
    path: PathBuf,
    /// The shelf [`Repo::path`] override (`None` for layout-derived
    /// repos). This — not the resolved clone `path` above — is the
    /// fourth component of the cache's `(host, owner, name, path?)`
    /// identity key, so a post-action refresh writes the snapshot back
    /// to the same row [`build_rows`] read it from.
    repo_path: Option<PathBuf>,
    /// `slug` + " " + path-as-string, cached so the per-keystroke
    /// nucleo scorer in [`App::refilter`] has a single haystack to
    /// score against. Path is included because the dashboard now
    /// renders multiple checkouts of the same remote (different
    /// `path`s for the same triple), and a slug-only filter would
    /// be useless for picking among them.
    search_key: String,
    /// Display string — `tags.join(", ")` cached so we don't
    /// re-join per frame.
    tags_display: String,
    /// Cached git status snapshot from `cache.toml`. `None` when
    /// the entry hasn't been refreshed yet — the TUI renders that
    /// distinctly from "snapshot says clean" so users can tell
    /// "unchecked" apart from "no changes".
    status: Option<GitStatusSnapshot>,
    /// Cached gh snapshot — open PR count + most-recent CI
    /// conclusion. `None` for non-github hosts, missing tokens, or
    /// API errors; TUI renders `-` in the PR/CI cells for that
    /// case so users can distinguish "no data" from "zero PRs".
    gh: Option<GhSnapshot>,
}

fn build_rows(
    shelf: &Shelf,
    resolved: &ResolvedConfig,
    cache: &Cache,
    tag_filter: &[String],
) -> Result<Vec<DashRow>> {
    let mut engine = Engine::new();
    let mut out = Vec::with_capacity(shelf.len());
    for repo in &shelf.repos {
        if !tag_filter.is_empty() && !has_all_tags(repo, tag_filter) {
            continue;
        }
        let path = resolved
            .clone_path_for(repo, &mut engine)
            .with_context(|| format!("resolving clone path for {}", repo.slug()))?;
        // Split lookup: `git_status` is per-checkout so it needs the
        // path-aware identity (multi-clone rows must each carry
        // their own branch / dirty / ahead-behind), while `gh` is
        // remote-derived — open PR count + CI status are the same
        // upstream value regardless of which local copy asks, so
        // sharing across triple siblings is correct and saves API
        // budget. Resolves the TODO from #57.
        let status = cache
            .find(&repo.host, &repo.owner, &repo.name, repo.path.as_deref())
            .and_then(|c| c.git_status.clone());
        let gh = cache
            .find_gh_by_triple(&repo.host, &repo.owner, &repo.name)
            .cloned();
        let slug = repo.slug();
        let search_key = format!("{slug} {}", path.display());
        out.push(DashRow {
            slug,
            host: repo.host.clone(),
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            path,
            repo_path: repo.path.clone(),
            search_key,
            tags_display: repo.tags.join(", "),
            status,
            gh,
        });
    }
    Ok(out)
}

fn has_all_tags(repo: &Repo, wanted: &[String]) -> bool {
    wanted.iter().all(|w| repo.tags.iter().any(|t| t == w))
}

/// Case-insensitive owner membership check used by the mine-only
/// gate. GitHub / GitLab / Gitea all treat owner names (users and
/// orgs) as case-insensitive, so a config of `["YukimemI"]` against
/// a slug `github.com/yukimemi/shoka` must still match — a literal
/// `==` would silently drop the row.
fn owner_in(owners: &[String], candidate: &str) -> bool {
    owners.iter().any(|o| o.eq_ignore_ascii_case(candidate))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
}

struct App {
    rows: Vec<DashRow>,
    /// Char count of the longest slug in [`rows`], precomputed once
    /// at construction so [`render_table`] doesn't redo an O(N)
    /// character-count sweep on every frame. Safe to cache: rows are
    /// fixed for the lifetime of the TUI session — post-action
    /// refreshes rewrite `status` / `gh`, never the slug set.
    longest_slug: u16,
    /// Owners (`<owner>` in `<host>/<owner>/<name>`) the user counts
    /// as "theirs" — sourced from `[ui].own_owners`. Empty disables
    /// the mine-only feature entirely (no toggle pill, `m` is a
    /// no-op-with-hint).
    own_owners: Vec<String>,
    /// When true, [`refilter`] excludes rows whose owner isn't in
    /// [`own_owners`]. Initialised to `!own_owners.is_empty()` so a
    /// configured user lands directly on their shelf; toggled by `m`.
    mine_only: bool,
    /// Filter query (the live input). Empty in normal mode.
    filter: String,
    /// Indices into `rows`, sorted by nucleo score (highest first).
    /// Recomputed from scratch whenever `filter` changes.
    matches: Vec<usize>,
    /// Position within `matches` of the currently-highlighted row.
    /// 0 when `matches` is empty (and the selection is just "none").
    cursor: usize,
    mode: Mode,
    /// `?` help popup overlay state. Orthogonal to [`Mode`] so the
    /// popup can be opened (or closed) from either Normal or Filter
    /// without leaking modal state between the two. Toggled by `?`
    /// or F1 and dismissed by `Esc`, `q`, or `?` again.
    show_help: bool,
    /// Issue / PR Telescope-style picker overlay. `Some` when an
    /// `i` / `p` keystroke fetched a list (or hit an error worth
    /// displaying); `None` otherwise. Intercepts all input while
    /// active, mirroring `show_help`. See [`Picker`] for the
    /// per-item state.
    picker: Option<Picker>,
    /// Fetch / push action result overlay. `Some` after `f` / `P`
    /// finishes (or fails). Any keystroke dismisses it. Intercepts
    /// input ahead of normal navigation so a stray `j` doesn't move
    /// the cursor while the user is still reading the result.
    action_popup: Option<ActionPopup>,
    /// Transient status banner shown in the footer after `y` / `o`
    /// (and other light, non-popup actions). Cleared on the next
    /// non-status-producing keystroke so the user always sees the
    /// outcome of their most recent action without it sticking
    /// around as visual noise.
    status_message: Option<String>,
    table_state: TableState,
    matcher: Matcher,
    /// Resolved paths, kept so a post-action status refresh can write
    /// the freshly-captured snapshot back to `cache.toml` for the one
    /// repo that was acted on — no full-shelf walk, just the row the
    /// user pressed `f` / `P` on.
    paths: ShokaPaths,
}

/// Result of an `f` / `P` keystroke. Holds the captured stdout +
/// stderr so the popup can show what git/jj said without the user
/// having to drop back to a shell. `outcome` is `None` for the
/// no-VCS-detected branch (where the action never ran).
#[derive(Debug, Clone)]
struct ActionPopup {
    kind: ActionKind,
    repo_label: String,
    outcome: Option<ActionOutcome>,
    /// Error message when [`outcome`] is `None`. Empty otherwise.
    error: String,
}

/// What the picker is showing — drives the title + the fetcher
/// chosen by `open_picker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Issues,
    Prs,
}

impl PickerKind {
    fn title(self) -> &'static str {
        match self {
            PickerKind::Issues => "Issues",
            PickerKind::Prs => "Pull Requests",
        }
    }
}

/// Picker overlay state. Three variants:
///
/// - [`PickerState::Loading`] — fetch task is in flight. Renders a
///   centred spinner so the user gets an immediate "I heard you" beat
///   instead of a frozen frame. Cancellable via Esc/q (the gh fetch
///   future is `.abort()`-ed on close).
/// - [`PickerState::Loaded`] — items are in, the filter input + list
///   are interactive.
/// - [`PickerState::Error`] — single-line failure message
///   (no token / non-github host / fetch errored). Same popup shell
///   so the user always sees *something* explaining why they pressed
///   the key.
struct Picker {
    kind: PickerKind,
    /// Display label for the repo the picker was opened on, e.g.
    /// `github.com/yukimemi/shoka`. Shown in the popup title so
    /// the user can tell which row they triggered against.
    repo_label: String,
    /// Allocated once at construction so per-keystroke refilter
    /// rounds don't pay for matcher init. Lives on `Picker` rather
    /// than inside [`PickerState::Loaded`] so it survives the
    /// `Loading → Loaded` state transition without re-allocating.
    matcher: Matcher,
    state: PickerState,
}

/// Result of an in-flight picker fetch: the items on success, or a
/// stringified error to display in the picker error state.
type PickerFetchResult = std::result::Result<Vec<crate::gh::PickerItem>, String>;

enum PickerState {
    Loading {
        /// Wall-clock instant the fetch task was spawned. Drives
        /// the spinner-frame computation in `render_picker` —
        /// `Instant::now().duration_since(started_at).as_millis() /
        /// FRAME_MS % FRAMES` keeps animation independent of the
        /// event-loop's wake cadence.
        started_at: std::time::Instant,
        /// Receiver for the spawned fetch task's result. Closed
        /// (`Err(Empty)` until ready, `Err(Closed)` if the task
        /// panicked / was dropped). `Ok(Ok(items))` on success,
        /// `Ok(Err(msg))` on a caught fetch error.
        rx: tokio::sync::oneshot::Receiver<PickerFetchResult>,
        /// Cancellation handle for the in-flight task. Closing the
        /// picker calls `.abort()` so a slow gh fetch isn't left
        /// charging through the user's network bandwidth after
        /// they've moved on.
        abort: tokio::task::AbortHandle,
    },
    Loaded {
        /// Items fetched from gh. Empty when the repo legitimately
        /// has no open issues / PRs.
        items: Vec<crate::gh::PickerItem>,
        /// Precomputed [`crate::gh::PickerItem::search_key`] per
        /// item. Built once at the `Loading → Loaded` transition
        /// so the hot path in [`Picker::refilter`] (one nucleo
        /// scoring round per item per keystroke) doesn't
        /// reallocate.
        search_keys: Vec<String>,
        /// Live filter query.
        filter: String,
        /// Indices into `items`, score-sorted (highest first).
        matches: Vec<usize>,
        /// Position within `matches` of the highlighted row.
        cursor: usize,
    },
    Error(String),
}

impl Picker {
    /// Test-only constructor that skips the async fetch and lands
    /// directly in the loaded state with the provided items.
    #[cfg(test)]
    fn loaded(kind: PickerKind, repo_label: String, items: Vec<crate::gh::PickerItem>) -> Self {
        let matches = (0..items.len()).collect();
        let search_keys = items.iter().map(|i| i.search_key()).collect();
        Self {
            kind,
            repo_label,
            matcher: Matcher::default(),
            state: PickerState::Loaded {
                items,
                search_keys,
                filter: String::new(),
                matches,
                cursor: 0,
            },
        }
    }

    fn error(kind: PickerKind, repo_label: String, msg: impl Into<String>) -> Self {
        Self {
            kind,
            repo_label,
            matcher: Matcher::default(),
            state: PickerState::Error(msg.into()),
        }
    }

    /// Indicates whether the event loop should switch to its
    /// short-timeout polling path (for spinner animation).
    fn is_loading(&self) -> bool {
        matches!(self.state, PickerState::Loading { .. })
    }

    /// Poll the in-flight fetch task. Transitions [`PickerState::
    /// Loading`] to either [`PickerState::Loaded`] (success) or
    /// [`PickerState::Error`] (caught failure, task drop, panic).
    /// No-op when the picker is already in a terminal state.
    /// Returns `true` iff a state transition happened so the
    /// event loop can decide to redraw immediately.
    fn poll_fetch(&mut self) -> bool {
        let PickerState::Loading { rx, .. } = &mut self.state else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(items)) => {
                let matches = (0..items.len()).collect();
                let search_keys = items.iter().map(|i| i.search_key()).collect();
                self.state = PickerState::Loaded {
                    items,
                    search_keys,
                    filter: String::new(),
                    matches,
                    cursor: 0,
                };
                true
            }
            Ok(Err(msg)) => {
                self.state = PickerState::Error(msg);
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.state = PickerState::Error("fetch task ended without a result".into());
                true
            }
        }
    }

    /// Abort the in-flight fetch task if any. Called when the user
    /// dismisses the picker before the fetch lands — we owe it to
    /// them not to keep the network handle live.
    fn abort_inflight(&self) {
        if let PickerState::Loading { abort, .. } = &self.state {
            abort.abort();
        }
    }

    /// Re-rank items against the current filter. Empty filter =
    /// identity order (as returned by the gh API, which is
    /// most-recently-updated first); a non-empty filter scores each
    /// item's precomputed `search_keys` entry via nucleo and keeps
    /// positive-score matches sorted descending. No-op outside of
    /// [`PickerState::Loaded`].
    fn refilter(&mut self) {
        let PickerState::Loaded {
            items,
            search_keys,
            filter,
            matches,
            cursor,
        } = &mut self.state
        else {
            return;
        };
        if filter.is_empty() {
            *matches = (0..items.len()).collect();
        } else {
            let pattern = Pattern::parse(filter, CaseMatching::Smart, Normalization::Smart);
            let mut scored: Vec<(usize, u32)> = Vec::new();
            let mut buf: Vec<char> = Vec::new();
            for (idx, key) in search_keys.iter().enumerate() {
                buf.clear();
                let haystack = nucleo::Utf32Str::new(key, &mut buf);
                if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                    scored.push((idx, score));
                }
            }
            scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
            *matches = scored.into_iter().map(|(idx, _)| idx).collect();
        }
        *cursor = 0;
    }

    fn move_down(&mut self) {
        let PickerState::Loaded {
            matches, cursor, ..
        } = &mut self.state
        else {
            return;
        };
        if matches.is_empty() {
            return;
        }
        *cursor = (*cursor + 1).min(matches.len() - 1);
    }

    fn move_up(&mut self) {
        let PickerState::Loaded { cursor, .. } = &mut self.state else {
            return;
        };
        *cursor = cursor.saturating_sub(1);
    }

    /// The currently-highlighted item, if any. `None` outside of
    /// `Loaded`, or when the list is empty.
    fn selected(&self) -> Option<&crate::gh::PickerItem> {
        let PickerState::Loaded {
            items,
            matches,
            cursor,
            ..
        } = &self.state
        else {
            return None;
        };
        let idx = *matches.get(*cursor)?;
        items.get(idx)
    }
}

impl Drop for Picker {
    /// Belt-and-braces cancellation: any path that drops the picker
    /// (Esc, `q`, app exit, panic, future dismissal sites) releases
    /// the in-flight fetch task instead of leaking the network
    /// handle. The explicit Esc/q handler no longer needs to call
    /// `abort_inflight` — Drop handles it.
    fn drop(&mut self) {
        self.abort_inflight();
    }
}

impl App {
    fn new(rows: Vec<DashRow>, own_owners: Vec<String>, paths: ShokaPaths) -> Self {
        let mine_only = !own_owners.is_empty();
        // Same identity-order seed as before, then narrowed by
        // `mine_only` so the initial matches respect the configured
        // default scope — without this, an own_owners user would see
        // every shelf row for one frame before the first navigation.
        let mut matches: Vec<usize> = (0..rows.len()).collect();
        if mine_only {
            matches.retain(|&i| owner_in(&own_owners, &rows[i].owner));
        }
        let mut table_state = TableState::default();
        table_state.select(if matches.is_empty() { None } else { Some(0) });
        // Sized to *all* rows, not the current matches, so the repo
        // column doesn't jitter while the user types a filter.
        let longest_slug = rows
            .iter()
            .map(|r| r.slug.chars().count())
            .max()
            .unwrap_or(0)
            .min(usize::from(u16::MAX)) as u16;
        Self {
            rows,
            longest_slug,
            own_owners,
            mine_only,
            filter: String::new(),
            matches,
            cursor: 0,
            mode: Mode::Normal,
            show_help: false,
            picker: None,
            action_popup: None,
            status_message: None,
            table_state,
            matcher: Matcher::default(),
            paths,
        }
    }

    /// True when [`row`] belongs to one of the configured
    /// [`own_owners`]. Empty own_owners short-circuits to `true` so
    /// callers that gate on this (currently only [`refilter`]) treat
    /// the unconfigured case as "everything is mine".
    fn is_mine(&self, row: &DashRow) -> bool {
        self.own_owners.is_empty() || owner_in(&self.own_owners, &row.owner)
    }

    /// Flip the mine-only flag and re-run the filter pass so the
    /// table reflects the new scope on the next frame. No-op (with a
    /// status banner) when own_owners is empty — pressing `m` then
    /// would silently change nothing and leave the user wondering
    /// whether the key registered.
    fn toggle_mine_only(&mut self) {
        if self.own_owners.is_empty() {
            self.status_message =
                Some("no `[ui].own_owners` configured — nothing to scope to".into());
            return;
        }
        self.mine_only = !self.mine_only;
        self.refilter();
        self.status_message = Some(if self.mine_only {
            "scope: mine".into()
        } else {
            "scope: all".into()
        });
    }

    /// Recompute `matches` against `filter` + the mine-only scope.
    /// Empty filter = identity (everything in shelf order, restricted
    /// to mine when [`mine_only`] is set); otherwise nucleo scores
    /// each row's `search_key` (slug + path) and we keep the matches
    /// sorted by score descending. Path is in the haystack so that
    /// multiple path-pinned checkouts of the same remote can be
    /// distinguished by typing part of the dir name. The mine-only
    /// gate is applied *before* scoring so non-owned rows never enter
    /// the result set even when their search_key would have matched.
    /// Cursor pins to the top so the highlighted row is always
    /// visible after a refilter.
    fn refilter(&mut self) {
        if self.filter.is_empty() {
            self.matches = (0..self.rows.len())
                .filter(|&i| !self.mine_only || self.is_mine(&self.rows[i]))
                .collect();
        } else {
            let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
            let mut scored: Vec<(usize, u32)> = Vec::new();
            let mut buf: Vec<char> = Vec::new();
            for (idx, row) in self.rows.iter().enumerate() {
                if self.mine_only && !self.is_mine(row) {
                    continue;
                }
                buf.clear();
                let haystack = nucleo::Utf32Str::new(&row.search_key, &mut buf);
                if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                    scored.push((idx, score));
                }
            }
            // Sort by score descending — Reverse so `sort_by_key`
            // gives the natural "best match first" ordering.
            scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
            self.matches = scored.into_iter().map(|(idx, _)| idx).collect();
        }
        self.cursor = 0;
        self.table_state.select(if self.matches.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1).min(self.matches.len() - 1);
        self.table_state.select(Some(self.cursor));
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        if !self.matches.is_empty() {
            self.table_state.select(Some(self.cursor));
        }
    }

    fn selected_row(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
    }
}

/// RAII guard: enabling raw mode + alt-screen in `new`, tearing
/// them down in `drop`. Using a guard rather than an explicit
/// cleanup block at the end of `run_app` means a panic anywhere
/// inside the TUI still restores the terminal — without the guard,
/// the user would be stranded in raw mode with their input
/// invisible until they hit `reset`.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enabling terminal raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
            .context("entering alt screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Tear down in the exact reverse of `new`: mouse capture +
        // alt-screen first, raw mode LAST. Order matters on Windows.
        //
        // crossterm's `DisableMouseCapture` restores the console input
        // mode that was captured when `EnableMouseCapture` ran — and that
        // snapshot was taken *after* `enable_raw_mode`, so it still has
        // ENABLE_LINE_INPUT / ENABLE_ECHO_INPUT / ENABLE_PROCESSED_INPUT
        // cleared. If we called `disable_raw_mode` first, the subsequent
        // `DisableMouseCapture` would clobber the cooked bits it just
        // restored, leaving the *whole console* in raw mode after shoka
        // exits — so the next program that does a line read in this
        // terminal (e.g. `shoka self-update`'s `[y/N]` prompt) hangs with
        // no echo and an inert Ctrl-C. Running `disable_raw_mode` last
        // gives it the final say on the input mode.
        //
        // Best-effort throughout — there's nothing we can do if any step
        // fails (no panic-in-drop), and the user gets their terminal back
        // one way or another. `cursor::Show` is explicit because
        // LeaveAlternateScreen restores the main buffer but not the cursor
        // visibility flag, which TableState rendering hid.
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

/// Main loop. Returns `Ok(Some(idx))` when the user pressed Enter on
/// a row, `Ok(None)` when they quit (q / Esc / Ctrl-C). The
/// [`TerminalGuard`] takes care of teardown — including on panic.
fn run_app(app: &mut App) -> Result<Option<usize>> {
    let _guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("constructing ratatui terminal")?;
    event_loop(&mut terminal, app)
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<Option<usize>> {
    loop {
        // Drain the picker's in-flight fetch channel before draw so
        // a just-arrived result lands in the same frame the user
        // sees rather than waiting for the next keystroke. No-op
        // outside of `PickerState::Loading`.
        if let Some(picker) = app.picker.as_mut() {
            picker.poll_fetch();
        }

        // ratatui's `Backend::Error` is not always `std::error::Error +
        // Send + Sync` (depends on the backend), so anyhow's
        // `.context` blanket impl doesn't apply. Convert manually.
        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow::anyhow!("drawing frame: {e}"))?;

        // Default to a blocking `event::read()` — there's nothing
        // animating in the quiescent TUI and Ctrl-C arrives as a
        // `KeyEvent` under crossterm's raw mode (not a signal), so
        // we're never stranded waiting. The exception is when a
        // picker is loading: we want the braille spinner to tick
        // every ~80 ms and we need to revisit `poll_fetch` even
        // without keyboard input, so switch to polled reads with
        // the spinner's frame interval as the timeout.
        if app.picker.as_ref().is_some_and(|p| p.is_loading())
            && !event::poll(std::time::Duration::from_millis(SPINNER_FRAME_MS as u64))
                .context("polling for event")?
        {
            // Timeout — no key. Loop back to redraw the next
            // spinner frame and re-poll the fetch channel.
            continue;
        }
        let Event::Key(key) = event::read().context("reading event")? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            // Windows fires Press + Release; we only act on Press so
            // a single physical keystroke doesn't double-fire.
            continue;
        }

        // Action popup intercepts everything — it's the latest
        // modal and the user is reading the captured git/jj output.
        // Any keystroke (other than Ctrl-C, which always quits)
        // dismisses it, matching the "press any key to continue"
        // convention.
        if app.action_popup.is_some() {
            if key.code == KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                return Ok(None);
            }
            app.action_popup = None;
            continue;
        }

        // Picker overlay intercepts input first — it's the most
        // recently opened modal, so dismissal there takes priority
        // over the help popup or normal navigation.
        if app.picker.is_some() {
            // Ctrl-C escape hatch (same reasoning as the help popup).
            if key.code == KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                return Ok(None);
            }
            handle_picker_key(app, key);
            continue;
        }

        // Help popup intercepts everything except its own dismissal
        // keys + Ctrl-C. Keep this check outside `match app.mode` so
        // the popup can be opened from Filter mode too — but only
        // Normal-mode keys (specifically `?` / F1) open it; while in
        // Filter, `?` is still a literal character to type.
        if app.show_help {
            // Ctrl-C is the "I want out, no matter what" key. Honour
            // it even with the popup open — making the user dismiss
            // the popup first before they can quit is a UX trap
            // (especially if a stuck render somehow strands the
            // popup), and Ctrl-C is the only key reliably reachable
            // when other dismissals don't.
            if key.code == KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                return Ok(None);
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1) => {
                    app.show_help = false;
                }
                _ => {}
            }
            continue;
        }

        match app.mode {
            Mode::Normal => {
                // Any keystroke in normal mode wipes the last
                // status banner (y / o being the only setters
                // today). y / o set it again inside their handlers
                // below, so repeating either keeps the message
                // visible until a *different* key arrives.
                app.status_message = None;
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        return Ok(None);
                    }
                    KeyCode::Char('?') | KeyCode::F(1) => {
                        app.show_help = true;
                    }
                    KeyCode::Char('i') => open_picker(app, PickerKind::Issues),
                    KeyCode::Char('p') => open_picker(app, PickerKind::Prs),
                    KeyCode::Char('f') => run_action_for_selected(app, ActionKind::Fetch),
                    KeyCode::Char('P') => run_action_for_selected(app, ActionKind::Push),
                    KeyCode::Char('y') => yank_selected_slug(app),
                    KeyCode::Char('o') => open_selected_repo_home(app),
                    KeyCode::Char('m') => app.toggle_mine_only(),
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Char('g') => {
                        app.cursor = 0;
                        if !app.matches.is_empty() {
                            app.table_state.select(Some(0));
                        }
                    }
                    KeyCode::Char('G') if !app.matches.is_empty() => {
                        app.cursor = app.matches.len() - 1;
                        app.table_state.select(Some(app.cursor));
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::Filter;
                    }
                    KeyCode::Enter => {
                        return Ok(app.selected_row());
                    }
                    _ => {}
                }
            }
            Mode::Filter => match key.code {
                KeyCode::Esc => {
                    app.filter.clear();
                    app.refilter();
                    app.mode = Mode::Normal;
                }
                KeyCode::Enter => {
                    // Accept the filter and drop back to normal mode.
                    // Cursor is already at the top match.
                    app.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.refilter();
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.refilter();
                }
                _ => {}
            },
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // table
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    render_header(f, chunks[0], app);
    render_table(f, chunks[1], app);
    render_footer(f, chunks[2], app);

    // Help popup is drawn over the dashboard; the picker is drawn
    // *over the help popup* — both use `Clear` to wipe their rect
    // first, so the lower layers don't bleed through. Order matters:
    // we draw picker last so it visually wins when both flags are
    // set (shouldn't happen via UI flow, but defensive).
    if app.show_help {
        render_help(f, f.area());
    }
    if let Some(picker) = &app.picker {
        render_picker(f, f.area(), picker);
    }
    if let Some(popup) = &app.action_popup {
        render_action_popup(f, f.area(), popup);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::text::Span;

    let total = app.rows.len();
    let shown = app.matches.len();

    // ✦ marker + bold "shoka" + version pulled from cargo so the
    // banner stays in lockstep with the release the user is running.
    let mut spans = vec![
        Span::styled(" ✦ ", Style::default().fg(theme::MAUVE)),
        Span::styled(
            "shoka",
            Style::default()
                .fg(theme::LAVENDER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme::OVERLAY),
        ),
        Span::styled("  📚 ", Style::default().fg(theme::PEACH)),
        Span::styled(
            shown.to_string(),
            Style::default()
                .fg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {total} repo(s)"),
            Style::default().fg(theme::SUBTEXT),
        ),
    ];
    // Surface the active scope when own_owners is configured so the
    // user can tell at a glance whether they're looking at the
    // narrowed default or the full shelf. Hidden otherwise to avoid
    // labelling a feature that isn't wired up for them.
    if !app.own_owners.is_empty() {
        spans.push(Span::styled("  ◇ ", Style::default().fg(theme::OVERLAY)));
        spans.push(Span::styled(
            if app.mine_only { "mine" } else { "all" },
            Style::default()
                .fg(if app.mine_only {
                    theme::TEAL
                } else {
                    theme::PEACH
                })
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !app.filter.is_empty() {
        spans.push(Span::styled(
            "  ◇ /",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            &app.filter,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::MANTLE)),
        area,
    );
}

/// Fixed width the table consumes outside the repo column: 2 block
/// borders + 3 highlight symbol (`" ▶ "`) + 8 column gaps (9 columns
/// at the default `column_spacing` of 1) + the fixed-width columns
/// (branch 14 + ↑↓ 8 + dirty 2 + PR 4 + CI 2 + activity 13 + tags 14
/// = 57) + the path column's `Min(20)`. Must stay in sync with the
/// `widths` array in [`render_table`].
const REPO_COL_RESERVED: u16 = 2 + 3 + 8 + 57 + 20;

/// Don't let the repo column collapse below this even on absurdly
/// narrow terminals — `…/name` tails stop being legible. ratatui's
/// layout solver squeezes further on its own when even the floor
/// doesn't fit.
const REPO_COL_FLOOR: u16 = 10;

/// Width for the repo column: hug the longest slug (+1 for the
/// leading pad span) when the terminal is wide, hand width back to
/// the path column's `Min(20)` when it isn't. The previous
/// `Percentage(28)` sizing truncated the *tail* of every slug on
/// narrow terminals — and since every row shares the `host/owner/`
/// prefix, the tail is exactly the part that tells repos apart.
fn repo_column_width(area_width: u16, longest_slug: u16) -> u16 {
    let available = area_width.saturating_sub(REPO_COL_RESERVED);
    longest_slug
        .saturating_add(1)
        .min(available)
        .max(REPO_COL_FLOOR)
}

/// Left-elide `slug` to at most `max_chars` characters, keeping the
/// tail. Slugs are `host/owner/name`, so when something has to go
/// it's the shared prefix — the name tail is what identifies the
/// row. Char-count width is fine here: hosts / owners / repo names
/// are ASCII on every forge shoka talks to.
fn elide_slug_left(slug: &str, max_chars: u16) -> String {
    let max = max_chars as usize;
    let len = slug.chars().count();
    if len <= max {
        return slug.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Slice at the char boundary instead of `.chars().collect()`ing
    // a temporary String — one allocation (the `format!`) instead of
    // two, and still UTF-8-safe via `char_indices`.
    let skip_chars = len - (max - 1);
    let byte_idx = slug
        .char_indices()
        .nth(skip_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(slug.len());
    format!("…{}", &slug[byte_idx..])
}

fn render_table(f: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::text::Span;
    use ratatui::widgets::Cell;

    // Content-driven repo column width. `longest_slug` is cached on
    // App (slugs never change mid-session), so this stays O(1) per
    // frame.
    let repo_w = repo_column_width(area.width, app.longest_slug);

    let header_row = Row::new(vec![
        Cell::from("  repo "),
        Cell::from(" branch "),
        Cell::from(" ↑↓ "),
        Cell::from(" ✓ "),
        Cell::from(" PR "),
        Cell::from(" CI "),
        Cell::from(" activity "),
        Cell::from(" path "),
        Cell::from(" tags "),
    ])
    .style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(theme::PINK)
            .bg(theme::SURFACE0),
    )
    .height(1);

    let rows: Vec<Row> = app
        .matches
        .iter()
        .enumerate()
        .map(|(visible_idx, &row_idx)| {
            let row = &app.rows[row_idx];
            let (branch, ahead_behind, dirty) = status_cells(row.status.as_ref());
            let (pr, ci) = gh_cells(row.gh.as_ref());
            let activity = sparkline(row.gh.as_ref().and_then(|g| g.weekly_commits.as_deref()));

            // Alternating row tint — subtle stripe that makes
            // long shelves easier to scan without competing with
            // the selection highlight (which gets full LAVENDER).
            let row_bg = if visible_idx % 2 == 0 {
                theme::BASE
            } else {
                theme::MANTLE
            };
            let base_style = Style::default().fg(theme::TEXT).bg(row_bg);

            // The selection highlight (LAVENDER bg) bleeds into
            // its cells via `row_highlight_style`, so cell-level
            // styling is the *unselected* look — the highlight
            // overrides only `bg` and `fg`, leaving span colors
            // for non-current rows intact.
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::raw(" "),
                    // `repo_w - 1` budgets for the pad span above.
                    Span::styled(
                        elide_slug_left(&row.slug, repo_w.saturating_sub(1)),
                        Style::default().fg(theme::TEXT),
                    ),
                ])),
                Cell::from(Span::styled(branch, Style::default().fg(theme::SKY))),
                Cell::from(style_ahead_behind(&ahead_behind)),
                Cell::from(style_dirty(&dirty)),
                Cell::from(style_pr(&pr)),
                Cell::from(style_ci(&ci)),
                Cell::from(Line::from(vec![Span::raw(" "), style_activity(activity)])),
                Cell::from(Span::styled(
                    row.path.to_string_lossy(),
                    Style::default().fg(theme::OVERLAY),
                )),
                Cell::from(Span::styled(
                    &row.tags_display,
                    Style::default().fg(theme::TEAL),
                )),
            ])
            .style(base_style)
        })
        .collect();

    let widths = [
        Constraint::Length(repo_w), // repo (content-driven; see repo_column_width)
        Constraint::Length(14),     // branch
        Constraint::Length(8),      // ↑N ↓N
        Constraint::Length(2),      // dirty glyph
        Constraint::Length(4),      // PR count (e.g. "99+")
        Constraint::Length(2),      // CI glyph
        Constraint::Length(13),     // activity sparkline (1 lead + up to 12 weeks)
        Constraint::Min(20),        // path
        Constraint::Length(14),     // tags
    ];
    let table = Table::new(rows, widths)
        .header(header_row)
        .row_highlight_style(
            Style::default()
                .bg(theme::LAVENDER)
                .fg(theme::MANTLE)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▶ ")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::OVERLAY))
                .style(Style::default().bg(theme::BASE)),
        );

    // Pass the real `TableState` by `&mut` so the scroll offset
    // ratatui computes (for keeping the highlighted row visible as
    // the cursor moves off-screen) actually persists across frames.
    // Cloning here would discard those mutations and break long-
    // shelf scrolling.
    f.render_stateful_widget(table, area, &mut app.table_state);
}

/// Style the ahead/behind cell by its shape. `=` (in sync) reads
/// dim green so it doesn't compete with the actually-interesting
/// rows; `↑N` (ahead, pushable) is bright green; `↓N` (behind,
/// needs pull) is yellow; mixed `↑N ↓M` is mauve (action-required
/// but ambiguous direction). Unknown / no-upstream falls back to
/// overlay so it visibly recedes.
fn style_ahead_behind(s: &str) -> ratatui::text::Span<'static> {
    use ratatui::text::Span;
    let color = if s == "=" {
        theme::OVERLAY
    } else if s.starts_with('↑') && s.contains('↓') {
        theme::MAUVE
    } else if s.starts_with('↑') {
        theme::GREEN
    } else if s.starts_with('↓') {
        theme::YELLOW
    } else {
        theme::OVERLAY
    };
    Span::styled(s.to_string(), Style::default().fg(color))
}

/// Dirty glyph: clean `✓` reads dim green, dirty `●` pops peach so
/// the user's eye immediately catches "this row has uncommitted
/// changes" without parsing text. `?` (never refreshed) stays
/// overlay-grey for the same don't-distract reason as `=` in
/// ahead/behind.
fn style_dirty(s: &str) -> ratatui::text::Span<'static> {
    use ratatui::text::Span;
    let color = match s {
        "✓" => theme::GREEN,
        "●" => theme::PEACH,
        _ => theme::OVERLAY,
    };
    Span::styled(s.to_string(), Style::default().fg(color))
}

/// PR cell: any non-zero, non-dash count is pink+bold so a busy
/// mono-repo visibly stands out. Zero / `-` reads overlay so it
/// recedes — the dashboard's job is to highlight *what to act on*.
fn style_pr(s: &str) -> ratatui::text::Span<'static> {
    use ratatui::text::Span;
    let style = if s == "0" || s == "-" {
        Style::default().fg(theme::OVERLAY)
    } else {
        Style::default()
            .fg(theme::PINK)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(s.to_string(), style)
}

/// CI glyph: green check / red cross / yellow pending / overlay
/// skipped — the standard traffic-light reading. `!` (unknown
/// conclusion) reads red since "we don't know if this passed" is
/// closer to "broken" than to "fine".
fn style_ci(s: &str) -> ratatui::text::Span<'static> {
    use ratatui::text::Span;
    let color = match s {
        "✓" => theme::GREEN,
        "✗" | "!" => theme::RED,
        "◐" => theme::YELLOW,
        "○" => theme::OVERLAY,
        _ => theme::OVERLAY,
    };
    Span::styled(s.to_string(), Style::default().fg(color))
}

/// Render weekly commit counts (oldest → newest) as a unicode
/// sparkline — the dashboard's contribution-graph column.
///
/// Normalised to the row's **own** max so each repo's recent rhythm
/// reads on its own scale: a busy mono-repo and a weekend hobby repo
/// both fill the cell, showing trend rather than absolute volume.
///
/// Three distinct looks:
/// - `None` / empty (snapshot absent, or GitHub hasn't computed the
///   stat yet) → a single dim `-`, "never measured".
/// - all-zero history → a flat run of the lowest bar `▁`, "measured,
///   dormant" — deliberately different from `-`.
/// - otherwise → `▁▂▃▄▅▆▇█` scaled 0..=max, rounded to nearest bar.
fn sparkline(weekly: Option<&[u32]>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let Some(weekly) = weekly.filter(|w| !w.is_empty()) else {
        return "-".into();
    };
    let max = weekly.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return std::iter::repeat_n(BARS[0], weekly.len()).collect();
    }
    let max = u64::from(max);
    weekly
        .iter()
        .map(|&w| {
            // Round (w/max)·7 to the nearest bar so a mid-height week
            // reads mid-height instead of truncating toward `▁`.
            let idx = ((u64::from(w) * 7 + max / 2) / max) as usize;
            BARS[idx.min(7)]
        })
        .collect()
}

/// Colour the activity sparkline: GitHub-green when there's data,
/// dim overlay for the `-` "no data" placeholder so it recedes like
/// the other unmeasured cells.
fn style_activity(s: String) -> ratatui::text::Span<'static> {
    use ratatui::text::Span;
    let color = if s == "-" {
        theme::OVERLAY
    } else {
        theme::GREEN
    };
    Span::styled(s, Style::default().fg(color))
}

/// Format the two gh cells (open PR count + CI glyph). `(-, -)`
/// when the snapshot is `None` (non-github host, missing token, or
/// API error); otherwise PR count is clamped to "99+" so the
/// 4-char column doesn't blow out on a busy mono-repo, and CI is
/// reduced to a single glyph for at-a-glance scanning.
fn gh_cells(snap: Option<&GhSnapshot>) -> (String, String) {
    let Some(snap) = snap else {
        return ("-".into(), "-".into());
    };
    let pr = match snap.open_pr_count {
        Some(n) if n >= 100 => "99+".into(),
        Some(n) => n.to_string(),
        None => "-".into(),
    };
    let ci = match snap.ci_status {
        Some(CiStatus::Success) => "✓".into(),
        Some(CiStatus::Failure) => "✗".into(),
        Some(CiStatus::Pending) => "◐".into(),
        Some(CiStatus::Skipped) => "○".into(),
        Some(CiStatus::Other) => "!".into(),
        None => "-".into(),
    };
    (pr, ci)
}

/// Format the three status cells the dashboard renders for one row.
/// Returns `(branch, ahead_behind, dirty)`. When `status` is `None`
/// (entry hasn't been refreshed yet), all three show a faint `?`
/// rather than blank — users can tell "unchecked" from "clean".
fn status_cells(status: Option<&GitStatusSnapshot>) -> (String, String, String) {
    let Some(s) = status else {
        return ("?".into(), "?".into(), "?".into());
    };
    let branch = s.branch.clone().unwrap_or_else(|| "-".into());
    let ahead_behind = match (s.ahead, s.behind) {
        (Some(0), Some(0)) => "=".to_string(),
        (Some(a), Some(b)) if a > 0 && b > 0 => format!("↑{a} ↓{b}"),
        (Some(a), Some(0)) if a > 0 => format!("↑{a}"),
        (Some(0), Some(b)) if b > 0 => format!("↓{b}"),
        // Either side unknown — no upstream ref to compare against.
        _ => "-".into(),
    };
    let dirty = if s.dirty { "●".into() } else { "✓".into() };
    (branch, ahead_behind, dirty)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::text::Span;

    // A status message (set by y / o) preempts the mode hint so the
    // user immediately sees what their last action did. Styled
    // brighter than the hint (teal vs subtext) so it reads as a
    // fresh result, not a permanent legend.
    if let Some(msg) = &app.status_message {
        let line = Line::from(vec![
            Span::styled(
                " ✦ ",
                Style::default()
                    .fg(theme::TEAL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                msg,
                Style::default()
                    .fg(theme::TEAL)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme::MANTLE)),
            area,
        );
        return;
    }

    let line = match app.mode {
        Mode::Normal => {
            // Build pill set dynamically: the `m` toggle only makes
            // sense when `[ui].own_owners` is configured, so hide it
            // entirely otherwise rather than render a key that
            // produces a "nothing to scope to" hint when pressed.
            // Label reflects the *result* of pressing (i.e. shows
            // `all` when currently mine, `mine` when currently all)
            // so the user can read the footer as "what `m` will do".
            let mut pills: Vec<(&'static str, &'static str)> =
                vec![("j/k", "move"), ("/", "filter"), ("⏎", "cd")];
            if !app.own_owners.is_empty() {
                pills.push(("m", if app.mine_only { "all" } else { "mine" }));
            }
            pills.extend_from_slice(&[
                ("i", "iss"),
                ("p", "PR"),
                ("f", "fetch"),
                ("P", "push"),
                ("y", "yank"),
                ("o", "open"),
                ("?", "help"),
                ("q", "quit"),
            ]);
            Line::from(footer_pills(&pills))
        }
        Mode::Filter => Line::from(footer_pills(&[
            ("type", "filter"),
            ("⌫", "del"),
            ("⏎", "accept"),
            ("esc", "clear"),
        ])),
    };
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::MANTLE)),
        area,
    );
}

/// Build the styled spans for the footer's keybind pills. Each
/// `(key, label)` pair renders as `[key]label` with the key bracketed
/// in mauve+bold and the label dim subtext. A space separator before
/// every pill keeps the gap consistent (a trailing one before the
/// first pill is fine — it doubles as the left-edge padding the
/// terminal would otherwise eat).
///
/// Labels are `&'static str` so the per-frame render path skips
/// the per-pill `String` allocation that an owned label would
/// require; the bracketed key still allocates (one `String` per
/// pill per frame) but that's bounded by the small pill count and
/// keeps the API ergonomic.
fn footer_pills(pairs: &[(&'static str, &'static str)]) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(pairs.len() * 4);
    for (key, label) in pairs {
        out.push(Span::raw(" "));
        out.push(Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        ));
        out.push(Span::styled(*label, Style::default().fg(theme::SUBTEXT)));
    }
    out
}

/// Render the help popup. Centered in the terminal, sized to fit
/// the keybind table comfortably without flexing per frame. Clears
/// its rect first so the underlying table doesn't bleed through.
///
/// Kept colocated with the dashboard widgets so the legend stays in
/// lockstep with what `event_loop` actually handles — if a key is
/// added or renamed there, this list is the one place to update
/// alongside it.
fn render_help(f: &mut Frame, area: Rect) {
    use ratatui::text::Span;

    let popup = centered_rect(62, 78, area);

    // Sectioned layout so related keys cluster visually instead of
    // dissolving into a flat 15-row list. Sections render with a
    // mauve header line; entries inside line up by widest-key
    // padding *within each section* so each block is locally tidy.
    let sections: [(&str, &[(&str, &str)]); 4] = [
        (
            "Navigation",
            &[
                ("j / ↓", "move down"),
                ("k / ↑", "move up"),
                ("g", "jump to top"),
                ("G", "jump to bottom"),
                ("/", "filter — type to narrow, esc to clear"),
                ("m", "toggle mine / all (needs [ui].own_owners)"),
                ("Enter", "select (emit path for the shell wrapper to cd)"),
            ],
        ),
        (
            "Pickers & Browser",
            &[
                ("i", "open Issues for this repo in a fuzzy picker"),
                ("p", "open Pull Requests for this repo in a fuzzy picker"),
                ("o", "open repo home in browser"),
                ("y", "yank slug to clipboard"),
            ],
        ),
        (
            "Repo actions",
            &[
                ("f", "fetch (jj git fetch / git fetch)"),
                ("P", "push (jj git push / git push)"),
            ],
        ),
        (
            "Quit",
            &[
                ("? / F1", "toggle this help"),
                ("q / Esc", "quit"),
                ("Ctrl-C", "quit"),
            ],
        ),
    ];

    // `chars().count()` rather than `.len()`: the latter is byte
    // length, which for entries like `j / ↓` overcounts by two bytes
    // per arrow (UTF-8 encoding of `↓` is 3 bytes vs. 1 char). That
    // would push every other row out of alignment in the popup.
    let mut body: Vec<Line> = Vec::new();
    for (i, (heading, entries)) in sections.iter().enumerate() {
        if i > 0 {
            body.push(Line::from(""));
        }
        body.push(Line::from(Span::styled(
            format!("  ✦ {heading}"),
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )));
        let key_width = entries
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(6);
        for (key, desc) in entries.iter() {
            body.push(Line::from(vec![
                Span::styled(
                    format!("    {key:>w$}  ", w = key_width),
                    Style::default()
                        .fg(theme::YELLOW)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(theme::SUBTEXT)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE))
        .title(Span::styled(
            " 📚 shoka — keybinds ",
            Style::default()
                .fg(theme::LAVENDER)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::BASE));

    let para = Paragraph::new(body).block(block);

    f.render_widget(Clear, popup);
    f.render_widget(para, popup);
}

/// Build a centered popup rect taking `pct_x` × `pct_y` of `area`.
/// Standard `Layout` split — two vertical splits then two horizontal
/// splits — so the popup is genuinely centered on any terminal
/// size, not just the dev's.
///
/// Percentages are clamped to `[0, 100]` so a caller-bug like
/// `pct_x = 120` doesn't underflow `100 - pct_x` and panic in
/// debug builds. The current call site is hard-coded to safe
/// values, but defensive clamping makes the helper reusable
/// without surprising the next caller.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let pct_x = pct_x.min(100);
    let pct_y = pct_y.min(100);
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

/// Validate the current row → spawn a background fetch task →
/// install a [`PickerState::Loading`] popup the next frame draws.
/// The synchronous part (slug parse, host check) runs inline so an
/// obvious failure (`local/...` row, malformed slug) lands in
/// [`PickerState::Error`] immediately without spinning the user
/// through a fake "loading…" beat.
///
/// All success-path failure modes (no token, gh client build error,
/// API error) are caught inside the spawned task and forwarded as a
/// `String` over the oneshot channel — the event loop turns that
/// into [`PickerState::Error`] on its next poll. Result: the user
/// always sees *something* explaining what happened, never a
/// silently-stuck keystroke.
///
/// The task is detached but cancellable: the returned
/// [`tokio::task::AbortHandle`] is stored on
/// [`PickerState::Loading`] so [`handle_picker_key`]'s
/// Esc/q/Ctrl-C arms can stop the request mid-flight.
fn open_picker(app: &mut App, kind: PickerKind) {
    let Some(row_idx) = app.selected_row() else {
        return;
    };
    let row = &app.rows[row_idx];

    // Slug is `<host>/<owner>/<name>`. Anything else means a
    // malformed shelf entry — short-circuit with a message rather
    // than panic.
    let parts: Vec<&str> = row.slug.splitn(3, '/').collect();
    let &[host, owner, name] = parts.as_slice() else {
        app.picker = Some(Picker::error(
            kind,
            row.slug.clone(),
            format!("can't parse slug `{}`", row.slug),
        ));
        return;
    };
    let repo_label = format!("{host}/{owner}/{name}");

    if host != "github.com" {
        app.picker = Some(Picker::error(
            kind,
            repo_label,
            format!(
                "{} are only available on github.com (this repo is on `{host}`)",
                kind.title()
            ),
        ));
        return;
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let owner = owner.to_string();
    let name = name.to_string();
    let join = tokio::spawn(async move {
        // Caught failure: every step gets folded into the `Err(String)`
        // arm of the channel send so the wire format the event loop
        // sees is uniform. The `let _ =` swallows a send failure
        // (receiver dropped because the user closed the picker
        // before the fetch landed) — we have nothing to do about it.
        let result: PickerFetchResult = async {
            let Some(token) = crate::gh::resolve_token().await else {
                return Err("no GITHUB_TOKEN — set the env var or run `gh auth login`".to_string());
            };
            let client = crate::gh::build_client(&token)
                .map_err(|e| format!("gh client init failed: {e:#}"))?;
            let fetched = match kind {
                PickerKind::Issues => crate::gh::list_open_issues(&client, &owner, &name).await,
                PickerKind::Prs => crate::gh::list_open_prs(&client, &owner, &name).await,
            };
            fetched.map_err(|e| format!("{e:#}"))
        }
        .await;
        let _ = tx.send(result);
    });

    app.picker = Some(Picker {
        kind,
        repo_label,
        matcher: Matcher::default(),
        state: PickerState::Loading {
            started_at: std::time::Instant::now(),
            rx,
            abort: join.abort_handle(),
        },
    });
}

/// Dispatch a keystroke while the picker overlay is open. Mutates
/// `app.picker` (close on Esc/q, refilter on typing, move cursor,
/// hand off to `open::that` on Enter).
///
/// During [`PickerState::Loading`] only Esc / q are honoured —
/// j/k navigation and filter typing have nothing to operate on
/// yet, and ignoring them lets the user mash the keyboard without
/// the closing keystroke accidentally landing as a filter
/// character once the fetch resolves. The close arm aborts the
/// in-flight fetch task via the stored [`tokio::task::AbortHandle`]
/// so a slow gh request doesn't keep using bandwidth after the
/// user moves on.
fn handle_picker_key(app: &mut App, key: crossterm::event::KeyEvent) {
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // `Picker::drop` aborts the in-flight fetch task, so
            // dropping the picker here is enough — no manual
            // `abort_inflight` call needed.
            app.picker = None;
        }
        _ if picker.is_loading() => {
            // Swallow every other key while the spinner is up.
            // Nothing to navigate, nothing to filter, and a stray
            // Enter shouldn't queue up a browser launch the user
            // can't see yet.
        }
        KeyCode::Char('j') | KeyCode::Down => picker.move_down(),
        KeyCode::Char('k') | KeyCode::Up => picker.move_up(),
        KeyCode::Enter => {
            if let Some(item) = picker.selected() {
                let url = item.html_url.clone();
                // `open::that` blocks until the OS handler launches
                // (xdg-open / open / start), but those are fast and
                // detach immediately, so the UI freeze is sub-100ms.
                if let Err(e) = open::that(&url) {
                    tracing::warn!(
                        target: "shoka",
                        "failed to open {url} in browser: {e:#}"
                    );
                }
            }
            app.picker = None;
        }
        KeyCode::Backspace => {
            if let PickerState::Loaded { filter, .. } = &mut picker.state {
                filter.pop();
            }
            picker.refilter();
        }
        KeyCode::Char(c) => {
            if let PickerState::Loaded { filter, .. } = &mut picker.state {
                filter.push(c);
            }
            picker.refilter();
        }
        _ => {}
    }
}

/// Run a fetch or push on the currently-selected row. Like
/// `open_picker`, this blocks on tokio under `block_in_place` so the
/// existing sync TUI loop doesn't need restructuring. The captured
/// stdout / stderr land in an `ActionPopup` the next frame draws —
/// the UI freeze during the action is the cost of v1 simplicity,
/// matched to the freeze pickers already accept.
fn run_action_for_selected(app: &mut App, kind: ActionKind) {
    let Some(row_idx) = app.selected_row() else {
        return;
    };
    let row = &app.rows[row_idx];
    let repo_label = row.slug.clone();
    let path = row.path.clone();

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_action(&path, kind))
    });

    app.action_popup = Some(match result {
        Ok(outcome) => {
            // The action ran (fetch / push — whatever `kind` was) and
            // touched the local repo's git state: a fetch advances the
            // `behind` count, a push zeroes `ahead`. Re-capture *just
            // this row* so the dashboard's branch / ↑↓ / ✓ columns
            // reflect the new reality the instant the popup is
            // dismissed, and write it back to the cache so the next
            // `shoka tui` / `list` agrees — without re-walking the
            // whole shelf the way `cache refresh` does.
            refresh_row_status_after_action(app, row_idx);
            ActionPopup {
                kind,
                repo_label,
                outcome: Some(outcome),
                error: String::new(),
            }
        }
        Err(e) => ActionPopup {
            kind,
            repo_label,
            outcome: None,
            error: format!("{e:#}"),
        },
    });
}

/// Re-capture the git status of the row at `row_idx` after a fetch /
/// push and fold the fresh snapshot into both the in-memory row (so
/// the redraw behind the action popup is already current) and the
/// on-disk cache (so the change survives to the next run).
///
/// Scoped to the single acted-on repo by design: the user pressed
/// `f` / `P` on *one* row, so re-running a full-shelf `cache refresh`
/// would be wasteful — and slow on a large shelf. We rebuild the
/// `(host, owner, name, path?)` identity from the row and `upsert`
/// exactly that entry.
///
/// Best-effort: a capture or cache I/O failure is logged and
/// swallowed. The action itself already succeeded and its popup is
/// the user's primary feedback; a momentarily-stale status cell isn't
/// worth surfacing an error popup over (the next background refresh
/// will reconcile it regardless).
fn refresh_row_status_after_action(app: &mut App, row_idx: usize) {
    let path = app.rows[row_idx].path.clone();
    let snapshot = match git_status::capture(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "shoka",
                "tui: post-action status capture failed for {}: {e:#}",
                path.display()
            );
            return;
        }
    };

    // In-memory first: cheapest path to a correct redraw, and it
    // holds even if the cache write below fails.
    app.rows[row_idx].status = Some(snapshot.clone());

    let row = &app.rows[row_idx];
    let mut repo = Repo::new(row.host.as_str(), row.owner.as_str(), row.name.as_str());
    repo.path = row.repo_path.clone();

    match Cache::load(&app.paths) {
        Ok(mut cache) => {
            let entry = cache.upsert(&repo);
            entry.git_status = Some(snapshot);
            entry.last_refreshed = Some(current_unix_secs());
            if let Err(e) = cache.save(&app.paths) {
                tracing::warn!(target: "shoka", "tui: cache save after action failed: {e:#}");
            }
        }
        Err(e) => {
            tracing::warn!(target: "shoka", "tui: cache load after action failed: {e:#}");
        }
    }
}

/// Copy the selected row's slug (e.g. `github.com/owner/name`) to
/// the system clipboard. Failures surface as a status banner rather
/// than a popup — yanking is meant to be invisible-on-success, and
/// a wall of red on a missing clipboard daemon would just be noise.
fn yank_selected_slug(app: &mut App) {
    let Some(row_idx) = app.selected_row() else {
        return;
    };
    let slug = app.rows[row_idx].slug.clone();
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(slug.clone())) {
        Ok(()) => {
            app.status_message = Some(format!("yanked: {slug}"));
        }
        Err(e) => {
            app.status_message = Some(format!("yank failed: {e}"));
        }
    }
}

/// Open the selected row's repo home page in the user's default
/// browser. `host/owner/name` slugs translate directly to
/// `https://host/owner/name`, which is what every real host
/// (github.com, gitlab.com, codeberg.org…) serves. The synthetic
/// `local/...` host that `shoka import` mints for repos without a
/// remote has no web home, so short-circuit with a status note
/// rather than launch an inevitably-404 browser tab.
fn open_selected_repo_home(app: &mut App) {
    let Some(row_idx) = app.selected_row() else {
        return;
    };
    let slug = app.rows[row_idx].slug.clone();
    if slug.starts_with("local/") {
        app.status_message = Some("no repo home: local-only repo".into());
        return;
    }
    let url = format!("https://{slug}");
    match open::that(&url) {
        Ok(()) => {
            app.status_message = Some(format!("opened: {url}"));
        }
        Err(e) => {
            app.status_message = Some(format!("open failed: {e}"));
        }
    }
}

/// Render the action result popup. Two modes — error (no VCS
/// detected, spawn failed) shows a red banner + dismiss hint;
/// outcome (subprocess ran) shows the command, exit status, and
/// captured stdout/stderr. Either mode is dismissed by any key.
fn render_action_popup(f: &mut Frame, area: Rect, popup: &ActionPopup) {
    let rect = centered_rect(80, 60, area);
    f.render_widget(Clear, rect);

    let title = format!(" ⚙  {} — {} ", popup.kind.label(), popup.repo_label);
    let border_color = match &popup.outcome {
        Some(o) if o.success => theme::GREEN,
        Some(_) => theme::RED,
        None => theme::RED,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(ratatui::text::Span::styled(
            title,
            Style::default()
                .fg(theme::LAVENDER)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::BASE));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(outcome) = &popup.outcome {
        let status = if outcome.success {
            "✓ success"
        } else {
            "✗ failed"
        };
        let status_color = if outcome.success {
            theme::GREEN
        } else {
            theme::RED
        };
        lines.push(Line::from(vec![
            ratatui::text::Span::styled(
                format!("  {} ", outcome.vcs.label()),
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            ratatui::text::Span::styled(
                format!("$ {}", outcome.command),
                Style::default().fg(theme::TEXT),
            ),
        ]));
        lines.push(Line::from(ratatui::text::Span::styled(
            format!("  {status}"),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        if !outcome.stdout.trim().is_empty() {
            lines.push(Line::from(ratatui::text::Span::styled(
                "  stdout:",
                Style::default().fg(theme::SKY).add_modifier(Modifier::BOLD),
            )));
            for line in outcome.stdout.lines() {
                lines.push(Line::from(ratatui::text::Span::styled(
                    format!("    {line}"),
                    Style::default().fg(theme::SUBTEXT),
                )));
            }
        }
        if !outcome.stderr.trim().is_empty() {
            if !outcome.stdout.trim().is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(ratatui::text::Span::styled(
                "  stderr:",
                Style::default()
                    .fg(theme::PINK)
                    .add_modifier(Modifier::BOLD),
            )));
            for line in outcome.stderr.lines() {
                lines.push(Line::from(ratatui::text::Span::styled(
                    format!("    {line}"),
                    Style::default().fg(theme::SUBTEXT),
                )));
            }
        }
        if outcome.stdout.trim().is_empty() && outcome.stderr.trim().is_empty() {
            lines.push(Line::from(ratatui::text::Span::styled(
                "  (no output)",
                Style::default().fg(theme::OVERLAY),
            )));
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(ratatui::text::Span::styled(
            format!("  ⚠  {}", popup.error),
            Style::default().fg(theme::RED),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(ratatui::text::Span::styled(
        "  press any key to close",
        Style::default().fg(theme::OVERLAY),
    )));

    // Wrap long lines instead of truncating them at the popup edge —
    // git / jj can emit very long URLs, error messages, and progress
    // strings that would otherwise be silently clipped (no horizontal
    // scrollbar exists in ratatui). `trim: false` keeps leading indent
    // so the `    stdout:` / `    stderr:` indent stays readable.
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Braille spinner frames. The Catppuccin-friendly Unicode dot set
/// (every glyph is in the `U+28xx` block, all fixed-width) and a
/// 10-frame cycle gives a ~800 ms rotation at the 80 ms event-loop
/// poll cadence — fast enough to feel responsive without distracting.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_MS: u128 = 80;

/// Render the picker overlay. Always centered, always full-bleed
/// over the dashboard. Three modes — loading (centred spinner +
/// "fetching…" label), error (single line + dismiss hint), or
/// loaded (filter line + scrollable list).
fn render_picker(f: &mut Frame, area: Rect, picker: &Picker) {
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let title_glyph = match picker.kind {
        PickerKind::Issues => "🐛",
        PickerKind::Prs => "🔀",
    };
    let title = format!(
        " {title_glyph}  {} — {} ",
        picker.kind.title(),
        picker.repo_label
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE))
        .title(ratatui::text::Span::styled(
            title,
            Style::default()
                .fg(theme::LAVENDER)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::BASE));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    match &picker.state {
        PickerState::Loading { started_at, .. } => {
            // Spinner mode — centred message with a rotating
            // braille glyph. Frame derived from wall time so the
            // animation stays in step regardless of the event-loop's
            // wake jitter (a `frame_idx += 1` counter would skip on
            // a slow frame; modulo elapsed time is self-correcting).
            let elapsed = started_at.elapsed().as_millis();
            // Take the modulo in `u128` *before* narrowing to `usize`
            // so a long-running fetch on a 32-bit target can't
            // truncate the frame index before it's bounded.
            let frame = SPINNER_FRAMES
                [((elapsed / SPINNER_FRAME_MS) % SPINNER_FRAMES.len() as u128) as usize];
            let body = vec![
                Line::from(""),
                Line::from(vec![
                    ratatui::text::Span::styled(
                        format!("  {frame}  "),
                        Style::default()
                            .fg(theme::PEACH)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ratatui::text::Span::styled(
                        format!("fetching {}…", picker.kind.title().to_lowercase()),
                        Style::default().fg(theme::TEXT),
                    ),
                ]),
                Line::from(""),
                Line::from(ratatui::text::Span::styled(
                    "  esc / q to cancel",
                    Style::default().fg(theme::OVERLAY),
                )),
            ];
            f.render_widget(Paragraph::new(body), inner);
        }
        PickerState::Error(err) => {
            // Single-line error mode: render the message and a
            // dismiss hint, skip the list entirely. Keeps the
            // visual weight matched to what the user can act on.
            let body = vec![
                Line::from(""),
                Line::from(ratatui::text::Span::styled(
                    format!("  ⚠  {err}"),
                    Style::default().fg(theme::RED),
                )),
                Line::from(""),
                Line::from(ratatui::text::Span::styled(
                    "  esc / q to close",
                    Style::default().fg(theme::OVERLAY),
                )),
            ];
            f.render_widget(Paragraph::new(body), inner);
        }
        PickerState::Loaded {
            items,
            filter,
            matches,
            cursor,
            ..
        } => {
            render_picker_loaded(f, inner, items, filter, matches, *cursor);
        }
    }
}

/// Pulled out of `render_picker` so the busy `Loaded` arm stays
/// readable. Same as the v0.13.0 picker render minus the
/// pre-refactor field access shape.
fn render_picker_loaded(
    f: &mut Frame,
    inner: Rect,
    items: &[crate::gh::PickerItem],
    filter: &str,
    matches: &[usize],
    cursor: usize,
) {
    use ratatui::text::Span;

    // Layout: filter row (1 line) + list (rest minus 1) + footer (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    // Filter row — caret `▌` glyph indicates this is a live input.
    let filter_line = Line::from(vec![
        Span::styled(
            "  / ",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(filter, Style::default().fg(theme::YELLOW)),
        Span::styled(
            "▌",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(filter_line), chunks[0]);

    // List — show `cursor` highlight + truncate items past visible area.
    // We don't paginate; if a repo has 100+ items the user just narrows
    // with the filter (that's the whole point of the popup).
    let visible_h = chunks[1].height as usize;
    let total = matches.len();
    let scroll_top = cursor.saturating_sub(visible_h.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::with_capacity(total.min(visible_h));
    for (visual_idx, &item_idx) in matches.iter().enumerate().skip(scroll_top).take(visible_h) {
        let item = &items[item_idx];
        let is_cursor = visual_idx == cursor;
        let prefix = if is_cursor { " ▶ " } else { "   " };

        let mut spans: Vec<Span> = vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(theme::PEACH)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("#{:<5}  ", item.number),
                Style::default().fg(theme::MAUVE),
            ),
            Span::styled(&item.title, Style::default().fg(theme::TEXT)),
        ];
        if !item.labels.is_empty() {
            spans.push(Span::styled(
                format!("  [{}]", item.labels.join(",")),
                Style::default().fg(theme::TEAL),
            ));
        }

        let line_style = if is_cursor {
            Style::default()
                .bg(theme::SURFACE1)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(spans).style(line_style));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(theme::OVERLAY),
        )));
    }
    f.render_widget(Paragraph::new(lines), chunks[1]);

    // Footer — pill-style hints matching the main dashboard footer.
    f.render_widget(
        Paragraph::new(Line::from(footer_pills(&[
            ("j/k", "move"),
            ("type", "filter"),
            ("⏎", "open"),
            ("esc", "close"),
        ]))),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner derived from a `host/owner/name` slug. Test rows often
    /// use bare strings like `"alpha"` (no slashes), where the second
    /// segment doesn't exist — fall back to an empty owner there so
    /// the mine-only filter excludes the row only when own_owners is
    /// set, matching production semantics.
    fn owner_from_slug(slug: &str) -> String {
        slug.split('/').nth(1).unwrap_or("").to_string()
    }

    fn rows(slugs: &[&str]) -> Vec<DashRow> {
        slugs
            .iter()
            .map(|s| {
                let path = PathBuf::from("/tmp");
                let search_key = format!("{s} {}", path.display());
                let (host, name) = host_name_from_slug(s);
                DashRow {
                    slug: (*s).into(),
                    host,
                    owner: owner_from_slug(s),
                    name,
                    path,
                    repo_path: None,
                    search_key,
                    tags_display: String::new(),
                    status: None,
                    gh: None,
                }
            })
            .collect()
    }

    /// Split a `host/owner/name` slug into `(host, name)`, mirroring
    /// [`owner_from_slug`]'s lenient fallback so malformed fixtures
    /// don't panic — the cache-identity fields aren't what those
    /// fixtures exercise.
    fn host_name_from_slug(slug: &str) -> (String, String) {
        let mut parts = slug.split('/');
        let host = parts.next().unwrap_or("").to_string();
        let name = slug.rsplit('/').next().unwrap_or("").to_string();
        (host, name)
    }

    /// `rows()` variant that lets a test pin per-row `path` strings,
    /// so we can verify the filter scores against `slug + path`.
    fn rows_with_paths(items: &[(&str, &str)]) -> Vec<DashRow> {
        items
            .iter()
            .map(|(slug, path)| {
                let path = PathBuf::from(path);
                let search_key = format!("{slug} {}", path.display());
                let (host, name) = host_name_from_slug(slug);
                DashRow {
                    slug: (*slug).into(),
                    host,
                    owner: owner_from_slug(slug),
                    name,
                    path,
                    repo_path: None,
                    search_key,
                    tags_display: String::new(),
                    status: None,
                    gh: None,
                }
            })
            .collect()
    }

    /// Construct an `App` with no `own_owners` configured — the
    /// pre-mine-only default behaviour. Most existing tests assert
    /// the unscoped path, so funnelling them through this helper
    /// keeps their original semantics intact.
    fn app(rows: Vec<DashRow>) -> App {
        App::new(rows, Vec::new(), test_paths())
    }

    /// Serializes every test in this module that reads or writes a
    /// process-global environment variable through `ShokaPaths::
    /// resolve`. `cargo test` runs tests on multiple threads, and a
    /// concurrent getenv/setenv pair is a data race (UB — segfaults
    /// or flaky failures), so any code path that touches the env must
    /// hold this lock first.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Throwaway [`ShokaPaths`] for `App` construction in tests. The
    /// tests that exercise post-action behaviour never reach a
    /// successful capture (no-VCS temp dir / empty shelf), so these
    /// paths are never actually written to — `resolve(None)` just
    /// computes the OS default locations without creating anything.
    /// Holds [`ENV_LOCK`] because `resolve` reads `SHOKA_CACHE_DIR` /
    /// `SHOKA_STATE_DIR`, which another test may be mid-`set_var` on.
    fn test_paths() -> ShokaPaths {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ShokaPaths::resolve(None).expect("resolve default paths for test")
    }

    #[test]
    fn build_rows_splits_status_per_path_but_shares_gh_by_triple() {
        // Regression guard for the split-lookup contract introduced
        // by #57: per-checkout `git_status` must come from the
        // path-pinned cache entry (different value per row), while
        // `gh` is remote-derived and shared across siblings (the
        // first populated snapshot wins, even if it lives on a
        // different sibling than the row being rendered).
        use crate::cache::Cache;
        use crate::config::ShokaConfig;
        use crate::gh::{CiStatus, GhSnapshot};
        use crate::git_status::GitStatusSnapshot;
        use crate::state::{Repo, Shelf};

        let path_a = PathBuf::from("/home/u/a/shoka");
        let path_b = PathBuf::from("/home/u/b/shoka");

        // Shelf: two checkouts of the same remote at different
        // paths. Identical (host, owner, name) — only the path
        // distinguishes them.
        let mut shelf = Shelf::default();
        shelf
            .add(Repo::new("github.com", "yukimemi", "shoka").with_path(path_a.clone()))
            .unwrap();
        shelf
            .add(Repo::new("github.com", "yukimemi", "shoka").with_path(path_b.clone()))
            .unwrap();

        // Cache: two path-pinned rows. Different `git_status`
        // snapshots per row (so we can detect cross-contamination)
        // and `gh` populated only on row B (so we can prove the
        // lookup walked past row A's `None`).
        let status_a = GitStatusSnapshot {
            branch: Some("feat/a".into()),
            dirty: false,
            ahead: Some(1),
            behind: Some(0),
        };
        let status_b = GitStatusSnapshot {
            branch: Some("feat/b".into()),
            dirty: true,
            ahead: Some(0),
            behind: Some(2),
        };
        let shared_gh = GhSnapshot {
            open_pr_count: Some(7),
            ci_status: Some(CiStatus::Success),
            weekly_commits: Some(vec![1, 0, 3, 2]),
        };

        let mut cache = Cache::default();
        let row_a = cache.upsert(&shelf.repos[0]);
        row_a.git_status = Some(status_a.clone());
        row_a.gh = None;
        let row_b = cache.upsert(&shelf.repos[1]);
        row_b.git_status = Some(status_b.clone());
        row_b.gh = Some(shared_gh.clone());

        // `clone_path_for` early-returns `repo.path` for pinned
        // entries before consulting routes, so the root only needs
        // to satisfy `resolve()`'s "must be set" guard — it never
        // surfaces in the output rows for this test's path-pinned
        // shelf.
        let mut cfg = ShokaConfig::default();
        cfg.global.root = Some("/unused-for-pinned-rows".into());
        let resolved = cfg.resolve(None).expect("resolve");
        let rows = build_rows(&shelf, &resolved, &cache, &[]).expect("build rows");
        assert_eq!(rows.len(), 2);

        // Per-path `git_status`: each row sees its own snapshot
        // (not a cross-row leak). This is the bug PR #59 left open
        // and #57 explicitly resolves.
        assert_eq!(rows[0].status.as_ref(), Some(&status_a));
        assert_eq!(rows[1].status.as_ref(), Some(&status_b));

        // Triple-shared `gh`: both rows see the same snapshot —
        // and crucially row A sees row B's populated value (not
        // row A's own `None`), proving `find_gh_by_triple` walked
        // past the unpopulated sibling.
        assert_eq!(rows[0].gh.as_ref(), Some(&shared_gh));
        assert_eq!(rows[1].gh.as_ref(), Some(&shared_gh));
    }

    #[test]
    fn gh_cells_renders_dashes_when_snapshot_missing() {
        let (pr, ci) = gh_cells(None);
        assert_eq!(pr, "-");
        assert_eq!(ci, "-");
    }

    #[test]
    fn sparkline_none_or_empty_renders_dash() {
        // "never measured" — distinct from a dormant all-zero history.
        assert_eq!(sparkline(None), "-");
        assert_eq!(sparkline(Some(&[])), "-");
    }

    #[test]
    fn sparkline_all_zero_is_a_flat_floor() {
        // "measured, dormant" → lowest bar repeated, one per week, so
        // it reads differently from the `-` no-data case.
        assert_eq!(sparkline(Some(&[0, 0, 0])), "▁▁▁");
    }

    #[test]
    fn sparkline_normalises_to_the_rows_own_max() {
        // max maps to the top bar, 0 to the bottom, mid-range rounds
        // to the nearest bar. With max=8: 8→█(7), 4→▅(idx4), 0→▁(0).
        assert_eq!(sparkline(Some(&[0, 4, 8])), "▁▅█");
        // A different absolute scale with the same *shape* yields the
        // same sparkline — that's the per-row normalisation working:
        // an active mono-repo and a hobby repo read on their own axis.
        assert_eq!(sparkline(Some(&[0, 50, 100])), "▁▅█");
    }

    #[test]
    fn sparkline_one_char_per_week() {
        // Width tracks the input length (the column is sized for up to
        // ACTIVITY_WEEKS); a short young-repo history stays short.
        assert_eq!(sparkline(Some(&[1, 2, 3, 4, 5])).chars().count(), 5);
    }

    #[test]
    fn elide_slug_left_keeps_short_slug_unchanged() {
        assert_eq!(
            elide_slug_left("github.com/yukimemi/shoka", 30),
            "github.com/yukimemi/shoka"
        );
    }

    #[test]
    fn elide_slug_left_exact_fit_is_unchanged() {
        // "github.com/yukimemi/shoka" is 25 chars — exactly at the
        // budget means no elision (the `…` would only eat a char).
        assert_eq!(
            elide_slug_left("github.com/yukimemi/shoka", 25),
            "github.com/yukimemi/shoka"
        );
    }

    #[test]
    fn elide_slug_left_drops_the_prefix_not_the_name() {
        // The whole point of left-elision: every row shares the
        // `host/owner/` prefix, so the *name* tail is what
        // distinguishes them — it must survive truncation.
        assert_eq!(
            elide_slug_left("github.com/yukimemi/shoka", 15),
            "…yukimemi/shoka"
        );
        assert_eq!(elide_slug_left("github.com/yukimemi/shoka", 8), "…i/shoka");
    }

    #[test]
    fn elide_slug_left_handles_degenerate_widths() {
        assert_eq!(elide_slug_left("github.com/yukimemi/shoka", 1), "…");
        assert_eq!(elide_slug_left("github.com/yukimemi/shoka", 0), "");
    }

    #[test]
    fn repo_column_width_tracks_longest_slug_when_roomy() {
        // Wide terminal: the column hugs the longest slug (+1 for the
        // leading pad span) instead of hoarding a fixed percentage,
        // leaving the leftover width for the path column.
        assert_eq!(repo_column_width(200, 25), 26);
    }

    #[test]
    fn repo_column_width_clamps_so_path_keeps_its_minimum() {
        // Narrow terminal: everything except the repo column is
        // reserved (REPO_COL_RESERVED, which embeds the path column's
        // Min(20)) — the repo column gets whatever is left rather
        // than its full ask.
        let w = repo_column_width(110, 60);
        assert_eq!(w, 110 - REPO_COL_RESERVED);
        assert!(w < 61);
    }

    #[test]
    fn repo_column_width_never_collapses_below_floor() {
        // Degenerate terminal: keep a usable floor so `…/name` tails
        // stay legible; ratatui squeezes further on its own if even
        // the floor doesn't fit.
        assert_eq!(repo_column_width(40, 25), REPO_COL_FLOOR);
    }

    #[test]
    fn gh_cells_renders_count_and_status_glyph() {
        let snap = GhSnapshot {
            open_pr_count: Some(5),
            ci_status: Some(CiStatus::Success),
            weekly_commits: None,
        };
        let (pr, ci) = gh_cells(Some(&snap));
        assert_eq!(pr, "5");
        assert_eq!(ci, "✓");
    }

    #[test]
    fn gh_cells_clamps_pr_count_at_99_plus() {
        let snap = GhSnapshot {
            open_pr_count: Some(150),
            ci_status: None,
            weekly_commits: None,
        };
        let (pr, _) = gh_cells(Some(&snap));
        assert_eq!(pr, "99+");
    }

    #[test]
    fn gh_cells_distinguishes_zero_from_none() {
        // Zero PRs is a definite "no PRs"; None is "didn't check".
        // The cell strings must differ so the user can tell.
        let zero = GhSnapshot {
            open_pr_count: Some(0),
            ci_status: None,
            weekly_commits: None,
        };
        let (pr_zero, _) = gh_cells(Some(&zero));
        let (pr_none, _) = gh_cells(None);
        assert_eq!(pr_zero, "0");
        assert_eq!(pr_none, "-");
        assert_ne!(pr_zero, pr_none);
    }

    #[test]
    fn gh_cells_renders_each_ci_status() {
        for (status, expected) in [
            (CiStatus::Success, "✓"),
            (CiStatus::Failure, "✗"),
            (CiStatus::Pending, "◐"),
            (CiStatus::Skipped, "○"),
            (CiStatus::Other, "!"),
        ] {
            let snap = GhSnapshot {
                open_pr_count: None,
                ci_status: Some(status),
                weekly_commits: None,
            };
            let (_, ci) = gh_cells(Some(&snap));
            assert_eq!(ci, expected, "ci glyph for {status:?}");
        }
    }

    fn snap(
        branch: &str,
        ahead: Option<usize>,
        behind: Option<usize>,
        dirty: bool,
    ) -> GitStatusSnapshot {
        GitStatusSnapshot {
            branch: Some(branch.into()),
            dirty,
            ahead,
            behind,
        }
    }

    #[test]
    fn status_cells_renders_unknown_for_missing_snapshot() {
        let (b, ab, d) = status_cells(None);
        assert_eq!(b, "?");
        assert_eq!(ab, "?");
        assert_eq!(d, "?");
    }

    #[test]
    fn status_cells_renders_clean_branch_with_equal_marker() {
        let s = snap("main", Some(0), Some(0), false);
        let (b, ab, d) = status_cells(Some(&s));
        assert_eq!(b, "main");
        assert_eq!(ab, "=");
        assert_eq!(d, "✓");
    }

    #[test]
    fn status_cells_renders_ahead_only_and_behind_only() {
        let (_, ab_ahead, _) = status_cells(Some(&snap("x", Some(3), Some(0), false)));
        assert_eq!(ab_ahead, "↑3");
        let (_, ab_behind, _) = status_cells(Some(&snap("x", Some(0), Some(2), false)));
        assert_eq!(ab_behind, "↓2");
    }

    #[test]
    fn status_cells_renders_diverged_with_both_arrows() {
        let (_, ab, _) = status_cells(Some(&snap("x", Some(2), Some(5), false)));
        assert_eq!(ab, "↑2 ↓5");
    }

    #[test]
    fn status_cells_renders_dash_when_upstream_unknown() {
        // No upstream ref → both ahead/behind are None → dash.
        let (_, ab, _) = status_cells(Some(&snap("x", None, None, false)));
        assert_eq!(ab, "-");
    }

    #[test]
    fn status_cells_renders_dirty_glyph() {
        let (_, _, d) = status_cells(Some(&snap("x", Some(0), Some(0), true)));
        assert_eq!(d, "●");
    }

    #[test]
    fn refilter_empty_query_keeps_shelf_order() {
        let mut app = app(rows(&["alpha", "beta", "gamma"]));
        app.refilter();
        assert_eq!(app.matches, vec![0, 1, 2]);
    }

    #[test]
    fn refilter_narrows_to_matches() {
        let mut app = app(rows(&["alpha", "beta", "gamma", "alphabeta"]));
        app.filter = "alpha".into();
        app.refilter();
        // `alpha` and `alphabeta` should match; `beta` / `gamma` not.
        // Order is nucleo's call (score-descending); both should be
        // first or second.
        let matched_slugs: Vec<&str> = app
            .matches
            .iter()
            .map(|&i| app.rows[i].slug.as_str())
            .collect();
        assert!(
            matched_slugs.contains(&"alpha"),
            "missing alpha: {matched_slugs:?}"
        );
        assert!(
            matched_slugs.contains(&"alphabeta"),
            "missing alphabeta: {matched_slugs:?}"
        );
        assert!(
            !matched_slugs.contains(&"beta"),
            "beta shouldn't match: {matched_slugs:?}"
        );
        assert!(
            !matched_slugs.contains(&"gamma"),
            "gamma shouldn't match: {matched_slugs:?}"
        );
    }

    #[test]
    fn refilter_no_matches_leaves_empty() {
        let mut app = app(rows(&["alpha", "beta"]));
        app.filter = "zzzzz".into();
        app.refilter();
        assert!(app.matches.is_empty());
        assert_eq!(app.cursor, 0);
        assert!(app.table_state.selected().is_none());
    }

    #[test]
    fn refilter_matches_against_path_for_multi_clone_disambiguation() {
        // Two rows with the same slug but different on-disk paths.
        // Filtering by a substring that only appears in one path
        // must narrow to that row — that's how the user picks
        // between multiple checkouts of the same remote.
        let mut app = app(rows_with_paths(&[
            (
                "github.com/yukimemi/admintask",
                "/home/u/src/DeviceManagement",
            ),
            (
                "github.com/yukimemi/admintask",
                "/home/u/old/admintask-backup",
            ),
            ("github.com/yukimemi/shoka", "/home/u/src/shoka"),
        ]));
        app.filter = "backup".into();
        app.refilter();
        assert_eq!(
            app.matches.len(),
            1,
            "exactly one row should match `backup`: {:?}",
            app.matches
                .iter()
                .map(|&i| &app.rows[i].path)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            app.rows[app.matches[0]].path.to_string_lossy(),
            "/home/u/old/admintask-backup"
        );
    }

    #[test]
    fn refilter_resets_cursor_to_top() {
        let mut app = app(rows(&["a", "b", "c", "d"]));
        app.cursor = 3;
        app.refilter();
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn navigation_clamps_at_edges() {
        let mut app = app(rows(&["a", "b", "c"]));
        // Up at top stays at top (saturating_sub).
        app.move_up();
        assert_eq!(app.cursor, 0);
        // Down moves until the last row.
        app.move_down();
        app.move_down();
        app.move_down(); // would be 3, clamped to 2
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn navigation_on_empty_match_list_is_noop() {
        let mut app = app(rows(&[]));
        app.move_down();
        app.move_up();
        assert_eq!(app.cursor, 0);
        assert!(app.selected_row().is_none());
    }

    #[test]
    fn selected_row_returns_underlying_shelf_index() {
        // The TUI tracks cursor against `matches`, but the result
        // needs to be a shelf-relative index so the path emission
        // hits the right repo even when the filter reorders rows.
        let mut app = app(rows(&["zzz", "aaa"]));
        app.filter = "aaa".into();
        app.refilter();
        // After filtering only "aaa" remains, mapping to shelf idx 1.
        assert_eq!(app.selected_row(), Some(1));
    }

    #[test]
    fn mine_only_starts_active_when_own_owners_configured() {
        // Two repos on the shelf: one mine, one upstream. With
        // `own_owners = ["yukimemi"]` the dashboard should land on
        // mine-only by default so the user sees their shelf first
        // (the headline behaviour change of this feature).
        let rows = rows(&["github.com/yukimemi/shoka", "github.com/rust-lang/rust"]);
        let app = App::new(rows, vec!["yukimemi".into()], test_paths());
        assert!(app.mine_only, "should default to mine when own_owners set");
        let matched_slugs: Vec<&str> = app
            .matches
            .iter()
            .map(|&i| app.rows[i].slug.as_str())
            .collect();
        assert_eq!(matched_slugs, vec!["github.com/yukimemi/shoka"]);
    }

    #[test]
    fn mine_only_disabled_when_own_owners_empty() {
        // Without `own_owners`, behaviour matches the pre-mine-only
        // dashboard — every row visible from the start.
        let app = app(rows(&[
            "github.com/yukimemi/shoka",
            "github.com/rust-lang/rust",
        ]));
        assert!(!app.mine_only);
        assert_eq!(app.matches, vec![0, 1]);
    }

    #[test]
    fn toggle_mine_only_flips_visible_set() {
        let rows = rows(&["github.com/yukimemi/shoka", "github.com/rust-lang/rust"]);
        let mut app = App::new(rows, vec!["yukimemi".into()], test_paths());
        // Default = mine; toggle exposes everything.
        app.toggle_mine_only();
        assert!(!app.mine_only);
        assert_eq!(app.matches.len(), 2);
        // Toggle again returns to mine-only.
        app.toggle_mine_only();
        assert!(app.mine_only);
        assert_eq!(app.matches.len(), 1);
    }

    #[test]
    fn toggle_mine_only_is_noop_with_status_when_unconfigured() {
        // Pressing `m` without configured owners shouldn't silently
        // do nothing — surface a banner so the user knows why.
        let mut app = app(rows(&["github.com/yukimemi/shoka"]));
        app.toggle_mine_only();
        assert!(!app.mine_only, "should stay off when nothing to scope to");
        let msg = app.status_message.as_ref().expect("status banner set");
        assert!(
            msg.contains("own_owners"),
            "banner should point at the config key, got: {msg}"
        );
    }

    #[test]
    fn mine_only_matches_owner_case_insensitively() {
        // GitHub/GitLab owner names are case-insensitive — a config
        // of `["YukimemI"]` against the canonical lowercase slug
        // must still match. Without this, the silent-exclude bug
        // would let a typo in [ui].own_owners hide every "own" repo
        // with no diagnostic.
        let rows = rows(&["github.com/yukimemi/shoka", "github.com/RUST-LANG/rust"]);
        // Mixed-case configuration entries: prove matching folds
        // both directions (config-uppercase vs slug-lowercase, and
        // config-lowercase vs slug-uppercase).
        let app = App::new(
            rows,
            vec!["YukimemI".into(), "rust-lang".into()],
            test_paths(),
        );
        assert!(app.mine_only);
        let matched: Vec<&str> = app
            .matches
            .iter()
            .map(|&i| app.rows[i].slug.as_str())
            .collect();
        assert_eq!(matched.len(), 2, "both rows should match: {matched:?}");
        assert!(matched.contains(&"github.com/yukimemi/shoka"));
        assert!(matched.contains(&"github.com/RUST-LANG/rust"));
    }

    #[test]
    fn owner_in_is_case_insensitive() {
        // Direct unit test of the helper so a future caller can rely
        // on the fold without re-discovering it.
        let owners = vec!["yukimemi".into(), "Rust-Lang".into()];
        assert!(owner_in(&owners, "yukimemi"));
        assert!(owner_in(&owners, "YukimemI"));
        assert!(owner_in(&owners, "rust-lang"));
        assert!(owner_in(&owners, "RUST-LANG"));
        assert!(!owner_in(&owners, "someone-else"));
        assert!(!owner_in(&[], "anyone"));
    }

    #[test]
    fn mine_only_composes_with_text_filter() {
        // The fuzzy filter still runs on top of the mine-only gate —
        // typing a substring that matches the *non-mine* row must
        // produce zero matches, not jump to it.
        let rows = rows(&[
            "github.com/yukimemi/shoka",
            "github.com/yukimemi/renri",
            "github.com/rust-lang/rust",
        ]);
        let mut app = App::new(rows, vec!["yukimemi".into()], test_paths());
        app.filter = "rust".into();
        app.refilter();
        // The only `rust` row is rust-lang/rust, which mine-only
        // excludes. So matches should be empty even though the
        // search would otherwise hit.
        assert!(
            app.matches.is_empty(),
            "non-mine matches must be hidden: {:?}",
            app.matches
                .iter()
                .map(|&i| app.rows[i].slug.as_str())
                .collect::<Vec<_>>()
        );
    }

    fn picker_items(items: &[(u64, &str, &[&str])]) -> Vec<crate::gh::PickerItem> {
        items
            .iter()
            .map(|(n, t, ls)| crate::gh::PickerItem {
                number: *n,
                title: (*t).into(),
                html_url: format!("https://github.com/x/y/issues/{n}"),
                labels: ls.iter().map(|s| (*s).into()).collect(),
            })
            .collect()
    }

    #[test]
    fn picker_refilter_empty_query_keeps_identity_order() {
        let mut p = Picker::loaded(
            PickerKind::Issues,
            "x/y/z".into(),
            picker_items(&[(1, "alpha", &[]), (2, "beta", &[]), (3, "gamma", &[])]),
        );
        p.refilter();
        assert_eq!(picker_matches(&p), &[0, 1, 2]);
    }

    #[test]
    fn picker_refilter_narrows_against_title_and_labels() {
        let mut p = Picker::loaded(
            PickerKind::Issues,
            "x/y/z".into(),
            picker_items(&[
                (1, "broken thing", &["bug"]),
                (2, "happy path", &["enhancement"]),
                (3, "another bug-fix", &[]),
            ]),
        );
        if let PickerState::Loaded { filter, .. } = &mut p.state {
            *filter = "bug".into();
        }
        p.refilter();
        // Items 1 and 3 should match (1 via label, 3 via title);
        // item 2 should not. Order is nucleo's call (score desc).
        let PickerState::Loaded { matches, items, .. } = &p.state else {
            panic!("expected loaded picker")
        };
        let matched: Vec<u64> = matches.iter().map(|&i| items[i].number).collect();
        assert!(
            matched.contains(&1) && matched.contains(&3),
            "expected items 1 + 3 to match, got: {matched:?}"
        );
        assert!(
            !matched.contains(&2),
            "item 2 should not match `bug`, got: {matched:?}"
        );
    }

    #[test]
    fn picker_error_state_holds_message_and_blocks_selection() {
        // Error-mode picker doesn't expose a list — `selected()`
        // returns `None`, and `render_picker` takes the error
        // branch and never reaches the list code.
        let p = Picker::error(PickerKind::Prs, "github.com/x/y".into(), "no GITHUB_TOKEN");
        assert!(matches!(p.state, PickerState::Error(_)));
        assert_eq!(picker_error_message(&p), "no GITHUB_TOKEN");
        assert!(p.selected().is_none());
    }

    #[test]
    fn picker_search_key_includes_number_title_and_labels() {
        // The key is what nucleo scores against, so it has to mention
        // every searchable surface. A regression here would silently
        // make label / number queries miss.
        let item = crate::gh::PickerItem {
            number: 42,
            title: "fix the thing".into(),
            html_url: "https://github.com/x/y/issues/42".into(),
            labels: vec!["bug".into(), "p1".into()],
        };
        let key = item.search_key();
        assert!(key.contains("42"), "number missing: {key}");
        assert!(key.contains("fix the thing"), "title missing: {key}");
        assert!(key.contains("bug"), "label missing: {key}");
        assert!(key.contains("p1"), "label missing: {key}");
    }

    /// Helper: construct an `App` whose first row carries the given
    /// slug, so `open_picker`'s row-resolution path runs against a
    /// known input. The non-slug fields don't matter for the early-
    /// return branches we're testing.
    fn app_with_single_slug(slug: &str) -> App {
        app(rows(&[slug]))
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// Pull the error string out of a `Picker` for assertions. The
    /// `Loading` arm only appears when `open_picker` actually spawned
    /// a task (which means a tokio runtime is required), so any test
    /// that reaches this helper expects the sync short-circuit
    /// arm — anything else is an outright test bug.
    fn picker_error_message(picker: &Picker) -> &str {
        match &picker.state {
            PickerState::Error(msg) => msg.as_str(),
            other => panic!(
                "expected PickerState::Error, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    /// Pull the filter string out of a loaded `Picker` for the
    /// keystroke-dispatch tests.
    fn picker_filter(picker: &Picker) -> &str {
        match &picker.state {
            PickerState::Loaded { filter, .. } => filter.as_str(),
            other => panic!(
                "expected PickerState::Loaded, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    /// Pull the `matches` slice out of a loaded picker for the
    /// "refilter actually ran" assertion.
    fn picker_matches(picker: &Picker) -> &[usize] {
        match &picker.state {
            PickerState::Loaded { matches, .. } => matches,
            other => panic!(
                "expected PickerState::Loaded, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn open_picker_short_circuits_on_malformed_slug() {
        // A slug missing the host/owner/name shape should land us in
        // the error popup with a message that includes the bad slug,
        // not the live-fetch path (which would need network).
        let mut app = app_with_single_slug("no-slashes-here");
        open_picker(&mut app, PickerKind::Issues);
        let picker = app.picker.as_ref().expect("error picker installed");
        let err = picker_error_message(picker);
        assert!(
            err.contains("no-slashes-here"),
            "malformed-slug error should mention the slug, got: {err}"
        );
    }

    #[test]
    fn open_picker_short_circuits_on_non_github_host() {
        // `host = local` (from `shoka import` for a local-only repo)
        // and `host = gitlab.com` / etc. should all hit the same
        // "github only" branch before any network call. Pointing at
        // a clearly-distinct host keeps the assertion robust against
        // future tweaks to the message text.
        let mut app = app_with_single_slug("gitlab.com/some/proj");
        open_picker(&mut app, PickerKind::Prs);
        let picker = app.picker.as_ref().expect("error picker installed");
        let err = picker_error_message(picker);
        assert!(
            err.contains("github.com") && err.contains("gitlab.com"),
            "non-github error should mention both the requirement and the actual host, got: {err}"
        );
        assert!(
            err.contains(PickerKind::Prs.title()),
            "error should name the kind so the user knows what they tried to open, got: {err}"
        );
    }

    #[test]
    fn handle_picker_key_esc_closes_picker() {
        let mut app = app(rows(&[]));
        app.picker = Some(Picker::loaded(
            PickerKind::Issues,
            "github.com/x/y".into(),
            picker_items(&[(1, "alpha", &[])]),
        ));
        handle_picker_key(&mut app, key(KeyCode::Esc));
        assert!(app.picker.is_none(), "Esc should close the picker");
    }

    #[test]
    fn handle_picker_key_q_closes_picker() {
        let mut app = app(rows(&[]));
        app.picker = Some(Picker::loaded(
            PickerKind::Issues,
            "github.com/x/y".into(),
            picker_items(&[(1, "alpha", &[])]),
        ));
        handle_picker_key(&mut app, key(KeyCode::Char('q')));
        assert!(app.picker.is_none(), "q should close the picker");
    }

    #[test]
    fn handle_picker_key_enter_on_empty_items_closes_without_browser_launch() {
        // Enter with no `selected()` skips the `open::that` call
        // entirely (no subprocess fired) and still closes the popup.
        // Tests run in CI where launching a browser would be
        // useless at best and flaky at worst, so the empty-list path
        // is the right hook to assert "Enter does close" without
        // collateral effects.
        let mut app = app(rows(&[]));
        app.picker = Some(Picker::loaded(
            PickerKind::Issues,
            "github.com/x/y".into(),
            vec![],
        ));
        handle_picker_key(&mut app, key(KeyCode::Enter));
        assert!(
            app.picker.is_none(),
            "Enter should close the picker even with nothing to open"
        );
    }

    #[test]
    fn handle_picker_key_char_appends_to_filter_and_refilters() {
        let mut app = app(rows(&[]));
        app.picker = Some(Picker::loaded(
            PickerKind::Issues,
            "github.com/x/y".into(),
            picker_items(&[(1, "alpha", &[]), (2, "zeta", &[])]),
        ));
        handle_picker_key(&mut app, key(KeyCode::Char('a')));
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker_filter(picker), "a");
        // Both items contain `a` (`alpha` literally, `zeta` ends with
        // it), so matches is non-empty — the real assertion is just
        // that typing went through and refilter ran. Stronger orderings
        // belong in the dedicated refilter tests above.
        assert!(!picker_matches(picker).is_empty());
    }

    #[test]
    fn handle_picker_key_backspace_pops_and_refilters() {
        let mut app = app(rows(&[]));
        let mut p = Picker::loaded(
            PickerKind::Issues,
            "github.com/x/y".into(),
            picker_items(&[(1, "alpha", &[])]),
        );
        if let PickerState::Loaded { filter, .. } = &mut p.state {
            *filter = "ab".into();
        }
        p.refilter();
        app.picker = Some(p);
        handle_picker_key(&mut app, key(KeyCode::Backspace));
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker_filter(picker), "a");
    }

    #[test]
    fn loading_state_swallows_navigation_until_fetch_lands() {
        // Construct a Loading picker by hand (no spawn) using a
        // dropped sender — the receiver will return `Closed` on
        // poll, but until we call `poll_fetch` we're still in the
        // Loading arm. We then assert that j/k/Enter are no-ops
        // (matched by the loading guard in `handle_picker_key`)
        // while Esc still closes.
        let (_tx, rx) = tokio::sync::oneshot::channel::<PickerFetchResult>();
        // A no-op task we can hand an abort handle from.
        let join = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .spawn(async {});
        let picker = Picker {
            kind: PickerKind::Issues,
            repo_label: "github.com/x/y".into(),
            matcher: Matcher::default(),
            state: PickerState::Loading {
                started_at: std::time::Instant::now(),
                rx,
                abort: join.abort_handle(),
            },
        };

        let mut app = App::new(rows(&[]), Vec::new(), test_paths());
        app.picker = Some(picker);
        assert!(app.picker.as_ref().unwrap().is_loading());

        // j / Enter / typing — all should leave the picker installed
        // and still in the Loading state.
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Backspace,
        ] {
            handle_picker_key(&mut app, key(code));
            let picker = app.picker.as_ref().expect("picker survives ignored key");
            assert!(
                picker.is_loading(),
                "key {code:?} must not transition the picker out of Loading"
            );
        }

        // Esc closes the picker outright (and aborts the inflight
        // task — covered indirectly: if abort weren't called, the
        // tokio runtime would leak the task across test runs).
        handle_picker_key(&mut app, key(KeyCode::Esc));
        assert!(
            app.picker.is_none(),
            "Esc on Loading must still close the picker"
        );
    }

    #[test]
    fn poll_fetch_closed_channel_transitions_to_error() {
        // The fetch task panicked / was dropped before sending a
        // result. We model that with a sender that's dropped
        // immediately. `poll_fetch` should fold that into the
        // Error arm so the popup explains "something went wrong"
        // rather than spinning forever.
        let (tx, rx) = tokio::sync::oneshot::channel::<PickerFetchResult>();
        drop(tx);
        // Same "throwaway runtime for abort_handle" trick as the
        // loading-state test above.
        let join = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .spawn(async {});
        let mut picker = Picker {
            kind: PickerKind::Issues,
            repo_label: "github.com/x/y".into(),
            matcher: Matcher::default(),
            state: PickerState::Loading {
                started_at: std::time::Instant::now(),
                rx,
                abort: join.abort_handle(),
            },
        };
        assert!(picker.poll_fetch(), "closed channel must transition state");
        assert!(matches!(picker.state, PickerState::Error(_)));
    }

    #[test]
    fn run_action_for_selected_noop_when_no_selection() {
        // Empty shelf → `selected_row()` is `None`, the action
        // function early-returns without spawning a subprocess
        // (which would be wrong: there's no row to act on) and
        // crucially without installing a popup. The dashboard stays
        // exactly as it was.
        let mut app = app(rows(&[]));
        run_action_for_selected(&mut app, ActionKind::Fetch);
        assert!(
            app.action_popup.is_none(),
            "no row selected → no popup should be set"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_action_for_selected_records_error_when_no_vcs() {
        // Real path that has neither `.jj/` nor `.git/`. `run_action`
        // returns `Err`, which `run_action_for_selected` should fold
        // into a popup with `outcome: None` + a non-empty error
        // message — never panic, never silently swallow.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let search_key = format!("local/test/repo {}", path.display());
        let row = DashRow {
            slug: "local/test/repo".into(),
            host: "local".into(),
            owner: "test".into(),
            name: "repo".into(),
            path,
            repo_path: None,
            search_key,
            tags_display: String::new(),
            status: None,
            gh: None,
        };
        let mut app = app(vec![row]);
        run_action_for_selected(&mut app, ActionKind::Fetch);
        let popup = app.action_popup.as_ref().expect("popup installed");
        assert!(
            popup.outcome.is_none(),
            "no-VCS branch should leave `outcome` None to drive the error styling"
        );
        assert!(
            !popup.error.is_empty(),
            "error message must be populated so the user sees why the action failed"
        );
        assert_eq!(popup.kind, ActionKind::Fetch);
        assert_eq!(popup.repo_label, "local/test/repo");
    }

    #[test]
    fn refresh_row_status_after_action_updates_row_and_cache() {
        // The headline behaviour: after a fetch / push lands, the
        // acted-on row's status must be re-captured *in memory* (so
        // the dashboard redraw behind the popup is already current)
        // *and* written back to `cache.toml` (so the next `shoka tui`
        // / `list` agrees) — without re-walking the rest of the shelf.
        use crate::cache::Cache;

        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        // A real (if empty) git repo so `git_status::capture` returns
        // Ok rather than erroring out the way a bare temp dir would.
        gix::init(&repo_root).expect("gix init");

        // Redirect the cache file into the tempdir. The env var is set
        // only across `resolve()` — `ShokaPaths` owns `cache_dir` once
        // resolved, so later load/save don't re-read the env and the
        // global mutation window stays as small as possible. Holding
        // `ENV_LOCK` for that window serializes against any concurrent
        // `test_paths()` resolve so the getenv/setenv pair isn't a
        // data race under multi-threaded `cargo test`.
        let cache_dir = tmp.path().join("cache");
        let paths = {
            let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("SHOKA_CACHE_DIR");
            unsafe {
                std::env::set_var("SHOKA_CACHE_DIR", &cache_dir);
                let p = ShokaPaths::resolve(None).expect("resolve paths");
                match &prev {
                    Some(v) => std::env::set_var("SHOKA_CACHE_DIR", v),
                    None => std::env::remove_var("SHOKA_CACHE_DIR"),
                }
                p
            }
        };

        let search_key = format!("github.com/yukimemi/repo {}", repo_root.display());
        let row = DashRow {
            slug: "github.com/yukimemi/repo".into(),
            host: "github.com".into(),
            owner: "yukimemi".into(),
            name: "repo".into(),
            path: repo_root.clone(),
            repo_path: None,
            search_key,
            tags_display: String::new(),
            status: None,
            gh: None,
        };
        let mut app = App::new(vec![row], Vec::new(), paths.clone());
        assert!(
            app.rows[0].status.is_none(),
            "precondition: no snapshot yet"
        );

        refresh_row_status_after_action(&mut app, 0);

        assert!(
            app.rows[0].status.is_some(),
            "row status must be recaptured in memory so the redraw is current"
        );

        let cache = Cache::load(&paths).expect("load cache");
        let entry = cache
            .find("github.com", "yukimemi", "repo", None)
            .expect("cache entry written for the acted-on repo");
        assert!(
            entry.git_status.is_some(),
            "git_status must be persisted for the next run"
        );
        assert!(
            entry.last_refreshed.is_some(),
            "last_refreshed must be stamped so the TTL logic sees a fresh entry"
        );
        // Only the acted-on repo is touched — no full-shelf walk.
        assert_eq!(cache.len(), 1, "exactly one repo's cache entry written");
    }

    #[test]
    fn yank_selected_slug_noop_when_no_selection() {
        // Empty shelf → no row, so the clipboard is never touched and
        // no status banner is set. Important because arboard's
        // `Clipboard::new()` can fail on headless CI runners, and we
        // don't want a stray `y` keystroke (e.g. while debugging an
        // empty shelf) to surface a confusing error message.
        let mut app = app(rows(&[]));
        yank_selected_slug(&mut app);
        assert!(
            app.status_message.is_none(),
            "no row selected → no status message should be set"
        );
    }

    #[test]
    fn open_selected_repo_home_short_circuits_on_local_host() {
        // `host = local` is the synthetic slug shoka mints for
        // jj-only / no-remote repos via `shoka import`. There's no
        // web home for these, so the open helper must surface a
        // status note instead of spawning a browser to
        // `https://local/...` (which would 404 — annoying, not
        // dangerous, but pointless).
        let mut app = app_with_single_slug("local/scratch/notes");
        open_selected_repo_home(&mut app);
        let msg = app
            .status_message
            .as_ref()
            .expect("status banner installed");
        assert!(
            msg.contains("local-only"),
            "local-host status banner should mention the cause, got: {msg}"
        );
    }

    #[test]
    fn open_selected_repo_home_noop_when_no_selection() {
        // Empty shelf → no row, so neither the browser nor the
        // status banner should fire. Defensive: a stray `o` on an
        // empty dashboard shouldn't open a `https:///` tab.
        let mut app = app(rows(&[]));
        open_selected_repo_home(&mut app);
        assert!(
            app.status_message.is_none(),
            "no row selected → no status message should be set"
        );
    }
}
