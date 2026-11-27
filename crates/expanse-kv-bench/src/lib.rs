//! Benchmark harness for `expanse-kv`.
//!
//! Home of the workload drivers, the comparator arms, the differential oracle
//! and the provenance capture that every published figure resolves to
//! (AGENTS.md §8). A comparator arm is a real process started by the harness;
//! an arm that did not run has no number (AGENTS.md §8.9).
//!
//! Nothing is implemented yet; `docs/PRD.md` §6 states the method.
