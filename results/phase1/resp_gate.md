# Phase 1 Gate Verification (`expanse-kv-core`)

- **PRD gate:** `docs/PRD.md` §7 Phase 1
- **Raw artifact:** [`results/phase1/resp_gate.json`](resp_gate.json)
- **Provenance:** `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)`
- **Host load (`loadavg[0]`):** start `0.10`, end `0.10` (limit `16.00`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)`

## Gate Verdicts

- **Fuzz stability (`10^7` inputs, 0 panics / 0 UB under `#![forbid(unsafe_code)]`):** `10000000` inputs, `0` panics (`331895` complete frames, `3557786` complete commands) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)` -> **PASS**
- **Pipelined `GET`/`SET` throughput (floor `>= 2.0 GB/s` `(target)`, BCa 95% CI lower bound):** `6.77 GB/s` (BCa 95% CI `[6.77, 6.77] GB/s`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)` -> **PASS**
- **Pipelined `GET`-only throughput:** `5.61 GB/s` (BCa 95% CI `[5.61, 5.61] GB/s`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)`
- **Pipelined `SET`-only throughput:** `7.61 GB/s` (BCa 95% CI `[7.60, 7.62] GB/s`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), b8847c2)`
