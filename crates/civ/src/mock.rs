//! A byte-level mock IC-7300 for tests: a fake serial port that speaks CI-V.
//!
//! [`MockPort`] implements [`Port`], so the real driver, `Ic7300<MockPort>`, is
//! exercised byte for byte. Behind the port, [`MockRadio`] parses each frame, checks
//! it against ICOM's *IC-7300 Full Manual* (`IC-7300_ENG_FM_12b`), Section 19
//! "CONTROL COMMAND", answers exactly as the manual describes, and models what the
//! node relies on: the keyer, semi break-in, the transmit status, the meters and the
//! tuner. A text copy of the manual is kept in the project files at
//! `reference/IC-7300_ENG_FM_12b.txt` (Section 19 from line 8531, the command table
//! from line 8771).
//!
//! What it checks, from the manual:
//! - Frames are `FE FE 94 E0 Cn Sc Data FD`; the radio answers `FE FE E0 94 FB FD`
//!   (OK), `... FA FD` (NG), or for a read the command, sub-command and data
//!   (p. 19-2).
//! - Every command the driver sends, with its data format and range (pp. 19-3 to
//!   19-7, 19-9, 19-13). Anything else, malformed data, out-of-range or non-BCD
//!   values are answered NG and recorded as a [`Violation`], so tests can require
//!   none.
//! - Command 17: "Up to 30 characters" of the listed codes, `FF` stops sending
//!   (p. 19-13), and the text is transmitted only "in the CW mode, if the
//!   [TRANSMIT] or an external TX switch is ON, or the Break-in function is ON"
//!   (footnote *2, p. 19-8).
//!
//! What it models, and where the manual leaves a choice to the mock (each is a
//! setting in [`MockConfig`] or a [`Fault`]):
//! - The keyer sends at the [KEY SPEED] level ("00 00=6wpm, 02 55=48wpm", 14 0C,
//!   p. 19-3) with PARIS timing (dot 1, dash 3, gaps 1, 3 and 7). Linear between the
//!   end points, as the driver assumes; the manual gives only the end points.
//! - Semi break-in "transmits when keying, and then automatically returns to receive
//!   after a preset time after you stop keying" (p. 4-15): the Break-IN Delay ("00
//!   00=2.0d to 02 55=13.0d", 14 0F, p. 19-3), linear like the driver assumes. Full
//!   break-in "immediately returns to receive after keying up" (p. 4-16).
//! - `1C 00` reads "00" on receive and "01" on transmit (p. 19-7).
//! - The Po and SWR meters (15 11, 15 12, p. 19-3) read output only while the key is
//!   down, on ICOM's calibration points with the same linear interpolation as the
//!   driver. The protection function's "Power down transmission" (p. 13-4) can be
//!   modelled as a fold-back of the output above a set SWR.
//! - The tuner (1C 01, p. 19-7) tunes for "2~3 seconds" with a carrier and reports
//!   "02" meanwhile; it matches loads "of less than 3:1" and "reduces the SWR to
//!   less than 1.5:1", otherwise "TUNE disappears and the tuning circuit is
//!   automatically bypassed" (p. 11-2).
//! - Command 17 and the tuner put out RF only inside the transmitter's frequency
//!   coverage (p. 16-2); command 05 accepts the receiver's.
//! - CI-V USB Echo Back (1A 05 00 75, "00=ON, 01=OFF", p. 19-5) repeats every frame
//!   received. ICOM's default is OFF (p. 12-11); the mock defaults to ON so that the
//!   driver's skipping of its own echoed frames is exercised.
//! - CI-V Transceive is ON by default: "When you change a setting on the
//!   transceiver, the same change is automatically set on other connected
//!   transceivers", to "the default transceive address", 00h (p. 12-10). A change
//!   made at the radio ([`MockRadio::turn_dial`], [`MockRadio::select_mode`]) is sent
//!   unasked as command 00 "Send frequency data (transceive)" or 01 "Send mode data
//!   (transceive)" (p. 19-3), with the data of p. 19-9. The manual does not say
//!   whether a change made by CI-V command is sent too; the mock does not send one.
//!
//! Everything the radio puts on the air is recorded in radio time (real time
//! multiplied by [`MockConfig::time_scale`]): the text each keyer message actually
//! got out, every transmit period, total and longest transmit and key-down times.

use crate::frame::{bcd_be, bcd_le, from_bcd_be, from_bcd_le, CONTROLLER, END, NG, OK, PREAMBLE};
use crate::ic7300::Port;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Open end of an interval that has not ended yet.
const OPEN: Duration = Duration::MAX;

/// The radio's settings and behaviour that tests choose.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// The transceiver's CI-V address; 94h is the default (p. 19-2).
    pub address: u8,
    /// CI-V USB Echo Back (p. 12-11). Can also be changed with 1A 05 00 75.
    pub echo: bool,
    /// Radio time runs this many times faster than real time: keying, break-in and
    /// tuning take `1 / time_scale` of their real length.
    pub time_scale: f32,
    /// From a CW message being accepted to its first element (radio time).
    pub tx_on_delay: Duration,
    /// Length of a tuner cycle (radio time); "2~3 seconds" (p. 11-2).
    pub tune_time: Duration,
    /// After `17 FF` or `1C 00 00` cuts a message short, stay on transmit for the
    /// break-in delay, as after the key goes up (p. 4-15). The manual does not say
    /// whether either command cuts the delay short, so the mock assumes it does not,
    /// which is the harder case for the node.
    pub hang_after_stop: bool,
    /// SWR of the load (the antenna) the radio sees with the tuner bypassed or not
    /// yet matched to it; the meter reads it while there is output.
    pub swr: f32,
    /// SWR the meter reads once the tuner has matched the load: "less than 1.5:1"
    /// (p. 11-2). A load that already reads lower keeps its own.
    pub tuned_swr: f32,
    /// Output reduction into a bad load.
    pub foldback: Option<Foldback>,
    /// How long a read waits for data before reporting a timeout (real time), as
    /// the serial port's read timeout does.
    pub read_timeout: Duration,
    /// CI-V Transceive (p. 12-10): send changes made at the radio unasked.
    pub transceive: bool,
    /// The filter command 06 selects when its filter byte is skipped: "the default
    /// filter setting of the operating mode" (p. 19-9), which the manual does not
    /// give. The mock uses FIL2, so that a driver relying on FIL1 there reads back
    /// something else (command 01 would select FIL1).
    pub default_filter: u8,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            address: 0x94,
            echo: true,
            time_scale: 1.0,
            tx_on_delay: Duration::from_millis(10),
            tune_time: Duration::from_millis(2500),
            hang_after_stop: true,
            swr: 1.2,
            tuned_swr: 1.3,
            foldback: None,
            read_timeout: Duration::from_millis(2),
            transceive: true,
            default_filter: 0x02,
        }
    }
}

/// "Power down transmission: Reduces the transmission output power" (p. 13-4):
/// above `above_swr` the output is `fraction` of the set power. With no output the
/// SWR meter has nothing to measure and reads 1.0.
#[derive(Debug, Clone, Copy)]
pub struct Foldback {
    pub above_swr: f32,
    pub fraction: f32,
}

/// What an injected reply fault does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplyFault {
    /// The radio acts on the command but its reply is lost.
    Drop,
    /// The radio acts on the command; the reply arrives this long (real time) later.
    Delay(Duration),
    /// The radio answers NG and does nothing.
    Ng,
}

#[derive(Debug, Clone)]
pub enum Fault {
    /// For commands whose body (command, sub-command, data) starts with `cmd`: let
    /// the first `skip` through, then fault the next `times`.
    Reply {
        cmd: Vec<u8>,
        skip: usize,
        times: usize,
        kind: ReplyFault,
    },
    /// After `skip` more keyer messages, the next one leaves the radio on transmit
    /// when it ends, with the key down too if `carrier`. If `recoverable`, `17 FF`
    /// or `1C 00 00` ends it; otherwise only [`MockRadio::clear_stuck`] does (a
    /// hardware timer or a power cycle).
    StickInTx {
        skip: usize,
        carrier: bool,
        recoverable: bool,
    },
    /// Tuner cycles never report done: 1C 01 reads "02" for ever. The carrier
    /// still ends after [`MockConfig::tune_time`].
    TuneNeverFinishes,
}

/// A frame the mock could not accept as the manual describes it, or one the node
/// must never send.
#[derive(Debug, Clone)]
pub struct Violation {
    /// Radio time.
    pub at: Duration,
    /// The whole frame (or the stray bytes) as received.
    pub bytes: Vec<u8>,
    pub reason: String,
}

