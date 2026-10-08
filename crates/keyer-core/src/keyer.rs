//! The box itself: commands in, replies out, and its outputs' state at every moment,
//! with every limit enforced here so that the firmware only has to move pins.
//!
//! The box has three outputs ([`Pin`]): the key (an optocoupler across a radio's key
//! jack), the PTT (a second optocoupler across an FM handheld's PTT contact) and the
//! tone (a square wave, filtered into the handheld's microphone). `CW` keys the text
//! on the key; `MCW` holds the PTT for the whole run and keys the text on the tone.
//! One input, the PTT line, reads the handheld's PTT contact: high while it is
//! open, low while anything holds it.
//!
//! Time is milliseconds since the box started, from whoever drives it: the
//! firmware's clock, or a mock's. [`Keyer::poll_with`] applies everything that came
//! due up to `now` at the exact time it came due, in order, so a caller may poll
//! every millisecond (the firmware) or only now and then (hfnode's mock box), and
//! sees the same output changes at the same times either way. The PTT line is the
//! exception: it is read, so [`Keyer::set_line`] must be told its level before each
//! poll, as the firmware does on every pass of its loop, and a caller polling now
//! and then steps through the time between.
//!
//! Commands (body of a line, see [`crate::frame`]):
//!
//! | Command | Reply |
//! |---|---|
//! | `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <rest ms> <duty budget s> <ptt limit s> <uptime ms> <boot> <build> <name>` |
//! | `STATUS` | `OK STATUS <key> <run> <ended> <trip> <rest left ms> <budget ms> <ptt> <line>` |
//! | `CW <wpm> <text>` | `OK CW`, keying from that moment |
//! | `MCW <wpm> <text>` | `OK MCW`, the PTT closed from that moment |
//! | `STOP` | `OK STOP` |
//! | `TEST ARM` | `OK TEST ARM` (bring-up only): the next `TEST HANG`, `TEST STUCK` or `TEST HOLD` within 2 s is taken |
//! | `TEST HANG` | `OK TEST HANG` (bring-up only) |
//! | `TEST STUCK` | `OK TEST STUCK` (bring-up only) |
//! | `TEST HOLD` | `OK TEST HOLD` (host tests only; an `MCW` run) |
//!
//! Errors are `ERR <command> <code>`: `CW` and `MCW` with `TRIP`, `RUN`, `WPM`,
//! `LEN`, `CHAR`, `LIMIT`, `LINE`, `REST` or `DUTY`; `TEST` with `RUN`, `ARM` or
//! `UNKNOWN`; anything else `ERR <word> UNKNOWN`.

use crate::frame::{self, Line};
use crate::morse::{self, Segments, TextError};
use crate::{limits, mcw, MAX_WPM, MIN_WPM, NAME, VERSION};
use core::fmt;

/// The limits a box keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub key_down_ms: u32,
    pub run_ms: u32,
    pub link_timeout_ms: u32,
    pub rest_ms: u32,
    pub duty_budget_ms: u32,
    pub ptt_ms: u32,
}

impl Limits {
    /// The real box's ([`crate::limits`]).
    pub const BOX: Self = Self {
        key_down_ms: limits::KEY_DOWN_MS,
        run_ms: limits::RUN_MS,
        link_timeout_ms: limits::LINK_TIMEOUT_MS,
        rest_ms: limits::REST_MS,
        duty_budget_ms: limits::DUTY_BUDGET_MS,
        ptt_ms: limits::PTT_MS,
    };
}

/// The box's outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// The key line (`CW`).
    Key,
    /// The PTT line (`MCW`).
    Ptt,
    /// The tone into the microphone (`MCW`): on for each element.
    Tone,
}

/// Why the box last started, reported by `HELLO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// Powered up (plugged in), or reset by its button or the debugger.
    Power,
    /// Its hardware watchdog reset it: the control loop stalled.
    Watchdog,
    Other,
}

impl Boot {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Power => "POWER",
            Self::Watchdog => "WATCHDOG",
            Self::Other => "OTHER",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Power, Self::Watchdog, Self::Other]
            .into_iter()
            .find(|b| b.as_str() == s)
    }
}

