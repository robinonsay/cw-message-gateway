//! Persistent `last_seq`, the highest sequence number the node has acted on.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Stores `last_seq` as a decimal number in a small text file.
///
/// Writes go to a temporary file that is synced and renamed over the old one, and
/// the directory is then synced so the rename itself is on disk. A power cut leaves
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
        fs::rename(&tmp, &self.path)?;
        // The rename is only durable once the directory entry is: without this a
        // power cut can bring back the old last_seq and re-enable used codes.
        sync_dir(dir)
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

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    // Directories cannot be opened for syncing here; the node runs on Linux.
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
        let dir = tempfile::tempdir().unwrap();
        sync_dir(dir.path()).unwrap();
        #[cfg(unix)]
        assert!(sync_dir(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn corrupt_file_is_an_error_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_seq");
        fs::write(&path, "garbage").unwrap();
        assert!(SeqStore::new(path).load().is_err());
    }
}
