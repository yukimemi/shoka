//! Stop version-controlling a path: append it to the repo's
//! `.gitignore`, then run `jj file untrack <path>`.
//!
//! The order matters — `jj file untrack` refuses a path that is not
//! already ignored, so the `.gitignore` edit has to land first. If
//! jj then fails the `.gitignore` edit is left in place (no silent
//! rollback) and the error says so.

use std::path::{Component, Path};
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use tokio::process::Command;

use crate::actions::{ACTION_TIMEOUT, VcsKind, detect_vcs};

/// Build the `.gitignore` line for `target`, anchored at the repo
/// root: `/dir/file`, with a trailing `/` for directories. Separators
/// are always `/`. Glob metacharacters are backslash-escaped so the
/// line matches the path literally.
///
/// Errors (without touching anything) for the root itself, paths
/// outside the root, `.gitignore` itself, VCS metadata dirs and
/// names containing a newline.
pub fn ignore_line(repo_root: &Path, target: &Path, is_dir: bool) -> Result<String> {
    let rel = target
        .strip_prefix(repo_root)
        .map_err(|_| anyhow!("{} is outside the repository", target.display()))?;
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => bail!("unsupported path component in {}", rel.display()),
        }
    }
    if parts.is_empty() {
        bail!("refusing to untrack the repository root");
    }
    if parts.iter().any(|p| p.contains('\n') || p.contains('\r')) {
        bail!("path contains a newline: {}", rel.display());
    }
    if parts.len() == 1 && matches!(parts[0].as_str(), ".gitignore" | ".git" | ".jj") {
        bail!("refusing to untrack {}", parts[0]);
    }
    let escaped: Vec<String> = parts.iter().map(|p| escape_literal(p)).collect();
    let mut line = format!("/{}", escaped.join("/"));
    if is_dir {
        line.push('/');
    }
    Ok(line)
}

fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(ch);
    }
    if out.ends_with(' ') {
        out.pop();
        out.push_str("\\ ");
    }
    out
}

/// Comparison key for dedupe: trailing spaces and the leading `/`
/// dropped. A trailing `/` is kept — it makes a pattern
/// directory-only — and leading whitespace is literal in gitignore,
/// so it is kept too. Negations (`!`) and comments (`#`) never match.
fn dedupe_key(line: &str) -> Option<&str> {
    let t = line.trim_end_matches([' ', '\t', '\r']);
    if t.is_empty() || t.starts_with('!') || t.starts_with('#') {
        return None;
    }
    Some(t.trim_start_matches('/'))
}

/// Whether an existing `.gitignore` line `have` already covers `want`
/// (both from [`dedupe_key`]). Equal keys match; a directory-only
/// `want` (`foo/`) is also covered by the bare `foo`, but a bare
/// `want` is never covered by the directory-only `foo/`.
fn covers(have: &str, want: &str) -> bool {
    have == want || want.strip_suffix('/') == Some(have)
}

