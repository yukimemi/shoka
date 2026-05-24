//! Volatile per-repo cache (last_accessed, status snapshot, gh PR
//! counts, …). Lives at `$XDG_DATA_HOME/shoka/cache.toml` and is
//! deliberately excluded from `shoka export` — anything in here is
//! reproducible from the working tree + remote state.
//!
//! Phase 2 deliverable.
