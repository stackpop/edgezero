//! Cross-process advisory lock for `provision` / `config push`.
//!
//! `provision --local` and `config push --local` both perform
//! read-modify-write on shared files -- `edgezero.toml`,
//! `.env` / `.dev.vars` / `.edgezero/.env`, adapter TOML manifests.
//! Two concurrent `edgezero` invocations against the same project
//! tree can interleave their reads and writes and silently drop each
//! other's edits (spec §"Non-atomic writes"): run A reads baseline,
//! run B reads baseline, both compute their appends, whichever
//! writes second wins and loses the loser's additions.
//!
//! An OS-level advisory lock on a sentinel file next to
//! `edgezero.toml` serialises the invocations. The lock is released
//! either explicitly (via drop) or automatically on process exit --
//! so a crashed run never leaves the lock stuck.
//!
//! Lock file: `<manifest-parent>/.edgezero/provision.lock`. Kept
//! inside `.edgezero/` so all per-machine CLI state (Axum's
//! `.edgezero/.env`, `.edgezero/local-config-<id>.json`, the
//! advisory lock) shares one gitignored directory. The parent dir
//! is created lazily on first acquire. The file is created lazily
//! and never truncated so multiple runs can share the sentinel
//! across time.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::path_safety::reject_symlink_components;

/// Guard object representing an active advisory lock. Drop it to
/// release; the OS will also release automatically on process exit.
#[must_use = "the lock is released when this guard is dropped -- bind it to a `_lock` variable that lives for the critical section"]
pub(crate) struct ProvisionLock {
    // Kept alive; drop calls `unlock` implicitly via the OS.
    file: File,
    // Read by the cfg(test) `path()` getter only. In non-test builds
    // the field is still needed so error diagnostics can name the
    // lockfile path -- silence dead_code accordingly.
    #[cfg_attr(not(test), expect(dead_code, reason = "diagnostics-only field"))]
    path: PathBuf,
}

impl ProvisionLock {
    /// Acquire an exclusive lock on `<manifest_root>/.edgezero/provision.lock`.
    /// Blocks until another concurrent invocation releases; the block
    /// is a bounded wait -- provision writes are fast (single-digit
    /// milliseconds to seconds for large fixtures), so the block is
    /// bounded by the peer's work.
    ///
    /// The `.edgezero/` parent dir is created lazily if absent (it's
    /// the same dir Axum writes `.edgezero/.env` and the local config
    /// JSON blobs into; nesting the lock inside keeps
    /// operator-visible provision state in one place).
    ///
    /// Returns Ok on lock acquisition. Errors surface the underlying
    /// filesystem error with the lockfile path so operators can
    /// diagnose disk-full / permission issues.
    pub(crate) fn acquire(manifest_root: &Path) -> Result<Self, String> {
        let dot_edgezero = manifest_root.join(".edgezero");
        let path = dot_edgezero.join("provision.lock");
        // Reject a symlinked `.edgezero/` or `provision.lock` BEFORE
        // creating or opening anything. `.edgezero/` is gitignored, so a hostile or
        // careless tree can carry either link without it showing up in
        // review: `OpenOptions::create(true).write(true)` follows a
        // symlinked final component and CREATES the target if the link
        // dangles, so we would take an flock on -- and hold a writable
        // descriptor to -- a file outside the project tree.
        reject_symlink_components(
            manifest_root,
            &path,
            "the provision lock path `<project>/.edgezero/provision.lock`",
        )?;
        fs::create_dir_all(&dot_edgezero).map_err(|err| {
            format!(
                "failed to create {} for provision lock: {err}",
                dot_edgezero.display()
            )
        })?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                format!(
                    "failed to open provision lock at {}: {err} -- provision writes to edgezero.toml + .env / .dev.vars are guarded by this file; check the parent directory is writable",
                    path.display()
                )
            })?;
        file.lock_exclusive().map_err(|err| {
            format!(
                "failed to acquire exclusive provision lock on {}: {err} -- another `edgezero provision` or `edgezero config push` may be running against the same tree",
                path.display()
            )
        })?;
        Ok(Self { file, path })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProvisionLock {
    fn drop(&mut self) {
        // The OS releases the lock on descriptor close, but call
        // `unlock` explicitly so double-close-in-drop doesn't leave a
        // stray flock reference in error paths.
        drop(FileExt::unlock(&self.file));
        // Note: we do NOT delete the lock file. Deletion races with
        // a peer that has the descriptor open (they'd hold a lock on
        // a nameless file for the rest of their lifetime). Leaving
        // the sentinel is safe -- flock semantics are per-descriptor.
    }
}

