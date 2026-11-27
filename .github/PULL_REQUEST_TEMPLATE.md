<!--
Keep the prose factual and impersonal (AGENTS.md §6): what changed, where, and
its status. No time estimates, no hostnames, LAN addresses or home paths.
-->

## What & why

<!-- One paragraph. Reference the issue: `Closes #N` / `Refs #N`, or `no-issue: <reason>`. -->

## Gate

<!-- The docs/PRD.md gate this change moves, quoted, or "none: <what it is instead>". -->

## Verification

<!-- Paste command output and artifact paths, not adjectives. -->

- [ ] `scripts/gate.sh` passed locally
- [ ] Tests added or extended for the change (name them):
- [ ] Every new `unsafe` block has a `// SAFETY:` rationale; none is in `expanse-kv-core` or `expanse-kv-persist`

## Invariants (AGENTS.md §2 — explain any unticked box)

- [ ] `expanse-trie` is consumed as published: not vendored, patched or path-linked
- [ ] No trie is allocated per logical collection
- [ ] No reclamation pin is held across network or disk I/O
- [ ] Key-encoding changes carry ordering and round-trip property tests

## Numbers (AGENTS.md §8 — only if this change publishes or moves a figure)

- [ ] Every figure carries `(measured: <hardware>, <commit>)` resolving to a committed artifact, `(target)` or `(projected)`
- [ ] Comparator arms ran in the same campaign; losing cells are published
- [ ] No pre-registered threshold was edited after the run
