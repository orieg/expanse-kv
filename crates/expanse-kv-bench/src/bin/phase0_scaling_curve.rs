//! Phase 0 thread-count scaling curve runner (`docs/PRD.md` §7 Phase 0 items 3–4,
//! `results/phase0/scaling_curve_prereg.md`).
//!
//! Runs the pre-registered $(T, P, \text{mix})$ matrix and cross-shard `SCAN`
//! comparison on `SyncExpanseOrderedBytesMap`, capturing anonymized hardware
//! provenance and host load averages before, between, and after blocks
//! (`AGENTS.md` §7, §8.11).

use expanse_kv_bench::bounds::scan_merge_cost;
use expanse_kv_bench::scaling::{
    CellConfig, CellResult, PartitionPolicy, ScanBenchResult, ShardedMap, WorkloadMix,
    generate_prefix_keys, run_cell, run_scan_bench,
};
use expanse_kv_bench::stats::{BcaInterval, bca_ci_95};
use expanse_trie::occ_stats;
use std::env;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Arc;

struct HardwareProvenance {
    cpu_model: String,
    logical_cores: usize,
    mem_gib: u64,
    os_kernel: String,
    git_commit: String,
    occ_stats_enabled: bool,
}

impl HardwareProvenance {
    fn tag(&self) -> String {
        format!(
            "measured: {} ({}T, {} GiB, {}), {}",
            self.cpu_model, self.logical_cores, self.mem_gib, self.os_kernel, self.git_commit
        )
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

fn capture_hardware() -> HardwareProvenance {
    let logical_cores = match std::thread::available_parallelism() {
        Ok(n) => usize::from(n),
        Err(e) => {
            eprintln!("WARN: available_parallelism failed ({e}); defaulting to 1");
            1
        }
    };

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

    let mem_gib = if cfg!(target_os = "macos") {
        cmd_stdout("sysctl", &["-n", "hw.memsize"])
            .and_then(|s| s.parse::<u64>().map_err(|e| e.to_string()))
            .map_or(0, |b| b / (1024 * 1024 * 1024))
    } else {
        match fs::read_to_string("/proc/meminfo") {
            Ok(s) => s
                .lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| match kb.parse::<u64>() {
                    Ok(v) => Some(v / (1024 * 1024)),
                    Err(_) => None,
                })
                .unwrap_or(0),
            Err(_) => 0,
        }
    };

    let os_kernel = cmd_stdout("uname", &["-srm"]).unwrap_or_else(|_| "unknown-os".to_string());
    let git_commit = cmd_stdout("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());

    HardwareProvenance {
        cpu_model,
        logical_cores,
        mem_gib,
        os_kernel,
        git_commit,
        occ_stats_enabled: occ_stats::enabled(),
    }
}

/// Reads the 1-minute host load average.
fn read_load_avg_1m() -> Result<f64, String> {
    if cfg!(target_os = "linux") {
        let s =
            fs::read_to_string("/proc/loadavg").map_err(|e| format!("read /proc/loadavg: {e}"))?;
        let tok = s
            .split_whitespace()
            .next()
            .ok_or_else(|| "empty /proc/loadavg".to_string())?;
        return tok
            .parse::<f64>()
            .map_err(|e| format!("parse /proc/loadavg: {e}"));
    }
    let out = cmd_stdout("sysctl", &["-n", "vm.loadavg"])?;
    let tok = out
        .trim_matches(|c| c == '{' || c == '}' || c == ' ')
        .split_whitespace()
        .next()
        .ok_or_else(|| "empty vm.loadavg".to_string())?;
    tok.parse::<f64>()
        .map_err(|e| format!("parse vm.loadavg: {e}"))
}

fn paired_ratio_ci(num: &[f64], den: &[f64], seed: u64) -> BcaInterval {
    let ratios: Vec<f64> = num.iter().zip(den.iter()).map(|(&a, &b)| a / b).collect();
    bca_ci_95(&ratios, 2_000, seed)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

struct ReportContext<'a> {
    hw: &'a HardwareProvenance,
    quick: bool,
    n_keys: usize,
    rounds: usize,
    ops_per_thread: usize,
    load_start: f64,
    load_end: f64,
    load_limit: f64,
    t_ref: usize,
    p_skew: usize,
    selected_p: usize,
    load_samples: &'a [(String, f64)],
    mem_by_p: &'a [(usize, usize)],
    ro_speedup_ci: Option<BcaInterval>,
    ro_tref_rfr: Option<f64>,
    rw_by_p: &'a [(usize, f64, BcaInterval, Option<f64>)],
    cells: &'a [CellResult],
    scan_results: &'a [ScanBenchResult],
    skew_results: &'a [CellResult],
}

fn render_json(ctx: &ReportContext<'_>) -> Result<String, fmt::Error> {
    let hw = ctx.hw;
    let mut json = String::new();
    writeln!(json, "{{")?;
    writeln!(json, "  \"provenance\": {{")?;
    writeln!(
        json,
        "    \"cpu_model\": \"{}\",",
        json_escape(&hw.cpu_model)
    )?;
    writeln!(json, "    \"logical_cores\": {},", hw.logical_cores)?;
    writeln!(json, "    \"mem_gib\": {},", hw.mem_gib)?;
    writeln!(
        json,
        "    \"os_kernel\": \"{}\",",
        json_escape(&hw.os_kernel)
    )?;
    writeln!(json, "    \"expanse_trie_version\": \"0.19.0\",")?;
    writeln!(
        json,
        "    \"git_commit\": \"{}\",",
        json_escape(&hw.git_commit)
    )?;
    writeln!(json, "    \"occ_stats_enabled\": {},", hw.occ_stats_enabled)?;
    writeln!(json, "    \"quick_mode\": {},", ctx.quick)?;
    writeln!(json, "    \"n_keys\": {},", ctx.n_keys)?;
    writeln!(json, "    \"rounds\": {},", ctx.rounds)?;
    writeln!(json, "    \"ops_per_thread\": {}", ctx.ops_per_thread)?;
    writeln!(json, "  }},")?;

    writeln!(json, "  \"load_avg_1m\": [")?;
    for (idx, (label, val)) in ctx.load_samples.iter().enumerate() {
        let comma = if idx + 1 == ctx.load_samples.len() {
            ""
        } else {
            ","
        };
        writeln!(
            json,
            "    {{\"phase\": \"{}\", \"load_1m\": {:.3}}}{comma}",
            json_escape(label),
            val
        )?;
    }
    writeln!(json, "  ],")?;

    writeln!(json, "  \"mem_used_bytes_by_p\": [")?;
    for (idx, (p, bytes)) in ctx.mem_by_p.iter().enumerate() {
        let comma = if idx + 1 == ctx.mem_by_p.len() {
            ""
        } else {
            ","
        };
        writeln!(
            json,
            "    {{\"shards\": {p}, \"mem_used_bytes\": {bytes}, \"bytes_per_key\": {:.3}}}{comma}",
            (*bytes as f64) / (ctx.n_keys as f64)
        )?;
    }
    writeln!(json, "  ],")?;

    writeln!(json, "  \"scaling_cells\": [")?;
    for (idx, c) in ctx.cells.iter().enumerate() {
        let comma = if idx + 1 == ctx.cells.len() { "" } else { "," };
        let rfr = c
            .read_fallback_rate
            .map_or_else(|| "\"UNRUN\"".to_string(), |v| format!("{v:.8}"));
        let wfr = c
            .write_fallback_rate
            .map_or_else(|| "\"UNRUN\"".to_string(), |v| format!("{v:.8}"));
        writeln!(
            json,
            "    {{\"shards\": {}, \"threads\": {}, \"mix\": \"{:?}\", \"qps_mean\": {:.2}, \"qps_ci_low\": {:.2}, \"qps_ci_high\": {:.2}, \"p99_ns_mean\": {:.2}, \"p99_ns_ci_low\": {:.2}, \"p99_ns_ci_high\": {:.2}, \"read_fallback_rate\": {rfr}, \"write_fallback_rate\": {wfr}}}{comma}",
            c.config.shards,
            c.config.threads,
            c.config.mix,
            c.throughput_ci.mean,
            c.throughput_ci.ci_low,
            c.throughput_ci.ci_high,
            c.p99_ns_ci.mean,
            c.p99_ns_ci.ci_low,
            c.p99_ns_ci.ci_high,
        )?;
    }
    writeln!(json, "  ],")?;

    writeln!(json, "  \"scan_batches\": [")?;
    for (idx, s) in ctx.scan_results.iter().enumerate() {
        let comma = if idx + 1 == ctx.scan_results.len() {
            ""
        } else {
            ","
        };
        writeln!(
            json,
            "    {{\"shards\": {}, \"policy\": \"{:?}\", \"count\": {}, \"ns_per_batch_mean\": {:.2}, \"ns_per_batch_ci_low\": {:.2}, \"ns_per_batch_ci_high\": {:.2}}}{comma}",
            s.shards,
            s.policy,
            s.count,
            s.ns_per_batch_ci.mean,
            s.ns_per_batch_ci.ci_low,
            s.ns_per_batch_ci.ci_high,
        )?;
    }
    writeln!(json, "  ],")?;

    writeln!(json, "  \"hot_prefix_skew\": [")?;
    for (idx, c) in ctx.skew_results.iter().enumerate() {
        let comma = if idx + 1 == ctx.skew_results.len() {
            ""
        } else {
            ","
        };
        writeln!(
            json,
            "    {{\"shards\": {}, \"threads\": {}, \"policy\": \"{:?}\", \"qps_mean\": {:.2}, \"qps_ci_low\": {:.2}, \"qps_ci_high\": {:.2}, \"p99_ns_mean\": {:.2}}}{comma}",
            c.config.shards,
            c.config.threads,
            c.config.policy,
            c.throughput_ci.mean,
            c.throughput_ci.ci_low,
            c.throughput_ci.ci_high,
            c.p99_ns_ci.mean,
        )?;
    }
    writeln!(json, "  ],")?;
    writeln!(json, "  \"selected_p\": {}", ctx.selected_p)?;
    writeln!(json, "}}")?;
    Ok(json)
}

fn render_markdown(ctx: &ReportContext<'_>) -> Result<String, fmt::Error> {
    let prov_tag = ctx.hw.tag();
    let t_ref = ctx.t_ref;
    let selected_p = ctx.selected_p;
    let mut md = String::new();
    writeln!(md, "# Phase 0 Scaling Curve Results\n")?;
    writeln!(
        md,
        "- **Pre-registration:** [`results/phase0/scaling_curve_prereg.md`](scaling_curve_prereg.md)"
    )?;
    writeln!(
        md,
        "- **Raw artifact:** [`results/phase0/scaling_curve.json`](scaling_curve.json)"
    )?;
    writeln!(md, "- **Provenance:** `({prov_tag})`")?;
    writeln!(
        md,
        "- **Host load (1-min avg):** start `{:.2}`, end `{:.2}` (limit `{:.2}`) `({prov_tag})`",
        ctx.load_start, ctx.load_end, ctx.load_limit
    )?;
    writeln!(
        md,
        "- **`occ-stats` enabled:** `{}`\n",
        ctx.hw.occ_stats_enabled
    )?;

    writeln!(md, "## 1. Pre-Registered Gate Evaluation\n")?;
    if let Some(ci) = ctx.ro_speedup_ci {
        let rfr_str = ctx
            .ro_tref_rfr
            .map_or_else(|| "UNRUN".to_string(), |v| format!("{:.4}%", v * 100.0));
        let verdict = if ci.ci_low >= 4.0 && ctx.ro_tref_rfr.is_some_and(|r| r <= 0.001) {
            "PASS"
        } else if ci.mean >= 4.0 && ci.ci_low < 4.0 {
            "INTERMEDIATE"
        } else {
            "FAIL"
        };
        writeln!(
            md,
            "- **Gate 1 (100% read speedup at `P=1`, `T={t_ref}` vs `T=1`, floor `>= 4.0x` `(target)`):** `{:.2}x` (BCa 95% CI `[{:.2}x, {:.2}x]`), reader fallback `{rfr_str}` `({prov_tag})` -> **{verdict}**",
            ci.mean, ci.ci_low, ci.ci_high
        )?;
    }

    if let Some((_, _, ratio_ci, rfr)) = ctx.rw_by_p.iter().find(|(p, _, _, _)| *p == selected_p) {
        let rfr_str = rfr.map_or_else(|| "UNRUN".to_string(), |v| format!("{:.4}%", v * 100.0));
        let p1_rfr = ctx
            .rw_by_p
            .iter()
            .find(|(p, _, _, _)| *p == 1)
            .and_then(|x| x.3);
        let verdict = if ratio_ci.ci_low >= 2.0 {
            "PASS"
        } else if ratio_ci.mean >= 2.0 {
            "INTERMEDIATE"
        } else {
            "FAIL"
        };
        let p1_rfr_str =
            p1_rfr.map_or_else(|| "UNRUN".to_string(), |v| format!("{:.4}%", v * 100.0));
        writeln!(
            md,
            "- **Gate 2 (95/5 sharding speedup at `P*={selected_p}` vs `P=1`, `T={t_ref}`, floor `>= 2.0x` `(target)`):** `{:.2}x` (BCa 95% CI `[{:.2}x, {:.2}x]`), reader fallback `{rfr_str}` vs `P=1` `{p1_rfr_str}` `({prov_tag})` -> **{verdict}**",
            ratio_ci.mean, ratio_ci.ci_low, ratio_ci.ci_high
        )?;
    }

    if let (Some(sh), Some(s1)) = (
        ctx.scan_results
            .iter()
            .find(|s| s.shards == selected_p && s.policy == PartitionPolicy::Hash),
        ctx.scan_results
            .iter()
            .find(|s| s.shards == 1 && s.policy == PartitionPolicy::Hash),
    ) {
        let measured_ratio = sh.ns_per_batch_ci.mean / s1.ns_per_batch_ci.mean;
        let bound = scan_merge_cost(selected_p, 100, 33, 10.0, 0.25)
            .expect("valid scan_merge_cost parameters");
        let verdict = if measured_ratio <= 2.0 * bound.work_ratio_vs_single_shard {
            "PASS"
        } else {
            "FAIL"
        };
        writeln!(
            md,
            "- **Gate 3 (`SCAN COUNT=100` slowdown at `P*={selected_p}` vs `P=1`, ceiling `<= 2.0x` analytical `{:.2}x` `(projected)`):** `{:.2}x` `({prov_tag})` -> **{verdict}**",
            bound.work_ratio_vs_single_shard, measured_ratio
        )?;
    }

    writeln!(md, "\n- **Selection rule output:** `P* = {selected_p}`\n")?;

    writeln!(
        md,
        "## 2. Throughput & Latency Matrix (`{}` prefix keys, `R={}`)\n",
        ctx.n_keys, ctx.rounds
    )?;
    writeln!(
        md,
        "| P | T | Mix | QPS (Mops/s) | BCa 95% CI (Mops/s) | p99 (ns) | Read Fallback | Write Fallback |"
    )?;
    writeln!(
        md,
        "| ---: | ---: | :--- | ---: | :--- | ---: | ---: | ---: |"
    )?;
    for c in ctx.cells {
        let rfr = c
            .read_fallback_rate
            .map_or_else(|| "UNRUN".to_string(), |v| format!("{:.4}%", v * 100.0));
        let wfr = c
            .write_fallback_rate
            .map_or_else(|| "UNRUN".to_string(), |v| format!("{:.4}%", v * 100.0));
        writeln!(
            md,
            "| {} | {} | {:?} | {:.2} `({prov_tag})` | [{:.2}, {:.2}] | {:.0} | {rfr} | {wfr} |",
            c.config.shards,
            c.config.threads,
            c.config.mix,
            c.throughput_ci.mean / 1e6,
            c.throughput_ci.ci_low / 1e6,
            c.throughput_ci.ci_high / 1e6,
            c.p99_ns_ci.mean,
        )?;
    }

    writeln!(
        md,
        "\n## 3. Cross-Shard `SCAN` (`COUNT = 100`) & Hot-Prefix Skew\n"
    )?;
    writeln!(
        md,
        "| P | Policy | Mean (us/batch) | BCa 95% CI (us/batch) |"
    )?;
    writeln!(md, "| ---: | :--- | ---: | :--- |")?;
    for s in ctx.scan_results {
        writeln!(
            md,
            "| {} | {:?} | {:.2} `({prov_tag})` | [{:.2}, {:.2}] |",
            s.shards,
            s.policy,
            s.ns_per_batch_ci.mean / 1e3,
            s.ns_per_batch_ci.ci_low / 1e3,
            s.ns_per_batch_ci.ci_high / 1e3,
        )?;
    }

    writeln!(
        md,
        "\n### Hot-Prefix 95/5 Write Skew (`P={}`, `T={t_ref}`, writes on 1% contiguous prefix)\n",
        ctx.p_skew
    )?;
    writeln!(
        md,
        "| Policy | QPS (Mops/s) | BCa 95% CI (Mops/s) | p99 (ns) |"
    )?;
    writeln!(md, "| :--- | ---: | :--- | ---: |")?;
    for c in ctx.skew_results {
        writeln!(
            md,
            "| {:?} | {:.2} `({prov_tag})` | [{:.2}, {:.2}] | {:.0} |",
            c.config.policy,
            c.throughput_ci.mean / 1e6,
            c.throughput_ci.ci_low / 1e6,
            c.throughput_ci.ci_high / 1e6,
            c.p99_ns_ci.mean,
        )?;
    }
    Ok(md)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");

    let hw = capture_hardware();
    let load_start = match read_load_avg_1m() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FAIL: could not read host load average ({e}) (AGENTS.md §8.11)");
            return ExitCode::FAILURE;
        }
    };

