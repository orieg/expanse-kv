# Phase 0 Scaling Curve Results

- **Pre-registration:** [`results/phase0/scaling_curve_prereg.md`](scaling_curve_prereg.md)
- **Raw artifact:** [`results/phase0/scaling_curve.json`](scaling_curve.json)
- **Provenance:** `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)`
- **Host load (`loadavg[0]`):** start `1.43`, end `2.95` (limit `16.00`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)`
- **`occ-stats` enabled:** `true`

## 1. Pre-Registered Gate Evaluation

- **Gate 1 (100% read speedup at `P=1`, `T=8` vs `T=1`, floor `>= 4.0x` `(target)`):** `8.16x` (BCa 95% CI `[7.58x, 8.76x]`), reader fallback `0.0000%` `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` -> **PASS**
- **Gate 2 (95/5 sharding speedup at `P*=4` vs `P=1`, `T=8`, floor `>= 2.0x` `(target)`):** `0.87x` (BCa 95% CI `[0.79x, 0.97x]`), reader fallback `0.0000%` vs `P=1` `0.0000%` `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` -> **FAIL**
- **Gate 3 (`SCAN COUNT=100` slowdown at `P*=4` vs `P=1`, ceiling `<= 2.0x` analytical `1.75x` `(projected)`):** `3.33x` `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` -> **PASS**

- **Selection rule output:** `P* = 4`

## 2. Throughput & Latency Matrix (`100000` prefix keys, `R=10`)

| P | T | Mix | QPS (Mops/s) | BCa 95% CI (Mops/s) | p99 (ns) | Read Fallback | Write Fallback |
| ---: | ---: | :--- | ---: | :--- | ---: | ---: | ---: |
| 1 | 1 | ReadOnly | 7.23 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [6.84, 7.66] | 501 | 0.0000% | 0.0000% |
| 1 | 2 | ReadOnly | 13.66 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [13.18, 14.34] | 551 | 0.0000% | 0.0000% |
| 1 | 4 | ReadOnly | 29.01 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [27.53, 30.18] | 391 | 0.0000% | 0.0000% |
| 1 | 8 | ReadOnly | 58.61 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [55.30, 61.24] | 307 | 0.0000% | 0.0000% |
| 1 | 16 | ReadOnly | 88.66 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [86.07, 90.24] | 250 | 0.0000% | 0.0000% |
| 1 | 1 | ReadWrite95_5 | 6.49 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [6.33, 6.77] | 653 | 0.0000% | 0.0000% |
| 1 | 2 | ReadWrite95_5 | 13.09 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [12.74, 13.69] | 596 | 0.0000% | 0.0000% |
| 1 | 4 | ReadWrite95_5 | 26.62 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [26.01, 27.26] | 493 | 0.0000% | 0.0000% |
| 1 | 8 | ReadWrite95_5 | 51.68 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [47.30, 55.06] | 451 | 0.0000% | 0.0000% |
| 1 | 16 | ReadWrite95_5 | 81.81 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [79.79, 83.16] | 482 | 0.0000% | 0.0000% |
| 4 | 1 | ReadOnly | 5.51 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.19, 5.85] | 447 | 0.0000% | 0.0000% |
| 4 | 2 | ReadOnly | 10.96 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.58, 11.32] | 450 | 0.0000% | 0.0000% |
| 4 | 4 | ReadOnly | 24.91 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [24.38, 25.35] | 344 | 0.0000% | 0.0000% |
| 4 | 8 | ReadOnly | 47.66 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [44.15, 49.94] | 276 | 0.0000% | 0.0000% |
| 4 | 16 | ReadOnly | 73.57 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [70.80, 75.73] | 218 | 0.0000% | 0.0000% |
| 4 | 1 | ReadWrite95_5 | 5.23 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.91, 5.43] | 479 | 0.0000% | 0.0000% |
| 4 | 2 | ReadWrite95_5 | 10.68 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.20, 11.16] | 460 | 0.0000% | 0.0000% |
| 4 | 4 | ReadWrite95_5 | 23.05 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [22.85, 23.20] | 422 | 0.0000% | 0.0000% |
| 4 | 8 | ReadWrite95_5 | 44.18 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [42.27, 45.84] | 367 | 0.0000% | 0.0000% |
| 4 | 16 | ReadWrite95_5 | 70.06 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [68.07, 71.38] | 389 | 0.0000% | 0.0000% |
| 8 | 1 | ReadOnly | 6.28 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [6.21, 6.31] | 407 | 0.0000% | 0.0000% |
| 8 | 2 | ReadOnly | 11.56 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [11.41, 11.73] | 451 | 0.0000% | 0.0000% |
| 8 | 4 | ReadOnly | 23.93 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [22.49, 24.74] | 349 | 0.0000% | 0.0000% |
| 8 | 8 | ReadOnly | 47.15 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [44.60, 48.72] | 321 | 0.0000% | 0.0000% |
| 8 | 16 | ReadOnly | 77.48 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [76.21, 78.81] | 216 | 0.0000% | 0.0000% |
| 8 | 1 | ReadWrite95_5 | 5.33 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.06, 5.63] | 507 | 0.0000% | 0.0000% |
| 8 | 2 | ReadWrite95_5 | 10.79 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.31, 11.11] | 470 | 0.0000% | 0.0000% |
| 8 | 4 | ReadWrite95_5 | 22.02 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [20.77, 22.74] | 431 | 0.0000% | 0.0000% |
| 8 | 8 | ReadWrite95_5 | 43.91 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [42.03, 45.07] | 417 | 0.0000% | 0.0000% |
| 8 | 16 | ReadWrite95_5 | 69.71 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [68.53, 70.73] | 368 | 0.0000% | 0.0000% |
| 16 | 1 | ReadOnly | 5.85 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.32, 6.13] | 375 | 0.0000% | 0.0000% |
| 16 | 2 | ReadOnly | 10.69 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.38, 10.96] | 494 | 0.0000% | 0.0000% |
| 16 | 4 | ReadOnly | 23.50 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [21.76, 24.28] | 319 | 0.0000% | 0.0000% |
| 16 | 8 | ReadOnly | 46.42 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [44.80, 47.39] | 276 | 0.0000% | 0.0000% |
| 16 | 16 | ReadOnly | 72.37 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [68.12, 73.92] | 226 | 0.0000% | 0.0000% |
| 16 | 1 | ReadWrite95_5 | 5.48 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.16, 5.81] | 455 | 0.0000% | 0.0000% |
| 16 | 2 | ReadWrite95_5 | 10.72 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.60, 11.00] | 498 | 0.0000% | 0.0000% |
| 16 | 4 | ReadWrite95_5 | 20.86 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [20.47, 21.24] | 471 | 0.0001% | 0.0000% |
| 16 | 8 | ReadWrite95_5 | 41.87 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [40.53, 43.02] | 455 | 0.0000% | 0.0000% |
| 16 | 16 | ReadWrite95_5 | 67.82 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [66.80, 68.50] | 460 | 0.0001% | 0.0002% |
| 32 | 1 | ReadOnly | 5.62 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.31, 5.93] | 383 | 0.0000% | 0.0000% |
| 32 | 2 | ReadOnly | 10.94 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [10.72, 11.18] | 500 | 0.0000% | 0.0000% |
| 32 | 4 | ReadOnly | 23.02 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [22.28, 23.64] | 345 | 0.0000% | 0.0000% |
| 32 | 8 | ReadOnly | 46.69 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [44.29, 47.76] | 247 | 0.0000% | 0.0000% |
| 32 | 16 | ReadOnly | 76.38 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [75.10, 77.68] | 215 | 0.0000% | 0.0000% |
| 32 | 1 | ReadWrite95_5 | 5.19 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.97, 5.45] | 470 | 0.0000% | 0.0000% |
| 32 | 2 | ReadWrite95_5 | 9.84 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [9.44, 10.15] | 577 | 0.0000% | 0.0000% |
| 32 | 4 | ReadWrite95_5 | 20.01 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [19.28, 21.00] | 591 | 0.0000% | 0.0000% |
| 32 | 8 | ReadWrite95_5 | 38.32 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [36.53, 39.20] | 640 | 0.0001% | 0.0000% |
| 32 | 16 | ReadWrite95_5 | 64.02 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [62.38, 64.92] | 706 | 0.0002% | 0.0004% |

## 3. Cross-Shard `SCAN` (`COUNT = 100`) & Hot-Prefix Skew

| P | Policy | Mean (us/batch) | BCa 95% CI (us/batch) |
| ---: | :--- | ---: | :--- |
| 1 | Hash | 4.98 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.97, 4.98] |
| 1 | Range | 5.63 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [5.25, 6.75] |
| 4 | Hash | 16.59 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [16.57, 16.62] |
| 4 | Range | 4.95 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.94, 4.95] |
| 8 | Hash | 28.59 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [28.58, 28.63] |
| 8 | Range | 4.94 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.94, 4.95] |
| 16 | Hash | 56.76 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [56.70, 56.84] |
| 16 | Range | 4.97 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.97, 4.97] |
| 32 | Hash | 62.37 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [62.23, 62.49] |
| 32 | Range | 4.98 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [4.98, 4.99] |

### Hot-Prefix 95/5 Write Skew (`P=16`, `T=8`, writes on 1% contiguous prefix)

| Policy | QPS (Mops/s) | BCa 95% CI (Mops/s) | p99 (ns) |
| :--- | ---: | :--- | ---: |
| Hash | 42.25 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [39.77, 43.45] | 449 |
| Range | 44.94 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, 91 GiB, Linux 6.8.0-139-generic x86_64), 2f18468)` | [43.35, 45.84] | 392 |
