//! Benchmark harness for `expanse-kv`.
//!
//! Home of the analytical bounds models (`AGENTS.md` §8.8), statistical gating
//! primitives (`AGENTS.md` §8.4), workload drivers, comparator arms,
//! differential oracle and provenance capture that every published figure
//! resolves to (`AGENTS.md` §8). A comparator arm is a real process started by
//! the harness; an arm that did not run has no number (`AGENTS.md` §8.9).

pub mod bounds;
pub mod scaling;
pub mod stats;
