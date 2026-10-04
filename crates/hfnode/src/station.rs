//! Transmit-side safety for unattended operation.
//!
//! Receive-only operation is harmless; the risks are on transmit. This layer is the
//! only code that keys the radio, and it enforces:
//!
//! - **Bounded keying runs.** Text goes out in keyer-sized pieces with a pause
//!   between segments. A piece counts as finished only once its full keying time
//!   (at the radio's actual keyer speed) has passed *and* the radio reports
//!   receive; the semi break-in delay is set longer than a word gap so the radio
//!   does not drop to receive part-way through a piece.
//! - **Forced receive.** After any failure, and on shutdown, the keyer is stopped
//!   and the radio switched to receive, then receive is confirmed by reading the
//!   radio's status, allowing for the break-in delay. If it cannot be confirmed,
//!   transmitting is inhibited. With a state directory the inhibit is also written
//!   to [`INHIBIT_FILE`] there, so a restart does not clear it (systemd restarts the
//!   service after a crash); only removing the file, once the radio has been
//!   checked, does.
//! - **Software watchdog.** A separate thread forces the radio back to receive if
//!   any one keying run lasts longer than `max_key_seconds`, and keeps trying until
//!   receive is confirmed. It backs up, and does not replace, the hardware transmit
//!   timer (docs/hardware-test-plan.md, step 10), which must act on CI-V keying.
//! - **SWR check.** SWR is sampled repeatedly during the first second or so of
//!   each transmission, counting only samples taken with the Po meter showing
//!   output, and the highest is used; above the limit the node stops and stays
//!   silent until the next window. If the first piece is keyed without one such
//!   sample, the node also stops and stays silent: the radio's own protection cuts
//!   its output into a bad load, so missing output is itself a sign of one.
//!   Each SWR sample also reads the transmit status: if the radio reads receive
//!   while the Po meter shows output, its status cannot be trusted for the checks
//!   above, and transmitting is inhibited as above.
//! - **Reduced power**, set at start-up.
//! - **Tuning** at start-up and at the top of each listening window.
//! - **A health log** of every tune and SWR reading, so a slow upward trend (a
//!   corroding connector, a loosened coil) shows up before it becomes a fault.
//! - **Storm stand-down.** With a [`StormHold`] attached, nothing is tuned or keyed
//!   while it is on, and a transmission under way is stopped and the radio forced to
//!   receive ([`crate::storm`]).

use crate::session::Transmission;
use crate::storm::StormHold;
use civ::{split_for_keyer, Rig, RigError};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct StationConfig {
    pub frequency_hz: u64,
    pub power_watts: u32,
    pub key_speed_wpm: u32,
    pub max_key: Duration,
    pub swr_limit: f32,
    pub segment_pause: Duration,
    /// How long after the keyer accepts a piece to take the first SWR sample.
    pub swr_delay: Duration,
    /// How long after the keyer accepts a piece to keep sampling SWR, at most; a
    /// shorter piece is sampled until its keying time plus the break-in delay
    /// (allowing for the transmitter's switch-on delay). Samples taken with the key
    /// up show no output and are not counted.
    pub swr_window: Duration,
    /// Po meter reading (percent of full output) that counts as the key being down
    /// for an SWR sample.
    pub swr_min_po: f32,
    /// Semi break-in delay in dots. Must exceed the 7-dot word gap so the radio
    /// stays on transmit for a whole piece.
    pub break_in_delay_dots: f32,
    /// Extra time allowed beyond the keying time and break-in delay before the
    /// transmitter is declared stuck.
    pub stuck_margin: Duration,
    /// Longest a tuner cycle may take before it is abandoned and receive forced.
    pub tune_timeout: Duration,
    pub poll: Duration,
}

