use super::*;
use crate::frame::{decode, encode};
use crate::keyer::{Boot, Ended, Limits};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Poll,
    Now,
    Epoch,
    Read,
    Write,
    Key(bool),
    Tone(bool),
    Ptt(bool),
    Line,
    Led(bool),
    Feed,
}

/// A board in memory: a clock the test moves on, a USB port, the pins, and a
/// record of every call the loop makes.
struct Board {
    t: u64,
    epoch: u32,
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    /// How many bytes `usb_write` takes per call; `usize::MAX` for all.
    room: usize,
    key: bool,
    tone: bool,
    ptt: bool,
    led: bool,
    /// The handheld on the PTT: the line reads low while the box's PTT is down,
    /// or while `held` (something else holds it); high otherwise.
    held: bool,
    /// The box's PTT does not reach the radio: the line stays high.
    open: bool,
    calls: std::cell::RefCell<Vec<Call>>,
}

impl Board {
    fn new() -> Self {
        Self {
            t: 0,
            epoch: 1,
            rx: VecDeque::new(),
            tx: Vec::new(),
            room: usize::MAX,
            key: false,
            tone: false,
            ptt: false,
            led: false,
            held: false,
            open: false,
            calls: Default::default(),
        }
    }

    fn call(&self, c: Call) {
        self.calls.borrow_mut().push(c);
    }

    fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.borrow_mut())
    }

    /// The host sends `body` under `id`.
    fn send(&mut self, id: u8, body: &str) {
        let line = encode(id, format_args!("{body}")).unwrap();
        self.rx.extend(line.as_bytes());
        self.rx.push_back(b'\n');
    }

    /// The reply bodies the box has sent, taken.
    fn replies(&mut self) -> Vec<String> {
        let tx = std::mem::take(&mut self.tx);
        tx.split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| decode(l).expect("reply decodes").1.to_string())
            .collect()
    }
}

impl Hardware for Board {
    fn now_ms(&self) -> u64 {
        self.call(Call::Now);
        self.t
    }
    fn usb_poll(&mut self) {
        self.call(Call::Poll);
    }
    fn link_epoch(&self) -> u32 {
        self.call(Call::Epoch);
        self.epoch
    }
    fn usb_read(&mut self, buf: &mut [u8]) -> usize {
        self.call(Call::Read);
        let n = buf.len().min(self.rx.len());
        for b in buf.iter_mut().take(n) {
            *b = self.rx.pop_front().unwrap();
        }
        n
    }
    fn usb_write(&mut self, bytes: &[u8]) -> usize {
        self.call(Call::Write);
        let n = bytes.len().min(self.room);
        self.tx.extend_from_slice(&bytes[..n]);
        n
    }
    fn set_key(&mut self, down: bool) {
        self.call(Call::Key(down));
        self.key = down;
    }
    fn set_tone(&mut self, on: bool) {
        self.call(Call::Tone(on));
        self.tone = on;
    }
    fn set_ptt(&mut self, down: bool) {
        self.call(Call::Ptt(down));
        self.ptt = down;
    }
    fn ptt_line(&mut self) -> bool {
        self.call(Call::Line);
        !(self.held || (self.ptt && !self.open))
    }
    fn set_led(&mut self, on: bool) {
        self.call(Call::Led(on));
        self.led = on;
    }
    fn feed_watchdog(&mut self) {
        self.call(Call::Feed);
    }
}

/// A box on a board, powered up at 0.
fn rig() -> (Control, Board) {
    let b = Board::new();
    let c = Control::new(Keyer::new(Limits::BOX, Boot::Power, 0), &b);
    (c, b)
}

/// The pin, pass by pass: its longest high stretch, and how long it was high in
/// all, in passes.
#[derive(Default)]
struct PinRecord {
    run: u64,
    longest: u64,
    total: u64,
}

impl PinRecord {
    fn pass(&mut self, high: bool) {
        if high {
            self.run += 1;
            self.total += 1;
            self.longest = self.longest.max(self.run);
        } else {
            self.run = 0;
        }
    }
}

