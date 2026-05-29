# Image for regenerating vhs/demo.gif on machines where native vhs is
# unreliable (Windows in particular — vhs hangs on `Set Theme` there).
#
# Build (from this directory):
#   docker build -t shoka-vhs .
#
# Run (regenerate demo.gif into the current dir):
#   docker run --rm \
#     -v "$PWD:/vhs" \
#     -e GITHUB_TOKEN="$GITHUB_TOKEN" \
#     shoka-vhs demo.tape
#
# GITHUB_TOKEN is optional — without it the gh-dependent columns
# (PR / CI) show "-" in the dashboard. With a token (any read scope
# is enough for public repos), the columns populate with real data
# from the github.com/yukimemi/* fixture repos baked into the image.

FROM ghcr.io/charmbracelet/vhs:latest

# Toolchain for shoka (Rust) + git for fixture clones.
# build-essential / pkg-config / libssl-dev are needed by some of
# shoka's transitive Rust deps (reqwest + rustls + gix).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        build-essential \
        pkg-config \
        libssl-dev \
        unzip \
        fontconfig \
    && rm -rf /var/lib/apt/lists/*

# PlemolJP Console NF — the font demo.tape uses (Set FontFamily ...).
# Without this, vhs renders the GIF with generic glyphs and the dashboard
# Unicode (↑↓ ✓ ✗ ◐ ○ ●) turns into tofu. Pinned to a known release;
# bump the ARG when upstream tags a newer version.
ARG PLEMOLJP_VERSION=v3.0.0
RUN curl -fsSL -o /tmp/plemoljp_nf.zip \
        "https://github.com/yuru7/PlemolJP/releases/download/${PLEMOLJP_VERSION}/PlemolJP_NF_${PLEMOLJP_VERSION}.zip" \
    && mkdir -p /usr/share/fonts/truetype/plemoljp \
    && unzip -j /tmp/plemoljp_nf.zip "*.ttf" -d /usr/share/fonts/truetype/plemoljp \
    && fc-cache -f \
    && rm -f /tmp/plemoljp_nf.zip

# Rust (minimal profile — no docs, no clippy, etc. — just rustc + cargo).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# shoka — pinned to the version this Dockerfile was tested against.
# Bump in lockstep with the release tag so old images don't silently
# lag behind the GIF's apparent state. Override with
# `--build-arg SHOKA_VERSION=X.Y.Z` when re-recording against an
# unreleased branch.
ARG SHOKA_VERSION=0.14.1
RUN cargo install shoka --version "${SHOKA_VERSION}" --locked

# Sandbox layout. The tape exports SHOKA_CONFIG / SHOKA_STATE_DIR /
# SHOKA_CACHE_DIR pointing here so the dashboard renders against
# *these* fixtures, never the host's real shelf.
ENV SHOKA_SANDBOX=/tmp/shoka-demo
RUN mkdir -p \
        ${SHOKA_SANDBOX}/config \
        ${SHOKA_SANDBOX}/state \
        ${SHOKA_SANDBOX}/cache \
        ${SHOKA_SANDBOX}/root/github.com/yukimemi

# Pre-staged config: ghq-layout root pointing at /tmp/shoka-demo/root,
# bg refresh off (we want the tape's explicit `cache refresh` to be
# the only refresh in the recording).
RUN printf '%s\n' \
        '[global]' \
        'root = "/tmp/shoka-demo/root"' \
        'default_host = "github.com"' \
        'default_protocol = "https"' \
        '' \
        '[global.cache]' \
        'background_refresh = false' \
    > ${SHOKA_SANDBOX}/config/config.toml

# Pre-clone four small yukimemi/* repos into the sandbox so `shoka
# import` finds something to adopt, `cache refresh` has real .git
# data to read (branch / ahead-behind / dirty), and the gh capture
# returns real PR / CI numbers when GITHUB_TOKEN is set at run time.
# Shallow clones keep the image lean (-> ~30 MB instead of hundreds).
# Note the explicit `|| exit 1`: a POSIX `sh` `for` loop's exit
# status is the status of its *last* command, so a mid-loop clone
# failure (transient network, deleted repo) would let the RUN
# succeed with a partially-staged sandbox. Forcing per-iteration
# exit on failure makes the build crash visibly instead.
RUN for repo in shoka renri kata teravars; do \
        git clone --depth 1 --quiet \
            "https://github.com/yukimemi/${repo}.git" \
            "${SHOKA_SANDBOX}/root/github.com/yukimemi/${repo}" \
        || exit 1; \
    done

# Default working dir; bind-mount the host vhs/ here at run time.
WORKDIR /vhs