impl StationConfig {
    pub fn from_config(c: &crate::config::Station) -> Self {
        Self {
            frequency_hz: c.frequency_hz,
            power_watts: c.power_watts,
            key_speed_wpm: c.key_speed_wpm,
            max_key: Duration::from_secs(c.max_key_seconds),
            swr_limit: c.swr_limit,
            segment_pause: Duration::from_millis(c.chunk_pause_ms),
            swr_delay: Duration::from_millis(50),
            swr_window: Duration::from_secs(1),
            // A quarter of the set power: well clear of key-up (0) and of the
            // CW envelope's rise and fall.
            swr_min_po: (c.power_watts as f32 * 0.25).max(2.0),
            // 10 dots: 3 dots more than a word gap. At most 2 s (at 6 wpm), which
            // the 3 s stuck margin covers.
            break_in_delay_dots: 10.0,
            stuck_margin: Duration::from_secs(3),
            // The manual's tuner takes "2~3 seconds" (p. 11-2), and "15 seconds
            // (maximum)" (p. 16-3, manual text line 8119): leave room above that.
            tune_timeout: Duration::from_secs(20),
            poll: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TxError {
    /// SWR was too high earlier in this window; transmitting is suspended.
    SwrLockout,
    /// SWR was too high just now; the transmission was cut off.
    HighSwr(f32),
    /// A piece was keyed without the Po meter showing output, so SWR could not be
    /// measured; treated like high SWR (the radio cuts its power into a bad load).
    NoOutput,
    /// The radio stayed on transmit too long and was forced back to receive.
    Stuck,
    /// The radio could not be confirmed back on receive; nothing more is sent until
    /// the node is restarted.
    Inhibited,
    /// Thunder near the station (or no storm check to say otherwise): nothing is
    /// keyed, and a transmission under way was stopped.
    Storm(String),
    Rig(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwrLockout => write!(f, "transmit locked out after high SWR"),
            Self::HighSwr(s) => write!(f, "SWR {s:.1} above limit"),
            Self::NoOutput => write!(f, "no output while keying: SWR not measured"),
            Self::Stuck => write!(f, "transmitter did not return to receive"),
            Self::Inhibited => write!(
                f,
                "radio not confirmed on receive: transmit inhibited until the node is \
                 restarted with {INHIBIT_FILE} removed from the state directory"
            ),
            Self::Storm(why) => write!(f, "storm stand-down: {why}"),
            Self::Rig(e) => write!(f, "radio error: {e}"),
        }
    }
}

impl From<civ::RigError> for TxError {
    fn from(e: civ::RigError) -> Self {
        Self::Rig(e.to_string())
    }
}

/// How many times [`force_receive`] tries, at least, before giving up.
const FORCE_RX_ATTEMPTS: u32 = 3;

/// Longest semi break-in delay the radio can be set to, in dots: "00 00=2.0d to
/// 02 55=13.0d" (14 0F, p. 19-3).
const MAX_BREAK_IN_DOTS: f32 = 13.0;

/// A dot at the keyer's slowest speed, 6 wpm ("00 00=6wpm", 14 0C, p. 19-3), for
/// when the speed cannot be read.
const SLOWEST_DOT: Duration = Duration::from_millis(200);

/// Put the radio on receive and confirm it: stop the keyer and switch to receive
/// (each sent whether or not the other worked), then read the transmit status.
/// Repeated until receive is seen; an error means receive could not be confirmed.
///
/// With semi break-in the radio "returns to receive after a preset time after you
/// stop keying" (p. 4-15), and the receive command may not cut that short, so the
/// attempts go on for the longest break-in delay at the keyer's speed before giving
/// up. That time counts from when the first stop and receive commands have gone
/// out: after a CI-V timeout the driver first waits for the link to go quiet (up to
/// four reply timeouts), and the radio cannot start its delay before then.
pub fn force_receive<R: Rig + ?Sized>(r: &mut R) -> civ::Result<()> {
    let dot = r.dot_duration().unwrap_or(SLOWEST_DOT);
    let mut deadline = None;
    let mut last = RigError::Timeout;
    for attempt in 0.. {
        if attempt >= FORCE_RX_ATTEMPTS && deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if attempt > 0 {
            thread::sleep(Duration::from_millis(100));
        }
        if let Err(e) = r.stop_cw() {
            log::warn!("forcing receive: stop CW: {e}");
        }
        if let Err(e) = r.set_transmit(false) {
            log::warn!("forcing receive: set receive: {e}");
        }
        deadline.get_or_insert_with(|| Instant::now() + dot.mul_f32(MAX_BREAK_IN_DOTS));
        match r.is_transmitting() {
            Ok(false) => return Ok(()),
            Ok(true) => last = RigError::Protocol("radio still reports transmit".into()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// [`force_receive`], latching `inhibit` if receive is not confirmed.
fn force_receive_or_inhibit<R: Rig>(rig: &Mutex<R>, inhibit: &Inhibit) -> Result<(), TxError> {
    let mut r = rig.lock().unwrap_or_else(|e| e.into_inner());
    force_receive(&mut *r).map_err(|e| {
        inhibit.latch(&format!("radio not confirmed on receive ({e})"));
        TxError::Inhibited
    })
}

/// Written to the state directory (beside the health log) when transmitting is
/// inhibited; while it exists, nothing is transmitted, across restarts.
pub const INHIBIT_FILE: &str = "tx-inhibited";

/// The latch that stops all transmitting, in memory and in [`INHIBIT_FILE`].
struct Inhibit {
    set: AtomicBool,
    file: Option<PathBuf>,
}

impl Inhibit {
    fn new(file: Option<PathBuf>) -> Self {
        let on_disk = file.as_deref().filter(|f| f.exists());
        if let Some(f) = on_disk {
            let why = std::fs::read_to_string(f).unwrap_or_default();
            log::error!(
                "transmit inhibited by {} ({}): remove it once the radio has been checked",
                f.display(),
                why.trim()
            );
        }
        Self {
            set: AtomicBool::new(on_disk.is_some()),
            file,
        }
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }

    fn latch(&self, why: &str) {
        if self.set.swap(true, Ordering::SeqCst) {
            return;
        }
        log::error!("{why}: transmit inhibited");
        if let Some(f) = &self.file {
            let line = format!("{} {why}\n", crate::gateway::unix_now());
            match std::fs::write(f, line) {
                Ok(()) => log::error!(
                    "wrote {}: nothing is transmitted, also after a restart, until it is removed",
                    f.display()
                ),
                Err(e) => log::error!("cannot write {}: {e}", f.display()),
            }
        }
    }
}

pub struct Station<R: Rig + 'static> {
    rig: Arc<Mutex<R>>,
    cfg: StationConfig,
    keying_since: Arc<Mutex<Option<Instant>>>,
    watchdog_fired: Arc<AtomicBool>,
    /// Latched when the radio could not be confirmed on receive, or its status
    /// could not be trusted; never cleared while running.
    tx_inhibit: Arc<Inhibit>,
    stop: Arc<AtomicBool>,
    swr_lockout: bool,
    /// SWR has been measured on the current transmission.
    swr_checked: bool,
    health_log: Option<PathBuf>,
    /// While this says so, nothing is tuned or keyed.
    storm: Option<Arc<StormHold>>,
}

/// Append `<unix time>,<event>,<value>` to the health log at `path`.
pub(crate) fn append_health(path: &Path, event: &str, value: &str) {
    // One line, three fields.
    let value: String = value
        .chars()
        .map(|c| if c == ',' || c.is_control() { ' ' } else { c })
        .collect();
    let line = format!("{},{event},{value}\n", crate::gateway::unix_now());
    if let Err(e) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
    {
        log::warn!("cannot write health log: {e}");
    }
}

impl<R: Rig + 'static> Station<R> {
    /// `health_log` is a file in the state directory; [`INHIBIT_FILE`] is kept
    /// beside it, and if it is already there nothing will be transmitted.
    pub fn new(rig: R, cfg: StationConfig, health_log: Option<PathBuf>) -> Self {
        let inhibit_file = health_log
            .as_deref()
            .map(|p| p.parent().unwrap_or(Path::new("")).join(INHIBIT_FILE));
        let s = Self {
            rig: Arc::new(Mutex::new(rig)),
            cfg,
            keying_since: Arc::new(Mutex::new(None)),
            watchdog_fired: Arc::new(AtomicBool::new(false)),
            tx_inhibit: Arc::new(Inhibit::new(inhibit_file)),
            stop: Arc::new(AtomicBool::new(false)),
            swr_lockout: false,
            swr_checked: false,
            health_log,
            storm: None,
        };
        s.spawn_watchdog();
        s
    }

    pub fn rig(&self) -> Arc<Mutex<R>> {
        self.rig.clone()
    }

    /// Stand down whenever `hold` says so (see [`crate::storm`]).
    pub fn set_storm_hold(&mut self, hold: Arc<StormHold>) {
        self.storm = Some(hold);
    }

    /// Why the storm stand-down is on, if it is.
    fn storm_reason(&self) -> Option<String> {
        self.storm.as_ref().and_then(|h| h.reason())
    }

    /// Stop with [`TxError::Storm`] if the storm stand-down is on.
    fn check_storm(&self) -> Result<(), TxError> {
        match self.storm_reason() {
            Some(why) => Err(TxError::Storm(why)),
            None => Ok(()),
        }
    }

    /// Whether transmitting has been inhibited (see [`INHIBIT_FILE`]).
    pub fn tx_inhibited(&self) -> bool {
        self.tx_inhibit.is_set()
    }

    fn spawn_watchdog(&self) {
        let (rig, since, fired, inhibit, stop, max) = (
            self.rig.clone(),
            self.keying_since.clone(),
            self.watchdog_fired.clone(),
            self.tx_inhibit.clone(),
            self.stop.clone(),
            self.cfg.max_key,
        );
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                let started = *since.lock().unwrap_or_else(|e| e.into_inner());
                if started.is_some_and(|t| t.elapsed() > max) {
                    log::error!("watchdog: keying exceeded {max:?}, forcing receive");
                    fired.store(true, Ordering::SeqCst);
                    // Keep trying on later ticks until receive is confirmed.
                    if force_receive_or_inhibit(&rig, &inhibit).is_ok() {
                        *since.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    }
                }
            }
        });
    }

    fn force_rx(&self) -> Result<(), TxError> {
        force_receive_or_inhibit(&self.rig, &self.tx_inhibit)
    }

    fn with_rig<T>(&self, f: impl FnOnce(&mut R) -> civ::Result<T>) -> civ::Result<T> {
        let mut r = self.rig.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut r)
    }

