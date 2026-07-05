//! Test-only process-wide guards for concurrent `$PATH` and
//! `env::set_var` mutations across every test module in this crate.
//!
//! `generator.rs::PathOverride` and `config.rs`'s push-shim tests
//! both mutate `$PATH`; without a shared guard, running the same
//! `edgezero-cli` test binary in parallel can interleave PATH
//! restores between the two callsites and produce intermittent
//! "git not found" / "spin: command not found" flakes.
//!
//! `adapter.rs`'s `apply_environment` tests (and any future test
//! that calls `env::set_var` / `env::remove_var` outside of
//! `PathPrepend`) share the same class of hazard: libc's
//! `setenv`/`getenv` aren't thread-safe, and Rust 1.80+ marked
//! `env::set_var` unsafe for exactly this reason.

use std::sync::{Mutex, OnceLock};

/// `$PATH`-mutating guard. Unix-only because Windows PATH semantics
/// and the shim installation pattern differ.
#[cfg(unix)]
pub(crate) fn path_mutation_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

/// Guard for arbitrary process env var mutations. Used across
/// platforms.
pub(crate) fn env_mutation_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
