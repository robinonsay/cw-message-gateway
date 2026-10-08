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
    led: bool,
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
            led: false,
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
        let read = calls.iter().position(|&c| c == Call::Read).unwrap();
        assert!(read < key && key < led, "{calls:?}");
        assert_eq!(
            calls.iter().filter(|c| matches!(c, Call::Key(_))).count(),
            1
        );
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
    assert!(r[0].starts_with("OK HELLO 2 "), "{r:?}");
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
