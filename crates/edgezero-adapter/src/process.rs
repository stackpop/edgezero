//! Child-process spawning under a process-wide stdout policy.
//!
//! `--format json` reserves the CLI's stdout for exactly one JSON document, so
//! a child that inherits stdout (`cargo`, `fastly`, `wrangler`, `spin`, a
//! manifest shell command) must write to stderr instead. Every inheriting
//! spawn in the workspace goes through [`status`]; `clippy.toml` disallows
//! `Command::status` / `Command::spawn` elsewhere so a new call site cannot
//! bypass the policy. Children whose stdout is captured (`.output()`, piped
//! stdio) never touch the CLI's stdout and are unaffected.
//!
//! The policy is process-wide because a CLI run executes exactly one command;
//! the CLI sets it once per command (see `edgezero_cli`'s `OutputScope`).

use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

static CHILD_STDOUT_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// Whether inheriting children currently have their stdout redirected to the
/// parent's stderr.
#[inline]
#[must_use]
pub fn child_stdout_to_stderr() -> bool {
    CHILD_STDOUT_TO_STDERR.load(Ordering::SeqCst)
}

/// Redirect (`true`) or restore (`false`) inheriting children's stdout.
/// Returns the previous setting so a scope guard can restore it.
#[inline]
pub fn set_child_stdout_to_stderr(enabled: bool) -> bool {
    CHILD_STDOUT_TO_STDERR.swap(enabled, Ordering::SeqCst)
}

/// Run `command` to completion with inherited stdio, except that its stdout is
/// sent to the parent's stderr while [`child_stdout_to_stderr`] is set.
///
/// # Errors
/// Returns the spawn error if the child cannot be started.
#[inline]
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    if child_stdout_to_stderr() {
        command.stdout(Stdio::from(io::stderr()));
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "the one sanctioned inheriting spawn: the stdout policy is applied above"
    )]
    command.status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

    /// Serialises tests that flip the process-wide policy.
    static POLICY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Sets the policy, and restores the previous one on drop (even if the
    /// test panics, so later tests start from the original policy).
    struct PolicyOverride(bool);

    impl PolicyOverride {
        fn set(to_stderr: bool) -> Self {
            Self(set_child_stdout_to_stderr(to_stderr))
        }
    }

    impl Drop for PolicyOverride {
        fn drop(&mut self) {
            set_child_stdout_to_stderr(self.0);
        }
    }

    /// Takes [`POLICY_LOCK`], recovering it if an earlier test panicked.
    fn policy_lock() -> MutexGuard<'static, ()> {
        POLICY_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn set_returns_previous_and_get_reflects_it() {
        let _guard = policy_lock();
        let _policy = PolicyOverride::set(true);
        assert!(child_stdout_to_stderr());
        assert!(set_child_stdout_to_stderr(false));
        assert!(!child_stdout_to_stderr());
    }

    #[cfg(unix)]
    #[test]
    fn status_reports_child_exit() {
        let _guard = policy_lock();
        let _policy = PolicyOverride::set(true);
        let ok = status(Command::new("sh").args(["-c", "echo routed; exit 0"])).expect("spawn");
        let failed = status(Command::new("sh").args(["-c", "exit 3"])).expect("spawn");
        assert!(ok.success());
        assert_eq!(failed.code(), Some(3_i32));
    }
}
