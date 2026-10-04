//! The transaction state machine: authenticated, two-code, read-back-then-commit.
//!
//! ```text
//!  field: CALL 42 code TX MOM ...  ──► node: R 42 TX MOM ... ?      (read-back)
//!  field: OK 43 code               ──► node: SENT 43                (acted on)
//! ```
//!
//! Rules from the design:
//! - A code is accepted only if its sequence number is greater than `last_seq`.
//! - The open code is burned (saved as `last_seq`) when the transaction opens, so a
//!   heard open can never be reused, whether the transaction commits, aborts or
//!   times out.
//! - Nothing beyond a read-back is transmitted until a second code commits it.
//! - Retries are idempotent and cost no new codes: re-sending exactly the pending
//!   open repeats the read-back. Any other open with the pending sequence number or
//!   lower is refused; a valid open on fresh lines replaces the pending one.
//!   Re-sending the commit repeats its result, but only a few times, only shortly
//!   after the commit, and only until a newer transaction opens.
//! - Silence is the NACK. Anything that fails to parse or authenticate gets no reply.
//! - `last_seq` is saved to disk before acting, so a crash can never let a used code
//!   be replayed.
//!
//! The state machine is pure apart from [`Services`] and the `last_seq` store, so
//! it can be driven by typed text in tests and in `hfnode sim`.

use crate::inbox::Message;
use auth::{SeqStore, Verifier};
use protocol::{
    chunk, chunk_count, parse, sanitize, Chunk, Command, FieldMsg, Place, Reply, Vocabulary,
    MAX_CHUNKS,
};
use std::time::{Duration, Instant};

/// What the session needs from the outside world.
pub trait Services {
    /// Deliver `text` to the contact named `dest`.
    fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String>;
    /// Screened inbound messages waiting to be read, oldest first.
    fn ready_messages(&mut self) -> Vec<Message>;
    /// Called by the node once a transmission carrying these messages was keyed.
    fn mark_read(&mut self, ids: &[u64]);
    /// A short forecast for the 4- or 6-character grid square `grid`.
    fn weather(&mut self, grid: &str) -> Result<String, WxError>;
}

/// Why there is no forecast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WxError {
    /// The weather service has nothing for that place (outside NWS coverage), so
    /// asking again will not help. Keyed as `FAIL <seq> WX NO COVERAGE`.
    NoCoverage,
    /// Anything else: not configured, network down, a bad response. Keyed as
    /// `FAIL <seq> WX`.
    Unavailable(String),
}

impl std::fmt::Display for WxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCoverage => write!(f, "no forecast for that place"),
            Self::Unavailable(e) => write!(f, "{e}"),
        }
    }
}

/// One transmission, as keying runs with a pause between each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmission {
    pub segments: Vec<String>,
    /// Inbound messages whose whole text is in `segments`. The node marks them read
    /// only after the transmission was keyed. Empty for anything but an `RX` result.
    pub read_ids: Vec<u64>,
}

impl Transmission {
    fn single(text: String) -> Self {
        Self {
            segments: vec![text],
            read_ids: Vec::new(),
        }
    }

    pub fn text(&self) -> String {
        self.segments.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Transmit(Transmission),
    /// No reply; the string says why, for the log only.
    Silent(String),
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub node_call: String,
    pub pending_timeout: Duration,
    pub chunk_chars: usize,
    /// Most inbound messages read out per `RX`.
    pub max_rx_messages: usize,
    /// `AGN` is honoured only this long after the last transmission, and a repeated
    /// commit only this long after the commit.
    pub again_window: Duration,
    /// Grid square for `WX` sent without a place; `None` without `[weather]`.
    pub wx_default_grid: Option<String>,
    /// `WX <n>` presets, as (number, grid square).
    pub wx_presets: Vec<(u32, String)>,
}

/// How many times a repeated commit is answered before it is ignored.
const MAX_COMMIT_RETRIES: u32 = 3;

/// Ends an inbound message that was too long for one `RX` and was cut.
const CUT_MARKER: &str = "TRUNCATED";

#[derive(Debug, Clone)]
struct Pending {
    open_seq: u64,
    open_code: String,
    cmd: Command,
    /// For `WX`: the grid square the read-back named, which the commit fetches.
    wx_grid: Option<String>,
    opened_at: Instant,
    read_back: Transmission,
}

#[derive(Debug, Clone)]
struct LastCommit {
    seq: u64,
    /// When the commit was acted on; retries do not move it.
    at: Instant,
    retries: u32,
    transmission: Transmission,
    chunks: Vec<Chunk>,
}

#[derive(Debug)]
pub struct Session {
    cfg: SessionConfig,
    vocab: Vocabulary,
    verifier: Verifier,
    store: SeqStore,
    pending: Option<Pending>,
    last_commit: Option<LastCommit>,
    last_tx: Option<(Instant, Transmission, Vec<Chunk>)>,
}

impl Session {
    pub fn new(cfg: SessionConfig, vocab: Vocabulary, verifier: Verifier, store: SeqStore) -> Self {
        Self {
            cfg,
            vocab,
            verifier,
            store,
            pending: None,
            last_commit: None,
            last_tx: None,
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.verifier.last_seq()
    }

    pub fn has_pending(&self, now: Instant) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| now.duration_since(p.opened_at) < self.cfg.pending_timeout)
    }

