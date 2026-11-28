# Phase 4 Persistence Gate Report

**Hardware:** Apple Silicon 8-core ARM64 (16 GiB RAM, macOS)
**Command:** `cargo run --release --example phase4_persist_gate -p expanse-kv-persist`
**Crate safety:** `#![forbid(unsafe_code)]`, zero external dependencies (`AGENTS.md` §2.7)

## Gate Verdicts

*(measured: Apple Silicon 8-core ARM64, 16 GiB RAM, macOS, `feat/phase4-persist`)*

| Check | Requirement (`docs/PRD.md` §7 Phase 4) | Measured | Verdict |
| :--- | :--- | :--- | :---: |
| AOF replay ($10^6$ commands) | 100% state checksum match | `1,000,000` commands (`54,440,104 B`), CRC64 `0x450769ffb58893bb`, `checksum_match = true`, `532.98 ms` | **PASS** |
| `fsync=always` `SIGKILL` truncation | 0 data loss across truncated tail writes | `200` committed commands, `73/73` byte-boundary tail cuts tested, `0` losses | **PASS** |
| Binary snapshot CRC64 roundtrip | 100% state checksum match; reject corruption | `17,757` records (`1,961,192 B`, CRC64 `0xaa367e64d9165f8f`), `64/64` single-bit flips rejected (`0` accepted) | **PASS** |
| Atomic AOF rewrite | 100% state checksum match after compaction | Compacted `1,000,000` -> `85,239` commands (`4,475,314 B`), `checksum_match = true` | **PASS** |
