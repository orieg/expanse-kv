#!/usr/bin/env bash
set -euo pipefail

# Fetch pinned upstream Valkey Tcl support files into gitignored upstream/
# (AGENTS.md §3.3: fetched at a pinned commit, never vendored).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UPSTREAM_DIR="${SCRIPT_DIR}/upstream"
PINNED_REPO="https://github.com/valkey-io/valkey.git"
PINNED_SHA="d6a2f561e2751d4ab246ad084e4fa185526fa479"

if [ -d "${UPSTREAM_DIR}/.git" ]; then
    CURRENT_SHA="$(git -C "${UPSTREAM_DIR}" rev-parse HEAD 2>/dev/null || true)"
    if [ "${CURRENT_SHA}" = "${PINNED_SHA}" ]; then
        echo "upstream Valkey already at pinned commit ${PINNED_SHA}"
        exit 0
    fi
    rm -rf "${UPSTREAM_DIR}"
fi

mkdir -p "${UPSTREAM_DIR}"
git -C "${UPSTREAM_DIR}" init -q
git -C "${UPSTREAM_DIR}" remote add origin "${PINNED_REPO}"
git -C "${UPSTREAM_DIR}" config core.sparseCheckout true
cat > "${UPSTREAM_DIR}/.git/info/sparse-checkout" <<'EOF'
tests/support/
EOF
git -C "${UPSTREAM_DIR}" fetch --depth 1 origin "${PINNED_SHA}"
git -C "${UPSTREAM_DIR}" checkout -q FETCH_HEAD

ACTUAL_SHA="$(git -C "${UPSTREAM_DIR}" rev-parse HEAD)"
if [ "${ACTUAL_SHA}" != "${PINNED_SHA}" ]; then
    echo "error: expected ${PINNED_SHA}, got ${ACTUAL_SHA}" >&2
    exit 1
fi
echo "fetched upstream Valkey tests/support at ${ACTUAL_SHA}"
