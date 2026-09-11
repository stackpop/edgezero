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
//! and opened without truncation so multiple runs share the sentinel
//! (and its flock identity) across time; the lock OWNER rewrites its
//! contents with a per-holder token (see [`LOCK_TOKEN_ENV`]) while
//! holding the lock, which never changes the file's identity.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf, absolute};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Env var carrying the per-holder TOKEN that authenticates the [`LOCK_ENV`]
/// advertisement. The owner writes a fresh token into the lock file while
/// holding the lock and advertises the SAME token here. A nested provision
/// borrows only when this env token matches the token currently in the lock
/// file -- proving the current holder is the very process that advertised.
/// A stale (ancestor already released, unrelated process now holds the
/// lock), leaked, or forged advertisement carries a token that no longer
/// matches the file, so the borrow is refused and the invocation serialises
/// on the real OS lock instead of bypassing an unrelated holder.
pub(crate) const LOCK_TOKEN_ENV: &str = "EDGEZERO_PROVISION_LOCK_TOKEN";

/// Env var carrying the borrow-tree DEPTH. A borrower serialises its file
/// writes against its CO-SIBLINGS on a depth-keyed sibling lock
/// (`provision.borrow.<depth>.lock`); a deploy advertises `depth + 1` to its
/// subprocess so a NESTED borrower serialises on a DIFFERENT file at the next
/// depth. This is what lets a THREE-level (or deeper) composed deploy hold
/// serialisation at EVERY level without a nested borrower dead-locking on the
/// single shared sibling file an ancestor already holds -- and, crucially,
/// makes a borrowing DEPLOY (which writes `fastly.toml` via `fastly compute
/// deploy`) serialise with its co-siblings instead of racing lock-free.
pub(crate) const SIBLING_DEPTH_ENV: &str = "EDGEZERO_PROVISION_SIBLING_DEPTH";

/// Read the inherited borrow depth (`SIBLING_DEPTH_ENV`), defaulting to 1 --
/// the first borrow level under a top-level main-lock owner.
fn inherited_sibling_depth() -> u32 {
    env::var(SIBLING_DEPTH_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|depth| *depth >= 1)
        .unwrap_or(1)
}

/// Process-lifetime counter making concurrently-generated tokens distinct.
static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint a token unique enough to distinguish this holder from any other:
/// pid + wall-clock nanos + a monotonic per-process sequence. This is an
/// ownership PROOF, not a secret -- a forged token with no real holder is
/// already caught (the non-blocking acquire succeeds and we own the lock),
/// so the token only needs to be unforgeable-by-accident across live
/// holders, which pid+time+seq satisfies.
/// Whether the advertised `LOCK_ENV` path and this process's calculated lock
/// path name the SAME lock file. A plain textual `==` is not enough: a
/// symlinked or otherwise aliased project root (e.g. `/tmp/proj` vs a symlink
/// `/tmp/link` -> `/tmp/proj`) opens the SAME inode under two spellings, and a
/// borrower that failed to recognise its ancestor's advertisement would block
/// on the very lock the ancestor holds -- a self-deadlock. Compare by resolved
/// identity (`canonicalize`), falling back to the textual check when either
/// path can't be resolved.
fn same_lock_target(advertised: &Path, calculated: &Path) -> bool {
    if advertised == calculated {
        return true;
    }
    match (fs::canonicalize(advertised), fs::canonicalize(calculated)) {
        (Ok(resolved_advertised), Ok(resolved_calculated)) => {
            resolved_advertised == resolved_calculated
        }
        (Err(_), _) | (_, Err(_)) => false,
    }
}

fn mint_token() -> String {
    let pid = process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{pid}-{nanos}-{seq}")
}

/// Overwrite the lock file's contents with `token` while holding the lock.
/// Truncates so a shorter new token can't leave a previous holder's
/// trailing bytes behind. FAIL-CLOSED: the caller ([`ProvisionLock::owned`])
/// turns an error here into a failed acquire, because a held lock with an
/// UNPERSISTED token would advertise a token no nested provision can read
/// back -- the nested borrow would fail authentication, block on the
/// parent's lock, and dead-lock the composed deploy.
fn write_token(file: &File, token: &str) -> io::Result<()> {
    let mut handle: &File = file;
    let len = u64::try_from(token.len()).unwrap_or(u64::MAX);
    handle.seek(SeekFrom::Start(0))?;
    handle.write_all(token.as_bytes())?;
    file.set_len(len)?;
    handle.flush()
}

