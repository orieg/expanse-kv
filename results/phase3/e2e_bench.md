# Phase 3 End-to-End 16-Core TCP Benchmark Report

**Hardware:** AMD Ryzen 9 9955HX 16-Core Processor (16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic x86_64)
**Host load (`AGENTS.md` §8.11):** `loadavg[0]` pre = `0.42`, post = `3.30` (admissible: `< 8.0`, cores/2)
**Server:** `expanse-kv serve --bind 127.0.0.1 --port <PORT>` (`P = 1` unsharded `SyncExpanseOrderedBytesMap`, Phase 0 decision `#14`)
**Driver:** `phase3_e2e_bench` (`threads = 16`, `pipeline = 16`, `ops_per_thread = 100,000`, `replicates = 5`, `value_bytes = 64`, prefix-structured keys `org:{o:04}:user:{u:06}:session:{id:08}`)

## `expanse-kv serve` End-to-End Loopback TCP Results

*(measured: AMD Ryzen 9 9955HX 16C/32T, 91 GiB RAM, Linux 6.8.0-139-generic, `eval/phase3-e2e-bench`)*

| Key Count (`N`) | Workload | QPS Mean (BCa 95% CI) | Batch p50 (`us`) | Batch p99 (`us`, BCa 95% CI) | `loadavg[0]` Post |
| :---: | :--- | :---: | :---: | :---: | :---: |
| `1,000,000` | `100% read` | `1.611 Mops/s` `[1.538, 1.667]` | `158.28` | `252.90` `[250.56, 255.84]` | `1.79` |
| `1,000,000` | `95/5 read/write` | `1.752 Mops/s` `[1.660, 1.893]` | `127.60` | `412.56` `[396.92, 422.65]` | `2.37` |
| `5,000,000` | `100% read` | `1.560 Mops/s` `[1.514, 1.625]` | `161.87` | `255.32` `[251.31, 257.46]` | `2.81` |
| `5,000,000` | `95/5 read/write` | `1.594 Mops/s` `[1.525, 1.676]` | `142.31` | `431.35` `[421.93, 438.26]` | `3.30` |

## Comparator Status (`AGENTS.md` §8.9)

| Arm | Status | Notes |
| :--- | :---: | :--- |
| 16-shard Valkey 8.x | `UNRUN` | `valkey-server` is not installed on the benchmark host |
| 16-core Dragonfly | `UNRUN` | `dragonfly` is not installed on the benchmark host |

Per `AGENTS.md` §8.9, the comparative equal-core gate is reported as **`UNRUN`** until both comparator binaries are installed and measured in the same campaign.