    let load_limit = (hw.logical_cores as f64) / 2.0;
    if !quick && load_start > load_limit {
        eprintln!(
            "INADMISSIBLE: baseline load average {load_start:.2} exceeds logical_cores/2 ({load_limit:.2}) (AGENTS.md §8.11)"
        );
        return ExitCode::FAILURE;
    }

    let (n_keys, warmup_rounds, rounds, ops_per_thread, scan_batches) = if quick {
        (10_000usize, 1usize, 3usize, 10_000usize, 20usize)
    } else {
        (100_000usize, 2usize, 10usize, 200_000usize, 200usize)
    };

    let thread_axes: Vec<usize> = if quick {
        vec![1, 2, 4]
            .into_iter()
            .filter(|&t| t <= hw.logical_cores)
            .collect()
    } else {
        vec![1, 2, 4, 8, 16]
            .into_iter()
            .filter(|&t| t <= hw.logical_cores)
            .collect()
    };
    let shard_axes: Vec<usize> = if quick {
        vec![1, 4, 8]
    } else {
        vec![1, 4, 8, 16, 32]
    };

    eprintln!(
        "Phase 0 scaling curve: {} | load_start={load_start:.2} | occ_stats={} | T={thread_axes:?} | P={shard_axes:?}",
        hw.tag(),
        hw.occ_stats_enabled
    );