/// The audit's chained-run case (KB-1): a host sends `CW 5 T` every millisecond
/// for two minutes, one pass of the loop each millisecond. The pin is never high
/// for more than the key-down limit, and its duty stays inside the budget's bound.
#[test]
fn runs_sent_back_to_back_do_not_hold_the_key_down() {
    let (mut c, mut b) = rig();
    let mut pin = PinRecord::default();
    let mut accepted = 0;
    for t in 0..120_000u64 {
        b.t = t;
        b.send((t % 200 + 1) as u8, "CW 5 T");
        c.pass(&mut b);
        pin.pass(b.key);
        accepted += b.replies().iter().filter(|r| *r == "OK CW").count();
    }
    // Each pass is 1 ms: passes are milliseconds.
    assert!(pin.longest <= 1000, "pin high {} ms at once", pin.longest);
    // A dash at 5 wpm (720 ms) then the 1 s rest: 42%. The budget alone would
    // allow at most (60 s + 120 s) / 2 over these 2 minutes.
    assert!(pin.total <= 90_000, "pin high {} ms of 120 s", pin.total);
    assert!(pin.total * 100 / 120_000 <= 45, "duty {} ms", pin.total);
    assert_eq!(pin.longest, 720);
    assert_eq!(c.keyer().trip(), Trip::None);
    assert!((60..=80).contains(&accepted), "{accepted} runs");
}

/// The same without the box's rest, to show the pin guard and the short-gap rule
/// catch what the rest prevents: with no rest, `CW 5 T` sent the moment each run
/// ends would hold the key down; the key-down limit, timing across the 0 ms
/// key-ups, trips the box.
#[test]
fn without_the_rest_chained_runs_trip_the_box() {
    let limits = Limits {
        rest_ms: 0,
        ..Limits::BOX
    };
    let mut b = Board::new();
    let mut c = Control::new(Keyer::new(limits, Boot::Power, 0), &b);
    let mut pin = PinRecord::default();
    for t in 0..10_000u64 {
        b.t = t;
        b.send((t % 200 + 1) as u8, "CW 5 T");
        c.pass(&mut b);
        pin.pass(b.key);
    }
    assert!(pin.longest <= 1000, "pin high {} ms at once", pin.longest);
    assert_eq!(c.keyer().trip(), Trip::Down);
}

/// Drive `g` every millisecond over `from..to`, `down`; no fault on the way.
fn drive(g: &mut PinGuard, from: u64, to: u64, down: bool) {
    for t in from..to {
        assert_eq!(g.fault(t, down), None, "at {t}");
        g.driven(t, down);
    }
}

#[test]
fn the_pin_guard_bridges_short_lows_and_trips_at_the_limit() {
    let mut g = PinGuard::new(0, 1000);
    drive(&mut g, 0, 600, true);
    // Low for 5 ms, under one dot at 50 wpm: not a key-up.
    drive(&mut g, 600, 605, false);
    drive(&mut g, 605, 1000, true);
    assert_eq!(g.fault(1000, true), Some(Trip::Pin));
    assert_eq!(g.high_for(1000), 1000);
    // A real key-up (24 ms, one dot at 50 wpm) starts it again.
    let mut g = PinGuard::new(0, 1000);
    drive(&mut g, 0, 600, true);
    drive(&mut g, 600, 624, false);
    drive(&mut g, 624, 1624, true);
    assert_eq!(g.fault(1624, true), Some(Trip::Pin));
    // Opening the key is never a fault, nor is a long wait with it open.
    let mut g = PinGuard::new(0, 1000);
    drive(&mut g, 0, 999, true);
    assert_eq!(g.fault(999, false), None);
    g.driven(999, false);
    assert_eq!(g.fault(5000, true), None);
}

