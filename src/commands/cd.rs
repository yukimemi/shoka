//! `shoka cd` — resolve a shelf entry to its on-disk path.
//!
//! Doesn't actually change the parent shell's cwd — a child process
//! can't. Instead, the chosen repo's clone path is emitted via one
//! of two channels:
//!
//! - **`$SHOKA_CD_OUT` (wrapper contract)** — when this env var is
//!   set, the path is written to the named file and nothing goes to
//!   stdout. The shell wrapper installed by `shoka init-shell`
//!   creates a temp file, points the env var at it, redirects shoka
//!   cd's stdout to stderr (so `inquire`'s prompt UI is *visible* to
//!   the user instead of being captured), and finally reads the temp
//!   file to do the `cd`. This sidechannel is the only safe way to
//!   pair an interactive picker with a captured path — `inquire`
//!   0.9 writes its UI to stdout and exposes no public switch to
//!   stderr, so the wrapper must give the path its own channel.
//! - **stdout (manual contract)** — when the env var is unset, the
//!   path is printed to stdout. Useful when invoking `shoka cd`
//!   directly to feed a script or copy a path.
//!
//! Matching:
//!
//! - Arg omitted → fuzzy select over the entire shelf (optionally
//!   filtered by `--tag`).
//! - Arg given → filter candidates whose slug contains the hint as a
//!   substring (case-insensitive). One match ⇒ use it. Multiple ⇒
//!   fuzzy pick among them. Zero ⇒ error out instead of silently
//!   falling back to the full shelf — surprising "I typed the wrong
//!   thing and ended up somewhere else" beats a clear "no match".
//!
//! Sanity: the resolved path is verified to exist on disk before
//! being emitted. A stale shelf entry (repo moved / deleted)
//! produces a clear shoka error rather than the shell's confusing
//! `cd: No such file or directory`.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use inquire::Select;

use crate::cli::CdArgs;
use crate::commands::ShokaContext;
use crate::config::ShokaConfig;
use crate::state::{Repo, Shelf};

/// Env var the shell wrapper uses to receive the resolved path
/// out-of-band from stdout. See module docs.
pub const CD_OUT_ENV: &str = "SHOKA_CD_OUT";

pub async fn run(ctx: &ShokaContext, args: CdArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;

    if shelf.is_empty() {
        bail!(
            "shelf is empty — nothing to cd into. `shoka clone <url>` or \
             `shoka import <dir>` first"
        );
    }

    // Tag filter first, then hint filter — mirrors `shoka list`'s
    // order so the two commands stay predictable together.
    let tag_filtered: Vec<&Repo> = if args.tags.is_empty() {
        shelf.repos.iter().collect()
    } else {
        shelf
            .repos
            .iter()
            .filter(|r| args.tags.iter().all(|t| r.tags.iter().any(|rt| rt == t)))
            .collect()
    };
    if tag_filtered.is_empty() {
        bail!(
            "no repos matched the tag filter ({} on the shelf total)",
            shelf.len()
        );
    }

    let page_size = resolved.ui.cd_page_size;
    let chosen = match args.repo.as_deref() {
        Some(hint) => choose_by_hint(&tag_filtered, hint, page_size)?,
        None => fuzzy_pick(&tag_filtered, "cd to:", page_size)?,
    };

    let path = resolved.clone_path_for_one(chosen)?;
    if !path.is_dir() {
        bail!(
            "{} resolves to {}, but that path doesn't exist — \
             the repo was probably moved or deleted; \
             try `shoka prune` to clean up the shelf",
            chosen.slug(),
            path.display()
        );
    }

    emit_path(&path)?;
    Ok(())
}