/// How the last `CW` or `MCW` run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// No run since the box started.
    None,
    /// Its text went out.
    Done,
    /// `STOP`.
    Stop,
    /// No valid line for the link timeout.
    Link,
    /// The run limit.
    Limit,
    /// The box tripped: its key-down limit, or the firmware's own watch on its
    /// pins ([`Trip`] says which).
    Down,
    /// The USB link went away: a bus reset, suspend (which is also how a pulled
    /// cable looks to the box), or the host deconfiguring it. Closing the port on
    /// the computer is none of these: the link timeout ends a run then.
    Usb,
    /// The PTT limit (and the box tripped).
    Ptt,
    /// `MCW`: the PTT line did not read low after the box closed the PTT (the
    /// cable is out, the radio is off, or the line is not wired).
    Line,
}

impl Ended {
    const ALL: [Self; 9] = [
        Self::None,
        Self::Done,
        Self::Stop,
        Self::Link,
        Self::Limit,
        Self::Down,
        Self::Usb,
        Self::Ptt,
        Self::Line,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Done => "DONE",
            Self::Stop => "STOP",
            Self::Link => "LINK",
            Self::Limit => "LIMIT",
            Self::Down => "DOWN",
            Self::Usb => "USB",
            Self::Ptt => "PTT",
            Self::Line => "LINE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// A fault that stops the box keying until it is power-cycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    None,
    /// The key (or the tone) stayed on past the key-down limit.
    Down,
    /// The firmware saw its key or tone pin on for the key-down limit, or its PTT
    /// pin down for the PTT limit ([`crate::control`]), whatever the box's
    /// timeline said.
    Pin,
    /// A pass of the firmware's control loop took longer than
    /// [`limits::SLOW_PASS_MS`] with the key or the tone on.
    Slow,
    /// The PTT stayed down past the PTT limit.
    Ptt,
    /// The PTT line still read low [`mcw::LINE_MS`] after the box opened the PTT:
    /// something else is holding it, and the radio may still be transmitting.
    Line,
}

impl Trip {
    const ALL: [Self; 6] = [
        Self::None,
        Self::Down,
        Self::Pin,
        Self::Slow,
        Self::Ptt,
        Self::Line,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Down => "DOWN",
            Self::Pin => "PIN",
            Self::Slow => "SLOW",
            Self::Ptt => "PTT",
            Self::Line => "LINE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// Where an `MCW` run is; a `CW` run is all [`Phase::Morse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// PTT down, no tone yet.
    Lead,
    Morse,
    /// PTT still down after the last element.
    Tail,
}

/// A `CW` or `MCW` run under way.
#[derive(Clone)]
struct Run {
    segs: Segments,
    /// The segment being keyed.
    idx: usize,
    dot_ms: u32,
    start: u64,
    /// When the current segment (or the lead or tail) ends.
    seg_end: u64,
    /// `MCW`: the elements are on the tone, under the PTT.
    mcw: bool,
    phase: Phase,
    /// `TEST STUCK`: the next element (or this one) is held.
    stuck: bool,
    /// `TEST HOLD`: the PTT stays down after the text.
    hold: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hang {
    No,
    /// `TEST HANG` taken: hang at the next element.
    Pending,
    /// Hung: the firmware stops its control loop, and nothing changes here again.
    Now,
}

/// What comes due next, in the order it is applied when two fall at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    KeyDownLimit,
    PttLimit,
    Link,
    RunLimit,
    /// The PTT line's check after the PTT last moved.
    LineCheck,
    Segment,
}

/// The box.
#[derive(Clone)]
pub struct Keyer {
    limits: Limits,
    boot: Boot,
    /// The firmware build, reported by `HELLO`.
    build: &'static str,
    run: Option<Run>,
    key: bool,
    tone: bool,
    /// When the key or the tone came on, not counting offs shorter than
    /// [`limits::MIN_GAP_MS`]: what the key-down limit times.
    elem_since: u64,
    /// When the key or the tone last went off; `None` if neither has been on.
    elem_off_at: Option<u64>,
    ptt: bool,
    /// When the PTT last went down.
    ptt_since: u64,
    /// The PTT line as last read: high (true) while the PTT contact is open.
    line: bool,
    /// When the PTT line must show the PTT's last move.
    line_due: Option<u64>,
    /// When the last run ended.
    run_ended_at: Option<u64>,
    /// The duty budget, ms, as of `budget_at`; below zero after a run that held
    /// the transmitter longer than it planned (`TEST STUCK`, `TEST HOLD`).
    budget: i64,
    budget_at: u64,
    /// `TEST ARM` taken: a test is accepted until then.
    armed_until: Option<u64>,
    last_line: u64,
    ended: Ended,
    trip: Trip,
    hang: Hang,
    now: u64,
}

impl Keyer {
    /// A box that started at `now` for `boot`, every output off, the PTT line not
    /// read yet (so low). Its duty budget starts full only after a power-up: a box
    /// that keeps restarting earns its budget again before it keys.
    pub fn new(limits: Limits, boot: Boot, now: u64) -> Self {
        Self {
            limits,
            boot,
            build: "-",
            run: None,
            key: false,
            tone: false,
            elem_since: now,
            elem_off_at: None,
            ptt: false,
            ptt_since: now,
            line: false,
            line_due: None,
            run_ended_at: None,
            budget: match boot {
                Boot::Power => i64::from(limits.duty_budget_ms),
                _ => 0,
            },
            budget_at: now,
            armed_until: None,
            last_line: now,
            ended: Ended::None,
            trip: Trip::None,
            hang: Hang::No,
            now,
        }
    }