/// One keyer message (command 17).
#[derive(Debug, Clone)]
pub struct Keyed {
    /// The text as accepted, upper case.
    pub text: String,
    /// The part of it actually keyed: every character whose last element finished.
    pub sent: String,
    /// Radio time it was accepted, its first element, and its last key-up.
    pub accepted: Duration,
    pub start: Duration,
    pub end: Duration,
    /// Keyed to the end (not stopped part-way, and finished by now).
    pub complete: bool,
    /// Whether it went on the air at all (footnote *2, p. 19-8).
    pub on_air: bool,
}

/// A continuous period on transmit.
#[derive(Debug, Clone)]
pub struct TxPeriod {
    pub start: Duration,
    /// `None` while still transmitting.
    pub end: Option<Duration>,
    /// Key-down (RF output) time within it.
    pub key_down: Duration,
}

/// Everything recorded so far.
#[derive(Debug, Clone)]
pub struct Report {
    /// Radio time now.
    pub now: Duration,
    pub keyed: Vec<Keyed>,
    pub transmissions: Vec<TxPeriod>,
    pub total_tx: Duration,
    pub total_key_down: Duration,
    /// Longest continuous transmit period.
    pub max_tx: Duration,
    /// Longest continuous key-down (carrier) run.
    pub max_key_down: Duration,
    pub violations: Vec<Violation>,
    /// Tuner cycles started.
    pub tunes: u32,
    pub transmitting: bool,
    pub keyer_busy: bool,
}

/// The radio's settings as last set.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub frequency_hz: u64,
    pub mode: u8,
    pub filter: u8,
    pub rf_power_level: u16,
    pub key_speed_level: u16,
    pub break_in: u8,
    pub break_in_delay_level: u16,
    pub tuner: u8,
    pub echo: bool,
}

#[derive(Debug)]
struct Stuck {
    carrier: bool,
    recoverable: bool,
    cleared: Option<Duration>,
}

#[derive(Debug)]
struct Piece {
    text: String,
    accepted: Duration,
    /// Key-down runs, radio time.
    marks: Vec<(Duration, Duration)>,
    /// (end of the character's last element, length of `text` up to it).
    chars: Vec<(Duration, usize)>,
    hang: Duration,
    stop: Option<Duration>,
    on_air: bool,
    stuck: Option<Stuck>,
}

impl Piece {
    fn first(&self) -> Duration {
        self.marks.first().map_or(self.accepted, |m| m.0)
    }

    fn last(&self) -> Duration {
        self.marks.last().map_or(self.accepted, |m| m.1)
    }

    /// When the keyer stopped keying it: at its end, or where it was stopped.
    fn keying_end(&self) -> Duration {
        self.stop.map_or(self.last(), |s| s.min(self.last()))
    }

    fn busy(&self, t: Duration) -> bool {
        self.on_air && t < self.keying_end()
    }

    fn sent(&self, t: Duration) -> String {
        let until = t.min(self.keying_end());
        let n = self
            .chars
            .iter()
            .take_while(|c| c.0 <= until)
            .last()
            .map_or(0, |c| c.1);
        self.text[..n].trim().to_string()
    }
}

#[derive(Debug)]
struct Tune {
    start: Duration,
    end: Duration,
    never: bool,
    /// The load SWR it was started on, if it can match it.
    matched: Option<f32>,
}

#[derive(Debug)]
struct State {
    cfg: MockConfig,
    epoch: Instant,
    /// Bytes written by the controller, not yet a whole frame.
    input: Vec<u8>,
    /// Bytes that have arrived at the controller and can be read.
    output: VecDeque<u8>,
    /// Bytes still on their way (a delayed reply), by real arrival time.
    in_flight: Vec<(Instant, Vec<u8>)>,
    frequency_hz: u64,
    /// Split on, transmitting on this frequency (the other VFO).
    split_tx_hz: Option<u64>,
    delta_tx: bool,
    mode: u8,
    filter: u8,
    rf_power: u16,
    key_speed: u16,
    break_in: u8,
    break_in_delay: u16,
    tuner: u8,
    tune: Option<Tune>,
    tunes: u32,
    pieces: Vec<Piece>,
    /// Transmit switched on by 1C 00 01: (on, off).
    forced: Vec<(Duration, Duration)>,
    faults: Vec<Fault>,
    violations: Vec<Violation>,
    commands: Vec<(Duration, Vec<u8>)>,
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
}

/// The mock radio. Clones share the same radio.
#[derive(Clone)]
pub struct MockRadio(Arc<Shared>);

/// The radio's USB serial port, as the driver sees it.
pub struct MockPort(MockRadio);

/// "The default transceive address is “00h.”" (p. 12-10).
const TRANSCEIVE_ADDRESS: u8 = 0x00;

/// Transmitter frequency coverage in Hz, from "Transmitter 1.800000~01.999999" MHz
/// on (p. 16-2). It is narrower "depending on the transceiver version"; the mock
/// takes the whole list.
const TX_RANGES: [(u64, u64); 12] = [
    (1_800_000, 1_999_999),
    (3_500_000, 3_999_999),
    (5_255_000, 5_405_000),
    (7_000_000, 7_300_000),
    (10_100_000, 10_150_000),
    (14_000_000, 14_350_000),
    (18_068_000, 18_168_000),
    (21_000_000, 21_450_000),
    (24_890_000, 24_990_000),
    (28_000_000, 29_700_000),
    (50_000_000, 54_000_000),
    (70_000_000, 70_500_000),
];

/// Meter calibration points (level, value) from the 15 11 and 15 12 rows (p. 19-3).
const PO_POINTS: [(f32, f32); 3] = [(0.0, 0.0), (143.0, 50.0), (213.0, 100.0)];
const SWR_POINTS: [(f32, f32); 4] = [(0.0, 1.0), (48.0, 1.5), (80.0, 2.0), (120.0, 3.0)];

/// The meter level that reads `value`, linear between ICOM's points and along the
/// last segment beyond them, within 0-255.
fn meter_level(points: &[(f32, f32)], value: f32) -> u16 {
    let seg = points
        .windows(2)
        .find(|w| value <= w[1].1)
        .unwrap_or(&points[points.len() - 2..]);
    let ((x0, y0), (x1, y1)) = (seg[0], seg[1]);
    let level = x0 + (value - y0) * (x1 - x0) / (y1 - y0);
    level.round().clamp(0.0, 255.0) as u16
}

/// Characters command 17 accepts: "Codes for CW message contents" (p. 19-13), plus
/// `^`, which "is used to transmit a string of characters with no inter-character
/// space" (its timing is not modelled; the node never sends it).
fn cw_byte_allowed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b" /?.-,:'()=+\"@^".contains(&b)
}

/// Key-down runs and character ends for `text` starting at `start`, with PARIS
/// timing at `dot`: dash 3, element gap 1, character gap 3, word gap 7.
type Schedule = (Vec<(Duration, Duration)>, Vec<(Duration, usize)>);

fn schedule(text: &str, start: Duration, dot: Duration) -> Schedule {
    let (mut marks, mut chars) = (Vec::new(), Vec::new());
    let mut t = start;
    let mut gap = 0;
    for (i, c) in text.char_indices() {
        if c == ' ' {
            if !marks.is_empty() {
                gap = 7;
            }
            continue;
        }
        let Some(p) = cw::encode_char(c) else {
            continue;
        };
        if !marks.is_empty() {
            t += dot * gap;
        }
        for (j, e) in p.chars().enumerate() {
            if j > 0 {
                t += dot;
            }
            let len = dot * if e == '.' { 1 } else { 3 };
            marks.push((t, t + len));
            t += len;
        }
        chars.push((t, i + c.len_utf8()));
        gap = 3;
    }
    (marks, chars)
}

