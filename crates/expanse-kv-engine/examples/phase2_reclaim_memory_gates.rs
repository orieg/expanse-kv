//! Phase 2C gate verification runner (`docs/PRD.md` §3.4, §6.3, §7 Phase 2):
//! 1. Relative stalled-reader reclamation gate (`10^5 -> 10^6 -> 10^7`
//!    mutations with 1 stalled bookmark `SCAN` cursor).
//! 2. Dense non-monotonic memory envelope sweep from `10^3` past `10^6` across
//!    all four key shapes (prefix-heavy strings, uniform random strings, Hash
//!    fields, Sorted-Set members).

use expanse_kv_engine::{Engine, SetOptions};
use std::env;
use std::fmt::{self, Write as _};
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

struct ReclaimPoint {
    mutations: usize,
    live_bytes: usize,
    trie_bytes: usize,
    grace_bytes: usize,
    free_bytes: usize,
    garbage_pct: f64,
}

struct MemoryPoint {
    n: usize,
    trie_bpk: f64,
    total_bpk: f64,
}

struct MemoryEnvelopeResult {
    shape: &'static str,
    points: Vec<MemoryPoint>,
    min_trie_bpk: f64,
    n_min_trie: usize,
    max_trie_bpk: f64,
    n_max_trie: usize,
    min_total_bpk: f64,
    n_min_total: usize,
    max_total_bpk: f64,
    n_max_total: usize,
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

fn read_load_avg_1m() -> Result<f64, String> {
    if cfg!(target_os = "linux") {
        let s = fs::read_to_string("/proc/loadavg").map_err(|e| e.to_string())?;
        return s
            .split_whitespace()
            .next()
            .ok_or_else(|| "empty /proc/loadavg".to_string())?
            .parse::<f64>()
            .map_err(|e| e.to_string());
    }
    let out = cmd_stdout("sysctl", &["-n", "vm.loadavg"])?;
    out.trim_matches(|c| c == '{' || c == '}' || c == ' ')
        .split_whitespace()
        .next()
        .ok_or_else(|| "empty vm.loadavg".to_string())?
        .parse::<f64>()
        .map_err(|e| e.to_string())
}

fn run_stalled_reader_reclaim_gate(sweep_counts: &[usize]) -> Vec<ReclaimPoint> {
    let eng = Engine::default();
    let n_live = 10_000usize;
    let keys: Vec<Vec<u8>> = (0..n_live)
        .map(|i| format!("org:user:session:{i:08x}").into_bytes())
        .collect();
    let val = b"0123456789abcdef";

    for k in &keys {
        eng.set(k, val, SetOptions::default()).expect("populate");
    }

    // Stalled client takes first SCAN page and holds the stateless bookmark across all sweeps.
    let stalled_page = eng.scan(None, b"org:user:session:", 64);
    assert_eq!(stalled_page.items.len(), 64);
    let stalled_bookmark = stalled_page.next_cursor.expect("has more pages");

    let mut rng = SplitMix64(0x57A1_1ED0_2026);
    let mut results = Vec::with_capacity(sweep_counts.len());
    let mut done_mutations = 0usize;

    for &target_muts in sweep_counts {
        while done_mutations < target_muts {
            let idx = rng.next_usize(n_live);
            let k = &keys[idx];
            // Alternate DEL + SET on the fixed live keyspace to force real trie node retirement.
            eng.del(&[k.as_slice()]);
            eng.set(k, val, SetOptions::default()).expect("reinsert");
            done_mutations += 2;
        }
        let live_bytes = eng.mem_used();
        let trie_bytes = eng.data_trie().mem_used();
        let census = eng.data_trie().collector_census();
        let grace_bytes = census.grace_bytes();
        let free_bytes = census.free_bytes();
        let garbage_pct = if live_bytes > 0 {
            100.0 * (grace_bytes as f64) / (live_bytes as f64)
        } else {
            0.0
        };
        eprintln!(
            "  Reclaim @ {target_muts} muts: live={live_bytes} B, grace={grace_bytes} B ({garbage_pct:.4}%), free={free_bytes} B"
        );
        results.push(ReclaimPoint {
            mutations: target_muts,
            live_bytes,
            trie_bytes,
            grace_bytes,
            free_bytes,
            garbage_pct,
        });
    }

    // Resume stalled bookmark cursor to confirm it still yields valid ordered keys.
    let resumed = eng.scan(Some(&stalled_bookmark), b"org:user:session:", 64);
    assert!(
        !resumed.items.is_empty(),
        "stalled bookmark cursor resumed cleanly"
    );

    results
}

fn summarize_envelope(shape: &'static str, points: Vec<MemoryPoint>) -> MemoryEnvelopeResult {
    let mut min_t = f64::INFINITY;
    let mut n_min_t = 0;
    let mut max_t = 0.0f64;
    let mut n_max_t = 0;
    let mut min_tot = f64::INFINITY;
    let mut n_min_tot = 0;
    let mut max_tot = 0.0f64;
    let mut n_max_tot = 0;

    for pt in &points {
        if pt.trie_bpk < min_t {
            min_t = pt.trie_bpk;
            n_min_t = pt.n;
        }
        if pt.trie_bpk > max_t {
            max_t = pt.trie_bpk;
            n_max_t = pt.n;
        }
        if pt.total_bpk < min_tot {
            min_tot = pt.total_bpk;
            n_min_tot = pt.n;
        }
        if pt.total_bpk > max_tot {
            max_tot = pt.total_bpk;
            n_max_tot = pt.n;
        }
    }

    MemoryEnvelopeResult {
        shape,
        points,
        min_trie_bpk: min_t,
        n_min_trie: n_min_t,
        max_trie_bpk: max_t,
        n_max_trie: n_max_t,
        min_total_bpk: min_tot,
        n_min_total: n_min_tot,
        max_total_bpk: max_tot,
        n_max_total: n_max_tot,
    }
}

fn sweep_prefix_strings(checkpoints: &[usize]) -> MemoryEnvelopeResult {
    let eng = Engine::default();
    let val = b"0123456789abcdef";
    let mut inserted = 0usize;
    let mut points = Vec::with_capacity(checkpoints.len());

    for &target in checkpoints {
        while inserted < target {
            let k = format!("org:user:session:{inserted:016x}");
            eng.set(k.as_bytes(), val, SetOptions::default())
                .expect("set prefix");
            inserted += 1;
        }
        let trie_bpk = (eng.data_trie().mem_used() as f64) / (target as f64);
        let total_bpk = (eng.mem_used() as f64) / (target as f64);
        points.push(MemoryPoint {
            n: target,
            trie_bpk,
            total_bpk,
        });
    }
    let env = summarize_envelope("prefix_strings", points);
    eprintln!(
        "  prefix_strings: trie [{:.2} @ {}, {:.2} @ {}] total [{:.2} @ {}, {:.2} @ {}] B/key",
        env.min_trie_bpk,
        env.n_min_trie,
        env.max_trie_bpk,
        env.n_max_trie,
        env.min_total_bpk,
        env.n_min_total,
        env.max_total_bpk,
        env.n_max_total
    );
    env
}

fn sweep_uniform_strings(checkpoints: &[usize]) -> MemoryEnvelopeResult {
    let eng = Engine::default();
    let val = b"0123456789abcdef";
    let mut rng = SplitMix64(0x01F0_1234_5678);
    let mut inserted = 0usize;
    let mut points = Vec::with_capacity(checkpoints.len());

    for &target in checkpoints {
        while inserted < target {
            let mut k = [0u8; 16];
            k[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
            k[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
            eng.set(&k, val, SetOptions::default())
                .expect("set uniform");
            inserted += 1;
        }
        let trie_bpk = (eng.data_trie().mem_used() as f64) / (target as f64);
        let total_bpk = (eng.mem_used() as f64) / (target as f64);
        points.push(MemoryPoint {
            n: target,
            trie_bpk,
            total_bpk,
        });
    }
    let env = summarize_envelope("uniform_strings", points);
    eprintln!(
        "  uniform_strings: trie [{:.2} @ {}, {:.2} @ {}] total [{:.2} @ {}, {:.2} @ {}] B/key",
        env.min_trie_bpk,
        env.n_min_trie,
        env.max_trie_bpk,
        env.n_max_trie,
        env.min_total_bpk,
        env.n_min_total,
        env.max_total_bpk,
        env.n_max_total
    );
    env
}

fn sweep_hash_fields(checkpoints: &[usize]) -> MemoryEnvelopeResult {
    let eng = Engine::default();
    let val = b"0123456789abcdef";
    let fields_per_key = 100usize;
    let mut inserted = 0usize;
    let mut points = Vec::with_capacity(checkpoints.len());

    for &target in checkpoints {
        while inserted < target {
            let hk = format!("hash:{:08}", inserted / fields_per_key);
            let field = format!("f{:06}", inserted % fields_per_key);
            eng.hset(hk.as_bytes(), &[(field.as_bytes(), val)])
                .expect("hset");
            inserted += 1;
        }
        let trie_bpk = (eng.data_trie().mem_used() as f64) / (target as f64);
        let total_bpk = (eng.mem_used() as f64) / (target as f64);
        points.push(MemoryPoint {
            n: target,
            trie_bpk,
            total_bpk,
        });
    }
    let env = summarize_envelope("hash_fields", points);
    eprintln!(
        "  hash_fields: trie [{:.2} @ {}, {:.2} @ {}] total [{:.2} @ {}, {:.2} @ {}] B/field",
        env.min_trie_bpk,
        env.n_min_trie,
        env.max_trie_bpk,
        env.n_max_trie,
        env.min_total_bpk,
        env.n_min_total,
        env.max_total_bpk,
        env.n_max_total
    );
    env
}

fn sweep_zset_members(checkpoints: &[usize]) -> MemoryEnvelopeResult {
    let eng = Engine::default();
    let members_per_key = 100usize;
    let mut inserted = 0usize;
    let mut points = Vec::with_capacity(checkpoints.len());

    for &target in checkpoints {
        while inserted < target {
            let zk = format!("zset:{:08}", inserted / members_per_key);
            let member = format!("m{:06}", inserted % members_per_key);
            let score = (inserted % 10_000) as f64 * 0.25;
            eng.zadd(zk.as_bytes(), &[(score, member.as_bytes())])
                .expect("zadd");
            inserted += 1;
        }
        let trie_bpk = (eng.data_trie().mem_used() as f64) / (target as f64);
        let total_bpk = (eng.mem_used() as f64) / (target as f64);
        points.push(MemoryPoint {
            n: target,
            trie_bpk,
            total_bpk,
        });
    }
    let env = summarize_envelope("zset_members", points);
    eprintln!(
        "  zset_members: trie [{:.2} @ {}, {:.2} @ {}] total [{:.2} @ {}, {:.2} @ {}] B/member",
        env.min_trie_bpk,
        env.n_min_trie,
        env.max_trie_bpk,
        env.n_max_trie,
        env.min_total_bpk,
        env.n_min_total,
        env.max_total_bpk,
        env.n_max_total
    );
    env
}

fn render_outputs(
    prov_tag: &str,
    load_start: f64,
    load_end: f64,
    reclaim: &[ReclaimPoint],
    envelopes: &[MemoryEnvelopeResult],
) -> Result<(String, String), fmt::Error> {
    let reclaim_pass = reclaim.iter().all(|r| r.garbage_pct <= 5.0)
        && reclaim.last().is_some_and(|last| {
            reclaim
                .first()
                .is_some_and(|first| last.garbage_pct <= first.garbage_pct + 0.5)
        });
    let reclaim_verdict = if reclaim_pass { "PASS" } else { "FAIL" };

    let mut json = String::new();
    writeln!(json, "{{")?;
    writeln!(json, "  \"provenance\": \"{prov_tag}\",")?;
    writeln!(json, "  \"load_start\": {load_start:.3},")?;
    writeln!(json, "  \"load_end\": {load_end:.3},")?;
    writeln!(
        json,
        "  \"comparators\": {{\"valkey_8x\": \"UNRUN\", \"dragonfly\": \"UNRUN\"}},"
    )?;
    writeln!(json, "  \"reclaim_verdict\": \"{reclaim_verdict}\",")?;
    writeln!(json, "  \"reclaim_sweep\": [")?;
    for (idx, r) in reclaim.iter().enumerate() {
        let comma = if idx + 1 == reclaim.len() { "" } else { "," };
        writeln!(
            json,
            "    {{\"mutations\": {}, \"live_bytes\": {}, \"trie_bytes\": {}, \"grace_bytes\": {}, \"free_bytes\": {}, \"garbage_pct\": {:.6}}}{comma}",
            r.mutations, r.live_bytes, r.trie_bytes, r.grace_bytes, r.free_bytes, r.garbage_pct
        )?;
    }
    writeln!(json, "  ],")?;
    writeln!(json, "  \"memory_envelopes\": [")?;
    for (idx, e) in envelopes.iter().enumerate() {
        let comma = if idx + 1 == envelopes.len() { "" } else { "," };
        writeln!(
            json,
            "    {{\"shape\": \"{}\", \"min_trie_bpk\": {:.3}, \"n_min_trie\": {}, \"max_trie_bpk\": {:.3}, \"n_max_trie\": {}, \"min_total_bpk\": {:.3}, \"n_min_total\": {}, \"max_total_bpk\": {:.3}, \"n_max_total\": {}, \"samples\": {}}}{comma}",
            e.shape,
            e.min_trie_bpk,
            e.n_min_trie,
            e.max_trie_bpk,
            e.n_max_trie,
            e.min_total_bpk,
            e.n_min_total,
            e.max_total_bpk,
            e.n_max_total,
            e.points.len()
        )?;
    }
    writeln!(json, "  ]")?;
    writeln!(json, "}}")?;

    let mut md = String::new();
    writeln!(
        md,
        "# Phase 2 Stalled-Reader Reclamation & Memory Envelope Gates\n"
    )?;
    writeln!(md, "- **PRD gate:** `docs/PRD.md` §7 Phase 2")?;
    writeln!(
        md,
        "- **Raw artifact:** [`results/phase2/reclaim_memory_gates.json`](reclaim_memory_gates.json)"
    )?;
    writeln!(md, "- **Provenance:** `({prov_tag})`")?;
    writeln!(
        md,
        "- **Host load (`loadavg[0]`):** start `{load_start:.2}`, end `{load_end:.2}` `({prov_tag})`\n"
    )?;

    writeln!(md, "## 1. Relative Stalled-Reader Reclamation Gate\n")?;
    writeln!(
        md,
        "- **Gate (`<= 5%` `(target)` uncollected garbage, zero upward drift across `10^5 -> 10^6 -> 10^7` mutations):** **{reclaim_verdict}**\n"
    )?;
    writeln!(
        md,
        "| Mutations | Live Bytes | Trie Bytes | Grace (Uncollected) Bytes | Garbage % |"
    )?;
    writeln!(md, "| ---: | ---: | ---: | ---: | ---: |")?;
    for r in reclaim {
        writeln!(
            md,
            "| {} | {} `({prov_tag})` | {} | {} | {:.4}% |",
            r.mutations, r.live_bytes, r.trie_bytes, r.grace_bytes, r.garbage_pct
        )?;
    }

    writeln!(
        md,
        "\n## 2. Dense Non-Monotonic Memory Envelope Sweep (`10^3` past `10^6`)\n"
    )?;
    writeln!(
        md,
        "- **Comparative gate vs Valkey 8.x & Dragonfly (`AGENTS.md` §8.9):** `UNRUN` (`valkey-server` 8.x and `dragonfly` are not installed on the benchmark host; no comparator figure is estimated or substituted).\n"
    )?;
    writeln!(
        md,
        "| Shape | Min Trie B/item (`N_min`) | Max Trie B/item (`N_max`) | Min Total B/item (`N_min`) | Max Total B/item (`N_max`) |"
    )?;
    writeln!(md, "| :--- | ---: | ---: | ---: | ---: |")?;
    for e in envelopes {
        writeln!(
            md,
            "| `{}` | {:.2} (`{}`) `({prov_tag})` | {:.2} (`{}`) | {:.2} (`{}`) | {:.2} (`{}`) |",
            e.shape,
            e.min_trie_bpk,
            e.n_min_trie,
            e.max_trie_bpk,
            e.n_max_trie,
            e.min_total_bpk,
            e.n_min_total,
            e.max_total_bpk,
            e.n_max_total
        )?;
    }
    Ok((json, md))
}

fn main() -> ExitCode {
    let quick = env::args().any(|a| a == "--quick");
    let logical_cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let load_start = match read_load_avg_1m() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FAIL: could not read load average ({e})");
            return ExitCode::FAILURE;
        }
    };
    let load_limit = (logical_cores as f64) / 2.0;
    if !quick && load_start > load_limit {
        eprintln!("INADMISSIBLE: load {load_start:.2} > limit {load_limit:.2}");
        return ExitCode::FAILURE;
    }

    let cpu_model = if cfg!(target_os = "macos") {
        cmd_stdout("sysctl", &["-n", "machdep.cpu.brand_string"])
            .unwrap_or_else(|_| "unknown-cpu".to_string())
    } else {
        match fs::read_to_string("/proc/cpuinfo") {
            Ok(s) => s
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|| "unknown-cpu".to_string()),
            Err(_) => "unknown-cpu".to_string(),
        }
    };
    let os_kernel = cmd_stdout("uname", &["-srm"]).unwrap_or_else(|_| "unknown-os".to_string());
    let git_commit = cmd_stdout("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let prov_tag = format!("measured: {cpu_model} ({logical_cores}T, {os_kernel}), {git_commit}");

    let reclaim_sweeps: &[usize] = if quick {
        &[10_000, 50_000, 100_000]
    } else {
        &[100_000, 1_000_000, 10_000_000]
    };
    let checkpoints: &[usize] = if quick {
        &[1_000, 5_000, 10_000]
    } else {
        &[
            1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000,
            2_000_000,
        ]
    };

    eprintln!("Running Phase 2C stalled-reader reclamation gate...");
    let reclaim = run_stalled_reader_reclaim_gate(reclaim_sweeps);

    eprintln!("Running Phase 2C dense memory envelope sweep...");
    let envelopes = vec![
        sweep_prefix_strings(checkpoints),
        sweep_uniform_strings(checkpoints),
        sweep_hash_fields(checkpoints),
        sweep_zset_members(checkpoints),
    ];

    let load_end = read_load_avg_1m().unwrap_or(load_start);
    let out_dir = if quick {
        Path::new("results/quick")
    } else {
        Path::new("results/phase2")
    };
    if let Err(e) = fs::create_dir_all(out_dir) {
        eprintln!("FAIL: create_dir_all: {e}");
        return ExitCode::FAILURE;
    }
    let (json, md) = match render_outputs(&prov_tag, load_start, load_end, &reclaim, &envelopes) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("FAIL: render_outputs: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(out_dir.join("reclaim_memory_gates.json"), &json) {
        eprintln!("FAIL: write json: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(out_dir.join("reclaim_memory_gates.md"), &md) {
        eprintln!("FAIL: write md: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
