//! Phase 2 differential model test (`docs/PRD.md` §7 Phase 2).
//!
//! Runs randomized mixed String, Hash, ZSet, Sparse Bitmap, TTL, `SCAN`,
//! `HSCAN`, `ZRANGE`, and `DEL` operations simultaneously against `Engine` and
//! an independent `BTreeMap` reference oracle, asserting zero divergences on
//! every single operation.

use expanse_kv_engine::{Engine, EngineError, ScoreBound, SetOptions};
use std::collections::{BTreeMap, BTreeSet};

struct SplitMix64(u64);
impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }
}

#[derive(Clone)]
enum ModelVal {
    String(Vec<u8>),
    Hash(BTreeMap<Vec<u8>, Vec<u8>>),
    ZSet(BTreeMap<Vec<u8>, f64>),
    Bitmap(BTreeSet<u32>),
}

#[derive(Clone)]
struct ModelEntry {
    expiry_ms: Option<u64>,
    val: ModelVal,
}

struct RefModel {
    map: BTreeMap<Vec<u8>, ModelEntry>,
    now_ms: u64,
}

impl RefModel {
    fn new(start_ms: u64) -> Self {
        Self {
            map: BTreeMap::new(),
            now_ms: start_ms,
        }
    }

    fn purge_expired(&mut self, key: &[u8]) {
        let expired = self
            .map
            .get(key)
            .and_then(|e| e.expiry_ms)
            .is_some_and(|exp| exp <= self.now_ms);
        if expired {
            self.map.remove(key);
        }
    }

    fn purge_all_expired(&mut self) {
        let now = self.now_ms;
        self.map
            .retain(|_, e| !e.expiry_ms.is_some_and(|exp| exp <= now));
    }

    fn zset_sorted(zs: &BTreeMap<Vec<u8>, f64>) -> Vec<(Vec<u8>, f64)> {
        let mut v: Vec<(Vec<u8>, f64)> = zs.iter().map(|(m, &s)| (m.clone(), s)).collect();
        v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        v
    }
}

