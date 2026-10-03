//! Persistent `last_seq`, the highest sequence number the node has acted on.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Stores `last_seq` as a decimal number in a small text file.
///
/// Writes go to a temporary file that is synced and renamed over the old one, so a
/// power cut leaves either the old or the new value, never a torn file. Losing an
/// update would let a used code be replayed, so callers must save before acting.
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
            Ok(s) => s
                .trim()
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, last_seq: u64) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            writeln!(f, "{last_seq}")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
    }
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
    fn corrupt_file_is_an_error_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_seq");
        fs::write(&path, "garbage").unwrap();
        assert!(SeqStore::new(path).load().is_err());
    }
}