/// Merge overlapping intervals, clipping open ones at `now`.
fn merge(mut v: Vec<(Duration, Duration)>, now: Duration) -> Vec<(Duration, Duration)> {
    v.retain(|i| i.0 < now.min(i.1));
    v.sort();
    let mut out: Vec<(Duration, Duration)> = Vec::new();
    for (s, e) in v {
        let e = e.min(now);
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn overlap(a: &[(Duration, Duration)], from: Duration, to: Duration) -> Duration {
    a.iter()
        .map(|&(s, e)| e.min(to).saturating_sub(s.max(from)))
        .sum()
}

impl State {
    fn now(&self) -> Duration {
        self.epoch.elapsed().mul_f32(self.cfg.time_scale)
    }

    /// Keyer speed in wpm for the 14 0C level: "00 00=6wpm, 02 55=48wpm" (p. 19-3).
    fn dot(&self) -> Duration {
        let wpm = 6.0 + self.key_speed as f32 * 42.0 / 255.0;
        Duration::from_secs_f32(1.2 / wpm)
    }

    /// The semi break-in delay: "00 00=2.0d to 02 55=13.0d" (14 0F, p. 19-3). Full
    /// break-in returns to receive at once (p. 4-16).
    fn hang(&self) -> Duration {
        if self.break_in == 0x02 {
            return Duration::ZERO;
        }
        self.dot()
            .mul_f32(2.0 + self.break_in_delay as f32 * 11.0 / 255.0)
    }

    /// Transmit (or, with `carrier`, key-down) intervals; open ones end at [`OPEN`].
    fn intervals(&self, carrier: bool) -> Vec<(Duration, Duration)> {
        let mut v = Vec::new();
        for p in self.pieces.iter().filter(|p| p.on_air) {
            let stop = p.stop.unwrap_or(OPEN);
            for &(s, e) in p.marks.iter().filter(|m| m.0 < stop) {
                let e = e.min(stop);
                if carrier {
                    v.push((s, e));
                } else if p.stop.is_some() && !self.cfg.hang_after_stop {
                    v.push((s, (e + p.hang).min(stop)));
                } else {
                    v.push((s, e + p.hang));
                }
            }
            if let Some(st) = &p.stuck {
                if !carrier || st.carrier {
                    v.push((p.keying_end(), st.cleared.unwrap_or(OPEN)));
                }
            }
        }
        if let Some(t) = &self.tune {
            v.push((t.start, t.start + self.cfg.tune_time));
        }
        if !carrier {
            v.extend(self.forced.iter().copied());
        }
        v
    }

    /// Transmitting (or, with `carrier`, key down) at `t`. The same as looking `t`
    /// up in [`State::intervals`], without building them all.
    fn at(&self, carrier: bool, t: Duration) -> bool {
        let piece = |p: &Piece| {
            if let Some(st) = p.stuck.as_ref().filter(|st| !carrier || st.carrier) {
                if p.keying_end() <= t && st.cleared.is_none_or(|c| t < c) {
                    return true;
                }
            }
            let stop = p.stop.unwrap_or(OPEN);
            let hang = if carrier { Duration::ZERO } else { p.hang };
            if !p.on_air || t < p.first() || t >= p.keying_end() + hang {
                return false;
            }
            p.marks.iter().filter(|m| m.0 < stop).any(|&(s, e)| {
                let e = e.min(stop);
                let end = if carrier {
                    e
                } else if p.stop.is_some() && !self.cfg.hang_after_stop {
                    (e + hang).min(stop)
                } else {
                    e + hang
                };
                s <= t && t < end
            })
        };
        self.pieces.iter().any(piece)
            || self
                .tune
                .as_ref()
                .is_some_and(|tu| tu.start <= t && t < tu.start + self.cfg.tune_time)
            || !carrier && self.forced.iter().any(|&(s, e)| s <= t && t < e)
    }

    fn keyed(&self, p: &Piece, now: Duration) -> Keyed {
        Keyed {
            text: p.text.clone(),
            sent: p.sent(now),
            accepted: p.accepted,
            start: p.first(),
            end: p.keying_end(),
            complete: p.on_air && p.stop.is_none_or(|s| s >= p.last()) && now >= p.last(),
            on_air: p.on_air,
        }
    }

    fn keyer_busy(&self, t: Duration) -> bool {
        self.pieces.iter().any(|p| p.busy(t))
    }

    fn tuning(&self, t: Duration) -> bool {
        self.tune.as_ref().is_some_and(|tu| tu.never || t < tu.end)
    }

    /// The SWR the radio sees at `t`: the load's, unless the tuner is in line and
    /// has finished matching this load, which "reduces the SWR to less than 1.5:1"
    /// (p. 11-2).
    fn swr(&self, t: Duration) -> f32 {
        let load = self.cfg.swr;
        let tuned = self.tune.as_ref().is_some_and(|tu| {
            self.tuner == 0x01 && !tu.never && t >= tu.end && tu.matched == Some(load)
        });
        if tuned {
            load.min(self.cfg.tuned_swr)
        } else {
            load
        }
    }

    /// Output in percent of full (100 W), with the key down.
    fn po_percent(&self, t: Duration) -> f32 {
        // [RF PWR] "00 00=max. CCW, 02 55=max. CW" (14 0A, p. 19-3): taken as
        // linear from 0 to 100 W, the assumption the driver makes.
        let pct = self.rf_power as f32 * 100.0 / 255.0;
        match self.cfg.foldback {
            Some(f) if self.swr(t) > f.above_swr => pct * f.fraction,
            _ => pct,
        }
    }

    fn in_tx_range(&self) -> bool {
        TX_RANGES
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&self.frequency_hz))
    }

    /// Send a transceive frame (command 00 or 01) unasked, if CI-V Transceive is on.
    fn transceive(&mut self, body: &[u8]) {
        if self.cfg.transceive {
            self.reply(TRANSCEIVE_ADDRESS, body, None);
        }
    }

    fn violation(&mut self, at: Duration, bytes: &[u8], reason: impl Into<String>) {
        self.violations.push(Violation {
            at,
            bytes: bytes.to_vec(),
            reason: reason.into(),
        });
    }

    /// Take everything that has arrived by now off the wire.
    fn deliver(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.in_flight.len() {
            if self.in_flight[i].0 <= now {
                let (_, bytes) = self.in_flight.remove(i);
                self.output.extend(bytes);
            } else {
                i += 1;
            }
        }
    }

    /// Handle bytes from the controller.
    fn receive(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
        loop {
            let Some(start) = self
                .input
                .windows(2)
                .position(|w| w == [PREAMBLE, PREAMBLE])
            else {
                // Keep a lone trailing FE: it may be the start of a preamble.
                let keep = usize::from(self.input.last() == Some(&PREAMBLE));
                let stray: Vec<u8> = self.input.drain(..self.input.len() - keep).collect();
                if !stray.is_empty() {
                    let now = self.now();
                    self.violation(now, &stray, "bytes outside a frame");
                }
                return;
            };
            if start > 0 {
                let stray: Vec<u8> = self.input.drain(..start).collect();
                let now = self.now();
                self.violation(now, &stray, "bytes outside a frame");
            }
            let Some(end) = self.input.iter().position(|&b| b == END) else {
                return;
            };
            let frame: Vec<u8> = self.input.drain(..=end).collect();
            self.frame(&frame);
        }
    }

    fn frame(&mut self, raw: &[u8]) {
        let now = self.now();
        if self.cfg.echo {
            self.output.extend(raw);
        }
        // FE FE <to> <from> <body> FD (p. 19-2); extra preamble bytes are allowed.
        let mut i = 2;
        while i < raw.len() && raw[i] == PREAMBLE {
            i += 1;
        }
        let inner = &raw[i..raw.len() - 1];
        if inner.len() < 3 {
            self.violation(now, raw, "frame without address and command");
            return;
        }
        let (to, from, body) = (inner[0], inner[1], &inner[2..]);
        if to != self.cfg.address {
            self.violation(
                now,
                raw,
                format!("addressed to {to:02X}h, not {:02X}h", self.cfg.address),
            );
            return;
        }
        if from != CONTROLLER {
            self.violation(now, raw, format!("from {from:02X}h, not E0h (p. 19-2)"));
        }
        self.commands.push((now, body.to_vec()));
        let fault = self.take_fault(body);
        if fault == Some(ReplyFault::Ng) {
            self.reply(from, &[NG], None);
            return;
        }
        let reply = match self.command(now, body) {
            Ok(r) => r,
            Err(why) => {
                self.violation(now, raw, why);
                vec![NG]
            }
        };
        match fault {
            Some(ReplyFault::Drop) => {}
            Some(ReplyFault::Delay(d)) => self.reply(from, &reply, Some(d)),
            _ => self.reply(from, &reply, None),
        }
    }

    fn reply(&mut self, to: u8, body: &[u8], delay: Option<Duration>) {
        let mut bytes = vec![PREAMBLE, PREAMBLE, to, self.cfg.address];
        bytes.extend_from_slice(body);
        bytes.push(END);
        match delay {
            Some(d) => self.in_flight.push((Instant::now() + d, bytes)),
            None => self.output.extend(bytes),
        }
    }

    /// The reply fault for `body`, if one is armed and due.
    fn take_fault(&mut self, body: &[u8]) -> Option<ReplyFault> {
        let i = self
            .faults
            .iter()
            .position(|f| matches!(f, Fault::Reply { cmd, .. } if body.starts_with(cmd)))?;
        let Fault::Reply {
            skip, times, kind, ..
        } = &mut self.faults[i]
        else {
            unreachable!()
        };
        if *skip > 0 {
            *skip -= 1;
            return None;
        }
        let kind = *kind;
        *times -= 1;
        if *times == 0 {
            self.faults.remove(i);
        }
        Some(kind)
    }

    /// Act on one command body; the reply body, or why it is refused (NG).
    fn command(&mut self, now: Duration, body: &[u8]) -> Result<Vec<u8>, String> {
        match body {
            // 03 "Read operating frequency" (p. 19-3), data as on p. 19-9.
            [0x03] => Ok([&[0x03][..], &bcd_le(self.frequency_hz, 5)].concat()),
            // 04 "Read operating mode" (p. 19-3): mode and filter (p. 19-9).
            [0x04] => Ok(vec![0x04, self.mode, self.filter]),
            // 0F "Read Split setting (00=OFF, 01=ON)" (p. 19-3).
            [0x0F] => Ok(vec![0x0F, u8::from(self.split_tx_hz.is_some())]),
            // 21 02 "Send/read ∂TX setting (00=OFF, 01=ON)" (p. 19-7).
            [0x21, 0x02] => Ok(vec![0x21, 0x02, u8::from(self.delta_tx)]),
            // 1C 03 "Read transmit frequency" (p. 19-7), as for 03 (p. 19-9). The
            // manual does not say whether a ∂TX offset is included; the mock leaves
            // it out.
            [0x1C, 0x03] => {
                let hz = self.split_tx_hz.unwrap_or(self.frequency_hz);
                Ok([&[0x1C, 0x03][..], &bcd_le(hz, 5)].concat())
            }
            [0x05, data @ ..] => self.set_frequency(data),
            [0x06, data @ ..] => self.set_mode(data),
            // 14 0A [RF PWR], 14 0C [KEY SPEED], 14 0F Break-IN Delay: "00 00 to
            // 02 55" (p. 19-3).
            [0x14, sub @ (0x0A | 0x0C | 0x0F), data @ ..] => {
                let field = match sub {
                    0x0A => &mut self.rf_power,
                    0x0C => &mut self.key_speed,
                    _ => &mut self.break_in_delay,
                };
                if data.is_empty() {
                    return Ok([&[0x14, *sub][..], &bcd_be(*field as u64, 2)].concat());
                }
                *field = level(data)?;
                Ok(vec![OK])
            }
            // 15 11 Po and 15 12 SWR meters: read only (p. 19-3).
            [0x15, sub @ (0x11 | 0x12)] => {
                let key_down = self.at(true, now);
                let po = if key_down { self.po_percent(now) } else { 0.0 };
                let value = match sub {
                    0x11 => meter_level(&PO_POINTS, po),
                    // With no forward power there is nothing to measure.
                    _ if po <= 0.0 => 0,
                    _ => meter_level(&SWR_POINTS, self.swr(now)),
                };
                Ok([&[0x15, *sub][..], &bcd_be(value as u64, 2)].concat())
            }
            // 16 47 "BK-IN function (00=BK-IN OFF, 01=Semi BK-IN ON, 02=Full BK-IN
            // ON)" (p. 19-3).
            [0x16, 0x47] => Ok(vec![0x16, 0x47, self.break_in]),
            [0x16, 0x47, v @ 0x00..=0x02] => {
                self.break_in = *v;
                Ok(vec![OK])
            }
            [0x17, data @ ..] => self.send_cw(now, data),
            // 1A 05 00 75: "echo back setting for CI-V operation from USB (00=ON,
            // 01=OFF)" (p. 19-5).
            [0x1A, 0x05, 0x00, 0x75] => Ok(vec![0x1A, 0x05, 0x00, 0x75, u8::from(!self.cfg.echo)]),
            [0x1A, 0x05, 0x00, 0x75, v @ (0x00 | 0x01)] => {
                self.cfg.echo = *v == 0x00;
                Ok(vec![OK])
            }
            // 1C 00 transceiver's status, "00" RX, "01" TX (p. 19-7).
            [0x1C, 0x00] => Ok(vec![0x1C, 0x00, u8::from(self.at(false, now))]),
            [0x1C, 0x00, 0x00] => {
                self.forced
                    .iter_mut()
                    .filter(|f| f.1 == OPEN)
                    .for_each(|f| f.1 = now);
                self.stop(now);
                Ok(vec![OK])
            }
            [0x1C, 0x00, 0x01] => {
                // Valid, but the node must never switch the transmitter on itself.
                self.violation(now, body, "node forced transmit on (1C 00 01)");
                if !self.forced.iter().any(|f| f.1 == OPEN) {
                    self.forced.push((now, OPEN));
                }
                Ok(vec![OK])
            }
            // 1C 01: "00=... tuner OFF, 01=... tuner ON, 02=... to tuning" (p. 19-7).
            [0x1C, 0x01] => {
                let v = if self.tuning(now) { 0x02 } else { self.tuner };
                Ok(vec![0x1C, 0x01, v])
            }
            [0x1C, 0x01, v @ (0x00 | 0x01)] => {
                self.tuner = *v;
                Ok(vec![OK])
            }
            [0x1C, 0x01, 0x02] => {
                if self.keyer_busy(now) {
                    self.violation(now, body, "tune started while the keyer is sending");
                }
                if !self.in_tx_range() {
                    // No carrier outside the transmitter's coverage (p. 16-2). The
                    // manual does not say how the radio answers; OK, as for 17.
                    self.violation(
                        now,
                        body,
                        format!(
                            "tune started on {} Hz, outside the transmit ranges (p. 16-2)",
                            self.frequency_hz
                        ),
                    );
                    return Ok(vec![OK]);
                }
                // Matches loads "of less than 3:1"; otherwise "TUNE disappears and the
                // tuning circuit is automatically bypassed" (p. 11-2).
                let matched = self.cfg.swr < 3.0;
                let never = self
                    .faults
                    .iter()
                    .any(|f| matches!(f, Fault::TuneNeverFinishes));
                self.tune = Some(Tune {
                    start: now,
                    end: now + self.cfg.tune_time,
                    never,
                    matched: matched.then_some(self.cfg.swr),
                });
                self.tuner = if matched { 0x01 } else { 0x00 };
                self.tunes += 1;
                Ok(vec![OK])
            }
            [0x03 | 0x04, ..]
            | [0x15, 0x11 | 0x12, ..]
            | [0x16, 0x47, ..]
            | [0x1A, 0x05, 0x00, 0x75, ..]
            | [0x1C, 0x00 | 0x01, ..] => Err(format!(
                "{:02X?}: data not allowed for this command (pp. 19-3 to 19-7)",
                body
            )),
            _ => Err(format!("{body:02X?}: command not modelled by the mock")),
        }
    }

    /// 05 "Set operating frequency" (p. 19-3): five BCD bytes, 10 Hz/1 Hz first,
    /// with the 1000 MHz and 100 MHz digits "0 (Fixed)" (p. 19-9). Refused outside
    /// the receiver's coverage, "0.030000~74.800000" MHz (p. 16-2); the manual does
    /// not say how the radio answers one, so this NG is the mock's choice.
    fn set_frequency(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let hz = match from_bcd_le(data) {
            Some(hz) if data.len() == 5 && data[4] == 0x00 => hz,
            _ => {
                return Err(format!(
                    "05 {data:02X?}: not 5 BCD bytes ending 00 (p. 19-9)"
                ))
            }
        };
        if !(30_000..=74_800_000).contains(&hz) {
            return Err(format!("05: {hz} Hz is outside 0.03-74.8 MHz (p. 16-2)"));
        }
        self.frequency_hz = hz;
        Ok(vec![OK])
    }

    /// 06 "Operating mode selection for transceive" (p. 19-3): mode 00-05, 07, 08
    /// and filter 01-03; "Filter setting (2) can be skipped", and then "the default
    /// filter setting of the operating mode is automatically selected" (p. 19-9).
    fn set_mode(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mode_ok = |m: &u8| matches!(m, 0x00..=0x05 | 0x07 | 0x08);
        let filter_ok = |f: &u8| matches!(f, 0x01..=0x03);
        match data {
            [m] if mode_ok(m) => (self.mode, self.filter) = (*m, self.cfg.default_filter),
            [m, f] if mode_ok(m) && filter_ok(f) => (self.mode, self.filter) = (*m, *f),
            _ => return Err(format!("06 {data:02X?}: mode or filter not on p. 19-9")),
        }
        Ok(vec![OK])
    }

    /// 17 "Send CW messages" (pp. 19-4, 19-13).
    fn send_cw(&mut self, now: Duration, data: &[u8]) -> Result<Vec<u8>, String> {
        if data == [0xFF] {
            self.stop(now);
            return Ok(vec![OK]);
        }
        if data.is_empty() || data.len() > 30 {
            return Err(format!(
                "17: {} characters, not 1-30 (p. 19-13)",
                data.len()
            ));
        }
        if let Some(b) = data.iter().find(|&&b| !cw_byte_allowed(b)) {
            return Err(format!(
                "17: character {b:02X}h is not in the list on p. 19-13"
            ));
        }
        let text = String::from_utf8_lossy(data).to_ascii_uppercase();
        let raw = [&[0x17][..], data].concat();
        // "In the CW mode, if the [TRANSMIT] or an external TX switch is ON, or the
        // Break-in function is ON, a message will be transmitted" (*2, p. 19-8).
        let cw_mode = matches!(self.mode, 0x03 | 0x07);
        let tx_on = self.forced.iter().any(|f| f.1 == OPEN);
        let in_range = self.in_tx_range();
        let on_air = cw_mode && (self.break_in != 0x00 || tx_on) && in_range;
        if !in_range {
            let why = format!(
                "17 sent on {} Hz, outside the transmit ranges (p. 16-2): not transmitted",
                self.frequency_hz
            );
            self.violation(now, &raw, why);
        } else if !on_air {
            self.violation(
                now,
                &raw,
                "17 sent but not transmitted: needs CW mode and break-in or TX ON (*2, p. 19-8)",
            );
        }
        // Not described by the manual: refuse to guess, and flag it.
        let mut start = now + self.cfg.tx_on_delay;
        if let Some(busy_until) = self.pieces.iter().rfind(|p| p.busy(now)).map(Piece::last) {
            self.violation(now, &raw, "17 sent while the keyer is still sending");
            start = start.max(busy_until + self.dot() * 7);
        }
        if self
            .tune
            .as_ref()
            .is_some_and(|t| t.start <= now && now < t.end)
        {
            self.violation(now, &raw, "17 sent while the tuner is tuning");
        }
        let stuck = if on_air { self.arm_stick() } else { None };
        let (marks, chars) = schedule(&text, start, self.dot());
        self.pieces.push(Piece {
            text,
            accepted: now,
            marks: if on_air { marks } else { Vec::new() },
            chars: if on_air { chars } else { Vec::new() },
            hang: self.hang(),
            stop: None,
            on_air,
            stuck,
        });
        Ok(vec![OK])
    }

    /// The stuck-transmit fault for the message being accepted, if it is due.
    fn arm_stick(&mut self) -> Option<Stuck> {
        let i = self
            .faults
            .iter()
            .position(|f| matches!(f, Fault::StickInTx { .. }))?;
        let Fault::StickInTx {
            skip,
            carrier,
            recoverable,
        } = &mut self.faults[i]
        else {
            unreachable!()
        };
        if *skip > 0 {
            *skip -= 1;
            return None;
        }
        let stuck = Stuck {
            carrier: *carrier,
            recoverable: *recoverable,
            cleared: None,
        };
        self.faults.remove(i);
        Some(stuck)
    }

    /// `17 FF` or `1C 00 00`: the keyer stops, and a recoverable stuck transmit ends.
    fn stop(&mut self, now: Duration) {
        for p in &mut self.pieces {
            if p.stop.is_none() && now < p.last() {
                p.stop = Some(now);
            }
            if let Some(st) = &mut p.stuck {
                if st.recoverable && st.cleared.is_none() {
                    st.cleared = Some(now);
                }
            }
        }
    }

    fn report(&self) -> Report {
        let now = self.now();
        let tx = merge(self.intervals(false), now);
        let key = merge(self.intervals(true), now);
        let still_tx = self.at(false, now);
        let transmissions = tx
            .iter()
            .enumerate()
            .map(|(i, &(s, e))| TxPeriod {
                start: s,
                end: (!(still_tx && i + 1 == tx.len())).then_some(e),
                key_down: overlap(&key, s, e),
            })
            .collect();
        let keyed = self.pieces.iter().map(|p| self.keyed(p, now)).collect();
        let len = |v: &[(Duration, Duration)]| v.iter().map(|i| i.1 - i.0).collect::<Vec<_>>();
        Report {
            now,
            keyed,
            transmissions,
            total_tx: len(&tx).iter().sum(),
            total_key_down: len(&key).iter().sum(),
            max_tx: len(&tx).into_iter().max().unwrap_or_default(),
            max_key_down: len(&key).into_iter().max().unwrap_or_default(),
            violations: self.violations.clone(),
            tunes: self.tunes,
            transmitting: still_tx,
            keyer_busy: self.keyer_busy(now),
        }
    }
}