/// Emit the resolved path. When [`CD_OUT_ENV`] is set, write to that
/// file (and nothing to stdout — the wrapper rendered the prompt UI
/// on stdout-redirected-to-stderr, and reads the path back from this
/// file). When unset, write to stdout for the direct-invocation case.
///
/// Public so `shoka tui` can reuse the exact same sidechannel
/// contract for its Enter-to-cd flow: the same shell wrapper that
/// services `shoka cd` also picks up the path that `shoka tui`
/// writes when the user picks a repo.
pub fn emit_path(path: &Path) -> Result<()> {
    let rendered = path.to_string_lossy();
    match std::env::var_os(CD_OUT_ENV) {
        Some(out) if !out.is_empty() => {
            // Write the sidechannel path FIRST, then announce the cwd —
            // ordering matters. The wrapper only runs its `cd` when this
            // write succeeds and `emit_path` returns `Ok`; if the write
            // fails we propagate the error and no chdir happens, so the
            // OSC 7 hint must not have fired yet (else the terminal would
            // believe it moved into a dir the shell never entered). The
            // manual / stdout branch below deliberately stays silent:
            // there a script is just capturing a path string and may
            // never chdir, so announcing a cwd would be a lie.
            std::fs::write(&out, rendered.as_bytes()).with_context(|| {
                format!(
                    "writing path to ${CD_OUT_ENV}={}",
                    Path::new(&out).display()
                )
            })?;
            emit_osc7_cwd(path);
            Ok(())
        }
        // Trailing newline only on the stdout path: command substitution
        // strips it, while the sidechannel reader doesn't expect one.
        _ => {
            println!("{rendered}");
            Ok(())
        }
    }
}

/// Announce `path` to the terminal as the new working directory via
/// an OSC 7 escape — `ESC ] 7 ; file://<host>/<path> ST`. Terminals
/// that understand it (WezTerm, iTerm2, Kitty, Windows Terminal,
/// VTE-based GNOME / Tilix, …) use the hint to open new tabs / splits
/// already `cd`'d into the same repo. Terminals that don't simply
/// ignore the unknown sequence, so this is harmless everywhere.
///
/// Best-effort and side-effect-only:
///
/// - Written to **stderr**, never stdout. In the manual `shoka cd`
///   contract stdout carries the resolved path for `$(…)` capture and
///   must stay byte-clean; stderr reaches the same TTY in every shell
///   wrapper flow.
/// - Skipped unless stderr is a real terminal, so the escape never
///   leaks into a pipe / file when the caller redirected output.
/// - The host is left empty (`file:///…`), which terminals read as
///   "local". Emitting the real hostname would buy only marginal SSH
///   disambiguation at the cost of a `gethostname` dependency; the
///   empty-authority form is the widely-supported minimal shape.
/// - Write / flush errors are swallowed: a failed cwd hint must never
///   take down the actual `cd`.
fn emit_osc7_cwd(path: &Path) {
    use std::io::{IsTerminal, Write};

    let mut stderr = std::io::stderr();
    if !stderr.is_terminal() {
        return;
    }
    let _ = write!(stderr, "{}", osc7_sequence(&file_url(path)));
    let _ = stderr.flush();
}

/// Wrap a `file://` URL in the OSC 7 control sequence. The terminator
/// is ST (`ESC \`), the spec-correct string terminator — preferred
/// over the BEL (`\x07`) shorthand some emitters use.
fn osc7_sequence(url: &str) -> String {
    format!("\x1b]7;{url}\x1b\\")
}

/// Build a `file://` URL for `path` suitable for an OSC 7 hint.
///
/// A normal absolute path has its authority (host) omitted, yielding
/// `file:///…`, which terminals read as the local machine. A Windows
/// UNC path (`\\server\share\…`) normalises to a `//server/share/…`
/// url-path whose leading `//` already *is* the authority, so it's
/// joined as `file:<path>` (→ `file://server/share/…`) rather than
/// the invalid four-slash `file:////server/…` a blind `file://`
/// prefix would produce.
fn file_url(path: &Path) -> String {
    let url_path = to_url_path(&path.to_string_lossy());
    if url_path.starts_with("//") {
        // UNC: the `//server/share` prefix is already authority + path.
        format!("file:{url_path}")
    } else {
        format!("file://{url_path}")
    }
}

