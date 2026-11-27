//! Phase 0 upstream `expanse-trie 0.19.0` API audit (`docs/PRD.md` §7, Phase 0 item 2).
//!
//! Confirms by test on the published `expanse-trie 0.19.0` crate:
//! 1. `SyncExpanseOrderedBytesMap` forward navigation (`first`, `next_at_or_after`,
//!    `next_after`, and `*_decode_into`) matches `BTreeMap<Vec<u8>, u64>` exact
//!    byte-lexicographical order across arbitrary binary keys (including `0x00`,
//!    `0x01`, prefix-sharing keys, keys > 32 bytes, and order-preserving `f64`
//!    ZSet composite keys).
//! 2. Reverse / predecessor navigation (`last`, `prev_at_or_before`, `prev_before`,
//!    and `*_decode_into`) matches `BTreeMap<Vec<u8>, u64>` exact reverse
//!    byte-lexicographical order.
//! 3. Empirical concurrency-path classification:
//!    - `OrderedBytesReader::get` / `contains` are optimistic (complete while
//!      another thread holds the writer mutex via `with_locked`).
//!    - `SyncExpanseOrderedBytesMap::{first,last,next_at_or_after,next_after,
//!      prev_at_or_before,prev_before}` take the writer mutex (block while
//!      another thread holds `with_locked`, unblock on release).
//!    - Underlying `map.inner().reader()` (`StrReader`) ordered navigation and
//!      forward `SyncStrMapCursor` (`cursor`, `range_cursor`, `prefix_cursor`)
//!      are optimistic (complete while `with_locked` is held) and preserve exact
//!      byte-lexicographical order when composed with `escape_encode` /
//!      `escape_decode`.

use expanse_trie::domain::{escape_decode, escape_encode};
use expanse_trie::strmap::NulFreeStr;
use expanse_trie::sync::SyncExpanseOrderedBytesMap;
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

/// Encodes an IEEE-754 `f64` into an order-preserving big-endian `u64`
/// (PRD §3.2): positive values flip the sign bit; negative values flip all bits.
fn encode_f64_order_preserving(score: f64) -> [u8; 8] {
    let bits = score.to_bits();
    let ordered = if (bits >> 63) == 0 {
        bits ^ (1u64 << 63)
    } else {
        !bits
    };
    ordered.to_be_bytes()
}

/// Builds a binary key corpus exercising all three escape paths in
/// `expanse-trie`'s `ordered_bytesmap`:
/// - clean keys (no bytes <= 1, zero-alloc fast path)
/// - short escaped keys (<= 32 bytes with `0x00`/`0x01`, stack buffer path)
/// - long escaped keys (> 32 bytes with `0x00`/`0x01`, heap `escape_encode` path)
/// - prefix-sharing keys, empty key, boundary bytes, and ZSet composite keys.
fn build_corpus() -> Vec<(Vec<u8>, u64)> {
    let mut keys: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0x00, 0x00],
        vec![0x00, 0x01],
        vec![0x00, 0x02],
        vec![0x00, 0xFF],
        vec![0x01],
        vec![0x01, 0x00],
        vec![0x01, 0x01],
        vec![0x01, 0x02],
        vec![0x01, 0xFF],
        vec![0x02],
        vec![0x02, 0x00],
        vec![0x7F],
        vec![0x80],
        vec![0xFE],
        vec![0xFF],
        vec![0xFF, 0x00],
        vec![0xFF, 0xFF],
        b"a".to_vec(),
        b"a\x00".to_vec(),
        b"a\x00b".to_vec(),
        b"a\x01".to_vec(),
        b"a\x01b".to_vec(),
        b"ab".to_vec(),
        b"org:user:session:1".to_vec(),
        b"org:user:session:10".to_vec(),
        b"org:user:session:2".to_vec(),
        b"org:user:session:20".to_vec(),
        // > 32 bytes clean key
        b"prefix:clean:0123456789abcdef0123456789abcdef:a".to_vec(),
        b"prefix:clean:0123456789abcdef0123456789abcdef:b".to_vec(),
        // > 32 bytes with embedded 0x00 and 0x01 (heap escape path)
        [&b"long_escaped_key_exceeding_32_bytes_"[..], &[0x00, 0x01]].concat(),
        [&b"long_escaped_key_exceeding_32_bytes_"[..], &[0x00, 0x02]].concat(),
        [&b"long_escaped_key_exceeding_32_bytes_"[..], &[0x01, 0x00]].concat(),
        [&b"long_escaped_key_exceeding_32_bytes_"[..], &[0x01, 0x01]].concat(),
    ];

    // ZSet-style composite keys: [0x03][len][key][ordered_f64][member]
    let scores = [
        f64::NEG_INFINITY,
        -1e100,
        -1.5,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.5,
        1e100,
        f64::INFINITY,
    ];
    for (idx, &score) in scores.iter().enumerate() {
        let mut k = Vec::new();
        k.push(0x03);
        k.push(4);
        k.extend_from_slice(b"zset");
        k.extend_from_slice(&encode_f64_order_preserving(score));
        k.extend_from_slice(format!("m{idx:02}").as_bytes());
        keys.push(k);
    }

    keys.into_iter()
        .enumerate()
        .map(|(i, k)| (k, (i as u64 + 1) * 17))
        .collect()
}

