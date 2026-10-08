//! A keyer box and a radio in memory, for tests and the self-tests: the box is
//! [`keyer_core`]'s, the same code the firmware runs, behind a fake serial port;
//! the radio keys from the box's key line and produces headphone audio (band
//! noise, its sidetone, its receiver muted while it transmits), fed to the
//! [`Monitor`] as a sound card would. Or, [`RadioSettings::handheld`], an FM
//! handheld on the box's PTT: it transmits while the PTT is held, its speaker
//! carries receive noise (squelch open) and goes quiet while it transmits, with no
//! sidetone, and its PTT contact holds the box's PTT line low while anything
//! holds it. Both run on one [`Clock`], faster than real time if asked, and either
//! can be given the faults the keyer rig must catch.

use super::link::Transport;
use super::monitor::Monitor;
use crate::audio::{Block, BlockSender};
use keyer_core::keyer::{Boot, Ended, Keyer, Limits, Pin};
use keyer_core::mcw;
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Radio and box time: wall-clock time since `epoch`, times `scale`.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub epoch: Instant,
    pub scale: f32,
}

impl Clock {
    pub fn new(scale: f32) -> Self {
        Self {
            epoch: Instant::now(),
            scale,
        }
    }

    pub fn secs(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * f64::from(self.scale)
    }

    pub fn ms(&self) -> u64 {
        (self.secs() * 1000.0) as u64
    }

    /// The wall-clock instant of radio time `t`.
    pub fn instant(&self, t: f64) -> Instant {
        self.epoch + Duration::from_secs_f64((t / f64::from(self.scale)).max(0.0))
    }
}

/// How long the box's USB takes to come back after a reset (radio time).
const USB_RESET_MS: u64 = 1_000;
/// Output changes kept, per output.
const CHANGES_KEPT: usize = 4096;

/// The box's USB link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Usb {
    Up,
    Unplugged,
    /// Re-enumerating after a reset, until then.
    Back(u64),
}

/// One `CW` or `MCW` command the box took.
#[derive(Debug, Clone)]
pub struct Run {
    pub text: String,
    pub wpm: u32,
    /// `MCW`: the PTT held for the run, the text keyed on the tone after the box's
    /// lead.
    pub mcw: bool,
    /// When it was taken and when it ended (box ms): its last key-up (`MCW`: its
    /// PTT up), or when it was stopped.
    pub start: u64,
    pub end: Option<u64>,
    pub ended: Ended,
}

impl Run {
    /// When its Morse starts (box ms).
    pub fn morse_start(&self) -> u64 {
        self.start + if self.mcw { u64::from(mcw::LEAD_MS) } else { 0 }
    }

    /// The characters keyed in full by `t` (box ms).
    pub fn sent_by(&self, t: u64) -> String {
        let elapsed = t.saturating_sub(self.morse_start());
        let words: Vec<&str> = self.text.split(' ').collect();
        let mut done = String::new();
        let mut text = String::new();
        for (i, w) in words.iter().enumerate() {
            for (j, c) in w.char_indices() {
                let upto = format!("{text}{}", &w[..j + c.len_utf8()]);
                match keyer_core::morse::run_ms(upto.as_bytes(), self.wpm) {
                    Ok(ms) if u64::from(ms) <= elapsed => done = upto,
                    _ => return done,
                }
            }
            text.push_str(w);
            if i + 1 < words.len() {
                text.push(' ');
            }
        }
        done
    }
}

/// A handheld's PTT contact as the box's PTT line (its sense input) reads it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PttLine {
    /// Wired to a handheld's PTT contact. A box on a key jack has nothing there,
    /// and the input reads high on its pull-up.
    pub wired: bool,
    /// The radio is off: its contact is not pulled up, and the line reads low.
    pub off: bool,
    /// The cable is out of the radio: the input reads high on its pull-up.
    pub cable_out: bool,
    /// The contact held low at the radio from this box ms on (a shorted
    /// optocoupler or cable, RF), until `held_until` if set.
    pub held_from: Option<u64>,
    pub held_until: Option<u64>,
}

impl PttLine {
    /// The line as `radio` holds it.
    pub fn of(radio: &RadioSettings) -> Self {
        let ms = |t: f64| (t.max(0.0) * 1000.0).round() as u64;
        Self {
            wired: radio.fm && !radio.sense_open,
            off: radio.off,
            cable_out: radio.cable_out,
            held_from: radio.stuck_from.map(ms),
            held_until: radio.stuck_until.map(ms),
        }
    }

