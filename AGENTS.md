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

## Homebrew tap

`brew install yukimemi/tap/shoka` is served by a formula in the
`yukimemi/homebrew-tap` repo, kept current by the consumer-owned
`.github/workflows/homebrew.yml` (NOT kata-managed, so edits there
survive `kata apply`). It triggers on `workflow_run` after the
`Release` workflow finishes — **not** `release: published`: release.yml
creates the GitHub Release with the default `GITHUB_TOKEN`, and GitHub
won't let a `GITHUB_TOKEN`-authored event trigger another workflow (the
same recursion guard that makes `auto-tag.yml` push its tag with a PAT),
so a `release`-triggered job would never fire. By the time `Release`
reports success its assets are uploaded, so the job checksums the
Apple-Silicon + Linux-x86_64 tarballs, renders `Formula/shoka.rb`, and
pushes it to the tap.

The tap repo already exists — `shun` publishes a GUI Cask
(`Casks/shun.rb`) there via the same pattern, and shoka's CLI
`Formula/shoka.rb` coexists with it. The only prerequisite is a
`HOMEBREW_TAP_PAT` secret on this repo (a PAT with `contents: write` on
the tap). **The name matches `shun`'s release workflow on purpose** so
one PAT covers both; if it's set at the org/user level it's inherited,
otherwise `gh secret set HOMEBREW_TAP_PAT`. Without it the job logs a
warning and no-ops, so a release is never blocked.

shun bumps its single-`.dmg` cask in place with `perl`; shoka renders
the whole formula instead, because its two platform blocks (macOS arm64
+ Linux x86_64) carry distinct `sha256`s that a blanket in-place
substitution would clobber. shun also inlines the tap push into its own
`release.yml`; shoka can't (its `release.yml` is kata-managed), hence
the standalone `homebrew.yml`.

To backfill the formula for a release cut before the workflow existed,
run it manually: `gh workflow run homebrew.yml -f tag=vX.Y.Z`.

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
- **Releases are PR-driven and tagging is automatic** — in repos that
  ship a release pipeline. Bump the version in the project's own
  manifest in a `chore/release-vX.Y.Z` PR; on merge to `main` the
  language layer's `auto-tag.yml` detects the bump, pushes the
  `vX.Y.Z` tag, and that tag is what fires `release.yml`. **Do not run
  `git tag` by hand** — the bot tag will collide and the manual push
  fails. The specifics belong to the layers shipping those two
  workflows, which are not the same layer: `kata:agents:rust:*` for
  which file holds the version and for `auto-tag.yml`,
  `kata:agents:rust-{cli,lib}:*` for what `release.yml` builds and
  publishes. A repo with no `auto-tag.yml` has no release pipeline at
  all: nothing tags, and the version field in its manifest may well
  be decoration.

### Pre-merge review