/// Normalise a filesystem path string into the path component of a
/// `file://` URL: backslashes become forward slashes (so Windows
/// `C:\a` → `/C:/a`), a single leading slash is guaranteed (Unix
/// paths already have one; a Windows drive path gains one so the
/// authority/path split is well-formed), and the result is
/// percent-encoded.
fn to_url_path(raw: &str) -> String {
    let mut slashed = raw.replace('\\', "/");
    if !slashed.starts_with('/') {
        slashed.insert(0, '/');
    }
    percent_encode_path(&slashed)
}

/// Percent-encode a URL path. The path-structural `/` and the Windows
/// drive `:` are kept literal along with the RFC 3986 unreserved set
/// (`ALPHA DIGIT - . _ ~`); every other byte — spaces, and each byte
/// of a multibyte UTF-8 char — becomes `%XX`.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

/// Uppercase hex digit for a nibble (`0..=15`). Used by the
/// percent-encoder; uppercase matches the RFC 3986 recommendation.
fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// Match `hint` against the candidate slugs (case-insensitive
/// substring). One hit ⇒ return it directly. Multiple ⇒ open a
/// fuzzy picker pre-seeded with the narrowed set. Zero ⇒ error out.
pub(crate) fn choose_by_hint<'a>(
    candidates: &[&'a Repo],
    hint: &str,
    page_size: usize,
) -> Result<&'a Repo> {
    let hint_lc = hint.to_lowercase();
    let matches: Vec<&'a Repo> = candidates
        .iter()
        .copied()
        .filter(|r| r.slug().to_lowercase().contains(&hint_lc))
        .collect();

    match matches.len() {
        0 => bail!(
            "no repos on the shelf match `{hint}` — \
             try `shoka list` to see what's there"
        ),
        1 => Ok(matches[0]),
        _ => fuzzy_pick(
            &matches,
            &format!("multiple matches for `{hint}`:"),
            page_size,
        ),
    }
}

/// Fuzzy-select among `candidates` via [`inquire::Select`] (which
/// uses its own internal fuzzy match algorithm; nucleo only lands
/// for the TUI). Items are wrapped in a thin `Display` adapter so
/// the picker can return the chosen [`Repo`] reference directly —
/// no string round-trip + linear scan on the way back.
///
/// Each item renders as `slug` plus, when the entry has a path
/// override (`shoka import` for repos cloned with a non-default
/// dir name, or multiple checkouts of the same remote), `→ <path>`
/// with `$HOME` tilde-shortened. Path-less entries (laid out by
/// `shoka clone` against the configured layout) render slug only,
/// matching the historic look.
///
/// `page_size` is the number of rows shown at once, sourced from
/// `[global.ui].cd_page_size` (floored at 1 by the config resolver).
pub(crate) fn fuzzy_pick<'a>(
    candidates: &[&'a Repo],
    prompt: &str,
    page_size: usize,
) -> Result<&'a Repo> {
    if candidates.is_empty() {
        // Defensive: callers already filter to non-empty, but in case
        // a future caller forgets, surface the empty-case explicitly.
        bail!("nothing to pick — candidate list is empty");
    }

    /// `inquire::Select` requires its options to be `Display` and
    /// returns the chosen option by value. A reference-carrying
    /// wrapper lets us hand the picker borrowed `&Repo`s and pull the
    /// reference back out without reallocating slug strings.
    #[derive(Clone)]
    struct RepoItem<'r>(&'r Repo);
    impl fmt::Display for RepoItem<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0.slug())?;
            if let Some(p) = &self.0.path {
                write!(f, "  →  {}", tilde_shorten(p))?;
            }
            Ok(())
        }
    }

    let items: Vec<RepoItem<'a>> = candidates.iter().copied().map(RepoItem).collect();
    // `page_size` controls how many rows the picker shows at once.
    // Configurable via `[global.ui].cd_page_size`; the resolver floors
    // it at 1 so inquire (which panics on a zero page size) is safe.
    let chosen = Select::new(prompt, items)
        .with_page_size(page_size)
        .prompt()
        .context("repo selection cancelled")?;
    Ok(chosen.0)
}

