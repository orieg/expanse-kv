# Upstream Valkey Tcl Compatibility Adapter (`docs/PRD.md` §7 Phase 3)

Per `AGENTS.md` §3.3, upstream test suites are fetched at a pinned commit into
the gitignored directory `tests/compat_tcl/upstream/` and are never vendored.

## Pinned Upstream Commit

- **Repository:** `https://github.com/valkey-io/valkey.git`
- **Tag:** `8.0.2`
- **Commit SHA:** `d6a2f561e2751d4ab246ad084e4fa185526fa479`

## Scoped Areas (`docs/PRD.md` §7 Phase 3)

The adapter sources upstream `tests/support/valkey.tcl` (the official Valkey Tcl
RESP client) and executes scoped compatibility suites against a live
`expanse-kv serve` TCP server across:

1. **Protocol** (`PING`, `ECHO`, `HELLO 2/3`, `CLIENT`, `MULTI`/`EXEC`/`DISCARD`, pipelining, error handling)
2. **Strings & Bitmaps** (`GET`, `SET` flags, `MGET`, `MSET`, `INCR`/`DECR`/`INCRBY`/`DECRBY`, `DEL`, `EXISTS`, `TYPE`, `SCAN`, `KEYS`, `SETBIT`/`GETBIT`/`BITCOUNT`)
3. **Hashes** (`HSET`, `HGET`, `HMGET`, `HDEL`, `HEXISTS`, `HLEN`, `HGETALL`, `HSCAN`, `WRONGTYPE`)
4. **Sorted Sets** (`ZADD`, `ZREM`, `ZSCORE`, `ZCARD`, `ZRANGE`, `ZREVRANGE`, `ZRANGEBYSCORE`, `ZREMRANGEBYSCORE`, tie-breaking, `WRONGTYPE`)
5. **Expiry** (`EX`, `PX`, `EXAT`, `EXPIRE`, `PEXPIRE`, `TTL`, `PTTL`, `PERSIST`, `KEEPTTL`)

## Usage

```bash
./tests/compat_tcl/run_compat.sh
```
