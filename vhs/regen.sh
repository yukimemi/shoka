#!/usr/bin/env bash
# Regenerate vhs/demo.gif (and any other tape passed in $1) using the
# `shoka-vhs` Docker image built from the sibling Dockerfile.
#
# Run from WSL bash. From PowerShell you can do:
#   wsl --cd /mnt/c/Users/<you>/src/.../shoka/vhs -- bash regen.sh
#
# Requires:
#   - docker (real `docker` or our `wsl docker` PowerShell wrapper)
#   - shoka-vhs image built: `docker build -t shoka-vhs .`
#   - GITHUB_TOKEN (optional but recommended) — populates the PR / CI
#     columns in the dashboard. Without it those cells render "-" in
#     the GIF, which still works but looks less compelling. Any token
#     scope reaches the public repos the fixture uses; the rate limit
#     bumps from 60 req/h to 5 000 req/h, plenty for one recording.

set -euo pipefail

TAPE="${1:-demo.tape}"

cd "$(dirname "$0")"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
    echo "warning: GITHUB_TOKEN not set — PR / CI columns will render '-' in the GIF." >&2
    echo "         set it to get real data: https://github.com/settings/tokens" >&2
fi

exec docker run --rm \
    -v "$PWD:/vhs" \
    -e GITHUB_TOKEN="${GITHUB_TOKEN:-}" \
    shoka-vhs "$TAPE"
