//! Phase 2 differential model gate runner (`docs/PRD.md` §7 Phase 2):
//! executes `10^6` randomized mixed operations against `Engine` and an
//! independent `BTreeMap` reference oracle, asserting 0 divergences.

use expanse_kv_engine::{Engine, EngineError, ScoreBound, SetOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

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

fn zset_sorted(zs: &BTreeMap<Vec<u8>, f64>) -> Vec<(Vec<u8>, f64)> {
    let mut v: Vec<(Vec<u8>, f64)> = zs.iter().map(|(m, &s)| (m.clone(), s)).collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn cmd_stdout(prog: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{prog} exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() -> ExitCode {
    let total_ops: usize = 1_000_000;
    let eng = Engine::default();
    let mut now_ms = 10_000u64;
    eng.set_clock_ms(now_ms);
    let mut model: BTreeMap<Vec<u8>, ModelEntry> = BTreeMap::new();
    let mut rng = SplitMix64(0xD1FF_10E6_2026);

    let key_pool: Vec<Vec<u8>> = (0..128)
        .map(|i| format!("org:u{}:k{i:03}", i % 8).into_bytes())
        .collect();
    let field_pool: Vec<Vec<u8>> = (0..16).map(|i| format!("f{i:02}").into_bytes()).collect();
    let scores = [-10.5, -1.0, -0.0, 0.0, 1.5, 42.0, 100.25];

    for step in 0..total_ops {
        if (step & 127) == 127 {
            now_ms += 50;
            eng.set_clock_ms(now_ms);
        }
        let key = &key_pool[rng.next_usize(key_pool.len())];
        let expired = model
            .get(key)
            .and_then(|e| e.expiry_ms)
            .is_some_and(|exp| exp <= now_ms);
        if expired {
            model.remove(key);
        }

        match rng.next_usize(14) {
            0 | 1 => {
                let val = format!("v{}", rng.next_u64() % 1000).into_bytes();
                let expiry_ms = if (rng.next_u64() & 3) == 0 {
                    Some(now_ms + 100 + (rng.next_u64() % 300))
                } else {
                    None
                };
                let actual = eng
                    .set(
                        key,
                        &val,
                        SetOptions {
                            expiry_ms,
                            ..SetOptions::default()
                        },
                    )
                    .expect("set ok");
                assert!(actual.0, "step {step}");
                model.insert(
                    key.clone(),
                    ModelEntry {
                        expiry_ms,
                        val: ModelVal::String(val),
                    },
                );
            }
            2 | 3 => {
                let actual = eng.get(key);
                let expected = match model.get(key) {
                    None => Ok(None),
                    Some(e) => match &e.val {
                        ModelVal::String(v) => Ok(Some(v.clone())),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} GET");
            }
            4 => {
                let actual = eng.del(&[key.as_slice()]);
                let expected = u64::from(model.remove(key).is_some());
                assert_eq!(actual, expected, "step {step} DEL");
            }
            5 => {
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let v = format!("hv{}", rng.next_u64() % 100).into_bytes();
                let actual = eng.hset(key, &[(f.as_slice(), v.as_slice())]);
                let expected = match model.get_mut(key) {
                    None => {
                        let mut h = BTreeMap::new();
                        h.insert(f.clone(), v);
                        model.insert(
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
            6 => {
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.hget(key, f);
                let expected = match model.get(key) {
                    None => Ok(None),
                    Some(e) => match &e.val {
                        ModelVal::Hash(h) => Ok(h.get(f).cloned()),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} HGET");
            }
            7 => {
                let f = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.hdel(key, &[f.as_slice()]);
                let expected = match model.get_mut(key) {
                    None => Ok(0),
                    Some(e) => match &mut e.val {
                        ModelVal::Hash(h) => {
                            let rem = u64::from(h.remove(f).is_some());
                            if h.is_empty() {
                                model.remove(key);
                            }
                            Ok(rem)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} HDEL");
            }
            8 => {
                let m = &field_pool[rng.next_usize(field_pool.len())];
                let raw_s = scores[rng.next_usize(scores.len())];
                let s = if raw_s == 0.0 { 0.0 } else { raw_s };
                let actual = eng.zadd(key, &[(raw_s, m.as_slice())]);
                let expected = match model.get_mut(key) {
                    None => {
                        let mut zs = BTreeMap::new();
                        zs.insert(m.clone(), s);
                        model.insert(
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
            9 => {
                let m = &field_pool[rng.next_usize(field_pool.len())];
                let actual = eng.zrem(key, &[m.as_slice()]);
                let expected = match model.get_mut(key) {
                    None => Ok(0),
                    Some(e) => match &mut e.val {
                        ModelVal::ZSet(zs) => {
                            let rem = u64::from(zs.remove(m).is_some());
                            if zs.is_empty() {
                                model.remove(key);
                            }
                            Ok(rem)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZREM");
            }
            10 => {
                let rev = (rng.next_u64() & 1) == 1;
                let actual = eng.zrange_by_rank(key, 0, -1, rev);
                let expected = match model.get(key) {
                    None => Ok(Vec::new()),
                    Some(e) => match &e.val {
                        ModelVal::ZSet(zs) => {
                            let mut v = zset_sorted(zs);
                            if rev {
                                v.reverse();
                            }
                            Ok(v)
                        }
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZRANGE");
            }
            11 => {
                let actual = eng.zrange_by_score(
                    key,
                    &ScoreBound::Inclusive(-1.0),
                    &ScoreBound::Inclusive(42.0),
                    false,
                    0,
                    None,
                );
                let expected = match model.get(key) {
                    None => Ok(Vec::new()),
                    Some(e) => match &e.val {
                        ModelVal::ZSet(zs) => Ok(zset_sorted(zs)
                            .into_iter()
                            .filter(|&(_, s)| (-1.0..=42.0).contains(&s))
                            .collect()),
                        _ => Err(EngineError::WrongType),
                    },
                };
                assert_eq!(actual, expected, "step {step} ZRANGE BYSCORE");
            }
            12 => {
                let off = (rng.next_u64() % 1_000_000) as u32;
                let bit = (rng.next_u64() & 1) == 1;
                let actual = eng.setbit(key, u64::from(off), bit);
                let expected = match model.get_mut(key) {
                    None => {
                        let mut bm = BTreeSet::new();
                        if bit {
                            bm.insert(off);
                        }
                        model.insert(
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
            }
            _ => {
                if (step & 15) == 0 {
                    model.retain(|_, e| !e.expiry_ms.is_some_and(|exp| exp <= now_ms));
                    let prefix = format!("org:u{}:", rng.next_usize(8)).into_bytes();
                    let mut scanned = Vec::new();
                    let mut cur: Option<Vec<u8>> = None;
                    loop {
                        let page = eng.scan(cur.as_deref(), &prefix, 8);
                        scanned.extend(page.items);
                        cur = page.next_cursor;
                        if cur.is_none() {
                            break;
                        }
                    }
                    let expected: Vec<Vec<u8>> = model
                        .keys()
                        .filter(|k| k.starts_with(&prefix))
                        .cloned()
                        .collect();
                    assert_eq!(scanned, expected, "step {step} SCAN");
                }
            }
        }
    }

    let git_commit = cmd_stdout("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let os_kernel = cmd_stdout("uname", &["-srm"]).unwrap_or_else(|_| "unknown-os".to_string());
    let prov = format!("measured: {os_kernel}, {git_commit}");

    let out_dir = Path::new("results/phase2");
    if let Err(e) = fs::create_dir_all(out_dir) {
        eprintln!("FAIL: create_dir_all: {e}");
        return ExitCode::FAILURE;
    }
    let json = format!(
        "{{\n  \"provenance\": \"{prov}\",\n  \"operations\": {total_ops},\n  \"divergences\": 0,\n  \"verdict\": \"PASS\"\n}}\n"
    );
    let mut md = String::new();
    let res = (|| -> Result<(), core::fmt::Error> {
        writeln!(md, "# Phase 2 Differential Model Gate\n")?;
        writeln!(md, "- **PRD gate:** `docs/PRD.md` §7 Phase 2")?;
        writeln!(
            md,
            "- **Raw artifact:** [`results/phase2/differential_gate.json`](differential_gate.json)"
        )?;
        writeln!(
            md,
            "- **Result:** `{total_ops}` randomized mixed operations (Strings, Hashes, ZSets, Bitmaps, TTL, `SCAN`), `0` divergences `({prov})` -> **PASS**"
        )?;
        Ok(())
    })();
    if let Err(e) = res {
        eprintln!("FAIL: format md: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(out_dir.join("differential_gate.json"), json) {
        eprintln!("FAIL: write json: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(out_dir.join("differential_gate.md"), md) {
        eprintln!("FAIL: write md: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("PASS: {total_ops} differential operations, 0 divergences ({prov})");
    ExitCode::SUCCESS
}
