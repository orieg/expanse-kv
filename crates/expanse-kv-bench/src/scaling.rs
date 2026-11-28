//! Phase 0 thread-count scaling curve and cross-shard `SCAN` harness
//! (`docs/PRD.md` §7 Phase 0 items 3–4, `results/phase0/scaling_curve_prereg.md`).

use crate::stats::{BcaInterval, SplitMix64, bca_ci_95, percentile_sorted};
use core::cmp::Reverse;
use core::hint::black_box;
use expanse_trie::domain::escape_decode;
use expanse_trie::occ_stats::{self, Stat};
use expanse_trie::sync::SyncExpanseOrderedBytesMap;
use std::collections::BinaryHeap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

/// Partitioning policy across `P` shards (`docs/PRD.md` §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionPolicy {
    /// Keys routed by FNV-1a 64-bit hash modulo `P`.
    Hash,
    /// Keys partitioned into `P` contiguous lexicographical ranges.
    Range,
}

/// Workload mix for the scaling matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadMix {
    /// 100% optimistic `OrderedBytesReader::get` lookups.
    ReadOnly,
    /// 95% optimistic `get`, 5% `insert` overwriting existing keys.
    ReadWrite95_5,
}

/// Configuration for one scaling-curve cell.
#[derive(Debug, Clone, Copy)]
pub struct CellConfig {
    /// Number of worker threads (`T`).
    pub threads: usize,
    /// Number of shards (`P`).
    pub shards: usize,
    /// Workload mix.
    pub mix: WorkloadMix,
    /// Partitioning policy.
    pub policy: PartitionPolicy,
    /// Unmeasured warmup rounds.
    pub warmup_rounds: usize,
    /// Measured rounds (`R`).
    pub rounds: usize,
    /// Operations executed per thread per round.
    pub ops_per_thread: usize,
    /// Sample latency every `latency_stride` operations (e.g., 64).
    pub latency_stride: usize,
    /// Restrict writes to the first `hot_prefix_keys` keys (0 = uniform over all keys).
    pub hot_prefix_keys: usize,
}

/// OCC instrumentation delta for one round (present only when compiled with `--features occ-stats`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OccStatsDelta {
    /// `Stat::ReadOps`
    pub read_ops: u64,
    /// `Stat::ReadAttempts`
    pub read_attempts: u64,
    /// `Stat::ReadFallbacks`
    pub read_fallbacks: u64,
    /// `Stat::LockedReads`
    pub locked_reads: u64,
    /// `Stat::WriteOps`
    pub write_ops: u64,
    /// `Stat::LockFallbacks`
    pub lock_fallbacks: u64,
}

/// Single-round measurements for one `(T, P, mix)` cell.
#[derive(Debug, Clone, Copy)]
pub struct RoundSample {
    /// Aggregate throughput across all `T` threads (ops/sec).
    pub ops_per_sec: f64,
    /// Median sampled operation latency (ns).
    pub p50_ns: u64,
    /// 99th-percentile sampled operation latency (ns).
    pub p99_ns: u64,
    /// Elapsed wall-clock seconds for this round.
    pub elapsed_secs: f64,
    /// OCC event counters for this round (`None` when `occ-stats` is not enabled).
    pub occ: Option<OccStatsDelta>,
}

/// Aggregated result for one `(T, P, mix)` cell across `R` rounds.
#[derive(Debug, Clone)]
pub struct CellResult {
    /// Cell configuration.
    pub config: CellConfig,
    /// Per-round measurements.
    pub rounds: Vec<RoundSample>,
    /// BCa 95% CI on aggregate throughput (ops/sec).
    pub throughput_ci: BcaInterval,
    /// BCa 95% CI on p99 latency (ns).
    pub p99_ns_ci: BcaInterval,
    /// Mean OCC reader fallback rate (`read_fallbacks / read_ops`), or `None` if `UNRUN`.
    pub read_fallback_rate: Option<f64>,
    /// Mean OCC writer lock fallback rate (`lock_fallbacks / write_ops`), or `None` if `UNRUN`.
    pub write_fallback_rate: Option<f64>,
}

