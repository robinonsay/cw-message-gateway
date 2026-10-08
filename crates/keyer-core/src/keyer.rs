//! The box itself: commands in, replies out, and the key's state at every moment,
//! with every limit enforced here so that the firmware only has to move a pin.
//!
//! Time is milliseconds since the box started, from whoever drives it: the
//! firmware's clock, or a mock's. [`Keyer::poll_with`] applies everything that came
//! due up to `now` at the exact time it came due, in order, so a caller may poll
//! every millisecond (the firmware) or only now and then (hfnode's mock box), and
//! sees the same key changes at the same times either way.
//!
//! Commands (body of a line, see [`crate::frame`]):
//!
//! | Command | Reply |
//! |---|---|
//! | `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <rest ms> <duty budget s> <uptime ms> <boot> <build> <name>` |
//! | `STATUS` | `OK STATUS <key> <run> <ended> <trip> <rest left ms> <budget ms>`; `<trip>` is `NONE`, `DOWN`, `PIN`, `SLOW`, `CLOCK` or `WATCHDOG` |
//! | `CW <wpm> <text>` | `OK CW`, keying from that moment |
//! | `STOP` | `OK STOP` |
//! | `TEST ARM` | `OK TEST ARM` (bring-up only): the next `TEST HANG` or `TEST STUCK` within 2 s is taken |
//! | `TEST HANG` | `OK TEST HANG` (bring-up only) |
//! | `TEST STUCK` | `OK TEST STUCK` (bring-up only) |
//!
//! Errors are `ERR <command> <code>`: `CW` with `TRIP`, `RUN`, `WPM`, `LEN`, `CHAR`,
//! `LIMIT`, `REST` or `DUTY`; `TEST` with `RUN`, `ARM` or `UNKNOWN`; anything else
//! `ERR <word> UNKNOWN`.

use crate::frame::{self, Line};
use crate::morse::{self, Segments, TextError};
use crate::{limits, MAX_WPM, MIN_WPM, NAME, VERSION};
use core::fmt;

/// The limits a box keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub key_down_ms: u32,
    pub run_ms: u32,
    pub link_timeout_ms: u32,
    pub rest_ms: u32,
    pub duty_budget_ms: u32,
}

impl Limits {
    /// The real box's ([`crate::limits`]).
    pub const BOX: Self = Self {
        key_down_ms: limits::KEY_DOWN_MS,
        run_ms: limits::RUN_MS,
        link_timeout_ms: limits::LINK_TIMEOUT_MS,
        rest_ms: limits::REST_MS,
        duty_budget_ms: limits::DUTY_BUDGET_MS,
    };
}

/// Why the box last started, reported by `HELLO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// Powered up (plugged in), or reset by its button or the debugger.
    Power,
    /// Its hardware watchdog reset it: the control loop stalled, `TEST HANG`
    /// stopped it, or it stopped feeding the watchdog on purpose after a `CLOCK`
    /// trip. The box comes up tripped (`WATCHDOG`, or the trip it saved).
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

/// How the last `CW` run ended.
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
    /// The box tripped: its key-down limit, or the firmware's own watch on the
    /// key pin ([`Trip`] says which).
    Down,
    /// The USB link went away: a bus reset, suspend (which is also how a pulled
    /// cable looks to the box), or the host deconfiguring it. Closing the port on
    /// the computer is none of these: the link timeout ends a run then.
    Usb,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Done => "DONE",
            Self::Stop => "STOP",
            Self::Link => "LINK",
            Self::Limit => "LIMIT",
            Self::Down => "DOWN",
            Self::Usb => "USB",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::None,
            Self::Done,
            Self::Stop,
            Self::Link,
            Self::Limit,
            Self::Down,
            Self::Usb,
        ]
        .into_iter()
        .find(|e| e.as_str() == s)
    }
}

