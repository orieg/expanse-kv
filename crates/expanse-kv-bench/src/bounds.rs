//! Analytical bounds and cost models for `expanse-kv` (`AGENTS.md` §8.8).
//!
//! All functions in this module are pure, validate their inputs, and carry
//! reference-pinned unit tests. They are committed ahead of any empirical
//! Phase 0 / Phase 2 / Phase 3 measurement so that measured envelopes and
//! scaling curves are checked against pre-registered analytical predictions.
//!
//! # Models
//!
//! 1. **Expected bytes per key by key shape** ([`expected_bytes_per_key`]):
//!    models 256-ary digital trie (`expanse-trie`) memory density across the
//!    four key shapes in `docs/PRD.md` §6.3, including the non-monotonic
//!    node-promotion sawtooth envelope (`min_bytes_per_key`,
//!    `max_bytes_per_key`).
//! 2. **$P$-way merge cost for cross-shard `SCAN`** ([`scan_merge_cost`]):
//!    models hash-sharded $P$-way merge `SCAN` overhead (shard seeks, binary
//!    heap comparisons, cursor bookmark memory, and work ratio relative to
//!    single-shard / range-sharded `SCAN`, `docs/PRD.md` §3.4).

use core::fmt;

/// Size of a `u64` value slot stored in `SyncExpanseOrderedBytesMap` (bytes).
pub const VALUE_SLOT_BYTES: usize = 8;

/// Size of one `expanse-trie` 64-bit `Edge` descriptor (bytes, `expanse-trie` `src/node.rs`).
pub const EDGE_BYTES: usize = 16;

/// Size of an IEEE-754 order-preserving `f64` score prefix in a ZSet score key (`docs/PRD.md` §3.2).
pub const ZSET_SCORE_BYTES: usize = 8;

/// Per-shard cursor bookmark metadata overhead (length, capacity, pointer words) in bytes.
pub const CURSOR_BOOKMARK_OVERHEAD_BYTES: usize = 24;

/// Expected escape-encoding expansion factor on uniform random bytes:
/// `0x00` and `0x01` each expand from 1 byte to 2 bytes (`expanse-trie` `src/ordered_bytesmap.rs`),
/// giving `(254 * 1 + 2 * 2) / 256 = 258 / 256 = 1.0078125`.
pub const ESCAPE_EXPANSION_UNIFORM: f64 = 258.0 / 256.0;

/// Maximum leaf capacity-class headroom per entry in the sawtooth envelope (bytes).
const LEAF_CLASS_SLACK_MAX_BYTES: f64 = 8.0;

/// Natural logarithm of the trie radix `M = 256` (`ln(256)`).
///
/// For an $M$-ary digital trie storing $n$ uniform random keys, the asymptotic
/// expected number of internal nodes per key is $1 / \ln(M)$ (Knuth, *The Art of
/// Computer Programming*, Vol. 3, §6.3, Eq. (17); Flajolet & Sedgewick,
/// *Analytic Combinatorics*, 2009, §VII.6).
const LN_256: f64 = core::f64::consts::LN_2 * 8.0;

/// Validation errors returned by bound functions on invalid domain inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundError {
    /// A strictly positive count (`n`, `p`, `count`, `fields_per_key`, `members_per_key`) was zero.
    ZeroCount(&'static str),
    /// A key or field length parameter was zero where a positive length is required.
    ZeroLength(&'static str),
    /// A floating-point cost ratio was negative, NaN, or infinite.
    InvalidRatio(&'static str),
}

impl fmt::Display for BoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCount(name) => write!(f, "{name} must be >= 1"),
            Self::ZeroLength(name) => write!(f, "{name} must be >= 1"),
            Self::InvalidRatio(name) => write!(f, "{name} must be finite and >= 0.0"),
        }
    }
}

impl std::error::Error for BoundError {}

