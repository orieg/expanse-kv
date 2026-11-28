//! Phase 3 end-to-end pipelined TCP benchmark driver (`docs/PRD.md` §7 Phase 3).
//!
//! Populates a live RESP server with `N` prefix-structured keys
//! (`org:{o:04}:user:{u:06}:session:{id:08}`), then measures multi-threaded
//! pipelined throughput (QPS) and batch p99 latency across `100% read` and
//! `95/5 read/write` mixes with BCa 95% confidence intervals and host load
//! snapshots (`AGENTS.md` §8.4, §8.6, §8.11).

use std::env;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use expanse_kv_bench::stats::{bca_ci_95, percentile_sorted};

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn format_key(id: u64, out: &mut Vec<u8>) {
    out.clear();
    let org = id % 64;
    let user = (id / 64) % 4096;
    write!(out, "org:{org:04}:user:{user:06}:session:{id:08}").expect("format key");
}

fn append_get(key: &[u8], out: &mut Vec<u8>) {
    write!(out, "*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).expect("format GET");
    out.extend_from_slice(key);
    out.extend_from_slice(b"\r\n");
}

fn append_set(key: &[u8], val: &[u8], out: &mut Vec<u8>) {
    write!(out, "*3\r\n$3\r\nSET\r\n${}\r\n", key.len()).expect("format SET");
    out.extend_from_slice(key);
    write!(out, "\r\n${}\r\n", val.len()).expect("format SET len");
    out.extend_from_slice(val);
    out.extend_from_slice(b"\r\n");
}

/// Reads and consumes exactly one RESP2 reply frame from `reader`, returning
/// the number of payload bytes consumed (`AGENTS.md` §8.6: output is consumed).
fn read_one_reply<R: Read>(reader: &mut BufReader<R>, line_buf: &mut Vec<u8>) -> u64 {
    line_buf.clear();
    let mut b = [0u8; 1];
    loop {
        reader.read_exact(&mut b).expect("read reply byte");
        line_buf.push(b[0]);
        if b[0] == b'\n' && line_buf.len() >= 2 && line_buf[line_buf.len() - 2] == b'\r' {
            break;
        }
    }
    match line_buf[0] {
        b'+' | b':' => line_buf.len() as u64,
        b'-' => {
            panic!(
                "server returned RESP error during benchmark: {}",
                String::from_utf8_lossy(line_buf)
            );
        }
        b'$' => {
            let len_str =
                std::str::from_utf8(&line_buf[1..line_buf.len() - 2]).expect("valid bulk length");
            let len: isize = len_str.parse().expect("parse bulk length");
            if len < 0 {
                return 0;
            }
            let to_read = (len as usize) + 2;
            let mut skip = vec![0u8; to_read];
            reader.read_exact(&mut skip).expect("read bulk payload");
            len as u64
        }
        other => panic!("unexpected RESP reply prefix byte 0x{other:02x}"),
    }
}

fn read_loadavg_1m() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg")
        && let Some(first) = text.split_whitespace().next()
        && let Ok(v) = first.parse::<f64>()
    {
        return Some(v);
    }
    None
}

fn populate(addr: &str, n_keys: u64, threads: usize, val_len: usize) -> f64 {
    let val = vec![b'x'; val_len];
    let per_thread = n_keys.div_ceil(threads as u64);
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let start = (tid as u64) * per_thread;
        let end = ((tid as u64 + 1) * per_thread).min(n_keys);
        if start >= end {
            continue;
        }
        let addr_owned = addr.to_string();
        let val_owned = val.clone();
        handles.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(&addr_owned).expect("connect for populate");
            stream.set_nodelay(true).expect("set_nodelay");
            let mut reader =
                BufReader::with_capacity(64 * 1024, stream.try_clone().expect("clone"));
            let mut key = Vec::with_capacity(64);
            let mut batch = Vec::with_capacity(16 * 1024);
            let mut line = Vec::with_capacity(128);
            let mut cur = start;
            let pipe = 64u64;
            while cur < end {
                let count = (end - cur).min(pipe);
                batch.clear();
                for k in cur..cur + count {
                    format_key(k, &mut key);
                    append_set(&key, &val_owned, &mut batch);
                }
                stream.write_all(&batch).expect("write populate batch");
                let mut pop_bytes = 0u64;
                for _ in 0..count {
                    pop_bytes = pop_bytes.wrapping_add(read_one_reply(&mut reader, &mut line));
                }
                std::hint::black_box(pop_bytes);
                cur += count;
            }
        }));
    }
    for h in handles {
        h.join().expect("populate thread panicked");
    }
    t0.elapsed().as_secs_f64()
}

