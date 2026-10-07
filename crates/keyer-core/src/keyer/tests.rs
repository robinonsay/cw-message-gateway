use super::*;
use crate::frame::encode;

/// A box under test, with every key change recorded.
struct Bench {
    k: Keyer,
    changes: Vec<(u64, bool)>,
    next_id: u8,
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
        }
    }

    /// Send `body` at `t`; the reply's body (the id checked and stripped).
    fn send(&mut self, t: u64, body: &str) -> Option<String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let line = encode(id, format_args!("{body}")).unwrap();
        let changes = &mut self.changes;
        let reply = self
            .k
            .handle_line_with(t, line.as_bytes(), |at, d| changes.push((at, d)))?;
        let (rid, rbody) = frame::decode(reply.as_bytes()).expect("reply decodes");
        assert_eq!(rid, id, "reply id");
        assert!(reply.len() <= crate::MAX_LINE);
        Some(rbody.to_string())
    }

    fn poll(&mut self, t: u64) -> bool {
        let changes = &mut self.changes;
        self.k.poll_with(t, |at, d| changes.push((at, d)))
    }

    /// Longest key-down in the record, up to `end` for one still down.
    fn longest_down(&self, end: u64) -> u64 {
        let mut longest = 0;
        let mut since = None;
        for &(t, d) in &self.changes {
            match (d, since) {
                (true, None) => since = Some(t),
                (false, Some(s)) => {
                    longest = longest.max(t - s);
                    since = None;
                }
                _ => panic!("key changes do not alternate: {:?}", self.changes),
            }
        }
        if let Some(s) = since {
            longest = longest.max(end - s);
        }
        longest
    }
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
        "OK HELLO 1 60 2000 1000 5230 POWER PICO2-KEYER"
    );
    let mut w = Keyer::new(Limits::BOX, Boot::Watchdog, 0);
    let line = encode(9, format_args!("HELLO")).unwrap();
    let r = w.handle_line(40, line.as_bytes()).unwrap();
    assert!(r.as_str().contains(" 40 WATCHDOG PICO2-KEYER*"), "{r:?}");
}

#[test]
fn cw_keys_its_text_with_standard_timing_and_ends() {
    let mut b = Bench::new();
    assert_eq!(b.send(1000, "STATUS").unwrap(), "OK STATUS 0 0 NONE NONE");
    assert_eq!(b.send(1000, "CW 20 R 42 ? DE N0DE K").unwrap(), "OK CW");
    assert_eq!(b.send(1001, "STATUS").unwrap(), "OK STATUS 1 1 NONE NONE");
    // Keep the link alive through the run, as hfnode does.
    let end = 1000 + u64::from(morse::run_ms(b"R 42 ? DE N0DE K", 20).unwrap());
    let mut t = 1000;
    while t < end + 500 {
        t += 250;
        b.send(t, "STATUS");
    }
    assert_eq!(b.changes, expected("R 42 ? DE N0DE K", 20, 1000));
    assert_eq!(b.send(t, "STATUS").unwrap(), "OK STATUS 0 0 DONE NONE");
    // And another after it.
    assert_eq!(b.send(t, "CW 25 E").unwrap(), "OK CW");
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
    assert_eq!(every.changes, expected(text, 18, 0));
}

#[test]
fn stop_opens_the_key_at_once() {
    let mut b = Bench::new();
    b.send(0, "CW 10 TTTT");
    // Mid-dash.
    assert!(b.poll(150));
    assert_eq!(b.send(200, "STOP").unwrap(), "OK STOP");
    assert!(!b.k.key_down());
    assert_eq!(b.changes, vec![(0, true), (200, false)]);
    assert_eq!(b.send(201, "STATUS").unwrap(), "OK STATUS 0 0 STOP NONE");
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
    let last = *b.changes.last().unwrap();
    assert_eq!(last, (5000, false), "2 s after the last good line");
    assert_eq!(b.k.ended(), Ended::Link);
    assert!(!b.k.running());
}

#[test]
fn unplugging_usb_ends_a_run() {
    let mut b = Bench::new();
    b.send(0, "CW 10 TTTT");
    let changes = &mut b.changes;
    b.k.link_lost(50, |at, d| changes.push((at, d)));
    assert_eq!(b.changes, vec![(0, true), (50, false)]);
    assert!(!b.k.key_down());
    assert_eq!(b.k.ended(), Ended::Usb);
    assert_eq!(b.send(60, "STATUS").unwrap(), "OK STATUS 0 0 USB NONE");
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
    assert_eq!(b.send(70, "TEST STUCK").unwrap(), "OK TEST STUCK");
    let mut t = 70;
    while t < 3000 {
        t += 250;
        b.send(t, "STATUS");
    }
    // Down at 240 (the second dot, after a character gap) and held to the limit.
    assert_eq!(
        b.changes,
        vec![(0, true), (60, false), (240, true), (1240, false)]
    );
    assert_eq!(b.k.trip(), Trip::Down);
    assert_eq!(b.send(t, "STATUS").unwrap(), "OK STATUS 0 0 DOWN DOWN");
    assert_eq!(b.send(t, "CW 20 E").unwrap(), "ERR CW TRIP");
    assert!(b.longest_down(t) <= 1000);
}

#[test]
fn a_hang_freezes_the_box_at_the_next_key_down() {
    let mut b = Bench::new();
    b.send(0, "CW 20 EEEE");
    b.poll(61);
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
    assert_eq!(b.changes, vec![(0, true), (60, false), (240, true)]);
}

#[test]
fn a_hang_asked_for_while_the_key_is_down_is_at_once() {
    let mut b = Bench::new();
    b.send(0, "CW 20 T");
    assert_eq!(b.send(10, "TEST HANG").unwrap(), "OK TEST HANG");
    assert!(b.k.hung());
    // A hang still pending when the run ends is dropped.
    let mut c = Bench::new();
    c.send(0, "CW 20 EE");
    c.send(100, "TEST HANG");
    assert!(!c.k.hung());
    c.send(150, "STOP");
    c.poll(1000);
    assert!(!c.k.hung() && !c.k.key_down());
    c.send(1000, "CW 20 E");
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
            match rng.below(9) {
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
                    stuck |= b.send(t, "TEST STUCK").as_deref() == Some("OK TEST STUCK");
                }
                5 => {
                    let changes = &mut b.changes;
                    b.k.link_lost(t, |at, d| changes.push((at, d)));
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