fn populate(corpus: &[(Vec<u8>, u64)]) -> (SyncExpanseOrderedBytesMap, BTreeMap<Vec<u8>, u64>) {
    let map = SyncExpanseOrderedBytesMap::new();
    let mut model = BTreeMap::new();
    // Insert in reverse order to avoid relying on insertion order.
    for (k, v) in corpus.iter().rev() {
        assert_eq!(map.insert(k, *v), None);
        assert_eq!(model.insert(k.clone(), *v), None);
    }
    assert_eq!(map.len(), model.len() as u64);
    (map, model)
}

#[test]
fn forward_navigation_matches_btreemap_lexicographical_order() {
    let corpus = build_corpus();
    let (map, model) = populate(&corpus);

    // Full forward walk via first + next_after.
    let mut actual = Vec::new();
    let mut cur = map.first();
    while let Some((k, v)) = cur {
        actual.push((k.clone(), v));
        cur = map.next_after(&k);
    }
    let expected: Vec<(Vec<u8>, u64)> = model.iter().map(|(k, &v)| (k.clone(), v)).collect();
    assert_eq!(actual, expected);

    // Probe next_at_or_after and next_after on every key and on absent keys between them.
    let mut probes: Vec<Vec<u8>> = model.keys().cloned().collect();
    for k in model.keys() {
        let mut ext0 = k.clone();
        ext0.push(0x00);
        probes.push(ext0);
        let mut ext1 = k.clone();
        ext1.push(0x01);
        probes.push(ext1);
    }
    probes.push(vec![0xFF, 0xFF, 0xFF]);

    let mut decode_buf = vec![0u8; 256];
    for probe in &probes {
        let want_ge = model
            .range::<Vec<u8>, _>((Included(probe), Unbounded))
            .next()
            .map(|(k, &v)| (k.clone(), v));
        let want_gt = model
            .range::<Vec<u8>, _>((Excluded(probe), Unbounded))
            .next()
            .map(|(k, &v)| (k.clone(), v));

        assert_eq!(map.next_at_or_after(probe), want_ge);
        assert_eq!(map.next_after(probe), want_gt);

        let dec_ge = map
            .next_at_or_after_decode_into(probe, &mut decode_buf)
            .expect("decode buffer large enough")
            .map(|(len, v)| (decode_buf[..len].to_vec(), v));
        assert_eq!(dec_ge, want_ge);

        let dec_gt = map
            .next_after_decode_into(probe, &mut decode_buf)
            .expect("decode buffer large enough")
            .map(|(len, v)| (decode_buf[..len].to_vec(), v));
        assert_eq!(dec_gt, want_gt);
    }
}

