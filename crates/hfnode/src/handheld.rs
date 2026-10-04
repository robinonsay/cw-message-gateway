//! An FM handheld as the node's radio (`station.rig = "handheld"`), for trying the
//! whole system out locally on 2 m before the IC-7300: a Quansheng UV-K1 or UV-K5,
//! or any FM handheld, with a sound-card cable such as the AIOC or a Digirig.
//!
//! Morse goes out as a keyed tone over FM (MCW): the node renders the tone itself
//! ([`cw::Keyer`]) and plays it to the radio's microphone while it holds PTT, and
//! hears the other station through the same decoder as on HF. What the handheld
//! cannot do that the IC-7300 can, and what covers it here:
//!
//! - **No transmit status, SWR or power reading.** [`Handheld::is_transmitting`]
//!   is the PTT line as the node set it. Each keying run is bounded three ways: the
//!   run's own worker releases PTT when the audio ends, or `playback_slack` after
//!   it should have; the station's software watchdog forces receive after
//!   `max_key_seconds`; and a deadman thread here releases PTT, on its own lock,
//!   once a run has lasted `max_key_seconds` plus [`DEADMAN_MARGIN`]. The PTT line
//!   is a serial control line, which the system drops when the process ends or the
//!   cable is unplugged ([`ptt`]). The radio's own transmit time-out timer is the
//!   backstop outside the computer (docs/handheld.md).
//! - **No tuner**: a window start sets nothing up on the radio and transmits
//!   nothing.
//! - **A small transmitter**: at most `max_duty_percent` of any `duty_window_secs`
//!   on the air; a long reply waits on receive between keying runs.
//! - **A shared channel**: FM simplex is used by others, and a node that cannot
//!   hear a carrier must not key over one. With a [`ChannelMonitor`] fed from the
//!   receive audio, the node keys only after the frequency has been quiet for
//!   `busy_quiet_ms`, and gives up after `busy_max_wait_secs`.
//! - **No frequency read-back**: the channel is set on the radio, and
//!   [`Handheld::frequency`] answers the configured one.

pub mod air;
pub mod playback;
pub mod ptt;

use crate::audio::Block;
use crate::config::{Config, RigKind};
use anyhow::{bail, Result};
use civ::{Rig, RigError, MAX_CW_CHARS};
use playback::{Playback, Playing};
use ptt::Ptt;
use serde::Deserialize;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How much longer than `max_key_seconds` a PTT run may last before the deadman
/// thread releases it: the station's watchdog should have acted by then.
pub const DEADMAN_MARGIN: Duration = Duration::from_secs(5);

/// The bands where US amateurs may send MCW, in Hz (47 CFR 97.305(c): 2 m from
/// 144.1 MHz, the 144.0-144.1 MHz segment being CW only; 1.25 m 222-225 MHz; 70 cm),
/// that FM handhelds cover.
pub const MCW_BANDS_HZ: [(u64, u64); 3] = [
    (144_100_000, 148_000_000),
    (222_000_000, 225_000_000),
    (420_000_000, 450_000_000),
];

/// National simplex calling frequencies (ARRL band plan): legal, but no place to
/// park an automatic Morse station.
const CALLING_HZ: [u64; 3] = [146_520_000, 223_500_000, 446_000_000];

/// Bring-up stages for a handheld (docs/handheld.md, "Bring-up"):
/// `[handheld] commissioned` names the last one passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Nothing has passed: `hfnode handheld check`, `listen`, `record` and
    /// `hfnode handheld rx` only; none of them keys the radio.
    #[default]
    None,
    /// `hfnode listen` decoded the other handheld's Morse correctly. Allows
    /// `hfnode handheld key`, with the operator at the radio.
    Listen,
    /// Short transmissions were heard correctly on the other handheld and PTT
    /// dropped after each; the watchdog test stopped a long one.
    Keying,
    /// The radio's own transmit time-out timer was checked. Allows `run`.
    Done,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Listen => "listen",
            Self::Keying => "keying",
            Self::Done => "done",
        })
    }
}

/// Commands that key a handheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Key,
    Run,
}