/// A level for 14 0A/0C/0F: exactly two BCD bytes, "00 00 to 02 55" (p. 19-3).
fn level(data: &[u8]) -> Result<u16, String> {
    match from_bcd_be(data) {
        Some(v) if data.len() == 2 && v <= 255 => Ok(v as u16),
        _ => Err(format!(
            "level {data:02X?} is not 00 00 to 02 55 in BCD (p. 19-3)"
        )),
    }
}

impl MockRadio {
    pub fn new(cfg: MockConfig) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                cfg,
                epoch: Instant::now(),
                input: Vec::new(),
                output: VecDeque::new(),
                in_flight: Vec::new(),
                // As if left in USB on 20 m at full power, keyer 20 wpm, break-in
                // off: the node has to set everything it relies on.
                frequency_hz: 14_200_000,
                split_tx_hz: None,
                delta_tx: false,
                mode: 0x01,
                filter: 0x01,
                rf_power: 255,
                key_speed: 85,
                break_in: 0x00,
                break_in_delay: 128,
                tuner: 0x01,
                tune: None,
                tunes: 0,
                pieces: Vec::new(),
                forced: Vec::new(),
                faults: Vec::new(),
                violations: Vec::new(),
                commands: Vec::new(),
            }),
            arrived: Condvar::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The serial port to give the driver.
    pub fn port(&self) -> MockPort {
        MockPort(self.clone())
    }

    /// Radio time since the mock was created.
    pub fn now(&self) -> Duration {
        self.lock().now()
    }

    /// When radio time zero was, in real time.
    pub fn epoch(&self) -> Instant {
        self.lock().epoch
    }

    pub fn time_scale(&self) -> f32 {
        self.lock().cfg.time_scale
    }

    pub fn inject(&self, fault: Fault) {
        self.lock().faults.push(fault);
    }

    /// Change the radio's behaviour, e.g. the SWR of its load.
    pub fn configure(&self, f: impl FnOnce(&mut MockConfig)) {
        f(&mut self.lock().cfg);
    }

    /// Someone at the radio tunes it to `hz`. With CI-V Transceive on, the radio
    /// sends "00 Send frequency data (transceive)" (p. 19-3) to 00h (p. 12-10), with
    /// the frequency as on p. 19-9.
    pub fn turn_dial(&self, hz: u64) {
        let mut s = self.lock();
        s.frequency_hz = hz;
        let body = [&[0x00][..], &bcd_le(hz, 5)].concat();
        s.transceive(&body);
        drop(s);
        self.0.arrived.notify_all();
    }

    /// Someone at the radio switches split on, transmitting on `tx_hz` (the other
    /// VFO), or off with `None`. The IC-7300 sends no transceive frame for it.
    pub fn set_split(&self, tx_hz: Option<u64>) {
        self.lock().split_tx_hz = tx_hz;
    }

    /// Someone at the radio switches ∂TX on or off.
    pub fn set_delta_tx(&self, on: bool) {
        self.lock().delta_tx = on;
    }

    /// Someone at the radio selects a mode and filter: "01 Send mode data
    /// (transceive)" (p. 19-3), with the mode and filter as on p. 19-9.
    pub fn select_mode(&self, mode: u8, filter: u8) {
        let mut s = self.lock();
        (s.mode, s.filter) = (mode, filter);
        s.transceive(&[0x01, mode, filter]);
        drop(s);
        self.0.arrived.notify_all();
    }

    /// End any stuck transmit, recoverable or not, as a hardware PTT timer or a
    /// power cycle would.
    pub fn clear_stuck(&self) {
        let mut s = self.lock();
        let now = s.now();
        for p in &mut s.pieces {
            if let Some(st) = &mut p.stuck {
                st.cleared.get_or_insert(now);
            }
        }
        s.faults.retain(|f| !matches!(f, Fault::StickInTx { .. }));
    }

    pub fn settings(&self) -> Settings {
        let s = self.lock();
        Settings {
            frequency_hz: s.frequency_hz,
            mode: s.mode,
            filter: s.filter,
            rf_power_level: s.rf_power,
            key_speed_level: s.key_speed,
            break_in: s.break_in,
            break_in_delay_level: s.break_in_delay,
            tuner: s.tuner,
            echo: s.cfg.echo,
        }
    }

    /// Transmitting, keying, or about to key: someone listening on the frequency
    /// would not start sending now.
    pub fn busy(&self) -> bool {
        let s = self.lock();
        let now = s.now();
        s.at(false, now) || s.keyer_busy(now)
    }

    /// On transmit now.
    pub fn transmitting(&self) -> bool {
        let s = self.lock();
        s.at(false, s.now())
    }

    /// Something to hear on the frequency now: the keyer is sending (between its
    /// elements too) or there is a carrier. A radio stuck on transmit with the key
    /// up is silent.
    pub fn audible(&self) -> bool {
        let s = self.lock();
        let now = s.now();
        s.keyer_busy(now) || s.at(true, now)
    }

    /// Tuner cycles started.
    pub fn tunes(&self) -> u32 {
        self.lock().tunes
    }

    /// The keyer messages from the `from`-th on, as [`Report::keyed`] lists them.
    pub fn keyed_from(&self, from: usize) -> Vec<Keyed> {
        let s = self.lock();
        let now = s.now();
        s.pieces
            .iter()
            .skip(from)
            .map(|p| s.keyed(p, now))
            .collect()
    }

    /// Key-down runs (radio time) that overlap `from..to`, clipped to it.
    pub fn key_down_between(&self, from: Duration, to: Duration) -> Vec<(Duration, Duration)> {
        self.between(true, from, to)
    }

    /// Transmit periods (radio time) that overlap `from..to`, clipped to it: while
    /// on transmit the radio does not receive.
    pub fn transmit_between(&self, from: Duration, to: Duration) -> Vec<(Duration, Duration)> {
        self.between(false, from, to)
    }

    fn between(&self, carrier: bool, from: Duration, to: Duration) -> Vec<(Duration, Duration)> {
        let s = self.lock();
        merge(s.intervals(carrier), s.now().min(to))
            .into_iter()
            .filter(|&(a, b)| b > from && a < to)
            .map(|(a, b)| (a.max(from), b.min(to)))
            .collect()
    }

    /// Every command body received, with its radio time.
    pub fn commands(&self) -> Vec<(Duration, Vec<u8>)> {
        self.lock().commands.clone()
    }

    pub fn report(&self) -> Report {
        self.lock().report()
    }
}