/// Render `p` with the user's home dir collapsed to `~`. Falls back
/// to the verbatim path string if home can't be resolved or if `p`
/// doesn't start under it — the worst case is "the path shows in
/// full", which is still readable, just longer.
///
/// The home-dir lookup is cached in a `OnceLock` because the picker
/// formatter calls this once per candidate per repaint, and
/// `BaseDirs::new()` does a system / env query under the hood. One
/// resolution per process is plenty — `$HOME` doesn't change at
/// runtime.
fn tilde_shorten(p: &Path) -> String {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let home =
        HOME.get_or_init(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()));
    match home {
        Some(h) => match p.strip_prefix(h) {
            Ok(rest) => {
                let sep = std::path::MAIN_SEPARATOR;
                if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~{sep}{}", rest.display())
                }
            }
            Err(_) => p.display().to_string(),
        },
        None => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GlobalConfig, VcsDefault};
    use std::collections::BTreeMap;

    fn r(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    fn r_owned(owner: &str, name: &str) -> Repo {
        Repo::new("github.com", owner, name)
    }

    #[test]
    fn hint_filters_to_substring_case_insensitive() {
        let a = r("shoka");
        let b = r("renri");
        let c = r("kanade");
        let candidates: Vec<&Repo> = vec![&a, &b, &c];

        // Unique substring match → returns the unique candidate.
        let picked = choose_by_hint(&candidates, "ren", 15).unwrap();
        assert_eq!(picked.name, "renri");

        // Case-insensitive — uppercase hint still matches.
        let picked = choose_by_hint(&candidates, "RENRI", 15).unwrap();
        assert_eq!(picked.name, "renri");
    }

    #[test]
    fn hint_with_zero_matches_errors_cleanly() {
        let a = r("shoka");
        let candidates: Vec<&Repo> = vec![&a];
        let err = choose_by_hint(&candidates, "no-such-thing", 15).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no repos on the shelf match"),
            "expected no-match error, got: {msg}"
        );
    }

    #[test]
    fn hint_substring_matches_owner_or_host() {
        // The hint matches against the full slug, not just the name —
        // so a hint that matches by owner works too. Useful when the
        // user remembers "the rust-org one" but not the project name.
        let a = r_owned("rust-org", "alpha");
        let b = r_owned("other-org", "beta");
        let candidates: Vec<&Repo> = vec![&a, &b];
        let picked = choose_by_hint(&candidates, "rust-org", 15).unwrap();
        assert_eq!(picked.name, "alpha");
    }

    fn resolved_with_layout(layout: &str, root: &str) -> crate::config::ResolvedConfig {
        ShokaConfig {
            global: GlobalConfig {
                root: Some(root.into()),
                layout: layout.into(),
                ..Default::default()
            },
            routes: vec![],
            profiles: BTreeMap::new(),
        }
        .resolve(None)
        .expect("resolve")
    }

    #[test]
    fn tilde_shorten_collapses_home_dir_prefix() {
        // We can't know the test runner's actual home dir, but we
        // can exercise the strip-prefix branch by passing the home
        // dir back in as the prefix. Two checks:
        // 1. A path with no home prefix renders verbatim (no panic).
        // 2. The home dir itself collapses to `~`.
        // 3. A nested path under home renders with `~/…` (or
        //    `~\…` on Windows — `MAIN_SEPARATOR` keeps the test
        //    platform-agnostic).
        let home = match directories::BaseDirs::new() {
            Some(b) => b.home_dir().to_path_buf(),
            None => return, // sandboxed CI without a home dir — skip silently.
        };
        let sep = std::path::MAIN_SEPARATOR;

        assert_eq!(tilde_shorten(&home), "~");

        let nested = home
            .join("src")
            .join("github.com")
            .join("yukimemi")
            .join("shoka");
        let rendered = tilde_shorten(&nested);
        assert!(
            rendered.starts_with(&format!("~{sep}src")),
            "expected `~{sep}src…`, got {rendered:?}"
        );

        // A path outside home falls back to verbatim — pick a path
        // that can't plausibly be a subpath of the test runner's home.
        let outside = std::path::PathBuf::from(if cfg!(windows) {
            r"C:\definitely\not\home"
        } else {
            "/definitely/not/home"
        });
        let rendered = tilde_shorten(&outside);
        assert!(
            !rendered.starts_with('~'),
            "outside-home path must not tilde-shorten, got {rendered:?}"
        );
    }

    #[test]
    fn percent_encode_keeps_path_structure_and_escapes_the_rest() {
        // `/`, `:` and the unreserved set survive verbatim so the URL
        // path stays readable; a space (the common offender) escapes.
        assert_eq!(
            percent_encode_path("/home/user/a-b._~/c"),
            "/home/user/a-b._~/c"
        );
        assert_eq!(
            percent_encode_path("/C:/Program Files"),
            "/C:/Program%20Files"
        );
        // Each byte of a multibyte UTF-8 char is percent-encoded — a
        // Japanese path must round-trip through a byte-wise encoder,
        // not a char-wise one. `雪` is E9 9B AA in UTF-8.
        assert_eq!(percent_encode_path("/雪"), "/%E9%9B%AA");
    }

    #[test]
    fn to_url_path_normalises_unix_and_windows_shapes() {
        // Unix path already absolute → untouched but for encoding.
        assert_eq!(to_url_path("/home/u/a b"), "/home/u/a%20b");
        // Windows path: backslashes → slashes, and a leading slash is
        // prepended so the drive letter lands in the path, not the
        // authority — `C:\Users\a b` → `/C:/Users/a%20b`.
        assert_eq!(to_url_path(r"C:\Users\a b"), "/C:/Users/a%20b");
    }

    #[test]
    fn file_url_and_osc7_sequence_have_the_expected_envelope() {
        // Forward-slash input keeps this assertion platform-agnostic
        // (no backslash branch), so it holds on the Linux CI runner
        // and a Windows dev box alike.
        let url = file_url(Path::new("/home/u/repo"));
        assert_eq!(url, "file:///home/u/repo");
        // OSC 7 envelope: `ESC ] 7 ; <url> ST`, ST = ESC `\`.
        let seq = osc7_sequence(&url);
        assert_eq!(seq, "\x1b]7;file:///home/u/repo\x1b\\");
        assert!(seq.starts_with("\x1b]7;"), "must open with OSC 7: {seq:?}");
        assert!(seq.ends_with("\x1b\\"), "must close with ST: {seq:?}");
    }

    #[test]
    fn file_url_keeps_windows_unc_paths_well_formed() {
        // A UNC path normalises to a `//server/share/…` url-path; the
        // leading `//` is the authority, so the join must NOT add its
        // own `//` (that yields the invalid `file:////server/…`). The
        // host segment carries the server name: `file://server/share`.
        assert_eq!(
            file_url(Path::new(r"\\server\share\repo")),
            "file://server/share/repo"
        );
        // A space in a UNC path still percent-encodes inside the path.
        assert_eq!(
            file_url(Path::new(r"\\nas\team share\repo")),
            "file://nas/team%20share/repo"
        );
    }

    #[test]
    fn clone_path_uses_layout_so_cd_lands_where_clone_left_it() {
        // Sanity: cd and clone share the same path-resolution machine.
        // If they ever drift apart, this test fails with a path
        // mismatch — easier to debug than a runtime cd-to-empty-dir.
        let r = r("shoka");
        let resolved = resolved_with_layout("{{ root }}/{{ name }}", "/data");
        let p = resolved.clone_path_for_one(&r).unwrap();
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(
            s.ends_with("/data/shoka"),
            "cd path should follow the configured layout, got {s:?}"
        );
        // Touch the unused VcsDefault import so this stays a no-op
        // compile dependency rather than a dead import.
        let _ = VcsDefault::Auto;
    }
}
