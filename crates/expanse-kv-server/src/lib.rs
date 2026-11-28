//! `expanse-kv` async TCP server, RESP2/RESP3 command router, persistence
//! wiring, and Prometheus metrics (`docs/PRD.md` §7 Phase 3).

#![forbid(unsafe_code)]

pub mod config;
pub mod metrics;
pub mod router;
pub mod server;
