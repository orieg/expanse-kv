//! In-memory storage engine for `expanse-kv`.
//!
//! Maps strings, hashes, sorted sets, bitmaps and the expiry index onto a
//! bounded set of shared `expanse-trie` maps through the composite key codecs
//! in `expanse-kv-core`. It never allocates one trie per logical collection
//! (AGENTS.md §2.2), and a cursor never holds a reclamation pin across a
//! caller's I/O (AGENTS.md §2.5).
//!
//! Nothing is implemented yet; `docs/PRD.md` Phase 2 states the scope and gate.
