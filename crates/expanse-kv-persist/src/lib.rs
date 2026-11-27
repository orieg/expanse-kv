//! Persistence for `expanse-kv`: the append-only log, its replay and rewrite,
//! and checksummed snapshots.
//!
//! This crate contains no `unsafe` (AGENTS.md §2.7): it parses bytes read back
//! from disk, which are untrusted input.
//!
//! Nothing is implemented yet; `docs/PRD.md` Phase 4 states the scope and gate.

#![forbid(unsafe_code)]
