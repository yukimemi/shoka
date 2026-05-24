//! GitHub integration.
//!
//! Token resolution order:
//! 1. `$GITHUB_TOKEN`
//! 2. `gh auth token` (if `gh` CLI is on PATH)
//!
//! If neither yields a token, gh-dependent features (PR / CI / 草) are
//! disabled at runtime; core commands keep working.
//!
//! API calls go through `octocrab` for async batching. `gh` CLI itself
//! is *not* a runtime dependency — only used to harvest auth.
//!
//! Phase 1 (clone listing) / phase 2 (TUI status) implementation pending.
