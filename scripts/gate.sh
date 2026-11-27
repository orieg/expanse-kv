#!/usr/bin/env bash
# Local mirror of the CI jobs in .github/workflows/ci.yml (AGENTS.md §4).
# Every step must pass; the script stops at the first failure.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }

step "fmt"
cargo fmt --all -- --check

step "clippy (warnings denied)"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "tests"
cargo test --workspace --locked

step "expanse-kv-core without std"
cargo check -p expanse-kv-core --no-default-features --locked

step "docs (warnings denied)"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

step "discipline"
if command -v discipline >/dev/null 2>&1; then
  discipline diff
else
  # Not a pass: CI runs the same gates and will report what this step did not.
  echo "NOT RUN: discipline is not on PATH (https://github.com/orieg/discipline)" >&2
fi

printf '\ngate.sh: all steps that ran passed\n'