/// FNV-1a 64-bit hash for deterministic hash-sharding across `P` shards.
#[inline]
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Generates `n` sorted, prefix-structured 33-byte keys:
/// `org:user:session:<16-hex-id>`.
#[must_use]
pub fn generate_prefix_keys(n: usize) -> Vec<Vec<u8>> {
    let mut keys = Vec::with_capacity(n);
    for i in 0..n {
        keys.push(format!("org:user:session:{i:016x}").into_bytes());
    }
    keys
}

/// Generates `n` deterministic uniform random 16-byte binary keys (sorted for range partitioning).
#[must_use]
pub fn generate_uniform_keys(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = SplitMix64::new(seed);
    let mut keys = Vec::with_capacity(n);
    for _ in 0..n {
        let mut k = Vec::with_capacity(16);
        k.extend_from_slice(&rng.next_u64().to_be_bytes());
        k.extend_from_slice(&rng.next_u64().to_be_bytes());
        keys.push(k);
    }
    keys.sort();
    keys.dedup();
    keys
}

/// A `P`-shard collection of `SyncExpanseOrderedBytesMap` instances.
pub struct ShardedMap {
    shards: Vec<SyncExpanseOrderedBytesMap>,
    policy: PartitionPolicy,
    /// Sorted split keys for `PartitionPolicy::Range` (`len == shards.len() - 1`).
    range_splits: Vec<Vec<u8>>,
}

impl ShardedMap {
    /// Builds a `P`-shard map populated with `sorted_keys` (values `1..=len`).
    ///
    /// `sorted_keys` must be sorted in ascending byte-lexicographical order for
    /// `PartitionPolicy::Range` to split evenly.
    #[must_use]
    pub fn from_sorted_keys(
        sorted_keys: &[Vec<u8>],
        num_shards: usize,
        policy: PartitionPolicy,
    ) -> Self {
        assert!(num_shards >= 1, "num_shards must be >= 1");
        let shards: Vec<SyncExpanseOrderedBytesMap> = (0..num_shards)
            .map(|_| SyncExpanseOrderedBytesMap::new())
            .collect();

        let mut range_splits = Vec::new();
        if policy == PartitionPolicy::Range && num_shards > 1 && !sorted_keys.is_empty() {
            for s in 1..num_shards {
                let idx = (s * sorted_keys.len()) / num_shards;
                range_splits.push(sorted_keys[idx].clone());
            }
        }

        let map = Self {
            shards,
            policy,
            range_splits,
        };
        for (i, k) in sorted_keys.iter().enumerate() {
            let sid = map.shard_index(k);
            map.shards[sid].insert(k, (i as u64) + 1);
        }
        map
    }

    /// Returns the shard index `0..P` responsible for `key`.
    #[inline]
    #[must_use]
    pub fn shard_index(&self, key: &[u8]) -> usize {
        if self.shards.len() == 1 {
            return 0;
        }
        match self.policy {
            PartitionPolicy::Hash => (fnv1a_64(key) as usize) % self.shards.len(),
            PartitionPolicy::Range => self
                .range_splits
                .partition_point(|split| split.as_slice() <= key),
        }
    }

    /// Returns the number of shards `P`.
    #[must_use]
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Total `mem_used()` summed across all shards.
    #[must_use]
    pub fn total_mem_used(&self) -> usize {
        self.shards
            .iter()
            .map(SyncExpanseOrderedBytesMap::mem_used)
            .sum()
    }

    /// Runs an ordered `SCAN` returning up to `count` keys strictly greater
    /// than `after` (or from the start if `after` is `None`), using:
    /// - a $P$-way binary min-heap merge over `SyncStrMapCursor`s for `PartitionPolicy::Hash`, or
    /// - sequential single-shard cursor traversal for `PartitionPolicy::Range`.
    ///
    /// No reclamation pin is held across calls (`AGENTS.md` §2.5).
    #[must_use]
    pub fn scan_batch(&self, after: Option<&[u8]>, count: usize) -> Vec<(Vec<u8>, u64)> {
        if count == 0 {
            return Vec::new();
        }
        match self.policy {
            PartitionPolicy::Hash if self.shards.len() > 1 => self.scan_hash_merge(after, count),
            _ => self.scan_range(after, count),
        }
    }

