use crate::{
    Result,
    archive::{checked_seek, copy_exact, parse},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub fn extract_binary<R: Read + Seek>(archive: &mut R, output_parent: &Path) -> Result<PathBuf> {
    let parsed = parse(archive)?;
    checked_seek(archive, SeekFrom::Start(parsed.binary_offset))?;
    atomic_publish(
        output_parent,
        "app-cli",
        0o755,
        |output| copy_exact(archive, output, parsed.binary_size),
        |path| verify_file(path, parsed.binary_size, 0o755),
    )
}

#[cfg(not(unix))]
pub fn extract_binary<R: Read + Seek>(_archive: &mut R, _output_parent: &Path) -> Result<PathBuf> {
    Err("atomic executable extraction requires Unix".into())
}

#[cfg(unix)]
pub(crate) fn validate_output_parent(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let canonical_parent = path.canonicalize().map_err(|error| error.to_string())?;
    require(
        canonical_parent.as_os_str() == path.as_os_str(),
        "output parent is not canonical",
    )?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    require(
        metadata.file_type().is_dir(),
        "output parent is not a directory",
    )?;
    require(
        metadata.permissions().mode() & 0o222 != 0,
        "output parent is not writable",
    )?;
    require(directory_is_empty(path)?, "output parent is not empty")
}

#[cfg(not(unix))]
pub(crate) fn validate_output_parent(_path: &Path) -> Result<()> {
    Err("atomic output validation requires Unix".into())
}

#[cfg(unix)]
pub(crate) fn atomic_publish(
    output_parent: &Path,
    basename: &str,
    mode: u32,
    write: impl FnOnce(&mut File) -> Result<()>,
    validate: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    validate_output_parent(output_parent)?;
    require(
        !basename.is_empty() && !basename.contains('/') && !basename.contains('\\'),
        "invalid output basename",
    )?;
    let final_path = output_parent.join(basename);
    let temporary_path = output_parent.join(format!(".{basename}.tmp"));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_path)
        .map_err(|error| error.to_string())?;
    let mut owned_path = match OwnedPath::new(temporary_path.clone(), &output) {
        Ok(owned_path) => owned_path,
        Err(error) => {
            drop(output);
            return Err(with_cleanup_error(
                error,
                cleanup_owned_path(&temporary_path),
            ));
        }
    };

    let staged = (|| -> Result<u64> {
        write(&mut output)?;
        output.flush().map_err(|error| error.to_string())?;
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        let size = output.metadata().map_err(|error| error.to_string())?.len();
        let identity = owned_path.snapshot(&output)?;
        owned_path.verify(&identity)?;
        verify_file(&temporary_path, size, mode)?;
        validate(&temporary_path)?;
        owned_path.verify(&identity)?;
        Ok(size)
    })();
    drop(output);
    let size = match staged {
        Ok(size) => size,
        Err(error) => return Err(with_cleanup_error(error, owned_path.cleanup())),
    };

    if let Err(error) = publish_no_replace(&temporary_path, &final_path) {
        return Err(with_cleanup_error(error, owned_path.cleanup()));
    }
    owned_path.transfer(final_path.clone());
    let published = (|| -> Result<()> {
        let identity = owned_path.snapshot_path()?;
        owned_path.verify(&identity)?;
        verify_file(&final_path, size, mode)?;
        owned_path.verify(&identity)
    })();
    if let Err(error) = published {
        return Err(with_cleanup_error(error, owned_path.cleanup()));
    }
    owned_path.disarm();
    Ok(final_path)
}

#[cfg(not(unix))]
pub(crate) fn atomic_publish(
    _output_parent: &Path,
    _basename: &str,
    _mode: u32,
    _write: impl FnOnce(&mut File) -> Result<()>,
    _validate: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    Err("atomic publication requires Unix".into())
}

#[cfg(unix)]
fn directory_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .next()
        .transpose()
        .map_err(|error| error.to_string())?
        .is_none())
}

#[cfg(unix)]
fn verify_file(path: &Path, expected_size: u64, expected_mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    require(
        metadata.file_type().is_file(),
        "output is not a regular file",
    )?;
    require(metadata.len() == expected_size, "output size changed")?;
    require(
        metadata.permissions().mode() & 0o7777 == expected_mode,
        "output mode is incorrect",
    )?;
    require(metadata.nlink() == 1, "output has multiple links")
}

