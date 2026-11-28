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

## Release

[`.github/workflows/release.yml`](../.github/workflows/release.yml) publishes
`expanse-kv-core`, `expanse-kv-engine`, `expanse-kv-persist` and `expanse-kv` to
crates.io, in that dependency order, when a version tag is pushed. It
authenticates through OIDC trusted publishing: the job trades its GitHub
identity for a short-lived registry token, and no token is stored in the
repository or on a workstation.

| Job | What it does |
| :--- | :--- |
| `release-gate` | Refuses unless the tag is annotated with a forge-verified signature, matches the workspace version, sits on `main`, and its commit's `CI Gate / All Checks Passed` concluded `success` |
| `package` | Packages and verifies the crates in `RELEASE_CRATES` together, without uploading |
| `publish-crates` | Publishes each crate from the `release` environment. A version already on the registry counts as published |

A pre-release tag (`vX.Y.Z-rc.N`) is gated and packaged and publishes nothing.
A manual run is a canary: it packages and verifies, and refuses to publish.

The workflow depends on two settings outside the repository's files:

- a GitHub environment named `release`, restricted to `v*` tags;
- on crates.io, a trusted publisher on each published crate naming this
  repository, the workflow `release.yml` and the environment `release`.

A repository ruleset, `release-tags`, forbids deleting, moving or force-updating
any `v*` tag, with no bypass. A pushed release tag is permanent, so a tag on the
wrong commit spends its version.

Creating a tag is the maintainer's action (`AGENTS.md` §5).
[`scripts/tag-release.sh X.Y.Z`](../scripts/tag-release.sh) makes the release
gate's checks before the tag exists: the remote is this repository, the commit
is on `main`, the tag is new, the workspace version matches, and the rollup
check concluded `success`. It creates the signed tag and prints the push
command; it never pushes.

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
