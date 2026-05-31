<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg" />
    <img src="assets/logo.svg" width="520" alt="shoka 書架 — your repository bookshelf" />
  </picture>
</p>

<p align="center"><em>your repository bookshelf.</em></p>

<p align="center">
  <a href="https://crates.io/crates/shoka"><img src="https://img.shields.io/crates/v/shoka.svg" alt="crates.io" /></a>
  <a href="https://github.com/yukimemi/shoka/actions/workflows/ci.yml"><img src="https://github.com/yukimemi/shoka/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license MIT" /></a>
</p>

**shoka** (書架, _"bookshelf"_) is a repository workspace manager
written in Rust — a modern, jj-aware successor to
[`ghq`](https://github.com/x-motemen/ghq) and
[`rhq`](https://github.com/ubnt-intrepid/rhq). It clones repos into
a flat `<root>/<host>/<owner>/<name>` layout, lets you fuzzy-`cd`
between them, runs commands in parallel across the whole shelf, and
surfaces every repo's working state at a glance via a TUI
dashboard.

<p align="center">
  <img src="vhs/demo.gif" alt="shoka demo — import, list, cache refresh, and the TUI dashboard" />
</p>

## Why another one

`ghq` and `rhq` nailed "where do I clone things" — but they're
git-only and stop at `list` / `look`. shoka picks up from there:

- **jj as a first-class VCS** alongside git — `shoka clone` /
  `exec` work for both, no `ghq-jj` shim needed.
- **TUI dashboard** (Phase 2) for the whole shelf at a glance.
- **Profiles** to keep `work` / `personal` / `oss` separated.
- **Routes** for per-org clone destinations + per-route VCS / protocol.
- **`AGENTS.md` aware** for AI-heavy workflows (`shoka list --has-agents`).
- **ghq layout compatible** — drop-in for existing `~/ghq/...`
  trees via `shoka import`.
- **In-process git via `gix`** — no `git` subprocess fan-out, so
  `import` over a large shelf stays fast even on Windows where
  `CreateProcess` dominates the cost.

## Install

```sh
cargo install shoka
```

Pre-built binaries for Linux / macOS / Windows are attached to each
[GitHub release](https://github.com/yukimemi/shoka/releases).

## Quick start

```sh
# 1. (optional) adopt an existing ghq tree
shoka import ~/ghq

# 2. or clone fresh
shoka clone yukimemi/shoka          # → <root>/github.com/yukimemi/shoka

# 3. see what's on the shelf
shoka list

# 4. jump into one (with the shell wrapper installed — see below)
s shoka

# 5. run something across everything (or a tag-filtered subset)
shoka exec --tag rust -- cargo check
```

## Commands

| Command | What it does |
| :--- | :--- |
| `shoka clone <spec>` | Clone a repo via `gix` (or `jj git clone` when `vcs = jj`). Accepts full URLs, `owner/name`, or `host/owner/name`. |
| `shoka new [owner/name]` | Scaffold a new repo: `gh repo create` (public by default; `--private` to opt out) + clone onto the shelf + optional `kata init` (preset from `--preset` or `[global.new].preset`; `--no-kata` to skip). |
| `shoka import <dir>` | Walk a directory, find existing `.git/` repos, and adopt them onto the shelf. |
| `shoka list` | Print the shelf with optional `--tag` / `--has-agents` filters. |
| `shoka cd [hint]` | Resolve a repo to its on-disk path (use the shell wrapper to actually `cd`). |
| `shoka exec -- <cmd>` | Run `<cmd>` in each matching repo in parallel; output is captured + banner-headed per repo. |
| `shoka prune` | Drop shelf entries whose clone path is missing on disk. `--dry-run` to rehearse; `--yes` to skip the prompt. |
| `shoka cache {refresh,show,clear}` | Per-repo volatile cache. Auto-refreshed in the background after most commands. |
| `shoka doctor` | Diagnose environment + config. |
| `shoka init-shell <shell>` | Print a shell wrapper that claims the `shoka` name and chdirs the parent shell on `cd` / `tui` (see below). |
| `shoka completion <shell>` | Print a shell-completion script. |
| `shoka tui` | TUI dashboard — branch / ↑↓ / dirty / PR / CI / activity columns from the cached snapshot (activity is a per-repo, self-normalised sparkline of the last ~12 weeks' commits); j/k navigation, `/` for nucleo filter, Enter exits and emits the chosen repo's path so the shell wrapper can `cd` to it. |

`shoka exec` is the transparent escape hatch: shoka never
interprets the command. `shoka exec -- git fetch`, `shoka exec --
jj git fetch`, `shoka exec -- cargo check` all work the same way.
Output from each repo is captured and printed as a banner-headed
block when the process exits, so parallel runs don't interleave
into nonsense.

## Shell integration

A child process can't chdir its parent shell — kernel rule, no
escape on any OS shoka cares about. So `shoka init-shell` emits a
shell function that claims the `shoka` name itself, intercepts the
`cd` and `tui` subcommands (which need to chdir the parent), and
transparently passes every other subcommand through to the binary.
The user sees one `shoka` they can run any subcommand against; the
wrapper is invisible until they touch `cd` or `tui`.

Behavior of the installed function:

- `shoka` *(no args)* — opens the TUI dashboard, Enter selects and
  cds into the highlighted repo.
- `shoka cd [hint]` — fuzzy picker (or direct hint), cds on
  selection. The path travels out-of-band via a `SHOKA_CD_OUT`
  sidechannel file so the interactive prompt UI can render normally.
- `shoka tui [--tag …]` — same as bare `shoka`, with explicit args.
- `shoka <anything-else>` — passes through to the binary
  unchanged (`shoka clone <url>`, `shoka list`, `shoka exec …`,
  `shoka --version`, …).

### PowerShell

```pwsh
# one-shot (current session):
shoka init-shell powershell | Out-String | Invoke-Expression

# persistent (append to $PROFILE — create it first if missing,
# which it is on a fresh PowerShell install):
if (!(Test-Path $PROFILE)) { New-Item -Type File -Path $PROFILE -Force | Out-Null }
shoka init-shell powershell | Add-Content $PROFILE
. $PROFILE
```

### bash / zsh

```sh
# bash — add to ~/.bashrc:
eval "$(shoka init-shell bash)"

# zsh — add to ~/.zshrc:
eval "$(shoka init-shell zsh)"
```

### fish

```fish
# ~/.config/fish/config.fish:
shoka init-shell fish | source
```

Pass `--name s` (or any other identifier) to install the function
under a shorter alias instead of shadowing `shoka`. The alias
behaves the same — `s` opens the TUI, `s cd <hint>` picks, `s
clone <url>` passes through.

## Configuration

`config.toml` lives at the OS-standard config dir (Linux
`$XDG_CONFIG_HOME/shoka/`, macOS
`~/Library/Application Support/yukimemi.shoka/`, Windows
`%APPDATA%\yukimemi\shoka\config\`). A starter is auto-written on
first run, so `root` is the only key you usually need to touch.

Minimal example:

```toml
[global]
root = "~/src"
```

### Reference

#### `[global]`

| Key | Default | Description |
| :--- | :--- | :--- |
| `root` | _(required)_ | Filesystem root under which repos are cloned. Set here or in the active profile. |
| `layout` | `"{{ root }}/{{ host }}/{{ owner }}/{{ name }}"` | Path-layout template, rendered at clone time. Context: `root`, `host`, `owner`, `name`, `profile`, `vcs`, `protocol`. |
| `default_vcs` | `"auto"` | `"auto"` / `"git"` / `"jj"`. |
| `default_protocol` | `"https"` | `"https"` / `"ssh"`. |
| `default_host` | `"github.com"` | Host assumed when a spec omits it (e.g. `owner/name`). |
| `default_profile` | _(none)_ | Profile used when neither `--profile` nor `$SHOKA_PROFILE` is set. |
| `exec_concurrency` | `8` | Max parallel jobs for `shoka exec`. Floored at 1. |

#### `[global.ui]`

| Key | Default | Description |
| :--- | :--- | :--- |
| `status_cache_ttl_secs` | `60` | How long a cached per-repo status snapshot is considered fresh. |
| `tui_refresh_ms` | `250` | TUI redraw interval in milliseconds. |
| `own_owners` | `[]` | Owners treated as "yours". When non-empty the TUI starts in mine-only mode and `m` toggles to the full shelf; empty hides the toggle. |
| `cd_page_size` | `15` | Rows the `shoka cd` fuzzy picker shows at once. Floored at 1. |

#### `[global.shell]`

| Key | Default | Description |
| :--- | :--- | :--- |
| `cd_command_name` | `"s"` | Default function name emitted by `shoka init-shell <shell>` (overridable with `--name`). |

#### `[global.cache]`

| Key | Default | Description |
| :--- | :--- | :--- |
| `background_refresh` | `true` | Spawn a detached background refresh after most commands. When `false`, only `shoka cache refresh` updates the cache. |
| `refresh_threshold_secs` | `60` | Skip a per-repo refresh if it ran within this many seconds. `--force` bypasses it. |
| `parallel_repos` | `8` | Cap on concurrent per-repo refresh tasks. Floored at 1. |

#### `[[routes]]`

Clone-destination routing, evaluated top-to-bottom at clone time —
the first matching route wins. A matched route's set fields override
the corresponding `[global]` values; unset fields fall through.

| Key | Description |
| :--- | :--- |
| `pattern` | `host:<host>`, `host:<host>/<owner>`, or `/<regex>/` (Rust `regex` crate). |
| `root` | Clone root for matching repos. |
| `layout` | Per-route layout template override. |
| `default_vcs` | Per-route VCS override. |
| `default_protocol` | Per-route protocol override. |

#### `[profiles.<name>]`

Selected via `--profile <name>` / `$SHOKA_PROFILE` /
`global.default_profile`. Set fields override `[global]`; unset
fields fall through.

| Key | Description |
| :--- | :--- |
| `root` | Profile clone root. When set, the profile claims clone routing and `[[routes]]` are skipped. |
| `layout` | Profile layout template. |
| `default_vcs` | Profile VCS default. |
| `default_protocol` | Profile protocol default. |
| `default_host` | Profile host default. |
| `exec_concurrency` | Profile `shoka exec` concurrency. |
| `git_config` | `key = "value"` pairs (a `[profiles.<name>.git_config]` table) injected as `git config` per-repo (e.g. `"user.email" = "work@example.com"`). |

### Full example

```toml
[global]
root = "~/src"
default_host = "github.com"
default_protocol = "https"
default_vcs = "auto"
exec_concurrency = 8

[global.ui]
own_owners = ["yukimemi"]
cd_page_size = 15

[global.shell]
cd_command_name = "s"

[global.cache]
background_refresh = true
refresh_threshold_secs = 60
parallel_repos = 8

[[routes]]
pattern = "host:github.com/mycompany"
root = "~/src/work"
default_protocol = "ssh"

[profiles.work]
default_host = "github.mycompany.com"

[profiles.work.git_config]
"user.email" = "work@example.com"
```

Precedence for clone destinations is `profile.root` (when set) >
first matching `[[routes]]` entry > `[global].root`.

### Layering & templating

Files in the same dir matching `config.*.toml` are layered on top
(alphabetical order; `config.local.toml` last wins). Point at a
specific file with `--config PATH` or `$SHOKA_CONFIG`. All values
flow through [`teravars`](https://github.com/yukimemi/teravars) so a
`[vars]` table (with helpers like `home()` / `env(name=...)` /
`is_windows()`) self-references and resolves before deserialization.

## Roadmap

- **Phase 1 — CLI MVP** ✅ `clone` / `import` / `list` / `cd` /
  `exec` / `prune` / `cache`, shell integration, completion. Done.
- **Phase 2 — TUI dashboard** ✅ `ratatui` + `crossterm` + `nucleo`
  fuzzy. Per-repo cached status (branch / ahead-behind / dirty)
  from `gix`, open PR count + CI status from `octocrab`. Done.
- **Phase 3 — Polish.** OSC 7 cwd hint ✅ (`cd` / `tui` announce the
  picked repo's dir so new tabs/splits inherit it). `shoka new`
  scaffolding ✅ (`gh repo create` + clone + `kata init`).
  Contribution-graph column ✅ (per-repo commit-activity sparkline in
  the TUI). Still open: per-profile route overrides, bulk org-move
  follow.

## Development

```sh
cargo make check        # fmt + clippy + locked check + tests
cargo make pre-release  # the whole check suite, exactly as CI runs it
```

PRs go through [Gemini Code Assist](https://gemini.google.com/) +
[CodeRabbit](https://coderabbit.ai/) reviewers on top of standard
CI. The yukimemi/* convention is:
[`renri`](https://github.com/yukimemi/renri) worktrees + `kata
apply` for the shared template, see [AGENTS.md](./AGENTS.md) for
the details.

## License

MIT — see [LICENSE](./LICENSE).
