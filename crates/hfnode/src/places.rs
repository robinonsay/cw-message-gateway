//! The last place each field callsign asked for the weather at, so that `WX` alone
//! means "where I am" once the operator has named it.
//!
//! This is a convenience, not security state: a file that cannot be read or written
//! is logged and the node carries on, falling back to `weather.default_grid`. The
//! read-back names the grid square either way, so the operator always hears which
//! place a forecast will be for.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Grid squares by field callsign, kept in a small JSON file.
#[derive(Debug, Clone, Default)]
pub struct LastPlaces {
    /// `None` keeps the places in memory only, for unit tests.
    path: Option<PathBuf>,
    grids: BTreeMap<String, String>,
}

impl LastPlaces {
    /// Load from `path`; a missing file is an empty one. A file that cannot be read
    /// or parsed is logged and treated as empty.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let grids = match fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<BTreeMap<String, String>>(&s) {
                Ok(g) => g
                    .into_iter()
                    .filter(|(_, grid)| protocol::is_grid(grid))
                    .collect(),
                Err(e) => {
                    log::warn!("ignoring {}: {e}", path.display());
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                BTreeMap::new()
            }
        };
        Self {
            path: Some(path),
            grids,
        }
    }

    /// In memory only.
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn get(&self, call: &str) -> Option<&str> {
        self.grids.get(call).map(String::as_str)
    }

    /// Remember `grid` for `call` and save. Memory is updated even if the save
    /// fails, so the node still remembers until it restarts.
    pub fn set(&mut self, call: &str, grid: &str) -> Result<()> {
        if self.get(call) == Some(grid) {
            return Ok(());
        }
        self.grids.insert(call.to_string(), grid.to_string());
        match &self.path {
            Some(path) => save(path, &self.grids),
            None => Ok(()),
        }
    }
}

fn save(path: &Path, grids: &BTreeMap<String, String>) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(grids)?.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    };
    write().with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/wx_last.json");
        let mut p = LastPlaces::open(&path);
        assert_eq!(p.get("W5XXX"), None);
        p.set("W5XXX", "DL89IG").unwrap();
        p.set("K1ABC", "FN31").unwrap();
        p.set("W5XXX", "DL89ME").unwrap();
        let p = LastPlaces::open(&path);
        assert_eq!(p.get("W5XXX"), Some("DL89ME"));
        assert_eq!(p.get("K1ABC"), Some("FN31"));
    }

    #[test]
    fn a_bad_file_is_ignored_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wx_last.json");
        fs::write(&path, "not json").unwrap();
        let mut p = LastPlaces::open(&path);
        assert_eq!(p.get("W5XXX"), None);
        // A hand-edited entry that is not a grid square is dropped.
        fs::write(&path, r#"{"W5XXX":"HOME","K1ABC":"FN31"}"#).unwrap();
        p = LastPlaces::open(&path);
        assert_eq!(p.get("W5XXX"), None);
        assert_eq!(p.get("K1ABC"), Some("FN31"));
    }

    #[test]
    fn a_failed_save_is_reported_and_still_remembered() {
        let dir = tempfile::tempdir().unwrap();
        // The parent "directory" is a file, so the save cannot work.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "").unwrap();
        let mut p = LastPlaces::open(blocker.join("wx_last.json"));
        assert!(p.set("W5XXX", "DL89IG").is_err());
        assert_eq!(p.get("W5XXX"), Some("DL89IG"));
    }
}
