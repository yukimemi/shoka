//! Git / Jujutsu (`jj`) abstraction.
//!
//! A repo carries an optional `vcs` flag (`auto` / `git` / `jj`); the
//! `auto` resolver inspects `.jj/` and `.git/` to decide. The `Vcs`
//! trait fronts both backends so callers (status, fetch, exec) stay
//! agnostic.
//!
//! Phase 1 implementation pending.