/// Pure `.gitignore` edit. Returns the new file content, or `None`
/// when `line` is already present (equivalent literal entry) and
/// nothing needs writing. Globs are never treated as equivalent.
/// A missing trailing newline on the existing content is added
/// before appending, and the result always ends with `\n`.
pub fn add_ignore_entry(existing: Option<&str>, line: &str) -> Option<String> {
    let existing = existing.unwrap_or("");
    let want = dedupe_key(line);
    if want.is_some()
        && existing
            .lines()
            .any(|l| dedupe_key(l).zip(want).is_some_and(|(h, w)| covers(h, w)))
    {
        return None;
    }
    let mut out = String::from(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    Some(out)
}

/// Add `target` to `<repo_root>/.gitignore` (re-reading the file now,
/// so concurrent edits are preserved) and report whether it changed.
pub fn update_gitignore(repo_root: &Path, target: &Path, is_dir: bool) -> Result<bool> {
    let line = ignore_line(repo_root, target, is_dir)?;
    let path = repo_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("reading {}: {e}", path.display()),
    };
    match add_ignore_entry(existing.as_deref(), &line) {
        Some(new) => {
            std::fs::write(&path, new).map_err(|e| anyhow!("writing {}: {e}", path.display()))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Ignore then untrack `target`. `Ok` carries a status-line message;
/// every failure is an `Err` whose message says what happened (and
/// whether `.gitignore` was already updated).
pub async fn untrack(repo_root: &Path, target: &Path) -> Result<String> {
    if detect_vcs(repo_root) != Some(VcsKind::Jj) {
        bail!("not a jj repo: {}", repo_root.display());
    }
    let is_dir = target.is_dir();
    let line = ignore_line(repo_root, target, is_dir)?;
    let added = update_gitignore(repo_root, target, is_dir)?;
    let note = if added {
        ".gitignore updated"
    } else {
        ".gitignore already had it"
    };
    let rel = line.trim_start_matches('/').trim_end_matches('/');
    let rel = target
        .strip_prefix(repo_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| rel.to_string());

    let mut cmd = Command::new("jj");
    cmd.args(["file", "untrack", "--", &rel])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("jj not found in PATH ({note}, untrack not run)")
        } else {
            anyhow!("failed to spawn jj: {e} ({note}, untrack not run)")
        }
    })?;
    match tokio::time::timeout(ACTION_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(format!("untracked {rel} ({note})")),
        Ok(Ok(out)) => bail!(
            "jj file untrack failed, {note}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Ok(Err(e)) => bail!("jj file untrack: {e} ({note})"),
        Err(_) => bail!("jj file untrack timed out ({note})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn appends_to_existing_content() {
        let out = add_ignore_entry(Some("target/\n"), "/foo").unwrap();
        assert_eq!(out, "target/\n/foo\n");
    }

    #[test]
    fn skips_equivalent_entry() {
        assert_eq!(add_ignore_entry(Some("/foo\n"), "/foo"), None);
        assert_eq!(add_ignore_entry(Some("foo\n"), "/foo"), None);
        assert_eq!(add_ignore_entry(Some("/foo/  \n"), "/foo/"), None);
    }

    #[test]
    fn trailing_slash_keeps_dir_only_semantics() {
        // Bare / same-form entries cover a directory target.
        assert_eq!(add_ignore_entry(Some("/build\n"), "/build/"), None);
        assert_eq!(add_ignore_entry(Some("/build/\n"), "/build/"), None);
        // `/foo/` is directory-only, so it must not cover a file `foo`.
        assert_eq!(
            add_ignore_entry(Some("/foo/\n"), "/foo").unwrap(),
            "/foo/\n/foo\n"
        );
        // Leading whitespace is literal, not the same pattern.
        assert!(add_ignore_entry(Some("  /foo\n"), "/foo").is_some());
    }

    #[test]
    fn negation_comment_and_glob_are_not_duplicates() {
        let out = add_ignore_entry(Some("!/foo\n"), "/foo").unwrap();
        assert_eq!(out, "!/foo\n/foo\n");
        assert!(add_ignore_entry(Some("# /foo\n"), "/foo").is_some());
        assert!(add_ignore_entry(Some("/fo*\n"), "/foo").is_some());
        assert!(add_ignore_entry(Some("/foo/bar\n"), "/foo").is_some());
    }

    #[test]
    fn creates_when_missing() {
        assert_eq!(add_ignore_entry(None, "/foo").unwrap(), "/foo\n");
    }

    #[test]
    fn adds_newline_when_file_lacks_one() {
        let out = add_ignore_entry(Some("target/"), "/foo").unwrap();
        assert_eq!(out, "target/\n/foo\n");
    }

    #[test]
    fn ignore_line_forms() {
        let root = Path::new("/r");
        assert_eq!(
            ignore_line(root, &root.join("a").join("b.txt"), false).unwrap(),
            "/a/b.txt"
        );
        assert_eq!(ignore_line(root, &root.join("dir"), true).unwrap(), "/dir/");
        assert_eq!(
            ignore_line(root, &root.join("a*b"), false).unwrap(),
            "/a\\*b"
        );
    }

    #[test]
    fn ignore_line_rejects_bad_targets() {
        let root = Path::new("/r");
        assert!(ignore_line(root, Path::new("/elsewhere/x"), false).is_err());
        assert!(ignore_line(root, root, true).is_err());
        assert!(ignore_line(root, &root.join(".gitignore"), false).is_err());
        assert!(ignore_line(root, &root.join(".jj"), true).is_err());
        assert!(ignore_line(root, &root.join("a\nb"), false).is_err());
    }

    #[test]
    fn update_gitignore_creates_appends_and_dedupes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let f = root.join("secret.txt");
        fs::write(&f, "x").unwrap();
        assert!(update_gitignore(root, &f, false).unwrap());
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "/secret.txt\n"
        );
        assert!(!update_gitignore(root, &f, false).unwrap());

        fs::write(root.join(".gitignore"), "target/").unwrap();
        let d = root.join("out");
        fs::create_dir(&d).unwrap();
        assert!(update_gitignore(root, &d, true).unwrap());
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "target/\n/out/\n"
        );
    }

    #[tokio::test]
    async fn untrack_errors_when_not_jj_repo_and_leaves_gitignore_alone() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let f = dir.path().join("a");
        fs::write(&f, "x").unwrap();
        let err = untrack(dir.path(), &f).await.unwrap_err();
        assert!(err.to_string().contains("not a jj repo"));
        assert!(!dir.path().join(".gitignore").exists());
    }
}
