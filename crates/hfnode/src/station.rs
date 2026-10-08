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
//! - **SWR check.** SWR is sampled for the whole time every piece keys, counting
//!   only samples taken with the Po meter showing output; above the limit the node
//!   stops at once and stays silent until it next tunes, so a coax or antenna that
//!   fails part-way through a transmission stops it. If the first piece is keyed
//!   without one such sample, or later the Po meter shows no output for
//!   [`NO_OUTPUT_SAMPLES`] samples in a row while keying, the node also stops and
//!   stays silent: the radio's protection cuts its output once its power amplifier
//!   runs hot into a bad load, so missing output is itself a sign of one.
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
//!   `schedule.retune_minutes`; `hfnode radio tune` tunes once. A tune that fails
//!   (no reply to its command, still tuning at its time limit) locks transmitting
//!   out until the next tune, after receive is forced; if the tuner still reads
//!   "tuning" (1C 01 02) after that, transmitting is inhibited. In `hfnode run` a
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
//! - **Rigs without a tuner or meters** (any radio keyed through its key jack by the
//!   keyer box, [`crate::keyer`]): a window start sets the radio up and checks it but
//!   tunes and keys nothing, and SWR is not checked. Instead, once the radio reads
//!   receive after each piece, the rig says whether it saw the radio key it
//!   ([`Rig::keying_confirmed`]: the keyer box's rig listens for the sidetone); if
//!   not, the node stops and stays silent until its next window start, as for no
//!   output on the Po meter.

use crate::session::Transmission;
use crate::storm::StormHold;
use civ::{split_for_keyer, Rig, RigError};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
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
    /// How long after the keyer accepts a piece to take the first SWR sample; every
    /// piece is sampled from then until the radio is back on receive.
    pub swr_delay: Duration,
    /// Least time that keying may go on with the Po meter showing no output, over
    /// [`NO_OUTPUT_SAMPLES`] samples in a row, before the node stops
    /// ([`TxError::NoOutput`]). Both must pass, so that neither a thread held up
    /// for a while nor a quick run of samples in one word gap stops it.
    pub swr_window: Duration,
    /// Po meter reading (percent of full output) that counts as the key being down
    /// for an SWR sample.
    pub swr_min_po: f32,
    /// Semi break-in delay in dots. Must exceed the 7-dot word gap so the radio
    /// stays on transmit for a whole piece.
    pub break_in_delay_dots: f32,
    /// Extra time allowed beyond the keying time and break-in delay before the
    /// transmitter is declared stuck (or the rig's [`Rig::receive_settle`], if
    /// longer).
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
    /// Po meter reading (percent of full output) above which a piece is stopped
    /// ([`TxError::HighPower`]): well above what the set power gives, since the
    /// meter and the power setting are not yet calibrated against each other
    /// (docs/hardware-test-plan.md, step 9).
    pub po_limit: f32,
    /// Most of any `duty_window` that the key may be down (carrier, by the Morse
    /// timing of what was keyed): pieces wait on receive for the share to come
    /// back. Lower at higher power ([`duty_for_power`]).
    pub duty: f32,
    pub duty_window: Duration,
    /// Most keying time (by the Morse timing, at the radio's speed) one
    /// transmission may take; a longer one is refused before anything is keyed.
    pub max_transmission: Duration,
    /// Longest the watchdog waits for the radio while another call holds it before
    /// it latches the inhibit without it: no call to a radio that answers holds it
    /// this long. Real time, also in tests.
    pub radio_wait: Duration,
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
            // With NO_OUTPUT_SAMPLES samples, at least 4 s at the 100 ms poll.
            swr_window: Duration::from_secs(1),
            // A quarter of the set power: well clear of key-up (0) and of the
            // CW envelope's rise and fall.
            swr_min_po: (c.power_watts as f32 * 0.25).max(2.0),
            break_in_delay_dots: BREAK_IN_DELAY_DOTS,
            stuck_margin: STUCK_MARGIN,
            // The manual's tuner takes "2~3 seconds" (p. 11-2), and "15 seconds
            // (maximum)" (p. 16-3, manual text line 8119): leave room above that.
            tune_timeout: Duration::from_secs(20),
            poll: Duration::from_millis(100),
            station_id: format!("DE {}", c.node_call.to_ascii_uppercase()),
            id_interval: ID_INTERVAL,
            po_limit: po_limit(c.power_watts),
            duty: duty_for_power(c.power_watts),
            duty_window: DUTY_WINDOW,
            max_transmission: MAX_TRANSMISSION,
            radio_wait: Duration::from_secs(10),
        }
    }
}

/// The semi break-in delay the node sets, in dots: 3 dots more than a word gap. At
/// most 2 s (at 6 wpm), which [`STUCK_MARGIN`] covers.
pub const BREAK_IN_DELAY_DOTS: f32 = 10.0;

/// Time allowed past a piece's keying time and break-in delay before the radio is
/// taken to be stuck on transmit.
pub const STUCK_MARGIN: Duration = Duration::from_secs(3);

/// The least `max_key` a piece of one character fits under at `wpm` (a zero, the
/// longest there is), with the break-in delay and stuck margin the node uses: below
/// it the watchdog cuts off pieces that cannot be split further
/// ([`Station::pieces`]).
pub fn shortest_max_key(wpm: u32) -> Duration {
    let dot = Duration::from_millis(1200) / wpm.max(1);
    dot.mul_f32(cw::units("0") as f32 + BREAK_IN_DELAY_DOTS) + STUCK_MARGIN
}

/// The Po meter reading (percent of full output, 100 W) that stops a piece at
/// `watts` set: half as much again, and 5 points more. The [RF PWR] level is a knob
/// position, not watts (14 0A, p. 19-3; manual text line 8677), so a radio whose
/// level-to-watts mapping is far off, or whose power was turned up at its front
/// panel, is stopped rather than trusted.
pub fn po_limit(watts: u32) -> f32 {
    watts as f32 * 1.5 + 5.0
}

/// The window [`StationConfig::duty`] is measured over.
pub const DUTY_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Share of [`DUTY_WINDOW`] the key may be down at `watts`: half up to 50 W, then
/// less, down to a quarter at 100 W. The manual gives no duty rating; the radio cuts
/// its power, then its transmitting, once its power amplifier runs hot (p. 13-4;
/// manual text lines 7316-7334), and long CW at high power is what heats it.
pub fn duty_for_power(watts: u32) -> f32 {
    (25.0 / watts.max(1) as f32).min(0.5)
}

