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
//!   to [`INHIBIT_FILE`] there, so a restart does not clear it (systemd or the
//!   start-up scripts in `deploy/` restart the node after a crash); only removing
//!   the file, once the radio has been checked, does. Whoever registered with
//!   [`Station::notify_inhibit`] gets one [`InhibitNotice`] when it latches, or at
//!   once if it already has (the file was there at start-up): `hfnode run` emails
//!   it to the owner (see `alert`).
//! - **Software watchdog.** A separate thread forces the radio back to receive if
//!   any one keying run lasts longer than `max_key_seconds`, and keeps trying until
//!   receive is confirmed. It backs up, and does not replace, the hardware transmit
//!   timer (docs/hardware-test-plan.md, step 10), which must act on CI-V keying.
//! - **SWR check.** SWR is sampled repeatedly during the first second or so of
//!   each transmission, counting only samples taken with the Po meter showing
//!   output, and the highest is used; above the limit the node stops and stays
//!   silent until it next tunes. If the first piece is keyed without one such
//!   sample, the node also stops and stays silent: the radio's own protection cuts
//!   its output into a bad load, so missing output is itself a sign of one.
//!   Each SWR sample also reads the transmit status: if the radio reads receive
//!   while the Po meter shows output, its status cannot be trusted for the checks
//!   above, and transmitting is inhibited as above.
//! - **The radio set up again before every transmission**: the settings are sent
//!   again, and split, ∂TX and the transmit frequency are checked, since the front
//!   panel, another program or a power cycle may have changed them. The node also
//!   does this every few minutes while it listens, without transmitting.
//! - **Reduced power**, set at start-up and with the other settings.
//! - **Tuning** when the node starts listening (at start-up, or at the top of each
//!   listening window), and before a reply once the last tune is older than
//!   `schedule.retune_minutes`; `hfnode radio tune` tunes once. In `hfnode run` a
//!   tune that matched when it starts listening is followed by `DE <call>`
//!   ([`Station::open_window`]), so its carrier is identified; a tune before a
//!   reply is identified by the reply that follows it.
//! - **Identification** (47 CFR 97.119(a)). Every transmission the session builds
//!   ends with `DE <call> K`; inside a long one this layer keys `DE <call>` on its
//!   own between chunks (or before the first, after a long over from the field), so
//!   that no more than [`ID_INTERVAL`] passes from the node's last ID to its next.
//!   An ID 10 minutes old or more no longer counts: then the time runs from the
//!   start of the transmission.
//! - **A health log** of every tune and SWR reading, so a slow upward trend (a
//!   corroding connector, a loosened coil) shows up before it becomes a fault.
//! - **Rigs without a tuner or meters** (an FM handheld, [`crate::handheld`]): a
//!   window start sets the radio up and checks it but tunes nothing, and SWR is not
//!   checked; such a rig enforces its own limits (a PTT time limit, a duty cycle,
//!   a clear channel), which it reports through [`Rig::rest_needed`] and which are
//!   waited out here, on receive, before each keying run. An ID that such a rest
//!   would make late is keyed before it.
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
use std::sync::mpsc::Sender;
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
    /// `DE <node_call>`, keyed on its own after the tune when the node starts listening, and inside long
    /// transmissions.
    pub station_id: String,
    /// Most time from the node's last ID (or the start of a transmission, if that ID
    /// is a quarter more than this old) to the end of the next ID: [`ID_INTERVAL`]
    /// (divided by the time scale in tests).
    pub id_interval: Duration,
}

