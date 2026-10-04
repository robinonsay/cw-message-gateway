//! Texting through a Google Voice number.
//!
//! The node's Gmail account has a Google Voice number with "forward messages to
//! email" on. A text to that number arrives as an email from
//! `<gv number>.<sender number>.<token>@txt.voice.google.com`, and an email sent to
//! that address is texted to the sender from the Google Voice number. So the node
//! reads texts like any other mail ([`super::email::accept_with`]), and keeps each
//! contact's reply address in `<state_dir>/google_voice.json` to text them.
//!
//! A reply address exists only once a contact has texted the number: the node
//! cannot start a conversation by email. Addresses are learned from the mailbox
//! ([`learn_gv`]) separately from reading texts, so a mail opened in Gmail, or one
//! with no text, still teaches the address.
//!
//! All of this rests on Google's mail formats, recalled rather than checked against
//! the owner's account: see docs/texting.md for the checks to make before relying on
//! it.

use super::email::{connect_imap, dkim_authenticated, us_number};
use crate::config::{Contact, Email, Phone, GOOGLE_VOICE_DOMAIN};
use anyhow::{Context, Result};
use mail_parser::{HeaderName, MessageParser, PartType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A Google Voice address, split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GvAddr {
    /// The Google Voice number the text was sent to.
    pub node: Phone,
    /// Who sent the text.
    pub sender: Phone,
    pub token: String,
    /// The whole address, as received: the token may be case-sensitive.
    pub address: String,
}

/// Parse `<gv number>.<sender number>.<token>@txt.voice.google.com`; the numbers are
/// 10 digits, or 11 with a leading 1.
pub fn parse_gv_from(addr: &str) -> Option<GvAddr> {
    let addr = addr.trim();
    let (local, domain) = addr.rsplit_once('@')?;
    if !domain.eq_ignore_ascii_case(GOOGLE_VOICE_DOMAIN) {
        return None;
    }
    let fields: Vec<&str> = local.split('.').collect();
    let [node, sender, token] = fields[..] else {
        return None;
    };
    let number = |f: &str| us_number(f).and_then(|n| Phone::parse(&format!("+1{n}")));
    let token_ok = !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !token_ok {
        return None;
    }
    Some(GvAddr {
        node: number(node)?,
        sender: number(sender)?,
        token: token.to_string(),
        address: addr.to_string(),
    })
}

/// The text of a Google Voice mail, without Google's footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GvBody {
    pub text: String,
    /// The footer line the text was cut at (Google's words, never the sender's).
    pub cut_at: String,
}

/// The line that starts the footer of a one-to-one text.
const MARKER: &str = "to respond to this text message";

