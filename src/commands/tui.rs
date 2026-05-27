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
//! What's intentionally **not** here yet:
//!
//! - **Multi-select / bulk ops.** The TUI is currently a fancy `cd`
//!   picker. Bulk `exec --tag X -- ...` would be a natural extension.
//! - **OSC 7 cwd hint.** Phase 3 polish.

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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState};
use ratatui::{Terminal, prelude::Backend};
use teravars::Engine;

use crate::cache::Cache;
use crate::cli::TuiArgs;
use crate::commands::ShokaContext;
use crate::commands::cd::emit_path;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::gh::{CiStatus, GhSnapshot};
use crate::git_status::GitStatusSnapshot;
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

    let mut app = App::new(rows);
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
    path: PathBuf,
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
        let cache_entry = cache.find(&repo.host, &repo.owner, &repo.name);
        let status = cache_entry.and_then(|c| c.git_status.clone());
        let gh = cache_entry.and_then(|c| c.gh.clone());
        out.push(DashRow {
            slug: repo.slug(),
            path,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
}

struct App {
    rows: Vec<DashRow>,
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
    table_state: TableState,
    matcher: Matcher,
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

/// Picker overlay state. Either showing live data ([`Self::loaded`])
/// or a single-line failure message ([`Self::error`]) — both render
/// in the same popup so the user always sees *something* explaining
/// why they pressed the key.
struct Picker {
    kind: PickerKind,
    /// Display label for the repo the picker was opened on, e.g.
    /// `github.com/yukimemi/shoka`. Shown in the popup title so
    /// the user can tell which row they triggered against.
    repo_label: String,
    /// Items fetched from gh. Empty when `error` is `Some` or when
    /// the repo legitimately has no open issues / PRs.
    items: Vec<crate::gh::PickerItem>,
    /// Precomputed [`crate::gh::PickerItem::search_key`] per item.
    /// Built once at construction so the hot path in [`refilter`]
    /// (one nucleo scoring round per item per keystroke) doesn't
    /// reallocate a `String` for every entry every time the user
    /// types a character.
    search_keys: Vec<String>,
    /// Live filter query.
    filter: String,
    /// Indices into `items`, score-sorted (highest first).
    matches: Vec<usize>,
    /// Position within `matches` of the highlighted row.
    cursor: usize,
    /// When `Some`, the popup renders just the message (no list,
    /// no filter). Drives the "no token" / "non-github host" /
    /// "fetch errored" branches.
    error: Option<String>,
    matcher: Matcher,
}

impl Picker {
    fn loaded(kind: PickerKind, repo_label: String, items: Vec<crate::gh::PickerItem>) -> Self {
        let matches = (0..items.len()).collect();
        let search_keys = items.iter().map(|i| i.search_key()).collect();
        Self {
            kind,
            repo_label,
            items,
            search_keys,
            filter: String::new(),
            matches,
            cursor: 0,
            error: None,
            matcher: Matcher::default(),
        }
    }

    fn error(kind: PickerKind, repo_label: String, msg: impl Into<String>) -> Self {
        Self {
            kind,
            repo_label,
            items: Vec::new(),
            search_keys: Vec::new(),
            filter: String::new(),
            matches: Vec::new(),
            cursor: 0,
            error: Some(msg.into()),
            matcher: Matcher::default(),
        }
    }

    /// Re-rank items against the current filter. Empty filter =
    /// identity order (as returned by the gh API, which is
    /// most-recently-updated first); a non-empty filter scores each
    /// item's precomputed `search_keys` entry via nucleo and keeps
    /// positive-score matches sorted descending.
    fn refilter(&mut self) {
        if self.filter.is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
            let mut scored: Vec<(usize, u32)> = Vec::new();
            let mut buf: Vec<char> = Vec::new();
            for (idx, key) in self.search_keys.iter().enumerate() {
                buf.clear();
                let haystack = nucleo::Utf32Str::new(key, &mut buf);
                if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                    scored.push((idx, score));
                }
            }
            scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
            self.matches = scored.into_iter().map(|(idx, _)| idx).collect();
        }
        self.cursor = 0;
    }

    fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1).min(self.matches.len() - 1);
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// The currently-highlighted item, if any. `None` when the
    /// list is empty (no matches, no fetched items, or error mode).
    fn selected(&self) -> Option<&crate::gh::PickerItem> {
        let idx = *self.matches.get(self.cursor)?;
        self.items.get(idx)
    }
}

