//! A keyer box and a radio in memory, for tests and the self-tests: the box is
//! [`keyer_core`]'s, the same code the firmware runs, behind a fake serial port;
//! the radio keys from the box's key line and produces headphone audio (band
//! noise, its sidetone, its receiver muted while it transmits), fed to the
//! [`Monitor`] as a sound card would. Both run on one [`Clock`], faster than real
//! time if asked, and either can be given the faults the keyer rig must catch.

use super::link::Transport;
use super::monitor::Monitor;
use crate::audio::{Block, BlockSender};
use keyer_core::keyer::{Boot, Keyer, Limits};
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

/// The box's USB link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Usb {
    Up,
    Unplugged,
    /// Re-enumerating after a reset, until then.
    Back(u64),
}

/// One `CW` command the box took.
#[derive(Debug, Clone)]
pub struct Run {
    pub text: String,
    pub wpm: u32,
    /// When it was taken and when it ended (box ms): its last key-up, or when it
    /// was stopped.
    pub start: u64,
    pub end: Option<u64>,
    pub ended: keyer_core::keyer::Ended,
}

impl Run {
    /// The characters keyed in full by `t` (box ms).
    pub fn sent_by(&self, t: u64) -> String {
        let elapsed = t.saturating_sub(self.start);
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

/// What the box sees and does.
pub struct BoxState {
    keyer: Keyer,
    /// Key changes, (box ms, down).
    keys: VecDeque<(u64, bool)>,
    usb: Usb,
    replies: VecDeque<String>,
    /// When the control loop hung (`TEST HANG`).
    hung_at: Option<u64>,
    /// Lines the box took, for tests.
    pub lines: Vec<String>,
    /// Every `CW` command it took, in order.
    pub runs: Vec<Run>,
    /// Times it restarted.
    pub resets: u32,
    /// A fault: the box's link timeout never comes, as if it kept hearing the
    /// node. For `hfnode keyer linktest`.
    pub deaf_to_silence: bool,
    /// A fault: it keys every run at this speed, whatever it was asked, so that a
    /// run lasts longer than the node expects (a box with a wrong clock).
    pub slow_wpm: Option<u32>,
}

impl BoxState {
    fn new(now: u64) -> Self {
        Self {
            keyer: Keyer::new(Limits::BOX, Boot::Power, now),
            keys: VecDeque::new(),
            usb: Usb::Up,
            replies: VecDeque::new(),
            hung_at: None,
            lines: Vec::new(),
            runs: Vec::new(),
            resets: 0,
            deaf_to_silence: false,
            slow_wpm: None,
        }
    }

    /// Close the open run, if the box is no longer keying it.
    fn close_run(&mut self, now: u64) {
        let running = self.keyer.running() && !self.keyer.hung();
        let Some(run) = self.runs.last_mut().filter(|r| r.end.is_none()) else {
            return;
        };
        if running {
            return;
        }
        let last_up = self
            .keys
            .iter()
            .rev()
            .find(|&&(at, down)| !down && at >= run.start)
            .map(|&(at, _)| at);
        run.end = Some(last_up.unwrap_or(now).min(now));
        run.ended = self.keyer.ended();
    }

    /// Bring the box up to `now`: its keying, its watchdog, its USB.
    fn poll(&mut self, now: u64) {
        let keys = &mut self.keys;
        if self.keyer.hung() {
            let at = *self.hung_at.get_or_insert(now);
            let reset = at + u64::from(keyer_core::limits::WATCHDOG_MS);
            if now >= reset {
                // The watchdog: the chip resets, its pin goes low, USB re-enumerates.
                if self.keyer.key_down() {
                    keys.push_back((reset, false));
                }
                if let Some(run) = self.runs.last_mut().filter(|r| r.end.is_none()) {
                    run.end = Some(reset);
                    run.ended = keyer_core::keyer::Ended::None;
                }
                self.keyer = Keyer::new(Limits::BOX, Boot::Watchdog, reset);
                self.hung_at = None;
                self.resets += 1;
                self.replies.clear();
                if self.usb != Usb::Unplugged {
                    self.usb = Usb::Back(reset + USB_RESET_MS);
                }
            }
        } else {
            if self.deaf_to_silence {
                // Its link never goes quiet: a line of its own every poll, the
                // reply dropped, so the node sees only the run going on.
                if let Some(l) = keyer_core::frame::encode(1, format_args!("STATUS")) {
                    let _ = self.keyer.handle_line(now, l.as_bytes());
                }
            }
            self.keyer
                .poll_with(now, |at, down| keys.push_back((at, down)));
        }
        if let Usb::Back(t) = self.usb {
            if now >= t {
                self.usb = Usb::Up;
            }
        }
        self.close_run(now);
        while self.keys.len() > 4096 {
            self.keys.pop_front();
        }
    }

    pub fn key_down(&self) -> bool {
        self.keyer.key_down()
    }

    pub fn running(&self) -> bool {
        self.keyer.running()
    }

    pub fn ended(&self) -> keyer_core::keyer::Ended {
        self.keyer.ended()
    }

    pub fn trip(&self) -> keyer_core::keyer::Trip {
        self.keyer.trip()
    }

    /// The key-down stretches that overlap `from..to` (box ms); one still down
    /// ends at `to`.
    pub fn downs(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut since: Option<u64> = None;
        for &(at, down) in &self.keys {
            match (down, since) {
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

    /// Pull the USB cable (the box loses power and opens its key), or plug it in.
    pub fn unplug(&self, out: bool) {
        let now = self.clock.ms();
        let mut s = self.now();
        if out {
            let mut ks = Vec::new();
            // Unpowered: the run ends and the pin goes low.
            let mut k = std::mem::replace(&mut s.keyer, Keyer::new(Limits::BOX, Boot::Power, now));
            k.link_lost(now, |at, down| ks.push((at, down)));
            if k.key_down() {
                ks.push((now, false));
            }
            s.keys.extend(ks);
            if let Some(run) = s.runs.last_mut().filter(|r| r.end.is_none()) {
                run.end = Some(now);
                run.ended = keyer_core::keyer::Ended::Usb;
            }
            s.usb = Usb::Unplugged;
        } else if s.usb == Usb::Unplugged {
            s.keyer = Keyer::new(Limits::BOX, Boot::Power, now);
            s.usb = Usb::Back(now + USB_RESET_MS);
        }
    }
}

/// The speed and text of a `CW` line (`<id> CW <wpm> <text>*<check>`).
fn cw_command(line: &str) -> Option<(u32, String)> {
    let body = line.split_once('*').map_or(line, |(b, _)| b);
    let (_, rest) = body.split_once(' ')?;
    let rest = rest.strip_prefix("CW ")?;
    let (wpm, text) = rest.split_once(' ')?;
    Some((wpm.parse().ok()?, text.to_string()))
}

struct MockTransport {
    b: MockBox,
}

/// A `CW <wpm> <text>` line with its speed changed, keeping its id: a box that
/// keys slower than it was asked.
fn rewrite_wpm(line: &str, wpm: u32) -> String {
    let same = || line.to_string();
    let Ok((id, body)) = keyer_core::frame::decode(line.as_bytes()) else {
        return same();
    };
    let Some((_, text)) = body
        .strip_prefix("CW ")
        .and_then(|rest| rest.split_once(' '))
    else {
        return same();
    };
    keyer_core::frame::encode(id, format_args!("CW {wpm} {text}"))
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
        let now = self.b.clock.ms();
        let mut s = self.b.now();
        if s.usb != Usb::Up {
            return Err(gone());
        }
        s.lines.push(line.to_string());
        // What the box keys: the line as sent, unless its clock is wrong.
        let keyed = match s.slow_wpm {
            Some(wpm) => rewrite_wpm(line, wpm),
            None => line.to_string(),
        };
        let mut ks = Vec::new();
        let reply = s
            .keyer
            .handle_line_with(now, keyed.as_bytes(), |at, down| ks.push((at, down)));
        s.keys.extend(ks);
        if s.keyer.hung() {
            s.hung_at.get_or_insert(now);
        }
        if let Some(r) = reply {
            if r.as_str().contains(" OK CW*") {
                if let Some((wpm, text)) = cw_command(line) {
                    s.runs.push(Run {
                        text,
                        wpm,
                        start: now,
                        end: None,
                        ended: keyer_core::keyer::Ended::None,
                    });
                }
            }
            s.replies.push_back(r.as_str().to_string());
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
    /// Sidetone amplitude in the headphone audio; 0 is off.
    pub sidetone: f32,
    /// Band noise (standard deviation) on receive.
    pub noise: f32,
    /// The sound card's own noise, heard while the receiver is muted or the radio
    /// is off.
    pub floor: f32,
    /// Semi break-in hang after key-up (s); 0 is full break-in.
    pub hang: f64,
    /// The radio's keying delay (s).
    pub delay: f64,
    /// The sound card and capture's delay (s).
    pub latency: f64,
    /// Faults.
    pub cable_out: bool,
    /// The key held down at the radio from this radio time on.
    pub stuck_from: Option<f64>,
    /// The key held down at the radio until this radio time, then released: a hold
    /// that clears by itself, which the node must still catch.
    pub stuck_until: Option<f64>,
    pub off: bool,
    /// No audio reaches the computer at all.
    pub unplugged: bool,
    /// A carrier at the pitch on receive: (from, to, amplitude), radio time.
    pub carrier: Option<(f64, f64, f32)>,
}

impl RadioSettings {
    pub fn new(sample_rate: u32, pitch_hz: f32) -> Self {
        Self {
            sample_rate,
            pitch_hz,
            sidetone: 0.3,
            noise: 0.02,
            floor: 0.0002,
            hang: 0.6,
            delay: 0.005,
            latency: 0.06,
            cable_out: false,
            stuck_from: None,
            stuck_until: None,
            off: false,
            unplugged: false,
            carrier: None,
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
        let settings = Arc::new(Mutex::new(settings));
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = {
            let (settings, stop) = (settings.clone(), stop.clone());
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
                    // The box's key over this block, its hang and the radio's delay.
                    let span = (t - s.hang - s.delay - 0.01, t + BLOCK_S);
                    let downs: Vec<(f64, f64)> = keyer_box
                        .now()
                        .downs(
                            (span.0.max(0.0) * 1000.0) as u64,
                            (span.1 * 1000.0).ceil() as u64,
                        )
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
                    // Whether the radio's key was closed at any time in `a..=b`.
                    let closed = |a: f64, b: f64| {
                        (!s.cable_out && downs.iter().any(|&(d, u)| d <= b && u > a)) || stuck(b)
                    };
                    for (i, out) in buf.iter_mut().enumerate() {
                        let at = t + i as f64 / sr - s.delay;
                        phase = (phase + w / sr) % (2.0 * std::f64::consts::PI);
                        if s.off {
                            *out = floor[i];
                            continue;
                        }
                        let down = closed(at, at);
                        let muted = down || (s.hang > 0.0 && closed(at - s.hang, at));
                        let mut v = if muted { floor[i] } else { rx[i] };
                        if !muted {
                            if let Some((from, to, a)) = s.carrier {
                                if at >= from && at < to {
                                    v += a * phase.sin() as f32;
                                }
                            }
                        }
                        if down {
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
            stop,
            feeder: Some(feeder),
        }
    }

    pub fn set(&self, f: impl FnOnce(&mut RadioSettings)) {
        f(&mut lock(&self.settings));
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