#[test]
fn a_slow_pass_with_the_key_down_trips_the_box() {
    let (mut c, mut b) = rig();
    b.send(1, "CW 5 TTTT");
    c.pass(&mut b);
    assert!(b.key);
    // Passes 1 ms apart, then one 11 ms later: the pin was high all that time
    // with nothing watching it.
    for t in 1..=100 {
        b.t = t;
        c.pass(&mut b);
    }
    assert!(b.key);
    b.t = 111;
    c.pass(&mut b);
    assert!(!b.key);
    assert_eq!(c.keyer().trip(), Trip::Slow);
    assert_eq!(c.keyer().ended(), Ended::Down);
    // A slow pass with the key up is nothing.
    let (mut c, mut b) = rig();
    b.t = 500;
    c.pass(&mut b);
    assert_eq!(c.keyer().trip(), Trip::None);
}

/// Each pass: USB serviced, then the clock read; the pin driven after the keyer
/// is brought up to that time, then the LED; the watchdog fed exactly once, last.
#[test]
fn each_pass_feeds_the_watchdog_once_after_driving_the_pin() {
    let (mut c, mut b) = rig();
    b.take_calls();
    b.send(1, "CW 20 TEST");
    for t in 0..50 {
        b.t = t;
        c.pass(&mut b);
        let calls = b.take_calls();
        assert_eq!(calls.first(), Some(&Call::Poll), "{calls:?}");
        assert_eq!(calls.get(1), Some(&Call::Now), "{calls:?}");
        assert_eq!(calls.last(), Some(&Call::Feed), "{calls:?}");
        assert_eq!(calls.iter().filter(|&&c| c == Call::Feed).count(), 1);
        let key = calls
            .iter()
            .position(|c| matches!(c, Call::Key(_)))
            .expect("pin driven");
        let led = calls
            .iter()
            .position(|c| matches!(c, Call::Led(_)))
            .expect("LED driven");
        let tone = calls
            .iter()
            .position(|c| matches!(c, Call::Tone(_)))
            .expect("tone driven");
        let ptt = calls
            .iter()
            .position(|c| matches!(c, Call::Ptt(_)))
            .expect("PTT driven");
        let line = calls.iter().position(|&c| c == Call::Line).unwrap();
        let read = calls.iter().position(|&c| c == Call::Read).unwrap();
        assert!(line < read, "the line is read before the lines: {calls:?}");
        assert!(
            read < key && key < tone && tone < ptt && ptt < led,
            "{calls:?}"
        );
        for pin in [Call::Key(true), Call::Tone(true), Call::Ptt(true)] {
            let n = calls
                .iter()
                .filter(|&&c| core::mem::discriminant(&c) == core::mem::discriminant(&pin))
                .count();
            assert_eq!(n, 1, "{pin:?} once: {calls:?}");
        }
    }
    assert_eq!(b.replies(), vec!["OK CW".to_string()]);
}

/// The USB link going away ends the run, and the key opens on the same pass.
#[test]
fn a_lost_link_opens_the_key_on_the_same_pass() {
    let (mut c, mut b) = rig();
    b.send(1, "CW 5 TTTT");
    for t in 0..100 {
        b.t = t;
        c.pass(&mut b);
    }
    assert!(b.key);
    // Half a line in, then the link drops: the half line is dropped too.
    b.rx.extend(b"02 STA");
    b.t = 100;
    b.epoch += 1;
    c.pass(&mut b);
    assert!(!b.key);
    assert_eq!(c.keyer().ended(), Ended::Usb);
    b.replies();
    b.rx.extend(b"TUS*00\n");
    b.send(3, "STATUS");
    b.t = 101;
    c.pass(&mut b);
    let r = b.replies();
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].starts_with("OK STATUS 0 0 USB NONE"), "{r:?}");
}

