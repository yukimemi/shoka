//! `shoka list` — show what's on the shelf.
//!
//! Reads [`Shelf`] from state.toml, applies the requested filters
//! (`--tag` AND-semantics, `--has-agents`), optionally sorts
//! (`--sort name|stars`) and prints the result.
//!
//! With `--sort`, repos with a cached GitHub snapshot (`shoka cache refresh`) are
//! rendered as one aligned line — `host/owner/name  ★ 24  Rust` — so
//! `shoka list --sort stars | head -5` yields five repos. Repos
//! without cached star data keep the multi-line format with indented
//! path / tags / vcs / note. `list` never touches the network: all
//! GitHub data comes from `cache.toml`.

use std::cmp::Ordering;
use std::io::IsTerminal;

use anyhow::Result;
use owo_colors::OwoColorize;
use teravars::Engine;
use unicode_width::UnicodeWidthStr;

use crate::cache::Cache;
use crate::cli::{ListArgs, ListSort};
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::state::{Repo, Shelf};

/// One shown repo plus whatever the cache knows about it.
#[derive(Debug)]
struct ListRow<'s> {
    repo: &'s Repo,
    /// `Some` ⇔ the repo counts as "cached" (a definite zero is
    /// distinct from "not fetched").
    stars: Option<u32>,
    language: Option<String>,
}

/// Colour only when stdout is a terminal and `NO_COLOR` is unset or
/// empty (<https://no-color.org>), so pipes and redirects get plain text.
fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

fn dim(color: bool, s: &str) -> String {
    if color {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

pub async fn run(ctx: &ShokaContext, args: ListArgs) -> Result<()> {
    let color = use_color();
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        // Empty shelf: friendly hint instead of just silence.
        println!(
            "{} (no repos yet — `shoka clone <url>` to add one)",
            dim(color, "0 repos")
        );
        return Ok(());
    }

    // One Engine per `list` invocation — clone_path_for renders the
    // layout template for every shown repo, and Engine construction
    // is the heavy bit (Tera setup).
    let mut engine = Engine::new();

    let filtered = filter_repos(&shelf, &resolved, &args, &mut engine)?;
    if filtered.is_empty() {
        println!(
            "{} matched the filters ({} on the shelf total)",
            dim(color, "0 repos"),
            shelf.len()
        );
        return Ok(());
    }

    // Without `--sort` the listing is exactly the classic multi-line
    // form (a background refresh may populate stars at any time, and
    // that must not change plain `list` output), so the cache is only
    // consulted when sorting. The read is best-effort: a broken
    // cache.toml must not take `list` down, it just means no stars.
    let cache = if args.sort.is_some() {
        match Cache::load(&ctx.paths) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("list: ignoring unreadable cache: {e:#}");
                Cache::default()
            }
        }
    } else {
        Cache::default()
    };
    let mut rows: Vec<ListRow> = filtered
        .iter()
        .map(|&repo| {
            let snap = cache.find_gh_by_triple(&repo.host, &repo.owner, &repo.name);
            ListRow {
                repo,
                stars: snap.and_then(|g| g.stargazer_count),
                language: snap.and_then(|g| g.primary_language.clone()),
            }
        })
        .collect();
    sort_rows(&mut rows, args.sort);

    if args.sort == Some(ListSort::Stars) && rows.iter().any(|r| r.stars.is_none()) {
        eprintln!(
            "hint: some repos have no cached star count; run `shoka cache refresh` to fetch it"
        );
    }

    let widths = column_widths(&rows);
    for row in &rows {
        match format_cached_line(row, widths, color) {
            Some(line) => println!("{line}"),
            None => print_repo(row.repo, &resolved, &mut engine, color)?,
        }
    }
    if filtered.len() < shelf.len() {
        println!();
        println!(
            "{} {} of {} repos matched",
            dim(color, "→"),
            filtered.len(),
            shelf.len()
        );
    }

    Ok(())
}

/// Reorder `rows` in place. `None` keeps shelf order; sorts are
/// stable. Stars sort descending with ties by slug, and rows with no
/// cached count (distinct from a cached 0) go last, also by slug.
fn sort_rows(rows: &mut [ListRow], sort: Option<ListSort>) {
    match sort {
        None => {}
        Some(ListSort::Name) => rows.sort_by_key(|a| a.repo.slug()),
        Some(ListSort::Stars) => rows.sort_by(|a, b| {
            let by_stars = match (a.stars, b.stars) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            by_stars.then_with(|| a.repo.slug().cmp(&b.repo.slug()))
        }),
    }
}