/// A fault that stops the box keying until it is power-cycled. A trip survives
/// a restart that is not a power-up ([`Saved`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    None,
    /// The key stayed down past the key-down limit.
    Down,
    /// The firmware saw its key pin high for the key-down limit
    /// ([`crate::control`]), whatever the box's timeline said.
    Pin,
    /// A pass of the firmware's control loop took longer than
    /// [`limits::SLOW_PASS_MS`] with the key pin high.
    Slow,
    /// The control loop's clock stopped or slowed against the processor's own
    /// count, or that count stopped against the clock ([`crate::control`]):
    /// no time limit can be trusted.
    Clock,
    /// The box restarted because its watchdog fired: its control loop stopped,
    /// with the key in a state nothing recorded.
    Watchdog,
}

impl Trip {
    const ALL: [Self; 6] = [
        Self::None,
        Self::Down,
        Self::Pin,
        Self::Slow,
        Self::Clock,
        Self::Watchdog,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Down => "DOWN",
            Self::Pin => "PIN",
            Self::Slow => "SLOW",
            Self::Clock => "CLOCK",
            Self::Watchdog => "WATCHDOG",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// What the box leaves for its next boot, in two words that survive a watchdog
/// reset but not a power-up (the RP2350's watchdog scratch registers): its trip,
/// whether its key was down, and its duty budget, as of the last pass of its
/// control loop ([`Keyer::saved`], [`Keyer::restore`]).
///
/// So a box that tripped stays tripped until it is unplugged, and a host that
/// makes the box restart (`TEST HANG`, or a fault) cannot get a fresh duty
/// budget out of it: a restart carries the budget over, with the key-down that
/// nobody saw taken off it ([`limits::RESTART_CHARGE_MS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Saved {
    pub trip: Trip,
    /// The key was down.
    pub key: bool,
    /// The duty budget, ms; below zero after a run held longer than it planned.
    pub budget: i32,
}

impl Saved {
    /// The top half of the first word: what marks the words as the box's.
    const MAGIC: u32 = 0x4b59;

    /// The two words: `MAGIC`, the flags (the trip's index, the key in bit 3)
    /// and a check byte in the first; the budget in the second.
    pub fn encode(&self) -> [u32; 2] {
        let trip = Trip::ALL.iter().position(|&t| t == self.trip).unwrap_or(0) as u32;
        let flags = trip | (u32::from(self.key) << 3);
        let budget = self.budget as u32;
        let head = (Self::MAGIC << 16) | (flags << 8);
        [head | u32::from(Self::check(head, budget)), budget]
    }

    /// The state in `words`, if they hold one: not if they are zero (as after a
    /// power-up), left by other firmware, or damaged.
    pub fn decode(words: [u32; 2]) -> Option<Self> {
        let [head, budget] = words;
        if head >> 16 != Self::MAGIC || head & 0xff != u32::from(Self::check(head & !0xff, budget))
        {
            return None;
        }
        let flags = (head >> 8) & 0xff;
        if flags & !0xf != 0 {
            return None;
        }
        Some(Self {
            trip: *Trip::ALL.get((flags & 0x7) as usize)?,
            key: flags & 0x8 != 0,
            budget: budget as i32,
        })
    }

    /// A byte that changes with any one changed bit of `head`'s top three bytes
    /// or of `budget`.
    fn check(head: u32, budget: u32) -> u8 {
        let mut c: u8 = 0xa5;
        for b in (head >> 8)
            .to_be_bytes()
            .into_iter()
            .skip(1)
            .chain(budget.to_be_bytes())
        {
            c = c.rotate_left(1) ^ b;
        }
        c
    }
}

/// A `CW` run under way.
#[derive(Clone)]
struct Run {
    segs: Segments,
    /// The segment being keyed.
    idx: usize,
    dot_ms: u32,
    start: u64,
    /// When the current segment ends.
    seg_end: u64,
    /// `TEST STUCK`: the next key-down (or this one) is held.
    stuck: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hang {
    No,
    /// `TEST HANG` taken: hang at the next key-down.
    Pending,
    /// Hung: the firmware stops its control loop, and nothing changes here again.
    Now,
}

/// What comes due next, in the order it is applied when two fall at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    KeyDownLimit,
    Link,
    RunLimit,
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
    /// When the key went down, not counting key-ups shorter than
    /// [`limits::MIN_GAP_MS`]: what the key-down limit times.
    key_since: u64,
    /// When the key last went up; `None` if it has never been down.
    key_up_at: Option<u64>,
    /// When the last run ended.
    run_ended_at: Option<u64>,
    /// The duty budget, ms, as of `budget_at`; below zero after a run that held
    /// the key longer than it planned (`TEST STUCK`).
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
    /// A box that started at `now` for `boot`, key up. Its duty budget starts full
    /// only after a power-up: a box that keeps restarting earns its budget again
    /// before it keys. After a watchdog reset it starts tripped (`WATCHDOG`):
    /// whatever stopped its control loop, it keys nothing more until it is
    /// unplugged.
    pub fn new(limits: Limits, boot: Boot, now: u64) -> Self {
        Self {
            limits,
            boot,
            build: "-",
            run: None,
            key: false,
            key_since: now,
            key_up_at: None,
            run_ended_at: None,
            budget: match boot {
                Boot::Power => i64::from(limits.duty_budget_ms),
                _ => 0,
            },
            budget_at: now,
            armed_until: None,
            last_line: now,
            ended: Ended::None,
            trip: match boot {
                Boot::Watchdog => Trip::Watchdog,
                _ => Trip::None,
            },
            hang: Hang::No,
            now,
        }
    }