    /// What the box reads at `t` (box ms), its PTT down or not: high while
    /// nothing holds the contact.
    pub fn reads(&self, t: u64, ptt: bool) -> bool {
        if !self.wired || self.cable_out {
            return true;
        }
        let held = self.held_from.is_some_and(|h| t >= h) && self.held_until.is_none_or(|u| t < u);
        !(self.off || ptt || held)
    }
}

/// The box's output changes, (box ms, on), per output.
#[derive(Debug, Default)]
struct Changes {
    key: VecDeque<(u64, bool)>,
    ptt: VecDeque<(u64, bool)>,
    tone: VecDeque<(u64, bool)>,
}

impl Changes {
    fn push(&mut self, at: u64, pin: Pin, on: bool) {
        let q = match pin {
            Pin::Key => &mut self.key,
            Pin::Ptt => &mut self.ptt,
            Pin::Tone => &mut self.tone,
        };
        q.push_back((at, on));
        while q.len() > CHANGES_KEPT {
            q.pop_front();
        }
    }

    /// Every output of `k` that is on, off at `at`: the box lost power or reset.
    fn all_off(&mut self, k: &Keyer, at: u64) {
        for (pin, on) in [
            (Pin::Key, k.key_down()),
            (Pin::Tone, k.tone()),
            (Pin::Ptt, k.ptt()),
        ] {
            if on {
                self.push(at, pin, false);
            }
        }
    }
}

/// The stretches an output was on that overlap `from..to` (box ms); one still on
/// ends at `to`.
fn stretches(q: &VecDeque<(u64, bool)>, from: u64, to: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut since: Option<u64> = None;
    for &(at, on) in q {
        match (on, since) {
            (true, None) => since = Some(at),
            (false, Some(s)) => {
                if at >= from && s <= to {
                    out.push((s, at));
                }
                since = None;
            }
            _ => {}
        }
    }
    if let Some(s) = since.filter(|&s| s <= to) {
        out.push((s, to.max(s)));
    }
    out
}

/// What the box sees and does.
pub struct BoxState {
    keyer: Keyer,
    /// Box time the keyer has been brought up to.
    at: u64,
    changes: Changes,
    /// The PTT line, as the radio holds it.
    pub line: PttLine,
    usb: Usb,
    replies: VecDeque<String>,
    /// When the control loop hung (`TEST HANG`).
    hung_at: Option<u64>,
    /// Lines the box took, for tests.
    pub lines: Vec<String>,
    /// Every `CW` and `MCW` command it took, in order.
    pub runs: Vec<Run>,
    /// Times it restarted.
    pub resets: u32,
    /// A fault: the box's link timeout never comes, as if it kept hearing the
    /// node. For `hfnode keyer linktest`.
    pub deaf_to_silence: bool,
    /// A fault: it keys every run at this speed, whatever it was asked, so that a
    /// run lasts longer than the node expects (a box with a wrong clock).
    pub slow_wpm: Option<u32>,
    /// A fault: after its watchdog fires, `STATUS` reports no trip, as firmware
    /// without the safety audit's KB-7 did (`HELLO` still says `WATCHDOG`).
    pub hides_watchdog_trip: bool,
}

impl BoxState {
    fn new(now: u64) -> Self {
        Self {
            keyer: Keyer::new(Limits::BOX, Boot::Power, now),
            at: now,
            changes: Changes::default(),
            line: PttLine::default(),
            usb: Usb::Up,
            replies: VecDeque::new(),
            hung_at: None,
            lines: Vec::new(),
            runs: Vec::new(),
            resets: 0,
            deaf_to_silence: false,
            slow_wpm: None,
            hides_watchdog_trip: false,
        }
    }

    /// Close the open run, if the box is no longer keying it. A hung box's run
    /// stays open until its watchdog resets it.
    fn close_run(&mut self, now: u64) {
        if self.keyer.running() || self.keyer.hung() {
            return;
        }
        let Some(run) = self.runs.last_mut().filter(|r| r.end.is_none()) else {
            return;
        };
        let q = if run.mcw {
            &self.changes.ptt
        } else {
            &self.changes.key
        };
        let last_off = q
            .iter()
            .rev()
            .find(|&&(at, on)| !on && at >= run.start)
            .map(|&(at, _)| at);
        run.end = Some(last_off.unwrap_or(now).min(now));
        run.ended = self.keyer.ended();
    }