    fn health(&self, event: &str, value: &str) {
        log::info!("health: {event} {value}");
        if let Some(path) = &self.health_log {
            append_health(path, event, value);
        }
    }

    /// Put the radio in the node's operating state.
    pub fn configure(&self) -> civ::Result<()> {
        self.with_rig(|r| r.set_transmit(false))?;
        self.apply_settings()
    }

    /// Frequency, mode, power and keyer settings; none of these transmits.
    fn apply_settings(&self) -> civ::Result<()> {
        let c = self.cfg.clone();
        self.with_rig(|r| {
            // Mode first: with SSB/CW Synchronous Tuning ON, a change from SSB to CW
            // shifts the frequency by the CW pitch (p. 12-6, manual text line 6409).
            r.set_mode_cw()?;
            r.set_frequency(c.frequency_hz)?;
            r.set_rf_power_watts(c.power_watts)?;
            r.set_key_speed(c.key_speed_wpm)?;
            r.set_break_in_delay(c.break_in_delay_dots)?;
            r.set_break_in(true)
        })
    }

    /// The radio would transmit on the configured frequency: split and ∂TX off, and
    /// 1C 03 reads the frequency set. Someone at the radio may have switched either
    /// on since start-up, when the preflight checked them.
    fn check_transmit_frequency(&self) -> civ::Result<()> {
        let hz = self.cfg.frequency_hz;
        self.with_rig(|r| {
            if r.split_or_delta_tx()? {
                return Err(RigError::Protocol("split or ∂TX is on".into()));
            }
            match r.transmit_frequency()? {
                tx if tx == hz => Ok(()),
                tx => Err(RigError::Protocol(format!(
                    "transmit frequency reads {tx} Hz, not {hz} Hz"
                ))),
            }
        })
    }

    /// Set the radio up again and run the internal tuner. Call at start-up and at
    /// the top of each listening window; clears any SWR lockout from the last window,
    /// and locks out transmitting for this one if the radio could not be set up or
    /// the tuner could not match.
    pub fn start_window(&mut self) -> civ::Result<()> {
        if self.tx_inhibited() {
            // Tuning transmits.
            return Err(RigError::Protocol(TxError::Inhibited.to_string()));
        }
        self.swr_lockout = false;
        self.swr_checked = false;
        // Set the radio up again: the front panel, another program or a power cycle
        // may have changed it since the last window, and the tune transmits. It
        // should be on receive already; if it is not, something else is keying it.
        // Split and ∂TX are not set by the node, so they are only checked.
        let ready = self
            .with_rig(|r| r.is_transmitting())
            .and_then(|tx| match tx {
                false => self.apply_settings(),
                true => Err(RigError::Protocol("on transmit at window start".into())),
            })
            .and_then(|()| self.check_transmit_frequency());
        if let Err(e) = ready {
            self.swr_lockout = true;
            log::error!("could not set the radio up ({e}): silent until next window");
            self.force_rx()
                .map_err(|e| RigError::Protocol(e.to_string()))?;
            return Err(e);
        }
        if let Some(why) = self.storm_reason() {
            // Tuning transmits. Not a lockout: once the stand-down ends, the SWR
            // check on the first transmission catches a bad match, as after a tune
            // that failed.
            self.health("tune", "storm");
            log::warn!("storm stand-down, not tuning: {why}");
            return Err(RigError::Protocol(format!("storm stand-down: {why}")));
        }
        let t0 = Instant::now();
        if let Err(e) = self.tune(t0) {
            // The radio may have taken 1C 01 02 even if its reply was lost, or still
            // be tuning: make sure it is back on receive before going on.
            self.force_rx()
                .map_err(|e| RigError::Protocol(e.to_string()))?;
            return Err(e);
        }
        if !self.with_rig(|r| r.tuner_matched())? {
            // The tuner bypassed itself: the antenna is beyond its 3:1 range.
            self.health("tune", "no-match");
            self.swr_lockout = true;
            log::error!("tuner could not match the antenna: silent until next window");
            return Err(RigError::Protocol("tuner could not match the load".into()));
        }
        self.health("tune", &format!("{}ms", t0.elapsed().as_millis()));
        Ok(())
    }