/// Key-shape specifications matching `docs/PRD.md` §6.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyShape {
    /// Prefix-structured string keys (e.g., `org:user:session:<id>`).
    PrefixStructured {
        /// Common prefix length shared across the population (bytes).
        shared_prefix_bytes: usize,
        /// Per-key unique suffix length (bytes).
        unique_suffix_bytes: usize,
        /// Count of bytes in the unique suffix that are `0x00` or `0x01`
        /// (each adds 1 byte under `expanse-trie`'s escape encoding).
        escape_bytes: usize,
    },
    /// Uniform random binary keys of fixed length.
    UniformRandom {
        /// Raw unescaped key length (bytes).
        key_bytes: usize,
    },
    /// Redis Hash fields stored under composite key `[0x02][len_varint(key)][key][field]`.
    HashFields {
        /// Logical Redis key length (bytes).
        key_bytes: usize,
        /// Number of fields stored under each logical Hash key.
        fields_per_key: usize,
        /// Average field name length (bytes).
        field_bytes: usize,
    },
    /// Redis Sorted Set members stored as two composite entries per member:
    /// score index `[0x03][len_varint(key)][key][score_be_u64][member]` and
    /// member index `[0x04][len_varint(key)][key][member]`.
    ZSetMembers {
        /// Logical Redis key length (bytes).
        key_bytes: usize,
        /// Number of members stored under each logical ZSet key.
        members_per_key: usize,
        /// Average member name length (bytes).
        member_bytes: usize,
    },
}

/// Analytical bytes-per-key envelope for a 256-ary digital trie (`expanse-trie`).
///
/// Because radix-trie bytes/key is non-monotonic in population $N$ (node
/// transitions across `BranchL3 -> BranchL7 -> BranchB -> BranchU` and leaf
/// capacity-class doublings create a sawtooth; `AGENTS.md` §8.10), the model
/// reports an envelope `(min_bytes_per_key, max_bytes_per_key)` rather than a
/// single point estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryEnvelope {
    /// Lower bound of expected trie bytes per logical entry (compact leaf/branch phase).
    pub min_bytes_per_key: f64,
    /// Upper bound of expected trie bytes per logical entry (post-promotion sawtooth peak).
    pub max_bytes_per_key: f64,
    /// Effective escaped key length stored per logical entry before prefix compression (bytes).
    pub escaped_key_bytes: f64,
    /// Number of trie entries per logical item (1 for strings/hashes, 2 for ZSet members).
    pub trie_entries_per_item: usize,
}

/// Returns the unsigned LEB128 varint length in bytes for `value`.
#[must_use]
pub const fn varint_len(mut value: usize) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

/// Returns `log_256(n)` for `n >= 1`.
///
/// Represents the expected branching depth in bytes of a 256-ary digital trie
/// holding `n` uniform random keys (Knuth, *TAOCP* Vol. 3, §6.3; Flajolet &
/// Sedgewick, *Analytic Combinatorics*, 2009, §VII.6).
///
/// # Errors
///
/// Returns [`BoundError::ZeroCount`] if `n == 0`.
pub fn log256(n: usize) -> Result<f64, BoundError> {
    if n == 0 {
        return Err(BoundError::ZeroCount("n"));
    }
    Ok((n as f64).log2() / 8.0)
}

