#!/usr/bin/env bash
set -euo pipefail

# Run the scoped Valkey/Redis Tcl compatibility suite against a live
# `expanse-kv serve` instance (docs/PRD.md §7 Phase 3).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PORT="${EXPANSE_KV_COMPAT_PORT:-16379}"

"${SCRIPT_DIR}/fetch_upstream.sh"

cargo build --quiet --locked --release -p expanse-kv --manifest-path "${REPO_ROOT}/Cargo.toml"

SERVER_PID=""
cleanup() {
    if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

"${REPO_ROOT}/target/release/expanse-kv" serve --bind 127.0.0.1 --port "${PORT}" >/dev/null 2>&1 &
SERVER_PID=$!

# Wait for TCP port readiness
READY=0
for _ in $(seq 1 50); do
    if tclsh <<EOF >/dev/null 2>&1
if {[catch {set s [socket 127.0.0.1 ${PORT}]; close \$s}]} { exit 1 }
exit 0
EOF
    then
        READY=1
        break
    fi
    sleep 0.1
done

if [ "${READY}" -ne 1 ]; then
    echo "error: expanse-kv server did not start on 127.0.0.1:${PORT}" >&2
    exit 1
fi

tclsh "${SCRIPT_DIR}/driver.tcl" 127.0.0.1 "${PORT}"
