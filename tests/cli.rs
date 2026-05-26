//! Integration tests for the `shoka` CLI.
//!
//! Uses `cargo run` via `env!("CARGO_BIN_EXE_shoka")` plus
//! [`assert_cmd::Command`] so we get the standard assertion DSL
//! without rolling our own `Command` wrapper. Tests stage their
//! fixtures under [`tempfile::TempDir`] so the user's real
//! `$XDG_CONFIG_HOME/shoka` is never touched.
//!
//! Hermeticity comes from three env-var overrides set by
//! [`cmd_with_isolated_config`]:
//!
//! - `SHOKA_CONFIG` — points at a tempdir config with
//!   `background_refresh = false` so the bg-refresh spawn short-
//!   circuits and tests stay fast.
//! - `SHOKA_STATE_DIR` — redirects `state.toml` into the tempdir so
//!   `clone` / `import` don't pollute the real OS data directory.
//! - `SHOKA_CACHE_DIR` — same idea, for `cache.toml`.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

/// Build an `assert_cmd::Command` pre-seeded with a hermetic config
/// plus state / cache dirs under a fresh [`TempDir`]. The returned
/// tempdir's `path()` is also the clone root, so callers can assert
/// on cloned-into paths directly.
fn cmd_with_isolated_config() -> (Command, TempDir) {
    let tmp = TempDir::new().expect("temp dir");
    let cfg = tmp.path().join("config.toml");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("mkdir root");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).expect("mkdir cache");

    let mut f = std::fs::File::create(&cfg).expect("write config.toml");
    write!(
        f,
        r#"
[global]
root = "{root}"

[global.cache]
background_refresh = false
"#,
        // TOML strings need backslashes escaped on Windows paths.
        root = root.display().to_string().replace('\\', "\\\\"),
    )
    .expect("write config body");

    let mut cmd = Command::cargo_bin("shoka").expect("binary built");
    cmd.env("SHOKA_CONFIG", &cfg)
        .env("SHOKA_STATE_DIR", &state_dir)
        .env("SHOKA_CACHE_DIR", &cache_dir)
        // Strip any inherited profile so the test exercises the
        // default code path regardless of the dev's environment.
        .env_remove("SHOKA_PROFILE");
    (cmd, tmp)
}

/// Write a minimal `state.toml` describing the given `(host, owner, name)`
/// triples to the test's state dir. Useful for tests that want a
/// pre-populated shelf without running `shoka clone` / `import` first.
fn seed_shelf(state_dir: &Path, repos: &[(&str, &str, &str)]) {
    std::fs::create_dir_all(state_dir).unwrap();
    let mut body = String::from("version = 1\n");
    for (host, owner, name) in repos {
        body.push_str(&format!(
            "\n[[repos]]\nhost = \"{host}\"\nowner = \"{owner}\"\nname = \"{name}\"\n"
        ));
    }
    std::fs::write(state_dir.join("state.toml"), body).expect("write state.toml");
}

/// Initialise a minimal git repo at `dir` and stamp a `remote.origin`
/// pointing at `url`. Sidesteps gix's higher-level remote-config API
/// (which would require a working tree commit + writing back via
/// `config_snapshot_mut().commit()`); for the import scan we only
/// need the `[remote "origin"] url = ...` lines to be present in
/// `.git/config`, so we append them after init.
fn init_git_repo_with_remote(dir: &Path, url: &str) {
    std::fs::create_dir_all(dir).unwrap();
    gix::init(dir).expect("gix init");
    let cfg = dir.join(".git").join("config");
    let mut body = std::fs::read_to_string(&cfg).expect("read .git/config");
    body.push_str(&format!(
        "\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
    ));
    std::fs::write(&cfg, body).expect("write .git/config");
}

#[test]
fn import_nonexistent_path_errors_cleanly() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd
        .args(["import", "/definitely/not/a/real/path/shoka-it"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("not a directory"),
        "expected 'not a directory' in stderr, got: {stderr}"
    );
}

#[test]
fn import_regular_file_path_errors_cleanly() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let file = tmp.path().join("not-a-dir.txt");
    std::fs::write(&file, "I am a file, not a dir.\n").unwrap();
    let assertion = cmd
        .args(["import", file.to_str().expect("utf-8 temp path")])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("not a directory"),
        "expected 'not a directory' in stderr, got: {stderr}"
    );
}