/// Whether `action` may run with `[handheld] commissioned` at `stage`.
pub fn check_stage(stage: Stage, action: Action) -> Result<()> {
    let (needs, name) = match action {
        Action::Key => (Stage::Listen, "hfnode handheld key"),
        Action::Run => (Stage::Done, "run"),
    };
    if stage < needs {
        bail!(
            "`{name}` needs bring-up stage `{needs}` to have passed, but \
             handheld.commissioned is `{stage}` (docs/handheld.md, \"Bring-up\")"
        );
    }
    Ok(())
}

/// The `[handheld]` settings, checked as part of [`Config::validate`].
pub fn validate(cfg: &Config) -> Result<()> {
    let s = &cfg.station;
    let h = match (s.rig, &cfg.handheld) {
        (RigKind::Ic7300, _) => return Ok(()),
        (RigKind::Handheld, None) => {
            bail!("station.rig is \"handheld\" but there is no [handheld] section")
        }
        (RigKind::Handheld, Some(h)) => h,
    };
    if !MCW_BANDS_HZ
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&s.frequency_hz))
    {
        bail!(
            "station.frequency_hz {} is outside the segments where US rules allow MCW \
             (47 CFR 97.305(c)): 144.1-148, 222-225 or 420-450 MHz",
            s.frequency_hz
        );
    }
    if CALLING_HZ.contains(&s.frequency_hz) {
        log::warn!(
            "station.frequency_hz {} is a national simplex calling frequency: pick a \
             quieter simplex channel",
            s.frequency_hz
        );
    }
    if s.serial_port.trim().is_empty() {
        bail!("station.serial_port must name the cable's serial port, whose line keys PTT");
    }
    if h.output_device.trim().is_empty() {
        bail!("handheld.output_device must name the cable's sound output");
    }
    // Above CTCSS tones (67-254 Hz), which receivers filter out, and inside the
    // 300-3000 Hz voice passband.
    if !(400.0..=2500.0).contains(&h.tone_hz) {
        bail!("handheld.tone_hz must be 400-2500");
    }
    if !(h.tone_level > 0.0 && h.tone_level <= 1.0) {
        bail!("handheld.tone_level must be above 0 and at most 1");
    }
    if !(100..=1000).contains(&h.lead_in_ms) {
        bail!("handheld.lead_in_ms must be 100-1000");
    }
    if h.tail_ms > 500 {
        bail!("handheld.tail_ms must be 0-500");
    }
    if !(10..=100).contains(&h.max_duty_percent) {
        bail!("handheld.max_duty_percent must be 10-100");
    }
    if !(60..=3600).contains(&h.duty_window_secs) {
        bail!("handheld.duty_window_secs must be 60-3600");
    }
    // The longest keying run the station allows must fit in the budget, or it
    // could never be keyed.
    let budget = h.duty_window_secs * u64::from(h.max_duty_percent) / 100;
    let run = s.max_key_seconds + (h.lead_in_ms + h.tail_ms).div_ceil(1000);
    if budget < run {
        bail!(
            "handheld.max_duty_percent of handheld.duty_window_secs allows {budget} s on \
             the air, less than one keying run of station.max_key_seconds ({run} s with \
             the lead-in and tail)"
        );
    }
    if !(0.0..1.0).contains(&h.busy_level) {
        bail!("handheld.busy_level must be 0 (off) or above, and below 1");
    }
    if h.busy_quiet_ms > 10_000 {
        bail!("handheld.busy_quiet_ms must be 0-10000");
    }
    if !(1..=600).contains(&h.busy_max_wait_secs) {
        bail!("handheld.busy_max_wait_secs must be 1-600");
    }
    Ok(())
}

