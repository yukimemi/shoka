# AGENTS.md

Guidance for AI agents (Claude / Codex / Gemini) working in this
repo. The yukimemi/* shared conventions live in the
`<!-- kata:agents:* -->` blocks below, sourced from
`yukimemi/pj-base` / `pj-rust` / `pj-rust-cli` via `kata apply` —
see those for git workflow, PR review cycle, build/lint/test
commands, release flow, and renri worktree usage.

The sections above the marker blocks are shoka-specific and
consumer-owned: edit them freely; `kata apply` won't touch them.

## What shoka is

A repository workspace manager — a modern, jj-aware successor to
[`ghq`](https://github.com/x-motemen/ghq) and
[`rhq`](https://github.com/ubnt-intrepid/rhq), written in Rust.
Clones repos into a flat `<root>/<host>/<owner>/<name>` layout,
lets you fuzzy-`cd` between them, runs git / jj commands in
parallel across the whole shelf, and surfaces working-state at a
glance via a TUI dashboard.

Name comes from 書架 (shoka, "bookshelf") — repositories sit on
the shelf, `shoka tui` lets you see the whole shelf at a glance,
and `shoka cd` is "pull a volume off the shelf".

## Design pillars (don't rediscover)

- **jj-first, git-friendly.** Repos carry a `vcs` flag
  (`auto` / `git` / `jj`); `shoka exec` auto-routes to the right
  binary so a single `shoka exec -- fetch` works regardless of
  which VCS each repo uses. ghq / rhq are git-only — this is the
  primary feature gap shoka closes.
- **TUI dashboard is the headline UX.** `shoka tui` opens a
  lazygit-style overview: branch, ahead/behind, dirty count, open
  PRs, CI status, last activity, all per-repo. Enter on a row =
  `cd` into that repo. Phase 2 deliverable, but the data model is
  designed for it from day one.
- **Profiles / workspaces.** `work` / `personal` / `oss` profiles
  scope `root`, default host, clone protocol (`https` vs `ssh`),
  and per-profile `git config` injection. Active profile is
  selectable via `--profile` or `SHOKA_PROFILE`.
- **ghq layout compatibility.** Reads existing `~/ghq/...` trees
  without migration. `shoka import` adopts them in place. The
  flat `<root>/<host>/<owner>/<name>` layout is non-negotiable so
  ghq habits port directly.
- **renri-aware.** Worktrees managed by
  [`renri`](https://github.com/yukimemi/renri) appear as children
  of their main repo in `list` / `tui`, not as duplicate
  top-level entries. `shoka cd` includes them as cd targets.
- **AGENTS.md aware.** A repo with `AGENTS.md` gets a 🤖 marker
  in the dashboard; `shoka list --has-agents` filters to it.
  Small affordance, large payoff in an AI-heavy workflow.
- **Config = TOML + Tera (`teravars`).** Follows the
  `[vars]`-self-ref convention shared by kanade / renri / kata.
- **Shell integration for `cd`.** A child process can't change
  the parent shell's cwd, so `shoka cd` prints the chosen path to
  stdout and a small shell function (`function s { Set-Location
  (shoka cd $args) }` for PowerShell, equivalents for bash / zsh)
  wraps it. Same approach ghq and rhq use; the wrapper script
  ships with the crate.

## Roadmap

- **Phase 1 — MVP CLI.** `clone`, `list` (text summary), `cd`
  (fuzzy via skim or nucleo), `exec` (tokio-parallel git/jj),
  `prune` (stale candidates), `import` (ghq tree adoption). Shell
  integration scripts (PowerShell / bash / zsh) for `cd`.
- **Phase 2 — TUI dashboard.** `shoka tui` (ratatui +
  crossterm), cached status layer (sled or json), `gh` CLI
  integration for open PRs / CI status / contribution graph.
- **Phase 3 — Polish & power features.** OSC 7 cwd hint for
  terminals that support it — done (`cd` / `tui` emit it via the
  `SHOKA_CD_OUT` sidechannel branch in `commands::cd::emit_path`).
  Scaffolding (`shoka new` = `gh repo create` + clone + kata init)
  — done (`commands::new`; reuses `clone::clone_and_record`, preset
  from `--preset` / `[global.new].preset`). Contribution-graph column
  — done (`tui` activity sparkline from `gh.rs` weekly commit counts
  via `/stats/commit_activity`, cached in `GhSnapshot.weekly_commits`).
  Still open: Profiles, bulk org-move follow.

## Open design questions

- TUI status cache invalidation strategy: filesystem mtime poll
  vs `notify` watch vs explicit refresh.
- Concurrency cap on `exec`: hard limit, CPU count, or
  configurable per-profile.
- Whether `cd` recents are per-profile or global.
- jj detection heuristic: `.jj/` presence is authoritative, but
  what about colocated `.git` + `.jj`? Probably honor an explicit
  per-repo `vcs` override.

## Useful invocations (planned)

```sh
shoka clone github.com/foo/bar       # → <root>/github.com/foo/bar
shoka list                            # text summary with status glyphs
shoka cd                              # fuzzy select + cd (via shell wrapper)
shoka tui                             # phase 2 — full dashboard
shoka exec -- git fetch               # parallel across all repos
shoka exec --filter dirty -- git status
shoka prune                           # propose stale / merged candidates
shoka import ~/ghq                    # adopt an existing ghq tree
```

## Testing policy

Practice **TDD** (red-green-refactor). Unit tests live in
`src/<mod>.rs::tests`; integration tests (CLI invocation via
`assert_cmd`) live in `tests/cli.rs`. Tests arrive in the same
commit as the behaviour they cover.

## Version + changelog

Version lives only in `Cargo.toml`. `cargo check` refreshes
`Cargo.lock` after a bump. Commit titles follow
`<type>: <summary> (vX.Y.Z)` (e.g. `feat: add cd subcommand
(v0.2.0)`) so the release surface is traceable from `git log`.

## Regenerating the demo GIF (`vhs/`)

The README GIF is recorded with [vhs](https://github.com/charmbracelet/vhs)
from `vhs/demo.tape`. **Always record through the Docker image**
(`vhs/Dockerfile`) — native vhs hangs on Windows at `Set Theme`,
and the image also bakes in the sandbox fixtures (four shallow
`yukimemi/*` clones + a pre-staged config) so recordings are
reproducible. Entry point:

```sh
cargo make vhs-regen      # builds the shoka-vhs image if needed, then records
```

On Windows the cargo-make tasks shell out via `wsl -- docker ...`
(Docker Engine inside WSL2; Docker Desktop not required).

Checklist when re-recording:

1. **Bump `ARG SHOKA_VERSION` in `vhs/Dockerfile`** to the latest
   released version (it's pinned in lockstep with release tags;
   the image `cargo install`s that version from crates.io). A
   stale pin re-records yesterday's binary. Image rebuild takes
   ~5 min.
2. **Forward `GITHUB_TOKEN`** so the dashboard's PR / CI columns
   populate instead of rendering `-`. From PowerShell in one
   shot (WSLENV makes the var cross the WSL boundary):

   ```powershell
   $env:GITHUB_TOKEN = (gh auth token); $env:WSLENV += ":GITHUB_TOKEN/u"; cargo make vhs-regen
   ```

   (Append with `+=` — overwriting `WSLENV` would drop any other
   var-forwarding rules already configured in the session; a
   leading `:` when it was unset is harmless. Same form as the
   `$PROFILE` snippet documented in `Makefile.toml`.)

   `regen.sh` forwards the token conditionally — never pass
   `-e GITHUB_TOKEN=""` to a raw `docker run` (empty-but-set is
   not the same as unset).
3. **Don't replace the tape's `Wait` lines with fixed sleeps.**
   The shell sections sync on the prompt via `Wait` (with
   `Set WaitTimeout 120s`) because `shoka cache refresh` duration
   varies wildly — the gh `/stats/commit_activity` endpoint can
   202-spin for a while. Fixed sleeps desync and produce a
   garbage recording (typeahead bleeding into the next command).
   The TUI section has no prompt to `Wait` on, so it stays
   sleep-driven by design.
4. **Verify the GIF before committing.** Sanity-check the file
   size against the previous `demo.gif` (a broken recording once
   came out at ~150 KB vs ~565 KB), and extract a few frames to
   eyeball — e.g. via dockerized ffmpeg:

   ```sh
   # from the repo root — demo.gif lands in vhs/, so mount that
   docker run --rm -v "$PWD/vhs:/work" -w /work linuxserver/ffmpeg \
     -i demo.gif -vf "select='not(mod(n,60))'" -vsync 0 frame_%02d.png
   ```
5. **Reclaim disk afterwards** — the image is multi-GB and cheap
   to rebuild: `docker rmi shoka-vhs` (plus
   `docker builder prune` if space is tight).

<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- **Releases are PR-driven, tagging is automatic.** Bump
  `[workspace.package].version` (workspace) or `[package].version`
  (single crate) in a `chore/release-vX.Y.Z` PR. On merge to `main`,
  `.github/workflows/auto-tag.yml` (kata-managed) detects the bump,
  pushes the `vX.Y.Z` tag, and that tag fires `release.yml` for
  binary builds + crates.io publish. **Do not run `git tag` by
  hand** — the bot tag will collide and the manual push fails.

### PR review cycle

- Every PR runs reviews from **Claude Code**
  (`.github/workflows/claude-review.yml`, kata-managed) and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved. The claude-review workflow skips
  review-exempt PRs by itself (its job-level `if:` excludes
  `chore/release-*`, `kata-apply/auto`, `apm-bump/auto`, and
  Renovate / Dependabot authors) — a missing Claude review on
  those PRs is expected, not a failure.
- **The Claude full review fires once, at PR open** (plus
  `ready_for_review` / `reopened`) — fix pushes do **not** re-trigger
  it (`synchronize` is deliberately off the trigger list; a full
  re-review per push doubled up with the mention-driven re-check
  below and burned tokens for no extra signal). Verification of
  fixes rides the `@claude` thread replies. After a large rework
  that changes the PR's shape, request a fresh full pass
  explicitly: `@claude please re-review the full PR`. CodeRabbit
  still reviews pushes on its own cadence (its app config, not
  this workflow).
- **After opening a PR, immediately enter the review-monitoring
  loop — do not ask the user whether to start it.** Drive the
  cadence with `/loop` — fixed-interval mode (e.g.
  `/loop 60s …`) schedules ticks via `CronCreate`; dynamic mode
  (no interval, `/loop …`) self-paces via `ScheduleWakeup`. The
  agent actively pulls fresh state each tick with
  `gh pr view <N> --json state,reviews,comments,statusCheckRollup`
  and `gh api repos/<owner>/<repo>/pulls/<N>/comments` (the
  latter covers inline review comments, which `gh pr view`
  does not surface) and reacts to new bot feedback. Passive
  watchers (background `gh` polls, file watchers, hooks) cannot
  trigger active follow-up, so they are not a substitute —
  without an active wake-up the agent never re-reads the PR.
- **Default polling interval: 60s.** Claude Code review /
  CodeRabbit typically reply within ~1–5 minutes of a push or
  thread reply, so a 60s tick catches them on the next wake-up
  without burning cache: 60s sits well inside the 5-minute
  prompt-cache TTL, so the conversation context stays cached
  across ticks. Do **not** stretch the interval to 300s — that
  is the worst-of-both window (you pay the cache miss without
  amortizing it). If the PR is idle but a bot re-review is still
  expected (e.g. a CodeRabbit rate-limit refill window), step
  **up** to 1200–1800s instead.
- **Stop the loop entirely when only owner approval is missing.**
  Once review bots are quiet (or quiet-by-exception — version-bump
  skip, Renovate/Dependabot skip), CI is green, and there is no
  other expected follow-up, the *only* remaining action is human
  approval. GitHub already notifies the owner; the agent
  re-entering on every cron tick to find the same "still waiting
  on owner" state burns cache and adds no value. Stop scheduling
  further wake-ups (`CronDelete` in fixed-interval mode; simply
  omit the next `ScheduleWakeup` in dynamic mode) and report the
  wait state to the user. The owner restarts the loop after their
  next push if a fresh bot pass is wanted, or merges directly.
  (A CodeRabbit rate-limit window doesn't qualify on its own — a
  re-review is still expected once the quota refills, so step up
  to 1200–1800s instead and let it ride. Stopping is only correct
  when the owner has explicitly chosen to skip the bot pass per
  the rate-limit exception below.)
- **Reply to reviewers after pushing a fix.** Reply on the
  corresponding review thread with an **@-mention**
  (`@claude` / `@coderabbitai`). Silent fixes are invisible to
  reviewers and cost the audit trail. Note `@claude` also
  triggers the interactive responder
  (`.github/workflows/claude.yml`, kata-managed) — it will
  re-check the fix and reply on the thread. Since fix pushes no
  longer re-trigger the full review, this mention-driven re-check
  is the **only** Claude-side verification of a fix — don't skip
  it for substantive fixes; do skip it for pure FYI notes that
  need no verification.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.
- **Version-bump-only PRs** (a single `chore/release-vX.Y.Z`
  branch whose entire diff is `[workspace.package].version` /
  `[package].version` + the matching inter-crate refs +
  `Cargo.lock`) **also skip the bot-review gate.** There is
  nothing for the bots to find in a version bump, and the
  release pipeline downstream of merge (auto-tag → release.yml)
  is time-sensitive. CI green + owner approval is enough.
- **Treat CodeRabbit rate-limit notices as "quiet" for the
  merge gate.** If CodeRabbit only posts a "Review limit
  reached" quota-exhaustion message (no findings, no inline
  comments), it has produced no review content — there is
  nothing to address. Re-trigger with `@coderabbitai review`
  once the quota refills if you want a real pass; for small or
  time-sensitive PRs, merge on owner approval without waiting.

### Worktree workflow

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name> --from main@origin            # create a worktree (jj-first), off latest upstream main
renri --vcs git add <branch-name> --from origin/main  # force a git worktree, off latest upstream main
renri remove <branch-name> -y --non-interactive  # cleanup after merge (agent-safe; see note)
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

**Always pass `--from <upstream main>`** (`main@origin` for jj,
`origin/main` for git). Without it, `renri add` forks off the *cwd
worktree's current HEAD* — in a long-lived main checkout that often
lags upstream, so the PR later shows up CONFLICTING against a `main`
that had already moved (e.g. a refactor merged upstream before the
branch was cut), forcing a manual re-port of the whole change.
`renri add` does fetch first, but fetching only updates `main@origin`
— it never moves the checkout's HEAD, so an explicit `--from` is what
guarantees a fresh base.

**Agents / non-interactive shells:** `renri remove` prints a details
panel and waits for a confirmation prompt — without `-y` it **hangs**,
and `--non-interactive` *alone* errors asking for `-y`. Always pass
`-y`, and add `--non-interactive` so a mistyped/omitted name fails
instead of opening a fuzzy picker (the same picker-fallback applies to
`remove` / `cd` / `exec` with no name). Use `-f`/`--force` to remove a
worktree that still has uncommitted changes or conflicts. To sweep
every merged-PR worktree in one shot: `renri remove --merged -y`.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead. The marker scopes are layered:

- `kata:agents:base:*` — language-agnostic conventions (this section).
- `kata:agents:rust:*` — added when `pj-rust` applies.
- `kata:agents:rust-cli:*` — added when `pj-rust-cli` applies.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/release-vX.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: release vX.Y.Z"
git push -u origin chore/release-vX.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

Releases are triggered by a Cargo.toml version bump landing on
`main`. The bump flow itself (PR with automerge → `auto-tag.yml`
pushes `vX.Y.Z` → `release.yml` runs) is documented in
`kata:agents:rust:*` under "Releasing: version bump PR +
auto-tag" — that block also covers the `KATA_APPLY_TOKEN` and
`delete_branch_on_merge` setup. What `release.yml` then does for
a **CLI** crate:

1. Cross-compiles binaries for x86_64 Linux / Windows / macOS,
   plus aarch64 macOS (Apple Silicon) — full triples
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
   `x86_64-apple-darwin`, `aarch64-apple-darwin`.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first release. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across CLIs using these templates unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.

### Release smoke target (`examples/smoke.rs`)

After `cargo build --release`, `release.yml` runs
`cargo run --release --target <T> --example smoke` on every build
matrix entry. `cargo test` runs only library code, so the produced
binary's startup path goes unverified — that's how shoka v0.10.0
shipped a rustls `CryptoProvider` panic to crates.io even though
all 13 CI checks were green.

The template's default `examples/smoke.rs` body is intentionally
no-op so kata can drop it into every consumer crate without
breaking releases. **Override it per crate** with the smallest
operation that exercises the regression-prone surface:

- HTTPS-using CLIs: build the API client (octocrab, reqwest, etc.)
  and issue a tiny no-auth GET — that forces the rustls handshake
  to run inside the same binary the release publishes.
- File-handling CLIs: write+read a temp file via the real I/O
  helpers (catches missing crate features, permission regressions).
- Pure library crates: leave as no-op.

A failing smoke blocks the release before publishing to GitHub
Releases / crates.io.
<!-- kata:agents:rust-cli:end -->