Review happens **before the pull request, on the operator's machine**,
via [magi](https://github.com/yukimemi/magi). This layer no longer
ships PR-side review bots: `claude-review.yml` and `claude.yml` were
removed from it. Their scope was
human-authored PRs — their own job-level `if:` already excluded
`chore/release-*`, `kata-apply/auto`, `apm-bump/auto` and
Renovate / Dependabot — which is exactly the set magi reviews, so
keeping them meant reviewing the same diff twice, a
`CLAUDE_CODE_OAUTH_TOKEN` secret per repository, Actions minutes on
private repos, and one trap that silently cost reviews: a PR editing
either workflow was skipped by `claude-code-action`'s
workflow-validation check and merged with a green check and no
review attached.

**"Removed" is a statement about this template layer, not about
every repo's current state.** Dropping a `[[file]]` entry stops kata
from managing the rendered file — it does not delete it. A repo that
had these workflows before this change keeps `claude-review.yml` /
`claude.yml` (and the `CLAUDE_CODE_OAUTH_TOKEN` secret) under
`.github/workflows/` until someone deletes them by hand, and until
then they still fire on every human-authored PR. Check
`.github/workflows/` before treating a PR as unreviewed-except-magi:
if either file is still there, its comments are a real review, not
noise to ignore.

- **`magi review <branch>`** runs only the review + verification +
  gate half of magi's graph: nothing competes, no implementation, no
  judging, no vote. That is the mode for hand-written work.
  `magi run "<task>"` is the full competition, for work handed over
  whole. Both end at the same gate.
- What the loop actually does: each reviewer gets its **own detached
  worktree pinned at the commit under review** (no reviewer can
  perturb the tree, and the fixer never races one); `verify.e2e` runs
  in the branch's worktree and its output is fed to the fixer;
  finding ids (`R2-1-3`) are assigned by magi, not by the agent, so
  the fixer's adoption report can be matched against them; the loop
  is bounded by `review_rounds`; `verify.gate` must exit 0 before any
  merge is attempted.
- **`magi.toml` is repo-owned, not kata-managed.** Point
  `verify.gate` at the exact command CI runs, so a local pass means a
  green PR, and point `verify.e2e` at the invocation that actually
  covers the repo — feature flags included. A gate that differs from
  CI turns a clean magi run into a red PR, which is the one failure
  this arrangement cannot absorb.
- **If you did not run magi, the change was not reviewed, and nothing
  will tell you.** Do not open a PR for a hand-written change before
  `magi review` comes back clean; if you must, say so in the PR body
  and say why. What does *not* count as a substitute: a green CI run
  (it compiles and tests, it does not review), and CodeRabbit's
  silence.
- **CodeRabbit stays installed and is not part of the gate.** It does
  not auto-review repositories under 10 stars — the common case here —
  so treat it as absent unless it posts. When it does post, its
  findings are a real review: address them, reply **in the inline
  thread** with an `@coderabbitai` mention (the review-comment
  *replies* endpoint,
  `gh api repos/<owner>/<repo>/pulls/<N>/comments/<id>/replies -f body=…`),
  and reply even when declining — say why, because a silent skip
  reads as overlooked. A "review limit reached" quota notice carries
  no findings and counts as quiet; re-trigger with
  `@coderabbitai review` when the quota refills if you want a real
  pass.
- **Read the report, not the exit status.** A reviewer seat that
  times out is logged as `WARN agent timed out seat=review-2` and
  then summarised as "raised 0 finding(s)" — indistinguishable from a
  genuinely clean pass in both the summary and `magi stats`. Check
  for timeouts before believing a clean round: a round where half the
  panel never answered is not a clean round.
- **Review artifacts stay local.** magi comments on a pull request
  only when it *stops* landing one. Findings, the fixer's adoption
  report and reviewer precision live in the run directory
  (`magi show`, `magi stats`). When the PR needs a record — a
  non-obvious fix, a finding declined with an argument — paste that
  part into the PR body or a comment yourself.
- With `merge = "pr"`, magi opens the pull request and keeps going:
  watches the checks, reads the review comments (human and bot), runs
  a bounded fix round when either is unhappy, pushes, and asks before
  merging. `land_approval` is on by default and **silence is a
  hold** — nothing merges unanswered. `magi answer` (or the web UI)
  is where it asks. Out of rounds leaves the PR open with a comment
  saying what still fails; `checks: unknown` never merges.
- **Merge gate**: magi's gate green — or CI green for a change magi
  never touched — **and** every review that did post resolved (a
  leftover `claude-review.yml`, CodeRabbit, a human) **and** the
  owner's explicit approval. The irreversible step stays a human
  decision.
- **No review-monitoring poll loop for bots this layer no longer
  ships.** The old loop existed to wait on them. Where a repo still
  has `claude-review.yml` (see above) the old cadence still applies
  until it is deleted; otherwise, after opening a PR wait for CI and
  report the wait state to the owner. When magi is landing the PR
  (`land = true`), magi does the watching.
- Bot-authored PRs (Renovate / Dependabot) need no review pass at
  all: CI green + owner approval.
- **Version-bump-only PRs** — a single `chore/release-vX.Y.Z` branch
  whose entire diff is `[workspace.package].version` /
  `[package].version` plus the matching inter-crate refs and the
  lockfile — likewise. There is nothing in a version bump for a
  reviewer to find, and the release pipeline downstream of merge
  (auto-tag → `release.yml`) is time-sensitive.

### Worktree workflow

> **Before your FIRST edit to any file, run `renri add` — NEVER edit the
> main checkout.** Read-only inspection (Read / Grep / Glob) stays on the
> main checkout; the instant you intend to *change* a file, you must
> already be in a worktree. The trap that keeps catching agents: diving
> into a fix the moment the diagnosis lands and editing in place. A
> concurrent agent shares the main checkout — your in-place edits will
> clobber theirs or be clobbered, and in a jj-colocated repo a stray
> working-copy commit entangles unrelated WIP into your branch. If you
> slip and edit in the main checkout, capture the diff first (jj already
> snapshotted it into the working-copy commit, so `jj diff > patch`; for
> git, `git stash` or save a patch — if you got as far as committing on a
> branch, just push it). Then reset the main checkout to pristine main
> (`jj new main@origin`, or `git switch -`), `renri add` a worktree, and
> re-apply the captured diff there.

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
…) instead.

