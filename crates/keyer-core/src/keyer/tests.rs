use super::*;
use crate::frame::encode;

/// The radio on the box's PTT output, as the PTT line reads it.
#[derive(Debug, Clone, Copy, Default)]
struct Radio {
    /// Switched off, its contact then reading low.
    off: bool,
    /// Something other than the box holds the PTT: the line reads low.
    held: bool,
    /// The box's PTT does not reach the radio (an optocoupler open, a broken
    /// wire, the cable out): the line stays high, on GP19's pull-up.
    open: bool,
}

impl Radio {
    fn line(&self, ptt: bool) -> bool {
        !(self.off || self.held || (ptt && !self.open))
    }
}

/// A box under test, with every output change recorded.
struct Bench {
    k: Keyer,
    changes: Vec<(u64, Pin, bool)>,
    next_id: u8,
    radio: Radio,
    /// Step through every millisecond (reading the PTT line each time, as the
    /// firmware does), or poll only when asked.
    step: bool,
    t: u64,
}

impl Bench {
    fn new() -> Self {
        Self::with(Limits::BOX)
    }

    fn with(limits: Limits) -> Self {
        Self {
            k: Keyer::new(limits, Boot::Power, 0),
            changes: Vec::new(),
            next_id: 1,
            radio: Radio::default(),
            step: false,
            t: 0,
        }
    }

    /// A box on a handheld's PTT, run millisecond by millisecond.
    fn ptt() -> Self {
        Self::ptt_with(Limits::BOX)
    }

    fn ptt_with(limits: Limits) -> Self {
        let mut b = Self::with(limits);
        b.step = true;
        b.k.set_line(true);
        b
    }

    /// Bring the box up to `t`.
    fn to(&mut self, t: u64) {
        let changes = &mut self.changes;
        if self.step {
            while self.t < t {
                self.t += 1;
                self.k.set_line(self.radio.line(self.k.ptt()));
                self.k
                    .poll_with(self.t, |at, p, d| changes.push((at, p, d)));
            }
        } else {
            self.k.set_line(self.radio.line(self.k.ptt()));
            self.k.poll_with(t, |at, p, d| changes.push((at, p, d)));
        }
        self.t = self.t.max(t);
    }

    /// Send `body` at `t`; the reply's body (the id checked and stripped).
    fn send(&mut self, t: u64, body: &str) -> Option<String> {
        self.to(t);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let line = encode(id, format_args!("{body}")).unwrap();
        let changes = &mut self.changes;
        let reply = self
            .k
            .handle_line_with(t, line.as_bytes(), |at, p, d| changes.push((at, p, d)))?;
        let (rid, rbody) = frame::decode(reply.as_bytes()).expect("reply decodes");
        assert_eq!(rid, id, "reply id");
        assert!(reply.len() <= crate::MAX_LINE);
        Some(rbody.to_string())
    }

    fn poll(&mut self, t: u64) -> bool {
        self.to(t);
        self.k.key_down()
    }

    /// `STATUS` every 250 ms up to `t`, as hfnode keeps a run alive.
    fn keep_alive(&mut self, from: u64, t: u64) {
        let mut at = from;
        while at + 250 <= t {
            at += 250;
            self.send(at, "STATUS");
        }
        self.to(t);
    }

    fn lost(&mut self, t: u64) {
        self.to(t);
        let changes = &mut self.changes;
        self.k.link_lost(t, |at, p, d| changes.push((at, p, d)));
    }

    /// The changes of one output.
    fn of(&self, pin: Pin) -> Vec<(u64, bool)> {
        self.changes
            .iter()
            .filter(|c| c.1 == pin)
            .map(|&(at, _, d)| (at, d))
            .collect()
    }

    fn keys(&self) -> Vec<(u64, bool)> {
        self.of(Pin::Key)
    }

    /// The stretches `pin` was on, one still on ending at `end`.
    fn ons(&self, pin: Pin, end: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut since = None;
        for (t, d) in self.of(pin) {
            match (d, since) {
                (true, None) => since = Some(t),
                (false, Some(s)) => {
                    out.push((s, t));
                    since = None;
                }
                _ => panic!("{pin:?} changes do not alternate: {:?}", self.changes),
            }
        }
        if let Some(s) = since {
            out.push((s, end));
        }
        out
    }

    /// Longest stretch `pin` was on, up to `end` for one still on. An off shorter
    /// than one dot at 50 wpm (as between two runs sent back to back) does not
    /// count: the radio's key would barely open, if at all.
    fn longest(&self, pin: Pin, end: u64) -> u64 {
        let mut longest = 0;
        let mut from: Option<u64> = None;
        let mut last_off: Option<u64> = None;
        for (on, off) in self.ons(pin, end) {
            let start = match (from, last_off) {
                (Some(f), Some(l)) if on - l < u64::from(limits::MIN_GAP_MS) => f,
                _ => on,
            };
            from = Some(start);
            last_off = Some(off);
            longest = longest.max(off - start);
        }
        longest
    }

    /// Longest key-down in the record, up to `end` for one still down, bridging
    /// short key-ups.
    fn longest_down(&self, end: u64) -> u64 {
        self.longest(Pin::Key, end)
    }

    /// Time `pin` was on over `from..to`.
    fn on_between(&self, pin: Pin, from: u64, to: u64) -> u64 {
        self.ons(pin, to)
            .iter()
            .map(|&(on, off)| off.min(to).saturating_sub(on.max(from)))
            .sum()
    }

    /// Key-down time over `from..to`.
    fn down_between(&self, from: u64, to: u64) -> u64 {
        self.on_between(Pin::Key, from, to)
    }
}

/// The first six words of a `STATUS` reply: all but the rest, budget, PTT and
/// line.
fn status4(reply: &str) -> String {
    reply.split(' ').take(6).collect::<Vec<_>>().join(" ")
}

/// The key changes `text` should make at `wpm` from `start`.
fn expected(text: &str, wpm: u32, start: u64) -> Vec<(u64, bool)> {
    let dot = u64::from(morse::dot_ms(wpm).unwrap());
    let mut t = start;
    let mut v = Vec::new();
    for s in Segments::of(text.as_bytes()).unwrap().as_slice() {
        v.push((t, s.down));
        t += u64::from(s.units) * dot;
    }
    v.push((t, false));
    v
}

