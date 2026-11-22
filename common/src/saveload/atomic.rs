use std::fmt;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const MAX_STAGING_ATTEMPTS: usize = 1024;
static NEXT_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Durability {
    RenameOnly,
    Durable,
}

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
    write_staged(path, Durability::Durable, |file| file.write_all(bytes))
}

pub(super) fn write_staged(
    path: &Path,
    durability: Durability,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    let parent = parent_directory(path);
    let durable = durability == Durability::Durable;
    if durable {
        create_durable_directory(parent)
    } else {
        fs::create_dir_all(parent)
    }
    .map_err(AtomicWriteError::PreCommit)?;

    let (staging, mut file) =
        create_staging_file(path, &NEXT_NONCE).map_err(AtomicWriteError::PreCommit)?;
    let prepared = (|| {
        write(&mut file)?;
        #[cfg(unix)]
        set_staging_permissions(path, &file)?;
        if durable {
            file.sync_all()?;
        }
        Ok(())
    })();
    drop(file);

    let result = prepared.and_then(|()| replace_file(&staging, path, durability));
    if let Err(error) = result {
        let _ = fs::remove_file(staging);
        return Err(AtomicWriteError::PreCommit(error));
    }

    if durable {
        sync_directory(parent).map_err(AtomicWriteError::PostCommitDurability)?;
    }
    Ok(())
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_durable_directory(path: &Path) -> io::Result<()> {
    let parent = parent_directory(path);
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                ErrorKind::NotADirectory,
                "save parent is not a directory",
            ))
        }
        Err(error) if error.kind() == ErrorKind::NotFound && parent != path => {}
        Err(error) => return Err(error),
    }

    create_durable_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => {
            sync_directory(parent)
        }
        Err(error) => Err(error),
    }
}

fn create_staging_file(path: &Path, next_nonce: &AtomicU64) -> io::Result<(PathBuf, File)> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    create_unique_file(path, next_nonce, &options)
}

fn create_unique_file(
    path: &Path,
    next_nonce: &AtomicU64,
    options: &fs::OpenOptions,
) -> io::Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "destination needs a file name"))?;
    let pid = std::process::id();
    for _ in 0..MAX_STAGING_ATTEMPTS {
        let nonce = next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut filename = name.to_os_string();
        filename.push(format!(".{pid}.{nonce}.tmp"));
        let staging = path.with_file_name(filename);
        match options.open(&staging) {
            Ok(file) => return Ok((staging, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "staging name retries exhausted",
    ))
}

#[cfg(unix)]
fn set_staging_permissions(path: &Path, file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.permissions(),
        Ok(_) => default_file_permissions(path)?,
        Err(error) if error.kind() == ErrorKind::NotFound => default_file_permissions(path)?,
        Err(error) => return Err(error),
    };
    file.set_permissions(fs::Permissions::from_mode(permissions.mode() & 0o7777))
}

