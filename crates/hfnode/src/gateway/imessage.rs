//! iMessage through Messages on the node's Mac, as the Apple ID signed in there.
//!
//! Sending runs `osascript` with a fixed script that is given the handle and the
//! text as arguments (never written into the script, so no text can change what it
//! does), then looks in Messages' database for the sent message to see whether
//! Apple's server took it.
//!
//! Replies are read from Messages' database (`~/Library/Messages/chat.db`), opened
//! read-only. That database holds all of the owner's conversations, so the node
//! selects only rows from contacts' handles, takes only one-to-one iMessages dated
//! within a reply window that an iMessage sent by TX opened (48 hours by default),
//! and never logs anyone's text.
//!
//! macOS lets a program read the database only with Full Disk Access, and control
//! Messages only with the owner's consent to "Automation". Both are granted to
//! Terminal, so iMessage works only when the node is started by hfnode.command in
//! Terminal (docs/texting.md); under the launchd agent it is turned off.
//!
//! The database layout and the script's terms are recalled from other programs and
//! Apple's scripting dictionary, not checked on the owner's Mac yet: see the checks
//! in docs/texting.md. Everything here compiles and is tested on every system, but
//! `[imessage]` can be configured only on macOS.

use super::typedstream::decode_attributed_body;
use crate::config::{handle_key, Contact, Handle, Imessage};
use crate::inbox::Inbox;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Whether Messages can send for the node now. Not yet checked counts as not ready,
/// so a TX in the node's first seconds goes another way instead of waiting on a
/// consent prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Unknown,
    Ready,
    NotReady(String),
}

/// iMessage state shared by the node's threads and its TX path.
#[derive(Debug)]
pub struct ImShared {
    /// Messages' database could be read at the last check.
    pub db_ok: AtomicBool,
    send: Mutex<Readiness>,
}

impl Default for ImShared {
    fn default() -> Self {
        Self::new()
    }
}

impl ImShared {
    pub fn new() -> Self {
        Self {
            db_ok: AtomicBool::new(false),
            send: Mutex::new(Readiness::Unknown),
        }
    }

    pub fn readiness(&self) -> Readiness {
        self.send.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, r: Readiness) {
        *self.send.lock().unwrap_or_else(|e| e.into_inner()) = r;
    }

    /// `Err` says why iMessage cannot send now.
    pub fn send_ready(&self) -> Result<(), String> {
        match self.readiness() {
            Readiness::Ready => Ok(()),
            Readiness::Unknown => Err("Messages has not been checked yet".into()),
            Readiness::NotReady(why) => Err(why),
        }
    }

    pub fn db_ok(&self) -> bool {
        self.db_ok.load(Ordering::Relaxed)
    }
}

pub const LAUNCHD_ADVICE: &str = "the node was started by the launchd agent, which cannot use \
     iMessage: start it with hfnode.command in Terminal (docs/texting.md)";

/// Set by the launchd agent's plist: macOS's grants for Messages are Terminal's.
pub fn under_launchd() -> bool {
    std::env::var_os("HFNODE_LAUNCHD").is_some_and(|v| v == "1")
}

// ---------------------------------------------------------------------------
// osascript

/// The script that sends: argument 1 is the handle, argument 2 the text.
const SEND_SCRIPT: &[&str] = &[
    "on run argv",
    "tell application \"Messages\"",
    "with timeout of 15 seconds",
    "set a to 1st account whose service type = iMessage",
    "if enabled of a is false then error \"iMessage is turned off in Messages\" number 1001",
    "if connection status of a is not connected then error \"Messages is not connected to iMessage\" number 1002",
    "send (item 2 of argv) to participant (item 1 of argv) of a",
    "end timeout",
    "end tell",
    "end run",
];

/// The same checks as [`SEND_SCRIPT`], sending nothing.
const PROBE_SCRIPT: &[&str] = &[
    "on run argv",
    "tell application \"Messages\"",
    "with timeout of 15 seconds",
    "set a to 1st account whose service type = iMessage",
    "if enabled of a is false then error \"iMessage is turned off in Messages\" number 1001",
    "if connection status of a is not connected then error \"Messages is not connected to iMessage\" number 1002",
    "get id of a",
    "end timeout",
    "end tell",
    "end run",
];

fn script_args(lines: &[&str]) -> Vec<OsString> {
    lines
        .iter()
        .flat_map(|l| [OsString::from("-e"), OsString::from(*l)])
        .collect()
}

/// osascript's arguments for sending `text` to `handle`: the script lines, then the
/// handle and the text as the script's own arguments. The handle never starts with
/// `-` (see [`Handle::parse`]), so option parsing stops at it and the text is passed
/// as it is, even `-5 F AT CAMP`.
pub fn osa_send_args(handle: &Handle, text: &str) -> Vec<OsString> {
    let mut args = script_args(SEND_SCRIPT);
    args.push(handle.as_str().into());
    args.push(text.into());
    args
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsaExit {
    Code(i32),
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct OsaResult {
    pub exit: OsaExit,
    pub stderr: String,
    /// The AppleScript error number, if it reported one.
    pub err_num: Option<i32>,
}

/// Runs osascript, killing it at a deadline: a consent prompt nobody answers would
/// otherwise hold the TX for good.
#[derive(Debug, Clone)]
pub struct OsaRunner {
    pub program: PathBuf,
    pub deadline: Duration,
}

impl OsaRunner {
    pub fn osascript() -> Self {
        Self {
            program: PathBuf::from("/usr/bin/osascript"),
            deadline: Duration::from_secs(20),
        }
    }

    pub fn run(&self, args: &[OsString]) -> std::io::Result<OsaResult> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let start = Instant::now();
        let exit = loop {
            if let Some(status) = child.try_wait()? {
                break OsaExit::Code(status.code().unwrap_or(-1));
            }
            if start.elapsed() >= self.deadline {
                let _ = child.kill();
                let _ = child.wait();
                break OsaExit::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // After a kill the pipe can stay open in a process osascript started, so it
        // is read only after osascript exited by itself.
        let mut stderr = Vec::new();
        if let (OsaExit::Code(_), Some(e)) = (&exit, child.stderr.take()) {
            let _ = e.take(4096).read_to_end(&mut stderr);
        }
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        Ok(OsaResult {
            exit,
            err_num: err_num(&stderr),
            stderr,
        })
    }
}

/// The last `(<number>)` in osascript's error output, e.g. `(-1743)`.
fn err_num(stderr: &str) -> Option<i32> {
    stderr
        .match_indices('(')
        .filter_map(|(i, _)| {
            let rest = &stderr[i + 1..];
            let end = rest.find(')')?;
            rest[..end].parse::<i32>().ok()
        })
        .next_back()
}

const NO_ACCOUNT_ADVICE: &str = "Messages has no working iMessage account: open Messages on the \
     Mac and check it is signed in with iMessage on";
const TIMEOUT_ADVICE: &str = "osascript did not finish in 20 s: a \"Terminal wants access to \
     control Messages\" prompt may be waiting on the Mac's screen, or Messages is stuck; the \
     message may still go out, check before sending it again";

/// What to do about an osascript failure.
fn advice(result: &OsaResult) -> String {
    match (result.exit, result.err_num) {
        (OsaExit::TimedOut, _) | (_, Some(-1712)) => TIMEOUT_ADVICE.into(),
        (_, Some(-1743)) => "macOS has not let this program control Messages: System Settings > \
             Privacy & Security > Automation > Terminal > Messages (the node must be started by \
             hfnode.command)"
            .into(),
        (_, Some(-1719 | -1728 | 1001 | 1002)) => NO_ACCOUNT_ADVICE.into(),
        (OsaExit::Code(c), _) => format!("osascript failed (exit {c}): {}", result.stderr),
    }
}

/// Error numbers after which Messages cannot send until something is fixed.
fn disables_sending(err: Option<i32>) -> bool {
    matches!(err, Some(-1743 | -1719 | 1001 | 1002))
}

// ---------------------------------------------------------------------------
// Messages' database

/// Seconds from 1970 to 2001-01-01, where Messages' dates start.
const APPLE_EPOCH: i64 = 978_307_200;

/// A `message.date` as Unix seconds: nanoseconds since 2001 on current macOS, seconds
/// on old versions.
pub fn apple_to_unix(d: i64) -> Option<u64> {
    let secs = if d > 100_000_000_000 {
        d / 1_000_000_000
    } else if d > 0 {
        d
    } else {
        return None;
    };
    u64::try_from(secs.checked_add(APPLE_EPOCH)?).ok()
}

fn unix_to_apple_ns(u: u64) -> i64 {
    (i64::try_from(u).unwrap_or(i64::MAX) - APPLE_EPOCH).saturating_mul(1_000_000_000)
}

const REQUIRED_COLUMNS: [&str; 11] = [
    "ROWID",
    "guid",
    "text",
    "attributedBody",
    "handle_id",
    "is_from_me",
    "date",
    "service",
    "cache_roomnames",
    "associated_message_type",
    "item_type",
];

/// Messages' database, opened read-only, and which of the columns that only newer
/// macOS versions have it has.
pub struct Db {
    pub conn: Connection,
    cols: Cols,
}

#[derive(Debug, Clone, Copy)]
struct Cols {
    retracted: bool,
    balloon: bool,
    send_style: bool,
}

/// Open Messages' database read-only. It is never copied, and never opened with
/// `immutable` or `nolock`, which would read stale data while Messages writes.
pub fn open_db(path: &Path) -> Result<Db> {
    if !path.is_absolute() {
        bail!("imessage.db {} is not a full path", path.display());
    }
    // SQLite reports both of these as "unable to open database file".
    if let Err(e) = std::fs::File::open(path) {
        match e.kind() {
            std::io::ErrorKind::NotFound => bail!(
                "no Messages database at {}: is Messages signed in for this macOS user, and is \
                 imessage.db right?",
                path.display()
            ),
            std::io::ErrorKind::PermissionDenied => bail!(
                "macOS refused access to {}: turn on Full Disk Access for Terminal (System \
                 Settings > Privacy & Security > Full Disk Access), quit and reopen Terminal, \
                 then start the node with hfnode.command",
                path.display()
            ),
            _ => return Err(e).with_context(|| format!("opening {}", path.display())),
        }
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Messages' database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let columns: HashSet<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(message)")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        names.collect::<rusqlite::Result<_>>()?
    };
    for col in REQUIRED_COLUMNS {
        if !columns.contains(col) {
            bail!(
                "Messages' database has no message.{col}: this macOS version is not supported yet"
            );
        }
    }
    Ok(Db {
        cols: Cols {
            retracted: columns.contains("date_retracted"),
            balloon: columns.contains("balloon_bundle_id"),
            send_style: columns.contains("expressive_send_style_id"),
        },
        conn,
    })
}

fn max_rowid(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(ROWID), 0) FROM message", [], |r| {
            r.get(0)
        })?,
    )
}

