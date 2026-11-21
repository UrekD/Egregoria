use std::fmt;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A failure during an atomic replacement, with the phase in which it occurred.
#[derive(Debug)]
pub enum AtomicWriteError {
    /// This write did not replace the destination.
    PreCommit(io::Error),
    /// The replacement is visible, but syncing its directory failed.
    PostCommitDurability(io::Error),
}

impl fmt::Display for AtomicWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreCommit(error) => write!(f, "could not replace file: {error}"),
            Self::PostCommitDurability(error) => {
                write!(f, "file was replaced but directory sync failed: {error}")
            }
        }
    }
}

impl std::error::Error for AtomicWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreCommit(error) | Self::PostCommitDurability(error) => Some(error),
        }
    }
}

/// Write and sync a new file beside the destination before atomically replacing it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    write_staged(path, |file| file.write_all(bytes))
}

pub(super) fn write_staged(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    static NEXT_NONCE: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (staging, mut file) =
        create_staging_file(path, &NEXT_NONCE).map_err(AtomicWriteError::PreCommit)?;
    let prepared = write(&mut file).and_then(|()| file.sync_all());
    drop(file);

    let result = prepared
        .map_err(AtomicWriteError::PreCommit)
        .and_then(|()| commit(&staging, path, parent));
    if matches!(result, Err(AtomicWriteError::PreCommit(_))) {
        let _ = fs::remove_file(staging);
    }
    result
}

fn create_staging_file(path: &Path, next_nonce: &AtomicU64) -> io::Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "destination needs a file name"))?;
    let pid = std::process::id();
    loop {
        let nonce = next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut filename = name.to_os_string();
        filename.push(format!(".{pid}.{nonce}.tmp"));
        let staging = path.with_file_name(filename);
        match File::options().write(true).create_new(true).open(&staging) {
            Ok(file) => return Ok((staging, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(windows))]
fn commit(staging: &Path, path: &Path, parent: &Path) -> Result<(), AtomicWriteError> {
    fs::rename(staging, path).map_err(AtomicWriteError::PreCommit)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(AtomicWriteError::PostCommitDurability)
}

#[cfg(windows)]
fn commit(staging: &Path, path: &Path, parent: &Path) -> Result<(), AtomicWriteError> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    // Canonicalizing the parent supplies the extended-length path prefix and
    // lets the rename replace a destination symlink without resolving it.
    let parent = parent.canonicalize().map_err(AtomicWriteError::PreCommit)?;
    let wide_path = |file: &Path| {
        parent
            .join(file.file_name().unwrap())
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let staging = wide_path(staging);
    let target = wide_path(path);
    // Windows requests durability through the rename flags. The arguments
    // are live NUL-terminated UTF-16 strings for the duration of the call.
    let moved = unsafe {
        MoveFileExW(
            staging.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(AtomicWriteError::PreCommit(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saveload::slot_tests::TestRoot;

    #[test]
    fn slot_staging_skips_a_directory_collision() {
        let root = TestRoot::new();
        let path = root.file("world.zip", b"old", 100);
        let collision = path.with_extension(format!("zip.{}.0.tmp", std::process::id()));
        fs::create_dir(&collision).unwrap();

        let (staging, mut file) = create_staging_file(&path, &AtomicU64::new(0)).unwrap();
        file.write_all(b"new").unwrap();
        assert_ne!(staging, collision);
        assert!(collision.is_dir());
        assert_eq!(fs::read(path).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn slot_staging_never_follows_a_symlink_collision() {
        let root = TestRoot::new();
        let path = root.file("world.zip", b"old", 100);
        let collision = path.with_extension(format!("zip.{}.0.tmp", std::process::id()));
        std::os::unix::fs::symlink(&path, &collision).unwrap();

        let (staging, mut file) = create_staging_file(&path, &AtomicU64::new(0)).unwrap();
        file.write_all(b"new").unwrap();
        assert_ne!(staging, collision);
        assert_eq!(fs::read_link(collision).unwrap(), path);
        assert_eq!(fs::read(path).unwrap(), b"old");
    }
}
