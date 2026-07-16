//! Test-only process-wide guard for concurrent `$PATH` mutations
//! across every test module in this crate.
//!
//! `generator.rs`'s scaffold test and `config.rs`'s push-shim tests
//! both mutate `$PATH`; without a shared guard, running the same
//! `edgezero-cli` test binary in parallel can interleave PATH
//! restores between the two callsites and produce intermittent
//! "git not found" / "spin: command not found" flakes.
//!
//! The mutation itself goes through `edgezero_core::test_env`'s
//! `PathPrepend` / `EnvOverride` RAII guards, which contain the
//! `unsafe` that edition 2024 requires around `env::set_var` /
//! `env::remove_var` (libc's `setenv`/`getenv` aren't thread-safe).
//! Holding this mutex for the guard's lifetime is what makes that
//! `unsafe` sound within this test binary.
//!
//! Tests that mutate a non-PATH env var take
//! `test_support::manifest_guard()` instead — it already serialises
//! the `EDGEZERO_MANIFEST` overrides those tests set.

use std::sync::{Mutex, OnceLock};

/// `$PATH`-mutating guard. Unix-only because Windows PATH semantics
/// and the shim installation pattern differ.
#[cfg(unix)]
pub(crate) fn path_mutation_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
