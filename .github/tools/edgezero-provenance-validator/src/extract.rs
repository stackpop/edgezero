use crate::{
    Result,
    archive::{checked_seek, copy_exact, parse},
};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub fn extract_binary<R: Read + Seek>(archive: &mut R, output_parent: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let canonical_parent = output_parent
        .canonicalize()
        .map_err(|error| error.to_string())?;
    require(
        canonical_parent.as_os_str() == output_parent.as_os_str(),
        "output parent is not canonical",
    )?;
    require(
        fs::symlink_metadata(output_parent)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_dir(),
        "output parent is not a directory",
    )?;
    require(
        directory_is_empty(output_parent)?,
        "output parent is not empty",
    )?;

    let parsed = parse(archive)?;
    checked_seek(archive, SeekFrom::Start(parsed.binary_offset))?;

    let final_path = output_parent.join("app-cli");
    let temporary_path = output_parent.join(".app-cli.tmp");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_path)
        .map_err(|error| error.to_string())?;
    let mut owned_path = OwnedPath::new(temporary_path.clone());

    let staged = (|| -> Result<()> {
        copy_exact(archive, &mut output, parsed.binary_size)?;
        output.flush().map_err(|error| error.to_string())?;
        output
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        verify_file(&temporary_path, parsed.binary_size)
    })();
    drop(output);
    if let Err(error) = staged {
        return Err(with_cleanup_error(error, owned_path.cleanup()));
    }

    if let Err(error) = publish_no_replace(&temporary_path, &final_path) {
        return Err(with_cleanup_error(error, owned_path.cleanup()));
    }
    owned_path.transfer(final_path.clone());
    if let Err(error) = verify_file(&final_path, parsed.binary_size) {
        return Err(with_cleanup_error(error, owned_path.cleanup()));
    }
    owned_path.disarm();

    Ok(final_path)
}

#[cfg(not(unix))]
pub fn extract_binary<R: Read + Seek>(_archive: &mut R, _output_parent: &Path) -> Result<PathBuf> {
    Err("atomic executable extraction requires Unix".into())
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
fn verify_file(path: &Path, expected_size: u64) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    require(
        metadata.file_type().is_file(),
        "output is not a regular file",
    )?;
    require(metadata.len() == expected_size, "output size changed")?;
    require(
        metadata.permissions().mode() & 0o7777 == 0o755,
        "output mode is not 0755",
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
}

#[cfg(unix)]
impl OwnedPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn cleanup(&mut self) -> Result<()> {
        let path = self.path.take().ok_or("owned path guard is disarmed")?;
        cleanup_owned_path(&path)
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
        if std::thread::panicking()
            && let Some(path) = self.path.take()
        {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}