pub(crate) fn cleanup_owned_path(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to remove owned path {}: {error}",
                path.display()
            ));
        }
    }

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!(
            "owned path still exists after cleanup: {}",
            path.display()
        )),
        Err(error) => Err(format!(
            "failed to verify owned path cleanup {}: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn with_cleanup_error(error: String, cleanup: Result<()>) -> String {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; cleanup failed: {cleanup_error}"),
    }
}

#[cfg(target_os = "linux")]
fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;

    unsafe extern "C" {
        fn renameat2(
            old_directory: i32,
            old_path: *const std::ffi::c_char,
            new_directory: i32,
            new_path: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    let source =
        CString::new(source.as_os_str().as_bytes()).map_err(|_| "temporary path contains NUL")?;
    let destination =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| "output path contains NUL")?;
    // The pinned Bookworm/Linux runtime requires renameat2; unsupported syscall or
    // filesystem errors fail closed because Linux intentionally has no fallback.
    // Both paths are fixed children of the validated output directory.
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::hard_link(source, destination).map_err(|error| error.to_string())?;
    if let Err(error) = fs::remove_file(source) {
        return Err(with_cleanup_error(
            error.to_string(),
            cleanup_owned_path(destination),
        ));
    }
    Ok(())
}

#[cfg(unix)]
struct OwnedPath {
    path: Option<PathBuf>,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    links: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[cfg(unix)]
impl OwnedPath {
    fn new(path: PathBuf, file: &File) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = file.metadata().map_err(|error| error.to_string())?;
        Ok(Self {
            path: Some(path),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn cleanup(&mut self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let path = self.path.take().ok_or("owned path guard is disarmed")?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(metadata)
                if !metadata.file_type().is_file()
                    || metadata.dev() != self.device
                    || metadata.ino() != self.inode =>
            {
                return Err(format!("owned path identity changed: {}", path.display()));
            }
            Ok(_) => {}
        }
        cleanup_owned_path(&path)
    }

    fn verify(&self, expected: &FileIdentity) -> Result<()> {
        let path = self.path.as_ref().ok_or("owned path guard is disarmed")?;
        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        require(
            metadata.file_type().is_file() && FileIdentity::from(&metadata) == *expected,
            &format!("owned path identity changed: {}", path.display()),
        )
    }

    fn snapshot(&self, file: &File) -> Result<FileIdentity> {
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        let identity = FileIdentity::from(&metadata);
        require(
            metadata.file_type().is_file()
                && identity.device == self.device
                && identity.inode == self.inode,
            "owned file identity changed while staging",
        )?;
        Ok(identity)
    }

    fn snapshot_path(&self) -> Result<FileIdentity> {
        let path = self.path.as_ref().ok_or("owned path guard is disarmed")?;
        let file = File::open(path).map_err(|error| error.to_string())?;
        self.snapshot(&file)
    }

    fn transfer(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

#[cfg(unix)]
impl Drop for OwnedPath {
    fn drop(&mut self) {
        if self.path.is_some() {
            let _ = self.cleanup();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "edgezero-provenance-extract-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn review_atomic_publish_rejects_same_inode_mutation_after_validation() {
        let parent = TempDir::new();
        let final_path = parent.0.join("artifact");
        let error = atomic_publish(
            &parent.0,
            "artifact",
            0o644,
            |output| {
                output
                    .write_all(b"original")
                    .map_err(|error| error.to_string())
            },
            |path| {
                fs::write(path, b"mutated!").map_err(|error| error.to_string())?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error.contains("identity changed"),
            "unexpected error: {error}"
        );
        assert!(!final_path.exists());
        assert!(fs::read_dir(&parent.0).unwrap().next().is_none());
    }

    #[test]
    fn review_owned_path_drop_preserves_a_replacement() {
        let parent = TempDir::new();
        let path = parent.0.join("owned");
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let owned = OwnedPath::new(path.clone(), &file).unwrap();
        drop(file);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();

        drop(owned);

        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }
}

#[cfg(unix)]
fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}
