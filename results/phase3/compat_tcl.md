# Phase 3 Scoped Valkey Tcl Compatibility Gate Report

**Hardware:** Apple Silicon 8-core ARM64 (16 GiB RAM, macOS)
**Upstream client:** `valkey-io/valkey` `8.0.2` (`d6a2f561e2751d4ab246ad084e4fa185526fa479`), fetched into gitignored `tests/compat_tcl/upstream/` (`AGENTS.md` §3.3)
**Command:** `./tests/compat_tcl/run_compat.sh`

## Results

*(measured: Apple Silicon 8-core ARM64, 16 GiB RAM, macOS, `feat/phase3-compat-tcl`)*

| Category | Passed | Total | Pass Rate |
| :--- | :---: | :---: | :---: |
| Protocol (RESP2/RESP3, `MULTI`/`EXEC`, `HELLO`) | 14 | 14 | 100.00% |
| Strings & Bitmaps (`SET`/`GET`/`SCAN`/`SETBIT`) | 15 | 15 | 100.00% |
| Hashes (`HSET`/`HGET`/`HMGET`/`HDEL`/`HSCAN`) | 6 | 6 | 100.00% |
| Sorted Sets (`ZADD`/`ZRANGE`/`BYSCORE`/`REV`) | 10 | 10 | 100.00% |
| Expiry (`EX`/`PX`/`PEXPIRE`/`TTL`/`PERSIST`) | 5 | 5 | 100.00% |
| **Total** | **50** | **50** | **100.00%** |

**Gate floor (`docs/PRD.md` §7 Phase 3):** $\ge 98.0\%$ -> **PASS** (`50 / 50 = 100.00%`).