    /// Start a tuner cycle and wait for it to end, for at most `tune_timeout`.
    fn tune(&self, t0: Instant) -> civ::Result<()> {
        // Tuning transmits: whoever calls this, not during a storm stand-down.
        if let Some(why) = self.storm_reason() {
            return Err(RigError::Protocol(format!("storm stand-down: {why}")));
        }
        self.with_rig(|r| r.start_tune())?;
        // The manual does not say how soon 1C 01 reads 02 ("tuning") after the
        // command: allow a moment for it, so that a tune is not taken as finished
        // before it has begun.
        let start_wait = self.cfg.tune_timeout / 20;
        let mut started = false;
        loop {
            let busy = self.with_rig(|r| r.tuner_busy())?;
            started |= busy;
            if !busy && (started || t0.elapsed() > start_wait) {
                break;
            }
            if t0.elapsed() > self.cfg.tune_timeout {
                self.health("tune", "timeout");
                return Err(civ::RigError::Timeout);
            }
            thread::sleep(self.cfg.poll);
        }
        if !started {
            log::warn!("the tuner never read 02 (tuning) after 1C 01 02");
        }
        Ok(())
    }

    /// Key a transmission, enforcing every safety rule above.
    pub fn transmit(&mut self, tx: &Transmission) -> Result<(), TxError> {
        if self.tx_inhibited() {
            return Err(TxError::Inhibited);
        }
        if self.swr_lockout {
            return Err(TxError::SwrLockout);
        }
        self.check_storm()?;
        // SWR is measured on every transmission, not once per window: the antenna
        // or a connector can fail in the middle of one.
        self.swr_checked = false;
        // On success the radio has been seen back on receive after the last piece;
        // on failure force it there.
        let result = self
            .transmit_inner(tx)
            .or_else(|e| self.force_rx().and(Err(e)));
        if result != Err(TxError::Inhibited) {
            *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        } else {
            // Not confirmed on receive: leave the watchdog something to retry.
            self.keying_since
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(Instant::now);
        }
        result
    }

    fn transmit_inner(&mut self, tx: &Transmission) -> Result<(), TxError> {
        self.watchdog_fired.store(false, Ordering::SeqCst);
        for (si, segment) in tx.segments.iter().enumerate() {
            if si > 0 {
                thread::sleep(self.cfg.segment_pause);
            }
            for piece in split_for_keyer(segment) {
                self.check_storm()?;
                self.wait_for_receive(Instant::now() + Duration::from_secs(2))?;
                // Timed at the speed the radio's keyer is really using.
                let dot = self.with_rig(|r| r.dot_duration())?;
                let keying = dot * cw::units(&piece);
                let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                self.with_rig(|r| r.send_cw(&piece))?;
                let sent = Instant::now();

                if !self.swr_checked {
                    self.check_swr(sent, keying + hang)?;
                }
                // The radio's status says nothing about the keyer until the whole
                // piece has had time to go out (it reads receive before semi
                // break-in has switched over), so wait that long first.
                self.sleep_until(sent + keying)?;
                self.wait_for_receive(sent + keying + hang + self.cfg.stuck_margin)?;
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        Ok(())
    }

    /// Sample SWR from `swr_delay` after `sent` until `swr_window` or `on_air` has
    /// passed, using only samples with the Po meter showing output on both sides of
    /// the SWR reading (key-up reads as SWR 1.0). Without such a sample by then,
    /// sampling goes on until `on_air` has passed, and if there is still none the
    /// piece is a fault: the radio reduces its output when the SWR is high
    /// ("Power down transmission", p. 13-4), so the Po meter may never reach
    /// `swr_min_po` into a bad load.
    fn check_swr(&mut self, sent: Instant, on_air: Duration) -> Result<(), TxError> {
        let (first_end, end) = (sent + self.cfg.swr_window.min(on_air), sent + on_air);
        let min_po = self.cfg.swr_min_po;
        let mut worst: Option<f32> = None;
        self.sleep_until(sent + self.cfg.swr_delay)?;
        // At least one sample, however late this thread gets to run.
        let mut sampled = false;
        loop {
            let now = Instant::now();
            if sampled && (now >= end || now >= first_end && worst.is_some()) {
                break;
            }
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            self.check_storm()?;
            let sample = self.with_rig(|r| {
                let before = r.read_po()?;
                let tx = r.is_transmitting()?;
                let swr = r.read_swr()?;
                let after = r.read_po()?;
                Ok((before.min(after) >= min_po).then_some((swr, tx)))
            })?;
            if let Some((_, false)) = sample {
                // Output on the Po meter while the radio reads receive: its status
                // does not show keyer transmissions, so no receive confirmation in
                // this module means anything. (The manual does not say 1C 00 covers
                // them; this checks it on every window.)
                self.health("tx-status", "rx-with-output");
                self.tx_inhibit.latch(
                    "radio reads receive (1C 00) while the Po meter shows output: its \
                     transmit status cannot be trusted",
                );
                return Err(TxError::Inhibited);
            }
            sampled = true;
            if let Some((swr, _)) = sample {
                worst = Some(worst.map_or(swr, |w| w.max(swr)));
                if swr > self.cfg.swr_limit {
                    self.health("swr", &format!("{swr:.2}"));
                    self.swr_lockout = true;
                    log::error!(
                        "SWR {swr:.2} above {:.1}: silent until next window",
                        self.cfg.swr_limit
                    );
                    return Err(TxError::HighSwr(swr));
                }
            }
            thread::sleep(self.cfg.poll);
        }
        match worst {
            Some(swr) => {
                self.swr_checked = true;
                self.health("swr", &format!("{swr:.2}"));
                Ok(())
            }
            None => {
                self.health("swr", "no-output");
                self.swr_lockout = true;
                log::error!("no output on the Po meter while keying: silent until next window");
                Err(TxError::NoOutput)
            }
        }
    }

    /// Sleep until `t`, stopping early if the watchdog fires or the storm
    /// stand-down comes on.
    fn sleep_until(&self, t: Instant) -> Result<(), TxError> {
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            self.check_storm()?;
            let now = Instant::now();
            if now >= t {
                return Ok(());
            }
            thread::sleep((t - now).min(self.cfg.poll));
        }
    }

    /// Wait until the radio reports receive, or declare it stuck at `deadline`.
    /// Stops early, to force receive, if the storm stand-down comes on.
    fn wait_for_receive(&self, deadline: Instant) -> Result<(), TxError> {
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            self.check_storm()?;
            if !self.with_rig(|r| r.is_transmitting())? {
                return Ok(());
            }
            if Instant::now() > deadline {
                log::error!("radio still transmitting; forcing receive");
                return Err(TxError::Stuck);
            }
            thread::sleep(self.cfg.poll);
        }
    }
}