impl Port for MockPort {
    fn discard_input(&mut self) -> std::io::Result<()> {
        let mut s = self.0.lock();
        s.deliver();
        s.output.clear();
        Ok(())
    }
}

impl Read for MockPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let shared = &self.0 .0;
        let mut s = self.0.lock();
        s.deliver();
        if s.output.is_empty() {
            let wait = match s.in_flight.iter().map(|f| f.0).min() {
                Some(t) => s
                    .cfg
                    .read_timeout
                    .min(t.saturating_duration_since(Instant::now())),
                None => s.cfg.read_timeout,
            };
            s = shared
                .arrived
                .wait_timeout(s, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            s.deliver();
        }
        if s.output.is_empty() {
            return Err(ErrorKind::TimedOut.into());
        }
        let n = buf.len().min(s.output.len());
        for (b, o) in buf.iter_mut().zip(s.output.drain(..n)) {
            *b = o;
        }
        Ok(n)
    }
}

impl Write for MockPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().receive(buf);
        self.0 .0.arrived.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{take_frame, Frame};
    use crate::ic7300::Ic7300;
    use crate::{Rig, RigError};
    use std::thread;

    fn radio(scale: f32) -> (MockRadio, Ic7300<MockPort>) {
        let m = MockRadio::new(MockConfig {
            time_scale: scale,
            ..MockConfig::default()
        });
        let r = Ic7300::with_port(m.port(), 0x94);
        (m, r)
    }

    fn setup(r: &mut Ic7300<MockPort>) {
        r.set_transmit(false).unwrap();
        r.set_frequency(7_030_000).unwrap();
        r.set_mode_cw().unwrap();
        r.set_rf_power_watts(40).unwrap();
        r.set_key_speed(20).unwrap();
        r.set_break_in_delay(10.0).unwrap();
        r.set_break_in(true).unwrap();
    }

    /// Send raw bytes and collect the frames that come back.
    fn raw(m: &MockRadio, bytes: &[u8]) -> Vec<Frame> {
        let mut p = m.port();
        p.write_all(bytes).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64];
        while let Ok(n) = p.read(&mut chunk) {
            buf.extend_from_slice(&chunk[..n]);
        }
        std::iter::from_fn(|| take_frame(&mut buf)).collect()
    }

    fn wait_idle(m: &MockRadio) {
        let t0 = Instant::now();
        while m.busy() {
            assert!(t0.elapsed() < Duration::from_secs(10), "radio never idle");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn the_driver_sets_up_the_radio_without_violations() {
        let (m, mut r) = radio(1.0);
        setup(&mut r);
        assert_eq!(r.frequency().unwrap(), 7_030_000);
        let s = m.settings();
        assert_eq!((s.mode, s.filter), (0x03, 0x01));
        assert_eq!(s.rf_power_level, 102);
        assert_eq!(s.key_speed_level, 85);
        assert_eq!((s.break_in, s.break_in_delay_level), (0x01, 185));
        // The driver asked for 20 wpm: level 85 is 20 wpm on the 6-48 scale.
        assert!((r.dot_duration().unwrap().as_secs_f32() - 0.06).abs() < 1e-4);
        assert!(!r.is_transmitting().unwrap());
        assert!(
            m.report().violations.is_empty(),
            "{:?}",
            m.report().violations
        );
    }

    #[test]
    fn echo_is_sent_back_ahead_of_the_reply() {
        let m = MockRadio::new(MockConfig::default());
        let f = raw(&m, &[0xFE, 0xFE, 0x94, 0xE0, 0x03, 0xFD]);
        assert_eq!(f.len(), 2);
        assert_eq!(
            (f[0].to, f[0].from, f[0].body.as_slice()),
            (0x94, 0xE0, &[0x03][..])
        );
        assert_eq!((f[1].to, f[1].from), (0xE0, 0x94));
        assert_eq!(f[1].body, [0x03, 0x00, 0x00, 0x20, 0x14, 0x00]);
        // Echo back OFF (1A 05 00 75 01): only the reply.
        let f = raw(
            &m,
            &[0xFE, 0xFE, 0x94, 0xE0, 0x1A, 0x05, 0x00, 0x75, 0x01, 0xFD],
        );
        assert_eq!(
            f.len(),
            2,
            "this frame's own echo is sent before the change"
        );
        assert!(f[1].is_ok());
        let f = raw(&m, &[0xFE, 0xFE, 0x94, 0xE0, 0x1A, 0x05, 0x00, 0x75, 0xFD]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].body, [0x1A, 0x05, 0x00, 0x75, 0x01]);
        assert!(!m.settings().echo);
        assert!(m.report().violations.is_empty());
        // The driver works either way.
        let mut r = Ic7300::with_port(m.port(), 0x94);
        setup(&mut r);
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn malformed_and_unknown_commands_get_ng_and_are_recorded() {
        let m = MockRadio::new(MockConfig::default());
        for (bytes, what) in [
            (
                &[0x05, 0x00, 0x00, 0x03, 0x07][..],
                "four bytes of frequency",
            ),
            (&[0x05, 0x00, 0x00, 0x03, 0x07, 0x01], "100 MHz digit not 0"),
            (&[0x05, 0x00, 0x0A, 0x03, 0x07, 0x00], "not BCD"),
            (&[0x05, 0x00, 0x00, 0x00, 0x90, 0x00], "90 MHz"),
            (&[0x06, 0x06], "mode 06"),
            (&[0x06, 0x03, 0x04], "filter 04"),
            (&[0x14, 0x0C, 0x02, 0x56], "level 256"),
            (&[0x14, 0x0C, 0x01], "one-byte level"),
            (&[0x15, 0x12, 0x00, 0x00], "meters are read only"),
            (&[0x16, 0x47, 0x03], "BK-IN 03"),
            (&[0x17], "empty CW message"),
            (&[0x17, b'H', b'I', b'#'], "# is not on p. 19-13"),
            (&[0x1C, 0x00, 0x02], "status 02"),
            (&[0x1C, 0x01, 0x03], "tuner 03"),
            (&[0x03, 0x00], "03 with data"),
            (&[0x99], "unknown command"),
        ] {
            let mut frame = vec![0xFE, 0xFE, 0x94, 0xE0];
            frame.extend_from_slice(bytes);
            frame.push(0xFD);
            let before = m.report().violations.len();
            let f = raw(&m, &frame);
            assert!(f.last().unwrap().is_ng(), "{what}: {f:?}");
            assert_eq!(m.report().violations.len(), before + 1, "{what}");
        }
        let mut long = vec![0xFE, 0xFE, 0x94, 0xE0, 0x17];
        long.extend(std::iter::repeat_n(b'E', 31));
        long.push(0xFD);
        assert!(raw(&m, &long).last().unwrap().is_ng());
        // Stray bytes, and a frame for another radio, get no answer at all.
        let before = m.report().violations.len();
        assert!(raw(&m, &[0x12, 0x34]).is_empty());
        assert_eq!(
            raw(&m, &[0xFE, 0xFE, 0x88, 0xE0, 0x03, 0xFD]).len(),
            1,
            "echo"
        );
        assert_eq!(m.report().violations.len(), before + 2);
        assert_eq!(m.settings().frequency_hz, 14_200_000, "nothing was changed");
    }

    #[test]
    fn keyed_text_and_timing_follow_the_key_speed() {
        // 100x: 20 wpm is a 0.6 ms dot in real time.
        let (m, mut r) = radio(100.0);
        setup(&mut r);
        r.send_cw("PARIS PARIS").unwrap();
        assert!(m.busy());
        wait_idle(&m);
        let rep = m.report();
        let k = &rep.keyed[0];
        assert_eq!(
            (k.sent.as_str(), k.complete, k.on_air),
            ("PARIS PARIS", true, true)
        );
        // 93 units of 60 ms, plus the switch-on delay.
        let len = k.end - k.start;
        assert!((len.as_secs_f32() - 93.0 * 0.06).abs() < 1e-3, "{len:?}");
        assert_eq!(k.start - k.accepted, Duration::from_millis(10));
        // One transmit period: keying plus the break-in delay, which the driver set
        // to level 185, 9.98 dots.
        assert_eq!(rep.transmissions.len(), 1);
        let t = &rep.transmissions[0];
        let tx = t.end.unwrap() - t.start;
        let hang = 2.0 + 185.0 * 11.0 / 255.0;
        assert!(
            (tx.as_secs_f32() - (93.0 + hang) * 0.06).abs() < 1e-3,
            "{tx:?}"
        );
        // Key-down: the marks of PARIS twice, 2 x 22 units; the longest is a dash.
        assert!((rep.total_key_down.as_secs_f32() - 44.0 * 0.06).abs() < 1e-3);
        assert!((rep.max_key_down.as_secs_f32() - 0.18).abs() < 1e-3);
        assert!(!rep.transmitting && !rep.keyer_busy);
        assert!(rep.violations.is_empty());
    }

    #[test]
    fn meters_read_output_only_with_the_key_down() {
        let (m, mut r) = radio(20.0);
        setup(&mut r);
        m.configure(|c| c.swr = 2.0);
        assert_eq!(r.read_po().unwrap(), 0.0);
        assert_eq!(r.read_swr().unwrap(), 1.0);
        r.send_cw("TTTTT").unwrap();
        let (mut po, mut swr) = (0.0f32, 0.0f32);
        while m.busy() {
            po = po.max(r.read_po().unwrap());
            swr = swr.max(r.read_swr().unwrap());
        }
        // 40 W is 40% of full output; the Po scale is not linear in level.
        assert!((po - 40.0).abs() < 0.5, "{po}");
        assert!((swr - 2.0).abs() < 1e-3, "{swr}");

        // Fold-back into a bad load: no output, nothing for the SWR meter.
        m.configure(|c| {
            c.swr = 4.0;
            c.foldback = Some(Foldback {
                above_swr: 3.0,
                fraction: 0.0,
            });
        });
        r.send_cw("TTTTT").unwrap();
        while m.busy() {
            assert_eq!(r.read_po().unwrap(), 0.0);
            assert_eq!(r.read_swr().unwrap(), 1.0);
        }
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn cw_needs_cw_mode_and_break_in() {
        let (m, mut r) = radio(100.0);
        r.set_mode_cw().unwrap();
        // Break-in still off: accepted, but nothing goes out (*2, p. 19-8).
        r.send_cw("TEST").unwrap();
        let rep = m.report();
        assert!(!rep.keyed[0].on_air && rep.transmissions.is_empty());
        assert_eq!(rep.violations.len(), 1, "{:?}", rep.violations);
    }

    #[test]
    fn stop_cuts_the_message_and_the_break_in_delay_follows() {
        let (m, mut r) = radio(50.0);
        setup(&mut r);
        r.send_cw("EEEEEEEEEEEEEEEEEEEEEEEEEEEEEE").unwrap();
        thread::sleep(Duration::from_millis(20));
        r.stop_cw().unwrap();
        // Still on transmit for the break-in delay after the key went up.
        assert!(r.is_transmitting().unwrap());
        wait_idle(&m);
        let k = &m.report().keyed[0];
        assert!(!k.complete);
        assert!(!k.sent.is_empty() && k.sent.len() < 30, "{}", k.sent);
        assert!(k.sent.chars().all(|c| c == 'E'));
    }

    #[test]
    fn stuck_transmit_ends_on_command_or_only_by_hand() {
        let (m, mut r) = radio(100.0);
        setup(&mut r);
        m.inject(Fault::StickInTx {
            skip: 1,
            carrier: true,
            recoverable: true,
        });
        r.send_cw("E").unwrap();
        wait_idle(&m);
        r.send_cw("E").unwrap();
        thread::sleep(Duration::from_millis(30));
        assert!(r.is_transmitting().unwrap());
        assert!(r.read_po().unwrap() > 30.0, "key stuck down");
        r.set_transmit(false).unwrap();
        wait_idle(&m);
        let rep = m.report();
        assert!(rep.max_key_down > Duration::from_secs(2), "{rep:?}");

        m.inject(Fault::StickInTx {
            skip: 0,
            carrier: false,
            recoverable: false,
        });
        r.send_cw("E").unwrap();
        thread::sleep(Duration::from_millis(30));
        r.stop_cw().unwrap();
        r.set_transmit(false).unwrap();
        thread::sleep(Duration::from_millis(30));
        assert!(r.is_transmitting().unwrap(), "jammed");
        assert_eq!(r.read_po().unwrap(), 0.0, "on transmit, key up");
        m.clear_stuck();
        assert!(!r.is_transmitting().unwrap());
        let rep = m.report();
        assert_eq!(rep.transmissions.len(), 3);
        assert!(rep.transmissions.iter().all(|t| t.end.is_some()));
    }

    #[test]
    fn tuner_reports_tuning_then_done() {
        let (m, mut r) = radio(100.0);
        setup(&mut r);
        r.start_tune().unwrap();
        assert!(r.tuner_busy().unwrap());
        assert!(r.is_transmitting().unwrap());
        assert!(r.read_po().unwrap() > 0.0, "tuning carrier");
        let t0 = Instant::now();
        while r.tuner_busy().unwrap() {
            thread::sleep(Duration::from_millis(1));
        }
        // 2.5 s of radio time at 100x.
        let took = t0.elapsed();
        assert!(took > Duration::from_millis(15) && took < Duration::from_millis(500));
        assert_eq!(m.settings().tuner, 0x01);
        assert_eq!(m.report().tunes, 1);

        m.inject(Fault::TuneNeverFinishes);
        r.start_tune().unwrap();
        thread::sleep(Duration::from_millis(40));
        assert!(r.tuner_busy().unwrap());
        assert!(!r.is_transmitting().unwrap(), "the carrier still ends");
    }

    #[test]
    fn a_matched_load_reads_below_1_5_after_tuning() {
        let (m, mut r) = radio(100.0);
        setup(&mut r);
        let max_swr = |m: &MockRadio, r: &mut Ic7300<MockPort>| {
            r.send_cw("TTTTT").unwrap();
            let mut swr = 0.0f32;
            while m.busy() {
                swr = swr.max(r.read_swr().unwrap());
            }
            swr
        };
        let tune = |r: &mut Ic7300<MockPort>| {
            r.start_tune().unwrap();
            while r.tuner_busy().unwrap() {
                thread::sleep(Duration::from_millis(1));
            }
        };
        // 2.5:1 is within the tuner's range: "less than 1.5:1" once tuned (p. 11-2).
        m.configure(|c| c.swr = 2.5);
        assert!((max_swr(&m, &mut r) - 2.5).abs() < 0.02, "not tuned yet");
        tune(&mut r);
        assert_eq!(m.settings().tuner, 0x01);
        let swr = max_swr(&m, &mut r);
        assert!(swr < 1.5 && (swr - 1.3).abs() < 0.02, "{swr}");
        // The load changes: the old match no longer holds.
        m.configure(|c| c.swr = 2.8);
        assert!((max_swr(&m, &mut r) - 2.8).abs() < 0.02);
        // 3.5:1 is beyond it: bypassed, the load's own SWR.
        m.configure(|c| c.swr = 3.5);
        tune(&mut r);
        assert_eq!(m.settings().tuner, 0x00);
        assert!((max_swr(&m, &mut r) - 3.5).abs() < 0.02);
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn nothing_goes_out_outside_the_transmit_ranges() {
        let (m, mut r) = radio(100.0);
        setup(&mut r);
        // 6.5 MHz: received (0.03-74.8 MHz) but not an amateur band (p. 16-2).
        r.set_frequency(6_500_000).unwrap();
        r.send_cw("TEST").unwrap();
        r.start_tune().unwrap();
        let rep = m.report();
        assert!(!rep.keyed[0].on_air && rep.transmissions.is_empty());
        assert_eq!(rep.tunes, 0);
        assert_eq!(rep.violations.len(), 2, "{:?}", rep.violations);
        // The edges of a band are inside it.
        r.set_frequency(14_350_000).unwrap();
        r.send_cw("E").unwrap();
        assert!(m.report().keyed[1].on_air);
    }

    #[test]
    fn mode_without_a_filter_gets_the_modes_default() {
        let m = MockRadio::new(MockConfig::default());
        let f = raw(&m, &[0xFE, 0xFE, 0x94, 0xE0, 0x06, 0x03, 0xFD]);
        assert!(f.last().unwrap().is_ok());
        assert_eq!((m.settings().mode, m.settings().filter), (0x03, 0x02));
        let f = raw(&m, &[0xFE, 0xFE, 0x94, 0xE0, 0x04, 0xFD]);
        assert_eq!(f.last().unwrap().body, [0x04, 0x03, 0x02]);
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn changes_at_the_radio_are_sent_unasked() {
        let m = MockRadio::new(MockConfig::default());
        let mut p = m.port();
        m.turn_dial(7_031_250);
        m.select_mode(0x03, 0x02);
        let mut buf = vec![0u8; 64];
        let n = p.read(&mut buf).unwrap();
        // FE FE 00 94 00 <frequency> FD, then FE FE 00 94 01 <mode> <filter> FD.
        assert_eq!(
            buf[..n],
            [
                0xFE, 0xFE, 0x00, 0x94, 0x00, 0x50, 0x12, 0x03, 0x07, 0x00, 0xFD, 0xFE, 0xFE, 0x00,
                0x94, 0x01, 0x03, 0x02, 0xFD
            ]
        );
        m.configure(|c| c.transceive = false);
        m.turn_dial(7_030_000);
        assert!(p.read(&mut buf).is_err(), "Transceive OFF: nothing");
        assert_eq!(m.settings().frequency_hz, 7_030_000);
    }

    #[test]
    fn split_and_delta_tx_read_as_set_at_the_radio() {
        let (m, mut r) = radio(1.0);
        setup(&mut r);
        assert!(!r.split_or_delta_tx().unwrap());
        assert_eq!(Rig::transmit_frequency(&mut r).unwrap(), 7_030_000);
        m.set_split(Some(7_040_000));
        assert!(r.split_or_delta_tx().unwrap());
        assert_eq!(Rig::transmit_frequency(&mut r).unwrap(), 7_040_000);
        m.set_split(None);
        m.set_delta_tx(true);
        assert!(r.split_or_delta_tx().unwrap());
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn the_driver_skips_transceive_frames() {
        let (m, mut r) = radio(1.0);
        setup(&mut r);
        // Someone at the radio keeps nudging the dial while the driver works.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let knob = {
            let (m, stop) = (m.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    m.turn_dial(7_030_010);
                    m.turn_dial(7_030_000);
                    thread::sleep(Duration::from_micros(200));
                }
            })
        };
        // The dial really is at 7.030010 MHz for a moment each time round, and the
        // radio answers with wherever the dial is when the command arrives.
        let on_dial = |hz: u64| hz == 7_030_000 || hz == 7_030_010;
        for _ in 0..50 {
            let hz = r.frequency().unwrap();
            assert!(on_dial(hz), "{hz}");
            assert!(!r.is_transmitting().unwrap());
            r.set_break_in(true).unwrap();
        }
        // A lost reply: the driver reads until quiet before the next command, which
        // the transceive frames keep from happening; it still gets the right reply.
        m.inject(Fault::Reply {
            cmd: vec![0x03],
            skip: 0,
            times: 1,
            kind: ReplyFault::Drop,
        });
        assert!(matches!(r.frequency(), Err(RigError::Timeout)));
        assert!(!r.is_transmitting().unwrap());
        let hz = r.frequency().unwrap();
        assert!(on_dial(hz), "{hz}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        knob.join().unwrap();
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn reply_faults_reach_the_driver() {
        let (m, mut r) = radio(1.0);
        setup(&mut r);
        m.inject(Fault::Reply {
            cmd: vec![0x1C, 0x00],
            skip: 1,
            times: 1,
            kind: ReplyFault::Ng,
        });
        assert!(!r.is_transmitting().unwrap());
        assert!(matches!(r.is_transmitting(), Err(RigError::Rejected)));
        assert!(!r.is_transmitting().unwrap());

        m.inject(Fault::Reply {
            cmd: vec![0x03],
            skip: 0,
            times: 1,
            kind: ReplyFault::Drop,
        });
        assert!(matches!(r.frequency(), Err(RigError::Timeout)));
        assert_eq!(r.frequency().unwrap(), 7_030_000);

        // A late OK to the stop command must not be taken for the next command's.
        m.inject(Fault::Reply {
            cmd: vec![0x17, 0xFF],
            skip: 0,
            times: 1,
            kind: ReplyFault::Delay(Duration::from_millis(700)),
        });
        assert!(matches!(r.stop_cw(), Err(RigError::Timeout)));
        m.inject(Fault::Reply {
            cmd: vec![0x16, 0x47],
            skip: 0,
            times: 1,
            kind: ReplyFault::Ng,
        });
        assert!(matches!(r.set_break_in(true), Err(RigError::Rejected)));
        assert!(m.report().violations.is_empty());
    }

    #[test]
    fn meter_levels_follow_icoms_points() {
        assert_eq!(meter_level(&SWR_POINTS, 1.0), 0);
        assert_eq!(meter_level(&SWR_POINTS, 1.5), 48);
        assert_eq!(meter_level(&SWR_POINTS, 3.0), 120);
        assert_eq!(meter_level(&SWR_POINTS, 3.5), 140);
        assert_eq!(meter_level(&SWR_POINTS, 10.0), 255);
        assert_eq!(meter_level(&PO_POINTS, 50.0), 143);
        assert_eq!(meter_level(&PO_POINTS, 100.0), 213);
        assert_eq!(meter_level(&PO_POINTS, 0.0), 0);
        let (marks, chars) = schedule("A B", Duration::ZERO, Duration::from_millis(1));
        let ms: Vec<_> = marks.iter().map(|m| m.0.as_millis()).collect();
        assert_eq!(ms, [0, 2, 12, 16, 18, 20]);
        assert_eq!(chars.iter().map(|c| c.1).collect::<Vec<_>>(), [1, 3]);
    }
}