    let keys = Arc::new(generate_prefix_keys(n_keys));
    let mut load_samples: Vec<(String, f64)> = vec![("start".to_string(), load_start)];

    let mut cells: Vec<CellResult> = Vec::new();
    let mut mem_by_p: Vec<(usize, usize)> = Vec::new();

    for &p in &shard_axes {
        let map = Arc::new(ShardedMap::from_sorted_keys(
            &keys,
            p,
            PartitionPolicy::Hash,
        ));
        mem_by_p.push((p, map.total_mem_used()));

        for mix in [WorkloadMix::ReadOnly, WorkloadMix::ReadWrite95_5] {
            for &t in &thread_axes {
                let cfg = CellConfig {
                    threads: t,
                    shards: p,
                    mix,
                    policy: PartitionPolicy::Hash,
                    warmup_rounds,
                    rounds,
                    ops_per_thread,
                    latency_stride: 64,
                    hot_prefix_keys: 0,
                };
                let res = run_cell(&map, &keys, cfg);
                eprintln!(
                    "  P={p:2} T={t:2} {:?}: {:.2} Mops/s [CI {:.2}, {:.2}] p99={:.0}ns rfr={:?}",
                    mix,
                    res.throughput_ci.mean / 1e6,
                    res.throughput_ci.ci_low / 1e6,
                    res.throughput_ci.ci_high / 1e6,
                    res.p99_ns_ci.mean,
                    res.read_fallback_rate,
                );
                cells.push(res);
            }
        }
        if let Ok(l) = read_load_avg_1m() {
            load_samples.push((format!("after_P{p}"), l));
        }
    }

