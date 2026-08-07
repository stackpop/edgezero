//! Test-only environment mutation, shared across the workspace.
//!
//! In edition 2024 `std::env::set_var` and `std::env::remove_var` are `unsafe`:
//! the process environment is global mutable state, and a write racing another
//! thread's read (including reads inside libc, e.g. `getaddrinfo` or `tzset`)
//! is undefined behaviour.
//!
//! Rather than spread `unsafe` — and an `#[expect(unsafe_code)]` to opt out of
//! the workspace's `unsafe_code = "deny"` lint — across every crate that
//! overrides an env var in its tests, all mutation funnels through this module.
//! It is the only place in the workspace that writes the environment, so the
//! `unsafe` blocks and their safety argument live in exactly one file.
//!
//! # Safety contract
//!
//! The guards below are RAII: they capture the prior value on construction and
//! restore it on drop, so a panicking test cannot leak state into the next one.
//! That alone does not make concurrent mutation sound — callers must also
//! serialise env-mutating tests on a process-wide mutex ([`env_lock`], or the
//! calling crate's own equivalent) and hold it for the guard's whole lifetime.
//! The `unsafe` here is sound only under that discipline, which is why this
//! module is test-only and never compiled into a production build.

// The workspace's only `unsafe`. See the module docs for the safety argument.
#![expect(
    unsafe_code,
    reason = "std::env::{set_var, remove_var} are unsafe in edition 2024; centralised here so no other crate has to opt out of the unsafe_code deny"
)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// RAII guard that overrides an environment variable for the duration of a test
/// and restores the prior value — or removes the variable if it was previously
/// unset — on drop.
///
/// Construct it while holding [`env_lock`] (or the calling crate's equivalent);
/// see the module docs for the full safety contract.
pub struct EnvOverride {
    key: OsString,
    original: Option<OsString>,
}

/// RAII guard that prepends a directory to `PATH`, so a test can shadow a real
/// CLI (`fastly`, `wrangler`, `spin`, …) with a stub, and restores the prior
/// `PATH` on drop.
pub struct PathPrepend {
    _inner: EnvOverride,
}

impl EnvOverride {
    /// Removes `key` until the guard is dropped.
    #[inline]
    #[must_use]
    pub fn remove<K: AsRef<OsStr>>(key: K) -> Self {
        let owned = key.as_ref().to_owned();
        let original = env::var_os(&owned);
        // SAFETY: the caller holds the env lock for this guard's lifetime, so no
        // other thread reads or writes the environment concurrently.
        let () = unsafe { env::remove_var(&owned) };
        Self {
            key: owned,
            original,
        }
    }

    /// Sets `key` to `value` until the guard is dropped.
    #[inline]
    #[must_use]
    pub fn set<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, value: V) -> Self {
        let owned = key.as_ref().to_owned();
        let original = env::var_os(&owned);
        // SAFETY: as above — the caller holds the env lock.
        let () = unsafe { env::set_var(&owned, value.as_ref()) };
        Self {
            key: owned,
            original,
        }
    }
}

impl Drop for EnvOverride {
    #[inline]
    fn drop(&mut self) {
        match &self.original {
            Some(original) => {
                // SAFETY: as above — the caller holds the env lock for the whole
                // lifetime of this guard, which includes its drop.
                unsafe {
                    env::set_var(&self.key, original);
                }
            }
            None => {
                // SAFETY: as above.
                unsafe {
                    env::remove_var(&self.key);
                }
            }
        }
    }
}

impl PathPrepend {
    /// Prepends `extra` to `PATH` until the guard is dropped.
    #[inline]
    #[must_use]
    pub fn new(extra: &Path) -> Self {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let new_path = match env::var_os("PATH") {
            Some(prev) if !prev.is_empty() => {
                let mut accum = OsString::from(extra);
                accum.push(separator);
                accum.push(prev);
                accum
            }
            _ => OsString::from(extra),
        };
        Self {
            _inner: EnvOverride::set("PATH", new_path),
        }
    }
}

/// Process-wide mutex serialising env-mutating tests within a single test
/// binary. Acquire it before constructing any guard above and hold it for the
/// guard's lifetime; that is what makes the `unsafe` in this module sound.
///
/// One lock per test binary is sufficient: Cargo compiles each crate's tests
/// into their own process, so cross-crate races are impossible. Crates whose
/// env-touching tests are `async` own an equivalent `tokio::sync::Mutex`
/// instead (a `std` guard cannot be held across an `.await`).
#[inline]
#[must_use]
pub fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
