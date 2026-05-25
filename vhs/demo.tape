# shoka demo — README GIF
# Usage:
#   Native (Linux / macOS): cd vhs && vhs demo.tape
#   Docker (recommended on Windows — native vhs hangs on Set Theme):
#     cd vhs && docker build -t shoka-vhs . && \
#       docker run --rm -v "$PWD:/vhs" -e GITHUB_TOKEN shoka-vhs demo.tape
#     (or just `bash regen.sh` from this directory.)
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
# Total runtime ~70s.

Output demo.gif
Set Shell "bash"
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 1100
Set Height 700
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

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
Sleep 1200ms

# ── 2. import the fixture tree ──
Type "shoka import /tmp/shoka-demo/root"
Sleep 500ms
Enter
Sleep 2500ms

# ── 3. list shows the shelf ──
Type "shoka list"
Sleep 500ms
Enter
Sleep 3500ms

# ── 4. cache refresh populates git_status + gh ──
# With GITHUB_TOKEN set, this also fetches open PR count + CI status
# for the github.com/yukimemi/* repos.
Type "shoka cache refresh --force"
Sleep 500ms
Enter
# Cache refresh hits the gh API per repo — gives it a beat to land.
Sleep 5000ms

# ── 5. tui — the headline feature ──
Type "shoka tui"
Sleep 500ms
Enter
# Let the dashboard settle so the viewer sees the populated columns.
Sleep 2500ms

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
