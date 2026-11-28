# Phase 3 Equal-Core 16-Core Comparative TCP Benchmark Report

**Hardware:** AMD Ryzen 9 9955HX 16-Core Processor (16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic x86_64)
**Host load (`AGENTS.md` §8.11):** `loadavg[0]` pre = `0.02`, post = `3.74` (admissible: `< 8.0`, cores/2)
**Driver:** `phase3_e2e_bench` (`threads = 16`, `pipeline = 16`, `ops_per_thread = 100,000`, `replicates = 5`, `value_bytes = 64`, prefix-structured keys `org:{o:04}:user:{u:06}:session:{id:08}`)

## Arms Measured in the Same Campaign (`AGENTS.md` §8.3, §8.9)

- **`expanse-kv 0.2.0`:** single 16-core process (`expanse-kv serve --bind 127.0.0.1 --port <P>`, `P = 1` unsharded `SyncExpanseOrderedBytesMap`).
- **16-shard `Valkey 8.1.12`:** 16 `valkey/valkey:8` containers (`--network=host`, 1 shard/core, `--save "" --appendonly no`), client-partitioned `key_id % 16` (zero cross-shard traffic).
- **16-core `Dragonfly v2.1`:** `dragonflydb/dragonfly:latest` (`--network=host --ulimit memlock=-1 --proactor_threads=16 --dbfilename ""`).

## Results — `N = 1,000,000` Prefix-Structured Keys (`pipeline = 16`, `16T`)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `eval/phase3-comparative`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `1.652 Mops/s` `[1.538, 1.963]` | `152.88` | `248.51` `[239.73, 252.21]` | `1.65` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `11.433 Mops/s` `[11.045, 11.712]` | `18.71` | `30.49` `[28.12, 32.49]` | `2.21` |
| `Dragonfly v2.1` (16-core) | `100% read` | `5.699 Mops/s` `[5.539, 5.787]` | `40.53` | `63.70` `[62.04, 65.37]` | `2.21` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `1.729 Mops/s` `[1.492, 1.977]` | `131.48` | `424.16` `[393.67, 449.77]` | `2.31` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `11.967 Mops/s` `[11.833, 12.070]` | `18.56` | `27.37` `[25.46, 29.14]` | `2.21` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `5.689 Mops/s` `[5.663, 5.710]` | `40.79` | `64.12` `[63.37, 65.53]` | `3.07` |

## Results — `N = 5,000,000` Prefix-Structured Keys (`pipeline = 16`, `16T`)

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `eval/phase3-comparative`)*

| Arm | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :--- | :--- | :---: | :---: | :---: | :---: |
| `expanse-kv 0.2.0` | `100% read` | `1.623 Mops/s` `[1.538, 1.721]` | `156.96` | `252.21` `[246.95, 255.90]` | `3.72` |
| `Valkey 8.1.12` (16-shard) | `100% read` | `10.727 Mops/s` `[10.064, 11.013]` | `20.36` | `31.54` `[29.94, 32.99]` | `3.98` |
| `Dragonfly v2.1` (16-core) | `100% read` | `5.464 Mops/s` `[5.442, 5.484]` | `41.93` | `70.19` `[69.77, 71.01]` | `3.74` |
| `expanse-kv 0.2.0` | `95/5 read/write` | `1.740 Mops/s` `[1.637, 1.823]` | `128.76` | `410.06` `[401.58, 420.03]` | `3.98` |
| `Valkey 8.1.12` (16-shard) | `95/5 read/write` | `10.531 Mops/s` `[10.242, 10.829]` | `20.89` | `32.59` `[30.19, 34.82]` | `3.98` |
| `Dragonfly v2.1` (16-core) | `95/5 read/write` | `5.440 Mops/s` `[5.381, 5.482]` | `42.34` | `68.83` `[67.40, 70.49]` | `3.74` |

## Honest Comparative Disclosure (`AGENTS.md` §8.3, §8.13)

- In this initial `0.2.0` server build, both 16-shard `Valkey 8.1.12` (`6.0×–6.9×` higher QPS) and 16-core `Dragonfly v2.1` (`3.1×–3.5×` higher QPS) outperform `expanse-kv 0.2.0` across all four point-lookup cells.
- By contrast, the underlying `SyncExpanseOrderedBytesMap` measured `88.66 Mops/s` at 16 threads in Phase 0 (`results/phase0/scaling_curve.md`), indicating that current end-to-end throughput is bounded by server/engine hot-path overhead rather than the trie; exact hotspot attribution is pending profiling (`AGENTS.md` §8.13).
