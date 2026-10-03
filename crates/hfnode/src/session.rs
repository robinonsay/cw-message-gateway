//! The transaction state machine: authenticated, two-code, read-back-then-commit.
//!
//! ```text
//!  field: CALL 42 code TX MOM ...  ──► node: R 42 TX MOM ... ?      (read-back)
//!  field: OK 43 code               ──► node: SENT 43                (acted on)
//! ```
//!
//! Rules from the design:
//! - A code is accepted only if its sequence number is greater than `last_seq`.
//! - Nothing beyond a read-back is transmitted until a second code commits it.
//! - Retries are idempotent and cost no new codes: re-sending the open repeats the
//!   read-back, re-sending the commit repeats its result.
//! - Silence is the NACK. Anything that fails to parse or authenticate gets no reply.
//! - `last_seq` is saved to disk before acting, so a crash can never let a used code
//!   be replayed.
//!
//! The state machine is pure apart from [`Services`] and the `last_seq` store, so
//! it can be driven by typed text in tests and in `hfnode sim`.

use crate::inbox::Message;
use auth::{SeqStore, Verifier};
use protocol::{chunk, parse, Chunk, Command, FieldMsg, Reply, Vocabulary};
use std::time::{Duration, Instant};

/// What the session needs from the outside world.
pub trait Services {
    /// Deliver `text` to the contact named `dest`.
    fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String>;
    /// Screened inbound messages waiting to be read, oldest first.
    fn ready_messages(&mut self) -> Vec<Message>;
    fn mark_read(&mut self, ids: &[u64]);
    /// A short forecast for `grid`, or the default location.
    fn weather(&mut self, grid: Option<&str>) -> Result<String, String>;
}

/// One transmission, as keying runs with a pause between each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmission {
    pub segments: Vec<String>,
}

impl Transmission {
    fn single(text: String) -> Self {
        Self {
            segments: vec![text],
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
    /// `AGN` is honoured only this long after the last transmission.
    pub again_window: Duration,
}

#[derive(Debug, Clone)]
struct Pending {
    open_seq: u64,
    open_code: String,
    cmd: Command,
    opened_at: Instant,
    read_back: Transmission,
}

#[derive(Debug, Clone)]
struct LastCommit {
    seq: u64,
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
        // Idempotent retry of the open we are already holding.
        if let Some(p) = &self.pending {
            if p.open_seq == seq && p.cmd == cmd && self.verifier.check_code_only(seq, code).is_ok()
            {
                return Outcome::Transmit(p.read_back.clone());
            }
        }
        if let Err(e) = self.verifier.check(seq, code) {
            return Outcome::Silent(format!("open from {call} rejected: {e}"));
        }
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
            Command::Wx { grid } => Reply::ReadBackWx {
                seq,
                grid: grid.clone(),
            },
        };
        let read_back = Transmission::single(reply.render(&self.cfg.node_call));
        log::info!("opened transaction {seq} from {call}: {cmd:?}");
        self.pending = Some(Pending {
            open_seq: seq,
            open_code: code.to_string(),
            cmd,
            opened_at: now,
            read_back: read_back.clone(),
        });
        Outcome::Transmit(read_back)
    }

    fn commit(&mut self, seq: u64, code: &str, _now: Instant, svc: &mut dyn Services) -> Outcome {
        // Idempotent retry of the commit we already acted on.
        if let Some(c) = &self.last_commit {
            if c.seq == seq && self.verifier.check_code_only(seq, code).is_ok() {
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
                    let take = msgs.len().min(self.cfg.max_rx_messages);
                    let mut text = String::new();
                    for (i, m) in msgs[..take].iter().enumerate() {
                        let body = m.screened.as_deref().unwrap_or_default();
                        text.push_str(&format!("NR {} FM {} {} ", i + 1, m.from, body));
                    }
                    if msgs.len() > take {
                        text.push_str(&format!("{} MORE", msgs.len() - take));
                    }
                    let ids: Vec<u64> = msgs[..take].iter().map(|m| m.id).collect();
                    svc.mark_read(&ids);
                    self.chunked(&text, &call)
                }
            }
            Command::Wx { grid } => match svc.weather(grid.as_deref()) {
                Ok(text) => self.chunked(&format!("WX {text}"), &call),
                Err(e) => {
                    log::warn!("weather failed: {e}");
                    (
                        Transmission::single(
                            Reply::Failed {
                                seq,
                                reason: "WX".into(),
                            }
                            .render(&call),
                        ),
                        Vec::new(),
                    )
                }
            },
        };
        self.last_commit = Some(LastCommit {
            seq,
            transmission: transmission.clone(),
            chunks: chunks.clone(),
        });
        Outcome::Transmit(transmission)
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
        (Transmission { segments }, chunks)
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
        fn weather(&mut self, grid: Option<&str>) -> Result<String, String> {
            Ok(format!(
                "{} TODAY SUNNY HI 95 TONIGHT CLEAR LO 60",
                grid.unwrap_or("HOME")
            ))
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
                },
                Vocabulary {
                    field_calls: vec!["W5XXX".into()],
                    contacts: vec!["MOM".into(), "BOB".into()],
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
            let text = text
                .replace("{42}", &self.book.code(42))
                .replace("{43}", &self.book.code(43));
            let text = text
                .replace("{44}", &self.book.code(44))
                .replace("{45}", &self.book.code(45));
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
        assert_eq!(r.stored_seq(), 41);

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
        assert_eq!(r.stored_seq(), 41, "an expired transaction burns no codes");
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
        assert!(r.svc.ready_messages().is_empty());

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
        let text = tx(&r.send(10, "OK 43 {43} K"));
        assert!(text.starts_with("WX DL89 TODAY SUNNY"), "{text}");
    }

    #[test]
    fn garbage_is_silent() {
        let mut r = Rig::new();
        assert!(silent(&r.send(0, "CQ CQ DE K1ABC K")));
        assert!(silent(&r.send(0, "E E T")));
    }
}
