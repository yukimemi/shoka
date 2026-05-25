//! Remote URL parsing — shared by `shoka import` (reads URLs from
//! existing `.git/config`) and `shoka clone` (turns user input into
//! a URL we can hand to gix).
//!
//! The parser is deliberately permissive on input shape (full URL,
//! `owner/name`, `host/owner/name`) and strict on output: every input
//! collapses to a [`RemoteParts`] triple — the same identity shape the
//! shelf records, so callers don't have to re-derive it.

use anyhow::{Context, Result, bail};

use crate::config::Protocol;

/// Parsed `(host, owner, name)` triple from a clone URL.
///
/// `Debug` is required by tests' `unwrap_err` (which formats the `Ok`
/// variant on panic to explain the unexpected success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParts {
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Extract `(host, owner, name)` from a parsed [`gix::Url`].
///
/// Returns an error when the URL has no host, a non-UTF-8 path, an
/// empty owner / name segment, or more than two path segments. The
/// shoka shelf can only represent two-segment owner/name pairs.
pub fn parse_remote_url(url: &gix::Url) -> Result<RemoteParts> {
    let host = url.host().context("remote URL has no host")?.to_string();
    // gix stores paths as bytes; valid SSH / HTTPS git URLs are ASCII
    // in practice, but go through `to_str` for safety.
    //
    // Trim slashes *first*, then strip the `.git` suffix exactly once.
    // Order matters: `owner/repo.git/` would otherwise survive the
    // suffix step ("doesn't end with `.git`") and pass through with
    // the bogus dotfile attached to the name.
    let trimmed = std::str::from_utf8(url.path.as_ref())
        .context("remote URL path is not UTF-8")?
        .trim_matches('/');
    let path = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut iter = path.splitn(2, '/');
    let owner = iter
        .next()
        .filter(|s| !s.is_empty())
        .context("remote URL has no owner segment")?
        .to_string();
    let name = iter
        .next()
        .filter(|s| !s.is_empty())
        .context("remote URL has no name segment")?
        .to_string();
    // Reject deeper paths (`github.com/foo/bar/baz`) — that's not a
    // shape this shelf understands. Better to surface the error than
    // silently lose the trailing segments.
    if name.contains('/') {
        bail!("remote URL path `{path}` has more than two segments");
    }
    Ok(RemoteParts { host, owner, name })
}

/// Turn a user-provided clone input into a `(RemoteParts, gix::Url)`
/// pair suitable for handing to gix / jj.
///
/// Accepted input shapes (in order of detection):
///
/// 1. **Full URL** — anything that looks like `scheme://…` or the SSH
///    shorthand `user@host:path`. Parsed as-is via [`gix::url::parse`].
/// 2. **`owner/name`** — two segments. Combined with `default_host` +
///    `default_protocol` into a URL. This is the ghq-style shortcut.
/// 3. **`host/owner/name`** — three segments. Combined with
///    `default_protocol` into a URL using the embedded host (ignoring
///    `default_host`). Lets users override the host inline without
///    typing the whole `https://…` prefix.
///
/// Anything else (single token, four-segment paths, …) errors out
/// rather than guess — silent guesses tend to produce subtly-wrong
/// clones, and a clear error is friendlier than a 404 ten seconds
/// later.
pub fn parse_clone_input(
    input: &str,
    default_host: &str,
    default_protocol: Protocol,
) -> Result<(RemoteParts, gix::Url)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty clone input — pass a URL or `owner/name`");
    }

    let url_string = if looks_like_url(trimmed) {
        trimmed.to_string()
    } else {
        let segs: Vec<&str> = trimmed.split('/').collect();
        match segs.as_slice() {
            [owner, name] if !owner.is_empty() && !name.is_empty() => {
                build_url(default_host, owner, name, default_protocol)
            }
            [host, owner, name] if !host.is_empty() && !owner.is_empty() && !name.is_empty() => {
                build_url(host, owner, name, default_protocol)
            }
            _ => bail!(
                "input `{trimmed}` is neither a URL nor `<owner>/<name>` / \
                 `<host>/<owner>/<name>` shape; pass a full URL or use the \
                 ghq-style shorthand"
            ),
        }
    };

    let url = gix::url::parse(url_string.as_str().into())
        .with_context(|| format!("parsing clone URL `{url_string}`"))?;
    let parts = parse_remote_url(&url)
        .with_context(|| format!("extracting host/owner/name from `{url_string}`"))?;
    Ok((parts, url))
}

fn build_url(host: &str, owner: &str, name: &str, protocol: Protocol) -> String {
    match protocol {
        Protocol::Https => format!("https://{host}/{owner}/{name}.git"),
        // `git@<host>:<owner>/<name>.git` is the canonical SSH form;
        // gix parses it as scp-style (no `://`).
        Protocol::Ssh => format!("git@{host}:{owner}/{name}.git"),
    }
}