    fn scan_range(&self, after: Option<&[u8]>, count: usize) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::with_capacity(count);
        let start_shard = match after {
            Some(k) => self.shard_index(k),
            None => 0,
        };
        let esc_start = after.map(encode_strict_successor);

        for sid in start_shard..self.shards.len() {
            let reader = self.shards[sid].inner().reader();
            let mut cursor = reader.cursor();
            cursor.set_batch_cap(count.saturating_sub(out.len()).max(1));
            if sid == start_shard
                && let Some(ref succ) = esc_start
            {
                let nfs = expanse_trie::strmap::NulFreeStr::new(succ)
                    .expect("escaped successor is NUL-free");
                if cursor.advance_to(nfs).is_none() {
                    continue;
                }
            }
            while out.len() < count {
                let Some((esc_k, v)) = cursor.next() else {
                    break;
                };
                let dec = escape_decode(esc_k).expect("stored key is valid escaped bytes");
                out.push((dec, v));
            }
            if out.len() >= count {
                break;
            }
        }
        out
    }

    fn scan_hash_merge(&self, after: Option<&[u8]>, count: usize) -> Vec<(Vec<u8>, u64)> {
        let p = self.shards.len();
        let esc_start = after.map(encode_strict_successor);

        // One pre-registered StrReader per shard, then one SyncStrMapCursor per shard.
        let readers: Vec<_> = self.shards.iter().map(|s| s.inner().reader()).collect();
        let mut cursors: Vec<_> = readers
            .iter()
            .map(|r| {
                let mut c = r.cursor();
                c.set_batch_cap(count.clamp(1, 64));
                if let Some(ref succ) = esc_start {
                    let nfs = expanse_trie::strmap::NulFreeStr::new(succ)
                        .expect("escaped successor is NUL-free");
                    if c.advance_to(nfs).is_none() {
                        // Cursor is exhausted past this shard's keys; `c.next()` will return None.
                    }
                }
                c
            })
            .collect();

        // Min-heap of Reverse<(decoded_key, shard_id, value)>.
        let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize, u64)>> = BinaryHeap::with_capacity(p);
        for (sid, cur) in cursors.iter_mut().enumerate() {
            if let Some((esc_k, v)) = cur.next() {
                let dec = escape_decode(esc_k).expect("stored key is valid escaped bytes");
                heap.push(Reverse((dec, sid, v)));
            }
        }

        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let Some(Reverse((k, sid, v))) = heap.pop() else {
                break;
            };
            out.push((k, v));
            if let Some((esc_k, nv)) = cursors[sid].next() {
                let dec = escape_decode(esc_k).expect("stored key is valid escaped bytes");
                heap.push(Reverse((dec, sid, nv)));
            }
        }
        out
    }
}

/// Encodes the immediate NUL-free escaped successor of `key`: `escape_encode(key) || [0x01]`.
fn encode_strict_successor(key: &[u8]) -> Vec<u8> {
    let mut esc = expanse_trie::domain::escape_encode(key);
    esc.push(0x01);
    esc
}