#[test]
fn hello_reports_the_limits_and_uptime() {
    let mut b = Bench::new();
    assert_eq!(
        b.send(5230, "HELLO").unwrap(),
        "OK HELLO 4 60 2000 1000 1000 60 60 5230 POWER - PICO2-KEYER"
    );
    let mut w = Keyer::new(Limits::BOX, Boot::Watchdog, 0).with_build("1a2b3c4d");
    let line = encode(9, format_args!("HELLO")).unwrap();
    let r = w.handle_line(40, line.as_bytes()).unwrap();
    assert!(
        r.as_str().contains(" 40 WATCHDOG 1a2b3c4d PICO2-KEYER*"),
        "{r:?}"
    );
    // A build that is not one word of up to 12 is reported as `?`.
    for bad in ["", "two words", "0123456789abc", "tab\t"] {
        let mut k = Keyer::new(Limits::BOX, Boot::Power, 0).with_build(bad);
        let r = k.handle_line(1, line.as_bytes()).unwrap();
        assert!(
            r.as_str().contains(" POWER ? PICO2-KEYER*"),
            "{bad:?}: {r:?}"
        );
    }
    // The longest HELLO there can be still fits a line.
    let mut k = Keyer::new(Limits::BOX, Boot::Watchdog, 0).with_build("0123456789ab");
    assert!(k.handle_line(u64::MAX / 2, line.as_bytes()).is_some());
}

#[test]
fn cw_keys_its_text_with_standard_timing_and_ends() {
    let mut b = Bench::new();
    assert_eq!(
        b.send(1000, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE NONE 0 60000 0 1"
    );
    assert_eq!(b.send(1000, "CW 20 R 42 ? DE N0DE K").unwrap(), "OK CW");
    assert_eq!(
        b.send(1001, "STATUS").unwrap(),
        "OK STATUS 1 1 NONE NONE 0 59999 0 1"
    );
    // Keep the link alive through the run, as hfnode does.
    let end = 1000 + u64::from(morse::run_ms(b"R 42 ? DE N0DE K", 20).unwrap());
    let mut t = 1000;
    while t < end + 500 {
        t += 250;
        b.send(t, "STATUS");
    }
    assert_eq!(b.keys(), expected("R 42 ? DE N0DE K", 20, 1000));
    assert!(b.of(Pin::Ptt).is_empty() && b.of(Pin::Tone).is_empty());
    assert_eq!(
        status4(&b.send(t, "STATUS").unwrap()),
        "OK STATUS 0 0 DONE NONE"
    );
    // And another after it, once the rest is over.
    assert_eq!(b.send(end + 1000, "CW 25 E").unwrap(), "OK CW");
}

#[test]
fn polling_now_and_then_gives_the_same_key_changes_as_every_millisecond() {
    let text = "CQ CQ DE N0CALL K";
    let mut every = Bench::new();
    let mut sparse = Bench::new();
    every.send(0, &format!("CW 18 {text}"));
    sparse.send(0, &format!("CW 18 {text}"));
    for t in 0..20_000u64 {
        every.poll(t);
        if t % 997 == 0 {
            sparse.poll(t);
        }
        // Keep-alives, at the same moments for both.
        if t % 400 == 0 && t > 0 {
            every.send(t, "STATUS");
            sparse.send(t, "STATUS");
        }
    }
    sparse.poll(20_000);
    every.poll(20_000);
    assert_eq!(every.changes, sparse.changes);
    assert_eq!(every.keys(), expected(text, 18, 0));
}

#[test]
fn stop_opens_the_key_at_once() {
    let mut b = Bench::new();
    b.send(0, "CW 10 TTTT");
    // Mid-dash.
    assert!(b.poll(150));
    assert_eq!(b.send(200, "STOP").unwrap(), "OK STOP");
    assert!(!b.k.key_down());
    assert_eq!(b.keys(), vec![(0, true), (200, false)]);
    assert_eq!(
        b.send(201, "STATUS").unwrap(),
        "OK STATUS 0 0 STOP NONE 999 59801 0 1"
    );
    // STOP with nothing keying is fine too.
    assert_eq!(b.send(300, "STOP").unwrap(), "OK STOP");
    assert_eq!(b.k.ended(), Ended::Stop);
}

#[test]
fn the_link_timeout_ends_a_run_when_the_node_goes_quiet() {
    let mut b = Bench::new();
    // About 13 s of dashes at 5 wpm.
    b.send(0, "CW 5 TTTTTTTTTTTTTTTTTT");
    b.send(1500, "STATUS");
    b.send(3000, "STATUS");
    // A damaged line does not count.
    let mut bad = encode(0x22, format_args!("STATUS"))
        .unwrap()
        .as_bytes()
        .to_vec();
    bad[3] = b'X';
    assert!(b.k.handle_line(4500, &bad).is_none());
    assert!(b.poll(4999));
    b.poll(10_000);
    let last = *b.keys().last().unwrap();
    assert_eq!(last, (5000, false), "2 s after the last good line");
    assert_eq!(b.k.ended(), Ended::Link);
    assert!(!b.k.running());
}

#[test]
fn unplugging_usb_ends_a_run() {
    let mut b = Bench::new();
    b.send(0, "CW 10 TTTT");
    b.lost(50);
    assert_eq!(b.keys(), vec![(0, true), (50, false)]);
    assert!(!b.k.key_down());
    assert_eq!(b.k.ended(), Ended::Usb);
    assert_eq!(
        status4(&b.send(60, "STATUS").unwrap()),
        "OK STATUS 0 0 USB NONE"
    );
}

#[test]
fn cw_refusals() {
    let mut b = Bench::new();
    for (cmd, code) in [
        ("CW", "LEN"),
        ("CW 20", "LEN"),
        ("CW 20 ", "LEN"),
        ("CW 20    ", "LEN"),
        ("CW 4 E", "WPM"),
        ("CW 51 E", "WPM"),
        ("CW 0100 E", "WPM"),
        ("CW X E", "WPM"),
        ("CW  20 E", "WPM"),
        ("CW 20 cq", "CHAR"),
        ("CW 20 A#", "CHAR"),
        ("CW 20 EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE", "LEN"),
        // 30 zeros at 5 wpm: 659 dots of 240 ms, over the minute.
        ("CW 5 000000000000000000000000000000", "LIMIT"),
    ] {
        assert_eq!(b.send(0, cmd).unwrap(), format!("ERR CW {code}"), "{cmd}");
    }
    assert!(b.changes.is_empty(), "nothing keyed");
    assert_eq!(
        b.send(0, "CW 20 EEEEEEEEEEEEEEEEEEEEEEEEEEEEEE").unwrap(),
        "OK CW"
    );
    assert_eq!(b.send(1, "CW 20 E").unwrap(), "ERR CW RUN");
}

#[test]
fn unknown_commands_and_tests_without_a_run() {
    let mut b = Bench::new();
    assert_eq!(b.send(0, "FREQ").unwrap(), "ERR FREQ UNKNOWN");
    assert_eq!(b.send(0, "HELLO THERE").unwrap(), "ERR HELLO UNKNOWN");
    assert_eq!(b.send(0, "cw 20 E").unwrap(), "ERR cw UNKNOWN");
    assert_eq!(
        b.send(0, "ABCDEFGHIJKLMNOPQRSTUVWXYZ").unwrap(),
        "ERR ABCDEFGHIJKL UNKNOWN"
    );
    // A line with an empty body does not decode: ignored.
    assert_eq!(b.send(0, ""), None);
    assert_eq!(b.send(0, "TEST HANG").unwrap(), "ERR TEST RUN");
    assert_eq!(b.send(0, "TEST STUCK").unwrap(), "ERR TEST RUN");
    assert_eq!(b.send(0, "TEST HOLD").unwrap(), "ERR TEST RUN");
    assert_eq!(b.send(0, "TEST FIRE").unwrap(), "ERR TEST UNKNOWN");
    assert!(!b.k.hung());
    assert!(b.changes.is_empty());
}

#[test]
fn a_stuck_key_is_opened_by_the_key_down_limit_and_trips_the_box() {
    let mut b = Bench::new();
    b.send(0, "CW 20 EEEEEEEEEE");
    // Mid-gap: the next element is the one held.
    b.poll(70);
    assert_eq!(b.send(70, "TEST STUCK").unwrap(), "ERR TEST ARM");
    assert_eq!(b.send(70, "TEST ARM").unwrap(), "OK TEST ARM");
    assert_eq!(b.send(70, "TEST STUCK").unwrap(), "OK TEST STUCK");
    let mut t = 70;
    while t < 3000 {
        t += 250;
        b.send(t, "STATUS");
    }
    // Down at 240 (the second dot, after a character gap) and held to the limit.
    assert_eq!(
        b.keys(),
        vec![(0, true), (60, false), (240, true), (1240, false)]
    );
    assert_eq!(b.k.trip(), Trip::Down);
    assert_eq!(
        status4(&b.send(t, "STATUS").unwrap()),
        "OK STATUS 0 0 DOWN DOWN"
    );
    assert_eq!(b.send(t, "CW 20 E").unwrap(), "ERR CW TRIP");
    assert!(b.longest_down(t) <= 1000);
}

#[test]
fn a_hang_freezes_the_box_at_the_next_key_down() {
    let mut b = Bench::new();
    b.send(0, "CW 20 EEEE");
    b.poll(61);
    b.send(61, "TEST ARM");
    assert_eq!(b.send(61, "TEST HANG").unwrap(), "OK TEST HANG");
    assert!(!b.k.hung(), "key up just now");
    b.poll(239);
    assert!(!b.k.hung());
    b.poll(240);
    assert!(b.k.hung() && b.k.key_down());
    // Frozen: nothing more happens here and nothing is answered; only the
    // firmware's watchdog ends it.
    b.poll(60_000);
    assert!(b.k.key_down());
    assert!(b.send(60_000, "STOP").is_none());
    assert_eq!(b.keys(), vec![(0, true), (60, false), (240, true)]);
}

#[test]
fn a_hang_asked_for_while_the_key_is_down_is_at_once() {
    let mut b = Bench::new();
    b.send(0, "CW 20 T");
    b.send(10, "TEST ARM");
    assert_eq!(b.send(10, "TEST HANG").unwrap(), "OK TEST HANG");
    assert!(b.k.hung());
    // A hang still pending when the run ends is dropped.
    let mut c = Bench::new();
    c.send(0, "CW 20 EE");
    c.send(100, "TEST ARM");
    c.send(100, "TEST HANG");
    assert!(!c.k.hung());
    c.send(150, "STOP");
    c.poll(1150);
    assert!(!c.k.hung() && !c.k.key_down());
    assert_eq!(c.send(1150, "CW 20 E").unwrap(), "OK CW");
    assert!(!c.k.hung());
}

/// A small deterministic generator, so the property test needs no crates.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn whatever_arrives_the_key_never_stays_down_past_the_limit() {
    const CHARS: &[u8] = b"ETAOINSHRDLU0123456789?/=+.,@ ";
    let mut rng = Lcg(42);
    for round in 0..300 {
        let mut b = Bench::new();
        let mut t = 0u64;
        let mut stuck = false;
        for _ in 0..40 {
            t += rng.below(700);
            match rng.below(10) {
                0..=2 => {
                    let n = 1 + rng.below(30) as usize;
                    let text: String = (0..n)
                        .map(|_| CHARS[rng.below(CHARS.len() as u64) as usize] as char)
                        .collect();
                    let wpm = 3 + rng.below(50);
                    b.send(t, &format!("CW {wpm} {text}"));
                }
                3 => {
                    b.send(t, "STOP");
                }
                4 => {
                    if rng.below(2) == 0 {
                        b.send(t, "TEST ARM");
                    }
                    stuck |= b.send(t, "TEST STUCK").as_deref() == Some("OK TEST STUCK");
                }
                9 => {
                    // Back to back: a run sent the moment the one before ends.
                    let end = b.changes.last().map_or(t, |&(at, _, _)| at.max(t));
                    let to = end + rng.below(3);
                    b.poll(to);
                    let text = ["T", "0", "TTTT", "00000"][rng.below(4) as usize];
                    b.send(to, &format!("CW 5 {text}"));
                    t = to;
                }
                5 => {
                    b.lost(t);
                }
                6 => {
                    // Silence: let the link timeout act.
                    t += rng.below(3000);
                    b.poll(t);
                }
                _ => {
                    b.send(t, "STATUS");
                }
            }
            if rng.below(5) == 0 {
                b.poll(t);
            }
        }
        b.poll(t + 120_000);
        let longest = b.longest_down(t + 120_000);
        assert!(
            longest <= 1000,
            "round {round}: key down {longest} ms at once"
        );
        assert!(!b.k.key_down(), "round {round}: key left down");
        // Without a stuck test, the key never outlasts a dash at the slowest speed.
        if !stuck {
            assert!(longest <= 720, "round {round}: {longest} ms without a trip");
        }
    }
}

