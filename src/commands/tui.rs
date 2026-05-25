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
//! 2. Table — slug / path / tags, current selection highlighted.
//! 3. Footer — mode-specific key hints, or the live filter input.
//!
//! What's intentionally **not** here yet:
//!
//! - **Git status snapshot.** `branch / ahead-behind / dirty / open
//!   PRs / CI` come from the cache layer, which doesn't yet collect
//!   them. That work belongs in a follow-up PR that extends
//!   `cache refresh` to capture the snapshot via `gix::status` +
//!   `octocrab`; this PR plumbs through the table data model so the
//!   columns can be added without restructuring.
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
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, TableState};
use ratatui::{Terminal, prelude::Backend};
use teravars::Engine;

use crate::cli::TuiArgs;
use crate::commands::ShokaContext;
use crate::commands::cd::emit_path;
use crate::config::{ResolvedConfig, ShokaConfig};
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

    let rows = build_rows(&shelf, &resolved, &args.tags)?;
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
}

fn build_rows(
    shelf: &Shelf,
    resolved: &ResolvedConfig,
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
        out.push(DashRow {
            slug: repo.slug(),
            path,
            tags_display: repo.tags.join(", "),
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
    table_state: TableState,
    matcher: Matcher,
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
    let header_row = Row::new(vec!["repo", "path", "tags"]).style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Yellow),
    );

    let rows: Vec<Row> = app
        .matches
        .iter()
        .map(|&idx| {
            let row = &app.rows[idx];
            Row::new(vec![
                row.slug.clone(),
                row.path.display().to_string(),
                row.tags_display.clone(),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(35),
        Constraint::Percentage(50),
        Constraint::Percentage(15),
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

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let hint = match app.mode {
        Mode::Normal => "j/k=move  g/G=top/bot  /=filter  enter=cd  q=quit",
        Mode::Filter => "type to filter  ⌫=delete  enter=accept  esc=clear",
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        area,
    );
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
            })
            .collect()
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
}
