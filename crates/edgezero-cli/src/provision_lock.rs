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

use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf, absolute};

use fs4::fs_std::FileExt;

use crate::path_safety::reject_symlink_components;

/// Env var advertising the absolute lock path a lock-holding process tree
/// already owns. `run_deploy` holds the lock and passes this to the deploy
/// subprocess via `Command::env` (safe, child-only); a nested `<app>-cli
/// provision` / `config push` invoked by that deploy inherits it and
/// BORROWS the lock instead of dead-locking on the parent's non-reentrant
/// file lock. An UNRELATED concurrent provision (a separate process tree,
/// no inherited env) still serialises on the OS file lock.
pub(crate) const LOCK_ENV: &str = "EDGEZERO_PROVISION_LOCK";

/// Guard object representing an active advisory lock. Drop it to
/// release; the OS will also release automatically on process exit.
#[must_use = "the lock is released when this guard is dropped -- bind it to a `_lock` variable that lives for the critical section"]
pub(crate) struct ProvisionLock {
    // The OS file lock this guard holds: EITHER the main `provision.lock`
    // (owner) OR the `provision.borrow.lock` sibling lock (borrower). Drop
    // releases whichever it is.
    file: File,
    // `true` when this guard BORROWS an ancestor deploy's advertised lock
    // (holding the sibling lock for intra-tree serialisation) rather than
    // owning the main lock. Read by the cfg(test) `owns_os_lock` getter.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the cfg(test) owns_os_lock getter")
    )]
    borrowed: bool,
    // Read by the cfg(test) `path()` getter only. In non-test builds
    // the field is still needed so error diagnostics can name the
    // lockfile path -- silence dead_code accordingly.
    #[cfg_attr(not(test), expect(dead_code, reason = "diagnostics-only field"))]
    path: PathBuf,
}

