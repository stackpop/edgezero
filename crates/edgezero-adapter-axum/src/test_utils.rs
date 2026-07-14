use std::sync::OnceLock;
use tokio::sync::Mutex;

/// Returns a process-wide mutex used to serialize tests that mutate environment variables.
///
/// Both `secret_store` and `service` tests share this lock to avoid data races across
/// test threads when setting or clearing environment variables. Hold it for the whole
/// lifetime of any `edgezero_core::test_env::EnvOverride` — that is the safety contract
/// the guard relies on.
#[inline]
pub fn env_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
