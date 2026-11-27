# expanse-kv

A memory-safe, multi-threaded, in-memory key-value store and cache that speaks
the Redis wire protocol (RESP2 and RESP3), indexed by the
[`expanse-trie`](https://github.com/orieg/expanse) radix trie.

> **Status: pre-release, Phase 0.** The workspace, CI and gates exist. No
> server is implemented: the `expanse-kv` binary prints its status and exits
> non-zero. Every performance and memory number in this repository is
> `(target)`; none has been measured.

## What it is for

- Redis clients connect unchanged over RESP2 or RESP3.
- Reads scale across cores in one process, through the trie's optimistic
  concurrent readers.
- Point lookups, prefix scans and ordered ranges are served from one ordered
  structure, with no separate hash table and skip list.

The scope, the design, the regimes where existing servers are expected to win,
and every gate are in [`docs/PRD.md`](docs/PRD.md).

## Start here

| If you want to… | Read |
| :--- | :--- |
| Understand the scope, design and phase gates | [`docs/PRD.md`](docs/PRD.md) |
| Work in this repository, as an agent or a person | [`AGENTS.md`](AGENTS.md) |
| Know what CI checks and how the gates are wired | [`docs/CI.md`](docs/CI.md) |
| Contribute | [`CONTRIBUTING.md`](CONTRIBUTING.md) |
| Report a vulnerability | [`SECURITY.md`](SECURITY.md) |

## Build and check

```bash
cargo build --workspace
```

```bash
scripts/gate.sh
```

`scripts/gate.sh` runs what CI runs: format, clippy with warnings denied, the
workspace tests, the `no_std` build of the core crate, and the
[discipline](https://github.com/orieg/discipline) diff gates when the tool is
installed.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