/// `(slug width, star-count digits)` measured over cached rows only.
fn column_widths(rows: &[ListRow]) -> (usize, usize) {
    rows.iter()
        .filter_map(|r| {
            r.stars
                .map(|n| (r.repo.slug().width(), n.to_string().len()))
        })
        .fold((0, 0), |(a, b), (w, d)| (a.max(w), b.max(d)))
}

/// One-line rendering for a row with cached stars, `None` otherwise.
///
/// Columns are padded on the *plain* text by display width and only
/// then coloured, so ANSI codes never skew alignment. The star is the
/// plain U+2605 (width 1), not the emoji, whose width varies by terminal.
fn format_cached_line(
    row: &ListRow,
    (slug_w, digits): (usize, usize),
    color: bool,
) -> Option<String> {
    let stars = row.stars?;
    let slug = row.repo.slug();
    let pad = " ".repeat(slug_w.saturating_sub(slug.width()));
    let star = format!("★ {stars:>digits$}");
    let (slug_c, star_c) = if color {
        (slug.blue().to_string(), star.yellow().to_string())
    } else {
        (slug, star)
    };
    let mut line = format!("{slug_c}{pad}  {star_c}");
    if let Some(lang) = row.language.as_deref().filter(|l| !l.is_empty()) {
        line.push_str("  ");
        line.push_str(&dim(color, lang));
    }
    Some(line)
}

/// Apply `--tag` (AND) and `--has-agents` filters to the shelf.
///
/// The `has_agents` filter touches the filesystem (checks for
/// `<clone_path>/AGENTS.md`), which is fine for "small N" Phase 1
/// shelves but will move into the cache layer once the shelf grows
/// to "lots of repos" territory.
fn filter_repos<'s>(
    shelf: &'s Shelf,
    resolved: &ResolvedConfig,
    args: &ListArgs,
    engine: &mut Engine,
) -> Result<Vec<&'s Repo>> {
    let mut out = Vec::with_capacity(shelf.len());

    for repo in &shelf.repos {
        if !args.tags.is_empty() && !has_all_tags(repo, &args.tags) {
            continue;
        }
        if args.has_agents {
            let path = resolved.clone_path_for(repo, engine)?;
            if !path.join("AGENTS.md").exists() {
                continue;
            }
        }
        out.push(repo);
    }
    Ok(out)
}

/// AND-semantics tag check.
///
/// Inlined iteration rather than `HashSet`-on-`HashSet`: per-repo
/// tag counts are tiny (typically <10), so the constant factor of a
/// hash beats the asymptotic improvement. Avoids the per-call
/// `HashSet` allocation on every shelf entry — relevant once
/// `shoka list` runs over hundreds of repos.
fn has_all_tags(repo: &Repo, wanted: &[String]) -> bool {
    wanted.iter().all(|w| repo.tags.iter().any(|t| t == w))
}