    /// Handle one decoded field transmission.
    pub fn handle(&mut self, decoded: &str, now: Instant, svc: &mut dyn Services) -> Outcome {
        if let Some(p) = &self.pending {
            if now.duration_since(p.opened_at) >= self.cfg.pending_timeout {
                log::info!("pending transaction {} timed out", p.open_seq);
                self.pending = None;
            }
        }
        let msg = match parse(decoded, &self.vocab) {
            Ok(m) => m,
            Err(e) => return Outcome::Silent(format!("unparsed ({e}): {decoded:?}")),
        };
        let outcome = match msg {
            FieldMsg::Open {
                call,
                seq,
                code,
                cmd,
            } => self.open(&call, seq, &code, cmd, now, svc),
            FieldMsg::Commit { seq, code } => self.commit(seq, &code, now, svc),
            FieldMsg::Abort => match self.pending.take() {
                Some(p) => {
                    log::info!("transaction {} aborted by field", p.open_seq);
                    Outcome::Transmit(Transmission::single(
                        Reply::Aborted.render(&self.cfg.node_call),
                    ))
                }
                None => Outcome::Silent("NO with nothing pending".into()),
            },
            FieldMsg::Again { chunk } => return self.again(chunk, now),
        };
        if let Outcome::Transmit(t) = &outcome {
            let chunks = self
                .last_commit
                .as_ref()
                .filter(|c| c.transmission == *t)
                .map(|c| c.chunks.clone());
            self.last_tx = Some((now, t.clone(), chunks.unwrap_or_default()));
        }
        outcome
    }

    fn open(
        &mut self,
        call: &str,
        seq: u64,
        code: &str,
        cmd: Command,
        now: Instant,
        svc: &mut dyn Services,
    ) -> Outcome {
        // Idempotent retry of the open we are already holding. Any other open with
        // that sequence number or lower is refused, so a heard code can never swap
        // in a different command for the field's OK to commit. Silence, not "R NO":
        // keying a reply to an unauthenticated open would only give an attacker
        // airtime, and the field can still send NO or wait for the timeout.
        if let Some(p) = &self.pending {
            if p.open_seq == seq && p.cmd == cmd && self.verifier.check_code_only(seq, code).is_ok()
            {
                return Outcome::Transmit(p.read_back.clone());
            }
            if seq <= p.open_seq {
                return Outcome::Silent(format!(
                    "open {seq} from {call} refused: transaction {} is pending",
                    p.open_seq
                ));
            }
        }
        // A later open with a valid code replaces a pending transaction: the field
        // may have missed the read-back and moved on to fresh lines. Every code up
        // to the pending open is burned, so a heard one cannot do this, and the
        // field's OK must follow the new open.
        if let Err(e) = self.verifier.check(seq, code) {
            return Outcome::Silent(format!("open from {call} rejected: {e}"));
        }
        if let Some(p) = &self.pending {
            log::info!("open {seq} from {call} replaces transaction {}", p.open_seq);
        }
        // Burn the open code before the read-back: if this write fails, do nothing.
        if let Err(e) = self.store.save(seq) {
            log::error!("cannot save last_seq {seq}: {e}; not opening");
            return Outcome::Silent(format!("state write failed: {e}"));
        }
        self.verifier.commit(seq);
        // A newer transaction ends the repeat of the previous commit.
        self.last_commit = None;
        let mut wx_grid = None;
        let reply = match &cmd {
            Command::Tx { dest, text } => Reply::ReadBackTx {
                seq,
                dest: dest.clone(),
                text: text.clone(),
            },
            Command::Rx => Reply::ReadBackRx {
                seq,
                count: svc.ready_messages().len(),
            },
            Command::Wx { place } => {
                let (preset, grid) = self.wx_place(place.as_ref());
                wx_grid = grid.clone();
                Reply::ReadBackWx { seq, preset, grid }
            }
        };
        let read_back = Transmission::single(reply.render(&self.cfg.node_call));
        log::info!("opened transaction {seq} from {call}: {cmd:?}");
        self.pending = Some(Pending {
            open_seq: seq,
            open_code: code.to_string(),
            cmd,
            wx_grid,
            opened_at: now,
            read_back: read_back.clone(),
        });
        Outcome::Transmit(read_back)
    }

