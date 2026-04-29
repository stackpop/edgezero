use std::env;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// RAII guard that sets an environment variable for the duration of a test and
/// restores the original value (or removes the variable) on drop.
pub struct EnvOverride {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvOverride {
    #[must_use]
    pub fn clear(key: &'static str) -> Self {
        let original = env::var_os(key);
        env::remove_var(key);
        Self { key, original }
    }

    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let original = env::var_os(key);
        env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            env::set_var(self.key, original);
        } else {
            env::remove_var(self.key);
        }
    }
}

/// Returns a process-wide mutex used to serialize tests that mutate environment variables.
///
/// Both `secret_store` and `service` tests share this lock to avoid data races across
/// test threads when setting or clearing environment variables.
pub fn env_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