impl ProvisionLock {
    /// Absolute `.edgezero/provision.lock` path for `manifest_root`, for
    /// advertising to a deploy subprocess via [`LOCK_ENV`].
    pub(crate) fn lock_path_for(manifest_root: &Path) -> PathBuf {
        let path = manifest_root.join(".edgezero").join("provision.lock");
        absolute(&path).unwrap_or(path)
    }

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
        // Reentrancy: if an ANCESTOR process (a `deploy` holding the lock)
        // advertised this exact path via `LOCK_ENV`, we MAY borrow it -- but
        // only after PROVING a lock is actually held. The env is inherited
        // and could be forged, stale (the ancestor already released), or
        // leaked to an unrelated child; trusting it blindly would let a
        // concurrent provision/push skip serialisation. So attempt a
        // NON-BLOCKING lock: if it would block, a real holder exists and we
        // borrow; if we acquire it, the advertisement was false and we KEEP
        // the lock (own it) rather than run unserialized.
        let abs = absolute(&path).unwrap_or_else(|_| path.clone());
        if env::var_os(LOCK_ENV).is_some_and(|held| Path::new(&held) == abs) {
            let acquired = file.try_lock_exclusive().map_err(|err| {
                format!(
                    "failed to test the advertised provision lock on {}: {err}",
                    path.display()
                )
            })?;
            if acquired {
                // No real holder despite the advertisement -- own it.
                return Ok(Self {
                    file,
                    borrowed: false,
                    path,
                });
            }
            // A real holder exists (our ancestor deploy). Don't take the
            // main lock (that would self-deadlock the composed deploy), but
            // DO serialise against SIBLING borrowers via a SEPARATE lock, so
            // two provisions the same deploy spawns can't rewrite the same
            // manifests / env files concurrently. Release the failed
            // non-blocking attempt fd first.
            drop(file);
            let sibling_path = dot_edgezero.join("provision.borrow.lock");
            reject_symlink_components(
                manifest_root,
                &sibling_path,
                "the sibling provision lock path `<project>/.edgezero/provision.borrow.lock`",
            )?;
            let sibling = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&sibling_path)
                .map_err(|err| {
                    format!(
                        "failed to open sibling provision lock at {}: {err}",
                        sibling_path.display()
                    )
                })?;
            sibling.lock_exclusive().map_err(|err| {
                format!(
                    "failed to acquire the sibling provision lock on {}: {err} -- another provision spawned by the same deploy may be running",
                    sibling_path.display()
                )
            })?;
            return Ok(Self {
                file: sibling,
                borrowed: true,
                path,
            });
        }
        file.lock_exclusive().map_err(|err| {
            format!(
                "failed to acquire exclusive provision lock on {}: {err} -- another `edgezero provision` or `edgezero config push` may be running against the same tree",
                path.display()
            )
        })?;
        Ok(Self {
            file,
            borrowed: false,
            path,
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `true` when this guard OWNS the main OS lock; `false` when it borrows
    /// an ancestor's advertised lock (holding the sibling lock instead).
    #[cfg(test)]
    pub(crate) fn owns_os_lock(&self) -> bool {
        !self.borrowed
    }
}

impl Drop for ProvisionLock {
    fn drop(&mut self) {
        // The OS releases the lock on descriptor close, but call `unlock`
        // explicitly so double-close-in-drop doesn't leave a stray flock
        // reference in error paths. Releases whichever lock this guard
        // holds -- the main lock (owner) or the sibling lock (borrower).
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
    fn acquire_borrows_only_when_a_real_lock_backs_the_advertisement() {
        // With `LOCK_ENV` advertising THIS path AND a real lock held (the
        // first acquire owns it), a second acquire BORROWS -- no OS lock, no
        // block -- so a composed deploy's nested provision doesn't self-
        // dead-lock.
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let advert = ProvisionLock::lock_path_for(temp.path());
        let _env = EnvOverride::set(super::LOCK_ENV, advert.as_os_str());
        let start = Instant::now();
        let l1 = ProvisionLock::acquire(temp.path()).expect("first acquire");
        assert!(
            l1.owns_os_lock(),
            "the first acquire must OWN the lock (nothing held it yet)"
        );
        let l2 = ProvisionLock::acquire(temp.path()).expect("second acquire");
        assert!(
            !l2.owns_os_lock(),
            "the second acquire must BORROW the lock the first proved held"
        );
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "the borrow must not block on the owner"
        );
    }

    #[test]
    fn forged_lock_env_does_not_bypass_serialization() {
        // A matching `LOCK_ENV` with NO real lock behind it (forged, stale,
        // or leaked to an unrelated child) must NOT let acquire skip the OS
        // lock -- it takes the real lock instead, so serialization holds.
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let advert = ProvisionLock::lock_path_for(temp.path());
        let _env = EnvOverride::set(super::LOCK_ENV, advert.as_os_str());
        // No lock is held anywhere; the env is the only "proof".
        let lock = ProvisionLock::acquire(temp.path()).expect("acquire");
        assert!(
            lock.owns_os_lock(),
            "a forged advertisement must not be trusted: acquire must own the real lock"
        );
    }

    #[test]
    fn sibling_borrowers_serialise_against_each_other() {
        // Two provisions the SAME deploy spawns both borrow the parent's
        // advertised lock -- but they must still serialise with EACH OTHER
        // (via the sibling lock) so they can't rewrite the same manifests
        // concurrently. The second borrower must BLOCK on the first.
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let advert = ProvisionLock::lock_path_for(temp.path());
        let _env = EnvOverride::set(super::LOCK_ENV, advert.as_os_str());
        // The "deploy parent" owns the main lock.
        let owner = ProvisionLock::acquire(temp.path()).expect("owner");
        assert!(owner.owns_os_lock(), "owner holds the main lock");

        let root = temp.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let root_a = root.clone();
        let handle_a = thread::spawn(move || {
            let lock = ProvisionLock::acquire(&root_a).expect("sibling A");
            assert!(!lock.owns_os_lock(), "A borrows (holds the sibling lock)");
            tx.send(()).expect("signal");
            thread::sleep(Duration::from_millis(50));
            drop(lock);
        });
        rx.recv().expect("await A");
        let start = Instant::now();
        // Sibling B must block on A's sibling lock.
        let lock_b = ProvisionLock::acquire(&root).expect("sibling B");
        let elapsed = start.elapsed();
        assert!(!lock_b.owns_os_lock(), "B borrows too");
        drop(lock_b);
        handle_a.join().expect("join A");
        assert!(
            elapsed >= Duration::from_millis(30),
            "B must serialise behind A's sibling lock; only waited {elapsed:?}"
        );
    }

    #[test]
    fn acquire_does_not_borrow_when_advertised_path_differs() {
        // A `LOCK_ENV` for a DIFFERENT project must not make this acquire
        // borrow -- it takes the real OS lock (and creates the lockfile).
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let other = TempDir::new().expect("tempdir");
        let _env = EnvOverride::set(
            super::LOCK_ENV,
            ProvisionLock::lock_path_for(other.path()).as_os_str(),
        );
        let lock = ProvisionLock::acquire(temp.path()).expect("real acquire");
        assert!(
            lock.path().exists(),
            "a mismatched advertisement must not borrow: the real lockfile is created"
        );
    }

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
