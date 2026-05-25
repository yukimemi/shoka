//! Integration tests for the `shoka` CLI.
//!
//! Uses `cargo run` via `env!("CARGO_BIN_EXE_shoka")` plus
//! [`assert_cmd::Command`] so we get the standard assertion DSL
//! without rolling our own `Command` wrapper. Tests stage their
//! fixtures under [`tempfile::TempDir`] so the user's real
//! `$XDG_CONFIG_HOME/shoka` is never touched.
//!
//! Background: shoka's bg-refresh wiring (`commands::dispatch`)
//! spawns `shoka cache refresh --background` as a detached child
//! after most commands. To keep these tests fast and hermetic we
//! point `$SHOKA_CONFIG` at a temp file with
//! `background_refresh = false` so the bg spawn short-circuits.

use std::io::Write;
use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::TempDir;

/// Build an `assert_cmd::Command` pre-seeded with `SHOKA_CONFIG`
/// pointing at a hermetic config that disables background refresh.
/// Returns the command + the tempdir guard (drop to clean up).
fn cmd_with_isolated_config() -> (Command, TempDir) {
    let tmp = TempDir::new().expect("temp dir");
    let cfg = tmp.path().join("config.toml");
    let mut f = std::fs::File::create(&cfg).expect("write config.toml");
    f.write_all(
        br#"
[global]
root = "/tmp/shoka-it-root"

[global.cache]
background_refresh = false
"#,
    )
    .expect("write config body");

    let mut cmd = Command::cargo_bin("shoka").expect("binary built");
    cmd.env("SHOKA_CONFIG", &cfg)
        // Strip any inherited profile so the test exercises the
        // default code path regardless of the dev's environment.
        .env_remove("SHOKA_PROFILE");
    (cmd, tmp)
}

#[test]
fn import_nonexistent_path_errors_cleanly() {
    let (mut cmd, _tmp) = cmd_with_isolated_config();
    let assertion = cmd
        .args(["import", "/definitely/not/a/real/path/shoka-it"])
        .assert()
        .failure();
    // The error wording is part of the user-visible contract: when
    // the path doesn't exist or isn't a directory, we say so.
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
    // Walking an empty directory should succeed cleanly with a
    // zero-imported summary — not error out. Guards against an
    // overly-strict "must find at least one repo" check creeping in.
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

/// Smoke test that the binary builds, parses args, and exits cleanly
/// for `--help`. Catches surface-level clap regressions early without
/// poking at any specific subcommand.
#[test]
fn help_prints_subcommands_and_exits_zero() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_shoka"));
    let mut cmd = Command::new(bin);
    let assertion = cmd.arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    for expected in ["clone", "list", "import", "cache", "doctor"] {
        assert!(
            stdout.contains(expected),
            "--help should mention `{expected}` subcommand, got: {stdout}"
        );
    }
}