/// A line trimmed, lowercased and with its whitespace collapsed, for comparing.
fn normalized(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether `line` (normalized) starts Google's footer.
fn starts_footer(line: &str) -> bool {
    line.starts_with(MARKER)
        || (line.starts_with("your account") && line.contains("help"))
        || line.starts_with("you received this")
        || line.starts_with("this email was sent")
        || line.starts_with("google llc")
}

/// A line that is only a link, like the logo line above the text.
fn is_link_line(line: &str) -> bool {
    let t = line.trim();
    (t.starts_with("<http") && t.ends_with('>') && !t.contains(' '))
        || (t.starts_with("http") && !t.contains(' '))
}

/// The plain-text part of a Google Voice mail. Never the HTML part converted to text:
/// that would turn markup nobody checked into words read on air.
fn plain_text<'a>(parsed: &'a mail_parser::Message<'a>) -> Option<&'a str> {
    match &parsed.text_part(0)?.body {
        PartType::Text(t) => Some(t.as_ref()),
        _ => None,
    }
}

/// What the sender of a Google Voice mail wrote: the lines above Google's footer.
/// Refused, rather than guessed at, if the mail does not look as expected.
pub fn gv_text(parsed: &mail_parser::Message) -> Result<GvBody, String> {
    let body = plain_text(parsed).ok_or("Google Voice mail without a text/plain part")?;
    let lines: Vec<&str> = body
        .lines()
        .skip_while(|l| l.trim().is_empty() || is_link_line(l))
        .collect();
    let normal: Vec<String> = lines.iter().map(|l| normalized(l)).collect();
    let end = normal.iter().position(|l| starts_footer(l));
    let has_marker = end.is_some_and(|e| normal[e..].iter().any(|l| l.starts_with(MARKER)));
    let (Some(end), true) = (end, has_marker) else {
        return Err(
            "unrecognized Google Voice mail (no \"To respond to this text message\" line)".into(),
        );
    };
    let text = super::email::strip_reply(&lines[..end].join("\n"));
    let lower = text.to_lowercase();
    if [
        "voice.google.com",
        "google llc",
        "help center",
        "<http",
        "amphitheatre",
    ]
    .iter()
    .any(|f| lower.contains(f))
    {
        return Err("Google Voice footer left in the text (format changed?)".into());
    }
    if text.is_empty() {
        return Err("no text in the Google Voice mail (picture only?)".into());
    }
    Ok(GvBody {
        text,
        cut_at: lines[end].trim().to_string(),
    })
}

/// Whether a Google Voice mail is a one-to-one text, whose sender's address is a
/// reply address to that person alone. Only what is known of one-to-one texts is
/// accepted: their Subject and the "To respond to this text message" line. A group
/// text's address would text the whole group.
pub fn one_to_one(parsed: &mail_parser::Message) -> bool {
    let subject = parsed.subject().unwrap_or("").trim();
    subject.starts_with("New text message from ")
        && plain_text(parsed)
            .is_some_and(|t| t.lines().any(|l| normalized(l).starts_with(MARKER)))
}

/// What one Google Voice mail teaches about reply addresses.
#[derive(Debug, PartialEq, Eq)]
pub enum Learn<'c> {
    /// A one-to-one text from this contact: reply to it at `address`.
    Yes { contact: &'c Contact, address: String },
    /// Not a Google Voice mail to this number from a contact's phone.
    NotContact,
    /// From a contact's number, but our mail server reports no DKIM pass for it.
    NotAuthenticated(&'c Contact),
    /// From a contact's number, but not recognisably a one-to-one text.
    NotOneToOne(&'c Contact),
}

/// Decide whether a mail teaches a contact's reply address. Whether it was read, and
/// what it says, do not matter: an opened or empty text teaches it too.
pub fn learn_decision<'c>(
    raw: &[u8],
    contacts: &'c [Contact],
    gv: &Phone,
    authserv_id: Option<&str>,
) -> Learn<'c> {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Learn::NotContact;
    };
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(addr) = parse_gv_from(&from) else {
        return Learn::NotContact;
    };
    if addr.node != *gv {
        return Learn::NotContact;
    }
    let Some(contact) = contacts
        .iter()
        .find(|c| c.phone.as_ref() == Some(&addr.sender))
    else {
        return Learn::NotContact;
    };
    let auth_results: Vec<&str> = parsed
        .headers()
        .iter()
        .filter(|h| h.name == HeaderName::AuthenticationResults)
        .filter_map(|h| {
            let raw = parsed
                .raw_message
                .get(h.offset_start as usize..h.offset_end as usize)?;
            std::str::from_utf8(raw).ok()
        })
        .collect();
    if !dkim_authenticated(&auth_results, &from, authserv_id) {
        return Learn::NotAuthenticated(contact);
    }
    if !one_to_one(&parsed) {
        return Learn::NotOneToOne(contact);
    }
    Learn::Yes {
        contact,
        address: addr.address,
    }
}

/// One contact's reply address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GvEntry {
    pub address: String,
    /// When the mail it came from arrived.
    pub learned_unix: u64,
    pub uid: u32,
}

impl GvEntry {
    fn newer_than(&self, other: &GvEntry) -> bool {
        (self.learned_unix, self.uid) > (other.learned_unix, other.uid)
    }
}

