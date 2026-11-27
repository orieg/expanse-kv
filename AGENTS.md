# AGENTS.md — Engineering & Quality Guide for expanse-kv

This document is the operating contract for every AI coding agent (Claude,
Gemini, Antigravity, Copilot, Cursor, Codex, Aider and others) and every human
contributor working in this repository.

> **Canonical source.** `AGENTS.md` is the single agent guide. `CLAUDE.md` and
> `GEMINI.md` are **symlinks** to it, and `.github/copilot-instructions.md` is a
> pointer back here. Edit `AGENTS.md` only; never replace a symlink with a copy.
>
> **Citing this file.** Section numbers are stable rule IDs, cited as
> `AGENTS.md §2.5` from CI, tests and docs. Never renumber one; add a new ID.

---

## 1. Mission & Identity

**expanse-kv** is a memory-safe, multi-threaded, in-memory key-value store and
cache that speaks the Redis wire protocol (RESP2 and RESP3). Its keyspace is
indexed by the [`expanse-trie`](https://github.com/orieg/expanse) radix trie,
which gives it ordered prefix and range scans from the same structure that
serves point lookups.

The product requirements, the phase plan and every gate are in
[`docs/PRD.md`](docs/PRD.md). **No server exists yet**: every performance and
memory number in this repository is `(target)` until a committed measurement
replaces it.

### Workspace structure

| Path | Package | Role |
| :--- | :--- | :--- |
| `crates/expanse-kv-core` | `expanse-kv-core` | `no_std` + `alloc`, no dependencies, no `unsafe`: RESP parser and encoder, composite key codecs, score codec |
| `crates/expanse-kv-engine` | `expanse-kv-engine` | In-memory engine on `expanse-trie`: typed operations, expiry index, eviction, memory accounting |
| `crates/expanse-kv-persist` | `expanse-kv-persist` | No `unsafe`: append-only log, replay, rewrite, checksummed snapshots |
| `crates/expanse-kv-server` | **`expanse-kv`** | The server binary: listener, TLS, command router, metrics, configuration |
| `crates/expanse-kv-bench` | `expanse-kv-bench` | Workload drivers, comparator arms, differential oracle, provenance capture |
| `tests/compat_tcl` | — | Adapter that runs an upstream compatibility suite against the server (§3) |
| `tests/fuzz` | — | `cargo-fuzz` targets |

This table and `docs/PRD.md` §5 are one inventory. A change that adds, removes
or renames a crate updates both in the same pull request.

### Where documentation lives

Update the canonical document. Do not create a new `.md` for a session's
findings, a status note or a dated snapshot.

| Content | Home |
| :--- | :--- |
| Requirements, scope, phases, gates | `docs/PRD.md` |
| CI jobs, the rollup gate, the discipline gates | `docs/CI.md` |
| Status and quick start | `README.md` |
| Vulnerability reporting and threat model | `SECURITY.md` |
| A phase's pre-registration and results | `results/<suite>/` beside the artifacts they describe |

A document with no home in this table is created only when it owns a scope no
existing document covers, and it is added to this table and linked from
`README.md` in the same pull request.

---

## 2. Architectural Invariants (non-negotiable)

1. **§2.1 Consume `expanse-trie`; never fork it.** The trie is a published
   dependency. It is not vendored, patched, path-linked or reimplemented here.
   A missing primitive is filed as an issue on `orieg/expanse` that stands on
   its own merit there, and this project waits for the release or works above
   the public API.
2. **§2.2 No trie per logical collection.** Hashes, sets and sorted sets never
   get a map each. Every logical structure lives in a bounded set of shared
   tries under composite keys (`docs/PRD.md` §3).
3. **§2.3 Ordered semantics come from the ordered map.** Anything that scans in
   key order (`SCAN`, `HSCAN`, `ZRANGE`, the expiry index) is built on a map
   whose iteration order is documented upstream as byte-lexicographic. Never
   infer order from a map that does not promise it.
4. **§2.4 The concurrency protocol is optimistic and blocking.** Writers on one
   map serialize on that map's writer mutex. Readers validate versions without
   a lock on the uncontended path and fall back to the writer mutex after
   repeated retries. Never describe any path as lock-free, wait-free or
   obstruction-free.
5. **§2.5 No reclamation pin across I/O.** A cursor never holds an epoch pin
   across a network read or write, a disk operation or a client stall. Cursors
   are key bookmarks that pin for one bounded batch and release.
6. **§2.6 `expanse-kv-core` stays `no_std` + `alloc` with no dependencies.** CI
   builds it for a bare-metal target. Code that needs `std` sits behind the
   `std` feature or lives in another crate.
7. **§2.7 No `unsafe` in code that parses untrusted bytes.** `expanse-kv-core`
   and `expanse-kv-persist` carry `#![forbid(unsafe_code)]`; the attribute is
   never removed or narrowed. `unsafe` elsewhere needs a `// SAFETY:` comment
   that states the invariant relied on.
8. **§2.8 A key encoding is a format.** The type-prefix bytes and composite key
   layouts are defined once, in `expanse-kv-core`. A change to one carries
   property tests for ordering and round-trip, and once a persisted file can
   contain it, a format version change.
9. **§2.9 In-memory only.** The working dataset lives in RAM. Disk is used for
   the append-only log and snapshots. Device-backed or tiered value storage is
   out of scope.

---

## 3. Compatibility Is Established Black-Box

1. **Behaviour is settled by published protocol and command documentation and
   by differential testing** against a running Valkey server. Record each
   resolved ambiguity next to the test that pins it.
2. **Do not port source code from Redis, Valkey, Dragonfly or any other
   server.** Their licences differ from this project's and from each other.
3. **Upstream test suites are fetched at a pinned commit, never vendored.**
   `tests/compat_tcl/upstream/` is gitignored. The adapter and the list of
   in-scope tests are this repository's own.
4. **A deliberate difference from Redis behaviour is a documented decision**,
   stated in `docs/PRD.md`, with the test that demonstrates it.

---

## 4. Rust Standards & Local Gates

- **Edition 2024, MSRV 1.88**, enforced by the `msrv` CI job. An API a plan
  names must exist at the MSRV: check its stabilization version first.
- **Run `scripts/gate.sh` before every commit.** It mirrors CI: `cargo fmt
  --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`, the `no_std` build of `expanse-kv-core`,
  and `discipline diff` when `discipline` is installed.
- **Public items are documented.** `missing_docs` is on for the workspace.
- **Tests pin behaviour, not existence.** A test asserts a value that would
  differ if the code were wrong. A test that cannot fail is removed, not kept.
- **No placeholder bodies.** `todo!()`, `unimplemented!()` and functions that
  return a constant in place of an unbuilt result are not merged. Unbuilt
  scope is absent, not stubbed.
- **A gate's motivating defect is its first test.** Any check written to stop a
  class of error includes the original failure as a test that fails without the
  check and passes with it.

---

## 5. Autonomy

**Unattended** — implementing against an existing gate without changing its
value; tests; refactors that leave gates and public interfaces identical.

**Ratify first** (an owner decision on an issue the pull request closes) —
adding, removing or re-scoping a gate; changing any threshold in either
direction; changing a provenance tag; adding a dependency; changing a key
encoding or persisted format; editing a path listed under
`[gates.ratified-paths]` in `discipline.toml`, which includes this file, the
workflows and the agent hooks.

**Never autonomous** — `cargo publish`; creating a tag or a release; changing
repository settings, rulesets or secrets; retracting a published claim;
deleting evidence under `results/`; any push to `orieg/expanse`.

An agent that can edit its own guardrails has none.

---

## 6. Git & Pull Request Protocol

- **`main` is protected.** Every change is a pull request; the one required
  status check is **`CI Gate / All Checks Passed`**.
- **Every pull request references an issue** (`Closes #N`, `Refs #N`) or
  carries `no-issue: <reason>`. An issue records why the change exists.
- **Branch names:** `feat/<scope>`, `fix/<scope>`, `perf/<scope>`,
  `docs/<scope>`, `ci/<scope>`, `refactor/<scope>`.
- **Commits:** Conventional Commits, `type(scope): description`, atomic and
  signed. The subject is concise and factual; the body states what and why.
  No agent or session trailers.
- **Commit and push only when the maintainer asks.**
- **Adding a CI job means adding it to `ci-gate.needs`**, and every `uses:` is
  pinned to a full commit SHA. The `ci-integrity` gate fails otherwise.
- **Prose is impersonal and factual** in commits, pull requests and issues:
  what changed, where, and its status. No time estimates, durations or
  calendar projections anywhere; use ordering, dependencies and gate criteria.
- **Fixes get the same scrutiny as features.** A remediation can introduce the
  regression it was meant to remove.

---

## 7. Privacy & Local Infrastructure

This repository is public. Never put private or personal identifiers into
code, comments, commit messages, pull requests, issues, benchmark labels or
result files: hostnames, LAN addresses, internal domains, home-directory
paths, usernames, or the names of unpublished projects. Benchmark results name
the host by hardware description (CPU model, core and thread count, memory,
kernel), never by hostname. Commands in documentation use environment
placeholders (`$BENCH_HOST`), not concrete hosts or paths.

---

## 8. Benchmark & Research Integrity

expanse-kv makes comparative performance and memory claims. These rules exist
so that each claim can be re-run and can fail.

- **§8.1 Fail loud.** A missing dependency, an unstarted server or an unparsed
  result exits non-zero. Never substitute an estimate, a mocked loop or a
  constant for an unrun measurement. An intentionally non-fatal degradation is
  printed prominently and never renders as success.
- **§8.2 No stamped constants.** Every table cell, ratio and finding in a
  generated report is computed from that run's artifacts.
- **§8.3 Fair twins.** A comparator runs in a realistic configuration at equal
  cores, with identical key and value distributions, seeds, hit rates, client
  driver and pipeline depth. Each comparator has a stated regime where it is
  expected to win, and that regime is measured and published.
- **§8.4 Statistical gating.** A continuous metric passes iff the BCa 95%
  bootstrap confidence bound (at least 1,000 resamples) clears the floor, not
  the point estimate. Binomial gates use a Wilson interval with a stated `n`.
  An interval that straddles the floor is reported as `INTERMEDIATE`. Never
  `assert!` on a wall-clock point estimate; hard assertions belong on
  deterministic invariants.
- **§8.5 Scratch isolation.** Quick and smoke runs write to `results/quick/`,
  which is gitignored. They never overwrite a committed result.
- **§8.6 Workload realism.** Every timed loop's output is consumed. Setup and
  teardown stay outside the timed window. A miss probe is drawn from the same
  generator as the population and rejected on membership. A workload that
  mutates the structure restores it before any check that assumes it unchanged.
- **§8.7 Provenance.** Every published number is tagged `(measured: <hardware>,
  <commit>)` resolving to a committed artifact, `(target)`, or `(projected)`.
  A pre-registered threshold is never edited to match an outcome: the
  refutation is reported with its verdict.
- **§8.8 Three commits per measured gate, in order:** the bound or model with
  reference-pinned tests; the locked pre-registration (threshold, method,
  sample size, falsifier); the data with intervals and a verdict.
- **§8.9 A comparator that did not run has no number.** Every figure for a
  comparator comes from its own process in the same campaign, with its binary
  and version recorded. An arm that did not start or did not parse is `UNRUN`,
  and every gate that reads it is `UNRUN`, never `PASS`.
- **§8.10 Memory is judged across a sweep.** Radix-trie bytes per key is not
  monotonic in the key count. A memory claim reports the envelope over a dense
  sweep of `N` and of key shapes; a single-`N` figure is descriptive, never a
  gate.
- **§8.11 Host load is part of the measurement.** Record load before, between
  and after comparison runs. A result without its load record is inadmissible.
- **§8.12 Retract, do not re-estimate.** When a harness is fixed, every figure
  it produced is re-measured or removed. Never backfill from another run.
- **§8.13 State causes only when measured.** Do not attribute a result to a
  microarchitectural cause without the counters that show it.

---

## 9. Where a Fact Lives

| Content | Home |
| :--- | :--- |
| What is true now | the code, the canonical document |
| Why it changed | the commit message |
| What was tried and rejected | the issue |
| A defect that must not recur | a fail-then-pass test |

State the invariant, not the incident. A comment explains a constraint a
reader must respect today; the history of how it was found belongs in the test
that pins it and the commit that fixed it.

---

## 10. Work Unit

The unit of work is **one gate moving from unmet to met**, never "a phase".
Before starting, state the gate verbatim from `docs/PRD.md`, its falsifier,
what else the change can break, and its autonomy tier (§5). Threshold, method
and sample size are fixed before execution; changing one after seeing results
relabels the result `INTERMEDIATE` and requires fresh seeds.
