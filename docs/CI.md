# CI

One workflow, [`.github/workflows/ci.yml`](../.github/workflows/ci.yml), runs
on every pull request and on every push to `main`. `scripts/gate.sh` runs the
same checks locally.

## The rollup gate

`main` requires one status check: **`CI Gate / All Checks Passed`** (job
`ci-gate`). It depends on every other job and fails unless each one concluded
`success`. No job is path-filtered, so a skipped job counts as a failure: a
job that did not run verified nothing.

Two rules follow, and the `ci-integrity` discipline gate enforces both:

- Adding a job means adding it to `ci-gate.needs`.
- Every `uses:` is pinned to a full commit SHA.

## Jobs

| Job | What it checks |
| :--- | :--- |
| `discipline` | The diff gates in [`discipline.toml`](../discipline.toml), judged with `main`'s copy of the policy |
| `lint` | `cargo fmt --check`, clippy with warnings denied, rustdoc with warnings denied |
| `test` | `cargo test --workspace` on Linux x86-64, Linux arm64 and macOS; and that the binary exits non-zero while it contains no server |
| `msrv` | The workspace builds on the `rust-version` declared in `Cargo.toml` |
| `no-std` | `expanse-kv-core` builds without `std` for a bare-metal target (`AGENTS.md` §2.6) |
| `supply-chain` | `cargo-deny`: advisories, bans, licences and sources, per [`deny.toml`](../deny.toml) |
| `ci-gate` | The rollup above |

`copilot-setup-steps.yml` is not a check. It installs `discipline` for the
Copilot cloud agent so the hooks in `.github/hooks/` can run.

## Discipline gates

[orieg/discipline](https://github.com/orieg/discipline) reads the diff, the
pull request title and body, and the commits. Most gates are on by default;
`discipline.toml` states only what differs. The ones this repository leans on:

| Gate | What it stops |
| :--- | :--- |
| `assertion-reduction`, `vacuous-tests`, `ignored-tests` | A test that was weakened, cannot fail, or was switched off |
| `stub-bodies` | Placeholder implementations |
| `unsafe-safety-comment` | `unsafe` without a real `// SAFETY:` rationale |
| `time-estimates` | Durations and calendar projections in docs and pull requests |
| `pii`, `shell-secrets` | Hostnames, addresses, home paths and credentials |
| `provenance-tags` | A published number with no `(measured: …)`, `(target)` or `(projected)` tag |
| `issue-link` | A pull request with no issue reference and no `no-issue:` waiver |
| `ratified-paths` | An edit to `AGENTS.md`, the policies, a workflow or an agent hook without the owner's ratification |
| `config-integrity`, `toolchain-config` | A change that loosens the policy or lint configuration it is judged by |
| `dependency-delta` | Dependencies and lockfile entries a change adds |
| `ci-integrity` | A job missing from `ci-gate.needs`, an unpinned action, a masked failure |

To see the effective state of every gate:

```bash
discipline gates
```

To see what a gate checks and the directive that lifts a finding:

```bash
discipline explain <gate>
```

A waiver is a line in the pull request body. It is recorded, counted and
visible in review; it is not a way around a finding that is correct.

## Agent hooks

The same gates run inside coding-agent sessions, so a finding appears while
the change is being written and not only in CI:

| Agent | Configuration |
| :--- | :--- |
| Claude Code | `.claude/settings.json`, `.claude/hooks/discipline-bootstrap.sh` |
| Antigravity | `.agents/hooks.json` |
| Copilot | `.github/hooks/discipline.json`, `.github/workflows/copilot-setup-steps.yml` |

These files are written by `discipline hook install` and are ratified paths.
A hook that cannot find `discipline` on `PATH` says so and does not block.
