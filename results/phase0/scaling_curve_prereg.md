# Phase 0 Pre-Registration: `SyncExpanseOrderedBytesMap` Thread-Count Scaling Curve

- **PRD gate:** `docs/PRD.md` §7, Phase 0 items 3–4 (`AGENTS.md` §8.8)
- **Tracking issue:** `orieg/expanse-kv#7`
- **Analytical bounds prerequisite:** `crates/expanse-kv-bench/src/bounds.rs` (`orieg/expanse-kv#5`)
- **Status:** Locked prior to any empirical execution or data commit

---

## 1. Objective & Novelty Tier

- **Objective:** Empirically characterize the multi-thread scaling, tail latency, OCC reader-fallback behavior (`occ-stats`), and cross-shard `SCAN` merge cost of `SyncExpanseOrderedBytesMap` (`expanse-trie 0.19.0`) across shard counts $P$ and thread counts $T$, in order to select shard count $P$ and partitioning policy (hash-sharded with $P$-way merge `SCAN` vs range-sharded) as an owner decision before locking Phase 3 throughput and latency floors.
- **Novelty tier claimed:** `Characterisation`
- **Tier justification:** Measures the empirical scaling envelope and OCC fallback boundary of `SyncExpanseOrderedBytesMap` across $(T, P)$ operating points to determine the shard count $P$ and partitioning policy; no new algorithm or closed-form theory is claimed.

---

## 2. Experimental Matrix & Method

### 2.1 Axes Swept

- **Thread counts ($T$):** $\{1, 2, 4, 8, 16\}$ (capped at host logical cores if host has fewer than 16 logical cores; any omitted $T$ is recorded as `UNRUN` in the provenance artifact, never interpolated, per `AGENTS.md` §8.9).
- **Shard counts ($P$):** $\{1, 4, 8, 16, 32\}$.
- **Workload mixes:**
  1. **100% Read** (`OrderedBytesReader::get`, pre-registered per-thread reader handle, zero per-call registry lock).
  2. **95/5 Read/Write** (95% `OrderedBytesReader::get`, 5% `SyncExpanseOrderedBytesMap::insert` overwriting existing keys to keep population $N$ fixed during the timed window).
- **Partitioning policies compared for `SCAN` (`docs/PRD.md` §3.4):**
  1. **Hash-sharded ($P$-way merge `SCAN`):** Keys routed by `hash(key) % P`; ordered `SCAN` merges $P$ shard cursors via a binary min-heap (`crates/expanse-kv-bench/src/bounds.rs::scan_merge_cost`).
  2. **Range-sharded (contiguous prefix/key ranges):** Keys partitioned into $P$ contiguous lexicographical ranges; ordered `SCAN` reads 1 shard per batch (at most 1 boundary crossing), with explicit measurement of hot-prefix write skew when all writes target a single prefix range.

### 2.2 Dataset, Key Distribution & Workload Realism (`AGENTS.md` §8.6)

- **Population size ($N$):** `100_000` pre-populated prefix-structured keys (`org:user:session:<16-hex-id>`, 33 bytes each, clean ASCII fast path) plus a secondary check on `100_000` uniform random 16-byte binary keys.
- **Setup/teardown isolation:** All population inserts complete before the timed measurement window starts; map drop occurs outside the timed window.
- **Output consumption:** Every lookup return value is folded into a per-thread accumulator passed through `core::hint::black_box` so no read is elided.
- **PRNG & seeds:** Deterministic `SplitMix64` PRNG with fixed per-round seeds (`seed = 0x9E37_79B9_7F4A_7C15 ^ round_id`), identical key access sequences across all $P$ arms within each round so paired per-round ratios are exact.
- **Access distribution:** Uniform random selection over the live keyset for the primary scaling matrix, plus skewed hot-prefix writes (`1` active prefix) for the hash-vs-range partitioning comparison.

### 2.3 Sample Size & Statistical Method (`AGENTS.md` §8.4)

- **Rounds per cell:** `R = 10` independent timed rounds per $(T, P, \text{mix})$ cell (after `2` unmeasured warmup rounds per cell).
- **Operations per thread per round:** `200_000` operations per thread (total `200_000 * T` operations per round), fixed by operation count rather than wall-clock duration.
- **Latency capture:** Sampled per-operation latency recorded per thread (1-in-64 stride to avoid timing overhead dominating fast lookups) to compute p50 and p99 latency per round.
- **Confidence intervals:** BCa 95% bootstrap confidence intervals with `2_000` resamples computed across the `R = 10` per-round throughput (ops/s), p99 latency (ns), and paired speedup ratios (`T`-thread / `1`-thread and `P`-shard / `1`-shard).
- **OCC instrumentation:** When `occ-stats` is enabled on `expanse-trie`, record `read_ops`, `read_attempts`, `read_fallbacks`, `locked_reads`, `write_ops`, and `lock_fallbacks` per cell via `expanse_trie::occ_stats::snapshot()`, resetting counters between rounds. If `occ-stats` is not enabled in the build, record those fields as `UNRUN` rather than `0` (`AGENTS.md` §8.1, §8.9).

### 2.4 Host Load & Provenance Admissibility (`AGENTS.md` §8.11)

