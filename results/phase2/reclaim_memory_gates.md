# Phase 2 Stalled-Reader Reclamation & Memory Envelope Gates

- **PRD gate:** `docs/PRD.md` §7 Phase 2
- **Raw artifact:** [`results/phase2/reclaim_memory_gates.json`](reclaim_memory_gates.json)
- **Provenance:** `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)`
- **Host load (`loadavg[0]`):** start `0.41`, end `0.50` `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)`

## 1. Relative Stalled-Reader Reclamation Gate

- **Gate (`<= 5%` `(target)` uncollected garbage, zero upward drift across `10^5 -> 10^6 -> 10^7` mutations):** **PASS**

| Mutations | Live Bytes | Trie Bytes | Grace (Uncollected) Bytes | Garbage % |
| ---: | ---: | ---: | ---: | ---: |
| 100000 | 799024 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 159024 | 0 | 0.0000% |
| 1000000 | 799024 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 159024 | 0 | 0.0000% |
| 10000000 | 799024 `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 159024 | 0 | 0.0000% |

## 2. Dense Non-Monotonic Memory Envelope Sweep (`10^3` past `10^6`)

- **Comparative gate vs Valkey 8.x & Dragonfly (`AGENTS.md` §8.9):** `UNRUN` (`valkey-server` 8.x and `dragonfly` are not installed on the benchmark host; no comparator figure is estimated or substituted).

| Shape | Min Trie B/item (`N_min`) | Max Trie B/item (`N_max`) | Min Total B/item (`N_min`) | Max Total B/item (`N_max`) |
| :--- | ---: | ---: | ---: | ---: |
| `prefix_strings` | 15.87 (`2000000`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 16.22 (`1000`) | 79.87 (`2000000`) | 80.22 (`1000`) |
| `uniform_strings` | 47.11 (`1000000`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 55.49 (`10000`) | 111.11 (`1000000`) | 119.49 (`10000`) |
| `hash_fields` | 19.05 (`2000000`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 19.28 (`1000`) | 83.53 (`2000000`) | 83.76 (`1000`) |
| `zset_members` | 69.84 (`1000`) `(measured: AMD Ryzen 9 9955HX 16-Core Processor (32T, Linux 6.8.0-139-generic x86_64), da1efde)` | 71.50 (`10000`) | 70.32 (`1000`) | 71.98 (`10000`) |