/// Render one repo as a slug headline + indented metadata lines.
/// Quiet by design: only metadata that's set gets a line — we don't
/// pad empty fields with placeholders.
fn print_repo(
    repo: &Repo,
    resolved: &ResolvedConfig,
    engine: &mut Engine,
    color: bool,
) -> Result<()> {
    let slug = repo.slug();
    println!("{}", if color { slug.bold().to_string() } else { slug });
    let path = resolved.clone_path_for(repo, engine)?;
    println!("  {} {}", dim(color, "path :"), path.display());
    if !repo.tags.is_empty() {
        println!("  {} {}", dim(color, "tags :"), repo.tags.join(" "));
    }
    if let Some(vcs) = repo.vcs {
        println!("  {} {vcs:?}", dim(color, "vcs  :"));
    }
    if let Some(note) = &repo.note {
        println!("  {} {}", dim(color, "note :"), note);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GlobalConfig, ShokaConfig, VcsDefault};
    use std::collections::BTreeMap;

    fn row<'a>(repo: &'a Repo, stars: Option<u32>, lang: Option<&str>) -> ListRow<'a> {
        ListRow {
            repo,
            stars,
            language: lang.map(String::from),
        }
    }

    fn names<'a>(rows: &'a [ListRow<'a>]) -> Vec<&'a str> {
        rows.iter().map(|r| r.repo.name.as_str()).collect()
    }

    #[test]
    fn sort_none_keeps_shelf_order() {
        let (a, b, c) = (
            Repo::new("github.com", "u", "zeta"),
            Repo::new("github.com", "u", "alpha"),
            Repo::new("github.com", "u", "mid"),
        );
        let mut rows = vec![
            row(&a, Some(1), None),
            row(&b, Some(9), None),
            row(&c, None, None),
        ];
        sort_rows(&mut rows, None);
        assert_eq!(names(&rows), ["zeta", "alpha", "mid"]);
    }

    #[test]
    fn sort_name_is_ascending() {
        let (a, b, c) = (
            Repo::new("github.com", "u", "zeta"),
            Repo::new("github.com", "u", "alpha"),
            Repo::new("github.com", "u", "mid"),
        );
        let mut rows = vec![
            row(&a, Some(9), None),
            row(&b, None, None),
            row(&c, Some(1), None),
        ];
        sort_rows(&mut rows, Some(ListSort::Name));
        assert_eq!(names(&rows), ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn sort_stars_desc_ties_by_name_missing_last() {
        let (a, b, c, d, e) = (
            Repo::new("github.com", "u", "b-tie"),
            Repo::new("github.com", "u", "a-tie"),
            Repo::new("github.com", "u", "top"),
            Repo::new("github.com", "u", "zero"),
            Repo::new("github.com", "u", "a-missing"),
        );
        let mut rows = vec![
            row(&a, Some(5), None),
            row(&b, Some(5), None),
            row(&c, Some(24), None),
            row(&d, Some(0), None),
            row(&e, None, None),
        ];
        sort_rows(&mut rows, Some(ListSort::Stars));
        // A cached 0 outranks "not fetched".
        assert_eq!(names(&rows), ["top", "a-tie", "b-tie", "zero", "a-missing"]);
    }

    #[test]
    fn uncached_row_has_no_one_line_form() {
        let r = Repo::new("github.com", "u", "n");
        assert!(format_cached_line(&row(&r, None, Some("Rust")), (10, 2), false).is_none());
    }

    #[test]
    fn cached_line_is_plain_when_color_off() {
        let r = Repo::new("github.com", "u", "n");
        let line = format_cached_line(&row(&r, Some(24), Some("Rust")), (15, 2), false).unwrap();
        assert!(!line.contains('\u{1b}'), "no ANSI expected: {line:?}");
        assert_eq!(line, "github.com/u/n   ★ 24  Rust");
        assert!(line.starts_with("github.com/u/n"));
    }

    #[test]
    fn cached_line_without_language_has_no_trailing_space() {
        let r = Repo::new("github.com", "u", "n");
        let line = format_cached_line(&row(&r, Some(3), None), (14, 1), false).unwrap();
        assert_eq!(line, "github.com/u/n  ★ 3");
    }

    #[test]
    fn cached_line_color_wraps_columns() {
        let r = Repo::new("github.com", "u", "n");
        let line = format_cached_line(&row(&r, Some(24), Some("Rust")), (14, 2), true).unwrap();
        assert!(line.contains("\u{1b}[34m"), "blue slug: {line:?}");
        assert!(line.contains("\u{1b}[33m"), "yellow stars: {line:?}");
        assert!(line.contains("\u{1b}[2m"), "dim language: {line:?}");
        assert!(!line.contains('\u{1f31f}') && line.contains('\u{2605}'));
    }

    #[test]
    fn columns_align_by_display_width() {
        let a = Repo::new("github.com", "u", "short");
        let b = Repo::new("github.com", "u", "much-longer-name");
        let c = Repo::new("github.com", "日本", "語");
        let rows = vec![
            row(&a, Some(7), Some("Go")),
            row(&b, Some(1234), Some("Rust")),
            row(&c, Some(55), Some("TS")),
        ];
        let w = column_widths(&rows);
        let lines: Vec<String> = rows
            .iter()
            .map(|r| format_cached_line(r, w, false).unwrap())
            .collect();
        // The ★ sits at the same display column on every row, and the
        // star glyph itself counts as one cell.
        let cols: Vec<usize> = lines
            .iter()
            .map(|l| l.split('★').next().unwrap().width())
            .collect();
        assert!(cols.windows(2).all(|p| p[0] == p[1]), "{lines:?}");
        assert_eq!("★".width(), 1);
        // The language column starts at one display column too.
        let langs: Vec<usize> = lines
            .iter()
            .map(|l| l.rsplit_once("  ").unwrap().0.width())
            .collect();
        assert!(langs.windows(2).all(|p| p[0] == p[1]), "{lines:?}");
    }

    fn resolved_default_layout() -> ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
            pinned: vec![],
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn clone_path_for_default_layout_is_ghq_shaped() {
        let r = resolved_default_layout();
        let repo = Repo::new("github.com", "yukimemi", "shoka");
        let p = r.clone_path_for_one(&repo).unwrap();
        // expand_home absolutises /r → drive-relative on Windows, kept
        // absolute on Unix. Check shape rather than exact equality.
        assert!(p.is_absolute(), "expected absolute, got {p:?}");
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(
            s.ends_with("/r/github.com/yukimemi/shoka"),
            "rendered path should follow the ghq layout, got {s:?}"
        );
    }

    #[test]
    fn clone_path_for_honors_route_layout() {
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ..Default::default()
            },
            routes: vec![crate::config::Route {
                pattern: "host:github.com".into(),
                root: None,
                layout: Some("{{ root }}/{{ owner }}-{{ name }}".into()),
                default_vcs: None,
                default_protocol: None,
            }],
            profiles: BTreeMap::new(),
            pinned: vec![],
        };
        let resolved = cfg.resolve(None).expect("resolve");
        let p = resolved
            .clone_path_for_one(&Repo::new("github.com", "foo", "bar"))
            .unwrap();
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(
            s.ends_with("/r/foo-bar"),
            "flat layout from route should win, got {s:?}"
        );
    }

    #[test]
    fn clone_path_for_per_repo_vcs_wins_over_default() {
        let r = resolved_default_layout();
        let mut repo = Repo::new("github.com", "u", "n");
        repo.vcs = Some(VcsDefault::Jj);
        // Layout doesn't reference {{ vcs }} so the path doesn't
        // change, but we still want the helper to accept the
        // override without erroring out. The behaviour matters once
        // someone writes `layout = ".../{{ vcs }}/..."` for whatever
        // reason — the helper must thread the per-repo override
        // through the Tera context.
        let p = r.clone_path_for_one(&repo).unwrap();
        assert!(p.is_absolute());
    }

    #[test]
    fn has_all_tags_is_and_semantics() {
        let mut repo = Repo::new("github.com", "u", "n");
        repo.tags = vec!["rust".into(), "cli".into()];
        let both: Vec<String> = vec!["rust".into(), "cli".into()];
        assert!(has_all_tags(&repo, &both));
        let extra: Vec<String> = vec!["rust".into(), "cli".into(), "tui".into()];
        assert!(!has_all_tags(&repo, &extra));
        let one: Vec<String> = vec!["rust".into()];
        assert!(has_all_tags(&repo, &one));
        // Empty filter trivially matches (`all` over an empty iter
        // is `true`) — caller is expected to short-circuit on empty
        // rather than rely on this, but the predicate stays
        // well-defined.
        let none: Vec<String> = vec![];
        assert!(has_all_tags(&repo, &none));
    }

    #[test]
    fn filter_repos_applies_tag_filter() {
        let mut shelf = Shelf::default();
        let mut a = Repo::new("github.com", "u", "rust-repo");
        a.tags = vec!["rust".into()];
        let mut b = Repo::new("github.com", "u", "go-repo");
        b.tags = vec!["go".into()];
        let mut c = Repo::new("github.com", "u", "both-repo");
        c.tags = vec!["rust".into(), "cli".into()];
        shelf.add(a).unwrap();
        shelf.add(b).unwrap();
        shelf.add(c).unwrap();

        let r = resolved_default_layout();
        let mut engine = Engine::new();
        let filtered = filter_repos(
            &shelf,
            &r,
            &ListArgs {
                tags: vec!["rust".into()],
                has_agents: false,
                sort: None,
            },
            &mut engine,
        )
        .unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|r| r.name == "rust-repo"));
        assert!(filtered.iter().any(|r| r.name == "both-repo"));

        let filtered = filter_repos(
            &shelf,
            &r,
            &ListArgs {
                tags: vec!["rust".into(), "cli".into()],
                has_agents: false,
                sort: None,
            },
            &mut engine,
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "both-repo");
    }

    #[test]
    fn filter_repos_has_agents_walks_filesystem() {
        // Stage a fake clone tree under a tempdir and point resolved
        // config's root at it via a custom layout so `clone_path_for`
        // lands inside the temp.
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root_str = tmp.path().to_string_lossy().to_string();
        let cfg = ShokaConfig {
            global: GlobalConfig {
                root: Some(root_str),
                layout: "{{ root }}/{{ name }}".into(), // flat for test simplicity
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
            pinned: vec![],
        };
        let resolved = cfg.resolve(None).unwrap();

        // Create AGENTS.md under one repo's would-be clone path.
        fs::create_dir_all(tmp.path().join("with-agents")).unwrap();
        fs::write(tmp.path().join("with-agents/AGENTS.md"), "# stub").unwrap();
        fs::create_dir_all(tmp.path().join("plain")).unwrap();

        let mut shelf = Shelf::default();
        shelf
            .add(Repo::new("github.com", "u", "with-agents"))
            .unwrap();
        shelf.add(Repo::new("github.com", "u", "plain")).unwrap();

        let mut engine = Engine::new();
        let filtered = filter_repos(
            &shelf,
            &resolved,
            &ListArgs {
                tags: vec![],
                has_agents: true,
                sort: None,
            },
            &mut engine,
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "with-agents");
    }
}