/// Computes the analytical trie bytes-per-key envelope for `shape` at population `n`.
///
/// Does not include external value-slab payload bytes; it models the
/// `SyncExpanseOrderedBytesMap` index bytes per logical entry (key suffix +
/// 8-byte value handle + amortized prefix + internal trie node overhead).
///
/// # Primary Sources
///
/// - Knuth, *The Art of Computer Programming*, Vol. 3, §6.3, Eq. (17)
///   (asymptotic internal nodes per key $1 / \ln M$ for $M$-ary digital tries).
/// - Flajolet & Sedgewick, *Analytic Combinatorics*, 2009, §VII.6 (digital
///   trie path length $\log_M n$).
/// - `expanse-trie 0.19.0` `src/ordered_bytesmap.rs` (escape encoding
///   `0x00 -> [0x01, 0x01]`, `0x01 -> [0x01, 0x02]`) and `src/node.rs`
///   (16-byte `Edge`, 8-byte `u64` value slot).
///
/// # Errors
///
/// Returns [`BoundError::ZeroCount`] or [`BoundError::ZeroLength`] on zero
/// population or degenerate shape parameters.
pub fn expected_bytes_per_key(shape: KeyShape, n: usize) -> Result<MemoryEnvelope, BoundError> {
    if n == 0 {
        return Err(BoundError::ZeroCount("n"));
    }

    // Internal branch amortization per trie entry: 1 / ln(256) internal nodes
    // per key times 16-byte Edge minimum, up to 4x at node-promotion sawtooth peaks.
    let internal_min = (EDGE_BYTES as f64) / LN_256;
    let internal_max = (4.0 * EDGE_BYTES as f64) / LN_256;

    match shape {
        KeyShape::PrefixStructured {
            shared_prefix_bytes,
            unique_suffix_bytes,
            escape_bytes,
        } => {
            if unique_suffix_bytes == 0 {
                return Err(BoundError::ZeroLength("unique_suffix_bytes"));
            }
            let escaped_suffix = (unique_suffix_bytes + escape_bytes) as f64;
            let amortized_prefix = (shared_prefix_bytes as f64) / (n as f64);
            let base = (VALUE_SLOT_BYTES as f64) + amortized_prefix + escaped_suffix;
            Ok(MemoryEnvelope {
                min_bytes_per_key: base + internal_min,
                max_bytes_per_key: base + internal_max + LEAF_CLASS_SLACK_MAX_BYTES,
                escaped_key_bytes: (shared_prefix_bytes as f64) + escaped_suffix,
                trie_entries_per_item: 1,
            })
        }
        KeyShape::UniformRandom { key_bytes } => {
            if key_bytes == 0 {
                return Err(BoundError::ZeroLength("key_bytes"));
            }
            let escaped_len = (key_bytes as f64) * ESCAPE_EXPANSION_UNIFORM;
            let branch_depth = log256(n)?;
            let stored_suffix = (escaped_len - branch_depth).max(0.0);
            let base = (VALUE_SLOT_BYTES as f64) + stored_suffix;
            Ok(MemoryEnvelope {
                min_bytes_per_key: base + internal_min,
                max_bytes_per_key: base + internal_max + LEAF_CLASS_SLACK_MAX_BYTES,
                escaped_key_bytes: escaped_len,
                trie_entries_per_item: 1,
            })
        }
        KeyShape::HashFields {
            key_bytes,
            fields_per_key,
            field_bytes,
        } => {
            if key_bytes == 0 {
                return Err(BoundError::ZeroLength("key_bytes"));
            }
            if fields_per_key == 0 {
                return Err(BoundError::ZeroCount("fields_per_key"));
            }
            if field_bytes == 0 {
                return Err(BoundError::ZeroLength("field_bytes"));
            }
            // Composite key: [0x02][varint(key_bytes)][key_bytes][field_bytes].
            let prefix_len = 1 + varint_len(key_bytes) + key_bytes;
            let amortized_prefix = (prefix_len as f64) / (fields_per_key as f64);
            let base = (VALUE_SLOT_BYTES as f64) + amortized_prefix + (field_bytes as f64);
            Ok(MemoryEnvelope {
                min_bytes_per_key: base + internal_min,
                max_bytes_per_key: base + internal_max + LEAF_CLASS_SLACK_MAX_BYTES,
                escaped_key_bytes: (prefix_len + field_bytes) as f64,
                trie_entries_per_item: 1,
            })
        }
        KeyShape::ZSetMembers {
            key_bytes,
            members_per_key,
            member_bytes,
        } => {
            if key_bytes == 0 {
                return Err(BoundError::ZeroLength("key_bytes"));
            }
            if members_per_key == 0 {
                return Err(BoundError::ZeroCount("members_per_key"));
            }
            if member_bytes == 0 {
                return Err(BoundError::ZeroLength("member_bytes"));
            }
            // Two entries per member:
            //   1. Score index:  [0x03][varint(key)][key][8-byte score][member] -> u64
            //   2. Member index: [0x04][varint(key)][key][member] -> u64
            let prefix_len = 1 + varint_len(key_bytes) + key_bytes;
            let amortized_prefix_both = (2.0 * prefix_len as f64) / (members_per_key as f64);
            let unique_both = (ZSET_SCORE_BYTES + 2 * member_bytes) as f64;
            let value_slots_both = (2 * VALUE_SLOT_BYTES) as f64;
            let base = value_slots_both + amortized_prefix_both + unique_both;
            Ok(MemoryEnvelope {
                min_bytes_per_key: base + 2.0 * internal_min,
                max_bytes_per_key: base + 2.0 * (internal_max + LEAF_CLASS_SLACK_MAX_BYTES),
                escaped_key_bytes: (2 * prefix_len + ZSET_SCORE_BYTES + 2 * member_bytes) as f64,
                trie_entries_per_item: 2,
            })
        }
    }
}

