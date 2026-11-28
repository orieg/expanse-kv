# Phase 3 Equal-Core 16-Core Comparative TCP Benchmark Report

**Hardware:** AMD Ryzen 9 9955HX 16-Core Processor (16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic x86_64)
**Host load (`AGENTS.md` §8.11):** `loadavg[0]` pre = `2.86`, post = `5.72` (admissible: `< 8.0`, cores/2)
**Driver:** `phase3_e2e_bench` (`threads = 16`, `pipeline = 16`, `ops_per_thread = 100,000`, `replicates = 5`, `value_bytes = 64`, prefix-structured keys `org:{o:04}:user:{u:06}:session:{id:08}`)

## Arms Measured in the Same Campaign (`AGENTS.md` §8.3, §8.9)

- **`expanse-kv 0.2.0` (optimized):** single 16-core process (`P = 1` unsharded `SyncExpanseOrderedBytesMap`, per-connection `EngineReader`, 64-shard `ValueSlab`, zero-alloc `GET`/dispatch, in-place String overwrite).
- **16-shard `Valkey 8.1.12`:** 16 `valkey/valkey:8` containers (`--network=host`, 1 shard/core, `--save "" --appendonly no`), client-partitioned `key_id % 16` (zero cross-shard traffic).
- **16-core `Dragonfly v2.1`:** `dragonflydb/dragonfly:latest` (`--network=host --ulimit memlock=-1 --proactor_threads=16 --dbfilename ""`).

## Results — `N = 1,000,000` Prefix-Structured Keys (`pipeline = 16`, `16T`)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `perf/hot-path-optimization`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `7.324 Mops/s` `[7.142, 7.481]` | `31.60` | `59.07` `[48.35, 79.33]` | `3.17` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `10.315 Mops/s` `[10.107, 10.557]` | `20.63` | `36.03` `[34.93, 36.76]` | `3.88` |
| `Dragonfly v2.1` (16-core) | `100% read` | `4.253 Mops/s` `[3.954, 4.471]` | `45.32` | `87.89` `[81.21, 92.22]` | `5.17` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `6.882 Mops/s` `[6.359, 7.230]` | `32.21` | `83.09` `[57.25, 109.32]` | `3.88` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `10.498 Mops/s` `[10.232, 10.729]` | `20.86` | `35.05` `[33.85, 36.46]` | `3.88` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `4.647 Mops/s` `[4.509, 4.744]` | `45.92` | `80.48` `[78.58, 82.97]` | `5.17` |

## Results — `N = 5,000,000` Prefix-Structured Keys (`pipeline = 16`, `16T`)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `perf/hot-path-optimization`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `6.518 Mops/s` `[6.076, 6.922]` | `34.53` | `87.01` `[48.99, 128.43]` | `4.99` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `9.282 Mops/s` `[8.837, 9.835]` | `22.32` | `36.52` `[33.64, 38.61]` | `4.91` |
| `Dragonfly v2.1` (16-core) | `100% read` | `4.465 Mops/s` `[4.376, 4.654]` | `45.04` | `78.91` `[77.90, 80.06]` | `5.72` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `6.755 Mops/s` `[6.688, 6.820]` | `34.65` | `50.81` `[49.15, 52.04]` | `4.99` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `10.004 Mops/s` `[9.931, 10.067]` | `22.45` | `32.62` `[31.97, 33.36]` | `4.91` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `4.371 Mops/s` `[4.260, 4.438]` | `46.15` | `81.19` `[80.32, 82.06]` | `5.72` |

## Comparative Analysis (`AGENTS.md` §8.3, §8.13)

- **Profile-driven gain (`perf record`):** Eliminating per-op `expanse_trie::occ::Reader::drop` registry-mutex contention (`28.51%` of prior cycles in `futex_wait`), sharding `ValueSlab` across 64 cache-line-padded shards, and zero-alloc `GET`/dispatch lifted `expanse-kv` from `1.62–1.74 Mops/s` to **`6.52–7.32 Mops/s`** (~4.0×–4.4× throughput, ~3×–8× lower p99 latency).
- **vs 16-core `Dragonfly v2.1`:** `expanse-kv` leads in **all four cells** by **`+46%` to `+72%`** QPS with non-overlapping BCa 95% CIs (`7.324` vs `4.253`, `6.882` vs `4.647`, `6.518` vs `4.465`, `6.755` vs `4.371 Mops/s`).
- **vs 16-shard `Valkey 8.1.12`:** 16-shard client-partitioned Valkey leads in all four point-lookup cells (`9.28–10.50 Mops/s` vs `6.52–7.32 Mops/s`, ~1.4×–1.5× higher QPS) at the cost of splitting the keyspace into 16 independent shards without unified ordered `SCAN`/`ZRANGE`.