#[test]
fn import_empty_dir_succeeds_with_zero_imported_summary() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let assertion = cmd
        .args(["import", empty.to_str().expect("utf-8 temp path")])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("0 imported"),
        "expected '0 imported' in stdout, got: {stdout}"
    );
}

#[test]
fn import_finds_repos_and_records_them_on_shelf() {
    // Deferred from PR #16 (CodeRabbit): exercise the success path
    // — a real `.git/config` with a parseable remote URL is found,
    // parsed, and ends up on the shelf.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let source = tmp.path().join("source");
    init_git_repo_with_remote(
        &source.join("foo-proj"),
        "https://github.com/yukimemi/foo-proj.git",
    );
    init_git_repo_with_remote(
        &source.join("nested").join("bar-tool"),
        "git@github.com:other-owner/bar-tool.git",
    );

    let assertion = cmd
        .args(["import", source.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("2 imported"),
        "expected '2 imported' in stdout, got: {stdout}"
    );

    // Verify the shelf got both entries — read state.toml directly so
    // we catch issues with the on-disk format, not just the in-memory
    // summary.
    let state = tmp.path().join("state").join("state.toml");
    let body = std::fs::read_to_string(&state).expect("state.toml exists");
    assert!(body.contains("\"yukimemi\""), "missing first owner: {body}");
    assert!(body.contains("\"foo-proj\""), "missing first name: {body}");
    assert!(
        body.contains("\"other-owner\""),
        "missing second owner: {body}"
    );
    assert!(body.contains("\"bar-tool\""), "missing second name: {body}");
}

#[test]
fn clone_with_invalid_input_errors_cleanly() {
    // Single-token input has no shape we accept (not a URL, not an
    // `owner/name` shorthand). Should surface a clear error before
    // any network IO.
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd.args(["clone", "justaname"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("neither a URL"),
        "expected shape error in stderr, got: {stderr}"
    );
}

#[test]
fn clone_refuses_to_overwrite_a_non_empty_destination() {
    // Pre-populate the destination path that `clone foo/bar` would
    // pick. Clone must refuse to clobber it, before any network IO.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let dest = tmp
        .path()
        .join("root")
        .join("github.com")
        .join("foo")
        .join("bar");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("leftover.txt"), "I was here first\n").unwrap();

    let assertion = cmd.args(["clone", "foo/bar"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("already exists") || stderr.contains("not empty"),
        "expected occupied-dest error in stderr, got: {stderr}"
    );
}

#[test]
fn cd_with_empty_shelf_errors_cleanly() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd.args(["cd", "anything"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("shelf is empty"),
        "expected empty-shelf hint in stderr, got: {stderr}"
    );
}

#[test]
fn cd_no_matching_hint_errors_cleanly() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    let assertion = cmd.args(["cd", "no-such-repo"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("no repos on the shelf match"),
        "expected no-match error in stderr, got: {stderr}"
    );
}

#[test]
fn cd_unique_hint_prints_path_to_stdout() {
    // The cd output contract: stdout is exactly the resolved path, so
    // the shell wrapper's `cd "$(shoka cd $args)"` actually lands in
    // the right place. Anything chatty (banners, slugs) belongs on
    // stderr or not at all.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    // The ghq layout puts the clone under `<root>/github.com/yukimemi/shoka`.
    // Create that directory so the existence check passes.
    let expected = tmp
        .path()
        .join("root")
        .join("github.com")
        .join("yukimemi")
        .join("shoka");
    std::fs::create_dir_all(&expected).unwrap();

    let assertion = cmd.args(["cd", "shoka"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout)
        .trim_end()
        .to_string();
    // Compare as canonical paths so trailing slashes / case differences
    // on Windows don't trip the assertion.
    assert_eq!(
        std::fs::canonicalize(&stdout).unwrap(),
        std::fs::canonicalize(&expected).unwrap(),
        "stdout should be the resolved path"
    );
}

#[test]
fn cd_writes_path_to_sidechannel_when_env_var_is_set() {
    // The shell wrapper sets SHOKA_CD_OUT to a temp file and reads
    // the path from there, so `inquire`'s stdout-writing prompt UI
    // doesn't get captured by the wrapper's command substitution.
    // Verify the sidechannel actually works: with the env var set,
    // stdout must stay empty and the path lands in the named file.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    let expected = tmp
        .path()
        .join("root")
        .join("github.com")
        .join("yukimemi")
        .join("shoka");
    std::fs::create_dir_all(&expected).unwrap();
    let out_file = tmp.path().join("cd-out");

    let assertion = cmd
        .env("SHOKA_CD_OUT", &out_file)
        .args(["cd", "shoka"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.is_empty(),
        "stdout should be empty when SHOKA_CD_OUT is set, got: {stdout:?}"
    );
    let from_file = std::fs::read_to_string(&out_file).expect("sidechannel file written");
    assert_eq!(
        std::fs::canonicalize(from_file.trim()).unwrap(),
        std::fs::canonicalize(&expected).unwrap(),
        "sidechannel should hold the resolved path"
    );
}

#[test]
fn cd_unknown_on_disk_path_errors_cleanly() {
    // Stale shelf entry: the repo is listed, but its clone path
    // doesn't actually exist on disk. cd must surface that as a
    // shoka error (not let the shell discover it with a confusing
    // `cd: No such file`).
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "ghost-repo")],
    );
    let assertion = cmd.args(["cd", "ghost-repo"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("doesn't exist"),
        "expected stale-entry hint in stderr, got: {stderr}"
    );
}

#[test]
fn exec_with_no_command_errors_cleanly() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd.arg("exec").assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("no command"),
        "expected missing-command error in stderr, got: {stderr}"
    );
}

