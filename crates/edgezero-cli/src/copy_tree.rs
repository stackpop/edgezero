//! Small internal recursive directory copy used by `provision
//! --local --dry-run` to stage mutable adapter paths. No new
//! workspace dep — built on `std::fs` only. Preserves regular
//! files and re-creates directories; symlinks and special files
//! are out of scope per spec §"Dry-run".

use std::fs;
use std::fs::FileType;
use std::io;
use std::path::Path;

/// True only for regular files (not directories, symlinks, fifos,
/// sockets, block/character devices). Regular-files-only IS the
/// spec §"Dry-run" semantic — clippy's warning that callers "often
/// forget `is_file()` excludes symlinks" is inverted for us: we
/// WANT that exclusion. Wrapping at one call site keeps
/// `copy_dir_recursive` free of the suppression.
#[expect(
    clippy::filetype_is_file,
    reason = "spec §\"Dry-run\": regular-files-only copy semantics — symlink/special-file exclusion is the intent, not a bug"
)]
fn is_regular_file(file_type: FileType) -> bool {
    file_type.is_file()
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for read_result in fs::read_dir(src)? {
        let entry = read_result?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if is_regular_file(file_type) {
            fs::copy(&src_path, &dst_path)?;
        } else {
            // Symlinks and special files (fifos, sockets, block/char
            // devices) are intentionally skipped per spec §"Dry-run"
            // — dry-run must not follow symlinks off the staged tree,
            // and adapter source trees shouldn't contain special files.
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn copies_nested_files_and_dirs() {
        let src = TempDir::new().unwrap();
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::write(src.path().join("a/top.toml"), "x = 1").unwrap();
        fs::write(src.path().join("a/b/nested.toml"), "y = 2").unwrap();

        let dst = TempDir::new().unwrap();
        copy_dir_recursive(src.path(), dst.path()).unwrap();

        assert_eq!(
            fs::read_to_string(dst.path().join("a/top.toml")).unwrap(),
            "x = 1"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("a/b/nested.toml")).unwrap(),
            "y = 2"
        );
    }

    #[test]
    fn missing_src_returns_error() {
        let dst = TempDir::new().unwrap();
        assert!(copy_dir_recursive(Path::new("/nonexistent"), dst.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn skips_symlinks_and_only_copies_regular_files() {
        use std::os::unix::fs::symlink;

        let src = TempDir::new().unwrap();
        fs::write(src.path().join("real.toml"), "keep").unwrap();
        symlink(src.path().join("real.toml"), src.path().join("link.toml")).unwrap();

        let dst = TempDir::new().unwrap();
        copy_dir_recursive(src.path(), dst.path()).unwrap();

        assert!(dst.path().join("real.toml").exists());
        assert!(
            !dst.path().join("link.toml").exists(),
            "symlink must not be reproduced under the staged tree"
        );
    }
}
