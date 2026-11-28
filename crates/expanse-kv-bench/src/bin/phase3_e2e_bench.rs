//! Phase 3 end-to-end pipelined TCP benchmark driver (`docs/PRD.md` §7 Phase 3).
//!
//! Populates a live RESP server with `N` prefix-structured keys
//! (`org:{o:04}:user:{u:06}:session:{id:08}`), then measures multi-threaded
//! pipelined throughput (QPS) and batch p99 latency across `100% read` and
//! `95/5 read/write` mixes with BCa 95% confidence intervals and host load
//! snapshots (`AGENTS.md` §8.4, §8.6, §8.11).

use std::env;
use std::io::{Read, Write};
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

#[inline]
fn write_fixed_dec(mut v: u64, dst: &mut [u8]) {
    let mut i = dst.len();
    while i > 0 {
        i -= 1;
        dst[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
}

#[inline]
fn format_key(id: u64, out: &mut Vec<u8>) {
    // Exact 36-byte key: "org:0000:user:000000:session:00000000" (37 bytes)
    out.clear();
    let mut buf = *b"org:0000:user:000000:session:00000000";
    let org = id % 64;
    let user = (id / 64) % 4096;
    write_fixed_dec(org, &mut buf[4..8]);
    write_fixed_dec(user, &mut buf[14..20]);
    write_fixed_dec(id, &mut buf[29..37]);
    out.extend_from_slice(&buf);
}

#[inline]
fn append_get(key: &[u8], out: &mut Vec<u8>) {
    if key.len() == 37 {
        out.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$37\r\n");
    } else {
        write!(out, "*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).expect("format GET");
    }
    out.extend_from_slice(key);
    out.extend_from_slice(b"\r\n");
}

#[inline]
fn append_set(key: &[u8], val: &[u8], out: &mut Vec<u8>) {
    if key.len() == 37 && val.len() == 64 {
        out.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$37\r\n");
        out.extend_from_slice(key);
        out.extend_from_slice(b"\r\n$64\r\n");
        out.extend_from_slice(val);
        out.extend_from_slice(b"\r\n");
        return;
    }
    write!(out, "*3\r\n$3\r\nSET\r\n${}\r\n", key.len()).expect("format SET");
    out.extend_from_slice(key);
    write!(out, "\r\n${}\r\n", val.len()).expect("format SET len");
    out.extend_from_slice(val);
    out.extend_from_slice(b"\r\n");
}

/// Zero-allocation pipelined RESP2 reply reader (`AGENTS.md` §8.6: consumes
/// every reply and folds payload lengths into a checksum).
struct ReplyReader {
    buf: Vec<u8>,
    pos: usize,
    filled: usize,
}

impl ReplyReader {
    fn new() -> Self {
        Self {
            buf: vec![0u8; 64 * 1024],
            pos: 0,
            filled: 0,
        }
    }

    fn read_n_replies(&mut self, stream: &mut TcpStream, count: usize) -> u64 {
        let mut checksum = 0u64;
        let mut remaining = count;
        while remaining > 0 {
            while remaining > 0 && self.pos < self.filled {
                let slice = &self.buf[self.pos..self.filled];
                match parse_one_reply(slice) {
                    Some((consumed, val_bytes)) => {
                        self.pos += consumed;
                        checksum = checksum.wrapping_add(val_bytes);
                        remaining -= 1;
                    }
                    None => break,
                }
            }
            if remaining == 0 {
                if self.pos == self.filled {
                    self.pos = 0;
                    self.filled = 0;
                }
                break;
            }
            if self.pos > 0 {
                let rem = self.filled - self.pos;
                self.buf.copy_within(self.pos..self.filled, 0);
                self.pos = 0;
                self.filled = rem;
            }
            if self.filled == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
            }
            let n = stream
                .read(&mut self.buf[self.filled..])
                .expect("read reply bytes");
            assert!(n > 0, "unexpected EOF while reading replies");
            self.filled += n;
        }
        checksum
    }
}

#[inline]
fn parse_one_reply(slice: &[u8]) -> Option<(usize, u64)> {
    let mut i = 1usize;
    let len = slice.len();
    let crlf = loop {
        if i + 1 >= len {
            return None;
        }
        if slice[i] == b'\r' && slice[i + 1] == b'\n' {
            break i;
        }
        i += 1;
    };
    match slice[0] {
        b'+' | b':' => Some((crlf + 2, (crlf + 2) as u64)),
        b'-' => panic!(
            "server returned RESP error during benchmark: {}",
            String::from_utf8_lossy(&slice[..crlf])
        ),
        b'$' => {
            if crlf == 3 && slice[1] == b'-' && slice[2] == b'1' {
                return Some((5, 0));
            }
            let mut n = 0usize;
            for &b in &slice[1..crlf] {
                n = n * 10 + usize::from(b - b'0');
            }
            let total = crlf + 2 + n + 2;
            if total > len {
                return None;
            }
            Some((total, n as u64))
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

fn populate(host: &str, ports: &[u16], n_keys: u64, threads: usize, val_len: usize) -> f64 {
    let val = vec![b'x'; val_len];
    let n_shards = ports.len() as u64;
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let port = ports[tid % ports.len()];
        let addr_owned = format!("{host}:{port}");
        let val_owned = val.clone();
        let (start, step, limit) = if n_shards > 1 {
            (tid as u64, n_shards, n_keys)
        } else {
            let per = n_keys.div_ceil(threads as u64);
            let s = (tid as u64) * per;
            let e = ((tid as u64 + 1) * per).min(n_keys);
            (s, 1, e)
        };
        if start >= limit {
            continue;
        }
        handles.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(&addr_owned).expect("connect for populate");
            stream.set_nodelay(true).expect("set_nodelay");
            let mut reader = ReplyReader::new();
            let mut key = Vec::with_capacity(64);
            let mut batch = Vec::with_capacity(16 * 1024);
            let mut cur = start;
            let pipe = 64usize;
            while cur < limit {
                batch.clear();
                let mut count = 0usize;
                while cur < limit && count < pipe {
                    format_key(cur, &mut key);
                    append_set(&key, &val_owned, &mut batch);
                    cur += step;
                    count += 1;
                }
                stream.write_all(&batch).expect("write populate batch");
                let pop_bytes = reader.read_n_replies(&mut stream, count);
                std::hint::black_box(pop_bytes);
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
    host: &'a str,
    ports: &'a [u16],
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
    let n_shards = params.ports.len() as u64;
    let mut handles = Vec::with_capacity(params.threads);

    for tid in 0..params.threads {
        let port = params.ports[tid % params.ports.len()];
        let addr_owned = format!("{}:{port}", params.host);
        let bar = Arc::clone(&barrier);
        let val_owned = val.clone();
        let n_keys = params.n_keys;
        let pipeline = params.pipeline;
        let shard_idx = (tid as u64) % n_shards;
        let shard_keys = if n_shards > 1 {
            (n_keys - shard_idx).div_ceil(n_shards).max(1)
        } else {
            n_keys
        };
        handles.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(&addr_owned).expect("connect worker");
            stream.set_nodelay(true).expect("set_nodelay");
            let mut reader = ReplyReader::new();
            let mut rng =
                XorShift64(rep_seed ^ ((tid as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)));
            let mut key = Vec::with_capacity(64);
            let mut batch = Vec::with_capacity(pipeline * 128);
            let mut latencies_ns: Vec<u64> = Vec::with_capacity(batches_per_thread);
            let mut checksum = 0u64;

            bar.wait();
            let t0 = Instant::now();
            for _ in 0..batches_per_thread {
                batch.clear();
                for _ in 0..pipeline {
                    let r = rng.next();
                    let id = if n_shards > 1 {
                        (r % shard_keys) * n_shards + shard_idx
                    } else {
                        r % n_keys
                    };
                    format_key(id, &mut key);
                    if write_pct > 0 && ((r >> 32) % 100) < write_pct {
                        append_set(&key, &val_owned, &mut batch);
                    } else {
                        append_get(&key, &mut batch);
                    }
                }
                let bt = Instant::now();
                stream.write_all(&batch).expect("write pipeline");
                checksum = checksum.wrapping_add(reader.read_n_replies(&mut stream, pipeline));
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
    let mut ports: Vec<u16> = vec![6379];
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
                ports = vec![args[i].parse().expect("invalid --port")];
            }
            "--ports" => {
                i += 1;
                ports = args[i]
                    .split(',')
                    .map(|p| p.parse::<u16>().expect("invalid port in --ports"))
                    .collect();
                assert!(!ports.is_empty(), "--ports must not be empty");
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

    let load_before = read_loadavg_1m();
    let pop_secs = if skip_populate {
        0.0
    } else {
        populate(&host, &ports, n_keys, threads, val_len)
    };
    let load_after_pop = read_loadavg_1m();

    let mixes = [("100_read", 0u64), ("95_5_rw", 5u64)];
    println!("{{");
    println!("  \"n_keys\": {n_keys},");
    println!("  \"shards\": {},", ports.len());
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
            host: &host,
            ports: &ports,
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