/// Runs one `(T, P, mix)` cell on a pre-populated `ShardedMap`.
#[must_use]
pub fn run_cell(map: &Arc<ShardedMap>, keys: &Arc<Vec<Vec<u8>>>, config: CellConfig) -> CellResult {
    let total_rounds = config.warmup_rounds + config.rounds;
    let mut measured = Vec::with_capacity(config.rounds);
    let n_keys = keys.len();
    let write_bound = if config.hot_prefix_keys > 0 && config.hot_prefix_keys < n_keys {
        config.hot_prefix_keys
    } else {
        n_keys
    };

    for round_idx in 0..total_rounds {
        let is_measured = round_idx >= config.warmup_rounds;
        let round_seed_base = 0x9E37_79B9_7F4A_7C15u64 ^ (round_idx as u64);

        if is_measured && occ_stats::enabled() {
            occ_stats::reset();
        }

        let start_barrier = Arc::new(Barrier::new(config.threads + 1));
        let end_barrier = Arc::new(Barrier::new(config.threads + 1));
        let mut handles = Vec::with_capacity(config.threads);

        for tid in 0..config.threads {
            let m = Arc::clone(map);
            let ks = Arc::clone(keys);
            let sb = Arc::clone(&start_barrier);
            let eb = Arc::clone(&end_barrier);
            let handle = thread::spawn(move || {
                // Pre-register one OrderedBytesReader per shard on this thread.
                let readers: Vec<_> = m
                    .shards
                    .iter()
                    .map(SyncExpanseOrderedBytesMap::reader)
                    .collect();
                let mut rng = SplitMix64::new(
                    round_seed_base ^ ((tid as u64).wrapping_mul(0x517C_C1B7_2722_0A95)),
                );
                let mut lats =
                    Vec::with_capacity(config.ops_per_thread / config.latency_stride + 1);
                let mut acc: u64 = 0;

                sb.wait();
                for op in 0..config.ops_per_thread {
                    let do_write = match config.mix {
                        WorkloadMix::ReadOnly => false,
                        WorkloadMix::ReadWrite95_5 => (rng.next_u64() % 20) < 1,
                    };
                    let idx = if do_write {
                        rng.next_usize(write_bound)
                    } else {
                        rng.next_usize(n_keys)
                    };
                    let key = &ks[idx];
                    let sid = m.shard_index(key);
                    let sample_lat = (op % config.latency_stride) < 1;
                    let t0 = if sample_lat {
                        Some(Instant::now())
                    } else {
                        None
                    };

                    if do_write {
                        let prev = m.shards[sid].insert(key, (op as u64) + 1);
                        acc = acc.wrapping_add(prev.unwrap_or(0));
                    } else {
                        let val = readers[sid].get(key);
                        acc = acc.wrapping_add(val.unwrap_or(0));
                    }

                    if let Some(start) = t0 {
                        lats.push(start.elapsed().as_nanos() as u64);
                    }
                }
                black_box(acc);
                eb.wait();
                lats
            });
            handles.push(handle);
        }

        start_barrier.wait();
        let t_start = Instant::now();
        end_barrier.wait();
        let elapsed = t_start.elapsed().as_secs_f64();

        let occ_snap = if is_measured && occ_stats::enabled() {
            let s = occ_stats::snapshot();
            Some(OccStatsDelta {
                read_ops: s[Stat::ReadOps as usize],
                read_attempts: s[Stat::ReadAttempts as usize],
                read_fallbacks: s[Stat::ReadFallbacks as usize],
                locked_reads: s[Stat::LockedReads as usize],
                write_ops: s[Stat::WriteOps as usize],
                lock_fallbacks: s[Stat::LockFallbacks as usize],
            })
        } else {
            None
        };

        let mut all_lats = Vec::new();
        for h in handles {
            let mut l = h.join().expect("worker thread joined");
            if is_measured {
                all_lats.append(&mut l);
            }
        }

        if is_measured {
            all_lats.sort_unstable();
            let total_ops = (config.threads * config.ops_per_thread) as f64;
            measured.push(RoundSample {
                ops_per_sec: total_ops / elapsed,
                p50_ns: percentile_sorted(&all_lats, 50.0),
                p99_ns: percentile_sorted(&all_lats, 99.0),
                elapsed_secs: elapsed,
                occ: occ_snap,
            });
        }
    }

    let qps_samples: Vec<f64> = measured.iter().map(|r| r.ops_per_sec).collect();
    let p99_samples: Vec<f64> = measured.iter().map(|r| r.p99_ns as f64).collect();
    let throughput_ci = bca_ci_95(&qps_samples, 2_000, 0xA11C_E001);
    let p99_ns_ci = bca_ci_95(&p99_samples, 2_000, 0xA11C_E002);

    let (read_fallback_rate, write_fallback_rate) = if occ_stats::enabled() {
        let total_ro: u64 = measured
            .iter()
            .filter_map(|r| r.occ.map(|o| o.read_ops))
            .sum();
        let total_rf: u64 = measured
            .iter()
            .filter_map(|r| r.occ.map(|o| o.read_fallbacks))
            .sum();
        let total_wo: u64 = measured
            .iter()
            .filter_map(|r| r.occ.map(|o| o.write_ops))
            .sum();
        let total_wf: u64 = measured
            .iter()
            .filter_map(|r| r.occ.map(|o| o.lock_fallbacks))
            .sum();
        let rfr = if total_ro > 0 {
            Some(total_rf as f64 / total_ro as f64)
        } else {
            Some(0.0)
        };
        let wfr = if total_wo > 0 {
            Some(total_wf as f64 / total_wo as f64)
        } else {
            Some(0.0)
        };
        (rfr, wfr)
    } else {
        (None, None)
    };

    CellResult {
        config,
        rounds: measured,
        throughput_ci,
        p99_ns_ci,
        read_fallback_rate,
        write_fallback_rate,
    }
}

