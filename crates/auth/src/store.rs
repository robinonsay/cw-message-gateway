//! Persistent `last_seq`, the highest sequence number the node has acted on.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Stores `last_seq` as a decimal number in a small text file.
///
/// Writes go to a temporary file that is synced and then put in place of the old one
/// with [`replace_file`], which returns once that is on disk. A power cut leaves
/// either the old or the new value, never a torn file, and once `save` returns Ok the
/// new value survives. Losing an update would let a used code be replayed, so callers
/// must save before acting and must not act if `save` fails.
#[derive(Debug, Clone)]
pub struct SeqStore {
    path: PathBuf,
}

impl SeqStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored value, or 0 when the file does not exist yet.
    pub fn load(&self) -> io::Result<u64> {
        match fs::read_to_string(&self.path) {
            Ok(s) => s.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", self.path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, last_seq: u64) -> io::Result<()> {
        let dir = parent_dir(&self.path);
        fs::create_dir_all(dir)?;
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            writeln!(f, "{last_seq}")?;
            f.sync_all()?;
        }
        // Without a durable replace a power cut can bring back the old last_seq and
        // re-enable used codes.
        replace_file(&tmp, &self.path)
    }
}

/// Puts `tmp`, already written and synced, in place of `path` in the same directory,
/// and returns only once that is on disk: after a power cut `path` has its old
/// contents or the new ones, and the new ones if this returned Ok.
#[cfg(not(windows))]
pub fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(tmp, path)?;
    // The rename is only durable once the directory entry is.
    sync_dir(parent_dir(path))
}

/// Puts `tmp`, already written and synced, in place of `path` in the same directory,
/// and returns only once that is on disk: after a power cut `path` has its old
/// contents or the new ones, and the new ones if this returned Ok.
///
/// `std::fs::rename` does not wait for the rename to reach the disk, and Windows
/// cannot sync a directory the way Unix does, so this asks for it directly:
/// MoveFileExW with MOVEFILE_WRITE_THROUGH "does not return until the file is
/// actually moved on the disk" (Microsoft's MoveFileExW documentation).
#[cfg(windows)]
pub fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from, to) = (wide(tmp), wide(path));
    let mut tries = 0;
    loop {
        // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        // A virus scanner or the search indexer holding the old file open for a
        // moment fails the replace with "access denied" (5) or a sharing violation
        // (32); try again shortly.
        tries += 1;
        if tries >= 10 || !matches!(e.raw_os_error(), Some(5 | 32)) {
            return Err(e);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The directory holding `path`; `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeqStore::new(dir.path().join("state/last_seq"));
        assert_eq!(store.load().unwrap(), 0);
        store.save(43).unwrap();
        assert_eq!(store.load().unwrap(), 43);
        store.save(44).unwrap();
        assert_eq!(store.load().unwrap(), 44);
    }

    #[test]
    fn syncs_the_directory_and_reports_failure() {
        assert_eq!(parent_dir(Path::new("last_seq")), Path::new("."));
        assert_eq!(
            parent_dir(Path::new("/var/hf/last_seq")),
            Path::new("/var/hf")
        );
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            sync_dir(dir.path()).unwrap();
            assert!(sync_dir(&dir.path().join("missing")).is_err());
        }
    }

    #[test]
    fn replace_file_replaces_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (tmp, path) = (dir.path().join("x.tmp"), dir.path().join("x"));
        for v in ["one", "two"] {
            fs::write(&tmp, v).unwrap();
            replace_file(&tmp, &path).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), v);
            assert!(!tmp.exists());
        }
        // Nothing to move: an error, and the old file is untouched.
        assert!(replace_file(&tmp, &path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }

    #[test]
    fn corrupt_file_is_an_error_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_seq");
        fs::write(&path, "garbage").unwrap();
        assert!(SeqStore::new(path).load().is_err());
    }
}