/// Read the token the current lock holder wrote at `path`. Reads do not
/// require the advisory lock, so a borrower can read while the owner holds
/// it. Returns an empty string on any read error (treated as "no match").
fn read_token(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|contents| contents.trim().to_owned())
        .unwrap_or_default()
}

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
    // The token that authenticates this held lock: minted+written when we
    // OWN the main lock, or the file token we validated when we BORROW.
    // `run_deploy` advertises it via `LOCK_TOKEN_ENV` so a nested provision
    // can prove the advertised holder is still the one holding the lock.
    token: String,
    // The depth `run_deploy` advertises to a deploy subprocess via
    // `SIBLING_DEPTH_ENV`: `1` when this guard OWNS the main lock (its
    // children are the first borrow level), or `held_depth + 1` when it holds
    // a depth-keyed sibling lock (its children serialise one level deeper).
    child_sibling_depth: u32,
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
        Self::acquire_inner(manifest_root)
    }

    /// Acquire for a DEPLOY. A deploy owns the main lock when top-level, or --
    /// when nested -- borrows the main lock AND takes a depth-keyed sibling
    /// lock, because `fastly compute deploy` writes `fastly.toml` (the
    /// `service_id`) and so must serialise with its co-siblings just like a
    /// provision. Depth-keying keeps a deeper nested borrower from
    /// dead-locking on this deploy's sibling lock.
    pub(crate) fn acquire_for_deploy(manifest_root: &Path) -> Result<Self, String> {
        Self::acquire_inner(manifest_root)
    }

    fn acquire_inner(manifest_root: &Path) -> Result<Self, String> {
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
        if env::var_os(LOCK_ENV).is_some_and(|held| same_lock_target(Path::new(&held), &abs)) {
            let acquired = file.try_lock_exclusive().map_err(|err| {
                format!(
                    "failed to test the advertised provision lock on {}: {err}",
                    path.display()
                )
            })?;
            if acquired {
                // No real holder despite the advertisement -- own it.
                return Self::owned(file, path);
            }
            // A real holder exists -- but is it the ancestor that advertised,
            // or an UNRELATED process that happens to hold this path (the
            // advertisement is stale/leaked)? Authenticate: the ancestor
            // wrote its token into the lock file while holding it and
            // advertised the SAME token via `LOCK_TOKEN_ENV`. Borrow ONLY
            // when the file token matches; otherwise the holder is not our
            // advertiser, so fall through to the blocking acquire below and
            // serialise on the REAL lock rather than bypass an unrelated one.
            let file_token = read_token(&path);
            let env_token = env::var(LOCK_TOKEN_ENV).unwrap_or_default();
            if !file_token.is_empty() && file_token == env_token {
                // Borrow the MAIN lock (never block on it -- that self-
                // deadlocks the composed deploy), but serialise this borrower's
                // file writes against its CO-SIBLINGS on a DEPTH-KEYED sibling
                // lock. BOTH provisions AND deploys take it: a borrowing deploy
                // writes `fastly.toml` (`fastly compute deploy` records the
                // service_id), so it MUST serialise too -- a lock-free deploy
                // borrower would race a co-sibling provision and lose an edit.
                // Depth-keying (not one shared sibling file) is what keeps a
                // NESTED borrower from dead-locking on an ancestor's sibling
                // lock.
                let sibling_guard = Self::borrow_via_sibling(
                    manifest_root,
                    &dot_edgezero,
                    path.clone(),
                    file_token,
                )?;
                // RE-AUTHENTICATE after the sibling lock. Acquiring it may have
                // QUEUED behind a co-sibling; while we waited the advertising
                // ancestor could have EXITED (releasing the main lock) and an
                // UNRELATED process could have acquired it and stamped its own
                // token. Proceeding on the pre-wait authentication would let us
                // race that new, unrelated main-lock owner. If the main token no
                // longer matches the advertisement, drop the borrow and OWN the
                // main lock (block) -- serialising behind the new owner instead.
                let current_token = read_token(&path);
                if !current_token.is_empty() && current_token == env_token {
                    return Ok(sibling_guard);
                }
                drop(sibling_guard);
            }
        }
        // Own the main lock: either no advertisement matched, or a matching
        // advertisement failed authentication (stale/unrelated holder) so we
        // block here until it releases rather than borrow past it.
        file.lock_exclusive().map_err(|err| {
            format!(
                "failed to acquire exclusive provision lock on {}: {err} -- another `edgezero provision` or `edgezero config push` may be running against the same tree",
                path.display()
            )
        })?;
        Self::owned(file, path)
    }

    /// Build an OWNER guard: mint a token, stamp it into the (already
    /// locked) file so a deploy spawned from here can advertise it, and
    /// record it for [`token`](Self::token).
    ///
    /// FAIL-CLOSED: if the token can't be persisted, return an error rather
    /// than a guard. Holding the lock while advertising a token the file
    /// doesn't carry would make every nested provision fail authentication,
    /// block on this lock, and dead-lock the composed deploy.
    fn owned(file: File, path: PathBuf) -> Result<Self, String> {
        let token = mint_token();
        write_token(&file, &token).map_err(|err| {
            format!(
                "failed to persist the provision lock token to {}: {err} -- refusing to hold the lock with an unpersisted token, which would dead-lock a nested provision that cannot authenticate the borrow",
                path.display()
            )
        })?;
        Ok(Self {
            file,
            borrowed: false,
            path,
            token,
            // A main-lock owner's children are the FIRST borrow level.
            child_sibling_depth: 1,
        })
    }

    /// Build a BORROWER guard for an authenticated ancestor deploy. Doesn't
    /// take the main lock (that would self-deadlock the composed deploy) but
    /// DOES hold a DEPTH-KEYED sibling lock (`provision.borrow.<depth>.lock`),
    /// so file writers spawned by the SAME deploy (co-siblings at this depth)
    /// serialise, while a MORE-nested borrower serialises on the NEXT depth's
    /// file and so never blocks on this one -- the reentrant, race-free
    /// replacement for a single shared sibling file.
    fn borrow_via_sibling(
        manifest_root: &Path,
        dot_edgezero: &Path,
        path: PathBuf,
        token: String,
    ) -> Result<Self, String> {
        let depth = inherited_sibling_depth();
        let sibling_path = dot_edgezero.join(format!("provision.borrow.{depth}.lock"));
        reject_symlink_components(
            manifest_root,
            &sibling_path,
            "the depth-keyed sibling provision lock path `<project>/.edgezero/provision.borrow.<depth>.lock`",
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
        Ok(Self {
            file: sibling,
            borrowed: true,
            path,
            token,
            // A borrower's own children serialise one level deeper.
            child_sibling_depth: depth.saturating_add(1),
        })
    }

    /// The borrow depth `run_deploy` advertises to a deploy subprocess via
    /// [`SIBLING_DEPTH_ENV`], so a nested provision / config push / deploy
    /// serialises on the correct depth-keyed sibling lock. `1` under a
    /// main-lock owner; one deeper than the depth this guard itself holds.
    pub(crate) fn child_sibling_depth(&self) -> u32 {
        self.child_sibling_depth
    }

    /// The token authenticating this held lock, for `run_deploy` to
    /// advertise via [`LOCK_TOKEN_ENV`] alongside the lock path.
    pub(crate) fn token(&self) -> &str {
        &self.token
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
        // The owner stamped a token; advertise it so the borrow authenticates.
        let _tok = EnvOverride::set(super::LOCK_TOKEN_ENV, OsStr::new(l1.token()));
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
    fn stale_token_does_not_bypass_an_unrelated_holder() {
        // LOCK_ENV matches the path AND a real lock is held, but the
        // advertised TOKEN does NOT match the token the holder wrote -- i.e.
        // the advertisement is stale/leaked and the current holder is
        // unrelated. acquire must NOT borrow past it; it must serialise on
        // the real OS lock (block until the holder releases, then own it).
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let advert = ProvisionLock::lock_path_for(temp.path());
        let _env = EnvOverride::set(super::LOCK_ENV, advert.as_os_str());
        let owner = ProvisionLock::acquire(temp.path()).expect("owner");
        assert!(owner.owns_os_lock(), "owner holds the main lock");
        // Advertise a token that does NOT match the owner's.
        let _tok = EnvOverride::set(
            super::LOCK_TOKEN_ENV,
            OsStr::new("stale-token-does-not-match"),
        );

        let root = temp.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let lock = ProvisionLock::acquire(&root).expect("acquire");
            let owns = lock.owns_os_lock();
            tx.send(owns).expect("signal");
            drop(lock);
        });
        // A stale token must NOT let the acquire return while the owner still
        // holds the lock -- a borrow would return immediately.
        let early = rx.recv_timeout(Duration::from_millis(150));
        assert!(
            early.is_err(),
            "a stale token must block on the unrelated holder, not borrow past it"
        );
        drop(owner);
        let owns = rx.recv().expect("await acquire");
        handle.join().expect("join");
        assert!(
            owns,
            "after the holder released, the acquire must OWN the real lock (never borrow on a stale token)"
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
        // Advertise the owner's token so both siblings authenticate the borrow.
        let _tok = EnvOverride::set(super::LOCK_TOKEN_ENV, OsStr::new(owner.token()));

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
    fn deploy_borrower_serialises_cosiblings_but_not_deeper_nesting() {
        // A composed deploy: the owner holds the main lock; a nested DEPLOY
        // borrows via `acquire_for_deploy` and -- unlike the earlier lock-free
        // behaviour -- DOES take a depth-keyed sibling lock, because `fastly
        // compute deploy` writes `fastly.toml`. A CO-SIBLING file writer at the
        // SAME depth must SERIALISE behind it (no race), while a MORE-nested
        // borrower one depth deeper (as the deploy advertises) must NOT block
        // on it (no composed-deploy deadlock).
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let advert = ProvisionLock::lock_path_for(temp.path());
        let _env = EnvOverride::set(super::LOCK_ENV, advert.as_os_str());
        let owner = ProvisionLock::acquire(temp.path()).expect("owner");
        assert!(owner.owns_os_lock(), "owner holds the main lock");
        let _tok = EnvOverride::set(super::LOCK_TOKEN_ENV, OsStr::new(owner.token()));

        // The nested deploy borrows the main lock AND takes the depth-1 sibling
        // lock; it advertises depth 2 to its own children.
        let deploy = ProvisionLock::acquire_for_deploy(temp.path()).expect("deploy borrow");
        assert!(
            !deploy.owns_os_lock(),
            "the nested deploy borrows the main lock"
        );
        assert_eq!(
            deploy.child_sibling_depth(),
            2,
            "deploy advertises the next depth to its children"
        );
        let root = temp.path().to_path_buf();

        // (a) A DEEPER-nested borrower (depth 2, as the deploy advertises to
        // its children) must NOT block on the deploy's depth-1 sibling lock --
        // it serialises on a different file.
        let depth_override = EnvOverride::set(super::SIBLING_DEPTH_ENV, OsStr::new("2"));
        let (tx, rx) = mpsc::channel();
        let deeper_root = root.clone();
        let deeper = thread::spawn(move || {
            let nested = ProvisionLock::acquire(&deeper_root).expect("deeper nested borrow");
            tx.send(()).expect("signal");
            drop(nested);
        });
        rx.recv_timeout(Duration::from_secs(3)).expect(
            "a deeper-nested borrow must not block on the deploy's sibling lock (deadlock)",
        );
        deeper.join().expect("join deeper");
        // Restore the default depth (1) for the co-sibling check below.
        drop(depth_override);

        // (b) A CO-SIBLING file writer at the SAME depth (1) MUST serialise
        // behind the deploy's depth-1 sibling lock.
        let start = Instant::now();
        let cosibling = thread::spawn(move || {
            let prov = ProvisionLock::acquire(&root).expect("co-sibling provision");
            drop(prov);
        });
        thread::sleep(Duration::from_millis(50));
        drop(deploy);
        cosibling.join().expect("join co-sibling");
        assert!(
            start.elapsed() >= Duration::from_millis(30),
            "the co-sibling must serialise behind the deploy's depth-1 sibling lock"
        );
        drop(owner);
    }

    #[cfg(unix)]
    #[test]
    fn borrows_through_an_aliased_symlinked_lock_path() {
        // The ancestor advertises `LOCK_ENV` via a SYMLINK spelling of the
        // project root; the child calculates the REAL spelling. A textual `==`
        // would not match, so the child would BLOCK on -- self-deadlock
        // against -- the very lock the ancestor holds. Identity comparison
        // (`same_lock_target`) must recognise the two spellings as one file and
        // BORROW instead.
        use crate::test_support::{EnvOverride, manifest_guard};
        use std::os::unix::fs::symlink;
        use std::sync::PoisonError;
        let _g = manifest_guard()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let real = TempDir::new().expect("tempdir");
        let link_parent = TempDir::new().expect("tempdir");
        let link = link_parent.path().join("link");
        symlink(real.path(), &link).expect("symlink");
        // Owner holds the lock via the REAL path; advertise via the SYMLINK.
        let owner = ProvisionLock::acquire(real.path()).expect("owner");
        let _env = EnvOverride::set(
            super::LOCK_ENV,
            ProvisionLock::lock_path_for(&link).as_os_str(),
        );
        let _tok = EnvOverride::set(super::LOCK_TOKEN_ENV, OsStr::new(owner.token()));
        // The child acquires via the real path in a thread: it must BORROW
        // (recognise the symlinked advertisement), not deadlock.
        let root = real.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let borrow = ProvisionLock::acquire(&root).expect("borrow");
            assert!(
                !borrow.owns_os_lock(),
                "the symlinked advertisement is recognised -> borrow, not own"
            );
            tx.send(()).expect("signal");
            drop(borrow);
        });
        rx.recv_timeout(Duration::from_secs(3))
            .expect("must borrow through the symlinked path, not deadlock");
        handle.join().expect("join borrow");
        drop(owner);
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