fn guid_at(conn: &Connection, rowid: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT guid FROM message WHERE ROWID = ?1", [rowid], |r| {
            r.get(0)
        })
        .optional()?)
}

/// `handle` rows whose id is one of the contacts' handles: rowid -> contact index.
/// Matching is done here, on the handle table only, so no other conversation's rows
/// are read.
pub fn contact_handles(conn: &Connection, contacts: &[Contact]) -> Result<HashMap<i64, usize>> {
    let keys = contact_keys(contacts);
    let mut stmt = conn.prepare("SELECT ROWID, id FROM handle")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut map = HashMap::new();
    for row in rows {
        let (rowid, id) = row?;
        if let Some(&i) = handle_key(&id).and_then(|k| keys.get(&k)) {
            map.insert(rowid, i);
        }
    }
    Ok(map)
}

/// Chat rows whose identifier is one of `contact`'s handles.
fn contact_chats(conn: &Connection, contact: &Contact) -> Result<Vec<i64>> {
    let keys: HashSet<String> = contact.imessage.iter().map(|h| h.to_string()).collect();
    let mut stmt = conn.prepare("SELECT ROWID, chat_identifier FROM chat")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (rowid, id) = row?;
        if id
            .and_then(|i| handle_key(&i))
            .is_some_and(|k| keys.contains(&k))
        {
            out.push(rowid);
        }
    }
    Ok(out)
}

/// Every contact's iMessage handles, as [`handle_key`]s: key -> contact index.
fn contact_keys(contacts: &[Contact]) -> HashMap<String, usize> {
    let mut keys = HashMap::new();
    for (i, c) in contacts.iter().enumerate() {
        for h in &c.imessage {
            keys.entry(h.to_string()).or_insert(i);
        }
    }
    keys
}