/// `<state_dir>/google_voice.json`: contacts' reply addresses, how far the mailbox
/// has been scanned for them, and when the number last sent a text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GvStore {
    #[serde(default = "version")]
    pub version: u32,
    /// The mailbox's UIDVALIDITY when `scanned_uid` was recorded.
    #[serde(default)]
    pub uidvalidity: Option<u32>,
    /// Google Voice mail up to this UID has been looked at.
    #[serde(default)]
    pub scanned_uid: u32,
    /// By the contact's phone number (E.164).
    #[serde(default)]
    pub contacts: BTreeMap<String, GvEntry>,
    #[serde(default)]
    pub last_sent_unix: Option<u64>,
}

fn version() -> u32 {
    1
}

pub const GV_FILE: &str = "google_voice.json";

impl GvStore {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(GV_FILE)
    }

    /// The stored state. A missing file is empty; so is one that cannot be read,
    /// which is logged: the node goes on, and learns the addresses again.
    pub fn read(state_dir: &Path) -> GvStore {
        let path = Self::path(state_dir);
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::error!("{GV_FILE} unreadable ({e}); treating as empty");
                GvStore::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => GvStore::default(),
            Err(e) => {
                log::error!("{GV_FILE} unreadable ({e}); treating as empty");
                GvStore::default()
            }
        }
    }

    /// The reply address to text `phone` from the Google Voice number `gv`, if one
    /// was learned and still fits both numbers (the Google Voice number may have
    /// changed, or the file been edited by hand).
    pub fn reply_address(&self, phone: &Phone, gv: &Phone) -> Option<String> {
        let entry = self.contacts.get(phone.as_str())?;
        match parse_gv_from(&entry.address) {
            Some(a) if a.node == *gv && a.sender == *phone => Some(a.address),
            _ => {
                static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
                let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
                if warned
                    .get_or_insert_with(HashSet::new)
                    .insert(phone.to_string())
                {
                    log::warn!(
                        "stored Google Voice address for {phone} does not match the configured \
                         numbers; ignored"
                    );
                }
                None
            }
        }
    }

    /// Combine `delta` into this state: the newer address for each contact, the
    /// scan position of the newer one, and the latest send.
    fn merge(&mut self, delta: &GvStore) {
        for (phone, e) in &delta.contacts {
            match self.contacts.get(phone) {
                Some(old) if !e.newer_than(old) => {}
                _ => {
                    self.contacts.insert(phone.clone(), e.clone());
                }
            }
        }
        if let Some(v) = delta.uidvalidity {
            if self.uidvalidity == Some(v) {
                self.scanned_uid = self.scanned_uid.max(delta.scanned_uid);
            } else {
                self.uidvalidity = Some(v);
                self.scanned_uid = delta.scanned_uid;
            }
        }
        self.last_sent_unix = self.last_sent_unix.max(delta.last_sent_unix);
    }

    /// Merge `delta` into the file and save it. The node and `hfnode messages send`
    /// may both write, so this is done under a lock on `google_voice.lock`, reading
    /// the file again first; without it one could drop the other's change.
    pub fn save_merge(state_dir: &Path, delta: &GvStore) -> Result<GvStore> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let _lock = lock(&state_dir.join("google_voice.lock"))?;
        let mut current = Self::read(state_dir);
        current.version = version();
        current.merge(delta);
        write_json(&Self::path(state_dir), &current)?;
        Ok(current)
    }

    /// Note that the number just sent a text: an unused number can be taken back.
    pub fn record_sent(state_dir: &Path, now: u64) -> Result<()> {
        let delta = GvStore {
            last_sent_unix: Some(now),
            ..Default::default()
        };
        Self::save_merge(state_dir, &delta).map(|_| ())
    }
}

/// An exclusive lock on `path` (created if missing), held until the file is dropped.
pub(crate) fn lock(path: &Path) -> Result<fs::File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(f)
}