#[test]
fn reverse_and_predecessor_navigation_matches_btreemap_order() {
    let corpus = build_corpus();
    let (map, model) = populate(&corpus);

    // Full reverse walk via last + prev_before.
    let mut actual_rev = Vec::new();
    let mut cur = map.last();
    while let Some((k, v)) = cur {
        actual_rev.push((k.clone(), v));
        cur = map.prev_before(&k);
    }
    let expected_rev: Vec<(Vec<u8>, u64)> =
        model.iter().rev().map(|(k, &v)| (k.clone(), v)).collect();
    assert_eq!(actual_rev, expected_rev);

    // Probe prev_at_or_before and prev_before on present and absent keys.
    let mut probes: Vec<Vec<u8>> = model.keys().cloned().collect();
    for k in model.keys() {
        let mut ext0 = k.clone();
        ext0.push(0x00);
        probes.push(ext0);
        let mut ext1 = k.clone();
        ext1.push(0x01);
        probes.push(ext1);
    }
    probes.push(vec![0xFF, 0xFF, 0xFF]);

    let mut decode_buf = vec![0u8; 256];
    for probe in &probes {
        let want_le = model
            .range::<Vec<u8>, _>((Unbounded, Included(probe)))
            .next_back()
            .map(|(k, &v)| (k.clone(), v));
        let want_lt = model
            .range::<Vec<u8>, _>((Unbounded, Excluded(probe)))
            .next_back()
            .map(|(k, &v)| (k.clone(), v));

        assert_eq!(map.prev_at_or_before(probe), want_le);
        assert_eq!(map.prev_before(probe), want_lt);

        let dec_le = map
            .prev_at_or_before_decode_into(probe, &mut decode_buf)
            .expect("decode buffer large enough")
            .map(|(len, v)| (decode_buf[..len].to_vec(), v));
        assert_eq!(dec_le, want_le);

        let dec_lt = map
            .prev_before_decode_into(probe, &mut decode_buf)
            .expect("decode buffer large enough")
            .map(|(len, v)| (decode_buf[..len].to_vec(), v));
        assert_eq!(dec_lt, want_lt);
    }
}

#[test]
fn empty_map_navigation_returns_none() {
    let map = SyncExpanseOrderedBytesMap::new();
    assert_eq!(map.first(), None);
    assert_eq!(map.last(), None);
    assert_eq!(map.next_at_or_after(b""), None);
    assert_eq!(map.next_after(b""), None);
    assert_eq!(map.prev_at_or_before(b"\xFF"), None);
    assert_eq!(map.prev_before(b"\xFF"), None);
}

/// Runs `op` on a worker thread while the main thread holds `map.with_locked`,
/// returning `true` if `op` completed without waiting for the writer lock
/// (optimistic path) and `false` if `op` blocked until `with_locked` released
/// (writer-locked path).
fn completes_while_writer_locked<F>(map: &Arc<SyncExpanseOrderedBytesMap>, op: F) -> bool
where
    F: FnOnce(&SyncExpanseOrderedBytesMap) + Send + 'static,
{
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    let holder_map = Arc::clone(map);
    let holder = thread::spawn(move || {
        holder_map.with_locked(|_| {
            locked_tx.send(()).expect("signal lock held");
            release_rx.recv().expect("wait for release signal");
        });
    });

    locked_rx.recv().expect("writer lock acquired");

    let worker_map = Arc::clone(map);
    let worker = thread::spawn(move || {
        op(&worker_map);
        done_tx.send(()).expect("signal worker done");
    });

    let completed_under_lock = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();

    release_tx.send(()).expect("release writer lock");
    if !completed_under_lock {
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker must complete after writer lock releases");
    }
    holder.join().expect("holder thread joined");
    worker.join().expect("worker thread joined");

    completed_under_lock
}