#[cfg(unix)]
fn default_file_permissions(path: &Path) -> io::Result<fs::Permissions> {
    // The mode of an empty sibling captures the effective creation mask without
    // changing the process-wide umask or making any staged data accessible.
    let mut options = File::options();
    options.write(true).create_new(true);
    let (probe, file) = create_unique_file(path, &NEXT_NONCE, &options)?;
    let metadata = file.metadata();
    drop(file);
    fs::remove_file(probe)?;
    Ok(metadata?.permissions())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> io::Result<()> {
    const EINVAL: i32 = 22;
    match File::open(path).and_then(|directory| directory.sync_all()) {
        Err(error)
            if error.kind() == ErrorKind::Unsupported || error.raw_os_error() == Some(EINVAL) =>
        {
            Ok(())
        }
        result => result,
    }
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Windows uses write-through replacement below. Directory fsync is unsupported.
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(staging: &Path, path: &Path, _durability: Durability) -> io::Result<()> {
    fs::rename(staging, path)
}

#[cfg(any(windows, test))]
fn retry_windows_replace(
    mut replace: impl FnMut() -> io::Result<()>,
    mut backoff: impl FnMut(Duration),
) -> io::Result<()> {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;

    for millis in [10, 20, 30, 40] {
        match replace() {
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
                ) =>
            {
                backoff(Duration::from_millis(millis));
            }
            result => return result,
        }
    }
    replace()
}

#[cfg(windows)]
fn replace_file(staging: &Path, path: &Path, durability: Durability) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::thread::sleep;

    if durability == Durability::RenameOnly {
        return retry_windows_replace(|| fs::rename(staging, path), sleep);
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    // Canonicalizing the parent supplies an extended-length path prefix while
    // leaving a destination symlink unresolved.
    let parent = parent_directory(path);
    let canonical_parent = parent.canonicalize().unwrap_or_else(|_| parent.to_owned());
    let wide_path = |file: &Path| {
        canonical_parent
            .join(file.file_name().unwrap())
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let staging = wide_path(staging);
    let target = wide_path(path);
    retry_windows_replace(
        || {
            // Both arguments are live NUL-terminated UTF-16 strings.
            let moved = unsafe {
                MoveFileExW(
                    staging.as_ptr(),
                    target.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if moved == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        },
        sleep,
    )
}

/// Remove regular staging files directly under `root` older than `older_than`.
/// Call at startup or use an age safely above the lifetime of an in-progress write.
/// Entry errors are skipped and logged. Entries that disappear are skipped quietly.
pub fn sweep_stale_tmp(root: &Path, older_than: Duration) -> io::Result<usize> {
    let dir = match fs::read_dir(root) {
        Ok(dir) => dir,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                log::warn!(
                    "could not read staging file entry under {}: {error}",
                    root.display()
                );
                continue;
            }
        };
        if !is_staging_name(entry.file_name().as_encoded_bytes()) {
            continue;
        }
        let path = entry.path();
        let result: io::Result<()> = (|| {
            let metadata = path.symlink_metadata()?;
            if metadata.is_file()
                && now
                    .duration_since(metadata.modified()?)
                    .is_ok_and(|age| age > older_than)
            {
                fs::remove_file(&path)?;
                removed += 1;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => log::warn!("could not sweep staging file {}: {error}", path.display()),
        }
    }
    Ok(removed)
}

fn is_staging_name(name: &[u8]) -> bool {
    let Some(stem) = name.strip_suffix(b".tmp") else {
        return false;
    };
    let mut parts = stem.rsplitn(3, |&b| b == b'.');
    let (Some(nonce), Some(pid), Some(target)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !target.is_empty()
        && pid.iter().all(u8::is_ascii_digit)
        && nonce.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(pid).is_ok_and(|pid| pid.parse::<u32>().is_ok())
        && std::str::from_utf8(nonce).is_ok_and(|nonce| nonce.parse::<u64>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saveload::slot_tests::TestRoot;
    use std::time::Duration;

    #[test]
    fn qf1_windows_replace_retries_transient_errors() {
        for code in [5, 32] {
            let mut attempts = 0;
            let mut delays = Vec::new();
            retry_windows_replace(
                || {
                    attempts += 1;
                    if attempts <= 2 {
                        Err(io::Error::from_raw_os_error(code))
                    } else {
                        Ok(())
                    }
                },
                |delay| delays.push(delay),
            )
            .unwrap();
            assert_eq!(attempts, 3);
            assert_eq!(
                delays,
                [Duration::from_millis(10), Duration::from_millis(20)]
            );
        }
    }

    #[test]
    fn qf1_windows_replace_returns_non_retryable_errors_immediately() {
        let error = retry_windows_replace(
            || Err(io::Error::from_raw_os_error(2)),
            |_| panic!("unexpected retry"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(2));
    }

    #[test]
    fn qf1_windows_replace_gives_up_after_five_attempts() {
        let mut attempts = 0;
        let mut delays = 0;
        let error = retry_windows_replace(
            || {
                attempts += 1;
                Err(io::Error::from_raw_os_error(32))
            },
            |_| delays += 1,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!((attempts, delays), (5, 4));
    }

    #[test]
    fn slot_staging_exhausts_a_bounded_collision_budget() {
        let root = TestRoot::new();
        let path = root.file("world.zip", b"old", 100);
        for nonce in 0..1024 {
            fs::create_dir(path.with_extension(format!("zip.{}.{nonce}.tmp", std::process::id())))
                .unwrap();
        }
        let next = AtomicU64::new(0);
        assert_eq!(
            create_staging_file(&path, &next).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(next.load(Ordering::Relaxed), 1024);
        assert_eq!(fs::read(path).unwrap(), b"old");
    }

    #[test]
    fn slot_sweep_removes_only_old_staging_files() {
        let root = TestRoot::new();
        let old = root.file("world.zip.42.11.tmp", b"staged", 100);
        let fresh = root.file("world.zip.42.12.tmp", b"staged", 100);
        let unrelated = root.file("world.zip.unrelated.tmp", b"keep", 100);
        let directory = old.with_file_name("world.zip.42.13.tmp");
        fs::create_dir(&directory).unwrap();
        File::options()
            .write(true)
            .open(&fresh)
            .unwrap()
            .set_modified(std::time::SystemTime::now())
            .unwrap();

        assert_eq!(
            sweep_stale_tmp(old.parent().unwrap(), Duration::from_secs(3600)).unwrap(),
            1
        );
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(unrelated.exists());
        assert!(directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn slot_sweep_skips_entry_removal_errors() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestRoot::new();
        let stale = root.file("world.zip.42.11.tmp", b"staged", 100);
        let probe = root.file("probe", b"", 100);
        let dir = stale.parent().unwrap();
        let permissions = fs::metadata(dir).unwrap().permissions();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
        let probe_result = fs::remove_file(probe);
        let sweep_result = sweep_stale_tmp(dir, Duration::ZERO);
        fs::set_permissions(dir, permissions).unwrap();

        // A privileged process can unlink files even without directory write permission.
        if probe_result.is_ok() {
            return;
        }
        assert_eq!(
            probe_result.unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(sweep_result.unwrap(), 0);
        assert!(stale.is_file());
        assert_eq!(sweep_stale_tmp(dir, Duration::ZERO).unwrap(), 1);
    }

    #[test]
    fn slot_atomic_write_creates_nested_parents() {
        let root = TestRoot::new();
        let sibling = root.file("sibling", b"fixture", 100);
        let path = sibling
            .with_file_name("save-root")
            .join("nested")
            .join("world.zip");
        write_atomic(&path, b"snapshot").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"snapshot");
    }

    #[test]
    fn slot_staging_skips_a_directory_collision() {
        let root = TestRoot::new();
        let path = root.file("world.zip", b"old", 100);
        let collision = path.with_extension(format!("zip.{}.0.tmp", std::process::id()));
        fs::create_dir(&collision).unwrap();

        let (staging, mut file) = create_staging_file(&path, &AtomicU64::new(0)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o7177, 0);
        }
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