impl<R: Rig + 'static> Drop for Station<R> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.force_rx();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use civ::sim::SimRig;

    fn cfg() -> StationConfig {
        StationConfig {
            frequency_hz: 7_030_000,
            power_watts: 40,
            key_speed_wpm: 20,
            max_key: Duration::from_secs(2),
            swr_limit: 2.0,
            segment_pause: Duration::from_millis(10),
            swr_delay: Duration::from_millis(1),
            swr_window: Duration::from_millis(200),
            swr_min_po: 10.0,
            break_in_delay_dots: 10.0,
            stuck_margin: Duration::from_millis(300),
            tune_timeout: Duration::from_secs(15),
            poll: Duration::from_millis(2),
        }
    }

    fn fast_rig() -> SimRig {
        let mut r = SimRig::new();
        r.time_scale = 50.0;
        r
    }

    fn tx(segments: &[&str]) -> Transmission {
        Transmission {
            segments: segments.iter().map(|s| s.to_string()).collect(),
            read_ids: Vec::new(),
        }
    }

    #[test]
    fn configures_and_keys_in_pieces() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        st.transmit(&tx(&[
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K",
            "SECOND = B",
        ]))
        .unwrap();
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert_eq!(r.power_watts, 40);
        assert!(r.cw_mode && r.break_in);
        assert_eq!(r.break_in_delay_dots, 10.0);
        assert_eq!(r.tunes, 1);
        assert!(r.sent.iter().all(|p| p.len() <= civ::MAX_CW_CHARS));
        assert_eq!(
            r.sent.join(" "),
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K SECOND = B"
        );
        assert!(!r.keyer_busy() && !r.is_transmitting().unwrap());
    }

    #[test]
    fn waits_for_the_keyer_despite_slow_switch_on() {
        // The radio reads receive for a while after accepting the text, and the
        // configured speed is above the keyer's 48 wpm limit.
        let mut rig = fast_rig();
        rig.tx_on_delay = Duration::from_millis(1500);
        let mut c = cfg();
        c.key_speed_wpm = 60;
        let mut st = Station::new(rig, c, None);
        st.configure().unwrap();
        st.transmit(&tx(&["R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K"]))
            .unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!(r.sent.len(), 2);
        assert!(!r.keyer_busy());
    }

    #[test]
    fn waits_for_the_keyer_when_the_radio_drops_out_between_words() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // A break-in delay shorter than a word gap (as if the radio's setting were
        // wrong): the radio reads receive between words.
        st.rig().lock().unwrap().break_in_delay_dots = 3.0;
        st.transmit(&tx(&[
            "A B C D E F G H I J K L M N O P Q R S T U V W X Y Z",
        ]))
        .unwrap();
        assert!(!st.rig().lock().unwrap().keyer_busy());
    }

    #[test]
    fn keying_run_stays_watched_until_receive() {
        // Keying ends long after send_cw returns, and 1.5 s (simulated) after the
        // station's own estimate of its end; the watchdog must still see it. At 10x
        // the SWR check has about 200 ms of key-down to sample, so a busy machine
        // does not turn this into a missed SWR reading.
        let mut rig = SimRig::new();
        rig.time_scale = 10.0;
        rig.tx_on_delay = Duration::from_millis(1500);
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        let since = st.keying_since.clone();
        let rig = st.rig();
        let watcher = thread::spawn(move || {
            let mut unwatched = 0;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                {
                    let mut r = rig.lock().unwrap();
                    if r.keyer_busy() && since.lock().unwrap().is_none() {
                        unwatched += 1;
                    }
                    if !r.sent.is_empty() && !r.keyer_busy() && !r.is_transmitting().unwrap() {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            unwatched
        });
        st.transmit(&tx(&["TEST TEST"])).unwrap();
        assert_eq!(watcher.join().unwrap(), 0);
    }

    #[test]
    fn high_swr_locks_out_until_next_window() {
        let mut st = Station::new(
            {
                let mut r = fast_rig();
                r.swr = 3.5;
                r
            },
            cfg(),
            None,
        );
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert!(
            !st.rig().lock().unwrap().is_transmitting().unwrap(),
            "back on receive"
        );
        st.rig().lock().unwrap().swr = 1.2;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn high_swr_is_caught_despite_slow_switch_on() {
        // The first moments after send_cw are still key-up (SWR meter 1.0).
        let mut rig = fast_rig();
        rig.swr = 3.5;
        rig.tx_on_delay = Duration::from_millis(1000);
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
    }

    #[test]
    fn a_tuner_that_cannot_match_locks_out_the_window() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.rig().lock().unwrap().tuner_bypassed = true;
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert!(st.rig().lock().unwrap().sent.is_empty(), "nothing keyed");
        st.rig().lock().unwrap().tuner_bypassed = false;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn a_radio_whose_status_misses_keying_is_not_trusted() {
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(st.tx_inhibited());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert_eq!(st.rig().lock().unwrap().sim.sent.len(), 1);
    }

    #[test]
    fn a_tune_that_errors_forces_receive() {
        let mut rig = Radio::new(fast_rig());
        rig.tune_reply_lost = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert!(st.start_window().is_err());
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert!(r.stops > 0, "receive forced");
        assert!(!r.is_transmitting().unwrap());
    }

    #[test]
    fn swr_is_checked_only_with_output_present() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // No sample reaches the Po threshold: not checked, and treated as a fault.
        st.cfg.swr_min_po = 1000.0;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::NoOutput));
        assert!(!st.swr_checked);
        st.cfg.swr_min_po = 10.0;
        st.start_window().unwrap();
        st.rig().lock().unwrap().swr = 3.5;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
    }

    #[test]
    fn each_window_sets_the_radio_up_again() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        // Someone at the front panel between windows.
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.frequency_hz = 14_074_000;
            r.power_watts = 100;
            r.cw_mode = false;
        }
        st.start_window().unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!(
            (r.frequency_hz, r.power_watts, r.cw_mode),
            (7_030_000, 40, true)
        );
        assert_eq!(r.tunes, 2);
    }

    #[test]
    fn a_radio_on_transmit_at_window_start_is_not_tuned() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // Something other than the node has put it on transmit.
        st.rig().lock().unwrap().set_transmit(true).unwrap();
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert_eq!(r.tunes, 0);
        assert!(!r.is_transmitting().unwrap(), "receive forced");
    }

    #[test]
    fn a_radio_left_in_split_is_not_tuned() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        // Someone at the radio switches split on between windows.
        st.rig().lock().unwrap().split_tx_hz = Some(7_040_000);
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert_eq!(st.rig().lock().unwrap().tunes, 1);
        // Back off, the next window works again.
        st.rig().lock().unwrap().split_tx_hz = None;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn swr_is_checked_on_every_transmission() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        // The antenna fails between two transmissions in the same window.
        st.rig().lock().unwrap().swr = 3.5;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
    }

    #[test]
    fn an_inhibit_survives_a_restart_until_its_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), Some(health.clone()));
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        drop(st);
        let file = dir.path().join(INHIBIT_FILE);
        let why = std::fs::read_to_string(&file).unwrap();
        assert!(why.contains("1C 00"), "{why}");
        // A restart, with a radio that now behaves: still nothing is transmitted.
        let mut st = Station::new(fast_rig(), cfg(), Some(health.clone()));
        st.configure().unwrap();
        assert!(st.tx_inhibited());
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        {
            let rig = st.rig();
            let r = rig.lock().unwrap();
            assert!(r.sent.is_empty());
            assert_eq!(r.tunes, 0);
        }
        drop(st);
        std::fs::remove_file(&file).unwrap();
        let mut st = Station::new(fast_rig(), cfg(), Some(health));
        st.configure().unwrap();
        assert!(!st.tx_inhibited());
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    /// A [`SimRig`] with two things a real radio may do: fold its output back to a
    /// fifth when the SWR is above 3:1 ("Power down transmission", p. 13-4), and
    /// stay on transmit for the break-in delay after being told to stop.
    struct Radio {
        sim: SimRig,
        foldback: bool,
        hang_after_stop: bool,
        hang_until: Option<Instant>,
        /// Time the next stop command spends before reaching the radio, as the
        /// driver's read-until-quiet after a CI-V timeout does.
        stop_lag: Option<Duration>,
        /// The transmit status reads receive whatever the radio is doing.
        status_blind: bool,
        /// The tuner starts, but the reply to the command is lost.
        tune_reply_lost: bool,
        /// Stop-CW commands received.
        stops: u32,
    }

    impl Radio {
        fn new(sim: SimRig) -> Self {
            Self {
                sim,
                foldback: false,
                hang_after_stop: false,
                hang_until: None,
                stop_lag: None,
                status_blind: false,
                tune_reply_lost: false,
                stops: 0,
            }
        }

        fn hang(&mut self) -> civ::Result<()> {
            if self.hang_after_stop && self.hang_until.is_none() && self.sim.is_transmitting()? {
                let hang = self
                    .sim
                    .dot_duration()?
                    .mul_f32(self.sim.break_in_delay_dots);
                self.hang_until = Some(Instant::now() + hang);
            }
            Ok(())
        }
    }

    impl Rig for Radio {
        fn frequency(&mut self) -> civ::Result<u64> {
            self.sim.frequency()
        }
        fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
            self.sim.set_frequency(hz)
        }
        fn set_mode_cw(&mut self) -> civ::Result<()> {
            self.sim.set_mode_cw()
        }
        fn set_rf_power_watts(&mut self, watts: u32) -> civ::Result<()> {
            self.sim.set_rf_power_watts(watts)
        }
        fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
            self.sim.set_key_speed(wpm)
        }
        fn set_break_in(&mut self, on: bool) -> civ::Result<()> {
            self.sim.set_break_in(on)
        }
        fn set_break_in_delay(&mut self, dots: f32) -> civ::Result<()> {
            self.sim.set_break_in_delay(dots)
        }
        fn dot_duration(&mut self) -> civ::Result<Duration> {
            self.sim.dot_duration()
        }
        fn start_tune(&mut self) -> civ::Result<()> {
            self.sim.start_tune()?;
            if self.tune_reply_lost {
                return Err(RigError::Timeout);
            }
            Ok(())
        }
        fn tuner_busy(&mut self) -> civ::Result<bool> {
            self.sim.tuner_busy()
        }
        fn tuner_matched(&mut self) -> civ::Result<bool> {
            self.sim.tuner_matched()
        }
        fn read_swr(&mut self) -> civ::Result<f32> {
            self.sim.read_swr()
        }
        fn read_po(&mut self) -> civ::Result<f32> {
            let po = self.sim.read_po()?;
            Ok(if self.foldback && self.sim.swr > 3.0 {
                po / 5.0
            } else {
                po
            })
        }
        fn send_cw(&mut self, text: &str) -> civ::Result<()> {
            self.hang_until = None;
            self.sim.send_cw(text)
        }
        fn stop_cw(&mut self) -> civ::Result<()> {
            self.stops += 1;
            if let Some(lag) = self.stop_lag.take() {
                thread::sleep(lag);
            }
            self.hang()?;
            self.sim.stop_cw()
        }
        fn is_transmitting(&mut self) -> civ::Result<bool> {
            if self.status_blind {
                return Ok(false);
            }
            let hanging = self.hang_until.is_some_and(|t| Instant::now() < t);
            Ok(hanging || self.sim.is_transmitting()?)
        }
        fn set_transmit(&mut self, tx: bool) -> civ::Result<()> {
            if !tx {
                self.hang()?;
            }
            self.sim.set_transmit(tx)
        }
    }

    #[test]
    fn output_cut_back_by_a_bad_load_stops_keying() {
        // 10:1 SWR: the radio's output drops to 8 W, below the 10 W threshold, so no
        // SWR sample ever counts.
        let mut rig = Radio::new(fast_rig());
        rig.foldback = true;
        rig.sim.swr = 10.0;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        let long = tx(&["R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K"]);
        assert_eq!(st.transmit(&long), Err(TxError::NoOutput));
        assert_eq!(st.transmit(&long), Err(TxError::SwrLockout));
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert_eq!(r.sim.sent.len(), 1, "stopped after the first piece");
        assert!(!r.is_transmitting().unwrap());
    }

    #[test]
    fn forced_receive_waits_out_the_break_in_delay() {
        // Real time: the radio holds transmit for 10 dots (600 ms at 20 wpm) after
        // the high-SWR cut-off, longer than three quick status checks.
        let mut rig = Radio::new(SimRig::new());
        rig.hang_after_stop = true;
        rig.sim.swr = 3.5;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert!(!st.tx_inhibited());
        assert!(!st.rig().lock().unwrap().is_transmitting().unwrap());
    }

    #[test]
    fn forced_receive_waits_out_the_break_in_delay_after_a_slow_stop() {
        // Real time: the stop reaches the radio 700 ms late (the driver draining the
        // link after a timeout), and only then does the 600 ms break-in delay start.
        let mut rig = Radio::new(SimRig::new());
        rig.hang_after_stop = true;
        let st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.rig().lock().unwrap().sim.set_transmit(true).unwrap();
        st.rig().lock().unwrap().stop_lag = Some(Duration::from_millis(700));
        assert_eq!(st.force_rx(), Ok(()));
        assert!(!st.tx_inhibited());
        assert!(!st.rig().lock().unwrap().is_transmitting().unwrap());
    }

    #[test]
    fn stuck_transmitter_is_forced_to_receive() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.rig().lock().unwrap().stuck_key = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Stuck));
        assert!(!st.rig().lock().unwrap().is_transmitting().unwrap());
        assert!(!st.tx_inhibited());
    }

    #[test]
    fn receive_is_forced_even_if_stop_cw_fails() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.stuck_key = true;
            r.stop_cw_fails = true;
        }
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Stuck));
        assert!(!st.rig().lock().unwrap().is_transmitting().unwrap());
        assert!(!st.tx_inhibited());
    }

    /// A radio switched off: no command gets a reply. Wraps a SimRig so it can be
    /// switched back on.
    struct Switchable {
        on: bool,
        rig: SimRig,
    }

    impl Switchable {
        fn rig(&mut self) -> civ::Result<&mut SimRig> {
            match self.on {
                true => Ok(&mut self.rig),
                false => Err(RigError::Timeout),
            }
        }
    }

    impl Rig for Switchable {
        fn frequency(&mut self) -> civ::Result<u64> {
            self.rig()?.frequency()
        }
        fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
            self.rig()?.set_frequency(hz)
        }
        fn set_mode_cw(&mut self) -> civ::Result<()> {
            self.rig()?.set_mode_cw()
        }
        fn set_rf_power_watts(&mut self, watts: u32) -> civ::Result<()> {
            self.rig()?.set_rf_power_watts(watts)
        }
        fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
            self.rig()?.set_key_speed(wpm)
        }
        fn set_break_in(&mut self, on: bool) -> civ::Result<()> {
            self.rig()?.set_break_in(on)
        }
        fn set_break_in_delay(&mut self, dots: f32) -> civ::Result<()> {
            self.rig()?.set_break_in_delay(dots)
        }
        fn dot_duration(&mut self) -> civ::Result<Duration> {
            self.rig()?.dot_duration()
        }
        fn start_tune(&mut self) -> civ::Result<()> {
            self.rig()?.start_tune()
        }
        fn tuner_busy(&mut self) -> civ::Result<bool> {
            self.rig()?.tuner_busy()
        }
        fn read_swr(&mut self) -> civ::Result<f32> {
            self.rig()?.read_swr()
        }
        fn read_po(&mut self) -> civ::Result<f32> {
            self.rig()?.read_po()
        }
        fn send_cw(&mut self, text: &str) -> civ::Result<()> {
            self.rig()?.send_cw(text)
        }
        fn stop_cw(&mut self) -> civ::Result<()> {
            self.rig()?.stop_cw()
        }
        fn is_transmitting(&mut self) -> civ::Result<bool> {
            self.rig()?.is_transmitting()
        }
        fn set_transmit(&mut self, tx: bool) -> civ::Result<()> {
            self.rig()?.set_transmit(tx)
        }
    }

    #[test]
    fn a_radio_off_at_window_start_inhibits_transmitting() {
        let rig = Switchable {
            on: true,
            rig: fast_rig(),
        };
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        st.rig().lock().unwrap().on = false;
        assert!(st.start_window().is_err());
        assert!(st.tx_inhibited());
        // Switched back on, it still keys nothing and does not tune.
        st.rig().lock().unwrap().on = true;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(st.start_window().is_err());
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.rig.tunes, r.rig.sent.len()), (1, 0));
    }

    #[test]
    fn unconfirmed_receive_inhibits_transmit() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // Stuck on transmit before anything is keyed: nothing is sent, and forcing
        // receive fails.
        st.rig().lock().unwrap().tx_jammed = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Inhibited));
        assert!(st.tx_inhibited());
        assert!(
            st.keying_since.lock().unwrap().is_some(),
            "watchdog retries"
        );
        // Even once the radio recovers, nothing more is keyed.
        st.rig().lock().unwrap().tx_jammed = false;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Inhibited));
        assert!(st.start_window().is_err());
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert!(r.sent.is_empty());
        assert_eq!(r.tunes, 0);
    }

    #[test]
    fn watchdog_keeps_trying_until_receive_is_confirmed() {
        let mut c = cfg();
        c.max_key = Duration::from_millis(1);
        let st = Station::new(fast_rig(), c, None);
        st.rig().lock().unwrap().tx_jammed = true;
        *st.keying_since.lock().unwrap() = Some(Instant::now());
        thread::sleep(Duration::from_millis(1500));
        assert!(st.tx_inhibited());
        assert!(st.keying_since.lock().unwrap().is_some());
        st.rig().lock().unwrap().tx_jammed = false;
        let t0 = Instant::now();
        while st.keying_since.lock().unwrap().is_some() {
            assert!(t0.elapsed() < Duration::from_secs(5), "watchdog gave up");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(st.tx_inhibited(), "inhibit stays latched");
    }

    #[test]
    fn drop_forces_receive_even_if_stop_cw_fails() {
        let st = Station::new(fast_rig(), cfg(), None);
        let rig = st.rig();
        {
            let mut r = rig.lock().unwrap();
            r.set_transmit(true).unwrap();
            r.stop_cw_fails = true;
        }
        drop(st);
        assert!(!rig.lock().unwrap().is_transmitting().unwrap());
    }

    #[test]
    fn health_log_records_tune_and_swr() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("health.csv");
        let mut st = Station::new(fast_rig(), cfg(), Some(log.clone()));
        st.configure().unwrap();
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        assert!(
            text.contains(",tune,") && text.contains(",swr,1.30"),
            "{text}"
        );
    }

    #[test]
    fn a_storm_hold_stops_tuning_and_keying_until_lifted() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("health.csv");
        let mut st = Station::new(fast_rig(), cfg(), Some(log.clone()));
        st.configure().unwrap();
        // On until the first check clears it.
        let hold = StormHold::new(Duration::from_secs(60));
        st.set_storm_hold(hold.clone());
        {
            st.rig().lock().unwrap().frequency_hz = 14_074_000;
        }
        let err = st.start_window().unwrap_err().to_string();
        assert!(err.contains("storm stand-down"), "{err}");
        assert_eq!(
            st.transmit(&tx(&["TEST"])),
            Err(TxError::Storm("no storm check yet".into()))
        );
        {
            let rig = st.rig();
            let r = rig.lock().unwrap();
            // Set up again, but not tuned and nothing keyed.
            assert_eq!(r.frequency_hz, 7_030_000);
            assert_eq!((r.tunes, r.sent.len()), (0, 0));
        }
        assert!(!st.tx_inhibited());
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .contains(",tune,storm"));
        // Lifted: the next transmission goes out, SWR-checked as always.
        hold.set(None);
        st.transmit(&tx(&["TEST"])).unwrap();
        st.start_window().unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.tunes, r.sent.join(" ")), (1, "TEST".to_string()));
    }

    #[test]
    fn a_storm_hold_stops_a_transmission_under_way() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        let hold = StormHold::new(Duration::from_secs(60));
        hold.set(None);
        st.set_storm_hold(hold.clone());
        st.start_window().unwrap();
        let long: Vec<String> = (0..8)
            .map(|i| format!("PART {i} OF A LONG REPLY THAT KEEPS THE KEYER BUSY"))
            .collect();
        let segments: Vec<&str> = long.iter().map(String::as_str).collect();
        let setter = {
            let hold = hold.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                hold.set(Some("alert: Severe Thunderstorm Warning".into()));
            })
        };
        let t0 = Instant::now();
        assert_eq!(
            st.transmit(&tx(&segments)),
            Err(TxError::Storm("alert: Severe Thunderstorm Warning".into()))
        );
        setter.join().unwrap();
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        // Stopped part-way, within a poll or two, and back on receive.
        assert!(
            t0.elapsed() < Duration::from_millis(400),
            "{:?}",
            t0.elapsed()
        );
        assert!(!r.sent.is_empty() && r.sent.len() < 16, "{:?}", r.sent);
        assert!(!r.keyer_busy() && !r.is_transmitting().unwrap());
        assert!(!st.tx_inhibited());
    }
}
