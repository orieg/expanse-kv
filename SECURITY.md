# Security Policy

## Reporting a vulnerability

Report privately through GitHub's **Security → Report a vulnerability** form on
this repository: <https://github.com/orieg/expanse-kv/security/advisories/new>.
That opens a private advisory visible only to the maintainers.

Please do not open a public issue for a suspected vulnerability first. Public
issues are the right place for a crash you can reproduce from safe input, a
build failure, or a question about the threat model below.

A useful report names the entry point (a wire frame, a command, or a persisted
file), the input that triggers the behaviour, and what you observed. A
reproducing test case or a fuzz artifact is worth more than a description.

## Supported versions

No version is released. Once releases exist, fixes land on `main` and ship in
the next release; only the latest released version is supported.

## Threat model

expanse-kv is a network server. Three inputs are untrusted:

- **Bytes from a client connection**, before and after authentication.
- **Bytes read back from disk**: the append-only log and snapshots.
- **Command arguments**: keys, values, patterns, counts and offsets.

The crates that parse the first two, `expanse-kv-core` and
`expanse-kv-persist`, contain no `unsafe` code, enforced by
`#![forbid(unsafe_code)]`. A panic, unbounded allocation or unbounded CPU
reachable from any of the three inputs is a vulnerability.

Out of scope: an operator with write access to the configuration or the data
directory, and denial of service by a client the operator has authorized to
issue expensive commands.
