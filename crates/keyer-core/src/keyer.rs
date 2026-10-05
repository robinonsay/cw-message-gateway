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
//! | `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <uptime ms> <boot> <name>` |
//! | `STATUS` | `OK STATUS <key> <run> <ended> <trip>` |
//! | `CW <wpm> <text>` | `OK CW`, keying from that moment |
//! | `STOP` | `OK STOP` |
//! | `TEST HANG` | `OK TEST HANG` (bring-up only) |
//! | `TEST STUCK` | `OK TEST STUCK` (bring-up only) |
//!
//! Errors are `ERR <command> <code>`: `CW` with `TRIP`, `RUN`, `WPM`, `LEN`, `CHAR`
//! or `LIMIT`; `TEST` with `RUN` or `UNKNOWN`; anything else `ERR <word> UNKNOWN`.

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
}

impl Limits {
    /// The real box's ([`crate::limits`]).
    pub const BOX: Self = Self {
        key_down_ms: limits::KEY_DOWN_MS,
        run_ms: limits::RUN_MS,
        link_timeout_ms: limits::LINK_TIMEOUT_MS,
    };
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
    /// The key-down limit (and the box tripped).
    Down,
    /// USB unplugged, or the port closed.
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

/// A fault that stops the box keying until it is power-cycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    None,
    /// The key stayed down past the key-down limit.
    Down,
}

impl Trip {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Down => "DOWN",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::None, Self::Down]
            .into_iter()
            .find(|t| t.as_str() == s)
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
    run: Option<Run>,
    key: bool,
    key_since: u64,
    last_line: u64,
    ended: Ended,
    trip: Trip,
    hang: Hang,
    now: u64,
}

impl Keyer {
    /// A box that started at `now` for `boot`, key up.
    pub fn new(limits: Limits, boot: Boot, now: u64) -> Self {
        Self {
            limits,
            boot,
            run: None,
            key: false,
            key_since: now,
            last_line: now,
            ended: Ended::None,
            trip: Trip::None,
            hang: Hang::No,
            now,
        }
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
            Event::KeyDownLimit => {
                self.trip = Trip::Down;
                self.stop_run(at, Ended::Down, on_key);
                self.set_key(at, false, on_key);
            }
            Event::Link => self.stop_run(at, Ended::Link, on_key),
            Event::RunLimit => self.stop_run(at, Ended::Limit, on_key),
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
            self.key = down;
            if down {
                self.key_since = at;
            }
            on_key(at, down);
        }
        if down && self.hang == Hang::Pending {
            self.hang = Hang::Now;
        }
    }

    fn stop_run(&mut self, at: u64, why: Ended, on_key: &mut impl FnMut(u64, bool)) {
        if self.run.take().is_some() {
            self.ended = why;
        }
        if self.hang == Hang::Pending {
            self.hang = Hang::No;
        }
        self.set_key(at, false, on_key);
    }

    /// USB was unplugged or the port closed at `now`: stop any run.
    pub fn link_lost(&mut self, now: u64, mut on_key: impl FnMut(u64, bool)) {
        self.poll_with(now, &mut on_key);
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
                    "OK HELLO {VERSION} {} {} {} {now} {} {NAME}",
                    self.limits.run_ms / 1000,
                    self.limits.link_timeout_ms,
                    self.limits.key_down_ms,
                    self.boot.as_str()
                ),
            ),
            (_, "STATUS") => frame::encode(
                id,
                format_args!(
                    "OK STATUS {} {} {} {}",
                    u8::from(self.key),
                    u8::from(self.run.is_some()),
                    self.ended.as_str(),
                    self.trip.as_str()
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
            (_, "TEST HANG") | (_, "TEST STUCK") if self.run.is_none() => {
                frame::encode(id, format_args!("ERR TEST RUN"))
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
        })
    }
}

#[cfg(test)]
mod tests;