- **Hardware provenance:** Dynamically capture CPU model, physical/logical core count, memory size, OS/kernel version, `expanse-trie` crate version, and git commit SHA into `results/phase0/scaling_curve.json`. Never record hostnames, LAN IPs, usernames, or home paths (`AGENTS.md` §7).
- **Host load record:** Capture 1-minute load average and top CPU consumers before the first cell, between comparison blocks, and after the final cell.
- **Admissibility rule:** Any run where baseline or mid-run load average exceeds `logical_cores / 2`, or where a non-target process exceeds 50% CPU, or where load shifts by more than `2.0` between compared cells is marked **inadmissible** and discarded (never published as a valid scaling curve).

---

## 3. Pre-Registered Thresholds, Selection Criteria & Falsifiers

No threshold below may be edited after observing measurement data (`AGENTS.md` §8.7). Any post-hoc adjustment relabels the outcome `INTERMEDIATE` and requires fresh seeds.

### 3.1 Scaling & Contention Gates

1. **Read-only scaling (100% read, $P = 1$):**
   - At $T = \min(8, \text{physical\_cores})$, throughput speedup over $T = 1$ clears `>= 4.0x` `(target, BCa 95% CI lower bound)` with OCC reader fallback rate `<= 0.1%` `(target)`.
   - **Falsifier:** BCa 95% CI lower bound `< 4.0x` on a verified idle host, or any non-zero reader fallback under 100% read (which would indicate unexpected writer-lock acquisition on the read path).
2. **Mixed 95/5 read/write sharding benefit:**
   - At $T = \min(8, \text{physical\_cores})$, the selected shard count $P^*$ improves 95/5 throughput over $P = 1$ by `>= 2.0x` `(target, BCa 95% CI lower bound)` and reduces OCC reader fallback rate relative to $P = 1$.
   - **Falsifier:** BCa 95% CI lower bound of `QPS(P*) / QPS(1)` `< 2.0x` across all tested $P \in \{4, 8, 16, 32\}$ on a verified idle host.
3. **Cross-shard `SCAN` merge overhead:**
   - Measured hash-sharded $P$-way merge `SCAN` work ratio vs single-shard `SCAN` for `COUNT = 100` is checked against the analytical prediction from `crates/expanse-kv-bench/src/bounds.rs::scan_merge_cost`.
   - **Falsifier:** Measured `SCAN` slowdown at $P^*$ exceeds `2.0x` `(target)` the analytical upper-bound work ratio from `scan_merge_cost`, triggering a root-cause post-mortem (`AGENTS.md` §8.13).

### 3.2 Fair-Twin / Incumbent Winning Regimes (`AGENTS.md` §8.3)

- **Where $P = 1$ (unsharded) wins:** Zero cross-shard `SCAN` merge overhead (`work_ratio = 1.0`), minimal cursor bookmark memory (`1` shard bookmark), and single-shard atomic transactions without cross-shard coordination.
- **Where range-sharding wins over hash-sharding:** Ordered `SCAN` / prefix scans touch `1` shard (no $P$-way heap merge).
- **Where hash-sharding wins over range-sharding:** Hot-prefix 95/5 workloads (`org:user:session:*`) distribute writes across all $P$ shards instead of serializing on a single range shard's writer mutex.

### 3.3 Shard Count $P^*$ and Partitioning Policy Selection Rule

Select the smallest $P^* \in \{4, 8, 16, 32\}$ such that:
1. $P^*$ achieves at least `90%` `(target)` of the maximum observed 95/5 throughput across $P \in \{1, 4, 8, 16, 32\}$ at $T = \min(8, \text{physical\_cores})$, AND
2. Increasing from $P^*$ to $2 P^*$ adds less than `10%` `(target)` 95/5 throughput while doubling `SCAN` cursor state bytes (`scan_merge_cost`).

The final choice of $P^*$ and partitioning policy is proposed as an **owner decision on a GitHub issue** (`AGENTS.md` §5, `docs/PRD.md` §7 Phase 0 item 4) before locking Phase 3 QPS/latency floors.

---

## 4. Operating-Point Constants Inventory

| Parameter | Value | Source | Sensitivity Claim |
| :--- | :--- | :--- | :--- |
| `N` (live keys) | `100_000` | `infrastructure_default` | Large enough to exceed L1/L2 cache and exercise multi-level trie branches; full $N$-sweep from $10^3$ to $10^7$ is gated in Phase 2 (`docs/PRD.md` §6.3). |
| `R` (rounds/cell) | `10` | `derived_from_math_first` | Provides 10 independent paired samples for BCa 95% bootstrap CIs (`2_000` resamples); sufficient to detect a `>= 2.0x` sharding lift cleanly. |
| `ops_per_thread_per_round` | `200_000` | `infrastructure_default` | Amortizes thread-barrier synchronization while keeping total matrix execution bounded on a single host. |
| `latency_sample_stride` | `64` | `infrastructure_default` | Yields `3_125` latency samples per thread per round (`>= 3_125 * T` per round) while keeping `Instant::now()` overhead `< 2%` `(projected)` of fast lookups. |
| `scan_count` | `100` | `infrastructure_default` | Representative Redis `SCAN COUNT 100` batch size; matches `bounds::scan_merge_cost` reference pin. |
| `T` (threads) | `{1, 2, 4, 8, 16}` | `swept_axis_of_this_spike` | Direct PRD Phase 0 item 3 axis. |
| `P` (shards) | `{1, 4, 8, 16, 32}` | `swept_axis_of_this_spike` | Direct PRD Phase 0 item 3 axis. |