    /// [`Keyer::new`], then what the box saved before it restarted, if it did
    /// ([`Saved`]): its trip, if it had one, stays; its duty budget is the lower
    /// of the new start's and the saved one, less [`limits::RESTART_CHARGE_MS`]
    /// if its key was down; and it rests ([`limits::REST_MS`]) before its first
    /// run, as after any run.
    pub fn restore(limits: Limits, boot: Boot, now: u64, saved: Option<Saved>) -> Self {
        let mut k = Self::new(limits, boot, now);
        if let Some(s) = saved {
            if s.trip != Trip::None {
                k.trip = s.trip;
            }
            let charge = if s.key {
                i64::from(limits::RESTART_CHARGE_MS)
            } else {
                0
            };
            k.budget = k.budget.min(i64::from(s.budget) - charge);
            k.run_ended_at = Some(now);
        }
        k
    }

    /// What to leave for the next boot at `now` ([`Saved`]).
    pub fn saved(&self, now: u64) -> Saved {
        let budget = self.budget_then(now.max(self.budget_at));
        Saved {
            trip: self.trip,
            key: self.key,
            budget: budget.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
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
    /// so stop feeding the watchdog) with the key as it is.
    pub fn hung(&self) -> bool {
        self.hang == Hang::Now
    }

    /// Bring the box up to `now`; whether the key is down.
    pub fn poll(&mut self, now: u64) -> bool {
        self.poll_with(now, |_, _| {})
    }

    /// [`Keyer::poll`], calling `on_key(at, down)` for every change of the key,
    /// with the time it changed.
    pub fn poll_with(&mut self, now: u64, mut on_key: impl FnMut(u64, bool)) -> bool {
        if self.hang == Hang::Now {
            return self.key;
        }
        let now = now.max(self.now);
        while let Some((at, ev)) = self.next_event() {
            if at > now {
                break;
            }
            self.apply(at, ev, &mut on_key);
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
        if self.key {
            consider(
                self.key_since + u64::from(self.limits.key_down_ms),
                Event::KeyDownLimit,
            );
        }
        if let Some(r) = &self.run {
            consider(
                self.last_line + u64::from(self.limits.link_timeout_ms),
                Event::Link,
            );
            consider(r.start + u64::from(self.limits.run_ms), Event::RunLimit);
            if !(r.stuck && self.key) {
                consider(r.seg_end, Event::Segment);
            }
        }
        next
    }

    fn apply(&mut self, at: u64, ev: Event, on_key: &mut impl FnMut(u64, bool)) {
        match ev {
            Event::KeyDownLimit => self.trip_at(at, Trip::Down, on_key),
            Event::Link => self.stop_run(at, Ended::Link, on_key),
            Event::RunLimit => {
                // A run exactly as long as the limit ends with its last element.
                let done = self
                    .run
                    .as_ref()
                    .is_some_and(|r| r.idx + 1 == r.segs.as_slice().len() && r.seg_end <= at);
                let why = if done { Ended::Done } else { Ended::Limit };
                self.stop_run(at, why, on_key)
            }
            Event::Segment => {
                let Some(r) = &mut self.run else { return };
                r.idx += 1;
                let Some(seg) = r.segs.as_slice().get(r.idx).copied() else {
                    self.stop_run(at, Ended::Done, on_key);
                    return;
                };
                r.seg_end = at + u64::from(seg.units) * u64::from(r.dot_ms);
                self.set_key(at, seg.down, on_key);
            }
        }
    }

    fn set_key(&mut self, at: u64, down: bool, on_key: &mut impl FnMut(u64, bool)) {
        if down != self.key {
            self.settle_budget(at);
            self.key = down;
            if down {
                // A key-up too short to be one (at most a pass of the firmware's
                // loop between two runs) does not restart the key-down limit.
                let short = self
                    .key_up_at
                    .is_some_and(|up| at.saturating_sub(up) < u64::from(limits::MIN_GAP_MS));
                if !short {
                    self.key_since = at;
                }
            } else {
                self.key_up_at = Some(at);
            }
            on_key(at, down);
        }
        if down && self.hang == Hang::Pending {
            self.hang = Hang::Now;
        }
    }

    /// The duty budget at `at`, the key as it has been since it was last settled:
    /// spent while it was down, earned back while it was up.
    fn budget_then(&self, at: u64) -> i64 {
        let dt = i64::try_from(at.saturating_sub(self.budget_at)).unwrap_or(i64::MAX);
        if self.key {
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

    fn stop_run(&mut self, at: u64, why: Ended, on_key: &mut impl FnMut(u64, bool)) {
        if self.run.take().is_some() {
            self.ended = why;
            self.run_ended_at = Some(at);
        }
        if self.hang == Hang::Pending {
            self.hang = Hang::No;
        }
        self.set_key(at, false, on_key);
    }

    fn trip_at(&mut self, at: u64, why: Trip, on_key: &mut impl FnMut(u64, bool)) {
        if self.trip == Trip::None {
            self.trip = why;
        }
        self.stop_run(at, Ended::Down, on_key);
        self.set_key(at, false, on_key);
    }

    /// The firmware saw a fault on its key pin at `now` ([`crate::control`]): open
    /// the key and trip, as the key-down limit does. Nothing more is keyed until
    /// the box is power-cycled.
    pub fn trip_now(&mut self, now: u64, why: Trip, mut on_key: impl FnMut(u64, bool)) {
        self.poll_with(now, &mut on_key);
        if self.hang != Hang::Now {
            self.trip_at(now.max(self.now), why, &mut on_key);
        }
    }

    /// The USB link went away at `now` (bus reset, suspend, deconfigured): stop any
    /// run.
    pub fn link_lost(&mut self, now: u64, mut on_key: impl FnMut(u64, bool)) {
        self.poll_with(now, &mut on_key);
        self.armed_until = None;
        if self.hang != Hang::Now && self.run.is_some() {
            self.stop_run(now.max(self.now), Ended::Usb, &mut on_key);
        }
    }

    /// A line received at `now` (without its newline): the reply to send, or `None`
    /// for a line that does not decode (ignored, and not counted for the link
    /// timeout) or a hung box.
    pub fn handle_line(&mut self, now: u64, line: &[u8]) -> Option<Line> {
        self.handle_line_with(now, line, |_, _| {})
    }

    /// [`Keyer::handle_line`], calling `on_key` for every key change as
    /// [`Keyer::poll_with`] does.
    pub fn handle_line_with(
        &mut self,
        now: u64,
        line: &[u8],
        mut on_key: impl FnMut(u64, bool),
    ) -> Option<Line> {
        self.poll_with(now, &mut on_key);
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
                    "OK HELLO {VERSION} {} {} {} {} {} {now} {} {} {NAME}",
                    self.limits.run_ms / 1000,
                    self.limits.link_timeout_ms,
                    self.limits.key_down_ms,
                    self.limits.rest_ms,
                    self.limits.duty_budget_ms / 1000,
                    self.boot.as_str(),
                    self.build
                ),
            ),
            (_, "STATUS") => frame::encode(
                id,
                format_args!(
                    "OK STATUS {} {} {} {} {} {}",
                    u8::from(self.key),
                    u8::from(self.run.is_some()),
                    self.ended.as_str(),
                    self.trip.as_str(),
                    if self.run.is_some() {
                        0
                    } else {
                        self.rest_left(now)
                    },
                    self.budget_at(now)
                ),
            ),
            (_, "STOP") => {
                self.stop_run(now, Ended::Stop, &mut on_key);
                frame::encode(id, format_args!("OK STOP"))
            }
            ("CW", _) => {
                let r = self.start_cw(now, &body[2..], &mut on_key);
                match r {
                    Ok(()) => frame::encode(id, format_args!("OK CW")),
                    Err(code) => frame::encode(id, format_args!("ERR CW {code}")),
                }
            }
            (_, "TEST ARM") => {
                self.armed_until = Some(now + u64::from(limits::ARM_MS));
                frame::encode(id, format_args!("OK TEST ARM"))
            }
            (_, "TEST HANG") | (_, "TEST STUCK") if self.run.is_none() => {
                frame::encode(id, format_args!("ERR TEST RUN"))
            }
            // A test is taken only just after `TEST ARM`, and once: a stray line
            // (a terminal on the port, a script) cannot start one.
            (_, "TEST HANG") | (_, "TEST STUCK")
                if self.armed_until.take().is_none_or(|until| now > until) =>
            {
                frame::encode(id, format_args!("ERR TEST ARM"))
            }
            (_, "TEST HANG") => {
                self.hang = Hang::Pending;
                if self.key {
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

    /// `CW <wpm> <text>`; `args` is what follows `CW`.
    fn start_cw(
        &mut self,
        now: u64,
        args: &str,
        on_key: &mut impl FnMut(u64, bool),
    ) -> Result<(), CwError> {
        if self.trip != Trip::None {
            return Err(CwError::Trip);
        }
        if self.run.is_some() {
            return Err(CwError::Run);
        }
        let args = args.strip_prefix(' ').ok_or(CwError::Len)?;
        let (wpm, text) = args.split_once(' ').unwrap_or((args, ""));
        if wpm.is_empty() || wpm.len() > 3 || !wpm.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CwError::Wpm);
        }
        let wpm: u32 = wpm.parse().map_err(|_| CwError::Wpm)?;
        if !(MIN_WPM..=MAX_WPM).contains(&wpm) {
            return Err(CwError::Wpm);
        }
        let dot_ms = morse::dot_ms(wpm).map_err(|_| CwError::Wpm)?;
        let segs = Segments::of(text.as_bytes()).map_err(|e| match e {
            TextError::Len => CwError::Len,
            TextError::Char => CwError::Char,
        })?;
        if segs.units() * dot_ms > self.limits.run_ms {
            return Err(CwError::Limit);
        }
        if self.rest_left(now) > 0 {
            return Err(CwError::Rest);
        }
        let down: u64 = segs
            .as_slice()
            .iter()
            .filter(|s| s.down)
            .map(|s| u64::from(s.units) * u64::from(dot_ms))
            .sum();
        if self.budget_at(now) < down {
            return Err(CwError::Duty);
        }
        let first = segs.as_slice()[0];
        self.run = Some(Run {
            segs,
            idx: 0,
            dot_ms,
            start: now,
            seg_end: now + u64::from(first.units) * u64::from(dot_ms),
            stuck: false,
        });
        self.set_key(now, true, on_key);
        Ok(())
    }
}

/// Why `CW` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwError {
    /// Tripped: unplug the box and plug it in again.
    Trip,
    /// A run is already under way.
    Run,
    Wpm,
    Len,
    Char,
    /// Longer than the run limit at that speed.
    Limit,
    /// The last run ended less than the rest ([`limits::REST_MS`]) ago.
    Rest,
    /// The duty budget holds less than the run's key-down time.
    Duty,
}

impl fmt::Display for CwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trip => "TRIP",
            Self::Run => "RUN",
            Self::Wpm => "WPM",
            Self::Len => "LEN",
            Self::Char => "CHAR",
            Self::Limit => "LIMIT",
            Self::Rest => "REST",
            Self::Duty => "DUTY",
        })
    }
}

#[cfg(test)]
mod tests;