The marker scopes are layered, one per applied layer:
`kata:agents:base:*` is this section, and each layer adds its own
(`kata:agents:rust:*`, `kata:agents:rust-cli:*`,
`kata:agents:pnpm:*`, `kata:agents:firebase:*`, …). Which ones apply
*here* is a grep away: `<!-- kata:` in this file.

### This project's own conventions

Everything a layer ships is generic by construction: it describes the
stack the template assumed, not what this repo grew into. **Bytes
outside every marker pair are yours and survive `kata apply`** — so
project-specific conventions belong in a section of their own, outside
the markers (conventionally at the end of the file; if a later layer
appends its block below yours, no matter — kata only ever rewrites
between its own markers). Same mechanism as the `.gitignore` /
`.gitattributes` blocks.

Write those conventions down there rather than leaving them in one
agent's head, in commit archaeology, or in a README the agent will not
read. What earns a line:

- **Any layer default that does not hold here.** A layer states its
  assumption flatly ("Hosting is the primary target", "these rules are
  a placeholder to replace"). When the project has diverged, say so and
  say why — the layer's text keeps asserting the opposite on every
  apply, and an agent that only reads the blocks will act on it.
- **Facts duplicated across files with no compiler in between** — an
  address or a path that appears in code *and* in a rules/config file
  that cannot import it, a timeout that has to stay inside another
  timeout. List every copy, so the next edit finds them all.
- **kata-shipped files this project deleted on purpose**, together with
  the `once_applied = true` line in `.kata/applied.toml` that keeps
  them deleted. Otherwise someone helpfully restores one.
- **Shapes the runtime forces but no tool checks** — an export form a
  platform requires, import specifiers that must (or must not) carry a
  file extension, a directory whose contents are reachable by URL.
- **Invariants that money or access rest on**, naming the file and line
  that actually enforces them.
- **Which language the code speaks versus what a user reads**, when the
  two differ.

A repo whose `AGENTS.md` is nothing but kata blocks is a repo where
every agent re-derives all of that from scratch — and gets the layer
defaults wrong the same way each time.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # editorconfig-check + fmt --check + clippy + test + lock-check (the pre-push gate)
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

**In a workspace, the version is in more than one place.** A member
that is published and depended on by another member is declared
with both a `path` and a `version` — crates.io needs a
requirement it can resolve for somebody who is not building from
the checkout, so a bare `path` will not do:

```toml
my-core = { path = "crates/my-core", version = "0.4.2" }
```

That literal does not follow `[workspace.package] version`.
Nothing in Cargo makes it, and the release above will not either.

**It fails late and quietly.** `version = "0.4.2"` means `^0.4.2`,
so a stale pin keeps resolving through every *patch* release and
stops only at the first bump that crosses the minor — where
`cargo build` refuses with `candidate versions found which didn't
match`, in the middle of cutting the release. Two repos on these
templates hit exactly this, one of them three releases after its
pins were last correct, and the other had already written the
hazard down in prose and drifted anyway.

So bump the pins in the same commit, keep them in
`[workspace.dependencies]` rather than in each member, and assert
it rather than remembering it. A test is the cheapest place —
`cargo test` already runs in CI, and it needs no toolchain a Rust
workspace does not have. [pj-rust-workspace's
README](https://github.com/yukimemi/pj-rust-workspace#the-internal-version-pin-and-the-check-for-it)
carries one to copy into any member's
`tests/check_versions.rs`: `internal_pins_match_the_workspace_version`
fails when a pin and the workspace version disagree, and
`members_inherit_the_workspace_version` fails when a member writes
its own version or reaches for a sibling by path.

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

1. Cross-compiles binaries for **three** targets — full triples
   `x86_64-unknown-linux-musl`, `x86_64-pc-windows-msvc`,
   `aarch64-apple-darwin`. Linux is musl (statically linked, so the
   binary runs on any glibc vintage); the Linux job installs
   `musl-tools` first. Intel Mac (`x86_64-apple-darwin`) is
   deliberately **not** built — Apple Silicon only.
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
