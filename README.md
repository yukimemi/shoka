<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg" />
    <img src="assets/logo.svg" width="520" alt="shoka 書架 — your repository bookshelf" />
  </picture>
</p>

<p align="center"><em>your repository bookshelf.</em></p>

A repository workspace manager written in Rust. A modern,
jj-aware successor to [ghq](https://github.com/x-motemen/ghq)
and [rhq](https://github.com/ubnt-intrepid/rhq), with a TUI
dashboard as its headline feature.

> [!WARNING]
> **Status: pre-alpha.** Design phase. No usable functionality
> yet. The shape below is the planned MVP, not what `cargo
> install shoka` currently gives you.

## Why another one

`ghq` and `rhq` solved "where do I clone things" beautifully, but
they're git-only and stop at `list` / `look`. shoka picks up from
there:

- **jj as a first-class VCS** alongside git
- **TUI dashboard** to see every repo's working state at a glance
- **Profiles** to keep `work` / `personal` / `oss` separated
- **renri worktree-aware** so worktrees show up under their parent
- **AGENTS.md / CLAUDE.md aware** for AI-heavy workflows
- **ghq layout compatible** — drop-in for existing `~/ghq/...`

## Planned commands

```sh
shoka clone github.com/foo/bar       # → <root>/github.com/foo/bar
shoka list                            # text summary with status glyphs
shoka cd                              # fuzzy select + cd (shell wrapper)
shoka tui                             # full dashboard (phase 2)
shoka exec -- git fetch               # parallel across all repos
shoka exec --filter dirty -- git status
shoka prune                           # stale / merged candidates
shoka import ~/ghq                    # adopt an existing ghq tree
```

## Roadmap

- **Phase 1 — MVP CLI.** `clone` / `list` / `cd` / `exec` /
  `prune` / `import`, plus shell integration scripts.
- **Phase 2 — TUI dashboard.** ratatui + crossterm, cached
  status, `gh` integration (open PRs / CI / contribution graph).
- **Phase 3 — Polish.** Profiles, scaffolding, bulk org-move
  follow, OSC 7 cwd hint.

## License

MIT — see [LICENSE](./LICENSE).