    /// The watchdog reset the chip at `at`: its pins go low and USB re-enumerates.
    fn watchdog_reset(&mut self, at: u64) {
        // As the firmware: what the last pass before the hang saved comes back, and
        // a watchdog restart comes up tripped.
        let saved = self.keyer.saved(self.hung_at.unwrap_or(at));
        self.changes.all_off(&self.keyer, at);
        if let Some(run) = self.runs.last_mut().filter(|r| r.end.is_none()) {
            run.end = Some(at);
            run.ended = Ended::None;
        }
        self.keyer = Keyer::restore(Limits::BOX, Boot::Watchdog, at, Some(saved));
        self.at = at;
        self.hung_at = None;
        self.resets += 1;
        self.replies.clear();
        if self.usb != Usb::Unplugged {
            self.usb = Usb::Back(at + USB_RESET_MS);
        }
    }

    /// Bring the box up to `now`: its outputs, its watchdog, its USB. It reads
    /// its PTT line every millisecond, as the firmware does every pass of its loop.
    fn poll(&mut self, now: u64) {
        loop {
            if self.keyer.hung() {
                let hung = *self.hung_at.get_or_insert(self.at);
                let reset = hung + u64::from(keyer_core::limits::WATCHDOG_MS);
                if now < reset {
                    break;
                }
                self.watchdog_reset(reset);
            } else if self.at < now {
                let t = self.at + 1;
                if self.deaf_to_silence && t.is_multiple_of(250) {
                    // Its link never goes quiet: a line of its own every 250 ms,
                    // the reply dropped, so the node sees only the run going on.
                    if let Some(l) = keyer_core::frame::encode(1, format_args!("STATUS")) {
                        let ch = &mut self.changes;
                        let _ = self
                            .keyer
                            .handle_line_with(t, l.as_bytes(), |at, pin, on| ch.push(at, pin, on));
                    }
                }
                self.keyer.set_line(self.line.reads(t, self.keyer.ptt()));
                let ch = &mut self.changes;
                self.keyer.poll_with(t, |at, pin, on| ch.push(at, pin, on));
                self.at = t;
            } else {
                break;
            }
        }
        if let Usb::Back(t) = self.usb {
            if now >= t {
                self.usb = Usb::Up;
            }
        }
        self.close_run(now);
    }

    pub fn key_down(&self) -> bool {
        self.keyer.key_down()
    }

    pub fn ptt(&self) -> bool {
        self.keyer.ptt()
    }

    pub fn tone(&self) -> bool {
        self.keyer.tone()
    }

    pub fn running(&self) -> bool {
        self.keyer.running()
    }

    pub fn ended(&self) -> Ended {
        self.keyer.ended()
    }

    pub fn trip(&self) -> keyer_core::keyer::Trip {
        self.keyer.trip()
    }

    /// Plugged in and enumerated: the node can talk to it.
    pub fn connected(&self) -> bool {
        self.usb == Usb::Up
    }

    /// The key-down stretches that overlap `from..to` (box ms); one still down
    /// ends at `to`.
    pub fn downs(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        stretches(&self.changes.key, from, to)
    }

    /// The PTT-down stretches, as [`BoxState::downs`].
    pub fn ptt_downs(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        stretches(&self.changes.ptt, from, to)
    }

    /// The tone's stretches, as [`BoxState::downs`].
    pub fn tones(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        stretches(&self.changes.tone, from, to)
    }

    /// The stretches the radio was keyed by the box's output for `fm` (the PTT)
    /// or not (the key), as [`BoxState::downs`].
    pub fn keyed(&self, fm: bool, from: u64, to: u64) -> Vec<(u64, u64)> {
        if fm {
            self.ptt_downs(from, to)
        } else {
            self.downs(from, to)
        }
    }
}

/// The keyer box, as a test sees it.
#[derive(Clone)]
pub struct MockBox {
    pub state: Arc<Mutex<BoxState>>,
    pub clock: Clock,
}