/// How the handheld rig behaves. Durations are real time except `lead_in` and
/// `tail`, which are rendered into the audio and so play at `time_scale`.
#[derive(Debug, Clone)]
pub struct Settings {
    pub tone_hz: f32,
    pub tone_level: f32,
    pub lead_in: Duration,
    pub tail: Duration,
    /// The deadman releases PTT after one run has lasted this long.
    pub max_run: Duration,
    /// Allowed beyond a run's audio for the output to start and drain before its
    /// worker gives up on it and releases PTT.
    pub playback_slack: Duration,
    /// Share of `duty_window` that may be spent on the air.
    pub duty: f32,
    pub duty_window: Duration,
    /// Quiet needed on the frequency before keying (with a channel monitor).
    pub busy_quiet: Duration,
    pub busy_max_wait: Duration,
    /// How often the worker and the deadman look.
    pub poll: Duration,
    /// Playback speed: 1 on the air, faster in tests.
    pub time_scale: f32,
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let Some(h) = &cfg.handheld else {
            bail!("no [handheld] section");
        };
        Ok(Self {
            tone_hz: h.tone_hz,
            tone_level: h.tone_level,
            lead_in: Duration::from_millis(h.lead_in_ms),
            tail: Duration::from_millis(h.tail_ms),
            max_run: Duration::from_secs(cfg.station.max_key_seconds) + DEADMAN_MARGIN,
            playback_slack: Duration::from_secs(2),
            duty: h.max_duty_percent as f32 / 100.0,
            duty_window: Duration::from_secs(h.duty_window_secs),
            busy_quiet: Duration::from_millis(h.busy_quiet_ms),
            busy_max_wait: Duration::from_secs(h.busy_max_wait_secs),
            poll: Duration::from_millis(20),
            time_scale: 1.0,
        })
    }

    /// `d` of audio in real time.
    fn real(&self, d: Duration) -> Duration {
        d.div_f32(self.time_scale.max(0.001))
    }
}

/// Watches the received audio for an open squelch: someone using the frequency.
#[derive(Debug)]
pub struct ChannelMonitor {
    level: f32,
    last_busy: Mutex<Option<Instant>>,
}

impl ChannelMonitor {
    /// Busy while the received audio's RMS is above `level` (0-1 of full scale).
    pub fn new(level: f32) -> Arc<Self> {
        Arc::new(Self {
            level,
            last_busy: Mutex::new(None),
        })
    }

    /// Note one block of received audio.
    pub fn observe(&self, block: &Block) {
        if block.samples.is_empty() {
            return;
        }
        let power = block.samples.iter().map(|s| s * s).sum::<f32>() / block.samples.len() as f32;
        if power.sqrt() > self.level {
            let mut last = self.last_busy.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_none_or(|t| block.at > t) {
                *last = Some(block.at);
            }
        }
    }