/// `TEST HANG`: the loop stops at the next key-down, with the key down and the
/// watchdog unfed. If the watchdog does not reset the chip, the loop opens the key
/// itself `HANG_OPEN_MS` later.
#[test]
fn a_hang_stops_feeding_the_watchdog_and_opens_the_key_if_it_does_not_bite() {
    let (mut c, mut b) = rig();
    // Two dashes at 5 wpm: down 0-720, up to 1440, down from 1440.
    b.send(1, "TEST ARM");
    b.send(2, "CW 5 TT");
    c.pass(&mut b);
    let mut fed_after_hang = false;
    for t in 1..=4000u64 {
        b.t = t;
        if t == 800 {
            b.send(3, "TEST HANG");
        }
        if t == 1500 {
            b.send(4, "STOP");
        }
        c.pass(&mut b);
        let calls = b.take_calls();
        if t > 1440 {
            assert!(c.hung());
            fed_after_hang |= calls.contains(&Call::Feed);
            assert!(!calls.contains(&Call::Poll) && !calls.contains(&Call::Read));
        }
        let open_by = 1440 + u64::from(limits::HANG_OPEN_MS);
        assert_eq!(b.key, t < 720 || (1440..open_by).contains(&t), "at {t}");
        assert_eq!(b.led, b.key, "at {t}");
    }
    assert!(!fed_after_hang, "the watchdog was fed while hung");
    assert_eq!(b.replies(), vec!["OK TEST ARM", "OK CW", "OK TEST HANG"]);
}

/// A hang taken with the key already down (as `hfnode keyer hangtest` does) is at
/// once; its reply still goes out. The key opens once the pin has been high for
/// the key-down limit, before `HANG_OPEN_MS` is up.
#[test]
fn a_hang_mid_element_answers_and_opens_within_the_key_down_limit() {
    let (mut c, mut b) = rig();
    b.send(1, "CW 5 T");
    c.pass(&mut b);
    for t in 1..=700 {
        b.t = t;
        c.pass(&mut b);
    }
    b.replies();
    b.send(2, "TEST ARM");
    b.send(3, "TEST HANG");
    b.t = 701;
    c.pass(&mut b);
    assert!(c.hung() && b.key);
    assert_eq!(
        b.replies(),
        vec!["OK TEST ARM", "OK TEST HANG"],
        "the reply went out before the loop stopped"
    );
    for t in 702..=2000u64 {
        b.t = t;
        c.pass(&mut b);
        assert_eq!(b.key, t < 1000, "at {t}");
    }
}

#[test]
fn replies_wait_for_room_in_the_usb_buffer() {
    let (mut c, mut b) = rig();
    b.room = 5;
    b.send(1, "HELLO");
    c.pass(&mut b);
    for t in 1..40 {
        b.t = t;
        c.pass(&mut b);
    }
    let r = b.replies();
    assert_eq!(r.len(), 1);
    assert!(r[0].starts_with("OK HELLO 3 "), "{r:?}");
}

/// A tripped box flashes its LED; one keying shows the key.
#[test]
fn the_led_shows_the_key_and_flashes_when_tripped() {
    let (mut c, mut b) = rig();
    b.send(1, "CW 20 E");
    c.pass(&mut b);
    assert!(b.led);
    for t in 1..=70 {
        b.t = t;
        c.pass(&mut b);
    }
    assert!(!b.led);
    b.t = 2000;
    b.send(3, "CW 5 TTTT");
    c.pass(&mut b);
    assert!(b.led);
    b.send(4, "TEST ARM");
    b.send(5, "TEST STUCK");
    let mut flips = 0;
    let mut last = b.led;
    for t in 2001..6000 {
        b.t = t;
        if t % 250 == 0 {
            b.send(6, "STATUS");
        }
        c.pass(&mut b);
        if b.led != last {
            flips += 1;
            last = b.led;
        }
    }
    assert_eq!(c.keyer().trip(), Trip::Down);
    assert!(flips > 10, "{flips}");
}

// The PTT, the tone and the PTT line: an FM handheld through its headset jack.

/// A box on a board with the PTT line high (a handheld on, its PTT open).
fn ptt_rig() -> (Control, Board) {
    let (mut c, mut b) = rig();
    c.pass(&mut b);
    (c, b)
}