    /// The firmware build `HELLO` reports: one word of up to 12 letters, digits,
    /// `.`, `-` or `_` (a git commit), or `?` if it is not.
    pub fn with_build(mut self, build: &'static str) -> Self {
        let ok = !build.is_empty()
            && build.len() <= 12
            && build
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        self.build = if ok { build } else { "?" };
        self
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether the key should be down, as of the last poll.
    pub fn key_down(&self) -> bool {
        self.key
    }

    /// Whether the PTT should be down, as of the last poll.
    pub fn ptt(&self) -> bool {
        self.ptt
    }

    /// Whether the tone should be on, as of the last poll.
    pub fn tone(&self) -> bool {
        self.tone
    }

    /// The PTT line as last read.
    pub fn line(&self) -> bool {
        self.line
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    pub fn ended(&self) -> Ended {
        self.ended
    }

    pub fn trip(&self) -> Trip {
        self.trip
    }

    /// `TEST HANG` has taken effect: the firmware must stop its control loop (and
    /// so stop feeding the watchdog) with its outputs as they are.
    pub fn hung(&self) -> bool {
        self.hang == Hang::Now
    }

    /// The PTT line reads `high` now. Call it before each poll with the level read
    /// just then (the firmware reads it on every pass of its loop): the check
    /// [`mcw::LINE_MS`] after the PTT moves judges the level last set.
    pub fn set_line(&mut self, high: bool) {
        if self.hang != Hang::Now {
            self.line = high;
        }
    }

    /// Bring the box up to `now`; whether the key is down.
    pub fn poll(&mut self, now: u64) -> bool {
        self.poll_with(now, |_, _, _| {})
    }

    /// [`Keyer::poll`], calling `on_change(at, pin, on)` for every change of an
    /// output, with the time it changed.
    pub fn poll_with(&mut self, now: u64, mut on_change: impl FnMut(u64, Pin, bool)) -> bool {
        if self.hang == Hang::Now {
            return self.key;
        }
        let now = now.max(self.now);
        while let Some((at, ev)) = self.next_event() {
            if at > now {
                break;
            }
            self.apply(at, ev, &mut on_change);
            if self.hang == Hang::Now {
                break;
            }
        }
        self.now = now;
        self.key
    }

    fn next_event(&self) -> Option<(u64, Event)> {
        let mut next: Option<(u64, Event)> = None;
        let mut consider = |at: u64, ev: Event| {
            // Earlier first; at the same time, in the order of `Event`.
            if next.is_none_or(|(t, _)| at < t) {
                next = Some((at, ev));
            }
        };
        if self.key || self.tone {
            consider(
                self.elem_since + u64::from(self.limits.key_down_ms),
                Event::KeyDownLimit,
            );
        }
        if self.ptt {
            consider(
                self.ptt_since + u64::from(self.limits.ptt_ms),
                Event::PttLimit,
            );
        }
        if let Some(r) = &self.run {
            consider(
                self.last_line + u64::from(self.limits.link_timeout_ms),
                Event::Link,
            );
            consider(r.start + u64::from(self.limits.run_ms), Event::RunLimit);
            let held = (r.stuck && (self.key || self.tone)) || (r.hold && r.phase == Phase::Tail);
            if !held {
                consider(r.seg_end, Event::Segment);
            }
        }
        if let Some(at) = self.line_due {
            consider(at, Event::LineCheck);
        }
        next
    }

    fn apply(&mut self, at: u64, ev: Event, on: &mut impl FnMut(u64, Pin, bool)) {
        match ev {
            Event::KeyDownLimit => self.trip_at(at, Trip::Down, Ended::Down, on),
            Event::PttLimit => self.trip_at(at, Trip::Ptt, Ended::Ptt, on),
            Event::Link => self.stop_run(at, Ended::Link, on),
            Event::RunLimit => {
                // A run exactly as long as the limit ends with its last element (an
                // `MCW` run, with its tail); one held by a test does not.
                let done = self.run.as_ref().is_some_and(|r| {
                    let last = if r.mcw {
                        r.phase == Phase::Tail
                    } else {
                        r.idx + 1 == r.segs.as_slice().len()
                    };
                    last && r.seg_end <= at && !r.hold && !r.stuck
                });
                let why = if done { Ended::Done } else { Ended::Limit };
                self.stop_run(at, why, on)
            }
            Event::LineCheck => {
                self.line_due = None;
                // Low while the PTT is down, high while it is up.
                if self.line == self.ptt {
                    if self.ptt {
                        // The PTT did not take: nothing is transmitting.
                        self.stop_run(at, Ended::Line, on);
                    } else {
                        // Something else holds it: nothing more is keyed.
                        self.trip_at(at, Trip::Line, self.ended, on);
                    }
                }
            }
            Event::Segment => self.next_segment(at, on),
        }
    }

    fn next_segment(&mut self, at: u64, on: &mut impl FnMut(u64, Pin, bool)) {
        let Some(r) = &mut self.run else { return };
        let next = match r.phase {
            Phase::Lead => Some(0),
            Phase::Morse => Some(r.idx + 1),
            Phase::Tail => None,
        };
        let seg = next.and_then(|i| r.segs.as_slice().get(i).copied().map(|s| (i, s)));
        match seg {
            Some((i, seg)) => {
                r.phase = Phase::Morse;
                r.idx = i;
                r.seg_end = at + u64::from(seg.units) * u64::from(r.dot_ms);
                self.set_elem(at, seg.down, on);
            }
            None if r.mcw && r.phase == Phase::Morse => {
                r.phase = Phase::Tail;
                r.seg_end = at + u64::from(mcw::TAIL_MS);
                self.set_elem(at, false, on);
            }
            None => self.stop_run(at, Ended::Done, on),
        }
    }

    /// The element output of the run under way (the key for `CW`, the tone for
    /// `MCW`) on or off; with no run, both off.
    fn set_elem(&mut self, at: u64, active: bool, on: &mut impl FnMut(u64, Pin, bool)) {
        let mcw = self.run.as_ref().is_some_and(|r| r.mcw);
        let (key, tone) = match (active, mcw) {
            (false, _) => (false, false),
            (true, false) => (true, false),
            (true, true) => (false, true),
        };
        let was = self.key || self.tone;
        if key || tone {
            if !was {
                // An off too short to be one (at most a pass of the firmware's
                // loop between two runs) does not restart the key-down limit.
                let short = self
                    .elem_off_at
                    .is_some_and(|off| at.saturating_sub(off) < u64::from(limits::MIN_GAP_MS));
                if !short {
                    self.elem_since = at;
                }
            }
        } else if was {
            self.elem_off_at = Some(at);
        }
        if key != self.key {
            // The key keys the transmitter: the duty budget changes with it.
            self.settle_budget(at);
            self.key = key;
            on(at, Pin::Key, key);
        }
        if tone != self.tone {
            self.tone = tone;
            on(at, Pin::Tone, tone);
        }
        if active && self.hang == Hang::Pending {
            self.hang = Hang::Now;
        }
    }

    fn set_ptt(&mut self, at: u64, down: bool, on: &mut impl FnMut(u64, Pin, bool)) {
        if down == self.ptt {
            return;
        }
        self.settle_budget(at);
        self.ptt = down;
        if down {
            self.ptt_since = at;
        }
        self.line_due = Some(at + u64::from(mcw::LINE_MS));
        on(at, Pin::Ptt, down);
    }

    /// Whether the transmitter is keyed: the key down, or the PTT down.
    fn keyed(&self) -> bool {
        self.key || self.ptt
    }

    /// The duty budget at `at`, the transmitter as it has been since it was last
    /// settled: spent while it was keyed, earned back while it was not.
    fn budget_then(&self, at: u64) -> i64 {
        let dt = i64::try_from(at.saturating_sub(self.budget_at)).unwrap_or(i64::MAX);
        if self.keyed() {
            self.budget.saturating_sub(dt)
        } else {
            self.budget
                .saturating_add(dt)
                .min(i64::from(self.limits.duty_budget_ms))
        }
    }

    fn settle_budget(&mut self, at: u64) {
        self.budget = self.budget_then(at);
        self.budget_at = self.budget_at.max(at);
    }

    /// The duty budget at `at`, ms, not below zero.
    fn budget_at(&self, at: u64) -> u64 {
        u64::try_from(self.budget_then(at)).unwrap_or(0)
    }

    /// How long from `at` until the rest after the last run is over.
    fn rest_left(&self, at: u64) -> u64 {
        self.run_ended_at.map_or(0, |end| {
            (end + u64::from(self.limits.rest_ms)).saturating_sub(at)
        })
    }

    fn stop_run(&mut self, at: u64, why: Ended, on: &mut impl FnMut(u64, Pin, bool)) {
        if self.run.is_some() {
            self.ended = why;
            self.run_ended_at = Some(at);
        }
        if self.hang == Hang::Pending {
            self.hang = Hang::No;
        }
        // The element first, then the PTT, as a run ends.
        self.set_elem(at, false, on);
        self.run = None;
        self.set_ptt(at, false, on);
    }

    fn trip_at(&mut self, at: u64, why: Trip, ended: Ended, on: &mut impl FnMut(u64, Pin, bool)) {
        if self.trip == Trip::None {
            self.trip = why;
        }
        self.stop_run(at, ended, on);
    }

    /// The firmware saw a fault on one of its pins at `now` ([`crate::control`]):
    /// open everything and trip, as the key-down limit does. Nothing more is keyed
    /// until the box is power-cycled.
    pub fn trip_now(&mut self, now: u64, why: Trip, mut on_change: impl FnMut(u64, Pin, bool)) {
        self.poll_with(now, &mut on_change);
        if self.hang != Hang::Now {
            self.trip_at(now.max(self.now), why, Ended::Down, &mut on_change);
        }
    }

    /// The USB link went away at `now` (bus reset, suspend, deconfigured): stop any
    /// run.
    pub fn link_lost(&mut self, now: u64, mut on_change: impl FnMut(u64, Pin, bool)) {
        self.poll_with(now, &mut on_change);
        self.armed_until = None;
        if self.hang != Hang::Now && self.run.is_some() {
            self.stop_run(now.max(self.now), Ended::Usb, &mut on_change);
        }
    }

    /// A line received at `now` (without its newline): the reply to send, or `None`
    /// for a line that does not decode (ignored, and not counted for the link
    /// timeout) or a hung box.
    pub fn handle_line(&mut self, now: u64, line: &[u8]) -> Option<Line> {
        self.handle_line_with(now, line, |_, _, _| {})
    }

    /// [`Keyer::handle_line`], calling `on_change` for every output change as
    /// [`Keyer::poll_with`] does.
    pub fn handle_line_with(
        &mut self,
        now: u64,
        line: &[u8],
        mut on_change: impl FnMut(u64, Pin, bool),
    ) -> Option<Line> {
        self.poll_with(now, &mut on_change);
        if self.hang == Hang::Now {
            return None;
        }
        let now = self.now;
        let (id, body) = frame::decode(line).ok()?;
        self.last_line = now;
        let mut w = body.split(' ');
        let word = w.next().unwrap_or("");
        match (word, body) {
            (_, "HELLO") => frame::encode(
                id,
                format_args!(
                    "OK HELLO {VERSION} {} {} {} {} {} {} {now} {} {} {NAME}",
                    self.limits.run_ms / 1000,
                    self.limits.link_timeout_ms,
                    self.limits.key_down_ms,
                    self.limits.rest_ms,
                    self.limits.duty_budget_ms / 1000,
                    self.limits.ptt_ms / 1000,
                    self.boot.as_str(),
                    self.build
                ),
            ),
            (_, "STATUS") => frame::encode(
                id,
                format_args!(
                    "OK STATUS {} {} {} {} {} {} {} {}",
                    u8::from(self.key),
                    u8::from(self.run.is_some()),
                    self.ended.as_str(),
                    self.trip.as_str(),
                    if self.run.is_some() {
                        0
                    } else {
                        self.rest_left(now)
                    },
                    self.budget_at(now),
                    u8::from(self.ptt),
                    u8::from(self.line)
                ),
            ),
            (_, "STOP") => {
                self.stop_run(now, Ended::Stop, &mut on_change);
                frame::encode(id, format_args!("OK STOP"))
            }
            ("CW" | "MCW", _) => {
                let mcw = word == "MCW";
                match self.start(now, &body[word.len()..], mcw, &mut on_change) {
                    Ok(()) => frame::encode(id, format_args!("OK {word}")),
                    Err(code) => frame::encode(id, format_args!("ERR {word} {code}")),
                }
            }
            (_, "TEST ARM") => {
                self.armed_until = Some(now + u64::from(limits::ARM_MS));
                frame::encode(id, format_args!("OK TEST ARM"))
            }
            (_, "TEST HANG" | "TEST STUCK") if self.run.is_none() => {
                frame::encode(id, format_args!("ERR TEST RUN"))
            }
            (_, "TEST HOLD") if !self.run.as_ref().is_some_and(|r| r.mcw) => {
                frame::encode(id, format_args!("ERR TEST RUN"))
            }
            // A test is taken only just after `TEST ARM`, and once: a stray line
            // (a terminal on the port, a script) cannot start one.
            (_, "TEST HANG" | "TEST STUCK" | "TEST HOLD")
                if self.armed_until.take().is_none_or(|until| now > until) =>
            {
                frame::encode(id, format_args!("ERR TEST ARM"))
            }
            (_, "TEST HANG") => {
                self.hang = Hang::Pending;
                if self.key || self.tone {
                    self.hang = Hang::Now;
                }
                frame::encode(id, format_args!("OK TEST HANG"))
            }
            (_, "TEST STUCK") => {
                if let Some(r) = &mut self.run {
                    r.stuck = true;
                }
                frame::encode(id, format_args!("OK TEST STUCK"))
            }
            (_, "TEST HOLD") => {
                if let Some(r) = &mut self.run {
                    r.hold = true;
                }
                frame::encode(id, format_args!("OK TEST HOLD"))
            }
            ("TEST", _) => frame::encode(id, format_args!("ERR TEST UNKNOWN")),
            _ => {
                let word = match word {
                    "" => "?",
                    w => &w[..w.len().min(12)],
                };
                frame::encode(id, format_args!("ERR {word} UNKNOWN"))
            }
        }
    }

    /// `CW <wpm> <text>` or `MCW <wpm> <text>`; `args` is what follows the word.
    fn start(
        &mut self,
        now: u64,
        args: &str,
        mcw: bool,
        on: &mut impl FnMut(u64, Pin, bool),
    ) -> Result<(), StartError> {
        if self.trip != Trip::None {
            return Err(StartError::Trip);
        }
        if self.run.is_some() {
            return Err(StartError::Run);
        }
        let args = args.strip_prefix(' ').ok_or(StartError::Len)?;
        let (wpm, text) = args.split_once(' ').unwrap_or((args, ""));
        if wpm.is_empty() || wpm.len() > 3 || !wpm.bytes().all(|b| b.is_ascii_digit()) {
            return Err(StartError::Wpm);
        }
        let wpm: u32 = wpm.parse().map_err(|_| StartError::Wpm)?;
        if !(MIN_WPM..=MAX_WPM).contains(&wpm) {
            return Err(StartError::Wpm);
        }
        let dot_ms = morse::dot_ms(wpm).map_err(|_| StartError::Wpm)?;
        let segs = Segments::of(text.as_bytes()).map_err(|e| match e {
            TextError::Len => StartError::Len,
            TextError::Char => StartError::Char,
        })?;
        let morse_ms = segs.units() * dot_ms;
        // How long the run keys the transmitter: the key-down time of `CW`, the
        // whole PTT time of `MCW` (a steady carrier).
        let keyed_ms: u64 = if mcw {
            // The whole PTT time, under both limits, so that a run that goes as
            // planned never meets either.
            let ptt_ms = mcw::LEAD_MS + morse_ms + mcw::TAIL_MS;
            if ptt_ms >= self.limits.ptt_ms.min(self.limits.run_ms) {
                return Err(StartError::Limit);
            }
            u64::from(ptt_ms)
        } else {
            if morse_ms > self.limits.run_ms {
                return Err(StartError::Limit);
            }
            segs.as_slice()
                .iter()
                .filter(|s| s.down)
                .map(|s| u64::from(s.units) * u64::from(dot_ms))
                .sum()
        };
        // The PTT line: nothing new while the last move of the PTT is unchecked;
        // and for `MCW`, the line high (the radio on, the cable in, the PTT open).
        if self.line_due.is_some() || (mcw && !self.line) {
            return Err(StartError::Line);
        }
        if self.rest_left(now) > 0 {
            return Err(StartError::Rest);
        }
        if self.budget_at(now) < keyed_ms {
            return Err(StartError::Duty);
        }
        let first = segs.as_slice()[0];
        let (phase, seg_end) = if mcw {
            (Phase::Lead, now + u64::from(mcw::LEAD_MS))
        } else {
            (
                Phase::Morse,
                now + u64::from(first.units) * u64::from(dot_ms),
            )
        };
        self.run = Some(Run {
            segs,
            idx: 0,
            dot_ms,
            start: now,
            seg_end,
            mcw,
            phase,
            stuck: false,
            hold: false,
        });
        if mcw {
            self.set_ptt(now, true, on);
        } else {
            self.set_elem(now, true, on);
        }
        Ok(())
    }
}

/// Why `CW` or `MCW` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// Tripped: unplug the box and plug it in again.
    Trip,
    /// A run is already under way.
    Run,
    Wpm,
    Len,
    Char,
    /// Longer than the run limit at that speed (for `MCW`, with the lead and tail,
    /// not under the run and PTT limits).
    Limit,
    /// The PTT line is low (`MCW`), or the PTT's last move has not been checked
    /// yet.
    Line,
    /// The last run ended less than the rest ([`limits::REST_MS`]) ago.
    Rest,
    /// The duty budget holds less than the time the run keys the transmitter.
    Duty,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trip => "TRIP",
            Self::Run => "RUN",
            Self::Wpm => "WPM",
            Self::Len => "LEN",
            Self::Char => "CHAR",
            Self::Limit => "LIMIT",
            Self::Line => "LINE",
            Self::Rest => "REST",
            Self::Duty => "DUTY",
        })
    }
}

#[cfg(test)]
mod tests;
