//! `shoka list` — show what's on the shelf.
//!
//! Reads [`Shelf`] from state.toml, applies the requested filters
//! (`--tag` AND-semantics, `--has-agents`), and prints one line per
//! matching repo. Optional indented metadata follows: tags / vcs
//! override / note / clone path.
//!
//! Phase 1 implementation: state-only. The next iteration will fold
//! in `cache.toml`'s git-status snapshot so the listing can show
//! `ahead` / `behind` / `dirty` glyphs without paying a fresh
//! `git status` per repo.

use std::collections::HashSet;

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::cli::ListArgs;
use crate::commands::ShokaContext;
use crate::config::{ResolvedConfig, ShokaConfig};
use crate::state::{Repo, Shelf};

pub async fn run(ctx: &ShokaContext, args: ListArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        // Empty shelf: friendly hint instead of just silence.
        println!(
            "{} (no repos yet — `shoka clone <url>` to add one)",
            "0 repos".dimmed()
        );
        return Ok(());
    }

    let filtered = filter_repos(&shelf, &resolved, &args)?;
    if filtered.is_empty() {
        println!(
            "{} matched the filters ({} on the shelf total)",
            "0 repos".dimmed(),
            shelf.len()
        );
        return Ok(());
    }

    for repo in &filtered {
        print_repo(repo, &resolved)?;
    }
    if filtered.len() < shelf.len() {
        println!();
        println!(
            "{} {} of {} repos matched",
            "→".dimmed(),
            filtered.len(),
            shelf.len()
        );
    }

    Ok(())
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
) -> Result<Vec<&'s Repo>> {
    let wanted_tags: HashSet<&str> = args.tags.iter().map(String::as_str).collect();
    let mut out = Vec::with_capacity(shelf.len());

    for repo in &shelf.repos {
        if !wanted_tags.is_empty() && !has_all_tags(repo, &wanted_tags) {
            continue;
        }
        if args.has_agents {
            let path = resolved.clone_path_for(repo)?;
            if !path.join("AGENTS.md").exists() {
                continue;
            }
        }
        out.push(repo);
    }
    Ok(out)
}

fn has_all_tags(repo: &Repo, wanted: &HashSet<&str>) -> bool {
    let owned: HashSet<&str> = repo.tags.iter().map(String::as_str).collect();
    wanted.is_subset(&owned)
}

/// Render one repo as a slug headline + indented metadata lines.
/// Quiet by design: only metadata that's set gets a line — we don't
/// pad empty fields with placeholders.
fn print_repo(repo: &Repo, resolved: &ResolvedConfig) -> Result<()> {
    println!("{}", repo.slug().bold());
    let path = resolved.clone_path_for(repo)?;
    println!("  {} {}", "path :".dimmed(), path.display());
    if !repo.tags.is_empty() {
        println!("  {} {}", "tags :".dimmed(), repo.tags.join(" "));
    }
    if let Some(vcs) = repo.vcs {
        println!("  {} {vcs:?}", "vcs  :".dimmed());
    }
    if let Some(note) = &repo.note {
        println!("  {} {}", "note :".dimmed(), note);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GlobalConfig, ShokaConfig, VcsDefault};
    use std::collections::BTreeMap;

    fn resolved_default_layout() -> ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some("/r".into()),
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn clone_path_for_default_layout_is_ghq_shaped() {
        let r = resolved_default_layout();
        let repo = Repo::new("github.com", "yukimemi", "shoka");
        let p = r.clone_path_for(&repo).unwrap();
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
        };
        let resolved = cfg.resolve(None).expect("resolve");
        let p = resolved
            .clone_path_for(&Repo::new("github.com", "foo", "bar"))
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
        let p = r.clone_path_for(&repo).unwrap();
        assert!(p.is_absolute());
    }

    #[test]
    fn has_all_tags_is_and_semantics() {
        let mut repo = Repo::new("github.com", "u", "n");
        repo.tags = vec!["rust".into(), "cli".into()];
        let want_both: HashSet<&str> = ["rust", "cli"].into_iter().collect();
        assert!(has_all_tags(&repo, &want_both));
        let want_extra: HashSet<&str> = ["rust", "cli", "tui"].into_iter().collect();
        assert!(!has_all_tags(&repo, &want_extra));
        let want_one: HashSet<&str> = ["rust"].into_iter().collect();
        assert!(has_all_tags(&repo, &want_one));
        let want_none: HashSet<&str> = HashSet::new();
        // Empty filter trivially matches (subset of any set) — caller
        // is expected to short-circuit on empty rather than rely on
        // this, but the predicate stays well-defined.
        assert!(has_all_tags(&repo, &want_none));
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
        let filtered = filter_repos(
            &shelf,
            &r,
            &ListArgs {
                tags: vec!["rust".into()],
                has_agents: false,
            },
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
            },
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

        let filtered = filter_repos(
            &shelf,
            &resolved,
            &ListArgs {
                tags: vec![],
                has_agents: true,
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "with-agents");
    }
}