/// Run passes 1 ms apart over `from..=to`, keeping the link alive with `STATUS`
/// every 250 ms.
fn run_to(c: &mut Control, b: &mut Board, from: u64, to: u64) {
    for t in from..=to {
        b.t = t;
        if t % 250 == 0 {
            b.send(9, "STATUS");
        }
        c.pass(b);
    }
}

#[test]
fn mcw_holds_the_ptt_pin_and_keys_the_tone_pin() {
    let (mut c, mut b) = ptt_rig();
    b.send(1, "MCW 20 E");
    let mut ptt = PinRecord::default();
    let mut tone = PinRecord::default();
    let mut led_on = 0;
    for t in 1..=2000u64 {
        b.t = t;
        c.pass(&mut b);
        ptt.pass(b.ptt);
        tone.pass(b.tone);
        assert!(!b.key, "the key line is not used");
        assert!(!b.tone || b.ptt, "the tone only under the PTT, at {t}");
        led_on += u64::from(b.led);
        assert_eq!(
            c.pins(),
            Outputs {
                key: b.key,
                tone: b.tone,
                ptt: b.ptt
            }
        );
    }
    // 500 ms lead, one dot of 60 ms, 200 ms tail.
    assert_eq!((ptt.longest, tone.longest), (760, 60));
    assert_eq!(led_on, ptt.total, "the LED shows the PTT");
    assert_eq!(c.keyer().ended(), Ended::Done);
    assert!(b.replies().contains(&"OK MCW".to_string()));
}

/// The pin guard on the PTT: the keyer's timeline is wrong (limits made for the
/// test let it hold the PTT for two minutes), but the loop's own watch on the PTT
/// pin trips the box at the box's PTT limit.
#[test]
fn the_ptt_pin_guard_trips_at_the_ptt_limit_whatever_the_keyer_says() {
    let loose = Limits {
        ptt_ms: 120_000,
        run_ms: 120_000,
        ..Limits::BOX
    };
    let mut b = Board::new();
    let k = Keyer::new(loose, Boot::Power, 0);
    let mut c = Control::new(k, &b);
    c.ptt_guard = PinGuard::ptt(0, limits::PTT_MS);
    c.pass(&mut b);
    b.send(1, "TEST ARM");
    b.send(2, "MCW 20 E");
    b.send(3, "TEST HOLD");
    let mut ptt = PinRecord::default();
    for t in 1..=70_000u64 {
        b.t = t;
        if t % 250 == 0 {
            b.send(9, "STATUS");
        }
        c.pass(&mut b);
        ptt.pass(b.ptt);
    }
    assert_eq!(ptt.longest, 60_000, "{} ms", ptt.longest);
    assert_eq!(c.keyer().trip(), Trip::Pin);
    assert!(!b.ptt && !b.tone);
}

/// A slow pass with the tone on trips the box, as with the key; with only the PTT
/// down (the lead) it does not.
#[test]
fn a_slow_pass_trips_with_the_tone_on_but_not_with_the_ptt_alone() {
    let (mut c, mut b) = ptt_rig();
    b.send(1, "MCW 5 TTTT");
    run_to(&mut c, &mut b, 1, 100);
    assert!(b.ptt && !b.tone);
    // 50 ms with no pass, in the lead: nothing.
    b.t = 150;
    c.pass(&mut b);
    assert_eq!(c.keyer().trip(), Trip::None);
    run_to(&mut c, &mut b, 151, 600);
    assert!(b.ptt && b.tone);
    b.t = 611;
    c.pass(&mut b);
    assert_eq!(c.keyer().trip(), Trip::Slow);
    assert!(!b.ptt && !b.tone);
}