    fn commit(&mut self, seq: u64, code: &str, now: Instant, svc: &mut dyn Services) -> Outcome {
        // Idempotent retry of the commit we already acted on, bounded so that a
        // heard OK cannot make the node key the whole result again and again.
        if let Some(c) = &mut self.last_commit {
            if c.seq == seq && self.verifier.check_code_only(seq, code).is_ok() {
                if now.duration_since(c.at) > self.cfg.again_window
                    || c.retries >= MAX_COMMIT_RETRIES
                {
                    self.last_commit = None;
                    return Outcome::Silent(format!("repeat of commit {seq} no longer honoured"));
                }
                c.retries += 1;
                return Outcome::Transmit(c.transmission.clone());
            }
        }
        let Some(p) = self.pending.clone() else {
            return Outcome::Silent(format!("OK {seq} with nothing pending"));
        };
        if seq <= p.open_seq {
            return Outcome::Silent(format!("OK {seq} is not after open {}", p.open_seq));
        }
        if let Err(e) = self.verifier.check(seq, code) {
            return Outcome::Silent(format!("commit rejected: {e}"));
        }
        // Burn the codes before acting: if this write fails, do nothing.
        if let Err(e) = self.store.save(seq) {
            log::error!("cannot save last_seq {seq}: {e}; not acting");
            return Outcome::Silent(format!("state write failed: {e}"));
        }
        self.verifier.commit(seq);
        self.pending = None;
        log::info!(
            "committed transaction {} with {seq} (open code {})",
            p.open_seq,
            p.open_code
        );

        let call = self.cfg.node_call.clone();
        let (transmission, chunks) = match p.cmd {
            Command::Tx { dest, text } => {
                let reply = match svc.send_message(&dest, &text) {
                    Ok(()) => Reply::Sent { seq },
                    Err(e) => {
                        log::warn!("sending to {dest} failed: {e}");
                        Reply::Failed {
                            seq,
                            reason: "GATEWAY".into(),
                        }
                    }
                };
                (Transmission::single(reply.render(&call)), Vec::new())
            }
            Command::Rx => {
                let msgs = svc.ready_messages();
                if msgs.is_empty() {
                    (
                        Transmission::single(Reply::NoMessages { seq }.render(&call)),
                        Vec::new(),
                    )
                } else {
                    // Messages are not marked read here: the node does that once the
                    // transmission was keyed, using `read_ids`.
                    let (text, ids) = self.rx_batch(&msgs);
                    let (mut t, chunks) = self.chunked(&text, &call);
                    t.read_ids = ids;
                    (t, chunks)
                }
            }
            Command::Wx { .. } => {
                let result = match &p.wx_grid {
                    Some(grid) => svc.weather(grid),
                    None => Err(WxError::Unavailable("no grid square to forecast".into())),
                };
                match result {
                    Ok(text) => self.chunked(&format!("WX {text}"), &call),
                    Err(e) => {
                        log::warn!("weather for {:?} failed: {e}", p.wx_grid);
                        let reason = match e {
                            WxError::NoCoverage => "WX NO COVERAGE",
                            WxError::Unavailable(_) => "WX",
                        };
                        (
                            Transmission::single(
                                Reply::Failed {
                                    seq,
                                    reason: reason.into(),
                                }
                                .render(&call),
                            ),
                            Vec::new(),
                        )
                    }
                }
            }
        };
        self.last_commit = Some(LastCommit {
            seq,
            at: now,
            retries: 0,
            transmission: transmission.clone(),
            chunks: chunks.clone(),
        });
        Outcome::Transmit(transmission)
    }

    /// The preset number (if one was sent) and the grid square a `WX` is for. The
    /// read-back names both, so the operator hears which place the forecast is for.
    fn wx_place(&self, place: Option<&Place>) -> (Option<u32>, Option<String>) {
        match place {
            Some(Place::Grid(g)) => (None, Some(g.clone())),
            Some(Place::Preset(n)) => (
                Some(*n),
                self.cfg
                    .wx_presets
                    .iter()
                    .find(|(p, _)| p == n)
                    .map(|(_, g)| g.clone()),
            ),
            None => (None, self.cfg.wx_default_grid.clone()),
        }
    }

