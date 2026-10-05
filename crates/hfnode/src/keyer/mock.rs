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
    /// Times it restarted.
    pub resets: u32,
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
            resets: 0,
        }
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
                self.keyer = Keyer::new(Limits::BOX, Boot::Watchdog, reset);
                self.hung_at = None;
                self.resets += 1;
                self.replies.clear();
                if self.usb != Usb::Unplugged {
                    self.usb = Usb::Back(reset + USB_RESET_MS);
                }
            }
        } else {
            self.keyer
                .poll_with(now, |at, down| keys.push_back((at, down)));
        }
        if let Usb::Back(t) = self.usb {
            if now >= t {
                self.usb = Usb::Up;
            }
        }
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
            s.usb = Usb::Unplugged;
        } else if s.usb == Usb::Unplugged {
            s.keyer = Keyer::new(Limits::BOX, Boot::Power, now);
            s.usb = Usb::Back(now + USB_RESET_MS);
        }
    }
}

struct MockTransport {
    b: MockBox,
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
        let mut ks = Vec::new();
        let reply = s
            .keyer
            .handle_line_with(now, line.as_bytes(), |at, down| ks.push((at, down)));
        s.keys.extend(ks);
        if s.keyer.hung() {
            s.hung_at.get_or_insert(now);
        }
        if let Some(r) = reply {
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
                    let stuck = |at: f64| s.stuck_from.is_some_and(|f| at >= f);
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
