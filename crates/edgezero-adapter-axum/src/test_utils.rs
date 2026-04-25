use std::ffi::OsString;
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// Returns a process-wide mutex used to serialize tests that mutate environment variables.
///
/// Both `secret_store` and `service` tests share this lock to avoid data races across
/// test threads when setting or clearing environment variables.
pub fn env_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

/// RAII guard that sets an environment variable for the duration of a test and
/// restores the original value (or removes the variable) on drop.
pub struct EnvOverride {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvOverride {
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }

    pub fn clear(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, original }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            std::env::set_var(self.key, original);
        } else {
            std::env::remove_var(self.key);
        }
    }
}