    /// When the frequency was last heard in use.
    pub fn last_busy(&self) -> Option<Instant> {
        *self.last_busy.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// PTT and the audio of the run being keyed, shared with the run's worker and the
/// deadman thread. Locks are taken in the order `state`, `ptt`.
struct Shared {
    state: Mutex<State>,
    ptt: Mutex<Box<dyn Ptt>>,
    /// The audio of the current run, while it plays.
    playing: Mutex<Option<Box<dyn Playing>>>,
    /// Stops the deadman.
    closed: AtomicBool,
}

#[derive(Debug, Default)]
struct State {
    /// PTT is keyed, or may be: set before keying, and cleared only once a release
    /// has succeeded.
    keyed: bool,
    /// When the current run keyed PTT.
    since: Option<Instant>,
    /// Counts keying runs; a run's worker releases only its own.
    run: u64,
    /// When PTT was keyed and released, within the duty window.
    on_air: VecDeque<(Instant, Instant)>,
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Stop the audio and release PTT, for keying run `run` only (`None`: whatever
    /// is keyed, and release the line even if nothing is). On error the line's
    /// state is unknown, and the rig counts as keyed.
    fn release(&self, run: Option<u64>) -> std::io::Result<()> {
        let mut st = lock(&self.state);
        if run.is_some_and(|r| r != st.run || !st.keyed) {
            return Ok(());
        }
        if let Some(mut p) = lock(&self.playing).take() {
            p.stop();
        }
        let released = lock(&self.ptt).set(false);
        match &released {
            Ok(()) => {
                if let (true, Some(since)) = (st.keyed, st.since) {
                    st.on_air.push_back((since, Instant::now()));
                }
                st.keyed = false;
                st.since = None;
            }
            Err(e) => log::error!("releasing PTT failed: {e}"),
        }
        released
    }
}

/// An FM handheld keyed through a sound-card cable. See the module notes.
pub struct Handheld {
    set: Settings,
    frequency_hz: u64,
    wpm: u32,
    shared: Arc<Shared>,
    playback: Arc<dyn Playback>,
    channel: Option<Arc<ChannelMonitor>>,
    /// When the node started waiting for the frequency to clear.
    busy_since: Option<Instant>,
    worker: Option<JoinHandle<()>>,
    deadman: Option<JoinHandle<()>>,
    describe: String,
}

impl Handheld {
    /// A handheld keyed by `ptt` (released here first) and fed by `playback`.
    pub fn new(
        mut ptt: Box<dyn Ptt>,
        playback: Arc<dyn Playback>,
        set: Settings,
        frequency_hz: u64,
        wpm: u32,
    ) -> Result<Self> {
        ptt.set(false)
            .map_err(|e| anyhow::anyhow!("releasing PTT ({}): {e}", ptt.describe()))?;
        let describe = format!(
            "FM handheld on {:.4} MHz: PTT {}, tone {} Hz to {}",
            frequency_hz as f64 / 1e6,
            ptt.describe(),
            set.tone_hz,
            playback.describe()
        );
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            ptt: Mutex::new(ptt),
            playing: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let deadman = {
            let (shared, max, poll) = (shared.clone(), set.max_run, set.poll);
            thread::Builder::new()
                .name("ptt deadman".into())
                .spawn(move || deadman(&shared, max, poll))?
        };
        Ok(Self {
            set,
            frequency_hz,
            wpm,
            shared,
            playback,
            channel: None,
            busy_since: None,
            worker: None,
            deadman: Some(deadman),
            describe,
        })
    }

    /// Open the cable configured in `cfg`: its serial port for PTT (released at
    /// once) and its sound output. Keys nothing.
    pub fn open(cfg: &Config) -> Result<Self> {
        let Some(h) = &cfg.handheld else {
            bail!("no [handheld] section");
        };
        let ptt = ptt::SerialPtt::open(&cfg.station.serial_port, h.ptt)?;
        let speaker = crate::audio::Speaker::open(&h.output_device)?;
        Self::new(
            Box::new(ptt),
            Arc::new(speaker),
            Settings::from_config(cfg)?,
            cfg.station.frequency_hz,
            cfg.station.key_speed_wpm,
        )
    }

    /// Wait for a clear frequency before keying, as heard by `monitor`.
    pub fn set_channel_monitor(&mut self, monitor: Arc<ChannelMonitor>) {
        self.channel = Some(monitor);
    }

    pub fn describe(&self) -> &str {
        &self.describe
    }

    /// The audio for one keying run: lead-in, the Morse, tail.
    fn render(&self, text: &str) -> Vec<f32> {
        let rate = self.playback.rate();
        let mut k = cw::Keyer::new(rate, self.set.tone_hz, self.wpm as f32);
        k.amplitude = self.set.tone_level;
        let silence = |d: Duration| vec![0.0; (d.as_secs_f64() * f64::from(rate)) as usize];
        let mut out = silence(self.set.lead_in);
        out.extend(k.render(text, 0.0));
        out.extend(silence(self.set.tail));
        out
    }

    /// The duty cycle's wait before a run of `on_air` (real time) may be keyed.
    fn duty_rest(&self, on_air: Duration) -> civ::Result<Duration> {
        let window = self.set.duty_window;
        let budget = window.mul_f32(self.set.duty);
        if on_air > budget {
            return Err(RigError::Protocol(format!(
                "a keying run of {:.0} s is more than the duty cycle allows in {:.0} s",
                on_air.as_secs_f32(),
                window.as_secs_f32()
            )));
        }
        let now = Instant::now();
        let mut st = lock(&self.shared.state);
        while st
            .on_air
            .front()
            .is_some_and(|&(_, end)| now.saturating_duration_since(end) >= window)
        {
            st.on_air.pop_front();
        }
        // Time on the air in the window ending `t` from now (nothing is keyed in
        // between); it only falls as `t` grows.
        let used = |t: Duration| -> Duration {
            let to = now + t;
            let from = to.checked_sub(window);
            st.on_air
                .iter()
                .map(|&(s, e)| {
                    let s = from.map_or(s, |f| s.max(f));
                    e.min(to).saturating_duration_since(s)
                })
                .sum()
        };
        if used(Duration::ZERO) + on_air <= budget {
            return Ok(Duration::ZERO);
        }
        // The shortest wait after which this run fits: once the window has slid
        // past enough of the earlier runs.
        let (mut lo, mut hi) = (Duration::ZERO, window);
        while hi - lo > Duration::from_millis(10) {
            let mid = lo + (hi - lo) / 2;
            if used(mid) + on_air <= budget {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(hi)
    }

    /// The wait for a clear frequency, or an error once it has been busy for
    /// longer than `busy_max_wait`.
    fn busy_rest(&mut self) -> civ::Result<Duration> {
        let Some(ch) = &self.channel else {
            return Ok(Duration::ZERO);
        };
        let now = Instant::now();
        let quiet = ch
            .last_busy()
            .map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        if quiet >= self.set.busy_quiet {
            self.busy_since = None;
            return Ok(Duration::ZERO);
        }
        let since = *self.busy_since.get_or_insert(now);
        if now.duration_since(since) > self.set.busy_max_wait {
            self.busy_since = None;
            return Err(RigError::Protocol(format!(
                "the frequency has been in use for over {} s: not transmitting",
                self.set.busy_max_wait.as_secs()
            )));
        }
        Ok(self.set.busy_quiet - quiet)
    }

    /// Clean up a finished worker.
    fn reap(&mut self) {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().map(JoinHandle::join);
        }
    }
}

/// Release PTT from a run that has lasted longer than `max`, until released.
fn deadman(shared: &Shared, max: Duration, poll: Duration) {
    while !shared.closed.load(Ordering::SeqCst) {
        thread::sleep(poll);
        let overdue = {
            let st = lock(&shared.state);
            st.keyed && st.since.is_none_or(|t| t.elapsed() > max)
        };
        if overdue {
            log::error!("PTT keyed for over {max:?}: the deadman is releasing it");
            let _ = shared.release(None);
        }
    }
}

impl Rig for Handheld {
    /// The configured channel: a handheld without CAT cannot be read.
    fn frequency(&mut self) -> civ::Result<u64> {
        Ok(self.frequency_hz)
    }
    fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
        self.frequency_hz = hz;
        Ok(())
    }
    /// The node sends MCW on an FM radio: nothing to set.
    fn set_mode_cw(&mut self) -> civ::Result<()> {
        Ok(())
    }
    /// Set on the radio itself.
    fn set_rf_power_watts(&mut self, _: u32) -> civ::Result<()> {
        Ok(())
    }
    fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
        self.wpm = wpm.clamp(5, 60);
        Ok(())
    }
    /// PTT is held for each keying run as a whole.
    fn set_break_in(&mut self, _: bool) -> civ::Result<()> {
        Ok(())
    }
    fn set_break_in_delay(&mut self, _: f32) -> civ::Result<()> {
        Ok(())
    }
    fn dot_duration(&mut self) -> civ::Result<Duration> {
        Ok(Duration::from_secs_f32(1.2 / self.wpm as f32))
    }
    /// No tuner: refused, so that nothing can key a carrier through here.
    fn start_tune(&mut self) -> civ::Result<()> {
        Err(RigError::Rejected)
    }
    fn tuner_busy(&mut self) -> civ::Result<bool> {
        Ok(false)
    }
    fn read_swr(&mut self) -> civ::Result<f32> {
        Err(RigError::Protocol("a handheld has no SWR meter".into()))
    }
    fn read_po(&mut self) -> civ::Result<f32> {
        Err(RigError::Protocol("a handheld has no power meter".into()))
    }

    /// Key PTT and play `text` as a tone, with the lead-in before and the tail
    /// after, and return at once; a worker releases PTT when the audio is done.
    fn send_cw(&mut self, text: &str) -> civ::Result<()> {
        if text.chars().count() > MAX_CW_CHARS {
            return Err(RigError::Protocol(format!(
                "{} characters: at most {MAX_CW_CHARS} per keying run",
                text.chars().count()
            )));
        }
        if let Some(c) = text.chars().find(|&c| !cw::is_sendable(c)) {
            return Err(RigError::Protocol(format!("{c:?} cannot be sent in Morse")));
        }
        self.reap();
        let samples = self.render(text);
        let length = self.set.real(Duration::from_secs_f64(
            samples.len() as f64 / f64::from(self.playback.rate()),
        ));
        let run = {
            let mut st = lock(&self.shared.state);
            if st.keyed {
                return Err(RigError::Protocol("still transmitting".into()));
            }
            st.run += 1;
            st.keyed = true;
            st.since = Some(Instant::now());
            // Keyed under the state lock, so a release cannot come in between.
            let keyed = lock(&self.shared.ptt).set(true);
            if let Err(e) = keyed {
                drop(st);
                let _ = self.shared.release(None);
                return Err(RigError::Io(e));
            }
            st.run
        };
        let playing = match self.playback.start(samples) {
            Ok(p) => p,
            Err(e) => {
                let _ = self.shared.release(Some(run));
                return Err(RigError::Io(std::io::Error::other(format!(
                    "starting the audio output: {e:#}"
                ))));
            }
        };
        *lock(&self.shared.playing) = Some(playing);
        let (shared, poll) = (self.shared.clone(), self.set.poll);
        let deadline = Instant::now() + length + self.set.playback_slack;
        let worker = thread::Builder::new()
            .name("ptt run".into())
            .spawn(move || {
                loop {
                    thread::sleep(poll);
                    if lock(&shared.state).run != run {
                        return;
                    }
                    let done = match lock(&shared.playing).as_mut() {
                        Some(p) => p.finished(),
                        // Stopped: released already, or being released.
                        None => true,
                    };
                    if done {
                        break;
                    }
                    if Instant::now() > deadline {
                        log::error!(
                            "the audio output has not finished {:?} after it should \
                             have: releasing PTT",
                            length
                        );
                        break;
                    }
                }
                // Retried by the deadman if this fails.
                let _ = shared.release(Some(run));
            });
        match worker {
            Ok(w) => {
                self.worker = Some(w);
                Ok(())
            }
            Err(e) => {
                let _ = self.shared.release(Some(run));
                Err(RigError::Io(e))
            }
        }
    }

    fn stop_cw(&mut self) -> civ::Result<()> {
        self.shared.release(None).map_err(RigError::Io)
    }

    /// PTT as the node last set it, or keyed if a release failed.
    fn is_transmitting(&mut self) -> civ::Result<bool> {
        Ok(lock(&self.shared.state).keyed)
    }

    /// Only receive: the node never keys a handheld except through
    /// [`Rig::send_cw`].
    fn set_transmit(&mut self, tx: bool) -> civ::Result<()> {
        if tx {
            return Err(RigError::Rejected);
        }
        self.stop_cw()
    }

    fn has_tuner(&self) -> bool {
        false
    }
    fn has_meters(&self) -> bool {
        false
    }

    fn rest_needed(&mut self, keying: Duration) -> civ::Result<Duration> {
        let on_air = keying + self.set.real(self.set.lead_in + self.set.tail);
        let duty = self.duty_rest(on_air)?;
        if !duty.is_zero() {
            log::info!(
                "duty cycle: {:.1} s on receive before the next keying run",
                duty.as_secs_f32()
            );
        }
        let busy = self.busy_rest()?;
        if !busy.is_zero() {
            log::info!("the frequency is in use: waiting for it to clear");
        }
        Ok(duty.max(busy))
    }
}

impl Drop for Handheld {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            if self.shared.release(None).is_ok() {
                break;
            }
            thread::sleep(self.set.poll);
        }
        for t in [self.worker.take(), self.deadman.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests;