    let mut scan_results: Vec<ScanBenchResult> = Vec::new();
    for &p in &shard_axes {
        for policy in [PartitionPolicy::Hash, PartitionPolicy::Range] {
            let map = ShardedMap::from_sorted_keys(&keys, p, policy);
            let res = run_scan_bench(&map, 100, scan_batches, warmup_rounds, rounds);
            eprintln!(
                "  SCAN COUNT=100 P={p:2} {policy:?}: {:.1} us/batch [CI {:.1}, {:.1}]",
                res.ns_per_batch_ci.mean / 1e3,
                res.ns_per_batch_ci.ci_low / 1e3,
                res.ns_per_batch_ci.ci_high / 1e3,
            );
            scan_results.push(res);
        }
    }
    if let Ok(l) = read_load_avg_1m() {
        load_samples.push(("after_scan".to_string(), l));
    }

    let t_ref = thread_axes
        .iter()
        .copied()
        .filter(|&t| t <= 8)
        .max()
        .unwrap_or(1);
    let p_skew = if shard_axes.contains(&16) {
        16
    } else {
        *shard_axes.last().unwrap_or(&1)
    };
    let hot_keys = (n_keys / 100).max(100);
    let mut skew_results: Vec<CellResult> = Vec::new();
    for policy in [PartitionPolicy::Hash, PartitionPolicy::Range] {
        let map = Arc::new(ShardedMap::from_sorted_keys(&keys, p_skew, policy));
        let cfg = CellConfig {
            threads: t_ref,
            shards: p_skew,
            mix: WorkloadMix::ReadWrite95_5,
            policy,
            warmup_rounds,
            rounds,
            ops_per_thread,
            latency_stride: 64,
            hot_prefix_keys: hot_keys,
        };
        let res = run_cell(&map, &keys, cfg);
        eprintln!(
            "  HOT-PREFIX 95/5 P={p_skew} T={t_ref} {policy:?}: {:.2} Mops/s [CI {:.2}, {:.2}] p99={:.0}ns",
            res.throughput_ci.mean / 1e6,
            res.throughput_ci.ci_low / 1e6,
            res.throughput_ci.ci_high / 1e6,
            res.p99_ns_ci.mean,
        );
        skew_results.push(res);
    }