    /// The text of one `RX` result and the ids of the messages it carries in full.
    ///
    /// Messages are taken oldest first while they fit in [`MAX_CHUNKS`] chunks, so
    /// nothing is cut off by [`chunk`]; the rest stay ready and are counted as
    /// `n MORE`. A message too long to fit even on its own is cut and ends with
    /// [`CUT_MARKER`]. It is counted as read: it can never be sent whole, and left
    /// ready it would come back cut on every later `RX` and block the ones behind it.
    fn rx_batch(&self, msgs: &[Message]) -> (String, Vec<u64>) {
        let fits = |t: &str| chunk_count(t, self.cfg.chunk_chars) <= MAX_CHUNKS;
        let more = |left: usize| {
            if left > 0 {
                format!("{left} MORE")
            } else {
                String::new()
            }
        };
        let mut text = String::new();
        let mut ids = Vec::new();
        for (i, m) in msgs.iter().take(self.cfg.max_rx_messages).enumerate() {
            let body = sanitize(m.screened.as_deref().unwrap_or_default());
            let head = format!("NR {} FM {} ", i + 1, m.from);
            let tail = more(msgs.len() - (i + 1));
            let entry = format!("{head}{body} ");
            if fits(&format!("{text}{entry}{tail}")) {
                text.push_str(&entry);
                ids.push(m.id);
                continue;
            }
            if i == 0 {
                // Longest prefix that fits with the marker (sanitize leaves ASCII).
                let fits_cut =
                    |n: usize| fits(&format!("{head}{} {CUT_MARKER} {tail}", &body[..n]));
                let (mut lo, mut hi) = (0, body.len());
                while lo < hi {
                    let mid = (lo + hi).div_ceil(2);
                    if fits_cut(mid) {
                        lo = mid;
                    } else {
                        hi = mid - 1;
                    }
                }
                // Prefer to end on a whole word.
                let n = match body[..lo].rfind(' ') {
                    Some(sp) if lo < body.len() && !body[lo..].starts_with(' ') && fits_cut(sp) => {
                        sp
                    }
                    _ => lo,
                };
                log::warn!(
                    "inbound message {} cut from {} to {n} characters to fit one RX",
                    m.id,
                    body.len()
                );
                text.push_str(&format!("{head}{} {CUT_MARKER} ", body[..n].trim_end()));
                ids.push(m.id);
            }
            break;
        }
        text.push_str(&more(msgs.len() - ids.len()));
        (text, ids)
    }

    fn chunked(&self, text: &str, call: &str) -> (Transmission, Vec<Chunk>) {
        let chunks = chunk(text, self.cfg.chunk_chars);
        let mut segments: Vec<String> = chunks.iter().map(Chunk::render).collect();
        let end = format!("DE {call} K");
        match segments.last_mut() {
            Some(last) => {
                last.push(' ');
                last.push_str(&end);
            }
            None => segments.push(end),
        }
        (
            Transmission {
                segments,
                read_ids: Vec::new(),
            },
            chunks,
        )
    }