/// Longest stretch without the node's callsign, from its last ID (the previous
/// over's `DE <call> K`, or an ID inside it) to the end of the next: well inside
/// the 10 minutes of 47 CFR 97.119(a). Measured from the start of a transmission
/// instead once the last ID is 10 minutes old, from an earlier exchange.
pub const ID_INTERVAL: Duration = Duration::from_secs(8 * 60);

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
            station_id: format!("DE {}", c.node_call.to_ascii_uppercase()),
            id_interval: ID_INTERVAL,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TxError {
    /// Since the last tune, SWR was too high, the tuner could not match, or the
    /// radio could not be set up for the tune; transmitting is suspended until the
    /// next one.
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
    /// The radio could not be set up again before keying, or would not transmit on
    /// the configured frequency (split or ∂TX on); nothing was keyed.
    NotReady(String),
    /// Thunder near the station (or no storm check to say otherwise): nothing is
    /// keyed, and a transmission under way was stopped.
    Storm(String),
    Rig(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwrLockout => write!(
                f,
                "transmit locked out until the node next tunes (high SWR, no tuner \
                 match, or the radio could not be set up)"
            ),
            Self::HighSwr(s) => write!(f, "SWR {s:.1} above limit"),
            Self::NoOutput => write!(f, "no output while keying: SWR not measured"),
            Self::Stuck => write!(f, "transmitter did not return to receive"),
            Self::Inhibited => write!(
                f,
                "radio not confirmed on receive: transmit inhibited until the node is \
                 restarted with {INHIBIT_FILE} removed from the state directory"
            ),
            Self::NotReady(e) => write!(f, "radio not ready to transmit: {e}"),
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

/// Transmitting has been inhibited: why and when, for telling the owner.
#[derive(Debug, Clone, PartialEq)]
pub struct InhibitNotice {
    /// Unix time it latched, as written in [`INHIBIT_FILE`]; `None` if a file left
    /// from before does not start with one (for example, written by hand).
    pub at: Option<u64>,
    /// Why, as logged and written in the file.
    pub reason: String,
    /// It was already in [`INHIBIT_FILE`] when this process started.
    pub from_file: bool,
    /// The file that keeps it across restarts; `None` without a state directory, or
    /// if it could not be written (a restart then clears the inhibit).
    pub file: Option<PathBuf>,
}

/// `<unix time> <reason>`, as [`Inhibit::latch`] writes it, as (time, reason); any
/// other text is all reason.
fn parse_inhibit_file(text: &str) -> (Option<u64>, String) {
    let text = text.trim();
    match text.split_once(' ').map(|(t, why)| (t.parse::<u64>(), why)) {
        Some((Ok(t), why)) => (Some(t), why.trim().to_string()),
        _ => (None, text.to_string()),
    }
}

/// The latch that stops all transmitting, in memory and in [`INHIBIT_FILE`].
struct Inhibit {
    set: AtomicBool,
    file: Option<PathBuf>,
    /// What latched it and who to tell, under one lock so that each registration
    /// hears of it exactly once, whichever comes first.
    notice: Mutex<Notify>,
}

struct Notify {
    latched: Option<InhibitNotice>,
    to: Option<Sender<InhibitNotice>>,
}

impl Inhibit {
    fn new(file: Option<PathBuf>) -> Self {
        let on_disk = file.as_deref().filter(|f| f.exists());
        let mut latched = None;
        if let Some(f) = on_disk {
            let why = std::fs::read_to_string(f).unwrap_or_default();
            log::error!(
                "transmit inhibited by {} ({}): once the radio has been checked, stop the node, \
                 remove the file and start it again",
                f.display(),
                why.trim()
            );
            let (at, reason) = parse_inhibit_file(&why);
            latched = Some(InhibitNotice {
                at,
                reason,
                from_file: true,
                file: Some(f.to_path_buf()),
            });
        }
        Self {
            set: AtomicBool::new(on_disk.is_some()),
            file,
            notice: Mutex::new(Notify { latched, to: None }),
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
        let at = crate::gateway::unix_now();
        let mut written = None;
        if let Some(f) = &self.file {
            // The state directory may not exist yet (a bench command run before the
            // node ever has); the inhibit must still reach the disk.
            let saved = f
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(f, format!("{at} {why}\n")));
            match saved {
                Ok(()) => {
                    log::error!(
                        "wrote {}: nothing is transmitted, also after a restart, until it is removed \
                         with the node stopped",
                        f.display()
                    );
                    written = Some(f.clone());
                }
                Err(e) => log::error!("cannot write {}: {e}", f.display()),
            }
        }
        let notice = InhibitNotice {
            at: Some(at),
            reason: why.to_string(),
            from_file: false,
            file: written,
        };
        // Only a channel send here: this can run on the watchdog thread, with the
        // radio locked.
        let mut n = self.notice.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(to) = &n.to {
            let _ = to.send(notice.clone());
        }
        n.latched = Some(notice);
    }

    fn notify(&self, to: Sender<InhibitNotice>) {
        let mut n = self.notice.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(latched) = &n.latched {
            let _ = to.send(latched.clone());
        }
        n.to = Some(to);
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
    /// The last [`Station::start_window`] got as far as starting the tuner.
    tuner_ran: bool,
    /// SWR has been measured on the current transmission.
    swr_checked: bool,
    /// When the node last identified: the start of the last piece of a
    /// transmission that ended with its ID, or of an ID keyed inside one.
    last_id: Option<Instant>,
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
            tuner_ran: false,
            swr_checked: false,
            last_id: None,
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

    /// Whether a transmission could be keyed now: transmitting is not inhibited, and
    /// not locked out until the next tune (high SWR, no output, a tuner that could
    /// not match, or a radio that could not be set up for the last tune). Reads no
    /// CI-V.
    pub fn can_transmit(&self) -> bool {
        !self.tx_inhibited() && !self.swr_lockout
    }

    /// Send one [`InhibitNotice`] to `to` when transmitting is inhibited: now if it
    /// already is (by [`INHIBIT_FILE`] at start-up, or an earlier latch), otherwise
    /// when it latches, from whichever thread latches it. Registering again replaces
    /// `to` (and tells the new one of an inhibit already latched).
    pub fn notify_inhibit(&self, to: Sender<InhibitNotice>) {
        self.tx_inhibit.notify(to);
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

    /// The radio is on receive, set up as configured, and would transmit on the
    /// configured frequency. It should be on receive already; if it is not,
    /// something else is keying it. Split and ∂TX are not set by the node, so they
    /// are only checked.
    fn prepare(&self) -> civ::Result<()> {
        self.with_rig(|r| r.is_transmitting())
            .and_then(|tx| match tx {
                false => self.apply_settings(),
                true => Err(RigError::Protocol(
                    "on transmit without the node keying it".into(),
                )),
            })
            .and_then(|()| self.check_transmit_frequency())
    }

    /// Set the radio up again and check it, as at a window start but without the
    /// tune: while the node listens for hours, the front panel, another program or a
    /// power cycle may change it, and leave the node deaf on another frequency or
    /// mode. Transmits nothing. If it fails, receive is forced, and transmitting is
    /// inhibited if receive cannot be confirmed, as anywhere else; otherwise
    /// transmitting is not locked out, since every transmission sets the radio up
    /// and checks it again first.
    pub fn check(&self) -> civ::Result<()> {
        if let Err(e) = self.prepare() {
            self.health("check", "failed");
            log::error!("could not set the radio up ({e}): checked again before transmitting");
            self.force_rx()
                .map_err(|e| RigError::Protocol(e.to_string()))?;
            return Err(e);
        }
        Ok(())
    }

    /// Set the radio up again and run the internal tuner. `hfnode run` calls it
    /// through [`Station::open_window`] when it starts listening (at start-up, or
    /// at the top of each listening window), and on its own before a reply when the
    /// last tune is too old to trust; `hfnode radio tune` calls it once. Clears any
    /// SWR lockout from before, and locks out transmitting until the next call if
    /// the radio could not be set up or the tuner could not match.
    pub fn start_window(&mut self) -> civ::Result<()> {
        self.tuner_ran = false;
        if self.tx_inhibited() {
            // Tuning transmits.
            return Err(RigError::Protocol(TxError::Inhibited.to_string()));
        }
        self.swr_lockout = false;
        self.swr_checked = false;
        // Set the radio up again: the front panel, another program or a power cycle
        // may have changed it since the last window, and the tune transmits.
        if let Err(e) = self.prepare() {
            self.swr_lockout = true;
            log::error!("could not set the radio up ({e}): silent until the next tune");
            self.force_rx()
                .map_err(|e| RigError::Protocol(e.to_string()))?;
            return Err(e);
        }
        if let Some(why) = self.storm_reason() {
            // Tuning transmits. Not a lockout, and the tuner has not run, so the
            // node tunes again before its first reply once the stand-down ends.
            self.health("tune", "storm");
            log::warn!("storm stand-down, not tuning: {why}");
            return Err(RigError::Protocol(format!("storm stand-down: {why}")));
        }
        let t0 = Instant::now();
        self.tuner_ran = true;
        if !self
            .rig
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_tuner()
        {
            // Nothing to tune (a handheld): set up and checked is all a window start
            // needs, and nothing is transmitted.
            return Ok(());
        }
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
            log::error!("tuner could not match the antenna: silent until the next tune");
            return Err(RigError::Protocol("tuner could not match the load".into()));
        }
        self.health("tune", &format!("{}ms", t0.elapsed().as_millis()));
        Ok(())
    }

    /// Whether the last [`Station::start_window`] started the tuner, whatever came
    /// of it; one that stopped before (inhibited, or the radio could not be set up)
    /// did not, and is worth trying again before the next reply.
    pub fn tuner_ran(&self) -> bool {
        self.tuner_ran
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

    /// Start listening as `hfnode run` does, at start-up or at the top of a
    /// listening window: [`Station::start_window`] (which also checks the transmit
    /// frequency and split), then, only if the tune
    /// matched (no lockout, no inhibit), `DE <call>` to identify its carrier, keyed
    /// as a transmission of its own with every check of [`Station::transmit`],
    /// the SWR check included. The bench's `radio tune` uses `start_window` alone.
    /// A rig without a tuner (a handheld) keyed nothing, so it sends no ID either.
    pub fn open_window(&mut self) -> Result<(), String> {
        self.start_window()
            .map_err(|e| format!("tune failed at window start: {e}"))?;
        if !self
            .rig
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_tuner()
        {
            return Ok(());
        }
        let id = Transmission {
            segments: vec![self.cfg.station_id.clone()],
            read_ids: Vec::new(),
        };
        self.transmit(&id)
            .map_err(|e| format!("station ID after the tune failed: {e}"))
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
        // Set the radio up again and check it before keying anything: the front
        // panel, another program or a power cycle may have changed it since the
        // window started, which for a node listening all the time can be hours ago.
        self.wait_for_receive(Instant::now() + Duration::from_secs(2))?;
        self.apply_settings()
            .and_then(|()| self.check_transmit_frequency())
            .map_err(|e| TxError::NotReady(e.to_string()))?;
        // Keyer-sized like any text: 17 takes "Up to 30 characters" (manual text
        // line 9711), and node_call is not limited in length.
        let id = split_for_keyer(&self.cfg.station_id);
        // The node's last ID if it is recent enough to be part of this exchange (the
        // last over's `DE <call> K`: the field operator's over since may have been
        // long, and the read-back of it as long again), and then an ID may be due
        // before the first chunk too; otherwise the start of the transmission. Then
        // the start of each ID keyed in it.
        let carried = self.last_id.filter(|t| t.elapsed() < self.id_carry());
        let mut since_id = carried.unwrap_or_else(Instant::now);
        for (si, segment) in tx.segments.iter().enumerate() {
            if si > 0 {
                thread::sleep(self.cfg.segment_pause);
            }
            let pieces = split_for_keyer(segment);
            let ends_with_id = si + 1 == tx.segments.len() && self.ends_with_id(segment);
            for (pi, piece) in pieces.iter().enumerate() {
                if si > 0 || pi > 0 || carried.is_some() {
                    // At a chunk boundary look ahead over the whole chunk, so the ID
                    // falls between chunks; inside one too long for that, over the
                    // next keyer piece.
                    let (ahead, ends) = if pi == 0 {
                        (&pieces[..], ends_with_id)
                    } else {
                        (&pieces[pi..=pi], ends_with_id && pi + 1 == pieces.len())
                    };
                    if self.id_due_after_rest(since_id, piece, ahead, ends, &id)? {
                        if pi > 0 {
                            thread::sleep(self.cfg.segment_pause);
                        }
                        log::info!("station ID inside a long transmission");
                        since_id = Instant::now();
                        for p in &id {
                            self.key_piece(p)?;
                        }
                        self.last_id = Some(since_id);
                        thread::sleep(self.cfg.segment_pause);
                        self.rest_for_piece_and_id(piece, &id)?;
                    }
                }
                let at = Instant::now();
                self.key_piece(piece)?;
                if ends_with_id && pi + 1 == pieces.len() {
                    self.last_id = Some(at);
                }
            }
        }
        Ok(())
    }

    /// Wait, on receive, for as long as the rig says it must rest before keying a
    /// run of `keying` ([`Rig::rest_needed`]: a handheld's duty cycle, or a busy
    /// channel), asking again after each wait. Not counted as keying by the
    /// watchdog, which only starts timing once the piece is sent.
    fn rest_before_keying(&self, keying: Duration) -> Result<(), TxError> {
        loop {
            // A storm may have come on during the rest.
            self.check_storm()?;
            let rest = self.with_rig(|r| r.rest_needed(keying))?;
            if rest.is_zero() {
                return Ok(());
            }
            log::info!(
                "waiting {:.1} s on receive before keying",
                rest.as_secs_f32()
            );
            self.sleep_until(Instant::now() + rest)?;
        }
    }

    /// Whether to key the ID `id` before `piece` ([`Station::id_due`] over `ahead`),
    /// for a rig that must rest on receive before keying ([`Rig::rest_needed`]: a
    /// handheld's duty cycle, a busy channel). It rests for the ID alone (normally
    /// nothing: [`Station::rest_for_piece_and_id`] before the last piece left room
    /// for it), then counts the rest that `piece` and an ID after it would need, so
    /// that a due ID goes first rather than wait out a rest only the piece needs.
    /// If the ID is not due it takes that rest, and weighs the ID again in case the
    /// rest ran long. With no rest needed this is `id_due`.
    fn id_due_after_rest(
        &self,
        since: Instant,
        piece: &str,
        ahead: &[String],
        ends_with_id: bool,
        id: &[String],
    ) -> Result<bool, TxError> {
        let (id_keying, both) = self.piece_and_id_keying(piece, id)?;
        self.rest_before_keying(id_keying)?;
        let rest = self.with_rig(|r| r.rest_needed(both))?;
        if rest.is_zero() {
            return self.id_due(since, Duration::ZERO, ahead, ends_with_id, id);
        }
        if self.id_due(since, rest, ahead, ends_with_id, id)? {
            return Ok(true);
        }
        self.rest_before_keying(both)?;
        self.id_due(since, Duration::ZERO, ahead, ends_with_id, id)
    }

    /// Rest as [`Station::rest_before_keying`] does, for keying `piece` and then the
    /// ID `id`, so that the next ID does not have to wait for a rest of its own.
    fn rest_for_piece_and_id(&self, piece: &str, id: &[String]) -> Result<(), TxError> {
        let (_, both) = self.piece_and_id_keying(piece, id)?;
        self.rest_before_keying(both)
    }

    /// How long the ID `id` keys, and `piece` with it.
    fn piece_and_id_keying(
        &self,
        piece: &str,
        id: &[String],
    ) -> Result<(Duration, Duration), TxError> {
        let dot = self.with_rig(|r| r.dot_duration())?;
        let id_keying = dot * id.iter().map(|p| cw::units(p)).sum::<u32>();
        Ok((id_keying, id_keying + dot * cw::units(piece)))
    }

    /// How long the node's last ID still counts for its next transmission: the 10
    /// minutes of 47 CFR 97.119(a) at the default [`ID_INTERVAL`] (and scaled with it
    /// in tests). An older one was in an earlier exchange.
    fn id_carry(&self) -> Duration {
        self.cfg.id_interval * 5 / 4
    }

    /// Whether `text` ends with the station ID as a whole word, as an over does
    /// (`DE <call> K`, or KN or SK).
    fn ends_with_id(&self, text: &str) -> bool {
        let t = text.trim_end();
        let t = ["K", "KN", "SK"]
            .iter()
            .find_map(|o| t.strip_suffix(o).filter(|r| r.ends_with(' ')))
            .map_or(t, str::trim_end);
        let id = self.cfg.station_id.as_str();
        t == id || t.strip_suffix(id).is_some_and(|r| r.ends_with(' '))
    }

    /// The longest `pieces` may keep the radio on transmit before this module cuts
    /// them off: their keying time at the radio's speed, the break-in delay and the
    /// stuck margin, each.
    fn keying_bound(&self, pieces: &[String], dot: Duration) -> Duration {
        let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
        pieces
            .iter()
            .map(|p| dot * cw::units(p) + hang + self.cfg.stuck_margin)
            .sum()
    }

    /// Whether to key the ID (`id`, in keyer pieces) before `ahead`: once `ahead`
    /// has gone out, after a `rest` on receive first, there must still be time for a
    /// pause and an ID within `id_interval` of `since`, unless `ahead` ends with the
    /// ID itself.
    fn id_due(
        &self,
        since: Instant,
        rest: Duration,
        ahead: &[String],
        ends_with_id: bool,
        id: &[String],
    ) -> Result<bool, TxError> {
        let dot = self.with_rig(|r| r.dot_duration())?;
        let mut need = rest + self.keying_bound(ahead, dot);
        if !ends_with_id {
            need += self.cfg.segment_pause + self.keying_bound(id, dot);
        }
        Ok(since.elapsed() + need > self.cfg.id_interval)
    }

    /// Key one keyer piece and wait for the radio to be back on receive.
    fn key_piece(&mut self, piece: &str) -> Result<(), TxError> {
        self.check_storm()?;
        self.wait_for_receive(Instant::now() + Duration::from_secs(2))?;
        // Timed at the speed the radio's keyer is really using.
        let dot = self.with_rig(|r| r.dot_duration())?;
        let keying = dot * cw::units(piece);
        let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
        self.rest_before_keying(keying)?;
        // A rig without meters (a handheld) cannot measure SWR or output: it has its
        // own limits instead, waited out just above.
        let meters = self
            .rig
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_meters();
        *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        self.with_rig(|r| r.send_cw(piece))?;
        let sent = Instant::now();

        if !self.swr_checked && meters {
            self.check_swr(sent, keying + hang)?;
        }
        // The radio's status says nothing about the keyer until the whole
        // piece has had time to go out (it reads receive before semi
        // break-in has switched over), so wait that long first.
        self.sleep_until(sent + keying)?;
        self.wait_for_receive(sent + keying + hang + self.cfg.stuck_margin)?;
        *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
                        "SWR {swr:.2} above {:.1}: silent until the next tune",
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
                log::error!("no output on the Po meter while keying: silent until the next tune");
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
    use std::sync::mpsc;

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
            station_id: "DE N0DE".into(),
            id_interval: ID_INTERVAL,
        }
    }

    fn fast_rig() -> SimRig {
        let mut r = SimRig::new();
        r.time_scale = 10.0;
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
        assert!(st.can_transmit());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert!(!st.can_transmit());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert!(
            !st.rig().lock().unwrap().is_transmitting().unwrap(),
            "back on receive"
        );
        st.rig().lock().unwrap().swr = 1.2;
        st.start_window().unwrap();
        assert!(st.can_transmit());
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
        assert!(!st.can_transmit());
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
    fn each_transmission_sets_the_radio_up_again() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        // Someone at the front panel after the window started.
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.frequency_hz = 14_074_000;
            r.power_watts = 100;
            r.cw_mode = false;
        }
        st.transmit(&tx(&["TEST"])).unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!(
            (r.frequency_hz, r.power_watts, r.cw_mode),
            (7_030_000, 40, true)
        );
        assert_eq!(r.tunes, 1, "no tune for a transmission");
    }

    #[test]
    fn nothing_is_keyed_while_split_is_on() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        // Someone at the radio switches split on after the window started.
        st.rig().lock().unwrap().split_tx_hz = Some(7_040_000);
        assert!(matches!(
            st.transmit(&tx(&["TEST"])),
            Err(TxError::NotReady(_))
        ));
        assert!(st.rig().lock().unwrap().sent.is_empty(), "nothing keyed");
        assert!(!st.tx_inhibited());
        // Not locked out: once split is off again the next transmission goes.
        st.rig().lock().unwrap().split_tx_hz = None;
        st.transmit(&tx(&["TEST"])).unwrap();
        assert_eq!(st.rig().lock().unwrap().sent, ["TEST"]);
    }

    #[test]
    fn nothing_is_keyed_while_the_radio_would_transmit_elsewhere() {
        let rig = Switchable {
            on: true,
            rig: fast_rig(),
            tx_hz: None,
        };
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        // Split and ∂TX read off, but the transmit frequency (1C 03) does not match.
        st.rig().lock().unwrap().tx_hz = Some(7_031_000);
        assert!(st.check().is_err());
        assert!(matches!(
            st.transmit(&tx(&["TEST"])),
            Err(TxError::NotReady(_))
        ));
        assert!(
            st.rig().lock().unwrap().rig.sent.is_empty(),
            "nothing keyed"
        );
        st.rig().lock().unwrap().tx_hz = None;
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn a_tune_that_never_started_is_not_counted() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        assert!(st.tuner_ran());
        // The radio cannot be set up for the tune: no tune, and locked out until
        // one runs.
        st.rig().lock().unwrap().split_tx_hz = Some(7_040_000);
        assert!(st.start_window().is_err());
        assert!(!st.tuner_ran());
        st.rig().lock().unwrap().split_tx_hz = None;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        st.start_window().unwrap();
        assert!(st.tuner_ran());
        st.transmit(&tx(&["TEST"])).unwrap();
        // A tuner that could not match ran all the same.
        st.rig().lock().unwrap().tuner_bypassed = true;
        assert!(st.start_window().is_err());
        assert!(st.tuner_ran());
        assert_eq!(st.rig().lock().unwrap().tunes, 3);
    }

    #[test]
    fn a_check_sets_the_radio_up_again_without_transmitting() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.frequency_hz = 14_074_000;
            r.power_watts = 100;
            r.cw_mode = false;
        }
        st.check().unwrap();
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            assert_eq!(
                (r.frequency_hz, r.power_watts, r.cw_mode),
                (7_030_000, 40, true)
            );
            assert_eq!((r.tunes, r.sent.len()), (1, 0));
            assert!(!r.is_transmitting().unwrap());
        }
        // A check that finds split on fails, and keeps nothing from transmitting
        // later: the transmission checks for itself.
        st.rig().lock().unwrap().split_tx_hz = Some(7_040_000);
        assert!(st.check().is_err());
        st.rig().lock().unwrap().split_tx_hz = None;
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn a_check_on_a_radio_switched_off_inhibits_transmitting() {
        let rig = Switchable {
            on: true,
            rig: fast_rig(),
            tx_hz: None,
        };
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        st.rig().lock().unwrap().on = false;
        assert!(st.check().is_err());
        assert!(st.tx_inhibited(), "as at a window start");
        st.rig().lock().unwrap().on = true;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
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
        let file = dir.path().join(INHIBIT_FILE);
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), Some(health.clone()));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        let latched = notices.try_recv().expect("notice when it latches");
        assert!(
            !latched.from_file && latched.reason.contains("1C 00"),
            "{latched:?}"
        );
        assert_eq!(latched.file.as_deref(), Some(file.as_path()));
        drop(st);
        assert!(notices.try_recv().is_err(), "once per latch");
        let why = std::fs::read_to_string(&file).unwrap();
        assert!(why.contains("1C 00"), "{why}");
        assert_eq!(
            why.split(' ').next(),
            latched.at.map(|t| t.to_string()).as_deref()
        );
        // A restart, with a radio that now behaves: still nothing is transmitted.
        let mut st = Station::new(fast_rig(), cfg(), Some(health.clone()));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        let at_start = notices.try_recv().expect("notice at start-up");
        assert_eq!(
            at_start,
            InhibitNotice {
                from_file: true,
                ..latched
            }
        );
        st.configure().unwrap();
        assert!(st.tx_inhibited());
        assert!(!st.can_transmit());
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(notices.try_recv().is_err(), "once per start");
        {
            let rig = st.rig();
            let r = rig.lock().unwrap();
            assert!(r.sent.is_empty());
            assert_eq!(r.tunes, 0);
        }
        drop(st);
        std::fs::remove_file(&file).unwrap();
        let mut st = Station::new(fast_rig(), cfg(), Some(health));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        assert!(!st.tx_inhibited());
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        drop(st);
        assert!(notices.try_recv().is_err(), "no inhibit, no notice");
    }

    #[test]
    fn a_notice_registered_after_the_latch_still_comes_once() {
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        let n = notices.try_recv().unwrap();
        assert!(!n.from_file && n.at.is_some() && n.file.is_none(), "{n:?}");
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(notices.try_recv().is_err());
    }

    #[test]
    fn lockouts_and_recovered_faults_send_no_notice() {
        let mut rig = Radio::new(fast_rig());
        rig.tune_reply_lost = true;
        let mut st = Station::new(rig, cfg(), None);
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        // A tuner error, high SWR and a stuck key: receive is confirmed each time.
        assert!(st.start_window().is_err());
        st.rig().lock().unwrap().sim.swr = 3.5;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.tune_reply_lost = false;
            r.sim.swr = 1.2;
            r.sim.stuck_key = true;
        }
        st.start_window().unwrap();
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Stuck));
        assert!(!st.tx_inhibited());
        drop(st);
        assert!(notices.try_recv().is_err());
    }

    #[test]
    fn inhibit_file_is_read_as_time_and_reason() {
        assert_eq!(
            parse_inhibit_file("1791120363 radio not confirmed on receive (no reply from radio)\n"),
            (
                Some(1_791_120_363),
                "radio not confirmed on receive (no reply from radio)".to_string()
            )
        );
        // Written by hand, or damaged.
        assert_eq!(
            parse_inhibit_file("checking the radio\n"),
            (None, "checking the radio".into())
        );
        assert_eq!(
            parse_inhibit_file("1791120363"),
            (None, "1791120363".into())
        );
        assert_eq!(parse_inhibit_file(""), (None, String::new()));
    }

    #[test]
    fn an_inhibit_is_written_even_without_a_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("not-yet/state");
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), Some(state.join("health.csv")));
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        drop(st);
        assert!(state.join(INHIBIT_FILE).exists());
        let st = Station::new(fast_rig(), cfg(), Some(state.join("health.csv")));
        assert!(st.tx_inhibited());
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
        /// The tuner reads "tuning" for ever.
        tuner_stuck: bool,
        /// When each keyer piece was accepted.
        sent_at: Vec<(Instant, String)>,
        /// Rests on receive it asks for, as a handheld's duty cycle does.
        rests: Option<Rests>,
    }

    /// After `every` keyer pieces other than the ID, anything longer than the ID
    /// (`id`, keying time) waits `rest` on receive, counted from when it is first
    /// asked for; the ID alone never waits.
    struct Rests {
        every: u32,
        id: Duration,
        rest: Duration,
        pieces: u32,
        until: Option<Instant>,
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
                tuner_stuck: false,
                sent_at: Vec::new(),
                rests: None,
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
            Ok(self.tuner_stuck || self.sim.tuner_busy()?)
        }
        fn tuner_matched(&mut self) -> civ::Result<bool> {
            self.sim.tuner_matched()
        }
        fn transmit_frequency(&mut self) -> civ::Result<u64> {
            self.sim.transmit_frequency()
        }
        fn split_or_delta_tx(&mut self) -> civ::Result<bool> {
            self.sim.split_or_delta_tx()
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
            self.sim.send_cw(text)?;
            self.sent_at.push((Instant::now(), text.to_string()));
            if let Some(r) = self.rests.as_mut().filter(|_| text != "DE N0DE") {
                r.pieces += 1;
            }
            Ok(())
        }
        fn rest_needed(&mut self, keying: Duration) -> civ::Result<Duration> {
            let Some(r) = self.rests.as_mut() else {
                return Ok(Duration::ZERO);
            };
            if keying <= r.id || r.pieces < r.every {
                return Ok(Duration::ZERO);
            }
            let now = Instant::now();
            let until = *r.until.get_or_insert(now + r.rest);
            if now >= until {
                (r.pieces, r.until) = (0, None);
                return Ok(Duration::ZERO);
            }
            Ok(until - now)
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
    /// switched back on. With `tx_hz`, its transmit frequency (1C 03) reads that
    /// although split and ∂TX are off.
    struct Switchable {
        on: bool,
        rig: SimRig,
        tx_hz: Option<u64>,
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
        fn transmit_frequency(&mut self) -> civ::Result<u64> {
            let tx_hz = self.tx_hz;
            let rig = self.rig()?;
            tx_hz.map_or_else(|| rig.transmit_frequency(), Ok)
        }
        fn split_or_delta_tx(&mut self) -> civ::Result<bool> {
            self.rig()?.split_or_delta_tx()
        }
    }

    #[test]
    fn a_radio_off_at_window_start_inhibits_transmitting() {
        let rig = Switchable {
            on: true,
            rig: fast_rig(),
            tx_hz: None,
        };
        let mut st = Station::new(rig, cfg(), None);
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        st.start_window().unwrap();
        st.rig().lock().unwrap().on = false;
        assert!(st.start_window().is_err());
        assert!(st.tx_inhibited());
        let n = notices.try_recv().unwrap();
        assert_eq!(
            n.reason,
            "radio not confirmed on receive (no reply from radio)"
        );
        // Switched back on, it still keys nothing and does not tune.
        st.rig().lock().unwrap().on = true;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(st.start_window().is_err());
        assert!(notices.try_recv().is_err(), "once per latch");
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.rig.tunes, r.rig.sent.len()), (1, 0));
    }

    #[test]
    fn unconfirmed_receive_inhibits_transmit() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        // Stuck on transmit before anything is keyed: nothing is sent, and forcing
        // receive fails.
        st.rig().lock().unwrap().tx_jammed = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Inhibited));
        assert!(st.tx_inhibited());
        assert_eq!(
            notices.try_recv().unwrap().reason,
            "radio not confirmed on receive (unexpected reply: radio still reports transmit)"
        );
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
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.rig().lock().unwrap().tx_jammed = true;
        *st.keying_since.lock().unwrap() = Some(Instant::now());
        // Latched on the watchdog's thread.
        let n = notices.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(n.reason.contains("still reports transmit"), "{n:?}");
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
        assert!(notices.try_recv().is_err(), "once per latch");
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
    fn open_window_identifies_a_matched_tune() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("health.csv");
        let mut st = Station::new(fast_rig(), cfg(), Some(log.clone()));
        st.configure().unwrap();
        st.open_window().unwrap();
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            assert_eq!((r.tunes, r.sent.clone()), (1, vec!["DE N0DE".to_string()]));
            assert!(!r.keyer_busy() && !r.is_transmitting().unwrap());
        }
        // The ID is SWR-checked like any transmission, right after the tune.
        let text = std::fs::read_to_string(&log).unwrap();
        let events: Vec<&str> = text.lines().map(|l| l.split(',').nth(1).unwrap()).collect();
        assert_eq!(events, ["tune", "swr"], "{text}");
        // The bench's `radio tune` (start_window alone) keys nothing.
        st.start_window().unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.tunes, r.sent.len()), (2, 1));
    }

    #[test]
    fn open_window_keys_nothing_unless_the_tune_matched() {
        let quick = || {
            let mut c = cfg();
            c.tune_timeout = Duration::from_millis(100);
            c
        };
        // Not civ::mock::Fault: what each case does to the test radio.
        type WindowFault = fn(&mut Radio);
        let cases: [(&str, WindowFault); 5] = [
            ("no match", |r: &mut Radio| r.sim.tuner_bypassed = true),
            ("on transmit", |r: &mut Radio| {
                r.sim.set_transmit(true).unwrap()
            }),
            ("split", |r: &mut Radio| r.sim.split_tx_hz = Some(7_040_000)),
            ("tune reply lost", |r: &mut Radio| r.tune_reply_lost = true),
            ("tune never ends", |r: &mut Radio| r.tuner_stuck = true),
        ];
        for (name, fault) in cases {
            let mut st = Station::new(Radio::new(fast_rig()), quick(), None);
            st.configure().unwrap();
            fault(&mut st.rig().lock().unwrap());
            assert!(st.open_window().is_err(), "{name}");
            let rig = st.rig();
            let r = rig.lock().unwrap();
            assert!(r.sim.sent.is_empty(), "{name}: keyed {:?}", r.sim.sent);
        }
        // Inhibited: no tune and no ID.
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        st.rig().lock().unwrap().status_blind = false;
        assert!(st.open_window().is_err());
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.sim.tunes, r.sim.sent.len()), (0, 1));
    }

    #[test]
    fn a_high_swr_on_the_window_id_locks_out_the_window() {
        let mut rig = fast_rig();
        rig.swr = 3.5;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert!(st.open_window().unwrap_err().contains("SWR 3.5"));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert_eq!(st.rig().lock().unwrap().sent, ["DE N0DE"]);
    }

    #[test]
    fn a_status_blind_radio_on_the_window_id_inhibits_and_tells_the_owner() {
        // The window ID's SWR check reads receive (1C 00) with the Po meter showing
        // output: the first keyed piece of the window latches the inhibit.
        let mut rig = Radio::new(fast_rig());
        rig.status_blind = true;
        let mut st = Station::new(rig, cfg(), None);
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        let e = st.open_window().unwrap_err();
        assert!(e.contains("station ID"), "{e}");
        assert!(st.tx_inhibited());
        let n = notices.try_recv().expect("notice when it latches");
        assert!(!n.from_file && n.reason.contains("1C 00"), "{n:?}");
        // Nothing more is keyed or tuned, and nobody is told twice.
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(st.open_window().is_err());
        let rig = st.rig();
        drop(st);
        assert!(notices.try_recv().is_err(), "once per latch");
        let r = rig.lock().unwrap();
        assert_eq!(
            (r.sim.tunes, r.sim.sent.clone()),
            (1, vec!["DE N0DE".into()])
        );
    }

    /// Checks one keyed transmission: the text without the IDs is `segments`, each ID
    /// is a piece of its own after a chunk's `= <letter>` (or inside a chunk too long
    /// for that), and no stretch from `start` (or an ID) to the next ID's end is
    /// longer than `interval`. `start` is the start of the transmission, or with
    /// `carried` the node's last ID before it, which may make one due before the
    /// first piece.
    fn check_ids(
        keyed: &[(Instant, String)],
        start: Instant,
        carried: bool,
        segments: &[String],
        interval: Duration,
        dot: Duration,
    ) -> usize {
        let text: Vec<&str> = keyed
            .iter()
            .map(|(_, p)| p.as_str())
            .filter(|p| *p != "DE N0DE")
            .collect();
        assert_eq!(text.join(" "), segments.join(" "));
        let mut since = start;
        let mut ids = 0;
        for (i, (at, piece)) in keyed.iter().enumerate() {
            let end = *at + dot * cw::units(piece);
            let last = i + 1 == keyed.len();
            if piece == "DE N0DE" || last {
                // 50 ms for thread scheduling on a busy machine.
                assert!(
                    end <= since + interval + Duration::from_millis(50),
                    "piece {i} {piece:?} ends {:?} after the last ID",
                    end - since
                );
                since = *at;
            }
            if piece == "DE N0DE" {
                ids += 1;
                assert!(!last && (i > 0 || carried), "{keyed:?}");
            }
        }
        assert!(keyed.last().unwrap().1.ends_with("DE N0DE K"));
        ids
    }

    #[test]
    fn long_transmissions_identify_between_chunks() {
        let mut c = cfg();
        c.id_interval = Duration::from_secs(2);
        let mut st = Station::new(Radio::new(fast_rig()), c.clone(), None);
        st.configure().unwrap();
        let mut segments: Vec<String> = (0..16)
            .map(|i| format!("TEST TEST TEST = {}", (b'A' + i) as char))
            .collect();
        segments.last_mut().unwrap().push_str(" DE N0DE K");
        let t = Transmission {
            segments: segments.clone(),
            read_ids: Vec::new(),
        };
        let dot = st.rig().lock().unwrap().dot_duration().unwrap();
        // The whole transmission, then again as AGN repeats it: the same rule, from
        // the ID at the end of the first.
        let mut last_id = None;
        for _ in 0..2 {
            st.rig().lock().unwrap().sent_at.clear();
            let start = last_id.unwrap_or_else(Instant::now);
            st.transmit(&t).unwrap();
            let keyed = st.rig().lock().unwrap().sent_at.clone();
            let ids = check_ids(
                &keyed,
                start,
                last_id.is_some(),
                &segments,
                c.id_interval,
                dot,
            );
            last_id = Some(keyed.last().unwrap().0);
            assert!(ids >= 1, "{keyed:?}");
            // Between chunks: after a chunk's letter (or before the first chunk).
            for (i, _) in keyed
                .iter()
                .enumerate()
                .filter(|(i, k)| k.1 == "DE N0DE" && *i > 0)
            {
                let prev = &keyed[i - 1].1;
                assert!(
                    prev.chars().nth_back(1) == Some(' ') && prev.contains(" = "),
                    "{prev:?}"
                );
            }
        }
    }

    #[test]
    fn a_rest_on_receive_does_not_hold_back_a_due_id() {
        let mut c = cfg();
        c.id_interval = Duration::from_millis(3500);
        let mut rig = Radio::new(fast_rig());
        let dot = rig.dot_duration().unwrap();
        // Each chunk keys for about 0.65 s and the ID for 0.37 s. Every third chunk
        // the radio wants 1.6 s on receive first: an ID weighed only after that rest
        // would end about 4 s after the start.
        rig.rests = Some(Rests {
            every: 3,
            id: dot * cw::units("DE N0DE"),
            rest: Duration::from_millis(1600),
            pieces: 0,
            until: None,
        });
        let mut st = Station::new(rig, c.clone(), None);
        st.configure().unwrap();
        let mut segments: Vec<String> = (0..8)
            .map(|i| format!("TEST TEST TEST = {}", (b'A' + i) as char))
            .collect();
        segments.last_mut().unwrap().push_str(" DE N0DE K");
        let t = Transmission {
            segments: segments.clone(),
            read_ids: Vec::new(),
        };
        let start = Instant::now();
        st.transmit(&t).unwrap();
        let keyed = st.rig().lock().unwrap().sent_at.clone();
        assert!(
            check_ids(&keyed, start, false, &segments, c.id_interval, dot) >= 2,
            "{keyed:?}"
        );
    }

    #[test]
    fn a_chunk_too_long_for_the_interval_is_split_by_an_id() {
        let mut c = cfg();
        // fast_rig runs ten times real speed: a 30-character piece still takes about 1.1 s.
        c.id_interval = Duration::from_millis(7500);
        let mut st = Station::new(Radio::new(fast_rig()), c.clone(), None);
        st.configure().unwrap();
        let segment = format!("{} DE N0DE K", vec!["TEST"; 40].join(" "));
        let t = Transmission {
            segments: vec![segment.clone()],
            read_ids: Vec::new(),
        };
        let dot = st.rig().lock().unwrap().dot_duration().unwrap();
        let start = Instant::now();
        st.transmit(&t).unwrap();
        let keyed = st.rig().lock().unwrap().sent_at.clone();
        assert!(
            check_ids(&keyed, start, false, &[segment], c.id_interval, dot) >= 1,
            "{keyed:?}"
        );
    }

    #[test]
    fn the_last_over_s_id_counts_toward_the_next_transmission() {
        let station = |interval| {
            let mut c = cfg();
            c.id_interval = interval;
            let st = Station::new(Radio::new(fast_rig()), c, None);
            st.configure().unwrap();
            st
        };
        let interval = Duration::from_secs(6);
        let mut st = station(interval);
        let dot = st.rig().lock().unwrap().dot_duration().unwrap();
        let read_back = format!("R 44 TX MOM {} ? DE N0DE K", ["TEST"; 9].join(" "));
        // An over, then the field operator's long one (the sleep), then the read-back
        // of it: within the interval on its own, but not from the last over's ID.
        st.transmit(&tx(&["SENT 43 DE N0DE K"])).unwrap();
        let over_id = st.rig().lock().unwrap().sent_at.last().unwrap().0;
        thread::sleep(Duration::from_millis(3500));
        st.rig().lock().unwrap().sent_at.clear();
        st.transmit(&tx(&[&read_back])).unwrap();
        let keyed = st.rig().lock().unwrap().sent_at.clone();
        assert_eq!(
            keyed[0].1, "DE N0DE",
            "the read-back opens with the ID: {keyed:?}"
        );
        assert_eq!(
            check_ids(&keyed, over_id, true, &[read_back], interval, dot),
            1,
            "{keyed:?}"
        );
        // An ID too old to count, from an earlier exchange: the reply as it is.
        let interval = Duration::from_secs(1);
        let mut st = station(interval);
        st.transmit(&tx(&["SENT 43 DE N0DE K"])).unwrap();
        thread::sleep(interval * 5 / 4);
        st.rig().lock().unwrap().sent_at.clear();
        let reply = "R 44 TX MOM HI ? DE N0DE K";
        st.transmit(&tx(&[reply])).unwrap();
        let keyed = st.rig().lock().unwrap().sent_at.clone();
        let text: Vec<&str> = keyed.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(text, [reply]);
    }

    #[test]
    fn a_call_too_long_for_one_keyer_command_is_split() {
        let mut c = cfg();
        // fast_rig runs ten times real speed: a 30-character piece still takes about 1.1 s.
        c.id_interval = Duration::from_millis(7500);
        // 31 characters: the keyer (and the sim) take at most 30.
        c.station_id = format!("DE {}", "N0DE".repeat(7));
        let mut st = Station::new(Radio::new(fast_rig()), c.clone(), None);
        st.configure().unwrap();
        let segments: Vec<String> = (0..12)
            .map(|i| format!("TEST TEST TEST = {}", (b'A' + i) as char))
            .collect();
        st.transmit(&Transmission {
            segments: segments.clone(),
            read_ids: Vec::new(),
        })
        .unwrap();
        let text = st.rig().lock().unwrap().sim.sent.join(" ");
        assert!(text.contains(&c.station_id), "{text}");
        let id = format!(" {}", c.station_id);
        assert_eq!(text.replace(&id, ""), segments.join(" "));
    }

    #[test]
    fn ends_with_id_needs_the_whole_call() {
        let st = Station::new(fast_rig(), cfg(), None);
        for t in ["DE N0DE", "X DE N0DE K", "X DE N0DE KN", "X DE N0DE SK "] {
            assert!(st.ends_with_id(t), "{t}");
        }
        for t in ["CODE N0DE K", "DE N0DE K X", "DE N0DEK", "X DE N0DE K K"] {
            assert!(!st.ends_with_id(t), "{t}");
        }
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
        // So the node tunes again before its first reply after the stand-down.
        assert!(!st.tuner_ran());
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
        assert!(st.tuner_ran());
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
                Instant::now()
            })
        };
        assert_eq!(
            st.transmit(&tx(&segments)),
            Err(TxError::Storm("alert: Severe Thunderstorm Warning".into()))
        );
        let stopped = Instant::now();
        let set_at = setter.join().unwrap();
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        // Stopped part-way, within a few polls, and back on receive.
        let took = stopped.saturating_duration_since(set_at);
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert!(!r.sent.is_empty() && r.sent.len() < 16, "{:?}", r.sent);
        assert!(!r.keyer_busy() && !r.is_transmitting().unwrap());
        assert!(!st.tx_inhibited());
    }
}