/// Benchmark result for cross-shard `SCAN` (`COUNT` keys per batch).
#[derive(Debug, Clone)]
pub struct ScanBenchResult {
    /// Shard count `P`.
    pub shards: usize,
    /// Partitioning policy.
    pub policy: PartitionPolicy,
    /// Keys requested per `SCAN` batch.
    pub count: usize,
    /// Per-round average nanoseconds per `SCAN` batch.
    pub round_ns_per_batch: Vec<f64>,
    /// BCa 95% CI on nanoseconds per `SCAN` batch.
    pub ns_per_batch_ci: BcaInterval,
}

/// Benchmarks `batches_per_round` consecutive `SCAN` batches of size `count` across `rounds`.
#[must_use]
pub fn run_scan_bench(
    map: &ShardedMap,
    count: usize,
    batches_per_round: usize,
    warmup_rounds: usize,
    rounds: usize,
) -> ScanBenchResult {
    let mut measured = Vec::with_capacity(rounds);
    for r in 0..(warmup_rounds + rounds) {
        let mut cursor_key: Option<Vec<u8>> = None;
        let t0 = Instant::now();
        let mut total_yielded = 0usize;
        for _ in 0..batches_per_round {
            let batch = map.scan_batch(cursor_key.as_deref(), count);
            total_yielded += batch.len();
            cursor_key = batch.last().map(|(k, _)| k.clone());
            black_box(batch.len());
            if cursor_key.is_none() {
                // Wrap around cleanly if we hit the end of the keyspace.
                cursor_key = None;
            }
        }
        black_box(total_yielded);
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        if r >= warmup_rounds {
            measured.push(elapsed_ns / (batches_per_round as f64));
        }
    }
    let ns_per_batch_ci = bca_ci_95(&measured, 2_000, 0x5CA1_0001);
    ScanBenchResult {
        shards: map.num_shards(),
        policy: map.policy,
        count,
        round_ns_per_batch: measured,
        ns_per_batch_ci,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_range_scan_match_sorted_keyset_across_shard_counts() {
        let keys = generate_prefix_keys(500);
        let expected: Vec<(Vec<u8>, u64)> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), (i as u64) + 1))
            .collect();

        for &p in &[1usize, 4, 8] {
            for policy in [PartitionPolicy::Hash, PartitionPolicy::Range] {
                let map = ShardedMap::from_sorted_keys(&keys, p, policy);
                let mut collected = Vec::new();
                let mut after: Option<Vec<u8>> = None;
                loop {
                    let batch = map.scan_batch(after.as_deref(), 37);
                    if batch.is_empty() {
                        break;
                    }
                    after = batch.last().map(|(k, _)| k.clone());
                    collected.extend(batch);
                }
                assert_eq!(collected, expected, "mismatch for P={p}, policy={policy:?}");
            }
        }
    }
}
