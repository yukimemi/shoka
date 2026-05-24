//! OS-aware path resolution.
//!
//! Uses the `directories` crate to locate:
//!
//! - **config**: `$XDG_CONFIG_HOME/shoka/` on Unix, `%APPDATA%\shoka\` on Windows
//! - **state / cache**: `$XDG_DATA_HOME/shoka/` on Unix, `%LOCALAPPDATA%\shoka\` on Windows
//!
//! `$SHOKA_CONFIG` (CLI `--config`) overrides the config file path
//! when set.
//!
//! Phase 1 implementation pending.
