//! The `expanse-kv` server binary.
//!
//! No server is implemented yet; `docs/PRD.md` Phase 3 states the scope and
//! gate. Until then the binary says so and exits non-zero, so nothing that
//! starts it can mistake it for a running server.

use std::process::ExitCode;

/// Exit status of a build that contains no server.
const EXIT_NOT_IMPLEMENTED: u8 = 2;

fn main() -> ExitCode {
    eprintln!(
        "expanse-kv {}: this build contains no server; see {}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    );
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}
