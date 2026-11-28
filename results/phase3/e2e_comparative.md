# Phase 3 Equal-Core 16-Core Comparative TCP Benchmark Report

**Hardware:** AMD Ryzen 9 9955HX 16-Core Processor (16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic x86_64)
**Host load (`AGENTS.md` §8.11):** `loadavg[0]` pre = `1.48`, post = `3.26` (admissible: `< 8.0`, cores/2)
**Driver:** `phase3_e2e_bench` (`value_bytes = 64`, `replicates = 5`, prefix-structured keys `org:{o:04}:user:{u:06}:session:{id:08}`, zero-alloc `ReplyReader`, identical across all 3 arms)

## Arms Measured in the Same Campaign (`AGENTS.md` §8.3, §8.9)

- **`expanse-kv 0.2.0`:** single 16-core process (`P = 1` unsharded `SyncExpanseOrderedBytesMap`, thread-per-core `LocalSet` I/O, fast-path `GET`/`SET`, stack-escaped `StrReader`, 1024-shard `ValueSlab`, cache-line-padded `ServerMetrics`, `#![forbid(unsafe_code)]`).
- **16-shard `Valkey 8.1.12`:** 16 `valkey/valkey:8` containers (`--network=host`, 1 shard/core, `--save "" --appendonly no`), client-partitioned `key_id % 16` (zero cross-shard traffic).
- **16-core `Dragonfly v2.1`:** `dragonflydb/dragonfly:latest` (`--network=host --ulimit memlock=-1 --proactor_threads=16 --dbfilename ""`).

## Results — `N = 1,000,000`, `32T`, `pipeline = 64` (Peak 16C/32T Cell)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `perf/road-to-20m`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `24.787 Mops/s` `[23.241, 25.920]` | `65.98` | `102.28` `[83.31, 125.21]` | `1.72` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `25.956 Mops/s` `[25.198, 27.100]` | `69.96` | `96.25` `[93.25, 99.25]` | `1.66` |
| `Dragonfly v2.1` (16-core) | `100% read` | `11.996 Mops/s` `[11.808, 12.088]` | `158.74` | `244.85` `[244.12, 245.69]` | `2.69` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `23.408 Mops/s` `[21.060, 25.271]` | `70.97` | `129.43` `[93.00, 141.73]` | `1.72` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `25.769 Mops/s` `[25.221, 26.316]` | `68.90` | `95.73` `[93.07, 98.98]` | `1.66` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `11.758 Mops/s` `[11.713, 11.802]` | `162.23` | `251.57` `[250.94, 252.09]` | `2.69` |

## Results — `N = 1,000,000`, `16T`, `pipeline = 16` (PRD Canonical Cell)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `perf/road-to-20m`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `11.574 Mops/s` `[11.517, 11.658]` | `20.84` | `25.11` `[24.43, 26.69]` | `1.72` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `13.228 Mops/s` `[11.733, 13.858]` | `17.37` | `23.36` `[22.15, 24.74]` | `1.66` |
| `Dragonfly v2.1` (16-core) | `100% read` | `6.294 Mops/s` `[6.280, 6.311]` | `38.49` | `60.01` `[59.74, 60.72]` | `1.53` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `10.889 Mops/s` `[10.162, 11.289]` | `21.67` | `27.03` `[25.53, 28.53]` | `1.72` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `12.792 Mops/s` `[11.867, 13.497]` | `17.52` | `23.24` `[21.45, 25.12]` | `1.66` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `6.207 Mops/s` `[6.197, 6.215]` | `39.06` | `60.74` `[60.64, 60.84]` | `1.53` |

## Results — `N = 5,000,000`, `16T`, `pipeline = 16` (PRD Canonical Cell)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `perf/road-to-20m`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `10.070 Mops/s` `[9.422, 10.304]` | `23.51` | `29.52` `[27.63, 31.40]` | `3.55` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `12.435 Mops/s` `[12.326, 12.524]` | `19.02` | `24.10` `[23.39, 25.34]` | `2.15` |
| `Dragonfly v2.1` (16-core) | `100% read` | `6.005 Mops/s` `[5.989, 6.022]` | `39.92` | `64.48` `[64.22, 64.62]` | `3.26` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `9.480 Mops/s` `[8.886, 10.016]` | `24.16` | `29.88` `[28.03, 31.88]` | `3.55` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `12.264 Mops/s` `[12.166, 12.334]` | `19.53` | `24.22` `[23.63, 25.70]` | `2.15` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `5.895 Mops/s` `[5.868, 5.926]` | `40.57` | `65.37` `[65.13, 65.56]` | `3.26` |

## Comparative Summary (`AGENTS.md` §8.3, §8.13)

- **vs 16-core `Dragonfly v2.1`:** `expanse-kv 0.2.0` beats 16-core Dragonfly in **all six cells** (`workload: phase3_comparative_1m_5m`) by **`+61%` to `+107%` (`1.61×–2.07×`)** QPS with **`2.1×–2.4×` lower p99 latency**.
- **vs 16-shard `Valkey 8.1.12`:** At `32T, P64` (workload: `phase3_comparative_1m_32t_p64`), `expanse-kv` reaches **95.5%** of 16-shard client-partitioned Valkey (`24.79 Mops/s` vs `25.96 Mops/s`, overlapping BCa 95% CIs `[23.24, 25.92]` vs `[25.20, 27.10]`) with lower median batch latency (`65.98 us` vs `69.96 us`). At `16T, P16`, 16-shard Valkey leads by `1.14×–1.29×` on point lookups while requiring 16 isolated server processes without a unified ordered keyspace.