    let load_end = read_load_avg_1m().unwrap_or(load_start);
    load_samples.push(("end".to_string(), load_end));

    let find_cell = |p: usize, t: usize, mix: WorkloadMix| -> Option<&CellResult> {
        cells
            .iter()
            .find(|c| c.config.shards == p && c.config.threads == t && c.config.mix == mix)
    };

    let ro_speedup_ci = find_cell(1, t_ref, WorkloadMix::ReadOnly)
        .zip(find_cell(1, 1, WorkloadMix::ReadOnly))
        .map(|(ct, c1)| {
            let num: Vec<f64> = ct.rounds.iter().map(|r| r.ops_per_sec).collect();
            let den: Vec<f64> = c1.rounds.iter().map(|r| r.ops_per_sec).collect();
            paired_ratio_ci(&num, &den, 0x6A7E_0001)
        });

    let ro_tref_rfr = find_cell(1, t_ref, WorkloadMix::ReadOnly).and_then(|c| c.read_fallback_rate);

    let mut rw_by_p: Vec<(usize, f64, BcaInterval, Option<f64>)> = Vec::new();
    let rw_p1 = find_cell(1, t_ref, WorkloadMix::ReadWrite95_5);
    for &p in &shard_axes {
        if let (Some(cp), Some(c1)) = (find_cell(p, t_ref, WorkloadMix::ReadWrite95_5), rw_p1) {
            let num: Vec<f64> = cp.rounds.iter().map(|r| r.ops_per_sec).collect();
            let den: Vec<f64> = c1.rounds.iter().map(|r| r.ops_per_sec).collect();
            let ratio_ci = paired_ratio_ci(&num, &den, 0x6A7E_0100 + (p as u64));
            rw_by_p.push((p, cp.throughput_ci.mean, ratio_ci, cp.read_fallback_rate));
        }
    }