/// The PTT line read on each pass reaches the keyer: a PTT that does not take ends
/// the run, and one still held after the box lets go trips it.
#[test]
fn the_line_check_runs_on_the_pins_the_board_reads() {
    let (mut c, mut b) = ptt_rig();
    b.open = true;
    b.send(1, "MCW 20 E");
    run_to(&mut c, &mut b, 1, 300);
    assert_eq!(c.keyer().ended(), Ended::Line);
    assert!(!b.ptt && !b.tone);
    assert_eq!(c.keyer().trip(), Trip::None);
    b.open = false;
    let (mut c, mut b) = ptt_rig();
    b.send(1, "MCW 20 E");
    run_to(&mut c, &mut b, 1, 300);
    b.held = true;
    run_to(&mut c, &mut b, 301, 1200);
    assert_eq!(c.keyer().ended(), Ended::Done);
    assert_eq!(c.keyer().trip(), Trip::Line);
    // The radio's line is held low before a run: `MCW` is refused.
    let (mut c, mut b) = ptt_rig();
    b.held = true;
    b.send(1, "MCW 20 E");
    run_to(&mut c, &mut b, 1, 10);
    assert!(b.replies().contains(&"ERR MCW LINE".to_string()));
    assert!(!b.ptt);
}

/// `TEST HANG` in an `MCW` run: the loop stops at the first tone element with
/// the PTT down and the tone on. If the watchdog does not bite, the loop opens
/// both `HANG_OPEN_MS` later, or sooner once the tone has been on for the
/// key-down limit.
#[test]
fn a_hang_in_mcw_opens_the_ptt_and_the_tone_if_the_watchdog_does_not_bite() {
    let (mut c, mut b) = ptt_rig();
    b.send(1, "MCW 20 T");
    b.send(2, "TEST ARM");
    b.send(3, "TEST HANG");
    let mut fed_after_hang = false;
    for t in 1..=3000u64 {
        b.t = t;
        c.pass(&mut b);
        let calls = b.take_calls();
        // `MCW` taken on the pass at 1: the lead ends at 501.
        if t >= 501 {
            assert!(c.hung(), "at {t}");
            fed_after_hang |= calls.contains(&Call::Feed);
        }
        let open_by = 501 + u64::from(limits::HANG_OPEN_MS);
        assert_eq!(b.ptt, t < open_by, "at {t}");
        // The dash (180 ms) would have ended at 681: hung, the tone stays on.
        assert_eq!(b.tone, (501..open_by).contains(&t), "at {t}");
    }
    assert!(!fed_after_hang, "the watchdog was fed while hung");
}

/// Whatever the host sends, a pass at a time: the PTT never down past its limit,
/// the tone never on past the key-down limit nor outside the PTT, and the key
/// never under the PTT.
#[test]
fn whatever_the_host_sends_the_pins_stay_within_their_limits() {
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    for round in 0..6 {
        let (mut c, mut b) = ptt_rig();
        let mut ptt = PinRecord::default();
        let mut elem = PinRecord::default();
        for t in 1..=150_000u64 {
            b.t = t;
            if t % 97 == 0 {
                let body = match next(9) {
                    0 => "MCW 5 TTTTTTTTTT",
                    1 => "MCW 20 PARIS PARIS",
                    2 => "CW 5 TT",
                    3 => "TEST ARM",
                    4 => "TEST HOLD",
                    5 => "TEST STUCK",
                    6 => "STOP",
                    _ => "STATUS",
                };
                b.send((t % 200 + 1) as u8, body);
            }
            if t % 5003 == 0 {
                b.held = next(4) == 0;
            }
            c.pass(&mut b);
            b.replies();
            ptt.pass(b.ptt);
            elem.pass(b.key || b.tone);
            assert!(
                !b.tone || b.ptt,
                "round {round}: tone without the PTT at {t}"
            );
            assert!(!(b.key && b.ptt), "round {round}: key under the PTT at {t}");
        }
        assert!(
            ptt.longest <= 60_000,
            "round {round}: PTT {} ms",
            ptt.longest
        );
        assert!(
            elem.longest <= 1000,
            "round {round}: element {} ms",
            elem.longest
        );
        // The duty budget bounds the carrier over the whole 150 s.
        assert!(
            ptt.total <= 150_000 / 2 + 60_000,
            "round {round}: {}",
            ptt.total
        );
    }
}