#[test]
fn time_never_runs_backwards_inside() {
    let mut b = Bench::new();
    b.send(1000, "CW 20 TEST");
    // A clock read late on one path and early on another must not undo changes.
    b.poll(1500);
    let before = b.changes.clone();
    b.poll(1200);
    b.send(1100, "STATUS");
    assert_eq!(b.changes, before);
    assert!(b.changes.windows(2).all(|w| w[0].0 <= w[1].0));
}

/// A late clock read must not move the box's time back: here a line stamped
/// before the last poll would otherwise restart the link timeout from the past
/// and end the run retroactively, at a time already polled.
#[test]
fn a_late_clock_read_does_not_undo_time() {
    let mut b = Bench::new();
    // About 13 s of dashes at 5 wpm.
    b.send(0, "CW 5 TTTTTTTTTTTTTTTTTT");
    b.send(1500, "STATUS");
    b.send(3000, "STATUS");
    b.poll(4000);
    // Read late: stamped 1000, after the box has already seen 4000.
    b.send(1000, "STATUS");
    let before = b.changes.clone();
    b.poll(4500);
    assert!(b.k.running(), "the link timeout runs from 4000, not 1000");
    assert!(b.changes[before.len()..]
        .iter()
        .all(|&(at, _, _)| at >= 4000));
    b.poll(6000);
    assert_eq!(b.k.ended(), Ended::Link);
    assert_eq!(*b.keys().last().unwrap(), (6000, false));
}