/// Analytical cost breakdown for a single cross-shard `SCAN` batch of `count`
/// keys across `p` hash-partitioned shards (`docs/PRD.md` §3.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanMergeCost {
    /// Number of shard cursor seeks required to initialize the batch (`p`).
    pub init_seeks: usize,
    /// Upper bound on key comparisons to build the initial $P$-way min-heap
    /// (`0` for `p == 1`, `2 * p` for `p >= 2`; Floyd's heapify).
    pub heap_build_comparisons: usize,
    /// Key comparisons per yielded entry (`ceil(log2(p))`).
    pub per_key_comparisons: usize,
    /// Total upper-bound key comparisons for the batch (`heap_build + count * per_key`).
    pub total_comparisons: usize,
    /// Cursor state memory required to hold one bookmark/head per shard (bytes).
    pub cursor_state_bytes: usize,
    /// Ratio of hash-sharded $P$-way merge batch work to single-shard (or
    /// range-sharded) batch work under the supplied cost weights.
    pub work_ratio_vs_single_shard: f64,
}

/// Computes `ceil(log2(p))` for `p >= 1`.
///
/// # Errors
///
/// Returns [`BoundError::ZeroCount`] if `p == 0`.
pub const fn ceil_log2(p: usize) -> Result<usize, BoundError> {
    if p == 0 {
        return Err(BoundError::ZeroCount("p"));
    }
    if p == 1 {
        Ok(0)
    } else {
        Ok((usize::BITS - (p - 1).leading_zeros()) as usize)
    }
}

