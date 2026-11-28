//! Stateless key-bookmark optimistic cursors over `SyncExpanseOrderedBytesMap`
//! (`AGENTS.md` §2.3, §2.5).
//!
//! Every function here borrows `map.inner().reader()` (`StrReader`) for one
//! bounded batch, copies decoded `(key, value)` entries out, and drops the
//! reader/cursor before returning. No reclamation epoch pin is ever held
//! across caller I/O or across `SCAN` / `HSCAN` / `ZRANGE` round-trips.

use expanse_trie::domain::{escape_decode, escape_encode};
use expanse_trie::strmap::NulFreeStr;
use expanse_trie::sync::SyncExpanseOrderedBytesMap;

/// Encodes the immediate NUL-free escaped successor of `key`:
/// `escape_encode(key) || [0x01]`.
fn encode_strict_successor(key: &[u8]) -> Vec<u8> {
    let mut esc = escape_encode(key);
    esc.push(0x01);
    esc
}

/// Scans up to `count` entries whose raw keys start with `prefix` and (when
/// `after` is `Some`) are strictly greater than `after`, in ascending
/// byte-lexicographical order.
///
/// Uses `StrReader::prefix_cursor` (optimistic, unpinned between batches).
#[must_use]
pub fn scan_prefix_batch(
    map: &SyncExpanseOrderedBytesMap,
    prefix: &[u8],
    after: Option<&[u8]>,
    count: usize,
) -> Vec<(Vec<u8>, u64)> {
    if count == 0 {
        return Vec::new();
    }
    let esc_prefix = escape_encode(prefix);
    let prefix_nfs = NulFreeStr::new(&esc_prefix).expect("escaped prefix is NUL-free");

    let reader = map.inner().reader();
    let mut cursor = reader.prefix_cursor(prefix_nfs);
    cursor.set_batch_cap(count.clamp(1, 256));

    if let Some(bookmark) = after {
        let succ = encode_strict_successor(bookmark);
        let succ_nfs = NulFreeStr::new(&succ).expect("escaped successor is NUL-free");
        if cursor.advance_to(succ_nfs).is_none() {
            return Vec::new();
        }
    }

    let mut out = Vec::with_capacity(count.min(256));
    while out.len() < count {
        let Some((esc_k, v)) = cursor.next() else {
            break;
        };
        let dec = escape_decode(esc_k).expect("stored key is valid escaped bytes");
        if !dec.starts_with(prefix) {
            break;
        }
        out.push((dec, v));
    }
    out
}

/// Scans all entries whose raw keys start with `prefix` in ascending order,
/// returning `(decoded_key, value)` pairs.
#[must_use]
pub fn collect_prefix(map: &SyncExpanseOrderedBytesMap, prefix: &[u8]) -> Vec<(Vec<u8>, u64)> {
    let mut all = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch = scan_prefix_batch(map, prefix, after.as_deref(), 256);
        if batch.is_empty() {
            break;
        }
        after = batch.last().map(|(k, _)| k.clone());
        all.extend(batch);
    }
    all
}

/// Scans entries in `[start_key, end_key]` (with configurable inclusivity) in
/// **ascending** byte-lexicographical order, skipping `offset` matches and
/// returning at most `limit` entries.
#[must_use]
pub fn scan_range_forward(
    map: &SyncExpanseOrderedBytesMap,
    prefix: &[u8],
    lo: Option<(&[u8], bool)>,
    hi: Option<(&[u8], bool)>,
    offset: usize,
    limit: Option<usize>,
) -> Vec<(Vec<u8>, u64)> {
    if limit == Some(0) {
        return Vec::new();
    }
    let esc_prefix = escape_encode(prefix);
    let prefix_nfs = NulFreeStr::new(&esc_prefix).expect("escaped prefix is NUL-free");
    let reader = map.inner().reader();
    let mut cursor = reader.prefix_cursor(prefix_nfs);

    if let Some((lo_key, inclusive)) = lo {
        let seek_bytes = if inclusive {
            escape_encode(lo_key)
        } else {
            encode_strict_successor(lo_key)
        };
        let seek_nfs = NulFreeStr::new(&seek_bytes).expect("escaped seek is NUL-free");
        if cursor.advance_to(seek_nfs).is_none() {
            return Vec::new();
        }
    }

    let mut skipped = 0usize;
    let mut out = Vec::new();
    while let Some((esc_k, v)) = cursor.next() {
        let dec = escape_decode(esc_k).expect("stored key is valid escaped bytes");
        if !dec.starts_with(prefix) {
            break;
        }
        if let Some((hi_key, inclusive)) = hi {
            let past_end = if inclusive {
                dec.as_slice() > hi_key
            } else {
                dec.as_slice() >= hi_key
            };
            if past_end {
                break;
            }
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        out.push((dec, v));
        if let Some(max) = limit
            && out.len() >= max
        {
            break;
        }
    }
    out
}

/// Scans entries under `prefix` within optional `[lo, hi]` bounds in
/// **descending** byte-lexicographical order using optimistic `StrReader::prev_*`
/// steps (unpinned per step, resuming by key bookmark; upstream reverse batch
/// cursor tracked in `orieg/expanse#1801`).
#[must_use]
pub fn scan_range_reverse(
    map: &SyncExpanseOrderedBytesMap,
    prefix: &[u8],
    lo: Option<(&[u8], bool)>,
    hi: Option<(&[u8], bool)>,
    offset: usize,
    limit: Option<usize>,
) -> Vec<(Vec<u8>, u64)> {
    if limit == Some(0) {
        return Vec::new();
    }
    let reader = map.inner().reader();

    // Determine initial upper bound in escaped space.
    let mut current: Option<(Vec<u8>, u64)> = match hi {
        Some((hi_key, inclusive)) => {
            let esc = escape_encode(hi_key);
            let nfs = NulFreeStr::new(&esc).expect("escaped hi is NUL-free");
            if inclusive {
                reader.prev_at_or_before(nfs)
            } else {
                reader.prev_before(nfs)
            }
        }
        None => {
            // Find the first key strictly past every extension of `prefix`.
            let esc_prefix = escape_encode(prefix);
            match prefix_upper_bound(&esc_prefix) {
                Some(upper) => {
                    let nfs = NulFreeStr::new(&upper).expect("escaped upper is NUL-free");
                    reader.prev_before(nfs)
                }
                None => reader.last(),
            }
        }
    };

    let mut skipped = 0usize;
    let mut out = Vec::new();

    while let Some((esc_k, v)) = current {
        let dec = escape_decode(&esc_k).expect("stored key is valid escaped bytes");
        if !dec.starts_with(prefix) {
            break;
        }
        if let Some((lo_key, inclusive)) = lo {
            let below_start = if inclusive {
                dec.as_slice() < lo_key
            } else {
                dec.as_slice() <= lo_key
            };
            if below_start {
                break;
            }
        }
        if skipped < offset {
            skipped += 1;
        } else {
            out.push((dec, v));
            if let Some(max) = limit
                && out.len() >= max
            {
                break;
            }
        }
        let nfs = NulFreeStr::new(&esc_k).expect("escaped key is NUL-free");
        current = reader.prev_before(nfs);
    }
    out
}

/// Returns the smallest byte string strictly greater than every extension of `prefix`,
/// or `None` if `prefix` is empty or all `0xFF`.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while p.last() == Some(&0xFF) {
        p.pop();
    }
    let last = p.last_mut()?;
    *last += 1;
    Some(p)
}
