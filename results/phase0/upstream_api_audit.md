# Phase 0 Upstream API Audit (`expanse-trie 0.19.0`)

- **PRD gate:** `docs/PRD.md` §7, Phase 0 item 2
- **Tracking issue:** `orieg/expanse-kv#3`
- **Upstream issue filed:** `orieg/expanse#1801` (related to `orieg/expanse#1563`)
- **Verification suite:** [`crates/expanse-kv-engine/tests/upstream_api_audit.rs`](../../crates/expanse-kv-engine/tests/upstream_api_audit.rs)

## Findings (verified by test on `expanse-trie 0.19.0`)

### 1. Byte-lexicographical ordering

`SyncExpanseOrderedBytesMap` encodes arbitrary binary keys with an order-preserving, NUL-free escape (`0x00 -> [0x01, 0x01]`, `0x01 -> [0x01, 0x02]`, `0x02..=0xFF` unchanged) over `SyncExpanseStrMap`.

Forward navigation (`first`, `next_at_or_after`, `next_after`, and `*_decode_into`) and reverse/predecessor navigation (`last`, `prev_at_or_before`, `prev_before`, and `*_decode_into`) match `BTreeMap<Vec<u8>, u64>` across empty keys, embedded `0x00`/`0x01` combinations, prefix-sharing keys, keys exceeding the stack-buffer threshold, and order-preserving IEEE-754 `f64` ZSet composite keys.

### 2. Concurrency path classification (optimistic vs writer-locked)

Verified empirically by holding the writer mutex on a separate thread via `SyncExpanseOrderedBytesMap::with_locked` and observing whether each operation completes under the lock (optimistic) or blocks until release (writer-locked):

| Surface | Methods | Concurrency Path | Unpinned Batch Cursor? |
| :--- | :--- | :--- | :--- |
| `OrderedBytesReader` | `get`, `contains` | Optimistic (OCC + mutex fallback) | N/A (point lookup) |
| `OrderedBytesReader` | `first`, `last`, `next_*`, `prev_*`, `cursor`, `range_cursor`, `prefix_cursor` | **Absent** in `0.19.0` | **Absent** |
| `SyncExpanseOrderedBytesMap` | `first`, `last`, `next_at_or_after`, `next_after`, `prev_at_or_before`, `prev_before` (and `*_decode_into`) | **Writer-locked** (`with_locked`) | Single-step only |
| `SyncExpanseOrderedBytesMap::inner().reader()` (`StrReader`) | `first`, `last`, `next_at_or_after`, `next_after`, `prev_at_or_before`, `prev_before` | Optimistic (OCC + mutex fallback) | Single-step only |
| `SyncExpanseOrderedBytesMap::inner().reader()` (`StrReader`) | `cursor`, `range_cursor`, `prefix_cursor` (`SyncStrMapCursor`) | Optimistic (OCC + mutex fallback) | **Yes (forward only)** — unpins between bounded batches |
| `StrReader` / `OrderedBytesReader` | Reverse batch cursor (`rev_cursor`, `rev_range_cursor`, `rev_prefix_cursor`) | **Absent** in `0.19.0` | **Absent** |

### 3. Upstream primitive gaps and `expanse-kv` impact

Filed upstream as [`orieg/expanse#1801`](https://github.com/orieg/expanse/issues/1801) (related to [`orieg/expanse#1563`](https://github.com/orieg/expanse/issues/1563)):

1. **`OrderedBytesReader` optimistic ordered navigation and forward batch cursors.** Until `expanse-trie` exposes these directly on `OrderedBytesReader`, `expanse-kv` can work above the public API (`SyncExpanseOrderedBytesMap::inner().reader()` + `expanse_trie::domain::{escape_encode, escape_decode}` + `NulFreeStr`) without taking the writer mutex or patching the trie (`AGENTS.md` §2.1).
2. **Optimistic reverse batch cursor (`ZRANGE ... REV`).** Neither `StrReader` nor `OrderedBytesReader` provides a descending batch cursor in `0.19.0`. Until upstream adds one, descending scans either compose single-step optimistic `StrReader::prev_before` calls (each unpinned, resuming by key bookmark) or wait for `orieg/expanse#1801` before Phase 2.
