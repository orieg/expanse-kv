# Contributing to expanse-kv

[`AGENTS.md`](AGENTS.md) is the contract for every contributor, human or
agent. This page is the short path through it.

1. **Find or open an issue.** Every pull request references one, or carries
   `no-issue: <reason>` in its body.
2. **Name the gate.** Work is sized as one gate in [`docs/PRD.md`](docs/PRD.md)
   moving from unmet to met (`AGENTS.md` §10).
3. **Branch from `main`**, commit with Conventional Commits, and sign your
   commits.
4. **Run `scripts/gate.sh`** until it is green.
5. **Open a pull request.** The one required check is
   `CI Gate / All Checks Passed`; [`docs/CI.md`](docs/CI.md) explains each job
   and how a discipline finding is lifted.

Changes to a threshold, a dependency, a key encoding, the workflows or
`AGENTS.md` itself need an owner decision first (`AGENTS.md` §5).

Unless you state otherwise, a contribution you submit is licensed under the
same dual MIT / Apache-2.0 terms as the project.