impl App {
    fn new(rows: Vec<DashRow>) -> Self {
        let matches = (0..rows.len()).collect();
        let mut table_state = TableState::default();
        table_state.select(if rows.is_empty() { None } else { Some(0) });
        Self {
            rows,
            filter: String::new(),
            matches,
            cursor: 0,
            mode: Mode::Normal,
            show_help: false,
            picker: None,
            table_state,
            matcher: Matcher::default(),
        }
    }

    /// Recompute `matches` against `filter`. Empty filter = identity
    /// (everything in shelf order); otherwise nucleo scores each
    /// row's slug and we keep the matches sorted by score
    /// descending. Cursor pins to the top so the highlighted row is
    /// always visible after a refilter.
    fn refilter(&mut self) {
        if self.filter.is_empty() {
            self.matches = (0..self.rows.len()).collect();
        } else {
            let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
            let mut scored: Vec<(usize, u32)> = Vec::new();
            let mut buf: Vec<char> = Vec::new();
            for (idx, row) in self.rows.iter().enumerate() {
                buf.clear();
                let haystack = nucleo::Utf32Str::new(&row.slug, &mut buf);
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
        // Best-effort cleanup — there's nothing we can do if any of
        // these fail (no panic-in-drop), and the user is going to
        // get their terminal back one way or another. Print cursor
        // show explicitly: LeaveAlternateScreen restores the main
        // buffer but the cursor visibility flag persists, and we
        // hid it via TableState rendering.
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
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
        // ratatui's `Backend::Error` is not always `std::error::Error +
        // Send + Sync` (depends on the backend), so anyhow's
        // `.context` blanket impl doesn't apply. Convert manually.
        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow::anyhow!("drawing frame: {e}"))?;

        // Block on `read()` rather than polling: there's no
        // animation or background work to drive, and Ctrl-C arrives
        // as a `KeyEvent` under crossterm's raw mode (not a signal),
        // so we're never stranded waiting. The polled variant
        // burned CPU + wall-time for no UX gain.
        let evt = event::read().context("reading event")?;
        let Event::Key(key) = evt else { continue };
        if key.kind == KeyEventKind::Release {
            // Windows fires Press + Release; we only act on Press so
            // a single physical keystroke doesn't double-fire.
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
            Mode::Normal => match key.code {
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
            },
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
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let total = app.rows.len();
    let shown = app.matches.len();
    let filter_part = if app.filter.is_empty() {
        String::new()
    } else {
        format!("  /{}", app.filter)
    };
    let line = Line::from(format!("shoka — {shown}/{total} repo(s){filter_part}"))
        .style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(Paragraph::new(line), area);
}

fn render_table(f: &mut Frame, area: Rect, app: &mut App) {
    let header_row = Row::new(vec![
        "repo", "branch", "↑↓", "✓", "PR", "CI", "path", "tags",
    ])
    .style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Yellow),
    );

    let rows: Vec<Row> = app
        .matches
        .iter()
        .map(|&idx| {
            let row = &app.rows[idx];
            let (branch, ahead_behind, dirty) = status_cells(row.status.as_ref());
            let (pr, ci) = gh_cells(row.gh.as_ref());
            Row::new(vec![
                row.slug.clone(),
                branch,
                ahead_behind,
                dirty,
                pr,
                ci,
                row.path.display().to_string(),
                row.tags_display.clone(),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(28), // repo
        Constraint::Length(14),     // branch
        Constraint::Length(8),      // ↑N ↓N
        Constraint::Length(2),      // dirty glyph
        Constraint::Length(4),      // PR count (e.g. "99+")
        Constraint::Length(2),      // CI glyph
        Constraint::Min(20),        // path
        Constraint::Length(14),     // tags
    ];
    let table = Table::new(rows, widths)
        .header(header_row)
        .row_highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    // Pass the real `TableState` by `&mut` so the scroll offset
    // ratatui computes (for keeping the highlighted row visible as
    // the cursor moves off-screen) actually persists across frames.
    // Cloning here would discard those mutations and break long-
    // shelf scrolling.
    f.render_stateful_widget(table, area, &mut app.table_state);
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
    let hint = match app.mode {
        Mode::Normal => "j/k=move  /=filter  enter=cd  i=issues  p=PRs  ?=help  q=quit",
        Mode::Filter => "type to filter  ⌫=delete  enter=accept  esc=clear",
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        area,
    );
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
    let popup = centered_rect(60, 70, area);

    // List entries chosen for screen-readability: the key column is
    // right-aligned in a fixed width so the descriptions line up.
    let entries: [(&str, &str); 11] = [
        ("j / ↓", "move down"),
        ("k / ↑", "move up"),
        ("g", "jump to top"),
        ("G", "jump to bottom"),
        ("/", "filter — type to narrow, esc to clear"),
        ("Enter", "select (emit path for the shell wrapper to cd)"),
        ("i", "open Issues for this repo in a fuzzy picker"),
        ("p", "open Pull Requests for this repo in a fuzzy picker"),
        ("? / F1", "toggle this help"),
        ("q / Esc", "quit"),
        ("Ctrl-C", "quit"),
    ];

    // `chars().count()` rather than `.len()`: the latter is byte
    // length, which for entries like `j / ↓` overcounts by two bytes
    // per arrow (UTF-8 encoding of `↓` is 3 bytes vs. 1 char). That
    // would push every other row out of alignment in the popup.
    let key_width = entries
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8);
    let body: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                ratatui::text::Span::styled(
                    format!("  {key:>w$}  ", w = key_width),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                ratatui::text::Span::raw(*desc),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" shoka — keybinds ")
        .title_style(Style::default().add_modifier(Modifier::BOLD));

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

/// Resolve the current row → octocrab client → list_open_issues /
/// list_open_prs (synchronous: `block_on` inside `block_in_place`,
/// safe under the multi-threaded tokio runtime shoka's `main` uses).
/// All failure modes (no selected row, non-github host, missing
/// token, gh client build error, API error) collapse to a single
/// `Picker::error(...)` so the user always gets a popup that
/// explains what happened instead of a silently-stuck keystroke.
///
/// Blocking the runtime here makes the UI freeze briefly during the
/// fetch — acceptable for a v1 picker, since the call is bounded by
/// `per_page(100)` plus normal network RTT. A future polish PR can
/// move this to a background tokio task + spinner without
/// restructuring callers.
fn open_picker(app: &mut App, kind: PickerKind) {
    let Some(row_idx) = app.selected_row() else {
        return;
    };
    let row = &app.rows[row_idx];

    // Slug is `<host>/<owner>/<name>`. Anything else means a malformed
    // shelf entry — short-circuit with a message rather than panic.
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

    // Resolve token + build client + fetch, all under one block_on
    // so we don't fragment the runtime stack. block_in_place lets
    // the multi-threaded scheduler reuse this worker thread while
    // we block, rather than stalling other tasks.
    let fetched = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(token) = crate::gh::resolve_token().await else {
                return Err(anyhow::anyhow!(
                    "no GITHUB_TOKEN — set the env var or run `gh auth login`"
                ));
            };
            let client = crate::gh::build_client(&token)
                .map_err(|e| anyhow::anyhow!("gh client init failed: {e:#}"))?;
            match kind {
                PickerKind::Issues => crate::gh::list_open_issues(&client, owner, name).await,
                PickerKind::Prs => crate::gh::list_open_prs(&client, owner, name).await,
            }
        })
    });