fn ints(v: &[i64]) -> String {
    v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// A message's text: `text` without attachment placeholders, or else the text in
/// `attributedBody`. `Ok(None)`: nothing to read (an attachment alone).
fn row_text(text: Option<&str>, body: Option<&[u8]>) -> Result<Option<String>, ()> {
    let plain = text
        .map(|t| t.replace('\u{FFFC}', "").trim().to_string())
        .filter(|t| !t.is_empty());
    if plain.is_some() {
        return Ok(plain);
    }
    match body {
        Some(b) => match decode_attributed_body(b) {
            Ok(t) if t.is_empty() => Ok(None),
            Ok(t) => Ok(Some(t)),
            Err(_) => Err(()),
        },
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Sending

/// The node's own sent message, as Messages recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutRow {
    pub rowid: i64,
    pub guid: String,
    pub is_sent: i64,
    pub is_delivered: i64,
    pub error: i64,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImOutcome {
    /// Apple's server took it.
    Confirmed,
    /// Not sent, for certain: another route may be tried.
    Definite(String),
    /// It may still go out: no other route is tried.
    Uncertain(String),
}

/// What an osascript run and the row it left (if any) say about a send.
pub fn classify(exit: OsaExit, err: Option<&OsaResult>, row: Option<&OutRow>) -> ImOutcome {
    if let Some(r) = row {
        return if r.error == 0 && r.is_sent == 1 && r.service == "iMessage" {
            ImOutcome::Confirmed
        } else if r.error != 0 && r.is_sent == 0 {
            ImOutcome::Definite(format!(
                "Messages marked it not delivered (error {})",
                r.error
            ))
        } else {
            ImOutcome::Uncertain("queued in Messages, not confirmed sent".into())
        };
    }
    let err_num = err.and_then(|e| e.err_num);
    match (exit, err_num) {
        (OsaExit::Code(0), _) => {
            ImOutcome::Uncertain("Messages took the command but no sent message appeared".into())
        }
        (OsaExit::TimedOut, _) | (_, Some(-1712)) => ImOutcome::Uncertain(TIMEOUT_ADVICE.into()),
        (OsaExit::Code(c), _) => ImOutcome::Definite(match err {
            Some(e) => advice(e),
            None => format!("osascript failed (exit {c})"),
        }),
    }
}

/// The result of one iMessage send.
#[derive(Debug, Clone)]
pub struct SendReport {
    pub outcome: ImOutcome,
    pub row: Option<OutRow>,
    pub elapsed: Duration,
}

/// Sends iMessages for the node and records reply windows.
#[derive(Debug, Clone)]
pub struct ImSender {
    pub db: PathBuf,
    pub state_dir: PathBuf,
    pub reply_hours: u64,
    pub runner: OsaRunner,
    pub shared: Arc<ImShared>,
    /// How long to look for the sent message after osascript starts.
    pub confirm_for: Duration,
    pub poll_every: Duration,
}

impl ImSender {
    pub fn new(cfg: &Imessage, state_dir: &Path, shared: Arc<ImShared>) -> Self {
        Self {
            db: cfg.db.clone(),
            state_dir: state_dir.to_path_buf(),
            reply_hours: cfg.reply_hours,
            runner: OsaRunner::osascript(),
            shared,
            confirm_for: Duration::from_secs(30),
            poll_every: Duration::from_millis(500),
        }
    }

    /// Send `text` to `contact` at `handle`.
    pub fn send(&self, contact: &Contact, handle: &Handle, text: &str) -> SendReport {
        let start = Instant::now();
        let report = |outcome, row| SendReport {
            outcome,
            row,
            elapsed: start.elapsed(),
        };
        let db = match open_db(&self.db) {
            Ok(db) => db,
            Err(e) => {
                self.shared.db_ok.store(false, Ordering::Relaxed);
                return report(
                    ImOutcome::Definite(format!("cannot read Messages' database: {e:#}")),
                    None,
                );
            }
        };
        let before = match max_rowid(&db.conn) {
            Ok(n) => n,
            Err(e) => {
                return report(
                    ImOutcome::Definite(format!("cannot read Messages' database: {e:#}")),
                    None,
                )
            }
        };
        let sent_at = super::unix_now();
        let result = match self.runner.run(&osa_send_args(handle, text)) {
            Ok(r) => r,
            // Not started (for one, a NUL in the text): nothing was sent.
            Err(e) => {
                return report(
                    ImOutcome::Definite(format!("could not run osascript: {e}")),
                    None,
                )
            }
        };
        if disables_sending(result.err_num) {
            self.shared.set(Readiness::NotReady(advice(&result)));
        }
        let failed = !matches!(result.exit, OsaExit::Code(0) | OsaExit::TimedOut)
            && result.err_num != Some(-1712);
        let deadline = if failed {
            Instant::now() + Duration::from_secs(2)
        } else {
            start + self.confirm_for
        };
        let mut row = None;
        loop {
            match find_sent(&db, contact, before, text) {
                Ok(Some(r)) => {
                    let done = !matches!(
                        classify(result.exit, Some(&result), Some(&r)),
                        ImOutcome::Uncertain(_)
                    );
                    row = Some(r);
                    if done {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => log::warn!("looking for the sent iMessage failed: {e:#}"),
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(self.poll_every);
        }
        let outcome = classify(result.exit, Some(&result), row.as_ref());
        if !matches!(outcome, ImOutcome::Definite(_)) {
            let record = SentRecord {
                unix: sent_at,
                contact: contact.name.clone(),
                guid: row.as_ref().map(|r| r.guid.clone()),
                outcome: match &outcome {
                    ImOutcome::Confirmed => "confirmed".into(),
                    _ => "uncertain".into(),
                },
            };
            if let Err(e) = open_window(
                &self.state_dir,
                &contact.name,
                sent_at.saturating_sub(1),
                super::unix_now(),
                self.reply_hours,
                record,
            ) {
                log::error!(
                    "could not open the iMessage reply window for {}: {e:#}",
                    contact.name
                );
            }
        }
        report(outcome, row)
    }
}

/// The node's message to `contact` sent after row `before`, matched by its text:
/// the owner's iPhone's own messages to the same person are in the database too.
fn find_sent(db: &Db, contact: &Contact, before: i64, text: &str) -> Result<Option<OutRow>> {
    let handles: Vec<i64> = contact_handles(&db.conn, std::slice::from_ref(contact))?
        .into_keys()
        .collect();
    let chats = contact_chats(&db.conn, contact)?;
    let sql = format!(
        "SELECT m.ROWID, m.guid, m.is_sent, m.is_delivered, m.error, m.service, m.text, m.attributedBody \
         FROM message m \
         WHERE m.is_from_me = 1 AND m.ROWID > ?1 \
           AND (m.handle_id IN ({}) OR m.ROWID IN \
                (SELECT message_id FROM chat_message_join WHERE chat_id IN ({}))) \
         ORDER BY m.ROWID",
        ints(&handles),
        ints(&chats)
    );
    let mut stmt = db.conn.prepare(&sql)?;
    let mut rows = stmt.query([before])?;
    let want = text.trim();
    while let Some(r) = rows.next()? {
        let t: Option<String> = r.get(6)?;
        let body: Option<Vec<u8>> = r.get(7)?;
        if row_text(t.as_deref(), body.as_deref())
            .ok()
            .flatten()
            .as_deref()
            == Some(want)
        {
            return Ok(Some(OutRow {
                rowid: r.get(0)?,
                guid: r.get(1)?,
                is_sent: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                is_delivered: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                error: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                service: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            }));
        }
    }
    Ok(None)
}

/// Check that iMessage can be used, and record it in `shared`: Messages' database,
/// then a script that checks the iMessage account and sends nothing.
pub fn probe(cfg: &Imessage, runner: &OsaRunner, shared: &ImShared) -> Readiness {
    if under_launchd() {
        shared.db_ok.store(false, Ordering::Relaxed);
        let r = Readiness::NotReady(LAUNCHD_ADVICE.into());
        shared.set(r.clone());
        return r;
    }
    let db = open_db(&cfg.db).and_then(|db| {
        db.conn
            .query_row("SELECT 1 FROM message LIMIT 1", [], |_| Ok(()))
            .optional()?;
        Ok(db)
    });
    shared.db_ok.store(db.is_ok(), Ordering::Relaxed);
    let r = match db {
        Err(e) => Readiness::NotReady(format!("{e:#}")),
        Ok(_) => match runner.run(&script_args(PROBE_SCRIPT)) {
            Ok(res) if res.exit == OsaExit::Code(0) => Readiness::Ready,
            Ok(res) => Readiness::NotReady(advice(&res)),
            Err(e) => Readiness::NotReady(format!("could not run osascript: {e}")),
        },
    };
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if term != "Apple_Terminal" {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "iMessage needs the node started by hfnode.command in Terminal (TERM_PROGRAM is {})",
                if term.is_empty() { "not set" } else { &term }
            );
        }
    }
    shared.set(r.clone());
    r
}

// ---------------------------------------------------------------------------
// Reply windows

/// After an iMessage TX to a contact, their iMessages are read for this long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub opened_unix: u64,
    pub until_unix: u64,
}

impl Window {
    pub fn contains(&self, t: u64) -> bool {
        (self.opened_unix..=self.until_unix).contains(&t)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRecord {
    pub unix: u64,
    pub contact: String,
    pub guid: Option<String>,
    pub outcome: String,
}

/// `<state_dir>/imessage_windows.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Windows {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub windows: BTreeMap<String, Window>,
    #[serde(default)]
    pub sent: Vec<SentRecord>,
}

pub const WINDOWS_FILE: &str = "imessage_windows.json";

/// A window is kept this long after it closed, so a node that was down then can
/// still take replies dated inside it.
const KEEP_CLOSED: u64 = 14 * 86_400;
const KEEP_SENT: usize = 50;

impl Windows {
    pub fn read(state_dir: &Path) -> Windows {
        let path = state_dir.join(WINDOWS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::error!("{WINDOWS_FILE} unreadable ({e}); treating as empty");
                Windows::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Windows::default(),
            Err(e) => {
                log::error!("{WINDOWS_FILE} unreadable ({e}); treating as empty");
                Windows::default()
            }
        }
    }

    /// The earliest time any window starts, to bound the reply query.
    pub fn floor(&self) -> Option<u64> {
        self.windows.values().map(|w| w.opened_unix).min()
    }
}

/// Open, or extend, `contact`'s reply window after an iMessage sent at
/// `sent_unix`, and record the send. Locked: `hfnode messages send` may write too.
pub fn open_window(
    state_dir: &Path,
    contact: &str,
    sent_unix: u64,
    now: u64,
    reply_hours: u64,
    record: SentRecord,
) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let _lock = super::google_voice::lock(&state_dir.join("imessage_windows.lock"))?;
    let mut w = Windows::read(state_dir);
    w.version = 1;
    let until = now + reply_hours * 3600;
    let opened = match w.windows.get(contact) {
        Some(old) if old.until_unix >= sent_unix => old.opened_unix,
        _ => sent_unix,
    };
    w.windows.insert(
        contact.to_string(),
        Window {
            opened_unix: opened,
            until_unix: until,
        },
    );
    w.windows
        .retain(|_, win| win.until_unix.saturating_add(KEEP_CLOSED) >= now);
    w.sent.push(record);
    let excess = w.sent.len().saturating_sub(KEEP_SENT);
    w.sent.drain(..excess);
    super::google_voice::write_json(&state_dir.join(WINDOWS_FILE), &w)
}

// ---------------------------------------------------------------------------
// Reading replies

/// `<state_dir>/imessage.json`: how far Messages' database has been read. It
/// belongs to one Mac's database.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub version: u32,
    pub cursor: i64,
    /// The guid of the row at `cursor`, to notice a replaced database.
    #[serde(default)]
    pub anchor_guid: Option<String>,
    #[serde(default)]
    pub last_poll_unix: u64,
    /// Contacts' rows whose text could not be read (the last 20).
    #[serde(default)]
    pub undecoded: Vec<i64>,
}

pub const CURSOR_FILE: &str = "imessage.json";

impl Cursor {
    /// `Ok(None)`: no file, the node has not read this database yet.
    pub fn read(state_dir: &Path) -> Result<Option<Cursor>> {
        let path = state_dir.join(CURSOR_FILE);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(
                serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        super::google_voice::write_json(&state_dir.join(CURSOR_FILE), self)
    }
}

/// One of a contact's incoming rows, as the reply query returns it.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub rowid: i64,
    pub guid: String,
    pub date: i64,
    pub text: Option<String>,
    pub body: Option<Vec<u8>>,
    pub service: Option<String>,
    pub handle_id: i64,
    pub cache_roomnames: Option<String>,
    pub associated_message_type: i64,
    pub item_type: i64,
    pub balloon: Option<String>,
    pub send_style: Option<String>,
    pub retracted: i64,
    pub handle: String,
    pub n_chats: i64,
    pub chat_style: Option<i64>,
    pub chat_identifier: Option<String>,
    pub n_members: Option<i64>,
    pub member: Option<String>,
}

/// What to do with a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Take(String),
    /// Too recent: it may still be unsent. Stop here and look again next time.
    Hold,
    /// From a contact, but its text cannot be read.
    Unreadable,
    Skip(String),
}

/// A message is held this long after it was sent, since iMessage lets the sender
/// unsend it for two minutes.
const HOLD_SECS: u64 = 150;

/// Messages' own link previews: the text is there as usual.
const LINK_PREVIEW: &str = "com.apple.messages.URLBalloonProvider";

/// Whether a row is a one-to-one iMessage, from a contact, in their reply window,
/// and readable. `when` is the row's time, `None` if its date is unusable.
pub fn decide(row: &Row, window: Option<&Window>, when: Option<u64>, now: u64) -> Decision {
    let Some(when) = when.filter(|&t| window.is_some_and(|w| w.contains(t))) else {
        return Decision::Skip("outside reply window".into());
    };
    if row.service.as_deref() != Some("iMessage") {
        return Decision::Skip("not iMessage (SMS/RCS)".into());
    }
    let key = handle_key(&row.handle);
    let one_to_one = row.n_chats == 1
        && row.cache_roomnames.is_none()
        && row.n_members == Some(1)
        && key.is_some()
        && row.member.as_deref().and_then(handle_key) == key
        && row.chat_identifier.as_deref().and_then(handle_key) == key
        && row.chat_style == Some(45);
    if !one_to_one {
        return Decision::Skip(format!(
            "group or unknown chat (chats {}, members {}, style {})",
            row.n_chats,
            row.n_members.map_or("?".into(), |n| n.to_string()),
            row.chat_style.map_or("?".into(), |n| n.to_string())
        ));
    }
    if row.associated_message_type != 0 {
        return Decision::Skip("reaction".into());
    }
    if row.item_type != 0 {
        return Decision::Skip("chat event".into());
    }
    if row.balloon.as_deref().is_some_and(|b| b != LINK_PREVIEW) {
        return Decision::Skip("app message".into());
    }
    if row
        .send_style
        .as_deref()
        .is_some_and(|s| s.contains("invisibleink"))
    {
        return Decision::Skip("invisible ink".into());
    }
    if when > now.saturating_sub(HOLD_SECS) {
        return Decision::Hold;
    }
    if row.retracted != 0 {
        return Decision::Skip("unsent".into());
    }
    match row_text(row.text.as_deref(), row.body.as_deref()) {
        Ok(Some(t)) => Decision::Take(t),
        Ok(None) => Decision::Skip("empty or attachment only".into()),
        Err(()) => Decision::Unreadable,
    }
}

/// A row's time, Unix seconds; a date in the future (clock skew) counts as now.
fn row_time(date: i64, now: u64) -> Option<u64> {
    apple_to_unix(date).map(|t| if t > now + 600 { now } else { t })
}

/// Contacts' incoming rows after `cursor` up to `snap`, dated from `floor` on.
fn reply_rows(
    conn: &Connection,
    cols: Cols,
    handles: &HashMap<i64, usize>,
    cursor: i64,
    snap: i64,
    floor: u64,
) -> Result<Vec<Row>> {
    let handle_ids: Vec<i64> = handles.keys().copied().collect();
    let opt = |present: bool, col: &str, absent: &str| {
        if present {
            format!("m.{col}")
        } else {
            absent.to_string()
        }
    };
    let sql = format!(
        "SELECT m.ROWID, m.guid, m.date, m.text, m.attributedBody, m.service, m.handle_id, \
                m.cache_roomnames, m.associated_message_type, m.item_type, {}, {}, {}, h.id, \
                (SELECT COUNT(*) FROM chat_message_join j2 WHERE j2.message_id = m.ROWID), \
                c.style, c.chat_identifier, \
                (SELECT COUNT(*) FROM chat_handle_join ch WHERE ch.chat_id = c.ROWID), \
                (SELECT h2.id FROM chat_handle_join ch JOIN handle h2 ON h2.ROWID = ch.handle_id \
                  WHERE ch.chat_id = c.ROWID LIMIT 1) \
         FROM message m \
         JOIN handle h ON h.ROWID = m.handle_id \
         LEFT JOIN chat_message_join j ON j.message_id = m.ROWID \
         LEFT JOIN chat c ON c.ROWID = j.chat_id \
         WHERE m.is_from_me = 0 AND m.handle_id IN ({}) \
           AND m.ROWID > ?1 AND m.ROWID <= ?2 AND m.date >= ?3 \
         ORDER BY m.ROWID",
        opt(cols.balloon, "balloon_bundle_id", "NULL"),
        opt(cols.send_style, "expressive_send_style_id", "NULL"),
        opt(cols.retracted, "date_retracted", "0"),
        ints(&handle_ids)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params![cursor, snap, unix_to_apple_ns(floor)],
        |r| {
            Ok(Row {
                rowid: r.get(0)?,
                guid: r.get(1)?,
                date: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                text: r.get(3)?,
                body: r.get(4)?,
                service: r.get(5)?,
                handle_id: r.get(6)?,
                cache_roomnames: r.get(7)?,
                associated_message_type: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                item_type: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                balloon: r.get(10)?,
                send_style: r.get(11)?,
                retracted: r.get::<_, Option<i64>>(12)?.unwrap_or(0),
                handle: r.get(13)?,
                n_chats: r.get(14)?,
                chat_style: r.get(15)?,
                chat_identifier: r.get(16)?,
                n_members: r.get(17)?,
                member: r.get(18)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What one look at Messages' database found: the rows to judge, and where the
/// database ends.
struct Snapshot {
    snap: i64,
    rescan: bool,
    handles: HashMap<i64, usize>,
    rows: Vec<Row>,
}

/// Everything the poll reads, in one short read transaction; none is held across
/// polls, which would keep Messages from checkpointing its log.
fn snapshot(db: &Db, contacts: &[Contact], st: &Cursor, windows: &Windows) -> Result<Snapshot> {
    let tx = db.conn.unchecked_transaction()?;
    let snap = max_rowid(&tx)?;
    let anchor = guid_at(&tx, st.cursor)?;
    let rescan = snap < st.cursor || (st.anchor_guid.is_some() && anchor != st.anchor_guid);
    let cursor = if rescan { 0 } else { st.cursor };
    let handles = contact_handles(&tx, contacts)?;
    let rows = match windows.floor() {
        Some(floor) if !handles.is_empty() => {
            reply_rows(&tx, db.cols, &handles, cursor, snap, floor)?
        }
        _ => Vec::new(),
    };
    tx.commit()?;
    Ok(Snapshot {
        snap,
        rescan,
        handles,
        rows,
    })
}

/// Take contacts' new iMessages into the inbox. Returns how many were added.
///
/// The first time (no `imessage.json`) only notes where the database ends: nothing
/// older is taken. A database that no longer matches the stored position (replaced,
/// restored, or a state folder copied from another Mac) is read again from the start
/// of the reply windows; the inbox drops what it already has.
pub fn poll(
    cfg: &Imessage,
    contacts: &[Contact],
    state_dir: &Path,
    inbox: &Arc<Mutex<Inbox>>,
    now: u64,
) -> Result<usize> {
    let db = open_db(&cfg.db)?;
    let Some(mut st) = Cursor::read(state_dir)? else {
        let snap = max_rowid(&db.conn)?;
        let first = Cursor {
            version: 1,
            cursor: snap,
            anchor_guid: guid_at(&db.conn, snap)?,
            last_poll_unix: now,
            undecoded: Vec::new(),
        };
        first.save(state_dir)?;
        log::info!("iMessage: reading replies from Messages' row {snap} on");
        return Ok(0);
    };
    let windows = Windows::read(state_dir);
    let found = snapshot(&db, contacts, &st, &windows)?;
    if found.rescan {
        log::warn!(
            "Messages' database changed under the iMessage cursor (replaced, moved to another \
             Mac, or that message deleted); rescanning from the reply windows"
        );
    }
    let mut added = 0;
    let mut hold = None;
    let mut last = None;
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for row in &found.rows {
        // A message in more than one chat comes once per chat; the first decides.
        if last == Some(row.rowid) {
            continue;
        }
        last = Some(row.rowid);
        let Some(&ci) = found.handles.get(&row.handle_id) else {
            continue;
        };
        let contact = &contacts[ci];
        let n = &contact.name;
        let when = row_time(row.date, now);
        let add = |source: String, text: &str, at: u64| -> Result<bool> {
            let mut ib = inbox.lock().map_err(|_| anyhow!("inbox lock poisoned"))?;
            ib.add(n, &source, text, at)
        };
        let result = match decide(row, windows.windows.get(n), when, now) {
            Decision::Hold => {
                hold = Some(row.rowid);
                break;
            }
            Decision::Skip(why) => {
                *skipped.entry(why).or_default() += 1;
                Ok(false)
            }
            Decision::Take(text) => {
                let at = super::stamp(when.and_then(|t| i64::try_from(t).ok()), now);
                let r = add(format!("imessage:{}", row.guid), &text, at);
                if matches!(r, Ok(true)) {
                    log::info!("new iMessage from {n} (row {})", row.rowid);
                }
                r
            }
            Decision::Unreadable => {
                log::warn!("iMessage from {n} could not be read (row {})", row.rowid);
                if !st.undecoded.contains(&row.rowid) {
                    st.undecoded.push(row.rowid);
                    let excess = st.undecoded.len().saturating_sub(20);
                    st.undecoded.drain(..excess);
                }
                add(
                    format!("imessage-unreadable:{}", row.guid),
                    super::email::UNREADABLE,
                    now,
                )
            }
        };
        match result {
            Ok(true) => added += 1,
            Ok(false) => {}
            // Never move past a row that is not safely in the inbox.
            Err(e) => {
                st.cursor = row.rowid - 1;
                st.anchor_guid = guid_at(&db.conn, st.cursor)?;
                st.save(state_dir)?;
                return Err(e.context("saving an iMessage in the inbox"));
            }
        }
    }
    if !skipped.is_empty() {
        let counts: Vec<String> = skipped.iter().map(|(w, n)| format!("{n} {w}")).collect();
        log::info!(
            "iMessage rows from contacts not taken: {}",
            counts.join(", ")
        );
    }
    st.version = 1;
    st.cursor = hold.map_or(found.snap, |r| r - 1);
    st.anchor_guid = guid_at(&db.conn, st.cursor)?;
    st.last_poll_unix = now;
    st.save(state_dir)?;
    Ok(added)
}

// ---------------------------------------------------------------------------
// `hfnode messages check`

/// What `hfnode messages check` shows of iMessage: whether Messages is ready, the
/// contacts' handles, reply windows, the cursor, failed sends and contacts' recent
/// rows (their text only inside a window). Nothing is written. Other people's rows
/// are only counted.
pub fn check_report(
    cfg: &Imessage,
    contacts: &[Contact],
    state_dir: &Path,
    readiness: &Readiness,
    since_hours: u64,
    dump: Option<i64>,
    now: u64,
) -> Vec<String> {
    let mut out = vec![match readiness {
        Readiness::Ready => "Messages: ready to send".to_string(),
        Readiness::Unknown => "Messages: not checked".to_string(),
        Readiness::NotReady(why) => format!("Messages: NOT ready: {why}"),
    }];
    let db = match open_db(&cfg.db) {
        Ok(db) => db,
        Err(e) => {
            out.push(format!("database: {e:#}"));
            return out;
        }
    };
    if let Err(e) = check_db(&db, contacts, state_dir, since_hours, dump, now, &mut out) {
        out.push(format!("database: {e:#}"));
    }
    out
}

fn check_db(
    db: &Db,
    contacts: &[Contact],
    state_dir: &Path,
    since_hours: u64,
    dump: Option<i64>,
    now: u64,
    out: &mut Vec<String>,
) -> Result<()> {
    let conn = &db.conn;
    let handles = contact_handles(conn, contacts)?;
    // Handles, by service, and earlier delivered iMessages to them.
    for (ci, c) in contacts.iter().enumerate() {
        for h in &c.imessage {
            let rows: Vec<i64> = handles
                .iter()
                .filter(|&(_, &i)| i == ci)
                .map(|(&r, _)| r)
                .collect();
            let mut services = HashSet::new();
            let mut matched = Vec::new();
            for &r in &rows {
                let (id, service): (String, Option<String>) = conn.query_row(
                    "SELECT id, service FROM handle WHERE ROWID = ?1",
                    [r],
                    |x| Ok((x.get(0)?, x.get(1)?)),
                )?;
                if handle_key(&id).as_deref() == Some(h.as_str()) {
                    services.insert(service.unwrap_or_default());
                    matched.push(r);
                }
            }
            let found = if services.contains("iMessage") {
                "found in Messages (iMessage)"
            } else if services.is_empty() {
                "not found: check the handle"
            } else {
                "found (SMS only)"
            };
            let delivered: i64 = if matched.is_empty() {
                0
            } else {
                conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM message WHERE is_from_me = 1 AND is_delivered = 1 \
                         AND handle_id IN ({})",
                        ints(&matched)
                    ),
                    [],
                    |x| x.get(0),
                )?
            };
            out.push(format!(
                "{} {h}: {found}; {delivered} iMessage(s) delivered to it before",
                c.name
            ));
        }
    }
    // Reply windows.
    let windows = Windows::read(state_dir);
    if windows.windows.is_empty() {
        out.push("reply windows: none (opened by an iMessage TX)".into());
    }
    for (n, w) in &windows.windows {
        let state = if w.until_unix >= now {
            "open"
        } else {
            "closed"
        };
        out.push(format!(
            "reply window {n}: {state}, {} to {} UTC",
            super::date(w.opened_unix),
            super::date(w.until_unix)
        ));
    }
    // Where the node reads from.
    let snap = max_rowid(conn)?;
    out.push(match Cursor::read(state_dir)? {
        None => format!("cursor: none yet: the node starts after row {snap}"),
        Some(st) => {
            let anchor = guid_at(conn, st.cursor)?;
            if snap < st.cursor || (st.anchor_guid.is_some() && anchor != st.anchor_guid) {
                format!(
                    "cursor: would rescan: row {} no longer matches (database replaced or moved)",
                    st.cursor
                )
            } else {
                format!("cursor: ok (row {} of {snap})", st.cursor)
            }
        }
    });
    // The node's own sends that failed or are still not delivered.
    for s in &windows.sent {
        let Some(guid) = &s.guid else { continue };
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT COALESCE(error, 0), COALESCE(is_delivered, 0) FROM message WHERE guid = ?1",
                [guid],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?;
        match row {
            Some((e, _)) if e != 0 => out.push(format!(
                "sent to {} {}: Messages reports error {e}",
                s.contact,
                super::date(s.unix)
            )),
            Some((_, 0)) if now.saturating_sub(s.unix) > 3600 => out.push(format!(
                "sent to {} {}: not delivered after an hour",
                s.contact,
                super::date(s.unix)
            )),
            _ => {}
        }
    }
    // Contacts' recent rows; text only for rows inside a window.
    let floor = now.saturating_sub(since_hours * 3600);
    if !handles.is_empty() {
        let mut rows = reply_rows(conn, db.cols, &handles, 0, snap, floor)?;
        rows.dedup_by_key(|r| r.rowid);
        let skip = rows.len().saturating_sub(20);
        out.push(format!(
            "contacts' iMessage rows in the last {since_hours} h: {}",
            rows.len()
        ));
        for row in &rows[skip..] {
            let Some(&ci) = handles.get(&row.handle_id) else {
                continue;
            };
            let c = &contacts[ci];
            let when = row_time(row.date, now);
            let window = windows.windows.get(&c.name);
            let at = when.map_or("?".into(), super::date);
            let inside = when.is_some_and(|t| window.is_some_and(|w| w.contains(t)));
            let decision = decide(row, window, when, now);
            out.push(if inside {
                let what = match decision {
                    Decision::Take(t) => format!("would take: {t}"),
                    Decision::Hold => "held: too recent, read on a later poll".into(),
                    Decision::Unreadable => {
                        format!(
                            "text not readable: the field would get {}",
                            super::email::UNREADABLE
                        )
                    }
                    Decision::Skip(w) => format!("skipped: {w}"),
                };
                format!("  row {} {} {at}: {what}", row.rowid, c.name)
            } else {
                let decodes = match row_text(row.text.as_deref(), None) {
                    Ok(Some(t)) => format!("{} chars from text", t.chars().count()),
                    _ => match row_text(None, row.body.as_deref()) {
                        Ok(Some(t)) => format!("{} chars from attributedBody", t.chars().count()),
                        Ok(None) => "no text".into(),
                        Err(()) => "attributedBody NOT readable".into(),
                    },
                };
                format!(
                    "  row {} {} {at}: {}; decodes: {decodes}",
                    row.rowid,
                    c.name,
                    match decision {
                        Decision::Skip(w) => w,
                        Decision::Hold => "held".into(),
                        Decision::Take(_) | Decision::Unreadable => "outside reply window".into(),
                    }
                )
            });
        }
    }
    let others: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM message WHERE is_from_me = 0 AND date >= ?1 \
             AND handle_id NOT IN ({})",
            ints(&handles.keys().copied().collect::<Vec<_>>())
        ),
        [unix_to_apple_ns(floor)],
        |x| x.get(0),
    )?;
    out.push(format!("messages from other people (not shown): {others}"));
    if let Some(rowid) = dump {
        let row: Option<(i64, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT handle_id, attributedBody FROM message WHERE ROWID = ?1 AND is_from_me = 0",
                [rowid],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?;
        out.push(match row {
            Some((h, body)) if handles.contains_key(&h) => match body {
                Some(b) => format!(
                    "row {rowid} attributedBody ({} bytes): {}",
                    b.len(),
                    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                ),
                None => format!("row {rowid} has no attributedBody"),
            },
            _ => format!("row {rowid} is not from a contact"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::typedstream::tests::blob;

    /// Recalled, not yet a dump of a real database.
    const DDL: &str = "
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE, id TEXT NOT NULL, service TEXT NOT NULL, UNIQUE (id, service));
        CREATE TABLE message (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL, text TEXT, attributedBody BLOB,
          handle_id INTEGER DEFAULT 0, service TEXT, date INTEGER, is_from_me INTEGER DEFAULT 0, is_sent INTEGER DEFAULT 0,
          is_delivered INTEGER DEFAULT 0, error INTEGER DEFAULT 0, cache_roomnames TEXT, associated_message_type INTEGER DEFAULT 0,
          item_type INTEGER DEFAULT 0 OPTIONAL_COLUMNS);
        CREATE TABLE chat (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL, style INTEGER, chat_identifier TEXT, room_name TEXT);
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER, PRIMARY KEY (chat_id, message_id));
        CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER, UNIQUE (chat_id, handle_id));";

    const OPTIONAL: &str = ", balloon_bundle_id TEXT, expressive_send_style_id TEXT, \
                            date_retracted INTEGER DEFAULT 0, date_edited INTEGER DEFAULT 0";

    /// When MOM's reply window opens.
    const T: u64 = 1_790_000_000;
    const NOW: u64 = T + 4 * 3600;

    fn ns(unix: u64) -> i64 {
        unix_to_apple_ns(unix)
    }

    fn mom() -> Contact {
        Contact {
            name: "MOM".into(),
            imessage: vec![
                Handle::parse("+1 555 123 4567").unwrap(),
                Handle::parse("mom@icloud.com").unwrap(),
            ],
            ..Default::default()
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        cfg: Imessage,
        inbox: Arc<Mutex<Inbox>>,
    }

    impl Fixture {
        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn writer(&self) -> Connection {
            Connection::open(&self.cfg.db).unwrap()
        }

        fn poll(&self, now: u64) -> Result<usize> {
            poll(&self.cfg, &[mom()], &self.state(), &self.inbox, now)
        }

        fn cursor(&self) -> Cursor {
            Cursor::read(&self.state()).unwrap().unwrap()
        }

        fn texts(&self) -> Vec<(String, String)> {
            let ib = Inbox::open(self.state().join("inbox.json")).unwrap();
            ib.unscreened()
                .into_iter()
                .map(|m| (m.source_id, m.raw))
                .collect()
        }

        fn set_cursor(&self, cursor: i64, anchor: Option<&str>) {
            Cursor {
                version: 1,
                cursor,
                anchor_guid: anchor.map(str::to_string),
                ..Default::default()
            }
            .save(&self.state())
            .unwrap();
        }
    }

    type Row = (
        i64,
        i64,
        Option<&'static str>,
        Option<Vec<u8>>,
        i64,
        u64,
        &'static str,
        Option<&'static str>,
        i64,
    );

    /// The 12 rows of the design: rows 3 and 9 are taken, 8 gets a notice, 11 is
    /// held, everything else is skipped or never selected.
    fn fixture(optional: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("chat.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(&DDL.replace(" OPTIONAL_COLUMNS", if optional { OPTIONAL } else { "" }))
            .unwrap();
        c.execute_batch(
            "INSERT INTO handle VALUES (1, '+15551234567', 'iMessage');
             INSERT INTO handle VALUES (2, '+15559990000', 'iMessage');
             INSERT INTO handle VALUES (3, '+15551234567', 'SMS');
             INSERT INTO handle VALUES (4, 'Mom@iCloud.com', 'iMessage');
             INSERT INTO chat VALUES (1, 'c1', 45, '+15551234567', NULL);
             INSERT INTO chat VALUES (2, 'c2', 43, 'chat123', 'chat123');
             INSERT INTO chat VALUES (3, 'c3', 43, 'chat456', NULL);
             INSERT INTO chat VALUES (4, 'c4', 45, '+15551234567', NULL);
             INSERT INTO chat VALUES (5, 'c5', 45, 'mom@icloud.com', NULL);
             INSERT INTO chat VALUES (6, 'c6', 45, '+15559990000', NULL);
             INSERT INTO chat_handle_join VALUES (1, 1), (2, 1), (2, 2), (2, 5), (3, 1), (3, 2),
                                                 (4, 3), (5, 4), (6, 2);",
        )
        .unwrap();
        // ROWID, handle, text, attributedBody, chat, time, service, room name,
        // associated_message_type.
        #[rustfmt::skip]
        let rows: [Row; 12] = [
            (1, 1, Some("old"), None, 1, T - 10 * 86_400, "iMessage", None, 0),
            (2, 2, Some("your code is 123456"), None, 6, T + 3600, "iMessage", None, 0),
            (3, 1, None, Some(blob(b"See you Sunday", 0x94)), 1, T + 2 * 3600, "iMessage", None, 0),
            (4, 1, Some("Family dinner?"), None, 2, T + 2 * 3600, "iMessage", Some("chat123"), 0),
            (5, 1, Some("no roomname"), None, 3, T + 2 * 3600, "iMessage", None, 0),
            (6, 3, Some("sms"), None, 4, T + 2 * 3600, "SMS", None, 0),
            (7, 1, Some("Liked \u{201C}HI\u{201D}"), None, 1, T + 2 * 3600, "iMessage", None, 2001),
            (8, 1, None, Some(b"garbage".to_vec()), 1, T + 2 * 3600, "iMessage", None, 0),
            (9, 4, Some("from email"), None, 5, T + 3 * 3600, "iMessage", None, 0),
            (10, 1, Some("unsent"), None, 1, T + 3 * 3600, "iMessage", None, 0),
            (11, 1, Some("just now"), None, 1, NOW - 30, "iMessage", None, 0),
            (12, 1, Some("resynced"), None, 1, T - 30 * 86_400, "iMessage", None, 0),
        ];
        for (rowid, handle, text, body, chat, at, service, room, amt) in rows {
            c.execute(
                "INSERT INTO message (ROWID, guid, text, attributedBody, handle_id, service, date, \
                 cache_roomnames, associated_message_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![rowid, format!("g{rowid}"), text, body, handle, service, ns(at), room, amt],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_message_join VALUES (?1, ?2)",
                [chat, rowid],
            )
            .unwrap();
        }
        if optional {
            c.execute(
                "UPDATE message SET date_retracted = ?1 WHERE ROWID = 10",
                [ns(T + 3 * 3600 + 60)],
            )
            .unwrap();
        }
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let mut w = Windows::default();
        w.windows.insert(
            "MOM".into(),
            Window {
                opened_unix: T,
                until_unix: T + 48 * 3600,
            },
        );
        crate::gateway::google_voice::write_json(&state.join(WINDOWS_FILE), &w).unwrap();
        let inbox = Arc::new(Mutex::new(Inbox::open(state.join("inbox.json")).unwrap()));
        let cfg = Imessage {
            db,
            poll_secs: 60,
            reply_hours: 48,
            tag: "{call}:".into(),
        };
        Fixture { dir, cfg, inbox }
    }

    #[test]
    fn only_contacts_one_to_one_imessages_in_their_window_are_taken() {
        let f = fixture(true);
        f.set_cursor(0, None);
        assert_eq!(f.poll(NOW).unwrap(), 3);
        assert_eq!(
            f.texts(),
            [
                ("imessage:g3".to_string(), "See you Sunday".to_string()),
                (
                    "imessage-unreadable:g8".into(),
                    crate::gateway::email::UNREADABLE.into()
                ),
                ("imessage:g9".into(), "from email".into()),
            ]
        );
        // Row 11 is too recent: held, and the cursor stops before it.
        let st = f.cursor();
        assert_eq!((st.cursor, st.anchor_guid.as_deref()), (10, Some("g10")));
        assert_eq!(st.undecoded, [8]);
        assert_eq!(f.poll(NOW + 200).unwrap(), 1);
        assert_eq!(f.texts().last().unwrap().1, "just now");
        assert_eq!(f.cursor().cursor, 12);
        // Nothing of the stranger's anywhere.
        for name in ["inbox.json", CURSOR_FILE] {
            let s = std::fs::read_to_string(f.state().join(name)).unwrap();
            assert!(!s.contains("9990000") && !s.contains("123456"), "{name}");
        }
        // Received at the message's own time.
        let ib = Inbox::open(f.state().join("inbox.json")).unwrap();
        assert!(ib
            .unscreened()
            .iter()
            .any(|m| m.received_unix == T + 2 * 3600));
    }

    #[test]
    fn older_databases_without_the_newer_columns_work() {
        let f = fixture(false);
        f.set_cursor(0, None);
        f.poll(NOW).unwrap();
        // No date_retracted: the unsent row cannot be told apart and is taken.
        assert!(f.texts().iter().any(|(_, t)| t == "unsent"));
    }

    #[test]
    fn the_first_look_takes_nothing() {
        let f = fixture(true);
        assert_eq!(f.poll(NOW).unwrap(), 0);
        assert_eq!(f.cursor().cursor, 12);
        assert!(f.texts().is_empty());
        assert_eq!(f.poll(NOW).unwrap(), 0);
    }

    #[test]
    fn a_replaced_database_is_read_again_without_duplicates() {
        let f = fixture(true);
        f.set_cursor(0, None);
        f.poll(NOW + 200).unwrap();
        let before = f.texts();
        // Past the end of the database, then a row that is not the one recorded.
        f.set_cursor(100, None);
        assert_eq!(f.poll(NOW + 200).unwrap(), 0);
        assert_eq!(f.cursor().cursor, 12);
        f.set_cursor(10, Some("other"));
        assert_eq!(f.poll(NOW + 200).unwrap(), 0);
        assert_eq!(f.texts(), before);
    }

    #[test]
    fn a_row_not_saved_is_read_again() {
        let f = fixture(true);
        f.set_cursor(0, None);
        // The inbox cannot save.
        std::fs::create_dir(f.state().join("inbox.tmp")).unwrap();
        assert!(f.poll(NOW).is_err());
        assert_eq!(f.cursor().cursor, 2, "stops before row 3");
        std::fs::remove_dir(f.state().join("inbox.tmp")).unwrap();
        assert_eq!(f.poll(NOW).unwrap(), 3);
    }

    #[test]
    fn no_window_no_rows() {
        let f = fixture(true);
        f.set_cursor(0, None);
        std::fs::remove_file(f.state().join(WINDOWS_FILE)).unwrap();
        assert_eq!(f.poll(NOW).unwrap(), 0);
        assert_eq!(f.cursor().cursor, 12);
    }

    #[test]
    fn check_reads_only() {
        let f = fixture(true);
        f.set_cursor(5, Some("g5"));
        let snapshot = |d: &Path| -> Vec<(String, Vec<u8>)> {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read(e.path()).unwrap(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        let before = snapshot(&f.state());
        let lines = check_report(
            &f.cfg,
            &[mom()],
            &f.state(),
            &Readiness::Ready,
            1000,
            Some(3),
            NOW,
        );
        assert_eq!(snapshot(&f.state()), before);
        let text = lines.join("\n");
        assert!(text.contains("found in Messages (iMessage)"), "{text}");
        assert!(text.contains("would take: See you Sunday"), "{text}");
        assert!(text.contains("cursor: ok (row 5 of 12)"), "{text}");
        assert!(text.contains("row 3 attributedBody"), "{text}");
        // Text outside the window and other people's messages are not shown.
        assert!(
            !text.contains(": old") && !text.contains("resynced"),
            "{text}"
        );
        assert!(
            !text.contains("your code") && !text.contains("9990000"),
            "{text}"
        );
        assert!(
            text.contains("messages from other people (not shown): 1"),
            "{text}"
        );
        assert!(
            text.contains("row 8 MOM 2026-09-21: text not readable"),
            "{text}"
        );
        assert!(text.contains("row 11 MOM 2026-09-21: held"), "{text}");
        let other = check_report(
            &f.cfg,
            &[mom()],
            &f.state(),
            &Readiness::Ready,
            1000,
            Some(2),
            NOW,
        );
        assert!(other.join("\n").contains("row 2 is not from a contact"));
    }

    fn out(is_sent: i64, error: i64, service: &str) -> OutRow {
        OutRow {
            rowid: 20,
            guid: "x".into(),
            is_sent,
            is_delivered: 0,
            error,
            service: service.into(),
        }
    }

    fn osa(code: i32, err_num: Option<i32>) -> OsaResult {
        OsaResult {
            exit: OsaExit::Code(code),
            stderr: String::new(),
            err_num,
        }
    }

    #[test]
    fn send_outcomes() {
        use ImOutcome::*;
        let ok = osa(0, None);
        let c = |exit, r: &OsaResult, row: Option<&OutRow>| classify(exit, Some(r), row);
        assert_eq!(
            c(OsaExit::Code(0), &ok, Some(&out(1, 0, "iMessage"))),
            Confirmed
        );
        assert!(matches!(
            c(OsaExit::Code(0), &ok, Some(&out(0, 22, "iMessage"))),
            Definite(_)
        ));
        assert!(matches!(
            c(OsaExit::Code(0), &ok, Some(&out(0, 0, "iMessage"))),
            Uncertain(_)
        ));
        assert!(matches!(
            c(OsaExit::Code(0), &ok, Some(&out(1, 0, "SMS"))),
            Uncertain(_)
        ));
        assert!(matches!(c(OsaExit::Code(0), &ok, None), Uncertain(_)));
        assert!(matches!(
            c(OsaExit::Code(1), &osa(1, Some(-1712)), None),
            Uncertain(_)
        ));
        assert!(matches!(
            c(OsaExit::TimedOut, &osa(0, None), None),
            Uncertain(_)
        ));
        match c(OsaExit::Code(1), &osa(1, Some(-1743)), None) {
            Definite(e) => assert!(e.contains("Automation"), "{e}"),
            other => panic!("{other:?}"),
        }
        match c(OsaExit::Code(1), &osa(1, Some(1002)), None) {
            Definite(e) => assert!(e.contains("signed in with iMessage"), "{e}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dates() {
        assert_eq!(apple_to_unix(ns(T)), Some(T));
        assert_eq!(apple_to_unix(1000), Some(978_308_200));
        assert_eq!(apple_to_unix(0), None);
        assert_eq!(apple_to_unix(-5), None);
        assert_eq!(row_time(ns(NOW + 3600), NOW), Some(NOW));
        assert_eq!(row_time(ns(NOW + 60), NOW), Some(NOW + 60));
    }

    #[test]
    fn osascript_error_numbers() {
        assert_eq!(
            err_num("123:130: execution error: Not authorized to send Apple events to Messages. (-1743)"),
            Some(-1743)
        );
        assert_eq!(
            err_num("execution error: Messages got an error: x (1002)"),
            Some(1002)
        );
        assert_eq!(err_num("no number here"), None);
    }

    #[test]
    fn the_script_never_holds_the_handle_or_the_text() {
        let h = Handle::parse("+15551234567").unwrap();
        let args = osa_send_args(&h, "-e do shell script \"x\"");
        let n = SEND_SCRIPT.len() * 2;
        assert_eq!(args.len(), n + 2);
        for (i, line) in SEND_SCRIPT.iter().enumerate() {
            assert_eq!(args[2 * i], "-e");
            assert_eq!(args[2 * i + 1], *line);
        }
        assert_eq!(args[n], "+15551234567");
        assert_eq!(args[n + 1], "-e do shell script \"x\"");
    }

    #[cfg(unix)]
    fn fake_osascript(dir: &Path, body: &str) -> OsaRunner {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("osascript");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        OsaRunner {
            program: p,
            deadline: Duration::from_secs(2),
        }
    }

    #[cfg(unix)]
    #[test]
    fn arguments_reach_osascript_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let rec = dir.path().join("argv");
        let r = fake_osascript(
            dir.path(),
            &format!("printf '%s\\0' \"$@\" > '{}'", rec.display()),
        );
        let h = Handle::parse("+15551234567").unwrap();
        for text in [
            "-5 F AT CAMP",
            "\"quote\" \\ back",
            "-e do shell script \"x\"",
            "two\nlines",
        ] {
            let res = r.run(&osa_send_args(&h, text)).unwrap();
            assert_eq!(res.exit, OsaExit::Code(0));
            let got = std::fs::read(&rec).unwrap();
            let parts: Vec<&[u8]> = got.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
            let n = parts.len();
            assert_eq!(parts[n - 2], b"+15551234567");
            assert_eq!(parts[n - 1], text.as_bytes());
        }
    }

    #[cfg(unix)]
    #[test]
    fn osascript_failures_and_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let r = fake_osascript(
            dir.path(),
            "echo '0:10: execution error: Not authorized to send Apple events to Messages. (-1743)' >&2; exit 1",
        );
        let res = r.run(&[]).unwrap();
        assert_eq!((res.exit, res.err_num), (OsaExit::Code(1), Some(-1743)));
        let r = fake_osascript(dir.path(), "sleep 60");
        let start = Instant::now();
        let res = r.run(&[]).unwrap();
        assert_eq!(res.exit, OsaExit::TimedOut);
        assert!(start.elapsed() < r.deadline + Duration::from_secs(1));
    }

    #[cfg(unix)]
    fn sender(f: &Fixture, runner: OsaRunner) -> ImSender {
        let shared = Arc::new(ImShared::new());
        shared.set(Readiness::Ready);
        ImSender {
            db: f.cfg.db.clone(),
            state_dir: f.state(),
            reply_hours: 48,
            runner,
            shared,
            confirm_for: Duration::from_secs(3),
            poll_every: Duration::from_millis(50),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_send_is_confirmed_by_its_own_row() {
        let f = fixture(true);
        let s = sender(&f, fake_osascript(f.dir.path(), "exit 0"));
        // Messages writes the row a moment later; the owner's iPhone has just sent
        // the same person something else.
        let w = f.writer();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            for (rowid, text) in [(13, "from the iPhone"), (14, "W5XXX: HI")] {
                w.execute(
                    "INSERT INTO message (ROWID, guid, text, handle_id, service, date, is_from_me, is_sent) \
                     VALUES (?1, ?2, ?3, 1, 'iMessage', 0, 1, 1)",
                    rusqlite::params![rowid, format!("g{rowid}"), text],
                )
                .unwrap();
            }
        });
        let report = s.send(&mom(), &mom().imessage[0], "W5XXX: HI");
        writer.join().unwrap();
        assert_eq!(report.outcome, ImOutcome::Confirmed);
        assert_eq!(report.row.unwrap().guid, "g14");
        let w = Windows::read(&f.state());
        let win = w.windows["MOM"];
        assert!(win.until_unix >= crate::gateway::unix_now() + 47 * 3600);
        assert_eq!(w.sent.last().unwrap().guid.as_deref(), Some("g14"));
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_send_is_definite_and_stops_imessage() {
        let f = fixture(true);
        let s = sender(
            &f,
            fake_osascript(
                f.dir.path(),
                "echo 'execution error: x (-1743)' >&2; exit 1",
            ),
        );
        let before = std::fs::read(f.state().join(WINDOWS_FILE)).ok();
        let report = s.send(&mom(), &mom().imessage[0], "HI");
        assert!(
            matches!(report.outcome, ImOutcome::Definite(_)),
            "{report:?}"
        );
        assert!(s.shared.send_ready().is_err());
        // Nothing went out, so the reply windows are as they were.
        assert_eq!(std::fs::read(f.state().join(WINDOWS_FILE)).ok(), before);
        // osascript not there at all: nothing was sent either.
        let mut s = s;
        s.runner.program = f.dir.path().join("missing");
        assert!(matches!(
            s.send(&mom(), &mom().imessage[0], "HI").outcome,
            ImOutcome::Definite(_)
        ));
    }

    #[test]
    fn windows_open_and_extend() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let rec = |unix| SentRecord {
            unix,
            contact: "MOM".into(),
            guid: None,
            outcome: "confirmed".into(),
        };
        open_window(d, "MOM", 1000, 1001, 48, rec(1000)).unwrap();
        // Still open: keeps its start, ends later.
        open_window(d, "MOM", 5000, 5001, 48, rec(5000)).unwrap();
        let w = Windows::read(d).windows["MOM"];
        assert_eq!((w.opened_unix, w.until_unix), (1000, 5001 + 48 * 3600));
        // Closed long ago: a new one; others closed for over 14 days are dropped.
        open_window(d, "DAD", 10, 11, 1, rec(10)).unwrap();
        let later = 1001 + 48 * 3600 + 20 * 86_400;
        open_window(d, "MOM", later, later, 48, rec(later)).unwrap();
        let all = Windows::read(d);
        assert_eq!(all.windows["MOM"].opened_unix, later);
        assert!(!all.windows.contains_key("DAD"));
        assert_eq!(all.sent.len(), 4);
    }
}