#[test]
fn exec_with_empty_shelf_errors_cleanly() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    // assert_cmd interprets the literal `--` so we have to use args()
    // rather than the std splitting.
    let assertion = cmd.args(["exec", "--", "true"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("shelf is empty"),
        "expected empty-shelf hint in stderr, got: {stderr}"
    );
}

#[test]
fn exec_with_filter_flag_errors_with_phase2_pointer() {
    // `--filter` parses (forward compat) but currently bails — the
    // status snapshot it would consult lives in the Phase 2 cache
    // work. The error message has to point at that, so users
    // understand it's intentional.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    let assertion = cmd
        .args(["exec", "--filter", "dirty", "--", "true"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("Phase 2"),
        "expected phase-2 pointer in stderr, got: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn exec_runs_command_in_each_repo_cwd() {
    use std::os::unix::fs::PermissionsExt;

    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[
            ("github.com", "yukimemi", "repo-a"),
            ("github.com", "yukimemi", "repo-b"),
        ],
    );
    let dir_a = tmp.path().join("root/github.com/yukimemi/repo-a");
    let dir_b = tmp.path().join("root/github.com/yukimemi/repo-b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    // Verify cwd handling via `pwd` — universally available on Unix
    // shells. Each repo's banner should appear in stdout, and each
    // captured pwd output should be that repo's clone path.
    let assertion = cmd.args(["exec", "--", "pwd"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(stdout.contains("repo-a"), "missing repo-a banner: {stdout}");
    assert!(stdout.contains("repo-b"), "missing repo-b banner: {stdout}");
    // Path output from pwd: at minimum the leaf dir should show up.
    assert!(
        stdout.contains("/repo-a") && stdout.contains("/repo-b"),
        "pwd output missing one of the repo paths: {stdout}"
    );
    // Avoid the unused-import warning when the test binary is built
    // for an OS without the symbol.
    let _ = std::fs::Permissions::from_mode;
}

#[cfg(windows)]
#[test]
fn exec_runs_command_in_each_repo_cwd_windows() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[
            ("github.com", "yukimemi", "repo-a"),
            ("github.com", "yukimemi", "repo-b"),
        ],
    );
    let dir_a = tmp.path().join("root/github.com/yukimemi/repo-a");
    let dir_b = tmp.path().join("root/github.com/yukimemi/repo-b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    // `cmd /c cd` prints the current working directory — `chdir` is
    // an alias and `cd` without args prints. We invoke it via cmd.exe
    // since `cd` isn't a standalone .exe on Windows.
    let assertion = cmd
        .args(["exec", "--", "cmd", "/c", "cd"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(stdout.contains("repo-a"), "missing repo-a banner: {stdout}");
    assert!(stdout.contains("repo-b"), "missing repo-b banner: {stdout}");
}

#[test]
fn exec_propagates_failure_exit_code() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    let dir = tmp.path().join("root/github.com/yukimemi/shoka");
    std::fs::create_dir_all(&dir).unwrap();

    // `cargo --bogus-flag` exits non-zero on every platform that has
    // cargo (it's in PATH on rust CI runners). Falling back to any
    // ubiquitously-failing command isn't really portable, so prefer
    // a tool we know is there. If cargo isn't on PATH this test
    // would mis-diagnose (spawn-error path), which is at least a
    // visible failure rather than silent skip.
    let assertion = cmd
        .args(["exec", "--", "cargo", "--this-flag-does-not-exist"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("1 of 1 repo(s) failed") || stderr.contains("failed"),
        "expected per-repo failure summary in stderr, got: {stderr}"
    );
}

#[test]
fn prune_empty_shelf_is_a_no_op() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd.args(["prune", "--dry-run"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("shelf is empty"),
        "expected empty-shelf message, got: {stdout}"
    );
}

#[test]
fn prune_no_stale_entries_is_a_no_op() {
    // All shelf entries have their clone paths on disk → nothing to
    // prune. The exit code should still be 0 and the output should
    // report 0 candidates.
    let (mut cmd, tmp) = cmd_with_isolated_config();
    seed_shelf(
        &tmp.path().join("state"),
        &[("github.com", "yukimemi", "shoka")],
    );
    let path = tmp.path().join("root/github.com/yukimemi/shoka");
    std::fs::create_dir_all(&path).unwrap();
    let assertion = cmd.args(["prune", "--dry-run"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("0 of 1 repo(s) have missing clone paths"),
        "expected 0/1 stale report, got: {stdout}"
    );
}

#[test]
fn prune_dry_run_lists_candidates_without_modifying_shelf() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let state_dir = tmp.path().join("state");
    seed_shelf(
        &state_dir,
        &[
            ("github.com", "yukimemi", "alive"),
            ("github.com", "yukimemi", "ghost"),
        ],
    );
    // Only `alive`'s clone path exists.
    std::fs::create_dir_all(tmp.path().join("root/github.com/yukimemi/alive")).unwrap();

    let assertion = cmd.args(["prune", "--dry-run"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("1 of 2 repo(s) have missing clone paths"),
        "expected 1/2 stale report, got: {stdout}"
    );
    assert!(
        stdout.contains("yukimemi/ghost"),
        "expected stale slug in output, got: {stdout}"
    );
    assert!(
        stdout.contains("dry run"),
        "expected dry-run hint, got: {stdout}"
    );

    // Shelf must be untouched — count still 2.
    let body = std::fs::read_to_string(state_dir.join("state.toml")).unwrap();
    assert!(body.contains("alive"), "alive entry lost: {body}");
    assert!(body.contains("ghost"), "ghost entry lost: {body}");
}

#[test]
fn prune_yes_removes_stale_entries() {
    let (mut cmd, tmp) = cmd_with_isolated_config();
    let state_dir = tmp.path().join("state");
    seed_shelf(
        &state_dir,
        &[
            ("github.com", "yukimemi", "alive"),
            ("github.com", "yukimemi", "ghost"),
        ],
    );
    std::fs::create_dir_all(tmp.path().join("root/github.com/yukimemi/alive")).unwrap();

    let assertion = cmd.args(["prune", "--yes"]).assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        stdout.contains("removed 1 stale entry"),
        "expected 1-removed summary, got: {stdout}"
    );

    let body = std::fs::read_to_string(state_dir.join("state.toml")).unwrap();
    assert!(body.contains("alive"), "alive should remain: {body}");
    assert!(
        !body.contains("ghost"),
        "ghost should be gone after prune --yes: {body}"
    );
}

/// Smoke test that the binary builds, parses args, and exits cleanly
/// for `--help`. Catches surface-level clap regressions early without
/// poking at any specific subcommand.
#[test]
fn help_prints_subcommands_and_exits_zero() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_shoka"));
    let mut cmd = Command::new(bin);
    let assertion = cmd.arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    for expected in ["clone", "list", "import", "cache", "doctor", "self-update"] {
        assert!(
            stdout.contains(expected),
            "--help should mention `{expected}` subcommand, got: {stdout}"
        );
    }
}
