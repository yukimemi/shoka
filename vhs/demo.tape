# shoka demo — README GIF
# Usage:
#   Native (Linux / macOS): cd vhs && vhs demo.tape
#   Docker (recommended — and required on Windows where native vhs
#   hangs on Set Theme):
#     cd vhs && docker build -t shoka-vhs . && bash regen.sh
#   regen.sh handles GITHUB_TOKEN forwarding conditionally — passing
#   `-e GITHUB_TOKEN=""` to a raw `docker run` would define the var
#   as an empty string inside the container, which is subtly
#   different from "unset"; the helper script avoids that pitfall.
#   Or via cargo-make: `cargo make vhs-regen` (wraps the same flow).
#
# Sandbox: SHOKA_CONFIG / SHOKA_STATE_DIR / SHOKA_CACHE_DIR all point
# at /tmp/shoka-demo so the tape never touches the host's real shoka
# state. The Dockerfile pre-clones four small yukimemi/* fixture
# repos into /tmp/shoka-demo/root/ at image build time so the tape
# can show real branch + dirty + ahead/behind + (with GITHUB_TOKEN)
# PR / CI columns without needing network during the recording.
#
# Flow:
#   1. shoka import — adopt the pre-staged fixture tree
#   2. shoka list   — text summary of the shelf
#   3. shoka cache refresh --force — populate git_status + gh
#   4. shoka tui    — dashboard tour (j/k, /, Enter)
#
# Total runtime ~62s (the cache-refresh wait is hidden, see §4).

Output demo.gif
Set Shell "bash"
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 1100
Set Height 700
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"
# Sync on the shell prompt instead of guessing with fixed sleeps —
# `cache refresh` hits the live gh API (commit_activity can 202-spin
# while GitHub computes stats) and `import` walks real clones, so
# wall time varies run to run. A 2026-06 re-record desynced badly
# enough that the tui keystrokes landed in the shell and the
# dashboard never appeared in the GIF.
Set WaitTimeout 120s

# ── sandbox setup (hidden) ──
# Point shoka at /tmp/shoka-demo for the whole tape. The Dockerfile
# already pre-staged config.toml + fixture clones under that path,
# so the tape just exports the env vars and shoka picks them up.
Hide
Type "export SHOKA_CONFIG=/tmp/shoka-demo/config/config.toml"
Enter
Sleep 100ms
Type "export SHOKA_STATE_DIR=/tmp/shoka-demo/state"
Enter
Sleep 100ms
Type "export SHOKA_CACHE_DIR=/tmp/shoka-demo/cache"
Enter
Sleep 100ms
# Fresh shelf every run — clear any prior state.toml.
Type "rm -f $SHOKA_STATE_DIR/state.toml $SHOKA_CACHE_DIR/cache.toml"
Enter
Sleep 200ms
Type "clear"
Enter
Sleep 200ms
Show

# ── 1. version banner ──
Sleep 500ms
Type "shoka --version"
Sleep 500ms
Enter
Wait
Sleep 1200ms

# ── 2. import the fixture tree ──
Type "shoka import /tmp/shoka-demo/root"
Sleep 500ms
Enter
Wait
Sleep 1500ms

# ── 3. list shows the shelf ──
Type "shoka list"
Sleep 500ms
Enter
Wait
# Hold so the viewer can actually read the shelf summary.
Sleep 3000ms

# ── 4. cache refresh populates git_status + gh ──
# With GITHUB_TOKEN set, this also fetches open PR count + CI status
# for the github.com/yukimemi/* repos. The wait is wrapped in
# Hide/Show: the per-repo gh round-trips (commit_activity can
# 202-spin for tens of seconds) are real wall time but dead air in
# the GIF, so the viewer sees "command → done" while `Wait` still
# prompt-syncs the recording under the hood.
Type "shoka cache refresh --force"
Sleep 500ms
Enter
Hide
Wait
Show
# Hold the completed output just long enough to read it.
Sleep 1500ms

# ── 5. tui — the headline feature ──
# No prompt to `Wait` on once the alternate screen opens, so this
# section stays sleep-driven — but the shell is prompt-synced at
# this point, so the only latency left is the tui's own startup.
Type "shoka tui"
Sleep 500ms
Enter
# Let the dashboard settle so the viewer sees the populated columns.
Sleep 3000ms

# Navigate down through the shelf.
Type "j"
Sleep 700ms
Type "j"
Sleep 700ms
Type "j"
Sleep 1000ms

# Open the filter input.
Type "/"
Sleep 500ms
# Type a few chars to show live nucleo filtering.
Type "ren"
Sleep 1800ms
# Esc clears the filter and drops back to normal mode.
Escape
Sleep 800ms

# Pop the `?` help overlay — shows the full keybind cheat sheet
# (j/k, /, Enter, i/p pickers, f/P fetch+push, y/o yank+open, …).
# Held longer than other prompts so the viewer can actually read
# the 15-row table.
Type "?"
Sleep 3500ms
Escape
Sleep 700ms

# Jump down a row, then quit with q (we don't want the tape to
# trigger a real cd via SHOKA_CD_OUT — it's not set in the demo
# env anyway, so Enter would just print the path).
Type "j"
Sleep 600ms
Type "q"
Sleep 800ms

# ── 6. wrap up ──
Type "clear"
Enter
Sleep 200ms
Type "# shoka — your repository bookshelf"
Sleep 800ms
Enter
Sleep 1500ms