    fn again(&mut self, letter: Option<char>, now: Instant) -> Outcome {
        let Some((at, tx, chunks)) = &self.last_tx else {
            return Outcome::Silent("AGN with nothing sent".into());
        };
        if now.duration_since(*at) > self.cfg.again_window {
            return Outcome::Silent("AGN too long after last transmission".into());
        }
        match letter {
            None => Outcome::Transmit(tx.clone()),
            Some(l) => match chunks.iter().find(|c| c.letter == l) {
                Some(c) => Outcome::Transmit(Transmission::single(format!(
                    "{} DE {} K",
                    c.render(),
                    self.cfg.node_call
                ))),
                None => Outcome::Silent(format!("AGN {l}: no such chunk")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::State;
    use auth::CodeBook;

    const KEY: &[u8] = b"session test key";

    #[derive(Default)]
    struct Fake {
        sent: Vec<(String, String)>,
        inbox: Vec<Message>,
        fail_send: bool,
        weather_calls: Vec<String>,
        weather_error: Option<WxError>,
    }

    impl Services for Fake {
        fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String> {
            if self.fail_send {
                return Err("smtp down".into());
            }
            self.sent.push((dest.into(), text.into()));
            Ok(())
        }
        fn ready_messages(&mut self) -> Vec<Message> {
            self.inbox
                .iter()
                .filter(|m| m.state == State::Ready)
                .cloned()
                .collect()
        }
        fn mark_read(&mut self, ids: &[u64]) {
            for m in self.inbox.iter_mut().filter(|m| ids.contains(&m.id)) {
                m.state = State::Read;
            }
        }
        fn weather(&mut self, grid: &str) -> Result<String, WxError> {
            self.weather_calls.push(grid.to_string());
            match &self.weather_error {
                Some(e) => Err(e.clone()),
                None => Ok(format!("{grid} TODAY SUNNY HI 95 TONIGHT CLEAR LO 60")),
            }
        }
    }

    fn msg(id: u64, from: &str, text: &str) -> Message {
        Message {
            id,
            from: from.into(),
            received_unix: id,
            source_id: id.to_string(),
            raw: text.into(),
            screened: Some(text.into()),
            state: State::Ready,
        }
    }

    struct Rig {
        session: Session,
        book: CodeBook,
        svc: Fake,
        t0: Instant,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = SeqStore::new(dir.path().join("last_seq"));
            store.save(41).unwrap();
            let book = CodeBook::new(KEY);
            let session = Session::new(
                SessionConfig {
                    node_call: "N0DE".into(),
                    pending_timeout: Duration::from_secs(600),
                    chunk_chars: 40,
                    max_rx_messages: 5,
                    again_window: Duration::from_secs(900),
                    wx_default_grid: Some("EM10".into()),
                    wx_presets: vec![(1, "DL89IG".into()), (2, "DL89ME".into())],
                },
                Vocabulary {
                    field_calls: vec!["W5XXX".into()],
                    contacts: vec!["MOM".into(), "BOB".into()],
                    presets: vec![1, 2],
                },
                Verifier::new(book.clone(), store.load().unwrap()),
                store,
            );
            Self {
                session,
                book,
                svc: Fake::default(),
                t0: Instant::now(),
                _dir: dir,
            }
        }

        fn send(&mut self, secs: u64, text: &str) -> Outcome {
            let mut text = text.to_string();
            for n in 42..=47 {
                text = text.replace(&format!("{{{n}}}"), &self.book.code(n));
            }
            self.session
                .handle(&text, self.t0 + Duration::from_secs(secs), &mut self.svc)
        }

        fn stored_seq(&self) -> u64 {
            self.session.store.load().unwrap()
        }
    }

    fn tx(o: &Outcome) -> String {
        match o {
            Outcome::Transmit(t) => t.text(),
            Outcome::Silent(why) => panic!("expected a transmission, got silence: {why}"),
        }
    }

    fn silent(o: &Outcome) -> bool {
        matches!(o, Outcome::Silent(_))
    }

    #[test]
    fn spec_example_send_a_message() {
        let mut r = Rig::new();
        let o = r.send(0, "W5XXX 42 {42} TX MOM RUNNING LATE HOME SUN K");
        assert_eq!(tx(&o), "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K");
        assert!(r.svc.sent.is_empty(), "nothing is sent before the commit");
        assert_eq!(r.stored_seq(), 42, "the open code is burned on open");

        let o = r.send(30, "OK 43 {43} K");
        assert_eq!(tx(&o), "SENT 43 DE N0DE K");
        assert_eq!(
            r.svc.sent,
            [("MOM".to_string(), "RUNNING LATE HOME SUN".to_string())]
        );
        assert_eq!(r.stored_seq(), 43);
    }

    #[test]
    fn retries_are_idempotent() {
        let mut r = Rig::new();
        let first = tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        assert_eq!(tx(&r.send(20, "W5XXX 42 {42} TX MOM HI K")), first);
        let sent = tx(&r.send(40, "OK 43 {43} K"));
        assert_eq!(tx(&r.send(60, "OK 43 {43} K")), sent);
        assert_eq!(r.svc.sent.len(), 1, "a repeated commit must not send twice");
    }

    #[test]
    fn replays_and_bad_codes_get_silence() {
        let mut r = Rig::new();
        // Stale sequence number.
        assert!(silent(
            &r.send(0, &format!("W5XXX 41 {} RX K", r.book.code(41)))
        ));
        // Wrong code.
        assert!(silent(&r.send(0, "W5XXX 42 {43} RX K")));
        // Commit with no open.
        assert!(silent(&r.send(0, "OK 43 {43} K")));
        // Complete one transaction, then replay its open: stale, silent.
        tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        tx(&r.send(10, "OK 43 {43} K"));
        assert!(silent(&r.send(20, "W5XXX 42 {42} TX MOM HI K")));
        assert_eq!(r.svc.sent.len(), 1);
    }

    #[test]
    fn commit_must_follow_the_open() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 43 {43} TX MOM HI K"));
        // A valid but lower code cannot commit.
        assert!(silent(&r.send(10, "OK 42 {42} K")));
        assert!(r.svc.sent.is_empty());
        tx(&r.send(20, "OK 44 {44} K"));
        assert_eq!(r.svc.sent.len(), 1);
    }

    #[test]
    fn skipped_lines_are_fine() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 44 {44} TX BOB SKIPPED SOME K"));
        tx(&r.send(10, "OK 45 {45} K"));
        assert_eq!(r.stored_seq(), 45);
    }

    #[test]
    fn pending_times_out() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        assert!(silent(&r.send(601, "OK 43 {43} K")));
        assert!(r.svc.sent.is_empty());
        assert_eq!(r.stored_seq(), 42, "only the open code is burned");
        // The expired open cannot be replayed.
        assert!(silent(&r.send(700, "W5XXX 42 {42} TX MOM HI K")));
    }