/// Write `value` to `path` so that a power cut leaves the old file or the new one.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        f.sync_all()?;
    }
    auth::replace_file(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Most mails fetched by one full scan for reply addresses.
const FULL_SCAN_LIMIT: usize = 300;

/// The UIDs an incremental scan looks at: `UID n:*` also returns the highest UID
/// when that is below `n` (RFC 3501), so only those above `scanned` count.
pub fn new_uids(found: impl IntoIterator<Item = u32>, scanned: u32) -> Vec<u32> {
    let mut uids: Vec<u32> = found.into_iter().filter(|&u| u > scanned).collect();
    uids.sort_unstable();
    uids
}

/// Learn contacts' Google Voice reply addresses from the node's mailbox, which is
/// only examined (opened read-only): nothing is marked or moved. A full scan
/// (`full`, or a mailbox whose UIDs were renumbered) goes back from the newest mail
/// until every contact with a phone has an address or [`FULL_SCAN_LIMIT`] mails
/// were read; otherwise only mail since the last scan is read. Returns how many
/// addresses were learned or changed.
pub fn learn_gv(
    email: &Email,
    gv: &Phone,
    contacts: &[Contact],
    state_dir: &Path,
    full: bool,
) -> Result<usize> {
    let mut session = connect_imap(email)?;
    let mailbox = session.examine("INBOX")?;
    let stored = GvStore::read(state_dir);
    let uidvalidity = mailbox.uid_validity.unwrap_or(0);
    let full = full || stored.uidvalidity != Some(uidvalidity);
    let from = format!("FROM \"{GOOGLE_VOICE_DOMAIN}\"");
    let uids: Vec<u32> = if full {
        let mut u = new_uids(session.uid_search(&from)?, 0);
        u.reverse();
        u
    } else {
        let next = stored.scanned_uid.saturating_add(1);
        new_uids(
            session.uid_search(format!("UID {next}:* {from}"))?,
            stored.scanned_uid,
        )
    };
    let phones: Vec<&Phone> = contacts.iter().filter_map(|c| c.phone.as_ref()).collect();
    let mut delta = GvStore::default();
    let mut scanned = if full { 0 } else { stored.scanned_uid };
    let mut complete = true;
    let mut fetched = 0;
    for &uid in &uids {
        if full
            && (fetched >= FULL_SCAN_LIMIT
                || phones.iter().all(|p| delta.contacts.contains_key(p.as_str())))
        {
            break;
        }
        fetched += 1;
        let fetches = match session.uid_fetch(uid.to_string(), "(UID INTERNALDATE BODY.PEEK[])") {
            Ok(f) => f,
            Err(e) => {
                log::warn!("IMAP fetch of UID {uid} failed while looking for Google Voice addresses: {e}");
                complete = false;
                break;
            }
        };
        let Some(fetch) = fetches
            .iter()
            .filter(|f| f.uid.is_none_or(|u| u == uid))
            .find(|f| f.body().is_some())
        else {
            complete = false;
            break;
        };
        if !full {
            scanned = uid;
        }
        let raw = fetch.body().unwrap_or_default();
        let at = super::stamp(
            fetch.internal_date().map(|d| d.timestamp()),
            super::unix_now(),
        );
        match learn_decision(raw, contacts, gv, email.authserv_id.as_deref()) {
            Learn::Yes { contact, address } => {
                let entry = GvEntry {
                    address,
                    learned_unix: at,
                    uid,
                };
                let phone = contact.phone.as_ref().map(Phone::to_string).unwrap_or_default();
                match delta.contacts.get(&phone) {
                    Some(old) if !entry.newer_than(old) => {}
                    _ => {
                        delta.contacts.insert(phone, entry);
                    }
                }
            }
            Learn::NotContact => {}
            Learn::NotAuthenticated(c) => log::debug!(
                "Google Voice mail from {} (UID {uid}) has no DKIM pass from our mail server",
                c.name
            ),
            Learn::NotOneToOne(c) => log::info!(
                "Google Voice mail from {} (UID {uid}): not confirmed one-to-one, reply \
                 address not learned",
                c.name
            ),
        }
    }
    let _ = session.logout();
    if full && complete {
        scanned = uids.first().copied().unwrap_or(0);
    }
    // The scan position moves only with a complete pass and a saved file.
    if complete || !full {
        delta.uidvalidity = Some(uidvalidity);
        delta.scanned_uid = scanned;
    }
    let changed = report(contacts, &stored, &delta);
    let moved = delta.uidvalidity.is_some()
        && (delta.uidvalidity, delta.scanned_uid) != (stored.uidvalidity, stored.scanned_uid);
    if changed > 0 || moved {
        GvStore::save_merge(state_dir, &delta)?;
    }
    Ok(changed)
}

/// Log what `delta` changes in `stored`; returns how many addresses are new or
/// different.
fn report(contacts: &[Contact], stored: &GvStore, delta: &GvStore) -> usize {
    let mut changed = 0;
    for (phone, e) in &delta.contacts {
        let name = contacts
            .iter()
            .find(|c| c.phone.as_ref().is_some_and(|p| p.as_str() == phone))
            .map_or("?", |c| c.name.as_str());
        match stored.contacts.get(phone) {
            None => {
                log::info!("learned Google Voice reply address for {name}");
                changed += 1;
            }
            Some(old) if old.address != e.address && e.newer_than(old) => {
                let token = |a: &str| parse_gv_from(a).map(|g| g.token).unwrap_or_default();
                log::warn!(
                    "Google Voice reply address for {name} changed (token {} -> {})",
                    token(&old.address),
                    token(&e.address)
                );
                changed += 1;
            }
            Some(_) => {}
        }
    }
    changed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-to-one Google Voice text as recalled (not yet a real capture): to the
    /// number +1 555 000 1111, from +1 555 123 4567.
    pub(crate) const FIXTURE: &str = "Authentication-Results: mx.google.com;\r\n       dkim=pass header.i=@txt.voice.google.com header.s=20230601 header.b=XyZ;\r\n       spf=pass (google.com: domain of 15550001111.15551234567.AbCdEf1234@txt.voice.google.com designates 2607:f8b0::1 as permitted sender) smtp.mailfrom=15550001111.15551234567.AbCdEf1234@txt.voice.google.com;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=txt.voice.google.com\r\nFrom: \"(555) 123-4567\" <15550001111.15551234567.AbCdEf1234@txt.voice.google.com>\r\nTo: hfnode.node@gmail.com\r\nSubject: New text message from (555) 123-4567\r\nMessage-ID: <gv-1@txt.voice.google.com>\r\nDate: Sat, 03 Oct 2026 18:20:01 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n<https://voice.google.com>\r\nRunning late, home by 6\r\nYOUR ACCOUNT <https://voice.google.com> HELP CENTER <https://support.google.com/voice#topic=1707989> HELP FORUM <https://productforums.google.com/forum/#!forum/voice>\r\nTo respond to this text message, reply to this email or visit Google Voice <https://voice.google.com>.\r\nYou received this email because you turned on email notification for text messages.\r\nGoogle LLC\r\n1600 Amphitheatre Parkway, Mountain View CA 94043 USA\r\n\r\n--b1\r\nContent-Type: text/html; charset=\"UTF-8\"\r\n\r\n<html><body>Running late, home by 6</body></html>\r\n--b1--\r\n";

    pub(crate) fn gv_number() -> Phone {
        Phone::parse("+1 555 000 1111").unwrap()
    }

    pub(crate) fn contacts() -> Vec<Contact> {
        vec![
            Contact {
                name: "MOM".into(),
                phone: Phone::parse("+1 555 123 4567"),
                ..Default::default()
            },
            Contact {
                name: "BOB".into(),
                address: Some("bob@example.com".into()),
                ..Default::default()
            },
        ]
    }

    fn parse(raw: &str) -> mail_parser::Message<'_> {
        MessageParser::default().parse(raw.as_bytes()).unwrap()
    }

    #[test]
    fn gv_addresses() {
        let a = parse_gv_from("15550001111.15551234567.AbCdEf1234@txt.voice.google.com").unwrap();
        assert_eq!(a.node, gv_number());
        assert_eq!(a.sender.as_str(), "+15551234567");
        assert_eq!(a.token, "AbCdEf1234");
        // Ten or eleven digits on either side; the domain in any case.
        for ok in [
            "5550001111.15551234567.x_y-Z@txt.voice.google.com",
            "15550001111.5551234567.t@TXT.Voice.Google.COM",
        ] {
            assert!(parse_gv_from(ok).is_some(), "{ok}");
        }
        for bad in [
            "15550001111.15551234567@txt.voice.google.com",
            "15550001111.15551234567.tok.extra@txt.voice.google.com",
            "15550001111.1555123456x.tok@txt.voice.google.com",
            "15550001111.25551234567.tok@txt.voice.google.com",
            "15550001111.15551234567.@txt.voice.google.com",
            "15550001111.15551234567.to+k@txt.voice.google.com",
            "15550001111.15551234567.tok@voice.google.com",
            "15550001111.15551234567.tok@txt.voice.google.com.evil.example",
        ] {
            assert!(parse_gv_from(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn text_is_cut_at_the_footer() {
        let body = gv_text(&parse(FIXTURE)).unwrap();
        assert_eq!(body.text, "Running late, home by 6");
        assert!(body.cut_at.starts_with("YOUR ACCOUNT"), "{}", body.cut_at);
        // No logo line, and the marker straight after the text.
        let plain = FIXTURE
            .replace("<https://voice.google.com>\r\nRunning", "Running")
            .replace(
                "YOUR ACCOUNT <https://voice.google.com> HELP CENTER <https://support.google.com/voice#topic=1707989> HELP FORUM <https://productforums.google.com/forum/#!forum/voice>\r\n",
                "",
            );
        assert_ne!(plain, FIXTURE);
        assert_eq!(gv_text(&parse(&plain)).unwrap().text, "Running late, home by 6");
        // Several lines are joined.
        let two = FIXTURE.replace("Running late, home by 6", "Running late\r\nhome by 6");
        assert_eq!(gv_text(&parse(&two)).unwrap().text, "Running late home by 6");
    }

    #[test]
    fn unexpected_mail_is_refused() {
        // HTML only: the text/plain part is dropped.
        let start = FIXTURE.find("--b1\r\nContent-Type: text/plain").unwrap();
        let html = FIXTURE.find("--b1\r\nContent-Type: text/html").unwrap();
        let html_only = format!("{}{}", &FIXTURE[..start], &FIXTURE[html..]);
        let e = gv_text(&parse(&html_only)).unwrap_err();
        assert!(e.contains("without a text/plain part"), "{e}");
        // No marker line.
        let no_marker = FIXTURE.replace(
            "To respond to this text message, reply to this email or visit Google Voice <https://voice.google.com>.\r\n",
            "",
        );
        assert!(gv_text(&parse(&no_marker))
            .unwrap_err()
            .contains("unrecognized"));
        // A new footer line above the known ones.
        let nav = FIXTURE.replace(
            "YOUR ACCOUNT",
            "MANAGE SETTINGS <https://voice.google.com/settings>\r\nYOUR ACCOUNT",
        );
        assert!(gv_text(&parse(&nav)).unwrap_err().contains("footer left"));
        // Footer only: a picture.
        let picture = FIXTURE.replace("Running late, home by 6\r\n", "");
        assert!(gv_text(&parse(&picture)).unwrap_err().contains("no text"));
    }

    #[test]
    fn one_to_one_signature() {
        assert!(one_to_one(&parse(FIXTURE)));
        let group = FIXTURE.replace("New text message from", "New group message from");
        assert!(!one_to_one(&parse(&group)));
        let no_marker = FIXTURE.replace("To respond to this text message", "To see this");
        assert!(!one_to_one(&parse(&no_marker)));
    }

    #[test]
    fn learning_reply_addresses() {
        let c = contacts();
        let gv = gv_number();
        let learn = |raw: &str| learn_decision(raw.as_bytes(), &c, &gv, Some("mx.google.com"));
        // The address keeps its case.
        match learn(FIXTURE) {
            Learn::Yes { contact, address } => {
                assert_eq!(contact.name, "MOM");
                assert_eq!(
                    address,
                    "15550001111.15551234567.AbCdEf1234@txt.voice.google.com"
                );
            }
            other => panic!("{other:?}"),
        }
        // Read already, and with no text (a picture): still learned.
        let seen_empty = FIXTURE.replace("Running late, home by 6\r\n", "");
        assert!(matches!(learn(&seen_empty), Learn::Yes { .. }));
        // An eleven-digit local part for a contact written with ten digits.
        let c10 = vec![Contact {
            name: "MOM".into(),
            phone: Phone::parse("555-123-4567"),
            ..Default::default()
        }];
        assert!(matches!(
            learn_decision(FIXTURE.as_bytes(), &c10, &gv, None),
            Learn::Yes { .. }
        ));
        // Not authenticated.
        let unsigned = FIXTURE.replace("dkim=pass", "dkim=none");
        assert_eq!(learn(&unsigned), Learn::NotAuthenticated(&c[0]));
        // Not a contact's number.
        let stranger = FIXTURE.replace("15551234567", "15559990000");
        assert_eq!(learn(&stranger), Learn::NotContact);
        // Another Google Voice number.
        let other = FIXTURE.replace("15550001111.", "15550002222.");
        assert_eq!(learn(&other), Learn::NotContact);
        // A group text.
        let group = FIXTURE.replace("New text message from", "New group message from");
        assert_eq!(learn(&group), Learn::NotOneToOne(&c[0]));
    }

    #[test]
    fn incremental_uids() {
        assert!(new_uids([4821], 4821).is_empty());
        assert_eq!(new_uids([4830, 4822, 4821], 4821), [4822, 4830]);
    }

    fn entry(address: &str, learned_unix: u64, uid: u32) -> GvEntry {
        GvEntry {
            address: address.into(),
            learned_unix,
            uid,
        }
    }

    const ADDR: &str = "15550001111.15551234567.AbCdEf1234@txt.voice.google.com";

    #[test]
    fn store_merges_and_checks_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(GvStore::read(d), GvStore::default());
        let mut a = GvStore {
            uidvalidity: Some(7),
            scanned_uid: 10,
            ..Default::default()
        };
        a.contacts
            .insert("+15551234567".into(), entry(ADDR, 100, 10));
        GvStore::save_merge(d, &a).unwrap();
        // An older entry, processed later, does not replace the newer one.
        let mut older = GvStore::default();
        older
            .contacts
            .insert("+15551234567".into(), entry("old@x", 50, 4));
        let s = GvStore::save_merge(d, &older).unwrap();
        assert_eq!(s.contacts["+15551234567"].address, ADDR);
        assert_eq!((s.uidvalidity, s.scanned_uid), (Some(7), 10));
        GvStore::record_sent(d, 1234).unwrap();
        let s = GvStore::read(d);
        assert_eq!(s.last_sent_unix, Some(1234));
        let mom = Phone::parse("+15551234567").unwrap();
        assert_eq!(s.reply_address(&mom, &gv_number()).as_deref(), Some(ADDR));
        // A changed Google Voice number makes the stored address useless.
        let other = Phone::parse("+15550002222").unwrap();
        assert_eq!(s.reply_address(&mom, &other), None);
        // Renumbered UIDs take the new scan position.
        let renumbered = GvStore {
            uidvalidity: Some(8),
            scanned_uid: 3,
            ..Default::default()
        };
        let s = GvStore::save_merge(d, &renumbered).unwrap();
        assert_eq!((s.uidvalidity, s.scanned_uid), (Some(8), 3));
        // A corrupt file reads as empty.
        fs::write(GvStore::path(d), "{not json").unwrap();
        assert_eq!(GvStore::read(d), GvStore::default());
    }

    #[test]
    fn concurrent_saves_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8u32)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || {
                    let mut s = GvStore::default();
                    s.contacts
                        .insert(format!("+1555123456{i}"), entry(ADDR, 100, i));
                    GvStore::save_merge(&d, &s).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(GvStore::read(&d).contacts.len(), 8);
    }

    #[test]
    fn failed_save_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let first = GvStore {
            uidvalidity: Some(7),
            scanned_uid: 10,
            ..Default::default()
        };
        GvStore::save_merge(d, &first).unwrap();
        // Something in the way of the temporary file.
        fs::create_dir(GvStore::path(d).with_extension("tmp")).unwrap();
        let next = GvStore {
            uidvalidity: Some(7),
            scanned_uid: 20,
            ..Default::default()
        };
        assert!(GvStore::save_merge(d, &next).is_err());
        assert_eq!(GvStore::read(d).scanned_uid, 10);
    }
}
