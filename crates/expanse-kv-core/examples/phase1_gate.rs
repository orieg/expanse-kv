//! Phase 1 gate verification runner (`docs/PRD.md` §7 Phase 1):
//! 1. `10^7` adversarial/mutated fuzz inputs through `parse_frame_with_limits`
//!    and `parse_command_into` (0 panics / 0 UB).
//! 2. Single-core pipelined `GET`/`SET` throughput measurement (`>= 2.0 GB/s`
//!    per core, BCa 95% CI lower bound, `AGENTS.md` §8.4, §8.11).

use core::hint::black_box;
use expanse_kv_core::resp::{
    CommandStatus, ParseLimits, ParseStatus, parse_command_into, parse_frame_with_limits,
};
use std::env;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

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

fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * x.abs());
    let poly = t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    let pdf = (-0.5 * x * x).exp() / (2.0 * core::f64::consts::PI).sqrt();
    let tail = pdf * poly;
    if x >= 0.0 { 1.0 - tail } else { tail }
}

fn normal_inv_cdf(p: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    if p < 0.02425 {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - 0.02425 {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct BcaCi {
    mean: f64,
    ci_low: f64,
    ci_high: f64,
}

fn bca_ci_95(samples: &[f64], resamples: usize, seed: u64) -> BcaCi {
    let n = samples.len();
    let mean = samples.iter().sum::<f64>() / (n as f64);
    if samples.iter().all(|&x| (x - mean).abs() == 0.0) {
        return BcaCi {
            mean,
            ci_low: mean,
            ci_high: mean,
        };
    }
    let mut rng = SplitMix64(seed);
    let mut boot = Vec::with_capacity(resamples);
    let mut less = 0usize;
    for _ in 0..resamples {
        let mut s = 0.0;
        for _ in 0..n {
            s += samples[rng.next_usize(n)];
        }
        let bm = s / (n as f64);
        if bm < mean {
            less += 1;
        }
        boot.push(bm);
    }
    boot.sort_by(f64::total_cmp);
    let prop = (less as f64 / resamples as f64).clamp(
        1.0 / (resamples as f64 + 1.0),
        1.0 - 1.0 / (resamples as f64 + 1.0),
    );
    let z0 = normal_inv_cdf(prop);
    let total: f64 = samples.iter().sum();
    let jack: Vec<f64> = samples
        .iter()
        .map(|&x| (total - x) / ((n - 1) as f64))
        .collect();
    let jm = jack.iter().sum::<f64>() / (n as f64);
    let (mut num, mut den) = (0.0, 0.0);
    for &v in &jack {
        let d = jm - v;
        num += d * d * d;
        den += d * d;
    }
    let accel = if den > 0.0 {
        num / (6.0 * den.powf(1.5))
    } else {
        0.0
    };
    let adj = |z: f64| {
        let denom = 1.0 - accel * (z0 + z);
        if denom.abs() < 1e-9 {
            return if z < 0.0 { 0.025 } else { 0.975 };
        }
        normal_cdf(z0 + (z0 + z) / denom)
    };
    let i_lo = ((adj(normal_inv_cdf(0.025)).clamp(0.0, 1.0) * (resamples - 1) as f64).floor()
        as usize)
        .min(resamples - 1);
    let i_hi = ((adj(normal_inv_cdf(0.975)).clamp(0.0, 1.0) * (resamples - 1) as f64).ceil()
        as usize)
        .min(resamples - 1);
    BcaCi {
        mean,
        ci_low: boot[i_lo].min(mean),
        ci_high: boot[i_hi].max(mean),
    }
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

fn build_pipeline(kind: &str, target_bytes: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(target_bytes + 128);
    let mut i: u64 = 0;
    while buf.len() < target_bytes {
        let key = format!("org:user:session:{i:08x}");
        match kind {
            "get" => {
                let frame = format!("*2\r\n$3\r\nGET\r\n${}\r\n{key}\r\n", key.len());
                buf.extend_from_slice(frame.as_bytes());
            }
            "set" => {
                let val = format!("val:{i:028x}");
                let frame = format!(
                    "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{val}\r\n",
                    key.len(),
                    val.len()
                );
                buf.extend_from_slice(frame.as_bytes());
            }
            _ => {
                if (i & 1) == 0 {
                    let frame = format!("*2\r\n$3\r\nGET\r\n${}\r\n{key}\r\n", key.len());
                    buf.extend_from_slice(frame.as_bytes());
                } else {
                    let val = format!("val:{i:028x}");
                    let frame = format!(
                        "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{val}\r\n",
                        key.len(),
                        val.len()
                    );
                    buf.extend_from_slice(frame.as_bytes());
                }
            }
        }
        i += 1;
    }
    buf
}

fn bench_pipeline(buf: &[u8], passes_per_round: usize, warmup: usize, rounds: usize) -> BcaCi {
    let limits = ParseLimits::default();
    let mut gbps = Vec::with_capacity(rounds);
    let mut args: Vec<&[u8]> = Vec::with_capacity(8);

    for r in 0..(warmup + rounds) {
        let t0 = Instant::now();
        let mut checksum: usize = 0;
        let mut frames: usize = 0;
        for _ in 0..passes_per_round {
            let mut off = 0;
            while off < buf.len() {
                match parse_command_into(&buf[off..], &mut args, limits) {
                    Ok(CommandStatus::Complete { consumed }) => {
                        checksum = checksum.wrapping_add(args.len());
                        if let Some(a0) = args.first() {
                            checksum = checksum.wrapping_add(a0.len());
                        }
                        frames += 1;
                        off += consumed;
                    }
                    other => panic!("unexpected parse status in valid pipeline: {other:?}"),
                }
            }
        }
        black_box((checksum, frames));
        let secs = t0.elapsed().as_secs_f64();
        if r >= warmup {
            let total_bytes = (buf.len() * passes_per_round) as f64;
            gbps.push(total_bytes / secs / 1e9);
        }
    }
    bca_ci_95(&gbps, 2_000, 0x1A5E_0001)
}

fn run_fuzz_sweep(total_inputs: usize) -> (usize, usize, usize) {
    let mut rng = SplitMix64(0xF022_10E7_2026);
    let alphabet = b"+-:$*_#,(!=%~>0123456789-\r\n tf\"' \x00\xFF";
    let seeds: [&[u8]; 6] = [
        b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n",
        b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
        b"%2\r\n+a\r\n:1\r\n+b\r\n#t\r\n",
        b"=8\r\ntxt:data\r\n",
        b"PING\r\n",
        b"SET \"a b\" 'c'\r\n",
    ];
    let limits = ParseLimits {
        max_depth: 16,
        max_bulk_len: 1024,
        max_collection_len: 64,
        max_inline_len: 256,
    };
    let mut buf = Vec::with_capacity(64);
    let mut frame_ok = 0usize;
    let mut cmd_ok = 0usize;

    for i in 0..total_inputs {
        buf.clear();
        match i % 3 {
            0 => {
                // Grammar-alphabet random sequence.
                let len = rng.next_usize(48);
                for _ in 0..len {
                    buf.push(alphabet[rng.next_usize(alphabet.len())]);
                }
            }
            1 => {
                // Mutated valid seed frame.
                let seed = seeds[rng.next_usize(seeds.len())];
                buf.extend_from_slice(seed);
                let mutations = 1 + rng.next_usize(4);
                for _ in 0..mutations {
                    if !buf.is_empty() {
                        let pos = rng.next_usize(buf.len());
                        buf[pos] = alphabet[rng.next_usize(alphabet.len())];
                    }
                }
            }
            _ => {
                // Arbitrary uniform bytes.
                let len = rng.next_usize(32);
                for _ in 0..len {
                    buf.push((rng.next_u64() & 0xFF) as u8);
                }
            }
        }
        if let Ok(ParseStatus::Complete { .. }) = parse_frame_with_limits(&buf, limits) {
            frame_ok += 1;
        }
        let mut args = Vec::new();
        if let Ok(CommandStatus::Complete { .. }) = parse_command_into(&buf, &mut args, limits) {
            cmd_ok += 1;
        }
    }
    (total_inputs, frame_ok, cmd_ok)
}

struct GateSummary<'a> {
    prov_tag: &'a str,
    load_start: f64,
    load_end: f64,
    load_limit: f64,
    fuzz_inputs: usize,
    frame_ok: usize,
    cmd_ok: usize,
    mixed: BcaCi,
    get_only: BcaCi,
    set_only: BcaCi,
}

fn render_report(s: &GateSummary<'_>) -> Result<(String, String), fmt::Error> {
    let mut json = String::new();
    writeln!(json, "{{")?;
    writeln!(json, "  \"provenance\": \"{}\",", s.prov_tag)?;
    writeln!(json, "  \"load_start\": {:.3},", s.load_start)?;
    writeln!(json, "  \"load_end\": {:.3},", s.load_end)?;
    writeln!(json, "  \"load_limit\": {:.2},", s.load_limit)?;
    writeln!(json, "  \"fuzz_inputs\": {},", s.fuzz_inputs)?;
    writeln!(json, "  \"fuzz_panics\": 0,")?;
    writeln!(json, "  \"fuzz_frame_complete\": {},", s.frame_ok)?;
    writeln!(json, "  \"fuzz_cmd_complete\": {},", s.cmd_ok)?;
    writeln!(
        json,
        "  \"mixed_get_set_gbps\": {{\"mean\": {:.4}, \"ci_low\": {:.4}, \"ci_high\": {:.4}}},",
        s.mixed.mean, s.mixed.ci_low, s.mixed.ci_high
    )?;
    writeln!(
        json,
        "  \"get_only_gbps\": {{\"mean\": {:.4}, \"ci_low\": {:.4}, \"ci_high\": {:.4}}},",
        s.get_only.mean, s.get_only.ci_low, s.get_only.ci_high
    )?;
    writeln!(
        json,
        "  \"set_only_gbps\": {{\"mean\": {:.4}, \"ci_low\": {:.4}, \"ci_high\": {:.4}}}",
        s.set_only.mean, s.set_only.ci_low, s.set_only.ci_high
    )?;
    writeln!(json, "}}")?;

    let fuzz_verdict = if s.fuzz_inputs >= 10_000_000 {
        "PASS"
    } else {
        "QUICK"
    };
    let tput_verdict = if s.mixed.ci_low >= 2.0 {
        "PASS"
    } else if s.mixed.mean >= 2.0 {
        "INTERMEDIATE"
    } else {
        "FAIL"
    };

    let mut md = String::new();
    writeln!(md, "# Phase 1 Gate Verification (`expanse-kv-core`)\n")?;
    writeln!(md, "- **PRD gate:** `docs/PRD.md` §7 Phase 1")?;
    writeln!(
        md,
        "- **Raw artifact:** [`results/phase1/resp_gate.json`](resp_gate.json)"
    )?;
    writeln!(md, "- **Provenance:** `({})`", s.prov_tag)?;
    writeln!(
        md,
        "- **Host load (`loadavg[0]`):** start `{:.2}`, end `{:.2}` (limit `{:.2}`) `({})`\n",
        s.load_start, s.load_end, s.load_limit, s.prov_tag
    )?;
    writeln!(md, "## Gate Verdicts\n")?;
    writeln!(
        md,
        "- **Fuzz stability (`10^7` inputs, 0 panics / 0 UB under `#![forbid(unsafe_code)]`):** `{}` inputs, `0` panics (`{}` complete frames, `{}` complete commands) `({})` -> **{fuzz_verdict}**",
        s.fuzz_inputs, s.frame_ok, s.cmd_ok, s.prov_tag
    )?;
    writeln!(
        md,
        "- **Pipelined `GET`/`SET` throughput (floor `>= 2.0 GB/s` `(target)`, BCa 95% CI lower bound):** `{:.2} GB/s` (BCa 95% CI `[{:.2}, {:.2}] GB/s`) `({})` -> **{tput_verdict}**",
        s.mixed.mean, s.mixed.ci_low, s.mixed.ci_high, s.prov_tag
    )?;
    writeln!(
        md,
        "- **Pipelined `GET`-only throughput:** `{:.2} GB/s` (BCa 95% CI `[{:.2}, {:.2}] GB/s`) `({})`",
        s.get_only.mean, s.get_only.ci_low, s.get_only.ci_high, s.prov_tag
    )?;
    writeln!(
        md,
        "- **Pipelined `SET`-only throughput:** `{:.2} GB/s` (BCa 95% CI `[{:.2}, {:.2}] GB/s`) `({})`",
        s.set_only.mean, s.set_only.ci_low, s.set_only.ci_high, s.prov_tag
    )?;
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

    let (fuzz_n, target_bytes, passes, rounds) = if quick {
        (100_000usize, 256 * 1024, 4usize, 5usize)
    } else {
        (10_000_000usize, 4 * 1024 * 1024, 16usize, 20usize)
    };

    eprintln!("Running {fuzz_n} fuzz inputs...");
    let (n_run, frame_ok, cmd_ok) = run_fuzz_sweep(fuzz_n);
    eprintln!("Fuzz done: {n_run} inputs, 0 panics ({frame_ok} frames, {cmd_ok} cmds)");

    let mixed_buf = build_pipeline("mixed", target_bytes);
    let get_buf = build_pipeline("get", target_bytes);
    let set_buf = build_pipeline("set", target_bytes);

    let mixed = bench_pipeline(&mixed_buf, passes, 2, rounds);
    let get_only = bench_pipeline(&get_buf, passes, 2, rounds);
    let set_only = bench_pipeline(&set_buf, passes, 2, rounds);
    let load_end = read_load_avg_1m().unwrap_or(load_start);

    eprintln!(
        "Mixed GET/SET: {:.2} GB/s [CI {:.2}, {:.2}] | GET: {:.2} GB/s | SET: {:.2} GB/s",
        mixed.mean, mixed.ci_low, mixed.ci_high, get_only.mean, set_only.mean
    );

    let out_dir = if quick {
        Path::new("results/quick")
    } else {
        Path::new("results/phase1")
    };
    if let Err(e) = fs::create_dir_all(out_dir) {
        eprintln!("FAIL: create_dir_all: {e}");
        return ExitCode::FAILURE;
    }
    let summary = GateSummary {
        prov_tag: &prov_tag,
        load_start,
        load_end,
        load_limit,
        fuzz_inputs: n_run,
        frame_ok,
        cmd_ok,
        mixed,
        get_only,
        set_only,
    };
    let (json, md) = match render_report(&summary) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("FAIL: render_report: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(out_dir.join("resp_gate.json"), &json) {
        eprintln!("FAIL: write json: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(out_dir.join("resp_gate.md"), &md) {
        eprintln!("FAIL: write md: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
