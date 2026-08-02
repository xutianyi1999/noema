//! Shared filesystem helpers for the storage layer.

use std::{fs, path::Path};

use walkdir::WalkDir;

use crate::error::AppError;

/// Recursively copy one file or directory tree, never following symlinks.
pub(crate) fn copy_path(source: &Path, destination: &Path) -> Result<(), AppError> {
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in WalkDir::new(source).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(source)?;
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(target)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(entry.path(), target)?;
            }
        }
    } else if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

/// Write a file atomically: the content lands in a temporary file in the
/// same directory and is renamed over the target.
pub(super) fn write_atomic(path: &Path, content: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, content)?;
    temporary
        .persist(path)
        .map_err(|error| AppError::Io(error.error))?;
    Ok(())
}