#[test]
fn a_run_must_wait_out_the_rest_after_the_last() {
    let mut b = Bench::new();
    b.send(0, "CW 20 E");
    b.poll(100);
    assert_eq!(b.keys(), vec![(0, true), (60, false)]);
    assert_eq!(
        b.send(100, "STATUS").unwrap(),
        "OK STATUS 0 0 DONE NONE 960 59980 0 1"
    );
    assert_eq!(b.send(100, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(1059, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(1060, "CW 20 E").unwrap(), "OK CW");
    // However the run ended: a STOP mid-run, the link timeout.
    b.send(1070, "STOP");
    assert_eq!(b.send(2069, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(2070, "CW 5 TTTTTTTT").unwrap(), "OK CW");
    // No keep-alives: the link timeout ends it 2 s after its CW.
    b.poll(4500);
    assert_eq!(b.k.ended(), Ended::Link);
    assert_eq!(b.send(4500, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(5069, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(5070, "CW 20 E").unwrap(), "OK CW");
    // Nothing refused for the rest keyed anything.
    b.poll(6000);
    assert!(b.longest_down(6000) <= 720, "{:?}", b.changes);
}

/// Without the rest (limits made for the test), runs sent back to back hold the
/// key down with key-ups of 0 ms: the key-down limit keeps timing across those
/// and trips the box. A key-up of one dot at 50 wpm is a real one.
#[test]
fn a_key_up_shorter_than_a_dot_does_not_restart_the_key_down_limit() {
    let limits = Limits {
        rest_ms: 0,
        ..Limits::BOX
    };
    let mut b = Bench::with(limits);
    let mut t = 0;
    while t < 5000 && b.k.trip() == Trip::None {
        assert_eq!(b.send(t, "CW 5 T").unwrap(), "OK CW");
        t += 720;
        b.poll(t);
    }
    assert_eq!(b.k.trip(), Trip::Down);
    assert_eq!(b.keys().last(), Some(&(1000, false)));
    assert!(b.longest_down(t) <= 1000);
    // 24 ms between them: no trip.
    let mut b = Bench::with(limits);
    let mut t = 0;
    for _ in 0..20 {
        assert_eq!(b.send(t, "CW 5 T").unwrap(), "OK CW", "at {t}");
        t += 720 + 24;
        b.poll(t);
    }
    assert_eq!(b.k.trip(), Trip::None);
}

/// Runs as long and as dense as the box takes, sent as fast as it takes them for
/// half an hour: the key is down at most 55% of any 10 minutes, and about half
/// of the whole, and the box says why it refuses.
#[test]
fn the_duty_budget_holds_the_key_to_half_the_time() {
    let mut b = Bench::new();
    // 25 zeros at 11 wpm: 59.6 s, the key down 69% of it.
    let text = "0".repeat(25);
    let mut t = 0u64;
    let (mut taken, mut duty) = (0, 0);
    while t < 30 * 60_000 {
        match b.send(t, &format!("CW 11 {text}")).unwrap().as_str() {
            "OK CW" => taken += 1,
            "ERR CW DUTY" => duty += 1,
            "ERR CW REST" | "ERR CW RUN" => {}
            other => panic!("{other}"),
        }
        // Keep-alives, as hfnode sends.
        t += 250;
        b.send(t, "STATUS");
    }
    assert!(
        duty > 0 && taken > 10,
        "{taken} taken, {duty} refused for duty"
    );
    let end = t;
    let mut worst = 0;
    let mut from = 0;
    while from + 600_000 <= end {
        worst = worst.max(b.down_between(from, from + 600_000));
        from += 1000;
    }
    assert!(worst <= 330_000, "{worst} ms down in 10 minutes");
    let total = b.down_between(0, end);
    assert!(total <= end / 2 + 60_000, "{total} ms down in {end}");
    assert!(b.longest_down(end) <= 720);
}

#[test]
fn the_budget_starts_empty_after_a_restart_that_was_not_a_power_up() {
    let mut b = Bench::new();
    b.k = Keyer::new(Limits::BOX, Boot::Other, 0);
    assert_eq!(
        b.send(0, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE NONE 0 0 0 1"
    );
    // One dot at 20 wpm needs 60 ms of budget, earned with the key up.
    assert_eq!(b.send(0, "CW 20 E").unwrap(), "ERR CW DUTY");
    assert_eq!(b.send(59, "CW 20 E").unwrap(), "ERR CW DUTY");
    assert_eq!(b.send(60, "CW 20 E").unwrap(), "OK CW");
    assert_eq!(
        b.send(120, "STATUS").unwrap(),
        "OK STATUS 0 0 DONE NONE 1000 0 0 1"
    );
}

/// The audit's KB-7: after its watchdog fired, the box keys nothing until it is
/// unplugged, whatever stopped its loop.
#[test]
fn a_box_restarted_by_its_watchdog_comes_up_tripped() {
    let mut b = Bench::new();
    b.k = Keyer::new(Limits::BOX, Boot::Watchdog, 0);
    assert_eq!(
        b.send(0, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE WATCHDOG 0 0 0 1"
    );
    assert_eq!(b.send(5000, "CW 20 E").unwrap(), "ERR CW TRIP");
    // A trip it saved before the reset is the reason it gives.
    let saved = Saved {
        trip: Trip::Clock,
        key: false,
        budget: 0,
    };
    b.k = Keyer::restore(Limits::BOX, Boot::Watchdog, 0, Some(saved));
    assert_eq!(b.k.trip(), Trip::Clock);
}

/// Every trip, the key and any budget in range come back from the two words; zero
/// words (a power-up) and any one bit changed in either word come back as nothing.
#[test]
fn the_saved_state_round_trips_and_nothing_else_decodes() {
    assert_eq!(Saved::decode([0, 0]), None);
    assert_eq!(Saved::decode([u32::MAX, u32::MAX]), None);
    for trip in Trip::ALL {
        for key in [false, true] {
            for budget in [i32::MIN, -1000, -1, 0, 1, 59_999, 60_000, i32::MAX] {
                let s = Saved { trip, key, budget };
                let w = s.encode();
                assert_eq!(Saved::decode(w), Some(s));
                for bit in 0..64 {
                    let mut x = w;
                    x[bit / 32] ^= 1 << (bit % 32);
                    assert_eq!(Saved::decode(x), None, "{s:?} bit {bit}");
                }
            }
        }
    }
}

/// A restart that kept the saved state: the trip stays; the budget is the saved
/// one, never more than a fresh start of that kind gives, less the key-down
/// nobody saw if the key was down; and the box rests before its first run.
#[test]
fn a_restart_keeps_the_trip_and_the_spent_budget() {
    let tripped = Saved {
        trip: Trip::Pin,
        key: false,
        budget: 60_000,
    };
    for boot in [Boot::Power, Boot::Other, Boot::Watchdog] {
        let k = Keyer::restore(Limits::BOX, boot, 0, Some(tripped));
        assert_eq!(k.trip(), Trip::Pin, "{boot:?}");
    }
    // A power-up whose saved state survived (a debugger's reset): the budget saved.
    let mut b = Bench::new();
    let spent = Saved {
        trip: Trip::None,
        key: false,
        budget: 20_000,
    };
    b.k = Keyer::restore(Limits::BOX, Boot::Power, 0, Some(spent));
    assert_eq!(
        b.send(0, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE NONE 1000 20000 0 1"
    );
    assert_eq!(b.send(999, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(1000, "CW 20 E").unwrap(), "OK CW");
    // The key was down: 1 s more of key-down taken off.
    let down = Saved {
        trip: Trip::None,
        key: true,
        budget: 20_000,
    };
    let k = Keyer::restore(Limits::BOX, Boot::Power, 0, Some(down));
    assert_eq!(k.saved(0).budget, 20_000 - 1000);
    // Not a power-up: never more than the empty start, and a debt carries over.
    let k = Keyer::restore(Limits::BOX, Boot::Other, 0, Some(spent));
    assert_eq!(k.saved(0).budget, 0);
    let k = Keyer::restore(Limits::BOX, Boot::Other, 0, Some(down));
    assert_eq!(k.saved(0).budget, 0);
    let owed = Saved {
        trip: Trip::None,
        key: true,
        budget: -500,
    };
    let mut b = Bench::new();
    b.k = Keyer::restore(Limits::BOX, Boot::Other, 0, Some(owed));
    assert_eq!(b.k.saved(0).budget, -1500);
    // A dot at 20 wpm (60 ms) needs the 1.5 s debt earned back first.
    assert_eq!(b.send(1000, "CW 20 E").unwrap(), "ERR CW DUTY");
    assert_eq!(b.send(1559, "CW 20 E").unwrap(), "ERR CW DUTY");
    assert_eq!(
        b.send(1559, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE NONE 0 59 0 1"
    );
    assert_eq!(b.send(1560, "CW 20 E").unwrap(), "OK CW");
    // Nothing saved: as `new`.
    let k = Keyer::restore(Limits::BOX, Boot::Power, 0, None);
    assert_eq!((k.trip(), k.saved(0).budget), (Trip::None, 60_000));
}

#[test]
fn tests_are_taken_only_just_after_test_arm_and_once() {
    let mut b = Bench::new();
    b.send(0, "CW 5 TTTTTTTTTTTT");
    assert_eq!(b.send(10, "TEST STUCK").unwrap(), "ERR TEST ARM");
    assert_eq!(b.send(10, "TEST HANG").unwrap(), "ERR TEST ARM");
    assert_eq!(b.send(20, "TEST ARM").unwrap(), "OK TEST ARM");
    // Too late.
    b.send(1000, "STATUS");
    assert_eq!(b.send(2021, "TEST STUCK").unwrap(), "ERR TEST ARM");
    assert_eq!(b.send(2022, "TEST ARM").unwrap(), "OK TEST ARM");
    // A refused test uses the arm up too.
    assert_eq!(b.send(2030, "TEST FIRE").unwrap(), "ERR TEST UNKNOWN");
    assert_eq!(b.send(2040, "TEST STUCK").unwrap(), "OK TEST STUCK");
    assert_eq!(b.send(2050, "TEST HANG").unwrap(), "ERR TEST ARM");
    assert!(!b.k.hung());
    // Arming without a run is fine; the test still needs a run.
    let mut c = Bench::new();
    assert_eq!(c.send(0, "TEST ARM").unwrap(), "OK TEST ARM");
    assert_eq!(c.send(0, "TEST HANG").unwrap(), "ERR TEST RUN");
    // A lost link drops the arm.
    c.send(5, "TEST ARM");
    c.send(10, "CW 5 TTTT");
    c.lost(20);
    assert_eq!(c.send(1500, "CW 20 E").unwrap(), "OK CW");
    assert_eq!(c.send(1500, "TEST HANG").unwrap(), "ERR TEST ARM");
}

/// A run exactly as long as the run limit (limits made for the test: no run at
/// the box's own fits its 60 s exactly) ends as done, with its last element.
#[test]
fn a_run_exactly_the_run_limit_ends_done() {
    let text = "TEST";
    let ms = morse::run_ms(text.as_bytes(), 20).unwrap();
    let limits = Limits {
        run_ms: ms,
        ..Limits::BOX
    };
    let mut b = Bench::with(limits);
    assert_eq!(b.send(0, &format!("CW 20 {text}")).unwrap(), "OK CW");
    b.poll(u64::from(ms) + 10);
    assert_eq!(b.k.ended(), Ended::Done);
    assert_eq!(b.keys(), expected(text, 20, 0));
    // One unit longer is refused; one cut short by the limit ends at it.
    let mut c = Bench::with(Limits {
        run_ms: ms - 60,
        ..Limits::BOX
    });
    assert_eq!(c.send(0, &format!("CW 20 {text}")).unwrap(), "ERR CW LIMIT");
    let mut d = Bench::with(Limits {
        run_ms: ms - 60,
        ..Limits::BOX
    });
    d.k.start(0, " 20 TES", false, &mut |_, _, _| {}).unwrap();
    d.k.run.as_mut().unwrap().segs = Segments::of(text.as_bytes()).unwrap();
    d.poll(u64::from(ms) + 10);
    assert_eq!(d.k.ended(), Ended::Limit);
    assert!(!d.k.key_down());
}

#[test]
fn a_trip_from_the_firmware_opens_the_key_and_sticks() {
    let mut b = Bench::new();
    b.send(0, "CW 5 TTTT");
    b.poll(300);
    let changes = &mut b.changes;
    b.k.trip_now(300, Trip::Pin, |at, p, d| changes.push((at, p, d)));
    assert_eq!(b.keys(), vec![(0, true), (300, false)]);
    assert_eq!(
        status4(&b.send(310, "STATUS").unwrap()),
        "OK STATUS 0 0 DOWN PIN"
    );
    // The first trip's reason stays.
    let changes = &mut b.changes;
    b.k.trip_now(400, Trip::Slow, |at, p, d| changes.push((at, p, d)));
    assert_eq!(b.k.trip(), Trip::Pin);
    assert_eq!(b.send(5000, "CW 20 E").unwrap(), "ERR CW TRIP");
}

// The PTT output and `MCW`: an FM handheld through its headset jack.

/// The tone changes `text` should make at `wpm` in an `MCW` run from `start`.
fn expected_tone(text: &str, wpm: u32, start: u64) -> Vec<(u64, bool)> {
    expected(text, wpm, start + u64::from(mcw::LEAD_MS))
}

/// When an `MCW` run of `text` at `wpm` from `start` lets the PTT up.
fn mcw_end(text: &str, wpm: u32, start: u64) -> u64 {
    start + u64::from(mcw::LEAD_MS + morse::run_ms(text.as_bytes(), wpm).unwrap() + mcw::TAIL_MS)
}

/// The budget field of a `STATUS` reply.
fn budget(status: &str) -> u64 {
    status.split(' ').nth(7).unwrap().parse().unwrap()
}

#[test]
fn mcw_holds_the_ptt_and_keys_its_text_on_the_tone() {
    let mut b = Bench::ptt();
    assert_eq!(
        b.send(1000, "STATUS").unwrap(),
        "OK STATUS 0 0 NONE NONE 0 60000 0 1"
    );
    assert_eq!(b.send(1000, "MCW 20 CQ DE N0DE K").unwrap(), "OK MCW");
    assert_eq!(
        b.send(1050, "STATUS").unwrap(),
        "OK STATUS 0 1 NONE NONE 0 59950 1 0",
        "PTT down, its line low, the budget spent while it is"
    );
    let end = mcw_end("CQ DE N0DE K", 20, 1000);
    b.keep_alive(1050, end + 500);
    assert_eq!(b.of(Pin::Ptt), vec![(1000, true), (end, false)]);
    assert_eq!(b.of(Pin::Tone), expected_tone("CQ DE N0DE K", 20, 1000));
    assert!(b.keys().is_empty(), "the key line is not used");
    // The tone ends 200 ms before the PTT.
    assert_eq!(b.of(Pin::Tone).last().unwrap().0 + 200, end);
    // The budget: spent while the PTT was down (the whole run, a steady
    // carrier), earned back since.
    let left = 60_000 - (end - 1000) + (b.t - end);
    assert_eq!(
        b.send(b.t, "STATUS").unwrap(),
        format!("OK STATUS 0 0 DONE NONE 500 {left} 0 1")
    );
}

#[test]
fn mcw_refusals_and_its_length_under_both_limits() {
    let mut b = Bench::ptt();
    for (cmd, code) in [
        ("MCW", "LEN"),
        ("MCW 20 ", "LEN"),
        ("MCW 4 E", "WPM"),
        ("MCW 20 cq", "CHAR"),
        ("MCW 5 000000000000000000000000000000", "LIMIT"),
    ] {
        assert_eq!(b.send(0, cmd).unwrap(), format!("ERR MCW {code}"), "{cmd}");
    }
    assert!(b.changes.is_empty(), "nothing keyed");
    // Every length either side of the limits: `MCW` takes a run only if its PTT
    // time (lead, Morse, tail) is under the 60 s; `CW` one whose Morse fits it.
    let mut near = 0;
    for wpm in [5, 6, 7, 8] {
        for n in 1..=crate::MAX_TEXT {
            for c in *b"0TM" {
                let text = String::from_utf8(vec![c; n]).unwrap();
                let ms = morse::run_ms(text.as_bytes(), wpm).unwrap();
                let ptt = mcw::LEAD_MS + ms + mcw::TAIL_MS;
                let mut m = Bench::ptt();
                let r = m.send(0, &format!("MCW {wpm} {text}")).unwrap();
                assert_eq!(r == "OK MCW", ptt < limits::PTT_MS, "MCW {wpm} {text}: {r}");
                let mut k = Bench::new();
                let r = k.send(0, &format!("CW {wpm} {text}")).unwrap();
                assert_eq!(r == "OK CW", ms <= limits::RUN_MS, "CW {wpm} {text}: {r}");
                if ms <= limits::RUN_MS && ptt >= limits::PTT_MS {
                    near += 1;
                }
            }
        }
    }
    assert!(near > 0, "some text fits `CW` but not `MCW`");
}

#[test]
fn mcw_waits_for_the_ptt_line_and_spends_the_duty_budget() {
    let mut b = Bench::ptt();
    // Radio off, its contact reading low: nothing keys.
    b.radio.off = true;
    assert_eq!(b.send(10, "MCW 20 E").unwrap(), "ERR MCW LINE");
    assert!(b.changes.is_empty());
    // A 10 s budget: three runs of 3.28 s, each after the rest, not a fourth.
    let mut b = Bench::ptt_with(Limits {
        duty_budget_ms: 10_000,
        ..Limits::BOX
    });
    let need = mcw_end("PARIS", 20, 0);
    assert_eq!(need, 3280);
    let mut t = 0;
    for _ in 0..3 {
        assert_eq!(b.send(t, "MCW 20 PARIS").unwrap(), "OK MCW");
        let end = mcw_end("PARIS", 20, t);
        b.keep_alive(t, end + 1000);
        t = end + 1000;
    }
    assert_eq!(b.send(t, "MCW 20 PARIS").unwrap(), "ERR MCW DUTY");
    // Spent 3 x 3280, earned back 3 x 1000 while resting.
    let left = budget(&b.send(t, "STATUS").unwrap());
    assert_eq!(left, 10_000 - 3 * 3280 + 3 * 1000);
    // Earned back one for one with the PTT up.
    let ready = t + (need - left);
    b.keep_alive(t, ready - 1);
    assert_eq!(b.send(ready - 1, "MCW 20 PARIS").unwrap(), "ERR MCW DUTY");
    assert_eq!(b.send(ready, "MCW 20 PARIS").unwrap(), "OK MCW");
    // One budget for both outputs: a `CW` run after an `MCW` one spends what is
    // left, and a short run fits where a long one does not.
    let mut c = Bench::ptt_with(Limits {
        duty_budget_ms: 4_000,
        ..Limits::BOX
    });
    assert_eq!(c.send(0, "MCW 20 PARIS").unwrap(), "OK MCW");
    let end = mcw_end("PARIS", 20, 0) + 1000;
    c.keep_alive(0, end);
    assert_eq!(budget(&c.send(end, "STATUS").unwrap()), 1720);
    assert_eq!(c.send(end, "MCW 20 PARIS").unwrap(), "ERR MCW DUTY");
    // Five dashes at 5 wpm: 3.6 s with the key down.
    assert_eq!(c.send(end, "CW 5 TTTTT").unwrap(), "ERR CW DUTY");
    assert_eq!(c.send(end, "MCW 20 E").unwrap(), "OK MCW");
}

#[test]
fn mcw_waits_out_the_rest_after_any_run() {
    let mut b = Bench::ptt();
    b.send(0, "CW 20 E");
    b.to(100);
    assert_eq!(b.send(1059, "MCW 20 E").unwrap(), "ERR MCW REST");
    assert_eq!(b.send(1060, "MCW 20 E").unwrap(), "OK MCW");
    let end = mcw_end("E", 20, 1060);
    b.keep_alive(1060, end + 999);
    assert_eq!(b.send(end + 999, "MCW 20 E").unwrap(), "ERR MCW REST");
    assert_eq!(b.send(end + 999, "CW 20 E").unwrap(), "ERR CW REST");
    assert_eq!(b.send(end + 1000, "MCW 20 E").unwrap(), "OK MCW");
}

/// The PTT down is the transmitter keyed, as the key down is: a restart in an
/// `MCW` run's lead (tone off) still takes the unseen time off the duty budget.
#[test]
fn the_saved_state_counts_the_ptt_as_keyed() {
    let mut b = Bench::ptt();
    b.send(0, "MCW 20 E");
    b.to(100);
    assert!(b.k.ptt() && !b.k.key_down());
    let s = b.k.saved(100);
    assert!(s.key, "{s:?}");
    let k = Keyer::restore(Limits::BOX, Boot::Power, 0, Some(s));
    assert_eq!(
        k.saved(0).budget,
        s.budget - i32::try_from(limits::RESTART_CHARGE_MS).unwrap()
    );
}

#[test]
fn the_ptt_limit_trips_the_box_whatever_holds_the_run_open() {
    // `TEST HOLD`: the PTT stays down after the text, until the 60 s limit.
    let mut b = Bench::ptt();
    b.send(0, "MCW 20 E");
    assert_eq!(b.send(100, "TEST HOLD").unwrap(), "ERR TEST ARM");
    b.send(100, "TEST ARM");
    assert_eq!(b.send(100, "TEST HOLD").unwrap(), "OK TEST HOLD");
    b.keep_alive(100, 62_000);
    assert_eq!(b.of(Pin::Ptt), vec![(0, true), (60_000, false)]);
    assert_eq!(b.k.trip(), Trip::Ptt);
    assert_eq!(b.k.ended(), Ended::Ptt);
    let s = b.send(62_000, "STATUS").unwrap();
    assert!(s.starts_with("OK STATUS 0 0 PTT PTT 0 2000 0 1"), "{s}");
    assert_eq!(b.send(62_000, "MCW 20 E").unwrap(), "ERR MCW TRIP");
    assert_eq!(b.send(62_000, "CW 20 E").unwrap(), "ERR CW TRIP");
    // The limit is the PTT's own, not the run's: shorter than the run limit, it
    // still ends the hold at its own time.
    let mut s = Bench::ptt_with(Limits {
        ptt_ms: 10_000,
        ..Limits::BOX
    });
    s.send(0, "MCW 20 E");
    s.send(100, "TEST ARM");
    s.send(100, "TEST HOLD");
    s.keep_alive(100, 12_000);
    assert_eq!(s.of(Pin::Ptt), vec![(0, true), (10_000, false)]);
    assert_eq!(s.k.trip(), Trip::Ptt);
    // And a run that would reach it is refused.
    let mut r = Bench::ptt_with(Limits {
        ptt_ms: 1_000,
        ..Limits::BOX
    });
    assert_eq!(r.send(0, "MCW 20 EEE").unwrap(), "ERR MCW LIMIT");
    // `TEST HOLD` is for `MCW` runs only.
    let mut c = Bench::ptt();
    c.send(0, "CW 20 TTT");
    c.send(1, "TEST ARM");
    assert_eq!(c.send(1, "TEST HOLD").unwrap(), "ERR TEST RUN");
}

#[test]
fn the_run_limit_ends_an_mcw_run_too() {
    // A box with no PTT limit of its own (looser than the run's): the run limit
    // still lets the PTT up at 60 s, without a trip.
    let mut b = Bench::ptt_with(Limits {
        ptt_ms: 120_000,
        ..Limits::BOX
    });
    b.send(0, "MCW 20 E");
    b.send(100, "TEST ARM");
    b.send(100, "TEST HOLD");
    b.keep_alive(100, 61_000);
    assert_eq!(b.of(Pin::Ptt), vec![(0, true), (60_000, false)]);
    assert_eq!((b.k.ended(), b.k.trip()), (Ended::Limit, Trip::None));
}

#[test]
fn stop_link_loss_and_a_quiet_node_let_the_ptt_up_at_once() {
    // STOP mid-element.
    let mut b = Bench::ptt();
    b.send(0, "MCW 10 TTTT");
    b.keep_alive(0, 600);
    assert!(b.k.ptt() && b.k.tone());
    assert_eq!(b.send(650, "STOP").unwrap(), "OK STOP");
    assert!(!b.k.ptt() && !b.k.tone());
    assert_eq!(b.of(Pin::Ptt), vec![(0, true), (650, false)]);
    assert_eq!(b.of(Pin::Tone), vec![(500, true), (650, false)]);
    // USB unplugged.
    let mut u = Bench::ptt();
    u.send(0, "MCW 10 TTTT");
    u.lost(300);
    assert_eq!(u.of(Pin::Ptt), vec![(0, true), (300, false)]);
    assert_eq!(u.k.ended(), Ended::Usb);
    // The node goes quiet: 2 s after its last line.
    let mut q = Bench::ptt();
    q.send(0, "MCW 5 TTTTTTTTTTTT");
    q.send(1500, "STATUS");
    q.to(10_000);
    assert_eq!(q.of(Pin::Ptt), vec![(0, true), (3500, false)]);
    assert_eq!(q.k.ended(), Ended::Link);
    assert!(q.ons(Pin::Tone, 10_000).iter().all(|&(_, up)| up <= 3500));
}

#[test]
fn a_stuck_tone_trips_the_box_and_lets_the_ptt_up() {
    let mut b = Bench::ptt();
    b.send(0, "MCW 20 EEEE");
    // In the lead: the first element is the one held.
    b.send(100, "TEST ARM");
    assert_eq!(b.send(100, "TEST STUCK").unwrap(), "OK TEST STUCK");
    b.keep_alive(100, 3000);
    assert_eq!(b.of(Pin::Tone), vec![(500, true), (1500, false)]);
    assert_eq!(b.of(Pin::Ptt), vec![(0, true), (1500, false)]);
    assert_eq!((b.k.ended(), b.k.trip()), (Ended::Down, Trip::Down));
    assert!(b.longest(Pin::Tone, 3000) <= 1000);
}

#[test]
fn a_hang_in_mcw_freezes_the_box_with_the_ptt_down() {
    let mut b = Bench::ptt();
    b.send(0, "MCW 20 EE");
    b.send(100, "TEST ARM");
    assert_eq!(b.send(100, "TEST HANG").unwrap(), "OK TEST HANG");
    b.to(499);
    assert!(!b.k.hung(), "no element yet");
    b.to(500);
    // Hung at the first element: only the watchdog opens the PTT now.
    assert!(b.k.hung() && b.k.ptt() && b.k.tone());
    b.to(70_000);
    assert!(b.k.ptt());
    assert!(b.send(70_000, "STOP").is_none());
}

#[test]
fn a_ptt_that_does_not_take_ends_the_run_without_a_trip() {
    let mut b = Bench::ptt();
    b.radio.open = true;
    b.send(0, "MCW 20 E");
    b.keep_alive(0, 1000);
    assert_eq!(b.of(Pin::Ptt), vec![(0, true), (100, false)]);
    assert!(b.of(Pin::Tone).is_empty(), "no tone into a radio not keyed");
    assert_eq!((b.k.ended(), b.k.trip()), (Ended::Line, Trip::None));
    // The line read low only before the check, not at it: still not keyed.
    let mut c = Bench::ptt();
    c.send(0, "MCW 20 E");
    c.to(50);
    c.radio.open = true;
    c.to(200);
    assert_eq!(c.k.ended(), Ended::Line);
}

#[test]
fn a_ptt_held_after_the_box_lets_go_trips_it() {
    let mut b = Bench::ptt();
    b.send(0, "MCW 20 E");
    b.keep_alive(0, 300);
    // Something else holds the PTT: a shorted optocoupler, the cable, RF.
    b.radio.held = true;
    let end = mcw_end("E", 20, 0);
    b.keep_alive(300, end + 99);
    assert_eq!(b.k.trip(), Trip::None, "not yet");
    b.to(end + 100);
    assert_eq!(b.k.trip(), Trip::Line);
    assert_eq!(b.k.ended(), Ended::Done);
    let s = b.send(end + 200, "STATUS").unwrap();
    assert!(s.starts_with("OK STATUS 0 0 DONE LINE "), "{s}");
    assert!(s.ends_with(" 0 0"), "PTT up, its line low: {s}");
    b.radio.held = false;
    assert_eq!(b.send(end + 3000, "MCW 20 E").unwrap(), "ERR MCW TRIP");
    assert_eq!(b.send(end + 3000, "CW 20 E").unwrap(), "ERR CW TRIP");
    // A line that reads high for a moment after the release but low at the check
    // is held: it trips too.
    let mut c = Bench::ptt();
    c.send(0, "MCW 20 E");
    let end = mcw_end("E", 20, 0);
    c.keep_alive(0, end + 30);
    c.radio.held = true;
    c.to(end + 100);
    assert_eq!(c.k.trip(), Trip::Line);
}

#[test]
fn nothing_starts_until_the_line_shows_the_ptt_open() {
    let limits = Limits {
        rest_ms: 0,
        ..Limits::BOX
    };
    let mut b = Bench::ptt_with(limits);
    b.send(0, "MCW 20 E");
    let end = mcw_end("E", 20, 0);
    b.keep_alive(0, end - 10);
    // The line is slow back: low for 50 ms after the PTT is up.
    b.radio.held = true;
    b.to(end + 20);
    assert_eq!(b.send(end + 20, "CW 20 E").unwrap(), "ERR CW LINE");
    b.radio.held = false;
    b.to(end + 50);
    // Until the check, nothing starts, even with no rest.
    assert_eq!(b.send(end + 99, "CW 20 E").unwrap(), "ERR CW LINE");
    assert_eq!(b.send(end + 100, "CW 20 E").unwrap(), "OK CW");
    b.to(end + 500);
    assert_eq!(b.k.trip(), Trip::None);
}

#[test]
fn whatever_arrives_the_ptt_and_tone_stay_within_their_limits() {
    const CHARS: &[u8] = b"ETAOINSHRDLU0123456789?/=+.,@ ";
    let mut rng = Lcg(7);
    for round in 0..40 {
        let mut b = Bench::ptt();
        let mut t = 0u64;
        for _ in 0..25 {
            t += rng.below(2000);
            match rng.below(12) {
                0..=3 => {
                    let n = 1 + rng.below(30) as usize;
                    let text: String = (0..n)
                        .map(|_| CHARS[rng.below(CHARS.len() as u64) as usize] as char)
                        .collect();
                    let wpm = 3 + rng.below(50);
                    let cmd = if rng.below(4) == 0 { "CW" } else { "MCW" };
                    b.send(t, &format!("{cmd} {wpm} {text}"));
                }
                4 => {
                    b.send(t, "STOP");
                }
                5 => {
                    if rng.below(2) == 0 {
                        b.send(t, "TEST ARM");
                    }
                    b.send(t, ["TEST STUCK", "TEST HOLD"][rng.below(2) as usize]);
                }
                6 => b.lost(t),
                7 => {
                    t += rng.below(4000);
                    b.to(t);
                }
                8 => b.radio.held = rng.below(3) == 0,
                9 => b.radio.open = rng.below(3) == 0,
                _ => b.keep_alive(t, t + rng.below(3000)),
            }
            t = t.max(b.t);
        }
        b.radio = Radio::default();
        b.to(t + 61_000);
        let end = b.t;
        let ptts = b.ons(Pin::Ptt, end);
        for &(down, up) in &ptts {
            assert!(
                up - down <= 60_000,
                "round {round}: PTT down {} ms",
                up - down
            );
        }
        // The duty budget never overspent: the transmitter keyed (the PTT or the
        // key) only as long as it was not, beyond the 60 s it starts with. Only a
        // test may overspend it, held to a limit that trips the box for good.
        let mut keyed: Vec<(u64, u64)> = ptts.clone();
        keyed.extend(b.ons(Pin::Key, end));
        keyed.sort();
        let mut budget = 60_000i64;
        let mut last_up = 0;
        for (i, &(down, up)) in keyed.iter().enumerate() {
            budget = (budget + (down - last_up) as i64).min(60_000);
            budget -= (up - down) as i64;
            last_up = up;
            let tripped = i + 1 == keyed.len() && b.k.trip() != Trip::None;
            assert!(budget >= 0 || tripped, "round {round}: {keyed:?}");
        }
        assert!(b.longest(Pin::Tone, end) <= 1000, "round {round}");
        // The tone only under the PTT, and never with the key.
        for (on, off) in b.ons(Pin::Tone, end) {
            assert!(
                ptts.iter().any(|&(d, u)| d <= on && off <= u),
                "round {round}: tone {on}-{off} outside the PTT"
            );
        }
        for (on, off) in b.ons(Pin::Key, end) {
            assert!(
                ptts.iter().all(|&(d, u)| off <= d || on >= u),
                "round {round}: key {on}-{off} under the PTT"
            );
        }
        assert!(
            !b.k.ptt() && !b.k.tone() && !b.k.key_down(),
            "round {round}"
        );
    }
}

#[test]
fn hello_and_status_report_the_ptt() {
    let mut b = Bench::ptt();
    let h = b.send(10, "HELLO").unwrap();
    let f: Vec<&str> = h.split(' ').collect();
    // version, run s, link ms, key-down ms, rest ms, budget s, PTT s.
    assert_eq!(&f[2..9], ["4", "60", "2000", "1000", "1000", "60", "60"]);
    let mut c = Bench::ptt_with(Limits {
        ptt_ms: 30_000,
        ..Limits::BOX
    });
    let h = c.send(10, "HELLO").unwrap();
    assert_eq!(h.split(' ').nth(8), Some("30"));
    // The line is reported as read.
    b.radio.held = true;
    b.to(20);
    assert!(b.send(20, "STATUS").unwrap().ends_with(" 0 0"));
}
