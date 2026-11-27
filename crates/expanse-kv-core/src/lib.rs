//! Wire protocol and key codecs for `expanse-kv`.
//!
//! This crate is `no_std` with `alloc`, has no dependencies, and contains no
//! `unsafe` (AGENTS.md §2.6 and §2.7). It is the home of:
//!
//! - the RESP2 and RESP3 parser and encoder;
//! - the composite key codecs that map each logical type onto the shared
//!   tries, and the registry of their type-prefix bytes;
//! - the order-preserving `f64` score codec.
//!
//! Nothing is implemented yet; `docs/PRD.md` Phase 1 states the scope and gate.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;