/// The most keying time one transmission may take: the longest reply the session
/// builds at the default settings (26 chunks of 60 characters) is about 18 minutes
/// at 18 wpm, and `run` checks that it fits at the configured speed
/// ([`crate::commissioning::check_keying`]).
pub const MAX_TRANSMISSION: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, PartialEq)]
pub enum TxError {
    /// Since the last tune, SWR was too high, the output missing or too high, the
    /// radio stuck on transmit, the tuner could not match, the tune failed, or the
    /// radio could not be set up for the tune; transmitting is suspended until the
    /// next one.
    SwrLockout,
    /// SWR was too high just now; the transmission was cut off.
    HighSwr(f32),
    /// A piece was keyed without the Po meter showing output, so SWR could not be
    /// measured: the first piece of a transmission without one sample with
    /// output, or later [`NO_OUTPUT_SAMPLES`] samples in a row while keying with
    /// none; treated like high SWR (the radio cuts its power into a bad load once
    /// its power amplifier runs hot).
    NoOutput,
    /// A rig without meters did not see the radio key the piece just sent (the
    /// keyer box's rig heard no sidetone following it: the key cable, the radio
    /// off or not in CW, its sidetone off, or the audio); treated like no output.
    NotHeard,
    /// The Po meter read more output than the set power gives (percent of full
    /// output); treated like high SWR.
    HighPower(f32),
    /// The radio stayed on transmit too long and was forced back to receive; also
    /// treated like high SWR, since the next transmission may stick too.
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
    /// The transmission would key for longer than `limit`
    /// ([`StationConfig::max_transmission`]); nothing was keyed.
    TooLong {
        keying: Duration,
        limit: Duration,
    },
    Rig(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwrLockout => write!(
                f,
                "transmit locked out until the node next tunes, or starts a window \
                 on a rig without a tuner (high SWR, no output, too much output, \
                 keying not heard, a radio stuck on transmit, no tuner match, a \
                 failed tune, or the radio could not be set up)"
            ),
            Self::HighSwr(s) => write!(f, "SWR {s:.1} above limit"),
            Self::NoOutput => write!(f, "no output while keying: SWR not measured"),
            Self::HighPower(po) => write!(
                f,
                "output {po:.0}% of full, above what the set power gives: check the radio's \
                 RF POWER"
            ),
            Self::NotHeard => write!(
                f,
                "the radio was not heard keying (no sidetone): check the key cable, the \
                 radio and its sidetone"
            ),
            Self::Stuck => write!(f, "transmitter did not return to receive"),
            Self::Inhibited => write!(
                f,
                "radio not confirmed on receive: transmit inhibited until the node is \
                 restarted with {INHIBIT_FILE} removed from the state directory"
            ),
            Self::NotReady(e) => write!(f, "radio not ready to transmit: {e}"),
            Self::Storm(why) => write!(f, "storm stand-down: {why}"),
            Self::TooLong { keying, limit } => write!(
                f,
                "a transmission keying for {:.1} min is longer than the {:.0} min limit: not \
                 keyed",
                keying.as_secs_f32() / 60.0,
                limit.as_secs_f32() / 60.0
            ),
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

/// Readings of receive in a row that confirm the radio is on receive, and readings
/// of "not tuning" that confirm a tuner has stopped. CI-V frames carry no checksum
/// (p. 19-2; manual text lines 8551-8557): one flipped bit turns a transmit reply
/// (01) into receive (00), so one reading is not enough.
const RX_READINGS: u32 = 2;

/// Faults in a row that latch the transmit inhibit: high SWR, no output, too much
/// output, keying not heard, a radio left on transmit, a tuner that could not match
/// or a tune that failed. Each locks the node out until its next tune; the second
/// with no good transmission between them means the next tune would only key a
/// carrier into the same fault (a cut coax at every window), so a person must look.
pub const FAULTS_TO_LATCH: u32 = 2;

/// How often the watchdog thread looks.
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// While the node is not keying, the watchdog reads the transmit status of a rig
/// that [`Rig::polls_status_while_idle`] every this many ticks (a second).
const IDLE_POLL_TICKS: u32 = 4;

/// Status reads in a row that fail while the node is not keying before the
/// watchdog forces receive (which latches the inhibit if it cannot confirm it): a
/// radio switched off or a USB cable pulled is found within seconds, not at the next
/// check.
const IDLE_ERRORS: u32 = 5;

/// How far the RF power read back may be from the power set: the [RF PWR] position
/// is 0-255 ("00 00=max. CCW, 02 55=max. CW", 14 0A, p. 19-3; manual text lines
/// 8676-8677), onto which the driver maps 0-100 W ([`civ::ic7300::power_level`]),
/// so one step is 0.4 W on that scale.
const POWER_TOLERANCE_W: f32 = 1.0;

/// The radio's own Time-Out Timer, its shortest setting ("01=3 min.", 1A 05 00 29,
/// p. 19-4; manual text line 8862): required before anything transmits, as the
/// limit that holds with this software stopped.
pub const REQUIRED_TOT: Duration = Duration::from_secs(180);

/// Longest semi break-in delay the radio can be set to, in dots: "00 00=2.0d to
/// 02 55=13.0d" (14 0F, p. 19-3).
const MAX_BREAK_IN_DOTS: f32 = 13.0;

/// A dot at the keyer's slowest speed, 6 wpm ("00 00=6wpm", 14 0C, p. 19-3), for
/// when the speed cannot be read.
const SLOWEST_DOT: Duration = Duration::from_millis(200);

/// Samples in a row, taken while a piece keys, with the Po meter showing no output
/// on either reading, that stop a transmission ([`TxError::NoOutput`]), once
/// [`StationConfig::swr_window`] has passed too. A sample lands on key-up a little
/// over half the time in Morse text, and its two Po readings are often in the same
/// gap, so a run of 40 is about 1 in 10^10 by chance, and longer than any gap in
/// the keying (a word gap at 6 wpm is 1.4 s, about 7 samples). Each sample is the
/// 100 ms poll plus four CI-V reads, so 40 take 4 s at the very least and more like
/// 6 to 8 s. A radio protecting itself has already cut its output, so the wait
/// costs little. To be checked against the radio's Po meter on the bench
/// (docs/hardware-test-plan.md, step 6).
pub const NO_OUTPUT_SAMPLES: u32 = 40;

/// Put the radio on receive and confirm it: stop the keyer and switch to receive
/// (each sent whether or not the other worked), then read the transmit status.
/// Repeated until receive is seen; an error means receive could not be confirmed.
///
/// With semi break-in the radio "returns to receive after a preset time after you
/// stop keying" (p. 4-15), and the receive command may not cut that short, so the
/// attempts go on for the longest break-in delay at the keyer's speed before giving
/// up, or for as long as the rig says it needs to see receive
/// ([`Rig::receive_settle`]) if that is longer. That time counts from when the
/// first stop and receive commands have gone out: after a CI-V timeout the driver
/// first waits for the link to go quiet (up to four reply timeouts), and the radio
/// cannot start its delay before then.
pub fn force_receive<R: Rig + ?Sized>(r: &mut R) -> civ::Result<()> {
    let dot = r.dot_duration().unwrap_or(SLOWEST_DOT);
    let settle = r.receive_settle();
    let mut deadline = None;
    let mut last = RigError::Timeout;
    // Receive readings in a row, the latest included ([`RX_READINGS`]).
    let mut rx = 0;
    for attempt in 0.. {
        // A radio just seen on receive is read once more, whatever the time.
        if attempt >= FORCE_RX_ATTEMPTS && rx == 0 && deadline.is_some_and(|d| Instant::now() >= d)
        {
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
        deadline.get_or_insert_with(|| Instant::now() + dot.mul_f32(MAX_BREAK_IN_DOTS).max(settle));
        match r.is_transmitting() {
            Ok(false) => {
                rx += 1;
                if rx >= RX_READINGS {
                    return Ok(());
                }
            }
            Ok(true) => {
                rx = 0;
                last = RigError::Protocol(match r.transmit_detail() {
                    Some(why) => format!("radio still reports transmit: {why}"),
                    None => "radio still reports transmit".into(),
                })
            }
            Err(e) => {
                rx = 0;
                last = e;
            }
        }
    }
    // Not confirmed: whatever else the rig can send to keep the radio from
    // transmitting (the IC-7300: break-in off, TX Inhibit on).
    log::error!("radio not confirmed on receive ({last}): inhibiting transmit at the radio");
    if let Err(e) = r.inhibit_transmit() {
        log::error!("inhibiting transmit at the radio: {e}");
    }
    Err(last)
}

/// The transmit inhibit on its own, for the stop paths outside a [`Station`]:
/// `hfnode radio rx`, `hfnode keyer rx`, and the stop-signal handler in `main`.
///
/// A stop that cannot confirm the radio back on receive must leave
/// [`INHIBIT_FILE`] behind, so that nothing transmits after a restart until someone
/// has looked at the radio. Without that, a node stopped with its key stuck starts
/// up and keys again (the safety audit's KB-2(iii) for the keyer box, and K5 for the
/// IC-7300: the same defect on both rigs, which share these paths).
#[derive(Clone)]
pub struct InhibitLatch(Arc<Inhibit>);

impl InhibitLatch {
    /// The latch that keeps [`INHIBIT_FILE`] in `state_dir`. Reads it: if it is
    /// already there, or this user cannot tell whether it is, the latch starts set.
    pub fn in_dir(state_dir: &Path) -> Self {
        Self(Arc::new(Inhibit::new(Some(state_dir.join(INHIBIT_FILE)))))
    }

    /// A latch with nowhere to write: it holds for this process only (a command run
    /// without a state directory).
    pub fn in_memory() -> Self {
        Self(Arc::new(Inhibit::new(None)))
    }

    /// Stop all transmitting, writing [`INHIBIT_FILE`]; after the first time this
    /// does nothing.
    pub fn latch(&self, why: &str) {
        self.0.latch(why);
    }

    pub fn is_set(&self) -> bool {
        self.0.is_set()
    }
}

/// [`force_receive`], latching `inhibit` if receive is not confirmed.
pub fn force_receive_or_latch<R: Rig + ?Sized>(
    r: &mut R,
    inhibit: &InhibitLatch,
) -> civ::Result<()> {
    force_receive(r).map_err(|e| {
        inhibit.latch(&format!("radio not confirmed on receive ({e})"));
        e
    })
}

/// [`force_receive`], latching `inhibit` if receive is not confirmed.
fn force_receive_latching<R: Rig + ?Sized>(r: &mut R, inhibit: &Inhibit) -> Result<(), TxError> {
    force_receive(r).map_err(|e| {
        inhibit.latch(&format!("radio not confirmed on receive ({e})"));
        TxError::Inhibited
    })
}

/// The lock on `m`, waiting at most `wait` for whoever holds it; `None` if it is
/// still held then. A lock left poisoned by a panic is taken all the same.
///
/// For the stop paths: a call to the radio that never returns (a serial port that
/// never drains, the safety audit's K4) holds the radio's lock for good, and a stop
/// that waited for it would never latch the inhibit or tell anyone.
pub fn lock_within<T: ?Sized>(m: &Mutex<T>, wait: Duration) -> Option<MutexGuard<'_, T>> {
    let until = Instant::now() + wait;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() >= until => return None,
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Written to the state directory (beside the health log) when transmitting is
/// inhibited; while it exists, nothing is transmitted, across restarts. A node that
/// cannot tell whether it exists counts it as there ([`inhibit_on_disk`]), and one
/// that cannot write it does not key ([`check_state_dir`]).
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

/// The inhibit kept in `file` when this process starts, if there is one.
///
/// Only a file that is certainly not there lets the node transmit. Anything at that
/// name counts as the file, a dangling link too, and so does a file that cannot be
/// looked for: `Path::exists` answers "no" for a state directory this user may not
/// search, and a command run as the wrong user then keyed with the inhibit in place
/// (the safety audit's K6). Such an error inhibits transmitting here, with a reason
/// that says which file could not be checked and why, for the log and the email.
fn inhibit_on_disk(file: &Path) -> Option<InhibitNotice> {
    let (at, reason) = match std::fs::symlink_metadata(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(_) => {
            let why = std::fs::read_to_string(file).unwrap_or_default();
            log::error!(
                "transmit inhibited by {} ({}): once the radio has been checked, stop the node, \
                 remove the file and start it again",
                file.display(),
                why.trim()
            );
            parse_inhibit_file(&why)
        }
        Err(e) => {
            let why = format!(
                "could not check for {} ({e}), so it is taken as there",
                file.display()
            );
            log::error!(
                "transmit inhibited: {why}: run hfnode as the user that owns the state \
                 directory, or make the directory readable by this one"
            );
            (None, why)
        }
    };
    Some(InhibitNotice {
        at,
        reason,
        from_file: true,
        file: Some(file.to_path_buf()),
    })
}

/// Check that `dir` can keep [`INHIBIT_FILE`], creating it if it is not there yet:
/// a file must be created, written and removed in it. Every command that can key
/// the radio calls this before it opens any port, and refuses to go on if it fails.
///
/// A node that cannot write the inhibit there must not key: a stop that cannot
/// confirm receive would then leave nothing behind, and the next start would key
/// again. Nor can it be sure of reading an inhibit already there (see
/// [`inhibit_on_disk`]). Opening the port can itself key some radios (it pulses
/// DTR on Linux), so the check comes first.
pub fn check_state_dir(dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let probe = dir.join(format!(".write-check-{}", std::process::id()));
    std::fs::create_dir_all(dir)
        .and_then(|()| {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&probe)?;
            // A full disk shows only once something is written.
            let written = f.write_all(b"hfnode\n").and_then(|()| f.sync_all());
            drop(f);
            let removed = std::fs::remove_file(&probe);
            written.and(removed)
        })
        .with_context(|| {
            format!(
                "state_dir {} cannot be written: nothing is keyed without somewhere to keep \
                 {INHIBIT_FILE}. Run hfnode as the user that owns it, or make it writable \
                 by this one",
                dir.display()
            )
        })
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
        let latched = file.as_deref().and_then(inhibit_on_disk);
        Self {
            set: AtomicBool::new(latched.is_some()),
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

/// Kept in the state directory: the carrier keyed lately, for
/// [`StationConfig::duty`], so that a restart does not forget it. One line each,
/// `<unix time in ms it ended> <ms of carrier>`.
pub const DUTY_FILE: &str = "key-down";

/// The carrier the node has keyed (key down, and tunes), as (when it ended, how
/// long), over the last [`StationConfig::duty_window`].
struct DutyLog {
    keyed: VecDeque<(Instant, Duration)>,
    /// [`DUTY_FILE`], without which a restart starts from nothing.
    file: Option<PathBuf>,
}

impl DutyLog {
    /// Read `file`, keeping what is still inside `window`. Times in the future (a
    /// clock set back) count as now; lines that do not read are dropped.
    fn load(file: Option<PathBuf>, window: Duration) -> Self {
        let mut keyed = VecDeque::new();
        if let Some(text) = file
            .as_deref()
            .and_then(|f| std::fs::read_to_string(f).ok())
        {
            let (now_ms, now) = (unix_ms(), Instant::now());
            for line in text.lines() {
                let mut f = line.split_whitespace().map(str::parse::<u64>);
                let (Some(Ok(at)), Some(Ok(ms))) = (f.next(), f.next()) else {
                    continue;
                };
                let age = Duration::from_millis(now_ms.saturating_sub(at));
                if age < window {
                    let ended = now.checked_sub(age).unwrap_or(now);
                    keyed.push_back((ended, Duration::from_millis(ms)));
                }
            }
        }
        let log = Self { keyed, file };
        log.save();
        log
    }

    /// Carrier keyed in the last `window`.
    fn used(&self, window: Duration) -> Duration {
        self.keyed
            .iter()
            .filter(|(at, _)| at.elapsed() < window)
            .map(|&(_, d)| d)
            .sum()
    }

    /// How long to wait, on receive, before `carrier` more fits in `budget` of any
    /// `window`; `None` if it never can.
    fn rest(&self, carrier: Duration, budget: Duration, window: Duration) -> Option<Duration> {
        if carrier > budget {
            return None;
        }
        let mut used = self.used(window);
        for &(at, d) in &self.keyed {
            if used + carrier <= budget {
                break;
            }
            if at.elapsed() < window {
                used -= d;
                if used + carrier <= budget {
                    return Some((at + window).saturating_duration_since(Instant::now()));
                }
            }
        }
        Some(Duration::ZERO)
    }

    fn record(&mut self, carrier: Duration, window: Duration) {
        self.keyed.retain(|(at, _)| at.elapsed() < window);
        self.keyed.push_back((Instant::now(), carrier));
        self.save();
    }

    /// Write what is kept to [`DUTY_FILE`], through a file beside it, so that a
    /// crash part-way leaves the last one whole.
    fn save(&self) {
        let Some(f) = &self.file else { return };
        let now_ms = unix_ms();
        let text: String = self
            .keyed
            .iter()
            .map(|(at, d)| {
                let at = now_ms.saturating_sub(at.elapsed().as_millis() as u64);
                format!("{at} {}\n", d.as_millis())
            })
            .collect();
        let tmp = f.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, f)) {
            log::warn!("cannot write {}: {e}", f.display());
        }
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A keying run: the longest it may keep the radio busy, and how much of that the
/// key is down.
#[derive(Debug, Clone, Copy)]
struct Run {
    keying: Duration,
    carrier: Duration,
}

impl std::ops::Add for Run {
    type Output = Run;
    fn add(self, o: Run) -> Run {
        Run {
            keying: self.keying + o.keying,
            carrier: self.carrier + o.carrier,
        }
    }
}

/// `piece` in parts that each `fits`, split between words, and inside a word only
/// if it does not fit on its own; `None` if a single character does not fit.
fn split_to_fit(piece: &str, fits: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in piece.split_whitespace() {
        let joined = match cur.is_empty() {
            true => word.to_string(),
            false => format!("{cur} {word}"),
        };
        if fits(&joined) {
            cur = joined;
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        for c in word.chars() {
            cur.push(c);
            if fits(&cur) {
                continue;
            }
            cur.pop();
            if cur.is_empty() {
                return None;
            }
            out.push(std::mem::replace(&mut cur, c.to_string()));
            if !fits(&cur) {
                return None;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
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
    /// The last [`Station::start_window`] finished: see [`Station::tuned`].
    tuned: bool,
    /// SWR has been measured on the current transmission.
    swr_measured: bool,
    /// The highest SWR measured on the current transmission, for the health log.
    swr_worst: Option<f32>,
    /// Samples taken while keying in the current transmission with no output on
    /// the Po meter, since the last that showed some: when the first was taken,
    /// and how many.
    dark: Option<(Instant, u32)>,
    /// When the node last identified: the start of the last piece of a
    /// transmission that ended with its ID, or of an ID keyed inside one.
    last_id: Option<Instant>,
    health_log: Option<PathBuf>,
    /// While this says so, nothing is tuned or keyed.
    storm: Option<Arc<StormHold>>,
    /// Faults since the last transmission that went out whole
    /// ([`FAULTS_TO_LATCH`]).
    faults: u32,
    /// For [`StationConfig::duty`].
    duty: DutyLog,
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
    /// beside it, and if it is already there (or cannot be checked) nothing will be
    /// transmitted.
    pub fn new(rig: R, cfg: StationConfig, health_log: Option<PathBuf>) -> Self {
        let state_dir = health_log
            .as_deref()
            .map(|p| p.parent().unwrap_or(Path::new("")).to_path_buf());
        let inhibit_file = state_dir.as_deref().map(|d| d.join(INHIBIT_FILE));
        let duty = DutyLog::load(
            state_dir.as_deref().map(|d| d.join(DUTY_FILE)),
            cfg.duty_window,
        );
        let s = Self {
            rig: Arc::new(Mutex::new(rig)),
            cfg,
            keying_since: Arc::new(Mutex::new(None)),
            watchdog_fired: Arc::new(AtomicBool::new(false)),
            tx_inhibit: Arc::new(Inhibit::new(inhibit_file)),
            stop: Arc::new(AtomicBool::new(false)),
            swr_lockout: false,
            tuned: false,
            swr_measured: false,
            swr_worst: None,
            dark: None,
            last_id: None,
            health_log,
            storm: None,
            faults: 0,
            duty,
        };
        s.spawn_watchdog();
        s
    }

    pub fn rig(&self) -> Arc<Mutex<R>> {
        self.rig.clone()
    }

    /// The station's transmit inhibit, for a stop path outside it to latch.
    pub fn inhibit_latch(&self) -> InhibitLatch {
        InhibitLatch(self.tx_inhibit.clone())
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

    /// The watchdog thread. Every [`WATCHDOG_TICK`]:
    ///
    /// - While the node keys (a piece, or a tune), if that has gone on longer than
    ///   `max_key`, it forces receive, and keeps trying on later ticks until receive
    ///   is confirmed (latching the inhibit if it is not). If the radio stays busy
    ///   with another call for `radio_wait` (one that never returns), it latches the
    ///   inhibit without it, and keeps trying.
    /// - While it does not, it looks for the radio transmitting all the same: a key
    ///   held closed at the radio as the rig sees it ([`Rig::held_key`]), and, on a
    ///   rig whose status is a quick read ([`Rig::polls_status_while_idle`]), the
    ///   radio's own status every [`IDLE_POLL_TICKS`] ticks: transmit on
    ///   [`RX_READINGS`] reads in a row forces receive and latches the inhibit;
    ///   [`IDLE_ERRORS`] failed reads in a row force receive (which latches it if
    ///   receive cannot be confirmed). It never waits for the radio while another
    ///   call has it.
    fn spawn_watchdog(&self) {
        let (rig, since, fired, inhibit, stop, max, wait) = (
            self.rig.clone(),
            self.keying_since.clone(),
            self.watchdog_fired.clone(),
            self.tx_inhibit.clone(),
            self.stop.clone(),
            self.cfg.max_key,
            self.cfg.radio_wait,
        );
        thread::spawn(move || {
            // Not keying: ticks since the radio's status was last read, and the reads
            // in a row that found it transmitting, or failed.
            let (mut ticks, mut keyed, mut failed) = (0, 0, 0);
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(WATCHDOG_TICK);
                let started = *lock(&since);
                if started.is_some() || inhibit.is_set() {
                    (ticks, keyed, failed) = (0, 0, 0);
                }
                if started.is_some_and(|t| t.elapsed() > max) {
                    log::error!("watchdog: keying exceeded {max:?}, forcing receive");
                    fired.store(true, Ordering::SeqCst);
                    let Some(mut r) = lock_within(&rig, wait) else {
                        // A call to the radio that has not returned: nothing can be
                        // sent to it, so nothing more may be keyed. Tried again on the
                        // next tick.
                        inhibit.latch(&format!(
                            "keying exceeded {max:?}, and the radio could not be reached for \
                             {wait:?} to stop it (a call to it did not return)"
                        ));
                        continue;
                    };
                    // Keep trying on later ticks until receive is confirmed.
                    if force_receive_latching(&mut *r, &inhibit).is_ok() {
                        *lock(&since) = None;
                    }
                    continue;
                }
                if started.is_some() || inhibit.is_set() {
                    continue;
                }
                let mut r = match rig.try_lock() {
                    Ok(r) => r,
                    Err(TryLockError::Poisoned(e)) => e.into_inner(),
                    Err(TryLockError::WouldBlock) => continue,
                };
                // The station marks keying before it keys, and cannot key while this
                // holds the radio: if it has started since, it is not idle.
                if lock(&since).is_some() {
                    continue;
                }
                if let Some(why) = r.held_key() {
                    log::error!("watchdog: the radio looks keyed while idle: {why}");
                    let _ = force_receive_latching(&mut *r, &inhibit);
                    continue;
                }
                if !r.polls_status_while_idle() {
                    continue;
                }
                ticks += 1;
                if ticks < IDLE_POLL_TICKS {
                    continue;
                }
                ticks = 0;
                match r.is_transmitting() {
                    Ok(false) => (keyed, failed) = (0, 0),
                    Ok(true) => {
                        (keyed, failed) = (keyed + 1, 0);
                        if keyed >= RX_READINGS {
                            keyed = 0;
                            log::error!(
                                "watchdog: the radio is on transmit with nothing from the node \
                                 keying it: forcing receive"
                            );
                            let _ = force_receive(&mut *r);
                            inhibit.latch(
                                "the radio transmitted with nothing from the node keying it \
                                 (1C 00 read transmit twice while idle): someone at the radio, \
                                 VOX, a key or PTT line, or another program. Stop hfnode \
                                 before using the radio by hand",
                            );
                        }
                    }
                    Err(e) => {
                        (keyed, failed) = (0, failed + 1);
                        if failed >= IDLE_ERRORS {
                            failed = 0;
                            log::error!(
                                "watchdog: the radio's status could not be read {IDLE_ERRORS} \
                                 times in a row ({e}): forcing receive"
                            );
                            let _ = force_receive_latching(&mut *r, &inhibit);
                        }
                    }
                }
            }
        });
    }

    fn force_rx(&self) -> Result<(), TxError> {
        force_receive_latching(&mut *lock(&self.rig), &self.tx_inhibit)
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

    /// Frequency, mode, power and keyer settings; none of these transmits. Semi
    /// break-in goes on last, and only while transmitting is not inhibited: with it
    /// off neither a keyer message nor a key held closed transmits (p. 4-15; manual
    /// text lines 2833-2836), which is how [`Rig::inhibit_transmit`] leaves an
    /// IC-7300.
    fn apply_settings(&self) -> civ::Result<()> {
        let c = self.cfg.clone();
        let break_in = !self.tx_inhibited();
        self.with_rig(|r| {
            // Mode first: with SSB/CW Synchronous Tuning ON, a change from SSB to CW
            // shifts the frequency by the CW pitch (p. 12-6, manual text line 6409).
            r.set_mode_cw()?;
            r.set_frequency(c.frequency_hz)?;
            r.set_rf_power_watts(c.power_watts)?;
            r.set_key_speed(c.key_speed_wpm)?;
            r.set_break_in_delay(c.break_in_delay_dots)?;
            r.set_break_in(break_in)
        })
    }

    /// Once the settings have gone out: the radio is still on receive, its own
    /// Time-Out Timer is set to 3 minutes, its power reads back as set, and it
    /// would transmit on the configured frequency. Reads only.
    fn check_ready(&self) -> civ::Result<()> {
        self.confirm_receive()?;
        self.check_time_out_timer()?;
        self.check_power()?;
        self.check_transmit_frequency()
    }

    /// The radio reads receive [`RX_READINGS`] times in a row, a poll after semi
    /// break-in went on: "In the Semi Break-in mode, the transceiver transmits when
    /// keying" (p. 4-15; manual text lines 2732-2734), so a key held closed at the
    /// radio transmits from then on, as VOX or a PTT line would.
    fn confirm_receive(&self) -> civ::Result<()> {
        thread::sleep(self.cfg.poll);
        for _ in 0..RX_READINGS {
            if self.with_rig(|r| r.is_transmitting())? {
                return Err(RigError::Protocol(
                    "on transmit without the node keying it, once semi break-in was on".into(),
                ));
            }
        }
        Ok(())
    }

    /// The radio's own transmit limit is set to 3 minutes ([`REQUIRED_TOT`]): the one
    /// limit that holds with this software stopped. Read before every transmission
    /// and tune, since the menu can be changed at the radio while the node runs. A
    /// rig without such a setting passes.
    fn check_time_out_timer(&self) -> civ::Result<()> {
        match self.with_rig(|r| r.time_out_timer())? {
            None => Ok(()),
            Some(t) if t == REQUIRED_TOT => Ok(()),
            Some(t) if t.is_zero() => Err(RigError::Protocol(
                "the radio's Time-Out Timer (TOT) is OFF: set it to 3 min".into(),
            )),
            Some(t) => Err(RigError::Protocol(format!(
                "the radio's Time-Out Timer (TOT) is {} min: set it to 3 min",
                t.as_secs() / 60
            ))),
        }
    }

    /// The RF power reads back (14 0A) within a watt of what was set, so that a
    /// setting the radio did not take is not keyed at. A rig that cannot tell
    /// passes.
    fn check_power(&self) -> civ::Result<()> {
        let set = self.cfg.power_watts as f32;
        match self.with_rig(|r| r.rf_power_watts())? {
            Some(w) if (w - set).abs() > POWER_TOLERANCE_W => Err(RigError::Protocol(format!(
                "RF power reads back {w:.1} W, not the {set:.0} W set"
            ))),
            _ => Ok(()),
        }
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

    /// The radio is on receive, set up as configured, and ready to transmit
    /// ([`Station::check_ready`]). It should be on receive already; if it is not,
    /// something else is keying it, and nothing is written to it. Split and ∂TX
    /// are not set by the node, so they are only checked.
    fn prepare(&self) -> civ::Result<()> {
        if self.with_rig(|r| r.is_transmitting())? {
            return Err(RigError::Protocol(
                "on transmit without the node keying it".into(),
            ));
        }
        self.apply_settings()?;
        self.check_ready()
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
    /// SWR lockout from before once the tune runs (on a rig without a tuner, once
    /// the radio is set up and checked), and locks out transmitting until the next
    /// call if the radio could not be set up, the tune failed or the tuner could not
    /// match. A storm stand-down that stops it before the tune leaves the lockout
    /// as it was.
    /// A tune that failed is followed by a forced receive and a check that the tuner
    /// has stopped ([`Station::confirm_tuner_stopped`]), which inhibits
    /// transmitting if it has not.
    pub fn start_window(&mut self) -> civ::Result<()> {
        self.tuned = false;
        if self.tx_inhibited() {
            // Tuning transmits.
            return Err(RigError::Protocol(TxError::Inhibited.to_string()));
        }
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
        if !lock(&self.rig).has_tuner() {
            // Nothing to tune (a handheld, or any radio on the keyer box): set up and
            // checked is all a window start needs, and nothing is transmitted.
            self.swr_lockout = false;
            self.tuned = true;
            return Ok(());
        }
        // Cleared only now, as the tune runs: the tune decides it again below.
        self.swr_lockout = false;
        let t0 = Instant::now();
        // Whether the tuner matched, once it says it has finished.
        let matched = self
            .tune(t0)
            .and_then(|()| self.with_rig(|r| r.tuner_matched()));
        let result = match matched {
            Ok(true) => {
                self.tuned = true;
                self.health("tune", &format!("{}ms", t0.elapsed().as_millis()));
                Ok(())
            }
            Ok(false) => {
                // The tuner has given its answer, so this window's tune is done: it
                // bypassed itself, and the antenna is beyond its 3:1 range.
                self.tuned = true;
                self.health("tune", "no-match");
                log::error!("tuner could not match the antenna: silent until the next tune");
                self.fault("the tuner could not match the antenna");
                Err(RigError::Protocol("tuner could not match the load".into()))
            }
            Err(e) => {
                // A tune that cannot be trusted, whatever the radio did: locked out
                // until the next one, which is not put off as if this one had done.
                self.swr_lockout = true;
                match self.storm_reason() {
                    // Stopped for the storm stand-down: not a fault of the radio's.
                    Some(why) => log::warn!("storm stand-down during the tune ({why})"),
                    None => {
                        log::error!("tune failed ({e}): silent until the next tune");
                        self.fault(&format!("the tune failed ({e})"));
                    }
                }
                // The radio may have taken 1C 01 02 even if its reply was lost, or
                // still be tuning: make sure it is back on receive, and that the
                // tuner has stopped, before going on.
                self.force_rx()
                    .and_then(|()| self.confirm_tuner_stopped(t0))
                    .map_err(|e| RigError::Protocol(e.to_string()))
                    .and(Err(e))
            }
        };
        if !self.tx_inhibited() {
            // Not confirmed on receive leaves it to the watchdog to keep trying.
            *lock(&self.keying_since) = None;
        }
        result
    }

    /// Whether the last [`Station::start_window`] finished, so that the node can
    /// count its tune as recent: the radio was set up and checked and the tuner
    /// gave its answer, matched or (locked out until the next tune) unable to
    /// match; on a rig without a tuner, the window started. One that did not
    /// (inhibited, the radio could not be set up, a storm stand-down, or a tune
    /// that failed: no reply to its command, or still tuning at its time limit) is
    /// tried again before the next reply.
    pub fn tuned(&self) -> bool {
        self.tuned
    }

    /// After a tune that failed and a forced receive, make sure the tuner has
    /// stopped: 1C 01 must read other than 02 ("tuning", p. 19-7) twice in a row. A
    /// tune whose command reply was lost may still be running, and has until its own
    /// time limit (`tune_timeout` from `t0`), as any tune does; after a tune that
    /// timed out, that has passed, and the next readings decide. Still tuning, or not
    /// readable, at the limit: transmitting is inhibited, since the manual does not
    /// say that 17 FF or 1C 00 00 ends a tune, and nothing else the node can send
    /// would.
    fn confirm_tuner_stopped(&self, t0: Instant) -> Result<(), TxError> {
        let limit = t0 + self.cfg.tune_timeout;
        // "Not tuning" readings in a row ([`RX_READINGS`]): CI-V has no checksum.
        let mut stopped = 0;
        loop {
            let why = match self.with_rig(|r| r.tuner_busy()) {
                Ok(false) => {
                    stopped += 1;
                    if stopped >= RX_READINGS {
                        return Ok(());
                    }
                    continue;
                }
                Ok(true) => {
                    "the tuner still reads tuning (1C 01 02) after the tune was stopped".into()
                }
                Err(e) => {
                    format!("the tuner's state (1C 01) could not be read after a failed tune ({e})")
                }
            };
            stopped = 0;
            if Instant::now() >= limit {
                self.health("tune", "not-stopped");
                self.tx_inhibit.latch(&why);
                return Err(TxError::Inhibited);
            }
            thread::sleep(self.cfg.poll);
        }
    }

    /// Start a tuner cycle, wait for it to end, for at most `tune_timeout`, and for
    /// the radio to read receive after it. The watchdog times it as it times keying
    /// (`max_key`), from the tune command; it stops for the storm stand-down. Its
    /// whole time counts as carrier for [`StationConfig::duty`].
    fn tune(&mut self, t0: Instant) -> civ::Result<()> {
        // Tuning transmits: whoever calls this, not during a storm stand-down.
        if let Some(why) = self.storm_reason() {
            return Err(RigError::Protocol(format!("storm stand-down: {why}")));
        }
        self.watchdog_fired.store(false, Ordering::SeqCst);
        *lock(&self.keying_since) = Some(Instant::now());
        let result = self.run_tuner(t0);
        self.record_carrier(t0.elapsed());
        result
    }

    fn run_tuner(&self, t0: Instant) -> civ::Result<()> {
        self.with_rig(|r| r.start_tune())?;
        // The manual does not say how soon 1C 01 reads 02 ("tuning") after the
        // command: allow a moment for it, so that a tune is not taken as finished
        // before it has begun.
        let start_wait = self.cfg.tune_timeout / 20;
        let mut started = false;
        // "Not tuning" readings in a row, once it has started (RX_READINGS).
        let mut done = 0;
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(RigError::Protocol(format!(
                    "the tune went on longer than max_key_seconds ({:?})",
                    self.cfg.max_key
                )));
            }
            if let Some(why) = self.storm_reason() {
                return Err(RigError::Protocol(format!("storm stand-down: {why}")));
            }
            let (busy, tx) = self.with_rig(|r| Ok((r.tuner_busy()?, r.is_transmitting())))?;
            // The manual does not say what 1C 00 reads during a tune: recorded for
            // the bench (docs/hardware-test-plan.md, step 5), and not relied on.
            log::trace!(
                "tuning, {} ms: 1C 01 {}, 1C 00 {}",
                t0.elapsed().as_millis(),
                if busy { "02 (tuning)" } else { "not tuning" },
                match tx {
                    Ok(true) => "01 (TX)".to_string(),
                    Ok(false) => "00 (RX)".to_string(),
                    Err(e) => format!("not read ({e})"),
                }
            );
            started |= busy;
            done = if busy { 0 } else { done + 1 };
            if done >= RX_READINGS && (started || t0.elapsed() > start_wait) {
                break;
            }
            if t0.elapsed() > self.cfg.tune_timeout {
                self.health("tune", "timeout");
                return Err(civ::RigError::Timeout);
            }
            if done == 0 {
                thread::sleep(self.cfg.poll);
            }
        }
        if !started {
            log::warn!("the tuner never read 02 (tuning) after 1C 01 02");
        }
        // Back on receive once the tuner has stopped, within the stuck margin.
        self.wait_for_receive(Instant::now() + self.cfg.stuck_margin)
            .map_err(|e| RigError::Protocol(format!("after the tune: {e}")))
    }

    /// Start listening as `hfnode run` does, at start-up or at the top of a
    /// listening window: [`Station::start_window`] (which also checks the transmit
    /// frequency and split), then, only if the tune
    /// matched (no lockout, no inhibit), `DE <call>` to identify its carrier, keyed
    /// as a transmission of its own with every check of [`Station::transmit`],
    /// the SWR check included. The bench's `radio tune` uses `start_window` alone.
    /// A rig without a tuner (a handheld, the keyer box) keyed nothing, so it
    /// sends no ID either.
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
        // SWR is measured all through every transmission, not once per window: the
        // antenna or a connector can fail in the middle of one.
        (self.swr_measured, self.swr_worst, self.dark) = (false, None, None);
        // On success the radio has been seen back on receive after the last piece;
        // on failure force it there.
        let result = self
            .transmit_inner(tx)
            .or_else(|e| self.force_rx().and(Err(e)));
        match &result {
            // A transmission that went out whole: the faults before it are behind.
            Ok(()) => self.faults = 0,
            // Back on receive now, but the next transmission may stick too.
            Err(TxError::Stuck) => {
                log::error!("radio stuck on transmit: silent until the next tune");
                self.fault("the radio stayed on transmit and had to be forced to receive");
            }
            Err(_) => {}
        }
        // The highest SWR measured, once per transmission (a high one was logged
        // as it stopped it).
        if let Some(worst) = self.swr_worst.take() {
            if !matches!(result, Err(TxError::HighSwr(_))) {
                self.health("swr", &format!("{worst:.2}"));
            }
        }
        if let Err(e) = &result {
            self.health("tx-failed", &e.to_string());
        }
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
            .and_then(|()| self.check_ready())
            .map_err(|e| TxError::NotReady(e.to_string()))?;
        // Timed at the speed the radio's keyer is really using.
        let dot = self.with_rig(|r| r.dot_duration())?;
        let keying = dot * tx.segments.iter().map(|s| cw::units(s)).sum::<u32>();
        if keying > self.cfg.max_transmission {
            return Err(TxError::TooLong {
                keying,
                limit: self.cfg.max_transmission,
            });
        }
        // Keyer-sized like any text: 17 takes "Up to 30 characters" (manual text
        // line 9711), and node_call is not limited in length.
        let id = self.pieces(&self.cfg.station_id, dot);
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
            let pieces = self.pieces(segment, dot);
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

    /// Wait, on receive, for as long as the rig says it must rest before keying
    /// `run` ([`Rig::rest_needed`]: a handheld's duty cycle, or a busy channel), or
    /// for the node's own duty budget ([`StationConfig::duty`]) to have room for its
    /// carrier, asking again after each wait. Not counted as keying by the
    /// watchdog, which only starts timing once the piece is sent.
    fn rest_before_keying(&self, run: Run) -> Result<(), TxError> {
        loop {
            // A storm may have come on during the rest.
            self.check_storm()?;
            let rest = self.rest_needed(run)?;
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

    /// How long to rest on receive before `run`: the rig's own rest
    /// ([`Rig::rest_needed`]) or the duty budget's, whichever is longer.
    fn rest_needed(&self, run: Run) -> Result<Duration, TxError> {
        let rig = self.with_rig(|r| r.rest_needed(run.keying))?;
        Ok(rig.max(self.duty_rest(run.carrier)?))
    }

    /// How long until `carrier` more fits in the duty budget: at most
    /// [`StationConfig::duty`] of any [`StationConfig::duty_window`] with the key
    /// down. Carrier that could never fit is refused.
    fn duty_rest(&self, carrier: Duration) -> Result<Duration, TxError> {
        let window = self.cfg.duty_window;
        let budget = window.mul_f32(self.cfg.duty);
        self.duty.rest(carrier, budget, window).ok_or_else(|| {
            TxError::NotReady(format!(
                "{:.0} s of key-down is more than the duty budget of {:.0} s in {:.0} s",
                carrier.as_secs_f32(),
                budget.as_secs_f32(),
                window.as_secs_f32()
            ))
        })
    }

    /// Count `carrier`, just keyed, toward the duty budget.
    fn record_carrier(&mut self, carrier: Duration) {
        self.duty.record(carrier, self.cfg.duty_window);
    }

    /// Whether to key the ID `id` before `piece` ([`Station::id_due`] over `ahead`),
    /// for a rig that must rest on receive before keying ([`Rig::rest_needed`]: a
    /// handheld's duty cycle, a busy channel), or a duty budget used up. It rests for
    /// the ID alone (normally nothing: [`Station::rest_for_piece_and_id`] before the
    /// last piece left room for it), then counts the rest that `piece` and an ID
    /// after it would need, so that a due ID goes first rather than wait out a rest
    /// only the piece needs. If the ID is not due it takes that rest, and weighs the
    /// ID again in case the rest ran long. With no rest needed this is `id_due`.
    fn id_due_after_rest(
        &self,
        since: Instant,
        piece: &str,
        ahead: &[String],
        ends_with_id: bool,
        id: &[String],
    ) -> Result<bool, TxError> {
        let (id_run, both) = self.piece_and_id_keying(piece, id)?;
        self.rest_before_keying(id_run)?;
        let rest = self.rest_needed(both)?;
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

    /// The ID `id` as a run, and `piece` followed by the ID: the piece counted at
    /// the longest this module lets it keep the radio on transmit
    /// ([`Station::keying_bound`]), since a rig measures what it really keyed (a
    /// handheld's switch-over to transmit, its break-in tail), and a piece that ran
    /// over its Morse length would otherwise leave the ID a rest of its own.
    fn piece_and_id_keying(&self, piece: &str, id: &[String]) -> Result<(Run, Run), TxError> {
        let dot = self.with_rig(|r| r.dot_duration())?;
        let id = Run {
            keying: dot * id.iter().map(|p| cw::units(p)).sum::<u32>(),
            carrier: dot * id.iter().map(|p| cw::mark_units(p)).sum::<u32>(),
        };
        let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
        let piece = Run {
            keying: dot * cw::units(piece) + hang + self.cfg.stuck_margin,
            carrier: dot * cw::mark_units(piece),
        };
        Ok((id, id + piece))
    }

    /// Lock out until the next tune for a fault (high SWR, no output or too much,
    /// keying not heard, a radio left on transmit, no tuner match, a failed tune),
    /// and latch the inhibit at the [`FAULTS_TO_LATCH`]th in a row.
    fn fault(&mut self, what: &str) {
        self.swr_lockout = true;
        self.faults += 1;
        if self.faults >= FAULTS_TO_LATCH {
            self.health("faults", &self.faults.to_string());
            self.tx_inhibit.latch(&format!(
                "{what}: {} faults in a row with no transmission going out whole between \
                 them, so the next tune would only key into the same fault",
                self.faults
            ));
        }
    }

    /// `text` in keyer pieces ([`split_for_keyer`]), each split again, at a space
    /// where it can be, while it could keep the radio on transmit longer than
    /// `max_key` ([`Station::keying_bound`]), which the watchdog would cut off: at 18
    /// wpm, 30 zeros take 44 s. A piece is left whole if even one character would
    /// still be too long (a bench test of the watchdog sets `max_key_seconds` that
    /// low on purpose).
    fn pieces(&self, text: &str, dot: Duration) -> Vec<String> {
        let fits = |p: &str| self.keying_bound(&[p.to_string()], dot) <= self.cfg.max_key;
        split_for_keyer(text)
            .into_iter()
            .flat_map(|p| match fits(&p) {
                true => vec![p],
                false => split_to_fit(&p, fits).unwrap_or_else(|| vec![p]),
            })
            .collect()
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
        let carrier = dot * cw::mark_units(piece);
        let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
        self.rest_before_keying(Run { keying, carrier })?;
        // A rig without meters (a handheld, or any radio on the keyer box) cannot
        // measure SWR or output: it has its own limits instead (a handheld's waited
        // out just above), and may confirm the keying itself. One that only hears its
        // radio shows it back on receive once the audio has caught up, which takes
        // real time whatever the time scale.
        let (meters, settle) = {
            let r = lock(&self.rig);
            (r.has_meters(), r.receive_settle())
        };
        *lock(&self.keying_since) = Some(Instant::now());
        let sent = self.with_rig(|r| r.send_cw(piece));
        let at = Instant::now();
        let stuck_at = at + keying + hang + self.cfg.stuck_margin.max(settle);
        let watched = match sent {
            Err(e) => Err(e.into()),
            Ok(()) if meters => self.watch_piece(at, keying, stuck_at),
            Ok(()) => self.watch_unmetered(at, keying, stuck_at),
        };
        // All of it, also if it was cut off: a lost reply may still have keyed it.
        self.record_carrier(carrier);
        watched?;
        *lock(&self.keying_since) = None;
        Ok(())
    }

    /// Watch a piece the keyer accepted at `sent`, on a rig without meters: the
    /// radio's status says nothing about the keyer until the whole piece has had
    /// time to go out (it reads receive before semi break-in has switched over), so
    /// wait that long first, then for receive, then ask the rig whether it saw the
    /// radio key ([`Rig::keying_confirmed`]).
    fn watch_unmetered(
        &mut self,
        sent: Instant,
        keying: Duration,
        stuck_at: Instant,
    ) -> Result<(), TxError> {
        self.sleep_until(sent + keying)?;
        self.wait_for_receive(stuck_at)?;
        if self.with_rig(|r| r.keying_confirmed())? == Some(false) {
            self.health("keying", "not-heard");
            log::error!("the radio was not heard keying: silent until the next window start");
            self.fault("the radio was not heard keying");
            return Err(TxError::NotHeard);
        }
        Ok(())
    }

    /// Watch a piece the keyer accepted at `sent`, on a rig with meters, from
    /// `swr_delay` after it until the radio is back on receive: each sample reads
    /// the Po meter, the transmit status (1C 00), SWR and the Po meter again.
    ///
    /// - A sample with output on both Po readings (key-up reads SWR 1.0) measures
    ///   SWR: above the limit the transmission stops at once and transmitting is
    ///   locked out until the next tune ([`TxError::HighSwr`]). If the radio reads
    ///   receive in such a sample, its status does not show keyer transmissions,
    ///   so no receive confirmation in this module means anything, and
    ///   transmitting is inhibited. (The manual does not say 1C 00 covers them;
    ///   this checks it on every sample.)
    /// - Output must show: the first piece of a transmission must give one such
    ///   sample, and [`NO_OUTPUT_SAMPLES`] samples in a row taken while keying
    ///   (over `swr_window` at least) with no output on either Po reading stop it
    ///   ([`TxError::NoOutput`]). The radio's protection reacts to its power
    ///   amplifier's temperature, not to SWR directly: into a bad load it first
    ///   keeps its output (SWR reads high), then reduces it once the amplifier is
    ///   hot ("Protection function", p. 13-4; manual text lines 7316-7324).
    /// - The radio reads receive before semi break-in has switched over, so only a
    ///   receive reading once the piece's keying time has passed ends it; still on
    ///   transmit at `stuck_at`, it is stuck ([`TxError::Stuck`]).
    fn watch_piece(
        &mut self,
        sent: Instant,
        keying: Duration,
        stuck_at: Instant,
    ) -> Result<(), TxError> {
        let min_po = self.cfg.swr_min_po;
        let keyed_by = sent + keying;
        self.sleep_until(sent + self.cfg.swr_delay)?;
        let mut rx = 0;
        // At least one sample, however late this thread gets to run.
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            self.check_storm()?;
            let at = Instant::now();
            let (before, tx, swr, after) = self.with_rig(|r| {
                Ok((
                    r.read_po()?,
                    r.is_transmitting()?,
                    r.read_swr()?,
                    r.read_po()?,
                ))
            })?;
            let po = before.max(after);
            if po > self.cfg.po_limit {
                self.health("po", &format!("{po:.0}"));
                log::error!(
                    "Po {po:.0}% above {:.0}% for {} W set: silent until the next tune",
                    self.cfg.po_limit,
                    self.cfg.power_watts
                );
                self.fault(&format!(
                    "output {po:.0}% of full, above what {} W gives",
                    self.cfg.power_watts
                ));
                return Err(TxError::HighPower(po));
            }
            if before.min(after) >= min_po {
                if !tx {
                    self.health("tx-status", "rx-with-output");
                    self.tx_inhibit.latch(
                        "radio reads receive (1C 00) while the Po meter shows output: its \
                         transmit status cannot be trusted",
                    );
                    return Err(TxError::Inhibited);
                }
                self.swr_measured = true;
                self.swr_worst = Some(self.swr_worst.map_or(swr, |w| w.max(swr)));
                if swr > self.cfg.swr_limit {
                    self.health("swr", &format!("{swr:.2}"));
                    log::error!(
                        "SWR {swr:.2} above {:.1}: silent until the next tune",
                        self.cfg.swr_limit
                    );
                    self.fault(&format!("SWR {swr:.2} above {:.1}", self.cfg.swr_limit));
                    return Err(TxError::HighSwr(swr));
                }
            }
            if before.max(after) >= min_po {
                self.dark = None;
            } else if at < keyed_by {
                let run = self.dark.get_or_insert((at, 0));
                run.1 += 1;
                let (since, n) = *run;
                if n >= NO_OUTPUT_SAMPLES && at.duration_since(since) >= self.cfg.swr_window {
                    return Err(self.no_output());
                }
            }
            // Receive read RX_READINGS times in a row once the piece's keying time
            // has passed: CI-V has no checksum. The second sample is taken at once.
            rx = if !tx && at >= keyed_by { rx + 1 } else { 0 };
            if rx >= RX_READINGS {
                break;
            }
            if rx > 0 {
                continue;
            }
            // Timed from before the status was read, so that a thread held up
            // during this sample's other reads does not make a radio that was on
            // transmit before `stuck_at` look stuck.
            if tx && at > stuck_at {
                log::error!("radio still transmitting; forcing receive");
                return Err(TxError::Stuck);
            }
            thread::sleep(self.cfg.poll);
        }
        if !self.swr_measured {
            // The first piece of the transmission, keyed with no output to measure.
            return Err(self.no_output());
        }
        Ok(())
    }

    /// Lock out for [`TxError::NoOutput`].
    fn no_output(&mut self) -> TxError {
        self.health("swr", "no-output");
        log::error!("no output on the Po meter while keying: silent until the next tune");
        self.fault("no output on the Po meter while keying");
        TxError::NoOutput
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

    /// Wait until the radio reports receive [`RX_READINGS`] times in a row, or
    /// declare it stuck at `deadline`. Stops early, to force receive, if the storm
    /// stand-down comes on.
    fn wait_for_receive(&self, deadline: Instant) -> Result<(), TxError> {
        let mut rx = 0;
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            self.check_storm()?;
            if !self.with_rig(|r| r.is_transmitting())? {
                rx += 1;
                if rx >= RX_READINGS {
                    return Ok(());
                }
                continue;
            }
            rx = 0;
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
            max_key: Duration::from_secs(5),
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
            po_limit: po_limit(40),
            duty: duty_for_power(40),
            duty_window: DUTY_WINDOW,
            max_transmission: MAX_TRANSMISSION,
            radio_wait: Duration::from_secs(1),
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

    /// Run `f` on a thread of its own, and fail if it has not returned within
    /// `limit` of real time: a time limit that stopped working would otherwise hang
    /// the test run instead of failing it.
    fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(f());
        });
        match result.recv_timeout(limit) {
            Ok(v) => v,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("still running after {limit:?}: a time limit is not enforced")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("panicked"),
        }
    }

    /// A tune time limit of 100 ms, and the station's other settings.
    fn quick_tune() -> StationConfig {
        let mut c = cfg();
        c.tune_timeout = Duration::from_millis(100);
        c
    }

    #[test]
    fn a_tuner_still_tuning_after_its_time_limit_inhibits() {
        let mut rig = Radio::new(fast_rig());
        rig.tuner_stuck = true;
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut st = Station::new(rig, quick_tune(), Some(health.clone()));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        let (mut st, r) = within(Duration::from_secs(10), move || {
            let r = st.start_window();
            (st, r)
        });
        assert!(r.is_err());
        // Forced to receive, then 1C 01 still reads "tuning": inhibited, and the
        // owner told once.
        assert!(st.rig().lock().unwrap().stops > 0);
        assert!(st.tx_inhibited() && !st.can_transmit() && !st.tuned());
        let n = notices.try_recv().unwrap();
        assert!(n.reason.contains("1C 01"), "{n:?}");
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        let log = std::fs::read_to_string(&health).unwrap();
        assert!(
            log.contains(",tune,timeout") && log.contains(",tune,not-stopped"),
            "{log}"
        );
        assert!(dir.path().join(INHIBIT_FILE).exists());
        assert!(st.rig().lock().unwrap().sim.sent.is_empty());
    }

    #[test]
    fn a_lost_tune_reply_locks_out_until_the_next_tune() {
        // The radio takes 1C 01 02 and tunes (for 150 ms here), but its reply is
        // lost: the tune ends on its own within the time limit any tune has, so
        // the node is locked out, not inhibited.
        let mut rig = Radio::new(fast_rig());
        rig.tune_reply_lost = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        let (mut st, r) = within(Duration::from_secs(10), move || {
            let r = st.start_window();
            (st, r)
        });
        assert!(r.is_err());
        assert!(!st.tx_inhibited() && !st.can_transmit() && !st.tuned());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert!(
            st.rig().lock().unwrap().sim.sent.is_empty(),
            "nothing keyed"
        );
        // The next tune works: transmitting again.
        st.rig().lock().unwrap().tune_reply_lost = false;
        st.start_window().unwrap();
        assert!(st.tuned());
        st.transmit(&tx(&["TEST"])).unwrap();
    }

    #[test]
    fn a_lost_tune_reply_on_a_tuner_that_never_stops_inhibits() {
        let mut rig = Radio::new(fast_rig());
        rig.tune_reply_lost = true;
        rig.tuner_stuck = true;
        let mut st = Station::new(rig, quick_tune(), None);
        st.configure().unwrap();
        let (st, r) = within(Duration::from_secs(10), move || {
            let r = st.start_window();
            (st, r)
        });
        assert!(r.is_err());
        assert!(st.tx_inhibited());
    }

    #[test]
    fn a_tuner_that_cannot_be_read_after_its_tune_timed_out_inhibits() {
        // The tuner still reads tuning at its time limit; once receive is forced,
        // 1C 01 gets no reply. Nothing says the tune ended: inhibited.
        let mut rig = Radio::new(fast_rig());
        rig.tuner_stuck = true;
        rig.tuner_unreadable_after_stop = true;
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut st = Station::new(rig, quick_tune(), Some(health.clone()));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        st.configure().unwrap();
        let (mut st, r) = within(Duration::from_secs(10), move || {
            let r = st.start_window();
            (st, r)
        });
        assert!(r.is_err());
        assert!(
            st.rig().lock().unwrap().stopped_since_tune,
            "receive forced"
        );
        assert!(st.tx_inhibited() && !st.can_transmit() && !st.tuned());
        let n = notices.try_recv().unwrap();
        assert!(
            n.reason.contains("1C 01") && n.reason.contains("could not be read"),
            "{n:?}"
        );
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        let log = std::fs::read_to_string(&health).unwrap();
        assert!(
            log.contains(",tune,timeout") && log.contains(",tune,not-stopped"),
            "{log}"
        );
        assert!(dir.path().join(INHIBIT_FILE).exists());
    }

    #[test]
    fn a_tuner_that_cannot_be_read_after_its_tune_locks_out() {
        let mut rig = Radio::new(fast_rig());
        rig.matched_unreadable = true;
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert!(st.start_window().is_err());
        assert!(!st.tx_inhibited(), "the tuner reads as stopped");
        assert!(!st.can_transmit() && !st.tuned());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
    }

    #[test]
    fn swr_is_checked_only_with_output_present() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // No sample reaches the Po threshold: not checked, and treated as a fault.
        st.cfg.swr_min_po = 1000.0;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::NoOutput));
        assert!(!st.swr_measured);
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
    fn a_tune_that_never_started_or_failed_is_not_counted() {
        let mut st = Station::new(Radio::new(fast_rig()), cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        assert!(st.tuned());
        // The radio cannot be set up for the tune: no tune, and locked out until
        // one runs.
        st.rig().lock().unwrap().sim.split_tx_hz = Some(7_040_000);
        assert!(st.start_window().is_err());
        assert!(!st.tuned());
        st.rig().lock().unwrap().sim.split_tx_hz = None;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        st.start_window().unwrap();
        assert!(st.tuned());
        st.transmit(&tx(&["TEST"])).unwrap();
        // A tuner that could not match has given its answer: done, and locked out.
        st.rig().lock().unwrap().sim.tuner_bypassed = true;
        assert!(st.start_window().is_err());
        assert!(st.tuned());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        // A tune whose reply was lost is not, so the node tunes again before its
        // next reply.
        {
            let rig = st.rig();
            let mut r = rig.lock().unwrap();
            r.sim.tuner_bypassed = false;
            r.tune_reply_lost = true;
        }
        assert!(st.start_window().is_err());
        assert!(!st.tuned());
        assert_eq!(st.rig().lock().unwrap().sim.tunes, 4);
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
        // A tuner error, high SWR and a stuck key, each followed by a transmission
        // that goes out whole: receive is confirmed each time.
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        st.rig().lock().unwrap().tune_reply_lost = false;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        st.rig().lock().unwrap().sim.swr = 3.5;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        st.rig().lock().unwrap().sim.swr = 1.2;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        st.rig().lock().unwrap().sim.stuck_key = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Stuck));
        assert!(!st.tx_inhibited());
        assert!(notices.try_recv().is_err());
        // Locked out until the next tune, as for high SWR (the safety audit's K7).
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        st.rig().lock().unwrap().sim.stuck_key = false;
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        assert!(!st.tx_inhibited());
        drop(st);
        assert!(notices.try_recv().is_err());
    }

    /// The safety audit's K10: a fault that recurs at once is not tuned and keyed
    /// into again and again (a cut coax at every window), whichever faults they are.
    #[test]
    fn a_second_fault_in_a_row_latches_the_inhibit() {
        type Setup = fn(&mut Radio);
        let faults: [(&str, Setup); 4] = [
            ("high SWR", |r| r.sim.swr = 3.5),
            ("no match", |r| r.sim.tuner_bypassed = true),
            ("lost tune reply", |r| r.tune_reply_lost = true),
            ("stuck key", |r| r.sim.stuck_key = true),
        ];
        for (a, set_a) in faults {
            for (b, set_b) in faults {
                let mut st = Station::new(Radio::new(fast_rig()), cfg(), None);
                let (to, notices) = mpsc::channel();
                st.notify_inhibit(to);
                st.configure().unwrap();
                for set in [set_a, set_b] {
                    let rig = st.rig();
                    let mut r = rig.lock().unwrap();
                    (r.sim.swr, r.sim.tuner_bypassed, r.tune_reply_lost) = (1.2, false, false);
                    set(&mut r);
                    drop(r);
                    if st.start_window().is_ok() {
                        // A fault the tune does not show: the next transmission does.
                        assert!(st.transmit(&tx(&["TEST"])).is_err(), "{a}, {b}");
                        st.rig().lock().unwrap().sim.stuck_key = false;
                    }
                }
                assert!(st.tx_inhibited(), "{a}, then {b}");
                let why = notices.try_recv().unwrap().reason;
                assert!(why.contains("2 faults in a row"), "{a}, {b}: {why}");
                assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
            }
        }
    }

    #[test]
    fn a_good_transmission_between_faults_starts_the_count_again() {
        let mut st = Station::new(Radio::new(fast_rig()), cfg(), None);
        st.configure().unwrap();
        for _ in 0..3 {
            st.rig().lock().unwrap().sim.swr = 3.5;
            st.start_window().unwrap();
            assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
            // A matched tune alone does not start it again: the tuner matched into
            // a load that faulted at once before.
            st.rig().lock().unwrap().sim.swr = 1.2;
            st.start_window().unwrap();
            st.transmit(&tx(&["TEST"])).unwrap();
        }
        assert!(!st.tx_inhibited());
        st.rig().lock().unwrap().sim.swr = 3.5;
        st.start_window().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        st.start_window().unwrap();
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
        assert!(st.tx_inhibited());
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

    #[test]
    #[cfg(unix)]
    fn an_inhibit_that_cannot_be_checked_counts_as_there() {
        // `Path::exists` says "no" when the state directory cannot be searched, and
        // a command run as another user keyed with the inhibit in place (the safety
        // audit's K6). Root may search any directory, so the error here is another
        // one that root gets too: a "state directory" that is a regular file.
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::write(&state, "not a directory").unwrap();
        let latch = InhibitLatch::in_dir(&state);
        assert!(latch.is_set());
        let file = state.join(INHIBIT_FILE);
        let mut st = Station::new(fast_rig(), cfg(), Some(state.join("health.csv")));
        let (to, notices) = mpsc::channel();
        st.notify_inhibit(to);
        let n = notices.try_recv().expect("notice at start-up");
        assert!(n.from_file && n.at.is_none(), "{n:?}");
        assert_eq!(n.file.as_deref(), Some(file.as_path()));
        assert!(
            n.reason
                .starts_with(&format!("could not check for {}", file.display())),
            "{n:?}"
        );
        st.configure().unwrap();
        assert!(st.start_window().is_err());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
        assert!(st.rig().lock().unwrap().sent.is_empty());
        drop(st);
        // Anything at that name is the file, a link to nowhere too.
        let state = dir.path().join("linked");
        std::fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), state.join(INHIBIT_FILE)).unwrap();
        assert!(InhibitLatch::in_dir(&state).is_set());
        // Nothing there, in a directory that does not exist yet: not inhibited.
        assert!(!InhibitLatch::in_dir(&dir.path().join("not-yet")).is_set());
    }

    #[test]
    fn a_state_directory_must_take_a_file_before_anything_keys() {
        let dir = tempfile::tempdir().unwrap();
        // Made if it is not there, and left as it was found.
        let state = dir.path().join("a/b/state");
        check_state_dir(&state).unwrap();
        assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
        check_state_dir(&state).unwrap();
        // Under a regular file there is no directory to write in, also for root.
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        for bad in [file.clone(), file.join("state")] {
            let e = format!("{:#}", check_state_dir(&bad).unwrap_err());
            assert!(
                e.starts_with(&format!("state_dir {} cannot be written", bad.display())),
                "{e}"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_state_directory_that_takes_no_files_is_refused() {
        // A directory that is there but cannot be written: Linux's /proc takes no new
        // files even from root, which ignores a directory's mode bits.
        let e = format!("{:#}", check_state_dir(Path::new("/proc")).unwrap_err());
        assert!(e.starts_with("state_dir /proc cannot be written"), "{e}");
    }

    /// A [`SimRig`] with things a real radio may do, among them: cut its output
    /// back into a bad load, and stay on transmit for the break-in delay after being
    /// told to stop. The radio's protection reacts to its power amplifier's
    /// temperature ("Protection function", p. 13-4; manual text lines 7316-7324);
    /// `foldback` stands in for that with a fifth of the output above 3:1.
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
        /// Once this many pieces have been keyed, the transmit status reads receive
        /// whatever the radio is doing.
        blind_after: Option<usize>,
        /// The tuner starts, but the reply to the command is lost.
        tune_reply_lost: bool,
        /// Stop-CW commands received.
        stops: u32,
        /// The tuner reads "tuning" for ever.
        tuner_stuck: bool,
        /// Whether the tuner matched cannot be read after a tune.
        matched_unreadable: bool,
        /// The tuner's state (1C 01) cannot be read once a stop command has come
        /// after a tune began.
        tuner_unreadable_after_stop: bool,
        /// A tune has begun, and a stop command has come since.
        tune_started: bool,
        stopped_since_tune: bool,
        /// Once this many pieces have been keyed, the next and later ones find the
        /// load at this SWR.
        swr_after: Option<(usize, f32)>,
        /// This long after the first piece was accepted, the load goes to this SWR.
        swr_at: Option<(Duration, f32)>,
        /// Once this many pieces have been keyed, the Po meter reads nothing.
        dead_after: Option<usize>,
        /// When each keyer piece was accepted.
        sent_at: Vec<(Instant, String)>,
        /// Rests on receive it asks for, as a handheld's duty cycle does.
        rests: Option<Rests>,
        /// The first SWR reading taken once the keyer has finished, while the radio
        /// is still on transmit for its break-in delay, is held up this long (the
        /// station's thread descheduled on a busy computer).
        hold_up_in_hang: Option<Duration>,
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
                blind_after: None,
                tune_reply_lost: false,
                stops: 0,
                tuner_stuck: false,
                matched_unreadable: false,
                tuner_unreadable_after_stop: false,
                tune_started: false,
                stopped_since_tune: false,
                swr_after: None,
                swr_at: None,
                dead_after: None,
                sent_at: Vec::new(),
                rests: None,
                hold_up_in_hang: None,
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
            (self.tune_started, self.stopped_since_tune) = (true, false);
            if self.tune_reply_lost {
                return Err(RigError::Timeout);
            }
            Ok(())
        }
        fn tuner_busy(&mut self) -> civ::Result<bool> {
            if self.tuner_unreadable_after_stop && self.stopped_since_tune {
                return Err(RigError::Timeout);
            }
            Ok(self.tuner_stuck || self.sim.tuner_busy()?)
        }
        fn tuner_matched(&mut self) -> civ::Result<bool> {
            if self.matched_unreadable {
                return Err(RigError::Timeout);
            }
            self.sim.tuner_matched()
        }
        fn transmit_frequency(&mut self) -> civ::Result<u64> {
            self.sim.transmit_frequency()
        }
        fn split_or_delta_tx(&mut self) -> civ::Result<bool> {
            self.sim.split_or_delta_tx()
        }
        fn read_swr(&mut self) -> civ::Result<f32> {
            if let Some(d) = self.hold_up_in_hang {
                if !self.sim.keyer_busy() && self.sim.is_transmitting()? {
                    self.hold_up_in_hang = None;
                    thread::sleep(d);
                }
            }
            if let (Some((after, swr)), Some((first, _))) = (self.swr_at, self.sent_at.first()) {
                if first.elapsed() >= after {
                    self.sim.swr = swr;
                }
            }
            self.sim.read_swr()
        }
        fn read_po(&mut self) -> civ::Result<f32> {
            if self.dead_after.is_some_and(|n| self.sim.sent.len() > n) {
                return Ok(0.0);
            }
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
            if let Some((n, swr)) = self.swr_after.filter(|(n, _)| self.sim.sent.len() > *n) {
                self.sim.swr = swr;
                let _ = n;
            }
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
            self.stopped_since_tune |= self.tune_started;
            if let Some(lag) = self.stop_lag.take() {
                thread::sleep(lag);
            }
            self.hang()?;
            self.sim.stop_cw()
        }
        fn is_transmitting(&mut self) -> civ::Result<bool> {
            if self.status_blind || self.blind_after.is_some_and(|n| self.sim.sent.len() > n) {
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

    /// Four 29-character pieces, about 1 s each at `fast_rig`'s speed.
    fn four_pieces() -> Transmission {
        tx(&[&["TEST"; 24].join(" ")])
    }

    #[test]
    fn swr_is_watched_on_every_piece_not_just_the_first() {
        // The load fails after the first piece: the second is cut off.
        let mut rig = Radio::new(fast_rig());
        rig.swr_after = Some((1, 3.5));
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.start_window().unwrap();
        assert_eq!(split_for_keyer(&four_pieces().segments[0]).len(), 4);
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::HighSwr(3.5)));
        assert!(!st.can_transmit());
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::SwrLockout));
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert_eq!(r.sim.sent.len(), 2, "stopped in the second piece");
        assert!(!r.is_transmitting().unwrap() && !r.sim.keyer_busy());
    }

    #[test]
    fn swr_is_watched_for_the_whole_of_a_piece() {
        // One piece, and the load fails 0.4 s into it (4 s at the radio's speed):
        // stopped then, not when the piece ends.
        let mut rig = Radio::new(fast_rig());
        rig.swr_at = Some((Duration::from_millis(400), 3.5));
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        let piece = ["TEST"; 6].join(" ");
        let keying = st.rig().lock().unwrap().dot_duration().unwrap() * cw::units(&piece);
        assert!(keying > Duration::from_millis(900), "{keying:?}");
        let t0 = Instant::now();
        assert_eq!(st.transmit(&tx(&[&piece])), Err(TxError::HighSwr(3.5)));
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(800),
            "stopped {took:?} after the start"
        );
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!(r.sim.sent.len(), 1);
        assert!(!r.sim.keyer_busy());
    }

    #[test]
    fn output_lost_after_the_first_piece_stops_keying() {
        // The radio cuts its output (its amplifier hot into a bad load) after the
        // first piece: no SWR to measure, so the node stops in the second.
        let mut rig = Radio::new(fast_rig());
        rig.dead_after = Some(1);
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut st = Station::new(rig, cfg(), Some(health.clone()));
        st.configure().unwrap();
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::NoOutput));
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::SwrLockout));
        let rig = st.rig();
        let mut r = rig.lock().unwrap();
        assert_eq!(r.sim.sent.len(), 2, "stopped in the second piece");
        assert!(!r.is_transmitting().unwrap());
        let log = std::fs::read_to_string(&health).unwrap();
        assert!(log.contains(",swr,no-output"), "{log}");
    }

    #[test]
    fn output_while_the_status_reads_receive_inhibits_on_any_piece() {
        // The transmit status goes blind after the first piece: the second, keyed
        // with output, reads receive, and transmitting is inhibited.
        let mut rig = Radio::new(fast_rig());
        rig.blind_after = Some(1);
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::Inhibited));
        assert!(st.tx_inhibited());
        assert_eq!(st.transmit(&four_pieces()), Err(TxError::Inhibited));
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!(r.sim.sent.len(), 2, "stopped in the second piece");
        assert!(!r.sim.keyer_busy());
    }

    #[test]
    fn swr_just_above_the_limit_stops_and_at_it_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut st = Station::new(fast_rig(), cfg(), Some(health.clone()));
        st.configure().unwrap();
        assert_eq!(st.cfg.swr_limit, 2.0);
        st.rig().lock().unwrap().swr = 2.0;
        st.transmit(&tx(&["TEST"])).unwrap();
        let log = std::fs::read_to_string(&health).unwrap();
        assert!(log.contains(",swr,2.00"), "measured: {log}");
        st.rig().lock().unwrap().swr = 2.1;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(2.1)));
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
    }

    #[test]
    fn a_sample_held_up_in_the_hang_time_is_not_a_stuck_radio() {
        // The radio was on transmit (in its break-in delay) when the sample read
        // its status; the sample's SWR read is then held up past the stuck margin.
        // The radio was not stuck: the next sample finds it on receive.
        let mut rig = Radio::new(fast_rig());
        rig.hold_up_in_hang = Some(cfg().stuck_margin + Duration::from_millis(200));
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert!(r.hold_up_in_hang.is_none(), "the hold-up happened");
        assert_eq!(r.stops, 0, "receive was not forced");
    }

    #[test]
    fn swr_is_logged_once_per_transmission() {
        let dir = tempfile::tempdir().unwrap();
        let health = dir.path().join("health.csv");
        let mut st = Station::new(Radio::new(fast_rig()), cfg(), Some(health.clone()));
        st.configure().unwrap();
        st.transmit(&four_pieces()).unwrap();
        let log = std::fs::read_to_string(&health).unwrap();
        assert_eq!(log.matches(",swr,1.30").count(), 1, "{log}");
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
        // The watchdog's look at the radio while idle may find it first.
        let why = notices.try_recv().unwrap().reason;
        assert!(
            why == "radio not confirmed on receive (unexpected reply: radio still reports transmit)"
                || why.starts_with("the radio transmitted with nothing from the node keying it"),
            "{why}"
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
            let (st, r) = within(Duration::from_secs(10), move || {
                let r = st.open_window();
                (st, r)
            });
            assert!(r.is_err(), "{name}");
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
        assert!(!st.tuned());
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
        assert!(st.tuned());
        let rig = st.rig();
        let r = rig.lock().unwrap();
        assert_eq!((r.tunes, r.sent.join(" ")), (1, "TEST".to_string()));
    }

    #[test]
    fn a_window_start_cut_short_by_a_storm_keeps_the_lockout() {
        // The tuner could not match: locked out. The next window start is stopped
        // by a storm before it tunes, and the storm then passes: still locked out,
        // since nothing has been tuned since the no-match.
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.rig().lock().unwrap().tuner_bypassed = true;
        assert!(st.start_window().is_err());
        assert!(!st.can_transmit());
        st.rig().lock().unwrap().tuner_bypassed = false;
        let hold = StormHold::new(Duration::from_secs(60));
        st.set_storm_hold(hold.clone());
        let err = st.start_window().unwrap_err().to_string();
        assert!(err.contains("storm stand-down"), "{err}");
        hold.set(None);
        assert!(!st.can_transmit());
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::SwrLockout));
        assert!(st.rig().lock().unwrap().sent.is_empty(), "nothing keyed");
        // A tune that matches clears it.
        st.start_window().unwrap();
        st.transmit(&tx(&["TEST"])).unwrap();
        assert_eq!(st.rig().lock().unwrap().tunes, 2);
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