    #[test]
    fn abort_drops_the_pending_transaction() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM WRONG K"));
        assert_eq!(tx(&r.send(10, "NO K")), "R NO DE N0DE K");
        assert!(silent(&r.send(20, "OK 43 {43} K")));
        assert!(silent(&r.send(30, "NO K")));
    }

    #[test]
    fn send_failure_is_reported_and_codes_stay_used() {
        let mut r = Rig::new();
        r.svc.fail_send = true;
        tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        assert_eq!(tx(&r.send(10, "OK 43 {43} K")), "FAIL 43 GATEWAY DE N0DE K");
        assert_eq!(r.stored_seq(), 43);
    }

    #[test]
    fn spec_example_read_messages_with_chunks_and_agn() {
        let mut r = Rig::new();
        r.svc.inbox = vec![
            msg(1, "MOM", "DRIVE SAFE CALL WHEN YOU CAN"),
            msg(2, "BOB", "THE GAME WAS POSTPONED TO NEXT SATURDAY AT NOON"),
            msg(3, "MOM", "LOVE YOU"),
        ];
        assert_eq!(
            tx(&r.send(0, "W5XXX 42 {42} RX K")),
            "R 42 3 MSGS ? DE N0DE K"
        );
        let o = r.send(10, "OK 43 {43} K");
        let Outcome::Transmit(t) = &o else { panic!() };
        assert!(t.segments.len() > 1);
        assert!(t.segments[0].ends_with("= A"), "{:?}", t.segments);
        assert!(t.segments.last().unwrap().ends_with("DE N0DE K"));
        assert!(t.text().contains("NR 1 FM MOM DRIVE SAFE"));
        assert_eq!(t.read_ids, [1, 2, 3]);
        assert_eq!(
            r.svc.ready_messages().len(),
            3,
            "the node marks them read only once keyed"
        );

        // Repeat one chunk.
        let b = tx(&r.send(20, "AGN B K"));
        assert!(
            b.starts_with(&t.segments[1][..10]) && b.ends_with("= B DE N0DE K"),
            "{b}"
        );
        // Repeat everything.
        assert_eq!(tx(&r.send(30, "AGN K")), t.text());
        // Too late.
        assert!(silent(&r.send(2000, "AGN K")));
    }

    #[test]
    fn rx_with_nothing_waiting() {
        let mut r = Rig::new();
        assert_eq!(
            tx(&r.send(0, "W5XXX 42 {42} RX K")),
            "R 42 0 MSGS ? DE N0DE K"
        );
        assert_eq!(tx(&r.send(10, "OK 43 {43} K")), "R 43 NIL DE N0DE K");
    }

    #[test]
    fn weather() {
        let mut r = Rig::new();
        assert_eq!(
            tx(&r.send(0, "W5XXX 42 {42} WX DL89 K")),
            "R 42 WX DL89 ? DE N0DE K"
        );
        assert!(
            r.svc.weather_calls.is_empty(),
            "nothing is fetched before OK"
        );
        let text = tx(&r.send(10, "OK 43 {43} K"));
        assert!(text.starts_with("WX DL89 TODAY SUNNY"), "{text}");
        assert_eq!(r.svc.weather_calls, ["DL89"]);
    }

    #[test]
    fn weather_read_back_names_the_grid_it_will_use() {
        let mut r = Rig::new();
        // WX alone: the configured grid, named in the read-back.
        assert_eq!(
            tx(&r.send(0, "W5XXX 42 {42} WX K")),
            "R 42 WX EM10 ? DE N0DE K"
        );
        tx(&r.send(10, "OK 43 {43} K"));
        // A preset: its number and its grid.
        assert_eq!(
            tx(&r.send(20, "W5XXX 44 {44} WX 2 K")),
            "R 44 WX 2 DL89ME ? DE N0DE K"
        );
        // The exact retry repeats the same read-back.
        assert_eq!(
            tx(&r.send(30, "W5XXX 44 {44} WX 2 K")),
            "R 44 WX 2 DL89ME ? DE N0DE K"
        );
        let text = tx(&r.send(40, "OK 45 {45} K"));
        assert!(text.starts_with("WX DL89ME TODAY"), "{text}");
        // A grid split by a long gap is rejoined.
        assert_eq!(
            tx(&r.send(50, "W5XXX 46 {46} WX DL89 IG K")),
            "R 46 WX DL89IG ? DE N0DE K"
        );
        tx(&r.send(60, "OK 47 {47} K"));
        assert_eq!(r.svc.weather_calls, ["EM10", "DL89ME", "DL89IG"]);
    }

    #[test]
    fn unknown_presets_and_bad_grids_get_silence() {
        let mut r = Rig::new();
        assert!(silent(&r.send(0, "W5XXX 42 {42} WX 3 K")));
        assert!(silent(&r.send(0, "W5XXX 42 {42} WX ZZ99 K")));
        assert_eq!(r.stored_seq(), 41, "no code is burned");
        // The same line still works once sent right.
        assert_eq!(
            tx(&r.send(10, "W5XXX 42 {42} WX 1 K")),
            "R 42 WX 1 DL89IG ? DE N0DE K"
        );
    }

    #[test]
    fn weather_failures_say_whether_to_try_again() {
        let mut r = Rig::new();
        r.svc.weather_error = Some(WxError::NoCoverage);
        tx(&r.send(0, "W5XXX 42 {42} WX IO91 K"));
        assert_eq!(
            tx(&r.send(10, "OK 43 {43} K")),
            "FAIL 43 WX NO COVERAGE DE N0DE K"
        );
        r.svc.weather_error = Some(WxError::Unavailable("timed out".into()));
        tx(&r.send(20, "W5XXX 44 {44} WX DL89 K"));
        assert_eq!(tx(&r.send(30, "OK 45 {45} K")), "FAIL 45 WX DE N0DE K");
        assert_eq!(r.stored_seq(), 45, "the codes are used either way");
    }

    #[test]
    fn weather_without_any_grid_fails_without_fetching() {
        let mut r = Rig::new();
        r.session.cfg.wx_default_grid = None;
        assert_eq!(tx(&r.send(0, "W5XXX 42 {42} WX K")), "R 42 WX ? DE N0DE K");
        assert_eq!(tx(&r.send(10, "OK 43 {43} K")), "FAIL 43 WX DE N0DE K");
        assert!(r.svc.weather_calls.is_empty());
    }

    #[test]
    fn garbage_is_silent() {
        let mut r = Rig::new();
        assert!(silent(&r.send(0, "CQ CQ DE K1ABC K")));
        assert!(silent(&r.send(0, "E E T")));
    }

    #[test]
    fn a_heard_open_cannot_replace_the_pending_one() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        // An eavesdropper reuses the heard code with other text.
        assert!(silent(&r.send(10, "W5XXX 42 {42} TX BOB EVIL K")));
        // The exact retry is still answered.
        assert_eq!(
            tx(&r.send(20, "W5XXX 42 {42} TX MOM HI K")),
            "R 42 TX MOM HI ? DE N0DE K"
        );
        tx(&r.send(30, "OK 43 {43} K"));
        assert_eq!(r.svc.sent, [("MOM".to_string(), "HI".to_string())]);
    }

    #[test]
    fn a_fresh_open_replaces_the_pending_one() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM RUNNING LATE K"));
        // The field missed the read-back; its repeat is miscopied, so it is refused.
        assert!(silent(&r.send(10, "W5XXX 42 {42} TX MOM RUNNING LATF K")));
        // It moves on to fresh lines, as the operating guide says.
        assert_eq!(
            tx(&r.send(20, "W5XXX 44 {44} TX MOM RUNNING LATE K")),
            "R 44 TX MOM RUNNING LATE ? DE N0DE K"
        );
        assert_eq!(r.stored_seq(), 44);
        // The old transaction's OK no longer commits anything; the new one's does.
        assert!(silent(&r.send(30, "OK 43 {43} K")));
        assert!(r.svc.sent.is_empty());
        tx(&r.send(40, "OK 45 {45} K"));
        assert_eq!(
            r.svc.sent,
            [("MOM".to_string(), "RUNNING LATE".to_string())]
        );
    }

    #[test]
    fn an_aborted_open_cannot_be_replayed() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
        tx(&r.send(10, "NO K"));
        // Replaying the burned open keys nothing and does not hold the window open.
        assert!(silent(&r.send(20, "W5XXX 42 {42} TX MOM EVIL K")));
        assert!(!r.session.has_pending(r.t0 + Duration::from_secs(20)));
        // Nor can it replace a later transaction.
        tx(&r.send(30, "W5XXX 44 {44} TX MOM HELLO K"));
        assert!(silent(&r.send(40, "W5XXX 42 {42} TX BOB EVIL K")));
        tx(&r.send(50, "OK 45 {45} K"));
        assert_eq!(r.svc.sent, [("MOM".to_string(), "HELLO".to_string())]);
    }

    #[test]
    fn a_repeated_commit_is_bounded() {
        let open_and_commit = || {
            let mut r = Rig::new();
            tx(&r.send(0, "W5XXX 42 {42} TX MOM HI K"));
            let sent = tx(&r.send(10, "OK 43 {43} K"));
            (r, sent)
        };
        // A few repeats only.
        let (mut r, sent) = open_and_commit();
        for i in 0..MAX_COMMIT_RETRIES {
            assert_eq!(tx(&r.send(20 + u64::from(i), "OK 43 {43} K")), sent);
        }
        assert!(silent(&r.send(30, "OK 43 {43} K")));
        assert!(silent(&r.send(31, "OK 43 {43} K")));

        // Only shortly after the commit; repeats do not extend that.
        let (mut r, sent) = open_and_commit();
        assert_eq!(tx(&r.send(800, "OK 43 {43} K")), sent);
        assert!(silent(&r.send(911, "OK 43 {43} K")));

        // Never once a newer transaction has opened.
        let (mut r, _) = open_and_commit();
        tx(&r.send(20, "W5XXX 44 {44} RX K"));
        assert!(silent(&r.send(30, "OK 43 {43} K")));
        tx(&r.send(40, "NO K"));
        assert!(silent(&r.send(50, "OK 43 {43} K")));
        assert_eq!(r.svc.sent.len(), 1);
    }

    #[test]
    fn a_garbled_retry_ending_in_no_does_not_abort() {
        let mut r = Rig::new();
        tx(&r.send(0, "W5XXX 42 {42} TX MOM SAY NO K"));
        // The retry is heard with the callsign garbled beyond tolerance.
        assert!(silent(&r.send(10, "W5XX 42 {42} TX MOM SAY NO K")));
        assert_eq!(tx(&r.send(20, "OK 43 {43} K")), "SENT 43 DE N0DE K");
        assert_eq!(r.svc.sent, [("MOM".to_string(), "SAY NO".to_string())]);
    }

    #[test]
    fn rx_messages_stay_ready_until_the_node_keyed_them() {
        let mut r = Rig::new();
        r.svc.inbox = vec![msg(1, "MOM", "DRIVE SAFE"), msg(2, "BOB", "LOVE YOU")];
        tx(&r.send(0, "W5XXX 42 {42} RX K"));
        let Outcome::Transmit(t) = r.send(10, "OK 43 {43} K") else {
            panic!()
        };
        assert_eq!(t.read_ids, [1, 2]);
        // Keying failed, so the node never marked them: a new RX reads them again.
        assert_eq!(
            tx(&r.send(20, "W5XXX 44 {44} RX K")),
            "R 44 2 MSGS ? DE N0DE K"
        );
        let Outcome::Transmit(t) = r.send(30, "OK 45 {45} K") else {
            panic!()
        };
        assert_eq!(t.read_ids, [1, 2]);
        assert!(t.text().contains("NR 2 FM BOB LOVE"), "{}", t.text());
        // Non-RX results carry no ids.
        tx(&r.send(40, "W5XXX 46 {46} WX K"));
        let Outcome::Transmit(t) = r.send(50, "OK 47 {47} K") else {
            panic!()
        };
        assert!(t.read_ids.is_empty());
    }

    #[test]
    fn rx_sends_only_what_fits_and_cuts_a_message_that_never_can() {
        let mut r = Rig::new();
        // 2000 characters: more than 26 chunks of 40 on its own.
        let long = "WORD ".repeat(400);
        r.svc.inbox = vec![
            msg(1, "MOM", "SHORT ONE"),
            msg(2, "BOB", &long),
            msg(3, "MOM", "LAST"),
        ];
        let rx = |r: &mut Rig, at: u64, open: &str, commit: &str| {
            tx(&r.send(at, open));
            let Outcome::Transmit(t) = r.send(at + 10, commit) else {
                panic!()
            };
            assert!(t.segments.len() <= MAX_CHUNKS, "{}", t.segments.len());
            r.svc.mark_read(&t.read_ids);
            t
        };

        // The long one does not fit after the first: it waits for the next RX.
        let t = rx(&mut r, 0, "W5XXX 42 {42} RX K", "OK 43 {43} K");
        assert_eq!(t.read_ids, [1]);
        assert_eq!(t.text(), "NR 1 FM MOM SHORT ONE 2 MORE = A DE N0DE K");

        // On its own it still does not fit, so it is cut, says so, and is done with.
        let t = rx(&mut r, 100, "W5XXX 44 {44} RX K", "OK 45 {45} K");
        assert_eq!(t.read_ids, [2]);
        let text = t.text();
        assert!(text.starts_with("NR 1 FM BOB WORD WORD"), "{text}");
        assert!(text.contains("WORD TRUNCATED 1 MORE = "), "{text}");
        assert!(!text.contains(" MORE MORE"), "{text}");

        let t = rx(&mut r, 200, "W5XXX 46 {46} RX K", "OK 47 {47} K");
        assert_eq!(t.read_ids, [3]);
        assert!(r.svc.ready_messages().is_empty());
    }
}
