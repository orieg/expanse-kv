//! Persistence for `expanse-kv`: the append-only log, its replay and rewrite,
//! and checksummed snapshots (`docs/PRD.md` §7 Phase 4).
//!
//! This crate contains no `unsafe` (`AGENTS.md` §2.7): it parses bytes read
//! back from disk, which are untrusted input.

#![forbid(unsafe_code)]

pub mod aof;
pub mod crc64;
pub mod snapshot;