/// Heuristic: does `s` look like a URL we should hand verbatim to
/// gix, vs. a ghq-style shorthand we have to assemble ourselves?
///
/// Any colon is enough. Shorthands (`owner/name`, `host/owner/name`)
/// are colon-free by construction, and the URL forms we care about
/// all contain one: `://` for explicit schemes (`https://`, `ssh://`,
/// `file://`, …), `<user>@<host>:` for the canonical SCP shorthand,
/// and `<host>:<path>` for the userless SCP shorthand that picks up
/// the default user from `~/.ssh/config` (e.g.
/// `github.com:owner/repo.git`). The earlier `'@' && ':'` shape
/// missed that last case and produced bogus URLs like
/// `https://github.com/github.com:foo/bar.git`.
fn looks_like_url(s: &str) -> bool {
    s.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_url(url_str: &str) -> Result<RemoteParts> {
        let url = gix::url::parse(url_str.into())
            .with_context(|| format!("parsing test URL `{url_str}`"))?;
        parse_remote_url(&url)
    }

    #[test]
    fn ssh_url() {
        let p = parse_url("git@github.com:foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn https_url() {
        let p = parse_url("https://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn https_url_without_dot_git() {
        let p = parse_url("https://github.com/foo/bar").unwrap();
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn ssh_url_without_dot_git() {
        let p = parse_url("git@github.com:foo/bar").unwrap();
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn url_with_trailing_slash() {
        let p = parse_url("https://github.com/foo/bar/").unwrap();
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn trailing_slash_after_dot_git_strips_both() {
        // Regression for the slash-before-strip ordering bug.
        let p = parse_url("https://github.com/foo/bar.git/").unwrap();
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn ssh_alternate_user_segment_drops_user() {
        // `ssh://git@gh.example.com/foo/bar.git` — the user (`git`)
        // is part of the URL's userinfo, not the path. gix exposes
        // host = "gh.example.com", path = "foo/bar.git".
        let p = parse_url("ssh://git@gh.example.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "gh.example.com");
        assert_eq!(p.owner, "foo");
        assert_eq!(p.name, "bar");
    }

    #[test]
    fn subgroup_rejected_as_too_deep() {
        let err = parse_url("https://gitlab.com/group/sub/repo.git").unwrap_err();
        assert!(
            err.to_string().contains("more than two segments"),
            "expected too-deep error, got: {err}"
        );
    }

    #[test]
    fn clone_input_full_https_url_passes_through() {
        let (parts, url) = parse_clone_input(
            "https://github.com/foo/bar.git",
            "github.com",
            Protocol::Https,
        )
        .unwrap();
        assert_eq!(parts.host, "github.com");
        assert_eq!(parts.owner, "foo");
        assert_eq!(parts.name, "bar");
        // The URL we hand to gix should be the input verbatim — we
        // don't want to rewrite a user-provided URL.
        assert_eq!(url.to_string(), "https://github.com/foo/bar.git");
    }

    #[test]
    fn clone_input_userless_scp_ssh_passes_through() {
        // Regression: the older `'@' && ':'` heuristic misclassified
        // userless SCP-style SSH URLs (which rely on a default user
        // from `~/.ssh/config`) as shorthand and built
        // `https://github.com/github.com:foo/bar.git`. The colon-only
        // check correctly routes these to gix verbatim.
        let (parts, _) =
            parse_clone_input("github.com:foo/bar.git", "example.com", Protocol::Https).unwrap();
        assert_eq!(parts.host, "github.com");
        assert_eq!(parts.owner, "foo");
        assert_eq!(parts.name, "bar");
    }

    #[test]
    fn clone_input_ssh_shorthand_passes_through() {
        let (parts, _) =
            parse_clone_input("git@github.com:foo/bar.git", "github.com", Protocol::Https).unwrap();
        assert_eq!(parts.host, "github.com");
        assert_eq!(parts.owner, "foo");
        assert_eq!(parts.name, "bar");
    }

    #[test]
    fn clone_input_owner_name_uses_default_host_https() {
        let (parts, url) = parse_clone_input("foo/bar", "github.com", Protocol::Https).unwrap();
        assert_eq!(parts.host, "github.com");
        assert_eq!(parts.owner, "foo");
        assert_eq!(parts.name, "bar");
        assert_eq!(url.to_string(), "https://github.com/foo/bar.git");
    }

    #[test]
    fn clone_input_owner_name_uses_default_host_ssh() {
        let (parts, _) = parse_clone_input("foo/bar", "github.com", Protocol::Ssh).unwrap();
        assert_eq!(parts.host, "github.com");
        assert_eq!(parts.owner, "foo");
        assert_eq!(parts.name, "bar");
    }

    #[test]
    fn clone_input_host_owner_name_overrides_default_host() {
        // The embedded host beats the config default — that's the point
        // of typing the host inline.
        let (parts, _) =
            parse_clone_input("gitlab.com/group/proj", "github.com", Protocol::Https).unwrap();
        assert_eq!(parts.host, "gitlab.com");
        assert_eq!(parts.owner, "group");
        assert_eq!(parts.name, "proj");
    }

    #[test]
    fn clone_input_empty_errors() {
        let err = parse_clone_input("   ", "github.com", Protocol::Https).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected empty-input error, got: {err}"
        );
    }

    #[test]
    fn clone_input_single_token_errors() {
        let err = parse_clone_input("justaname", "github.com", Protocol::Https).unwrap_err();
        assert!(
            err.to_string().contains("neither a URL"),
            "expected shape error, got: {err}"
        );
    }

    #[test]
    fn clone_input_four_segments_errors() {
        let err = parse_clone_input("a/b/c/d", "github.com", Protocol::Https).unwrap_err();
        assert!(
            err.to_string().contains("neither a URL"),
            "expected shape error, got: {err}"
        );
    }
}
