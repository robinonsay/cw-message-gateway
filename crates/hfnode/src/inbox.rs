//! Inbound replies (texts and email) waiting to be read over the air.
//!
//! Messages arrive unscreened, are screened once by the compliance filter, and only
//! then become readable. A message that never passed the filter is never keyed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Received, not yet screened. Never transmitted in this state.
    Unscreened,
    /// Screened and waiting to be read.
    Ready,
    /// Sent over the air.
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    /// Contact name of the sender.
    pub from: String,
    pub received_unix: u64,
    /// Where it came from (e.g. the email Message-ID), for de-duplication.
    pub source_id: String,
    /// The text as received.
    pub raw: String,
    /// The text after screening, ready to key. Set once state is Ready.
    pub screened: Option<String>,
    pub state: State,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    next_id: u64,
    messages: Vec<Message>,
}

/// Inbox persisted as a JSON file, rewritten atomically on every change.
#[derive(Debug)]
pub struct Inbox {
    path: PathBuf,
    data: File,
}

/// Read messages are kept this long, then pruned.
const KEEP_READ_SECS: u64 = 30 * 24 * 3600;

impl Inbox {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let data = match fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, data })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn save(&self) -> Result<()> {
        let dir = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::create_dir_all(dir)?;
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&self.data)?.as_bytes())?;
            f.sync_all()?;
        }
        // Without a durable replace a power cut can bring back the old file after
        // the caller has acknowledged the source (the email marked \Seen), losing
        // the message.
        auth::replace_file(&tmp, &self.path)?;
        Ok(())
    }

    /// Add a received message. Returns false if `source_id` was already seen.
    ///
    /// Every change is saved before it counts: if the save fails, memory is rolled
    /// back to match the file, so what is in memory is always on disk. A caller may
    /// therefore treat `Ok` (either value) as "safely stored" and acknowledge the
    /// source (e.g. mark the email \Seen).
    pub fn add(
        &mut self,
        from: &str,
        source_id: &str,
        raw: &str,
        received_unix: u64,
    ) -> Result<bool> {
        if self.data.messages.iter().any(|m| m.source_id == source_id) {
            return Ok(false);
        }
        self.data.next_id += 1;
        self.data.messages.push(Message {
            id: self.data.next_id,
            from: from.to_string(),
            received_unix,
            source_id: source_id.to_string(),
            raw: raw.to_string(),
            screened: None,
            state: State::Unscreened,
        });
        if let Err(e) = self.save() {
            self.data.messages.pop();
            self.data.next_id -= 1;
            return Err(e);
        }
        Ok(true)
    }

    pub fn unscreened(&self) -> Vec<Message> {
        self.data
            .messages
            .iter()
            .filter(|m| m.state == State::Unscreened)
            .cloned()
            .collect()
    }

    pub fn set_screened(&mut self, id: u64, text: &str) -> Result<()> {
        let Some(i) = self.data.messages.iter().position(|m| m.id == id) else {
            return Ok(());
        };
        let before = self.data.messages[i].clone();
        let m = &mut self.data.messages[i];
        m.screened = Some(text.to_string());
        m.state = State::Ready;
        if let Err(e) = self.save() {
            self.data.messages[i] = before;
            return Err(e);
        }
        Ok(())
    }

    /// Screened messages not yet read, oldest first.
    pub fn ready(&self) -> Vec<Message> {
        let mut v: Vec<Message> = self
            .data
            .messages
            .iter()
            .filter(|m| m.state == State::Ready)
            .cloned()
            .collect();
        v.sort_by_key(|m| (m.received_unix, m.id));
        v
    }

    pub fn mark_read(&mut self, ids: &[u64], now_unix: u64) -> Result<()> {
        let before = self.data.messages.clone();
        for m in self.data.messages.iter_mut() {
            if ids.contains(&m.id) {
                m.state = State::Read;
            }
        }
        self.data.messages.retain(|m| {
            m.state != State::Read || now_unix.saturating_sub(m.received_unix) < KEEP_READ_SECS
        });
        if let Err(e) = self.save() {
            self.data.messages = before;
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.json");
        let mut inbox = Inbox::open(&path).unwrap();
        assert!(inbox.add("MOM", "<a@x>", "Drive safe!", 100).unwrap());
        assert!(!inbox.add("MOM", "<a@x>", "Drive safe!", 100).unwrap());
        assert!(inbox.add("BOB", "<b@x>", "ok", 50).unwrap());
        assert!(
            inbox.ready().is_empty(),
            "unscreened messages are never ready"
        );
        for m in inbox.unscreened() {
            inbox.set_screened(m.id, &m.raw.to_uppercase()).unwrap();
        }
        let ready = Inbox::open(&path).unwrap().ready();
        assert_eq!(
            ready.iter().map(|m| m.from.as_str()).collect::<Vec<_>>(),
            ["BOB", "MOM"]
        );
        inbox.mark_read(&[ready[0].id], 200).unwrap();
        assert_eq!(inbox.ready().len(), 1);
        // Read messages are pruned after 30 days.
        inbox.mark_read(&[], 50 + KEEP_READ_SECS).unwrap();
        assert_eq!(Inbox::open(&path).unwrap().data.messages.len(), 1);
    }

    #[test]
    fn failed_save_leaves_memory_matching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.json");
        let mut inbox = Inbox::open(&path).unwrap();
        assert!(inbox.add("BOB", "<b@x>", "ok", 50).unwrap());
        // A directory where the temp file goes makes every save fail, even as root.
        let blocker = path.with_extension("tmp");
        fs::create_dir(&blocker).unwrap();
        assert!(inbox.add("MOM", "<a@x>", "Drive safe!", 100).is_err());
        // Not on disk, so it must not look like a stored duplicate either.
        assert!(inbox.add("MOM", "<a@x>", "Drive safe!", 100).is_err());
        let id = inbox.unscreened()[0].id;
        assert!(inbox.set_screened(id, "OK").is_err());
        assert_eq!(inbox.unscreened().len(), 1);
        fs::remove_dir(&blocker).unwrap();
        inbox.set_screened(id, "OK").unwrap();
        fs::create_dir(&blocker).unwrap();
        assert!(inbox.mark_read(&[id], 60).is_err());
        assert_eq!(inbox.ready().len(), 1);
        fs::remove_dir(&blocker).unwrap();
        assert!(inbox.add("MOM", "<a@x>", "Drive safe!", 100).unwrap());
        let on_disk = Inbox::open(&path).unwrap();
        assert_eq!(on_disk.data.messages.len(), 2);
        assert_eq!(on_disk.data.next_id, 2);
        assert_eq!(on_disk.ready().len(), 1);
    }
}