/// Computes the $P$-way merge cost for a hash-sharded `SCAN` batch yielding
/// `count` keys across `p` shards (`docs/PRD.md` §3.4).
///
/// # Parameters
///
/// - `p`: number of shards (`>= 1`).
/// - `count`: number of keys yielded in the `SCAN` batch (`>= 1`).
/// - `avg_key_bytes`: average key length stored in cursor bookmarks (`>= 1`).
/// - `seek_cost_ratio`: cost of one shard cursor seek relative to yielding one
///   already-buffered key (dimensionless, `>= 0.0`).
/// - `cmp_cost_ratio`: cost of one heap key comparison relative to yielding one
///   already-buffered key (dimensionless, `>= 0.0`).
///
/// # Primary Sources
///
/// - Knuth, *The Art of Computer Programming*, Vol. 3, §5.2.3 (Floyd's
///   bottom-up heap construction $\le 2P$ comparisons) and §5.4.1 (multiway
///   merging $\lceil \log_2 P \rceil$ comparisons per output key).
///
/// # Errors
///
/// Returns [`BoundError`] if `p == 0`, `count == 0`, `avg_key_bytes == 0`, or
/// either cost ratio is negative or non-finite.
pub fn scan_merge_cost(
    p: usize,
    count: usize,
    avg_key_bytes: usize,
    seek_cost_ratio: f64,
    cmp_cost_ratio: f64,
) -> Result<ScanMergeCost, BoundError> {
    if p == 0 {
        return Err(BoundError::ZeroCount("p"));
    }
    if count == 0 {
        return Err(BoundError::ZeroCount("count"));
    }
    if avg_key_bytes == 0 {
        return Err(BoundError::ZeroLength("avg_key_bytes"));
    }
    if !seek_cost_ratio.is_finite() || seek_cost_ratio < 0.0 {
        return Err(BoundError::InvalidRatio("seek_cost_ratio"));
    }
    if !cmp_cost_ratio.is_finite() || cmp_cost_ratio < 0.0 {
        return Err(BoundError::InvalidRatio("cmp_cost_ratio"));
    }

    let per_key_comparisons = ceil_log2(p)?;
    let heap_build_comparisons = if p == 1 { 0 } else { 2 * p };
    let total_comparisons = heap_build_comparisons + count * per_key_comparisons;
    let cursor_state_bytes = p * (avg_key_bytes + CURSOR_BOOKMARK_OVERHEAD_BYTES);

    let single_shard_work = seek_cost_ratio + (count as f64);
    let p_way_work =
        (p as f64) * seek_cost_ratio + (total_comparisons as f64) * cmp_cost_ratio + (count as f64);
    let work_ratio_vs_single_shard = p_way_work / single_shard_work;

    Ok(ScanMergeCost {
        init_seeks: p,
        heap_build_comparisons,
        per_key_comparisons,
        total_comparisons,
        cursor_state_bytes,
        work_ratio_vs_single_shard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_approx(actual: f64, expected: f64, tol: f64) {
        assert!(
            (actual - expected).abs() <= tol,
            "expected {expected} ± {tol}, got {actual}"
        );
    }

    #[test]
    fn log256_matches_exact_powers_of_256() {
        assert_approx(log256(1).expect("n=1"), 0.0, 1e-12);
        assert_approx(log256(256).expect("n=256"), 1.0, 1e-12);
        assert_approx(log256(65_536).expect("n=256^2"), 2.0, 1e-12);
        assert_approx(log256(16_777_216).expect("n=256^3"), 3.0, 1e-12);
        assert_eq!(log256(0), Err(BoundError::ZeroCount("n")));
    }

    #[test]
    fn varint_len_matches_leb128_boundaries() {
        assert_eq!(varint_len(0), 1);
        assert_eq!(varint_len(127), 1);
        assert_eq!(varint_len(128), 2);
        assert_eq!(varint_len(16_383), 2);
        assert_eq!(varint_len(16_384), 3);
    }

    #[test]
    fn uniform_random_bytes_per_key_pinned_reference() {
        // Reference pin at N = 65_536 (log_256(N) = 2.0), 16-byte uniform keys:
        // escaped_len = 16 * 258 / 256 = 16.125
        // stored_suffix = 16.125 - 2.0 = 14.125
        // internal_min = 16 / ln(256) = 2.8853900817779268
        // internal_max = 64 / ln(256) = 11.541560327111707
        // min = 8 + 14.125 + 2.8853900817779268 = 25.010390081777927
        // max = 8 + 14.125 + 11.541560327111707 + 8 = 41.66656032711171
        let env = expected_bytes_per_key(KeyShape::UniformRandom { key_bytes: 16 }, 65_536)
            .expect("valid uniform shape");
        assert_eq!(env.trie_entries_per_item, 1);
        assert_approx(env.escaped_key_bytes, 16.125, 1e-12);
        assert_approx(env.min_bytes_per_key, 25.010390081777927, 1e-9);
        assert_approx(env.max_bytes_per_key, 41.66656032711171, 1e-9);
    }

    #[test]
    fn prefix_structured_amortizes_shared_prefix() {
        // org:user:session: (17 B shared) + 8 B unique suffix, N = 1_000_000:
        // amortized_prefix = 17e-6, base = 8 + 8 + 17e-6 = 16.000017
        let env = expected_bytes_per_key(
            KeyShape::PrefixStructured {
                shared_prefix_bytes: 17,
                unique_suffix_bytes: 8,
                escape_bytes: 0,
            },
            1_000_000,
        )
        .expect("valid prefix shape");
        assert_eq!(env.trie_entries_per_item, 1);
        assert_approx(env.escaped_key_bytes, 25.0, 1e-12);
        assert_approx(env.min_bytes_per_key, 16.000017 + 16.0 / LN_256, 1e-9);

        // At N = 1, the full 17 B prefix is paid by the sole key.
        let env_single = expected_bytes_per_key(
            KeyShape::PrefixStructured {
                shared_prefix_bytes: 17,
                unique_suffix_bytes: 8,
                escape_bytes: 0,
            },
            1,
        )
        .expect("valid single-key prefix shape");
        assert_approx(
            env_single.min_bytes_per_key - env.min_bytes_per_key,
            17.0 - 17e-6,
            1e-9,
        );
    }

    #[test]
    fn hash_and_zset_composite_shapes_pinned_reference() {
        // Hash: key_bytes = 10 -> prefix = 1 (tag) + 1 (varint) + 10 = 12 B.
        // With 100 fields of 8 B each: amortized_prefix = 0.12 B, base = 8 + 0.12 + 8 = 16.12 B.
        let hash_env = expected_bytes_per_key(
            KeyShape::HashFields {
                key_bytes: 10,
                fields_per_key: 100,
                field_bytes: 8,
            },
            100_000,
        )
        .expect("valid hash shape");
        assert_eq!(hash_env.trie_entries_per_item, 1);
        assert_approx(hash_env.min_bytes_per_key, 16.12 + 16.0 / LN_256, 1e-9);

        // ZSet: key_bytes = 10 -> prefix = 12 B per index, 24 B across both.
        // With 100 members of 8 B each: amortized_prefix_both = 0.24 B,
        // unique_both = 8 (score) + 16 (2 * member) = 24 B, value_slots_both = 16 B.
        // base = 40.24 B, min = 40.24 + 32 / ln(256).
        let zset_env = expected_bytes_per_key(
            KeyShape::ZSetMembers {
                key_bytes: 10,
                members_per_key: 100,
                member_bytes: 8,
            },
            100_000,
        )
        .expect("valid zset shape");
        assert_eq!(zset_env.trie_entries_per_item, 2);
        assert_approx(zset_env.min_bytes_per_key, 40.24 + 32.0 / LN_256, 1e-9);
        assert!(zset_env.max_bytes_per_key > zset_env.min_bytes_per_key);
    }

    #[test]
    fn expected_bytes_per_key_rejects_invalid_inputs() {
        assert_eq!(
            expected_bytes_per_key(KeyShape::UniformRandom { key_bytes: 16 }, 0),
            Err(BoundError::ZeroCount("n"))
        );
        assert_eq!(
            expected_bytes_per_key(KeyShape::UniformRandom { key_bytes: 0 }, 10),
            Err(BoundError::ZeroLength("key_bytes"))
        );
        assert_eq!(
            expected_bytes_per_key(
                KeyShape::HashFields {
                    key_bytes: 8,
                    fields_per_key: 0,
                    field_bytes: 4
                },
                10
            ),
            Err(BoundError::ZeroCount("fields_per_key"))
        );
        assert_eq!(
            expected_bytes_per_key(
                KeyShape::ZSetMembers {
                    key_bytes: 8,
                    members_per_key: 10,
                    member_bytes: 0
                },
                10
            ),
            Err(BoundError::ZeroLength("member_bytes"))
        );
    }

    #[test]
    fn ceil_log2_matches_exact_values() {
        assert_eq!(ceil_log2(0), Err(BoundError::ZeroCount("p")));
        assert_eq!(ceil_log2(1), Ok(0));
        assert_eq!(ceil_log2(2), Ok(1));
        assert_eq!(ceil_log2(3), Ok(2));
        assert_eq!(ceil_log2(4), Ok(2));
        assert_eq!(ceil_log2(5), Ok(3));
        assert_eq!(ceil_log2(8), Ok(3));
        assert_eq!(ceil_log2(16), Ok(4));
        assert_eq!(ceil_log2(32), Ok(5));
    }

    #[test]
    fn scan_merge_cost_pinned_against_knuth_heap_bounds() {
        // Single shard (P = 1): zero merge comparisons, work ratio = 1.0 exactly.
        let p1 = scan_merge_cost(1, 100, 24, 10.0, 0.25).expect("p=1");
        assert_eq!(p1.init_seeks, 1);
        assert_eq!(p1.heap_build_comparisons, 0);
        assert_eq!(p1.per_key_comparisons, 0);
        assert_eq!(p1.total_comparisons, 0);
        assert_eq!(p1.cursor_state_bytes, 48);
        assert_approx(p1.work_ratio_vs_single_shard, 1.0, 1e-12);

        // P = 16 shards, count = 100, avg_key_bytes = 24, seek_cost_ratio = 10, cmp_cost_ratio = 0.25:
        // heap_build = 32, per_key = 4, total_comparisons = 32 + 400 = 432
        // single_shard_work = 10 + 100 = 110
        // p_way_work = 16 * 10 + 432 * 0.25 + 100 = 160 + 108 + 100 = 368
        // work_ratio = 368 / 110 = 3.3454545454545452
        let p16 = scan_merge_cost(16, 100, 24, 10.0, 0.25).expect("p=16");
        assert_eq!(p16.init_seeks, 16);
        assert_eq!(p16.heap_build_comparisons, 32);
        assert_eq!(p16.per_key_comparisons, 4);
        assert_eq!(p16.total_comparisons, 432);
        assert_eq!(p16.cursor_state_bytes, 16 * 48);
        assert_approx(p16.work_ratio_vs_single_shard, 368.0 / 110.0, 1e-12);

        // Small COUNT (10) suffers higher relative fan-out overhead than large COUNT (1000).
        let p16_small = scan_merge_cost(16, 10, 24, 10.0, 0.25).expect("p=16 small");
        let p16_large = scan_merge_cost(16, 1000, 24, 10.0, 0.25).expect("p=16 large");
        assert!(p16_small.work_ratio_vs_single_shard > p16.work_ratio_vs_single_shard);
        assert!(p16.work_ratio_vs_single_shard > p16_large.work_ratio_vs_single_shard);
    }

    #[test]
    fn scan_merge_cost_rejects_invalid_inputs() {
        assert_eq!(
            scan_merge_cost(0, 100, 24, 10.0, 0.25),
            Err(BoundError::ZeroCount("p"))
        );
        assert_eq!(
            scan_merge_cost(4, 0, 24, 10.0, 0.25),
            Err(BoundError::ZeroCount("count"))
        );
        assert_eq!(
            scan_merge_cost(4, 100, 0, 10.0, 0.25),
            Err(BoundError::ZeroLength("avg_key_bytes"))
        );
        assert_eq!(
            scan_merge_cost(4, 100, 24, -1.0, 0.25),
            Err(BoundError::InvalidRatio("seek_cost_ratio"))
        );
        assert_eq!(
            scan_merge_cost(4, 100, 24, 10.0, f64::NAN),
            Err(BoundError::InvalidRatio("cmp_cost_ratio"))
        );
    }
}