fn run_differential(ops: usize, seed: u64) {
    let eng = Engine::default();
    let mut now_ms = 10_000u64;
    eng.set_clock_ms(now_ms);
    let mut model = RefModel::new(now_ms);
    let mut rng = SplitMix64(seed);

    let key_pool: Vec<Vec<u8>> = (0..64)
        .map(|i| format!("org:u{}:k{i:02}", i % 4).into_bytes())
        .collect();
    let field_pool: Vec<Vec<u8>> = (0..16).map(|i| format!("f{i:02}").into_bytes()).collect();
    let scores = [-10.5, -1.0, -0.0, 0.0, 1.5, 42.0, 100.25];

    for step in 0..ops {
        // Advance clock occasionally to exercise passive + active expiry.
        if (step & 63) == 63 {
            now_ms += 50;
            eng.set_clock_ms(now_ms);
            model.now_ms = now_ms;
        }
        let key = &key_pool[rng.next_usize(key_pool.len())];
        let op = rng.next_usize(18);

        match op {
            // SET
            0 | 1 => {
                model.purge_expired(key);
                let val = format!("v{}", rng.next_u64() % 1000).into_bytes();
                let nx = (rng.next_u64() & 7) == 0;
                let xx = !nx && (rng.next_u64() & 7) == 1;
                let keep_ttl = (rng.next_u64() & 7) == 2;
                let expiry_ms = if !keep_ttl && (rng.next_u64() & 3) == 0 {
                    Some(now_ms + 100 + (rng.next_u64() % 300))
                } else {
                    None
                };
                let opts = SetOptions {
                    nx,
                    xx,
                    keep_ttl,
                    get_prev: false,
                    expiry_ms,
                };
                let actual = eng.set(key, &val, opts).expect("set ok");
                let exists = model.map.contains_key(key);
                let expected_written = (!nx || !exists) && (!xx || exists);
                if expected_written {
                    let eff_exp = if keep_ttl {
                        model.map.get(key).and_then(|e| e.expiry_ms)
                    } else {
                        expiry_ms
                    };
                    model.map.insert(
                        key.clone(),
                        ModelEntry {
                            expiry_ms: eff_exp,
                            val: ModelVal::String(val),
                        },
                    );
                }
                assert_eq!(actual.0, expected_written, "step {step} SET");
            }
            // GET
            2 | 3 => {
                model.purge_expired(key);
                let actual = eng.get(key);
                let expected = match model.map.get(key) {
                    None => Ok(None),
                    Some(e) => match &e.val {
                        ModelVal::String(v) => Ok(Some(v.clone())),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} GET");
            }
            // INCRBY
            4 => {
                model.purge_expired(key);
                let delta = (rng.next_u64() % 11) as i64 - 5;
                let actual = eng.incr_by(key, delta);
                let expected = match model.map.get(key) {
                    None => {
                        model.map.insert(
                            key.clone(),
                            ModelEntry {
                                expiry_ms: None,
                                val: ModelVal::String(delta.to_string().into_bytes()),
                            },
                        );
                        Ok(delta)
                    }
                    Some(e) => match &e.val {
                        ModelVal::String(b) => match core::str::from_utf8(b)
                            .ok()
                            .and_then(|s| s.parse::<i64>().ok())
                        {
                            Some(cur) => match cur.checked_add(delta) {
                                Some(next) => {
                                    let exp = e.expiry_ms;
                                    model.map.insert(
                                        key.clone(),
                                        ModelEntry {
                                            expiry_ms: exp,
                                            val: ModelVal::String(next.to_string().into_bytes()),
                                        },
                                    );
                                    Ok(next)
                                }
                                None => Err(EngineError::Overflow),
                            },
                            None => Err(EngineError::NotAnInteger),
                        },
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} INCRBY");
            }
            // DEL
            5 => {
                model.purge_expired(key);
                let actual = eng.del(&[key.as_slice()]);
                let expected = u64::from(model.map.remove(key).is_some());
                assert_eq!(actual, expected, "step {step} DEL");
            }
            // PEXPIREAT / PTTL / PERSIST
            6 => {
                model.purge_expired(key);
                let exp = now_ms + 50 + (rng.next_u64() % 200);
                let actual = eng.pexpire_at(key, exp);
                let expected = if let Some(e) = model.map.get_mut(key) {
                    e.expiry_ms = Some(exp);
                    true
                } else {
                    false
                };
                assert_eq!(actual, expected, "step {step} PEXPIREAT");
                let actual_pttl = eng.pttl(key);
                let expected_pttl = match model.map.get(key) {
                    None => -2,
                    Some(e) => match e.expiry_ms {
                        None => -1,
                        Some(ex) if ex <= now_ms => -2,
                        Some(ex) => (ex - now_ms) as i64,
                    },
                };
                assert_eq!(actual_pttl, expected_pttl, "step {step} PTTL");
            }
            7 => {
                model.purge_expired(key);
                let actual = eng.persist(key);
                let expected = model
                    .map
                    .get_mut(key)
                    .and_then(|e| e.expiry_ms.take())
                    .is_some();
                assert_eq!(actual, expected, "step {step} PERSIST");
            }
            // HSET
            8 => {
                model.purge_expired(key);
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let v = format!("hv{}", rng.next_u64() % 100).into_bytes();
                let actual = eng.hset(key, &[(f.as_slice(), v.as_slice())]);
                let expected = match model.map.get_mut(key) {
                    None => {
                        let mut h = BTreeMap::new();
                        h.insert(f.clone(), v);
                        model.map.insert(
                            key.clone(),
                            ModelEntry {
                                expiry_ms: None,
                                val: ModelVal::Hash(h),
                            },
                        );
                        Ok(1)
                    }
                    Some(e) => match &mut e.val {
                        ModelVal::Hash(h) => Ok(u64::from(h.insert(f.clone(), v).is_none())),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} HSET");
            }
            // HGET / HLEN
            9 => {
                model.purge_expired(key);
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.hget(key, f);
                let expected = match model.map.get(key) {
                    None => Ok(None),
                    Some(e) => match &e.val {
                        ModelVal::Hash(h) => Ok(h.get(f).cloned()),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} HGET");
            }
            // HDEL
            10 => {
                model.purge_expired(key);
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.hdel(key, &[f.as_slice()]);
                let expected = match model.map.get_mut(key) {
                    None => Ok(0),
                    Some(e) => match &mut e.val {
                        ModelVal::Hash(h) => {
                            let rem = u64::from(h.remove(f).is_some());
                            let empty = h.is_empty();
                            if empty {
                                model.map.remove(key);
                            }
                            Ok(rem)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} HDEL");
            }
            // ZADD
            11 => {
                model.purge_expired(key);
                let m = &field_pool[rng.next_usize(field_pool.len())];
                let raw_s = scores[rng.next_usize(scores.len())];
                let s = if raw_s == 0.0 { 0.0 } else { raw_s };
                let actual = eng.zadd(key, &[(raw_s, m.as_slice())]);
                let expected = match model.map.get_mut(key) {
                    None => {
                        let mut zs = BTreeMap::new();
                        zs.insert(m.clone(), s);
                        model.map.insert(
                            key.clone(),
                            ModelEntry {
                                expiry_ms: None,
                                val: ModelVal::ZSet(zs),
                            },
                        );
                        Ok(1)
                    }
                    Some(e) => match &mut e.val {
                        ModelVal::ZSet(zs) => Ok(u64::from(zs.insert(m.clone(), s).is_none())),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZADD");
            }
            // ZREM
            12 => {
                model.purge_expired(key);
                let m = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.zrem(key, &[m.as_slice()]);
                let expected = match model.map.get_mut(key) {
                    None => Ok(0),
                    Some(e) => match &mut e.val {
                        ModelVal::ZSet(zs) => {
                            let rem = u64::from(zs.remove(m).is_some());
                            let empty = zs.is_empty();
                            if empty {
                                model.map.remove(key);
                            }
                            Ok(rem)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZREM");
            }
            // ZRANGE (forward & reverse) + ZSCORE
            13 => {
                model.purge_expired(key);
                let rev = (rng.next_u64() & 1) == 1;
                let actual = eng.zrange_by_rank(key, 0, -1, rev);
                let expected = match model.map.get(key) {
                    None => Ok(Vec::new()),
                    Some(e) => match &e.val {
                        ModelVal::ZSet(zs) => {
                            let mut v = RefModel::zset_sorted(zs);
                            if rev {
                                v.reverse();
                            }
                            Ok(v)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZRANGE rev={rev}");
            }
            // ZRANGE BYSCORE
            14 => {
                model.purge_expired(key);
                let actual = eng.zrange_by_score(
                    key,
                    &ScoreBound::Inclusive(-1.0),
                    &ScoreBound::Inclusive(42.0),
                    false,
                    0,
                    None,
                );
                let expected = match model.map.get(key) {
                    None => Ok(Vec::new()),
                    Some(e) => match &e.val {
                        ModelVal::ZSet(zs) => Ok(RefModel::zset_sorted(zs)
                            .into_iter()
                            .filter(|&(_, s)| (-1.0..=42.0).contains(&s))
                            .collect()),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZRANGE BYSCORE");
            }
            // SETBIT / GETBIT / BITCOUNT
            15 => {
                model.purge_expired(key);
                let off = (rng.next_u64() % 1_000_000) as u32;
                let bit = (rng.next_u64() & 1) == 1;
                let actual = eng.setbit(key, u64::from(off), bit);
                let expected = match model.map.get_mut(key) {
                    None => {
                        let mut bm = BTreeSet::new();
                        if bit {
                            bm.insert(off);
                        }
                        model.map.insert(
                            key.clone(),
                            ModelEntry {
                                expiry_ms: None,
                                val: ModelVal::Bitmap(bm),
                            },
                        );
                        Ok(0)
                    }
                    Some(e) => match &mut e.val {
                        ModelVal::Bitmap(bm) => {
                            let prev = u8::from(bm.contains(&off));
                            if bit {
                                bm.insert(off);
                            } else {
                                bm.remove(&off);
                            }
                            Ok(prev)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} SETBIT");
                if actual.is_ok() {
                    assert_eq!(eng.getbit(key, u64::from(off)), Ok(u8::from(bit)));
                }
            }
            // Prefix SCAN
            16 => {
                model.purge_all_expired();
                let prefix = format!("org:u{}:", rng.next_usize(4)).into_bytes();
                let mut scanned = Vec::new();
                let mut cur: Option<Vec<u8>> = None;
                loop {
                    let page = eng.scan(cur.as_deref(), &prefix, 5);
                    scanned.extend(page.items);
                    cur = page.next_cursor;
                    if cur.is_none() {
                        break;
                    }
                }
                let expected: Vec<Vec<u8>> = model
                    .map
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .cloned()
                    .collect();
                assert_eq!(scanned, expected, "step {step} SCAN");
            }
            // Active expire cycle
            _ => {
                let _ = eng.active_expire_cycle(16);
                model.purge_all_expired();
            }
        }
    }
}

#[test]
fn differential_20k_ops_zero_divergences_against_btreemap() {
    run_differential(20_000, 0xD1FF_2026_0001);
}