impl MockBox {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(BoxState::new(clock.ms()))),
            clock,
        }
    }

    /// The box, brought up to now.
    pub fn now(&self) -> MutexGuard<'_, BoxState> {
        let mut s = lock(&self.state);
        s.poll(self.clock.ms());
        s
    }

    pub fn transport(&self) -> Box<dyn Transport> {
        Box::new(MockTransport { b: self.clone() })
    }

    /// The radio holds the PTT line as `line` says from now on.
    pub fn set_line(&self, line: PttLine) {
        self.now().line = line;
    }

    /// Pull the USB cable (the box loses power and opens its outputs), or plug it
    /// in.
    pub fn unplug(&self, out: bool) {
        let mut guard = self.now();
        let s = &mut *guard;
        let now = s.at;
        if out {
            // Unpowered: the run ends and the pins go low.
            let mut k = std::mem::replace(&mut s.keyer, Keyer::new(Limits::BOX, Boot::Power, now));
            let ch = &mut s.changes;
            k.link_lost(now, |at, pin, on| ch.push(at, pin, on));
            ch.all_off(&k, now);
            if let Some(run) = s.runs.last_mut().filter(|r| r.end.is_none()) {
                run.end = Some(now);
                run.ended = Ended::Usb;
            }
            s.hung_at = None;
            s.usb = Usb::Unplugged;
        } else if s.usb == Usb::Unplugged {
            s.keyer = Keyer::new(Limits::BOX, Boot::Power, now);
            s.usb = Usb::Back(now + USB_RESET_MS);
        }
    }
}

/// Whether a line is `MCW`, and its speed and text (`<id> CW <wpm> <text>*<check>`
/// or `<id> MCW ...`).
fn run_command(line: &str) -> Option<(bool, u32, String)> {
    let body = line.split_once('*').map_or(line, |(b, _)| b);
    let (_, rest) = body.split_once(' ')?;
    let (mcw, rest) = match rest.strip_prefix("MCW ") {
        Some(r) => (true, r),
        None => (false, rest.strip_prefix("CW ")?),
    };
    let (wpm, text) = rest.split_once(' ')?;
    Some((mcw, wpm.parse().ok()?, text.to_string()))
}

struct MockTransport {
    b: MockBox,
}

/// A `CW <wpm> <text>` or `MCW <wpm> <text>` line with its speed changed, keeping
/// its id: a box that keys slower than it was asked.
fn rewrite_wpm(line: &str, wpm: u32) -> String {
    let same = || line.to_string();
    let Ok((id, body)) = keyer_core::frame::decode(line.as_bytes()) else {
        return same();
    };
    let Some((word, rest)) = body.split_once(' ') else {
        return same();
    };
    let Some((_, text)) = rest
        .split_once(' ')
        .filter(|_| word == "CW" || word == "MCW")
    else {
        return same();
    };
    keyer_core::frame::encode(id, format_args!("{word} {wpm} {text}"))
        .map_or_else(same, |l| l.as_str().to_string())
}

/// A `STATUS` reply with a `WATCHDOG` trip reported as none, keeping its id.
fn hide_watchdog_trip(reply: &str) -> String {
    let same = || reply.to_string();
    let Ok((id, body)) = keyer_core::frame::decode(reply.as_bytes()) else {
        return same();
    };
    if !body.starts_with("OK STATUS ") {
        return same();
    }
    let body = body.replace(" WATCHDOG ", " NONE ");
    keyer_core::frame::encode(id, format_args!("{body}"))
        .map_or_else(same, |l| l.as_str().to_string())
}

fn gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "the keyer box is not connected",
    )
}

impl Transport for MockTransport {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut guard = self.b.now();
        let s = &mut *guard;
        if s.usb != Usb::Up {
            return Err(gone());
        }
        let now = s.at;
        s.lines.push(line.to_string());
        // What the box keys: the line as sent, unless its clock is wrong.
        let keyed = match s.slow_wpm {
            Some(wpm) => rewrite_wpm(line, wpm),
            None => line.to_string(),
        };
        let ch = &mut s.changes;
        let reply = s
            .keyer
            .handle_line_with(now, keyed.as_bytes(), |at, pin, on| ch.push(at, pin, on));
        if s.keyer.hung() {
            s.hung_at.get_or_insert(now);
        }
        if let Some(r) = reply {
            let r = r.as_str();
            if r.contains(" OK CW*") || r.contains(" OK MCW*") {
                if let Some((mcw, wpm, text)) = run_command(line) {
                    s.runs.push(Run {
                        text,
                        wpm,
                        mcw,
                        start: now,
                        end: None,
                        ended: Ended::None,
                    });
                }
            }
            let r = if s.hides_watchdog_trip {
                hide_watchdog_trip(r)
            } else {
                r.to_string()
            };
            s.replies.push_back(r);
        }
        Ok(())
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>> {
        loop {
            {
                let mut s = self.b.now();
                if s.usb != Usb::Up {
                    return Err(gone());
                }
                if let Some(r) = s.replies.pop_front() {
                    return Ok(Some(r));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(1)));
        }
    }

