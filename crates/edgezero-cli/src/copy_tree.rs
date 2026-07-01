//! Small internal recursive directory copy used by `provision
//! --local --dry-run` to stage mutable adapter paths. No new
//! workspace dep — built on `std::fs` only. Preserves regular
//! files and re-creates directories; symlinks and special files
//! are out of scope per spec §"Dry-run".

use std::fs;
use std::io;
use std::path::Path;

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for read_result in fs::read_dir(src)? {
        let entry = read_result?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_symlink() {
            // Symlinks intentionally skipped per spec §"Dry-run".
        } else {
            // Regular files (and any other non-dir, non-symlink entry)
            // get copied. Special files won't appear in normal adapter
            // source trees; if one does, `fs::copy` will surface its
            // own error rather than silently drop it.
            fs::copy(&src_path, &dst_path)?;
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
}