    let picker = match fetched {
        Ok(items) => Picker::loaded(kind, repo_label, items),
        Err(e) => Picker::error(kind, repo_label, format!("{e:#}")),
    };
    app.picker = Some(picker);
}

/// Dispatch a keystroke while the picker overlay is open. Mutates
/// `app.picker` (close on Esc/q, refilter on typing, move cursor,
/// hand off to `open::that` on Enter).
fn handle_picker_key(app: &mut App, key: crossterm::event::KeyEvent) {
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.picker = None;
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
            picker.filter.pop();
            picker.refilter();
        }
        KeyCode::Char(c) => {
            picker.filter.push(c);
            picker.refilter();
        }
        _ => {}
    }
}

/// Render the picker overlay. Always centered, always full-bleed
/// over the dashboard. Three modes — error (single line + dismiss
/// hint), empty list (no items match the current filter), populated
/// list (filter line + scrollable items).
fn render_picker(f: &mut Frame, area: Rect, picker: &Picker) {
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let title = format!(" {} — {} ", picker.kind.title(), picker.repo_label);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Single-line error mode: render the message and a dismiss hint,
    // skip the list entirely. Keeps the visual weight matched to
    // what the user can act on.
    if let Some(err) = &picker.error {
        let body = vec![
            Line::from(""),
            Line::from(ratatui::text::Span::styled(
                format!("  ⚠  {err}"),
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(ratatui::text::Span::styled(
                "  esc / q to close",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body), inner);
        return;
    }

    // Layout: filter row (1 line) + list (rest minus 1) + footer (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    // Filter row — cursor `_` glyph indicates this is a live input.
    let filter_line = Line::from(vec![
        ratatui::text::Span::styled(
            "  / ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        ratatui::text::Span::raw(picker.filter.clone()),
        ratatui::text::Span::styled("_", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(filter_line), chunks[0]);

    // List — show `cursor` highlight + truncate items past visible area.
    // We don't paginate; if a repo has 100+ items the user just narrows
    // with the filter (that's the whole point of the popup).
    let visible_h = chunks[1].height as usize;
    let total = picker.matches.len();
    let scroll_top = picker.cursor.saturating_sub(visible_h.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::with_capacity(total.min(visible_h));
    for (visual_idx, &item_idx) in picker
        .matches
        .iter()
        .enumerate()
        .skip(scroll_top)
        .take(visible_h)
    {
        let item = &picker.items[item_idx];
        let is_cursor = visual_idx == picker.cursor;
        let prefix = if is_cursor { "▶ " } else { "  " };
        let labels = if item.labels.is_empty() {
            String::new()
        } else {
            format!("  [{}]", item.labels.join(","))
        };
        let row_text = format!("{prefix}#{:<5}  {}{}", item.number, item.title, labels);
        let style = if is_cursor {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(ratatui::text::Span::styled(row_text, style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(ratatui::text::Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), chunks[1]);

    // Footer.
    let footer = Line::from(ratatui::text::Span::styled(
        "  j/k=move  type=filter  enter=open in browser  esc=close",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(slugs: &[&str]) -> Vec<DashRow> {
        slugs
            .iter()
            .map(|s| DashRow {
                slug: (*s).into(),
                path: PathBuf::from("/tmp"),
                tags_display: String::new(),
                status: None,
                gh: None,
            })
            .collect()
    }

    #[test]
    fn gh_cells_renders_dashes_when_snapshot_missing() {
        let (pr, ci) = gh_cells(None);
        assert_eq!(pr, "-");
        assert_eq!(ci, "-");
    }

    #[test]
    fn gh_cells_renders_count_and_status_glyph() {
        let snap = GhSnapshot {
            open_pr_count: Some(5),
            ci_status: Some(CiStatus::Success),
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
        let mut app = App::new(rows(&["alpha", "beta", "gamma"]));
        app.refilter();
        assert_eq!(app.matches, vec![0, 1, 2]);
    }

    #[test]
    fn refilter_narrows_to_matches() {
        let mut app = App::new(rows(&["alpha", "beta", "gamma", "alphabeta"]));
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
        let mut app = App::new(rows(&["alpha", "beta"]));
        app.filter = "zzzzz".into();
        app.refilter();
        assert!(app.matches.is_empty());
        assert_eq!(app.cursor, 0);
        assert!(app.table_state.selected().is_none());
    }

    #[test]
    fn refilter_resets_cursor_to_top() {
        let mut app = App::new(rows(&["a", "b", "c", "d"]));
        app.cursor = 3;
        app.refilter();
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn navigation_clamps_at_edges() {
        let mut app = App::new(rows(&["a", "b", "c"]));
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
        let mut app = App::new(rows(&[]));
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
        let mut app = App::new(rows(&["zzz", "aaa"]));
        app.filter = "aaa".into();
        app.refilter();
        // After filtering only "aaa" remains, mapping to shelf idx 1.
        assert_eq!(app.selected_row(), Some(1));
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
        assert_eq!(p.matches, vec![0, 1, 2]);
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
        p.filter = "bug".into();
        p.refilter();
        // Items 1 and 3 should match (1 via label, 3 via title);
        // item 2 should not. Order is nucleo's call (score desc).
        let matched: Vec<u64> = p.matches.iter().map(|&i| p.items[i].number).collect();
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
    fn picker_error_renders_with_no_items() {
        // Error-mode picker has an empty items list and a non-None
        // error field. `selected()` returns None — render_picker
        // takes the error branch and never reaches the list code.
        let p = Picker::error(PickerKind::Prs, "github.com/x/y".into(), "no GITHUB_TOKEN");
        assert!(p.items.is_empty());
        assert_eq!(p.error.as_deref(), Some("no GITHUB_TOKEN"));
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
}