struct ReplicateResult {
    qps: f64,
    batch_p50_us: f64,
    batch_p99_us: f64,
    checksum: u64,
}

struct BenchParams<'a> {
    addr: &'a str,
    n_keys: u64,
    threads: usize,
    pipeline: usize,
    ops_per_thread: usize,
    val_len: usize,
}

fn run_replicate(params: &BenchParams<'_>, write_pct: u64, rep_seed: u64) -> ReplicateResult {
    let barrier = Arc::new(Barrier::new(params.threads));
    let val = vec![b'v'; params.val_len];
    let batches_per_thread = params.ops_per_thread / params.pipeline;
    let mut handles = Vec::with_capacity(params.threads);

    for tid in 0..params.threads {
        let addr_owned = params.addr.to_string();
        let bar = Arc::clone(&barrier);
        let val_owned = val.clone();
        let n_keys = params.n_keys;
        let pipeline = params.pipeline;
        handles.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(&addr_owned).expect("connect worker");
            stream.set_nodelay(true).expect("set_nodelay");
            let mut reader =
                BufReader::with_capacity(64 * 1024, stream.try_clone().expect("clone"));
            let mut rng =
                XorShift64(rep_seed ^ ((tid as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)));
            let mut key = Vec::with_capacity(64);
            let mut batch = Vec::with_capacity(pipeline * 128);
            let mut line = Vec::with_capacity(128);
            let mut latencies_ns: Vec<u64> = Vec::with_capacity(batches_per_thread);
            let mut checksum = 0u64;

            bar.wait();
            let t0 = Instant::now();
            for _ in 0..batches_per_thread {
                batch.clear();
                for _ in 0..pipeline {
                    let r = rng.next();
                    let id = r % n_keys;
                    format_key(id, &mut key);
                    if write_pct > 0 && ((r >> 32) % 100) < write_pct {
                        append_set(&key, &val_owned, &mut batch);
                    } else {
                        append_get(&key, &mut batch);
                    }
                }
                let bt = Instant::now();
                stream.write_all(&batch).expect("write pipeline");
                for _ in 0..pipeline {
                    checksum = checksum.wrapping_add(read_one_reply(&mut reader, &mut line));
                }
                latencies_ns.push(bt.elapsed().as_nanos() as u64);
            }
            let elapsed = t0.elapsed().as_secs_f64();
            (elapsed, latencies_ns, checksum)
        }));
    }

    let mut max_elapsed = 0.0f64;
    let mut all_lats_ns: Vec<u64> = Vec::with_capacity(params.threads * batches_per_thread);
    let mut total_checksum = 0u64;
    for h in handles {
        let (elapsed, lats, cs) = h.join().expect("worker panicked");
        if elapsed > max_elapsed {
            max_elapsed = elapsed;
        }
        all_lats_ns.extend_from_slice(&lats);
        total_checksum = total_checksum.wrapping_add(cs);
    }

    all_lats_ns.sort_unstable();
    let total_ops = (params.threads * batches_per_thread * params.pipeline) as f64;
    ReplicateResult {
        qps: total_ops / max_elapsed,
        batch_p50_us: percentile_sorted(&all_lats_ns, 50.0) as f64 / 1000.0,
        batch_p99_us: percentile_sorted(&all_lats_ns, 99.0) as f64 / 1000.0,
        checksum: total_checksum,
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 6379;
    let mut n_keys: u64 = 1_000_000;
    let mut threads: usize = 16;
    let mut pipeline: usize = 16;
    let mut ops_per_thread: usize = 100_000;
    let mut replicates: usize = 5;
    let mut val_len: usize = 64;
    let mut skip_populate = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                host = args[i].clone();
            }
            "--port" => {
                i += 1;
                port = args[i].parse().expect("invalid --port");
            }
            "--keys" => {
                i += 1;
                n_keys = args[i].parse().expect("invalid --keys");
            }
            "--threads" => {
                i += 1;
                threads = args[i].parse().expect("invalid --threads");
            }
            "--pipeline" => {
                i += 1;
                pipeline = args[i].parse().expect("invalid --pipeline");
            }
            "--ops-per-thread" => {
                i += 1;
                ops_per_thread = args[i].parse().expect("invalid --ops-per-thread");
            }
            "--replicates" => {
                i += 1;
                replicates = args[i].parse().expect("invalid --replicates");
            }
            "--value-bytes" => {
                i += 1;
                val_len = args[i].parse().expect("invalid --value-bytes");
            }
            "--skip-populate" => {
                skip_populate = true;
            }
            other => panic!("unknown arg: {other}"),
        }
        i += 1;
    }

    let addr = format!("{host}:{port}");
    let load_before = read_loadavg_1m();
    let pop_secs = if skip_populate {
        0.0
    } else {
        populate(&addr, n_keys, threads, val_len)
    };
    let load_after_pop = read_loadavg_1m();

    let mixes = [("100_read", 0u64), ("95_5_rw", 5u64)];
    println!("{{");
    println!("  \"n_keys\": {n_keys},");
    println!("  \"threads\": {threads},");
    println!("  \"pipeline\": {pipeline},");
    println!("  \"ops_per_thread\": {ops_per_thread},");
    println!("  \"replicates\": {replicates},");
    println!("  \"value_bytes\": {val_len},");
    println!("  \"populate_secs\": {pop_secs:.3},");
    println!(
        "  \"loadavg_before\": {},",
        load_before.map_or_else(|| "null".to_string(), |v| format!("{v:.2}"))
    );
    println!(
        "  \"loadavg_after_populate\": {},",
        load_after_pop.map_or_else(|| "null".to_string(), |v| format!("{v:.2}"))
    );
    println!("  \"workloads\": {{");

    for (idx, &(name, write_pct)) in mixes.iter().enumerate() {
        let mut qps_vals = Vec::with_capacity(replicates);
        let mut p50_vals = Vec::with_capacity(replicates);
        let mut p99_vals = Vec::with_capacity(replicates);
        let mut cs_total = 0u64;

        let params = BenchParams {
            addr: &addr,
            n_keys,
            threads,
            pipeline,
            ops_per_thread,
            val_len,
        };
        for rep in 0..replicates {
            let seed = 0x2026_0003_u64
                .wrapping_add((idx as u64) * 1000)
                .wrapping_add(rep as u64);
            let res = run_replicate(&params, write_pct, seed);
            qps_vals.push(res.qps);
            p50_vals.push(res.batch_p50_us);
            p99_vals.push(res.batch_p99_us);
            cs_total = cs_total.wrapping_add(res.checksum);
        }

        let qps_ci = bca_ci_95(&qps_vals, 2000, 0x51a7_0001 + idx as u64);
        let p99_ci = bca_ci_95(&p99_vals, 2000, 0x51a7_0010 + idx as u64);
        let p50_mean = p50_vals.iter().sum::<f64>() / p50_vals.len() as f64;
        let load_post = read_loadavg_1m();
        let comma = if idx + 1 < mixes.len() { "," } else { "" };

        println!("    \"{name}\": {{");
        println!("      \"qps_mean\": {:.1},", qps_ci.mean);
        println!("      \"qps_ci95_lo\": {:.1},", qps_ci.ci_low);
        println!("      \"qps_ci95_hi\": {:.1},", qps_ci.ci_high);
        println!("      \"batch_p50_us_mean\": {p50_mean:.2},");
        println!("      \"batch_p99_us_mean\": {:.2},", p99_ci.mean);
        println!("      \"batch_p99_us_ci95_lo\": {:.2},", p99_ci.ci_low);
        println!("      \"batch_p99_us_ci95_hi\": {:.2},", p99_ci.ci_high);
        println!(
            "      \"loadavg_after\": {},",
            load_post.map_or_else(|| "null".to_string(), |v| format!("{v:.2}"))
        );
        println!("      \"checksum\": {cs_total}");
        println!("    }}{comma}");
    }
    println!("  }}");
    println!("}}");
}