    let max_rw_qps = rw_by_p.iter().map(|(_, q, _, _)| *q).fold(0.0f64, f64::max);
    let mut selected_p = shard_axes.iter().copied().find(|&p| p >= 4).unwrap_or(1);
    for &(p, qps, _, _) in &rw_by_p {
        if p < 4 {
            continue;
        }
        let next_qps = rw_by_p
            .iter()
            .find(|(np, _, _, _)| *np == p * 2)
            .map(|(_, q, _, _)| *q);
        let clears_90pct = qps >= 0.90 * max_rw_qps;
        let marginal_under_10pct = next_qps.is_none_or(|nq| (nq - qps) / qps < 0.10);
        if clears_90pct && marginal_under_10pct {
            selected_p = p;
            break;
        }
    }

    let out_dir = if quick {
        Path::new("results/quick")
    } else {
        Path::new("results/phase0")
    };
    if let Err(e) = fs::create_dir_all(out_dir) {
        eprintln!("FAIL: could not create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let ctx = ReportContext {
        hw: &hw,
        quick,
        n_keys,
        rounds,
        ops_per_thread,
        load_start,
        load_end,
        load_limit,
        t_ref,
        p_skew,
        selected_p,
        load_samples: &load_samples,
        mem_by_p: &mem_by_p,
        ro_speedup_ci,
        ro_tref_rfr,
        rw_by_p: &rw_by_p,
        cells: &cells,
        scan_results: &scan_results,
        skew_results: &skew_results,
    };

    let json = match render_json(&ctx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL: render_json: {e}");
            return ExitCode::FAILURE;
        }
    };
    let md = match render_markdown(&ctx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL: render_markdown: {e}");
            return ExitCode::FAILURE;
        }
    };

    let json_path = out_dir.join("scaling_curve.json");
    if let Err(e) = fs::write(&json_path, &json) {
        eprintln!("FAIL: could not write {}: {e}", json_path.display());
        return ExitCode::FAILURE;
    }
    let md_path = out_dir.join("scaling_curve.md");
    if let Err(e) = fs::write(&md_path, &md) {
        eprintln!("FAIL: could not write {}: {e}", md_path.display());
        return ExitCode::FAILURE;
    }

    eprintln!("Wrote {} and {}", json_path.display(), md_path.display());
    ExitCode::SUCCESS
}
