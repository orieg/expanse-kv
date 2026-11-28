//! Wire protocol and key codecs for `expanse-kv`.
//!
//! This crate is `no_std` with `alloc`, has no dependencies, and contains no
//! `unsafe` (`AGENTS.md` §2.6 and §2.7). It is the home of:
//!
//! - the order-preserving IEEE-754 `f64` score codec ([`score`]);
//! - the composite key codecs and type-prefix registry ([`key`]);
//! - the RESP2 and RESP3 streaming parser and encoder (Phase 1).

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod key;
pub mod score;