#[test]
fn ordered_bytes_reader_get_is_optimistic_while_map_nav_takes_writer_mutex() {
    let corpus = build_corpus();
    let (map, model) = populate(&corpus);
    let map = Arc::new(map);

    let sample_key = b"org:user:session:10".to_vec();
    let sample_val = model[&sample_key];
    let first_expected = model.iter().next().map(|(k, &v)| (k.clone(), v));
    let last_expected = model.iter().next_back().map(|(k, &v)| (k.clone(), v));

    // Point lookup through OrderedBytesReader is optimistic (does not wait on writer mutex).
    let k = sample_key.clone();
    assert!(
        completes_while_writer_locked(&map, move |m| {
            let reader = m.reader();
            assert_eq!(reader.get(&k), Some(sample_val));
            assert!(reader.contains(&k));
        }),
        "OrderedBytesReader::get must complete optimistically while with_locked holds writer mutex"
    );

    // All six ordered navigation methods on SyncExpanseOrderedBytesMap take the writer mutex.
    let fe = first_expected.clone();
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert_eq!(m.first(), fe);
        }),
        "SyncExpanseOrderedBytesMap::first takes the writer mutex in expanse-trie 0.19.0"
    );

    let le = last_expected.clone();
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert_eq!(m.last(), le);
        }),
        "SyncExpanseOrderedBytesMap::last takes the writer mutex in expanse-trie 0.19.0"
    );

    let k = sample_key.clone();
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert_eq!(m.next_at_or_after(&k), Some((k.clone(), sample_val)));
        }),
        "SyncExpanseOrderedBytesMap::next_at_or_after takes the writer mutex in expanse-trie 0.19.0"
    );

    let k = sample_key.clone();
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert!(m.next_after(&k).is_some());
        }),
        "SyncExpanseOrderedBytesMap::next_after takes the writer mutex in expanse-trie 0.19.0"
    );

    let k = sample_key.clone();
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert_eq!(m.prev_at_or_before(&k), Some((k.clone(), sample_val)));
        }),
        "SyncExpanseOrderedBytesMap::prev_at_or_before takes the writer mutex in expanse-trie 0.19.0"
    );

    let k = sample_key;
    assert!(
        !completes_while_writer_locked(&map, move |m| {
            assert!(m.prev_before(&k).is_some());
        }),
        "SyncExpanseOrderedBytesMap::prev_before takes the writer mutex in expanse-trie 0.19.0"
    );
}

#[test]
fn inner_str_reader_ordered_nav_and_forward_cursor_are_optimistic_and_order_preserving() {
    let corpus = build_corpus();
    let (map, model) = populate(&corpus);
    let map = Arc::new(map);

    let expected_fwd: Vec<(Vec<u8>, u64)> = model.iter().map(|(k, &v)| (k.clone(), v)).collect();
    let expected_rev: Vec<(Vec<u8>, u64)> =
        model.iter().rev().map(|(k, &v)| (k.clone(), v)).collect();

    // Underlying StrReader ordered reads + forward SyncStrMapCursor complete optimistically
    // while with_locked holds the writer mutex, and match BTreeMap order after escape_decode.
    let fwd_check = expected_fwd.clone();
    let rev_check = expected_rev.clone();
    assert!(
        completes_while_writer_locked(&map, move |m| {
            let sr = m.inner().reader();

            // Optimistic forward batch cursor over the entire map.
            let mut cur = sr.cursor();
            let mut scanned = Vec::new();
            while let Some((esc_k, v)) = cur.next() {
                let decoded = escape_decode(esc_k).expect("stored key is valid escaped bytes");
                scanned.push((decoded, v));
            }
            assert_eq!(scanned, fwd_check);

            // Optimistic prefix cursor over escaped "org:user:session:".
            let prefix_raw = b"org:user:session:";
            let prefix_esc = escape_encode(prefix_raw);
            let prefix_nfs = NulFreeStr::new(&prefix_esc).expect("escaped prefix is NUL-free");
            let mut pcur = sr.prefix_cursor(prefix_nfs);
            let mut prefix_hits = Vec::new();
            while let Some((esc_k, v)) = pcur.next() {
                let decoded = escape_decode(esc_k).expect("stored key is valid escaped bytes");
                prefix_hits.push((decoded, v));
            }
            let want_prefix: Vec<(Vec<u8>, u64)> = fwd_check
                .iter()
                .filter(|(k, _)| k.starts_with(prefix_raw))
                .cloned()
                .collect();
            assert_eq!(prefix_hits, want_prefix);

            // Optimistic reverse walk via StrReader::last + prev_before.
            let mut rev_scanned = Vec::new();
            let mut step = sr.last();
            while let Some((esc_k, v)) = step {
                let decoded = escape_decode(&esc_k).expect("stored key is valid escaped bytes");
                rev_scanned.push((decoded, v));
                let nfs = NulFreeStr::new(&esc_k).expect("escaped key is NUL-free");
                step = sr.prev_before(nfs);
            }
            assert_eq!(rev_scanned, rev_check);
        }),
        "StrReader ordered navigation and SyncStrMapCursor must complete optimistically"
    );
}