    fn clear_input(&mut self) -> io::Result<()> {
        let mut s = self.b.now();
        if s.usb != Usb::Up {
            return Err(gone());
        }
        s.replies.clear();
        Ok(())
    }

    fn describe(&self) -> String {
        "mock keyer box".into()
    }
}

/// The radio: how it is set and what is wrong with it.
#[derive(Debug, Clone)]
pub struct RadioSettings {
    pub sample_rate: u32,
    pub pitch_hz: f32,
    /// An FM handheld on the box's PTT ([`RadioSettings::handheld`]), not a radio
    /// on its key line.
    pub fm: bool,
    /// Sidetone amplitude in the headphone audio; 0 is off.
    pub sidetone: f32,
    /// Band noise (standard deviation) on receive; a handheld's receive noise.
    pub noise: f32,
    /// The sound card's own noise, heard while the receiver is muted or the radio
    /// is off.
    pub floor: f32,
    /// Semi break-in hang after key-up (s), 0 for full break-in; a handheld's
    /// switch back to receive after its PTT is let up.
    pub hang: f64,
    /// The radio's keying delay (s).
    pub delay: f64,
    /// The sound card and capture's delay (s).
    pub latency: f64,
    /// Faults.
    /// The key cable out of the radio; a handheld's cable out of its jack (no
    /// PTT, no speaker audio).
    pub cable_out: bool,
    /// The key (a handheld's PTT) held down at the radio from this radio time on.
    pub stuck_from: Option<f64>,
    /// The key held down at the radio until this radio time, then released: a hold
    /// that clears by itself, which the node must still catch.
    pub stuck_until: Option<f64>,
    /// A handheld's PTT line not wired (its sense wire open): the box's input
    /// reads high whatever the PTT does.
    pub sense_open: bool,
    pub off: bool,
    /// No audio reaches the computer at all.
    pub unplugged: bool,
    /// A carrier at the pitch on receive: (from, to, amplitude), radio time. On a
    /// handheld, a station on the channel: it quiets the receive noise, its tone
    /// (if the amplitude is not 0) at the pitch.
    pub carrier: Option<(f64, f64, f32)>,
}

impl RadioSettings {
    pub fn new(sample_rate: u32, pitch_hz: f32) -> Self {
        Self {
            sample_rate,
            pitch_hz,
            fm: false,
            sidetone: 0.3,
            noise: 0.02,
            floor: 0.0002,
            hang: 0.6,
            delay: 0.005,
            latency: 0.06,
            cable_out: false,
            stuck_from: None,
            stuck_until: None,
            sense_open: false,
            off: false,
            unplugged: false,
            carrier: None,
        }
    }

    /// An FM handheld on the box's PTT, its squelch open: receive noise, no
    /// sidetone, and a quick switch between transmit and receive.
    pub fn handheld(sample_rate: u32, pitch_hz: f32) -> Self {
        Self {
            fm: true,
            sidetone: 0.0,
            noise: 0.05,
            hang: 0.15,
            delay: 0.03,
            ..Self::new(sample_rate, pitch_hz)
        }
    }
}

/// Receive audio from elsewhere (the field station), added to the band noise:
/// samples at radio time `t` (s), `n` of them.
pub type Field = Box<dyn FnMut(f64, usize) -> Vec<f32> + Send>;

/// The radio, its headphone audio running into the monitor (and, if given, the
/// node's decoder) as long as this lives.
pub struct MockRadio {
    pub settings: Arc<Mutex<RadioSettings>>,
    keyer_box: MockBox,
    stop: Arc<AtomicBool>,
    feeder: Option<JoinHandle<()>>,
}

/// Audio block length (radio time).
const BLOCK_S: f64 = 0.05;