#[cfg(test)]
mod tests {
    use super::ProvisionLock;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn acquire_creates_lockfile_under_dot_edgezero_dir() {
        let temp = TempDir::new().expect("tempdir");
        let lock = ProvisionLock::acquire(temp.path()).expect("acquire");
        assert!(
            lock.path().exists(),
            "lockfile must be created: {}",
            lock.path().display()
        );
        assert_eq!(
            lock.path().file_name().and_then(|name| name.to_str()),
            Some("provision.lock")
        );
        assert!(
            lock.path().parent().and_then(Path::file_name) == Some(OsStr::new(".edgezero")),
            "lockfile must sit inside .edgezero/: {}",
            lock.path().display()
        );
    }

    /// Regression: `.edgezero/`
    /// is gitignored, so a symlinked one never shows up in review.
    /// `create_dir_all` + `OpenOptions::create` would follow it and we
    /// would hold a writable descriptor outside the project tree.
    #[cfg(unix)]
    #[test]
    fn acquire_refuses_a_symlinked_dot_edgezero_dir() {
        use std::fs::create_dir_all;
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("project");
        let outside = temp.path().join("outside");
        create_dir_all(&root).expect("mkdir project");
        create_dir_all(&outside).expect("mkdir outside");
        symlink(&outside, root.join(".edgezero")).expect("symlink");

        // `expect_err` would need `ProvisionLock: Debug`; the guard
        // wraps a live descriptor and has no reason to derive it.
        let Err(err) = ProvisionLock::acquire(&root) else {
            panic!("symlinked .edgezero must be refused")
        };
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !outside.join("provision.lock").exists(),
            "the refused acquire must not have created a lockfile outside the project"
        );
    }

    /// The lockfile itself is the other half: a symlinked
    /// `provision.lock` inside a legitimate `.edgezero/` would have
    /// `OpenOptions::create(true).write(true)` create/open the target.
    #[cfg(unix)]
    #[test]
    fn acquire_refuses_a_symlinked_lockfile() {
        use std::fs::create_dir_all;
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("project");
        create_dir_all(root.join(".edgezero")).expect("mkdir .edgezero");
        let victim = temp.path().join("victim");
        symlink(&victim, root.join(".edgezero/provision.lock")).expect("symlink");

        let Err(err) = ProvisionLock::acquire(&root) else {
            panic!("symlinked provision.lock must be refused")
        };
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !victim.exists(),
            "the refused acquire must not have created the link target"
        );
    }

    #[test]
    fn two_concurrent_acquires_serialise_via_the_lock() {
        let temp = TempDir::new().expect("tempdir");
        let root_a = temp.path().to_path_buf();
        let root_b = root_a.clone();

        let (tx, rx) = mpsc::channel();
        // Thread A takes the lock and holds it for 50ms.
        let handle_a = thread::spawn(move || {
            let lock = ProvisionLock::acquire(&root_a).expect("A acquire");
            tx.send(()).expect("signal");
            thread::sleep(Duration::from_millis(50));
            drop(lock);
        });
        // Wait until A has definitely acquired.
        rx.recv().expect("await A");
        let start = Instant::now();
        // Thread B tries; must block until A releases.
        let lock_b = ProvisionLock::acquire(&root_b).expect("B acquire");
        let elapsed = start.elapsed();
        drop(lock_b);
        handle_a.join().expect("join A");
        assert!(
            elapsed >= Duration::from_millis(30),
            "B must have waited on A's lock; only waited {elapsed:?}"
        );
    }

    #[test]
    fn dropping_the_lock_releases_it_for_the_next_acquire() {
        let temp = TempDir::new().expect("tempdir");
        let lock = ProvisionLock::acquire(temp.path()).expect("acquire 1");
        drop(lock);
        // Should be immediately available.
        let start = Instant::now();
        let _lock2 = ProvisionLock::acquire(temp.path()).expect("acquire 2");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "second acquire must not block after drop"
        );
    }
}