impl MockRadio {
    pub fn start(
        settings: RadioSettings,
        keyer_box: MockBox,
        monitor: Arc<Mutex<Monitor>>,
        decoder: Option<BlockSender>,
        mut field: Option<Field>,
    ) -> Self {
        keyer_box.set_line(PttLine::of(&settings));
        let settings = Arc::new(Mutex::new(settings));
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = {
            let (settings, stop, keyer_box) = (settings.clone(), stop.clone(), keyer_box.clone());
            thread::spawn(move || {
                let clock = keyer_box.clock;
                let mut noise = cw::synth::Noise::new(11);
                let mut t = clock.secs();
                let mut phase = 0.0f64;
                while !stop.load(Ordering::Relaxed) {
                    let s = lock(&settings).clone();
                    let sr = f64::from(s.sample_rate);
                    let n = (sr * BLOCK_S).round() as usize;
                    // Wait until this block has been captured and its delay passed.
                    let due = clock.instant(t + BLOCK_S + s.latency);
                    let now = Instant::now();
                    if now < due {
                        thread::sleep((due - now).min(Duration::from_millis(5)));
                        continue;
                    }
                    // The box's key (a handheld's PTT) over this block, its hang and
                    // the radio's delay; and the PTT line as the radio holds it now.
                    let span = (t - s.hang - s.delay - 0.01, t + BLOCK_S);
                    let downs: Vec<(f64, f64)> = {
                        let mut b = keyer_box.now();
                        b.line = PttLine::of(&s);
                        b.keyed(
                            s.fm,
                            (span.0.max(0.0) * 1000.0) as u64,
                            (span.1 * 1000.0).ceil() as u64,
                        )
                    }
                    .into_iter()
                    .map(|(a, b)| (a as f64 / 1000.0, b as f64 / 1000.0))
                    .collect();
                    let mut buf = vec![0.0f32; n];
                    let mut rx = vec![0.0f32; n];
                    noise.add(&mut rx, s.noise);
                    if let Some(f) = field.as_mut() {
                        for (r, v) in rx.iter_mut().zip(f(t, n)) {
                            *r += v;
                        }
                    }
                    let mut floor = vec![0.0f32; n];
                    noise.add(&mut floor, s.floor);
                    let w = 2.0 * std::f64::consts::PI * f64::from(s.pitch_hz);
                    let stuck = |at: f64| {
                        s.stuck_from.is_some_and(|f| at >= f)
                            && s.stuck_until.is_none_or(|u| at < u)
                    };
                    // Whether the radio's key (or PTT) was closed at any time in
                    // `a..=b`.
                    let closed = |a: f64, b: f64| {
                        (!s.cable_out && downs.iter().any(|&(d, u)| d <= b && u > a)) || stuck(b)
                    };
                    // A handheld's cable out: nothing from its speaker either.
                    let silent = s.off || (s.fm && s.cable_out);
                    for (i, out) in buf.iter_mut().enumerate() {
                        let at = t + i as f64 / sr - s.delay;
                        phase = (phase + w / sr) % (2.0 * std::f64::consts::PI);
                        if silent {
                            *out = floor[i];
                            continue;
                        }
                        let down = closed(at, at);
                        let muted = down || (s.hang > 0.0 && closed(at - s.hang, at));
                        let mut v = if muted { floor[i] } else { rx[i] };
                        if !muted {
                            if let Some((from, to, a)) = s.carrier {
                                if at >= from && at < to {
                                    if s.fm {
                                        // FM: the carrier quiets the receive noise.
                                        v = floor[i];
                                    }
                                    v += a * phase.sin() as f32;
                                }
                            }
                        }
                        if down && !s.fm {
                            v += s.sidetone * phase.sin() as f32;
                        }
                        *out = v;
                    }
                    t += n as f64 / sr;
                    if s.unplugged {
                        continue;
                    }
                    let at = Instant::now();
                    lock(&monitor).push(at, &buf);
                    if let Some(d) = &decoder {
                        let _ = d.send(Block { at, samples: buf });
                    }
                }
            })
        };
        Self {
            settings,
            keyer_box,
            stop,
            feeder: Some(feeder),
        }
    }

    /// Change the radio; a handheld's PTT line follows at once.
    pub fn set(&self, f: impl FnOnce(&mut RadioSettings)) {
        let line = {
            let mut s = lock(&self.settings);
            f(&mut s);
            PttLine::of(&s)
        };
        self.keyer_box.set_line(line);
    }
}

impl Drop for MockRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.feeder.take() {
            let _ = h.join();
        }
    }
}
