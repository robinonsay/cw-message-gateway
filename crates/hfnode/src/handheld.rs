//! A handheld as the node's radio (`station.rig = "handheld"`), for trying the whole
//! system out locally on 2 m before the IC-7300: a Quansheng UV-K1 or UV-K5 v3
//! running the NR7Y CW firmware with hfnode's commands added (firmware/uv-k1), which
//! keys a carrier on commands over the radio's USB-C port, the way the IC-7300's
//! keyer takes CI-V commands. The command set is in [`proto`] and
//! docs/handheld-protocol.md; [`mock`] simulates a radio running it.
//!
//! The station's safety layer ([`crate::station`]) drives it like the IC-7300: the
//! software watchdog, keyer pieces of at most 30 characters with receive confirmed
//! after each, the transmit inhibit, the storm stand-down, and the radio checked
//! (frequency, transmit frequency equal to it, mode, power and break-in) before
//! every transmission. The firmware sets nothing: those are set by hand at the
//! radio, and the node refuses to transmit, saying what to change, when they read
//! back otherwise. What a handheld does not have, and what covers it here:
//!
//! - **No SWR or power meter.** The firmware's own transmit state
//!   ([`proto::Status`], which reads the radio chip's transmit bit) is read back
//!   instead, and a run read back as ended well before its text could have gone out
//!   fails the transmission. Each keying run is bounded besides the station's
//!   watchdog: the node stops a run that goes on past the end of its text (plus
//!   `run_slack`, and never past `max_key_seconds` plus [`DEADMAN_MARGIN`]), and
//!   fails the transmission; while a run lasts the node sends `STATUS` often enough
//!   to keep the firmware's link timeout from expiring, and no longer, so that the
//!   link timeout ends the run if the node dies or the cable is pulled; the firmware
//!   has a transmit limit of its own of at most a minute, checks that every stop
//!   turned the transmitter off and kept it off for a second (refusing `CW` with
//!   `WAIT` meanwhile, which the node waits out), keeps its own budget of time
//!   keyed, and has a hardware watchdog that resets the radio if it hangs during a
//!   run (firmware/uv-k1/README.md). The radio's own time-out timer does not work
//!   in CW.
//! - **No tuner**: a window start checks the radio and transmits nothing.
//! - **A small transmitter**: at most `max_duty_percent` of any `duty_window_secs`
//!   on the air, within the firmware's own budget ([`MAX_DUTY_BUDGET`]); a long
//!   reply waits on receive between keying runs.
//! - **A shared channel**: the node keys only once the squelch has been closed for
//!   `busy_quiet_ms` (the firmware reports how long), and gives up after
//!   `busy_max_wait_secs`.

pub mod link;
pub mod mock;
pub mod proto;

use crate::config::{Config, RigKind};
use anyhow::{bail, Context, Result};
use civ::{Rig, RigError, MAX_CW_CHARS};
use link::Link;
use proto::{Command, Hello, Power, Status};
use serde::Deserialize;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How much longer than `max_key_seconds` a keying run may last before the node
/// stops it here: the station's watchdog should have acted by then.
pub const DEADMAN_MARGIN: Duration = Duration::from_secs(5);

/// The longest transmit limit the firmware may keep.
pub const MAX_FIRMWARE_TX_LIMIT: Duration = Duration::from_secs(60);
/// The firmware's link timeout must lie between these: long enough that one lost
/// reply (the node waits [`REPLY_TIMEOUT`] before sending again) does not end a
/// run, short enough to end one soon after the node dies.
pub const MIN_LINK_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_LINK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the node waits for each reply from the firmware. A `CW` is answered
/// once keying has begun, within 300 ms.
pub const REPLY_TIMEOUT: Duration = Duration::from_millis(500);
/// From the firmware's main loop stopping during a run to its watchdog resetting
/// the radio: a second before it stops feeding the watchdog, and the watchdog's own
/// 2 s (2048 counts of a 32 kHz clock divided by 32; the clock is not precise).
pub const WATCHDOG_RESET: Duration = Duration::from_secs(3);
/// After a stop the firmware watches the transmitter for a second, and answers `CW`
/// with `ERR CW WAIT` meanwhile: the node tries again for this long.
pub const STOP_WATCH_WAIT: Duration = Duration::from_millis(1500);
/// The most time on the air the duty cycle may allow at once
/// (`duty_window_secs` times `max_duty_percent`): within the firmware's own budget
/// of time keyed, which refuses `CW` past 165 s and gets back a second for every
/// second off the air.
pub const MAX_DUTY_BUDGET: Duration = Duration::from_secs(150);
/// Allowed per character on top of a run's Morse length. A gap longer than the
/// radio's break-in tail (a character gap below about 12 wpm, a word gap below about
/// 28 wpm) ends the transmission, and the next element first switches the radio
/// back to transmit: the audio path's 20 ms and the transmitter's set-up, from the
/// firmware's code, not measured. The firmware times that element from when its
/// carrier is on, so the switch-over adds to the run.
pub const TX_START_ALLOWANCE: Duration = Duration::from_millis(50);
/// When a hang test passes: the firmware restarted this long after it hung,
/// its start-up included. The watchdog takes about [`WATCHDOG_RESET`], and the
/// operator is told to switch the radio off if it is still sending after 10 s.
pub const HANG_RESTART: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(1)..=Duration::from_secs(8);

/// The US amateur bands a UV-K1 or UV-K5 covers, in Hz, where CW may be sent
/// anywhere (47 CFR 97.305(a), from memory, not checked against the eCFR): 2 m,
/// 1.25 m and 70 cm. The node refuses any other frequency.
pub const BANDS_HZ: [(u64, u64); 3] = [
    (144_000_000, 148_000_000),
    (222_000_000, 225_000_000),
    (420_000_000, 450_000_000),
];

/// The CW and weak-signal ends of those bands (ARRL band plan, from memory):
/// elsewhere CW is legal but where nobody expects it, among FM simplex and
/// repeater channels.
const WEAK_SIGNAL_HZ: [(u64, u64); 3] = [
    (144_000_000, 144_275_000),
    (222_000_000, 222_150_000),
    (432_000_000, 432_100_000),
];

/// The band in [`BANDS_HZ`] holding `hz`.
pub fn band_of(hz: u64) -> Option<(u64, u64)> {
    BANDS_HZ
        .iter()
        .copied()
        .find(|&(lo, hi)| (lo..=hi).contains(&hz))
}

/// Bring-up stages for a handheld (docs/handheld.md, "Bring-up"):
/// `[handheld] commissioned` names the last one passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Nothing has passed: `hfnode handheld check` and `rx`, `listen` and `record`
    /// only; none of them keys the radio.
    #[default]
    None,
    /// `hfnode listen` decoded the other handheld's Morse correctly. Allows
    /// `hfnode handheld key` and `linktest`, with the operator at the radio.
    Listen,
    /// Short transmissions were heard correctly on the other handheld and read back
    /// as ended; the link test showed the firmware stopping on its own. Allows
    /// `hfnode handheld hangtest`.
    Keying,
    /// The hang test showed the firmware's watchdog ending a transmission. Allows
    /// `run`.
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
    Hang,
    Run,
}

/// Whether `action` may run with `[handheld] commissioned` at `stage`.
pub fn check_stage(stage: Stage, action: Action) -> Result<()> {
    let (needs, name) = match action {
        Action::Key => (Stage::Listen, "this keying command"),
        Action::Hang => (Stage::Keying, "the hang test"),
        Action::Run => (Stage::Done, "`run`"),
    };
    if stage < needs {
        bail!(
            "{name} needs bring-up stage `{needs}` to have passed, but \
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
    if band_of(s.frequency_hz).is_none() {
        bail!(
            "station.frequency_hz {} is outside the amateur bands a handheld covers: \
             144-148, 222-225 or 420-450 MHz",
            s.frequency_hz
        );
    }
    if !WEAK_SIGNAL_HZ
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&s.frequency_hz))
    {
        log::warn!(
            "station.frequency_hz {} is outside the CW and weak-signal end of its band \
             (144.000-144.275, 222.000-222.150, 432.000-432.100 MHz): legal for CW, but \
             among FM channels whose users will not expect it",
            s.frequency_hz
        );
    }
    if s.serial_port.trim().is_empty() {
        bail!("station.serial_port must name the handheld's serial port");
    }
    if ![9600, 19_200, 38_400, 57_600, 115_200].contains(&h.baud) {
        bail!("handheld.baud must be 9600, 19200, 38400, 57600 or 115200");
    }
    if s.max_key_seconds > MAX_FIRMWARE_TX_LIMIT.as_secs() {
        bail!(
            "station.max_key_seconds must be {} or less with a handheld: the firmware \
             ends any keying run at its own limit",
            MAX_FIRMWARE_TX_LIMIT.as_secs()
        );
    }
    // The firmware keeps the transmitter to about half the time.
    if !(10..=50).contains(&h.max_duty_percent) {
        bail!("handheld.max_duty_percent must be 10-50");
    }
    if !(60..=3600).contains(&h.duty_window_secs) {
        bail!("handheld.duty_window_secs must be 60-3600");
    }
    // The longest keying run the station allows must fit in the budget, or it
    // could never be keyed.
    let budget = h.duty_window_secs * u64::from(h.max_duty_percent) / 100;
    if budget > MAX_DUTY_BUDGET.as_secs() {
        bail!(
            "handheld.max_duty_percent of handheld.duty_window_secs allows {budget} s on \
             the air at once; the firmware's own limit allows {} s",
            MAX_DUTY_BUDGET.as_secs()
        );
    }
    if budget < s.max_key_seconds {
        bail!(
            "handheld.max_duty_percent of handheld.duty_window_secs allows {budget} s on \
             the air, less than one keying run of station.max_key_seconds ({} s)",
            s.max_key_seconds
        );
    }
    if h.busy_quiet_ms > 10_000 {
        bail!("handheld.busy_quiet_ms must be 0-10000");
    }
    if !(1..=600).contains(&h.busy_max_wait_secs) {
        bail!("handheld.busy_max_wait_secs must be 1-600");
    }
    Ok(())
}

/// Whether the node can work with the firmware that sent `h`: this protocol
/// version, and limits of its own no looser than [`MAX_FIRMWARE_TX_LIMIT`] and
/// [`MAX_LINK_TIMEOUT`].
pub fn check_hello(h: &Hello) -> Result<()> {
    if h.version != proto::VERSION {
        bail!(
            "the firmware speaks version {} of the handheld protocol; hfnode speaks {}",
            h.version,
            proto::VERSION
        );
    }
    if h.tx_limit.is_zero() || h.tx_limit > MAX_FIRMWARE_TX_LIMIT {
        bail!(
            "the firmware's own transmit limit is {} s; it must be 1-{} s",
            h.tx_limit.as_secs(),
            MAX_FIRMWARE_TX_LIMIT.as_secs()
        );
    }
    if h.link_timeout < MIN_LINK_TIMEOUT || h.link_timeout > MAX_LINK_TIMEOUT {
        bail!(
            "the firmware's link timeout is {} ms; it must be {}-{} ms",
            h.link_timeout.as_millis(),
            MIN_LINK_TIMEOUT.as_millis(),
            MAX_LINK_TIMEOUT.as_millis()
        );
    }
    Ok(())
}

/// How the handheld rig behaves. Durations are real time.
#[derive(Debug, Clone)]
pub struct Settings {
    pub power: Power,
    /// Share of `duty_window` that may be spent on the air.
    pub duty: f32,
    pub duty_window: Duration,
    /// Quiet needed on the frequency before keying; zero turns the check off.
    pub busy_quiet: Duration,
    pub busy_max_wait: Duration,
    /// Allowed beyond the end of a run's text before the node stops it.
    pub run_slack: Duration,
    /// The node stops any run that has lasted this long.
    pub max_run: Duration,
    /// How long to wait for each reply from the firmware.
    pub reply_timeout: Duration,
    /// Morse speed factor: 1 on the air, faster in tests (where the firmware's
    /// keyer is sped up alike).
    pub time_scale: f32,
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let Some(h) = &cfg.handheld else {
            bail!("no [handheld] section");
        };
        Ok(Self {
            power: h.power,
            duty: h.max_duty_percent as f32 / 100.0,
            duty_window: Duration::from_secs(h.duty_window_secs),
            busy_quiet: Duration::from_millis(h.busy_quiet_ms),
            busy_max_wait: Duration::from_secs(h.busy_max_wait_secs),
            run_slack: Duration::from_secs(2),
            max_run: Duration::from_secs(cfg.station.max_key_seconds) + DEADMAN_MARGIN,
            reply_timeout: REPLY_TIMEOUT,
            time_scale: 1.0,
        })
    }

    fn dot(&self, wpm: u32) -> Duration {
        Duration::from_secs_f32(1.2 / wpm.max(1) as f32 / self.time_scale.max(0.001))
    }
}

/// The link, and what the node knows of the firmware's keying, shared with the
/// thread that keeps a run's link alive. Where both are taken, `state` is taken
/// before `link`.
struct Shared {
    link: Mutex<Link>,
    state: Mutex<State>,
    /// Stops the keep-alive thread.
    closed: AtomicBool,
}

#[derive(Debug, Default)]
struct State {
    /// The firmware may be sending: from a `CW` command until a `STATUS` reads
    /// receive or a `STOP` is answered.
    keyed: bool,
    /// When the current run was started.
    since: Option<Instant>,
    /// When its text should have gone out.
    end: Option<Instant>,
    /// When the node stops the current run, whatever the firmware says.
    deadline: Option<Instant>,
    /// The current run was stopped, or that was tried, at its deadline: no more
    /// keep-alives for it, so the firmware's link timeout ends it if nothing else
    /// has.
    abandoned: bool,
    /// Counts keying runs.
    run: u64,
    /// When the firmware was sending, within the duty window.
    on_air: VecDeque<(Instant, Instant)>,
    /// A run that did not end as it should have (cut short, or stopped by the node
    /// for going on too long), not yet reported: the next status read through the
    /// rig fails with it, so that the station does not count the piece as sent.
    fault: Option<String>,
}

impl State {
    fn ended(&mut self) {
        if let (true, Some(since)) = (self.keyed, self.since) {
            self.on_air.push_back((since, Instant::now()));
        }
        self.keyed = false;
        self.since = None;
        self.end = None;
        self.deadline = None;
        self.abandoned = false;
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn request(&self, cmd: &Command) -> civ::Result<Vec<String>> {
        lock(&self.link).request(cmd)
    }

    /// `CW`, tried again while the firmware answers `WAIT` (it is still watching
    /// its last stop), for up to [`STOP_WATCH_WAIT`].
    fn request_cw(&self, cmd: &Command) -> civ::Result<Vec<String>> {
        let until = Instant::now() + STOP_WATCH_WAIT;
        loop {
            match lock(&self.link).request_reply(cmd)? {
                proto::Reply::Ok(fields) => return Ok(fields),
                proto::Reply::Err(code) if code == "WAIT" && Instant::now() < until => {
                    log::debug!("handheld: still checking its last stop; trying again");
                    thread::sleep(Duration::from_millis(100));
                }
                proto::Reply::Err(code) => return Err(link::refused(cmd, &code)),
            }
        }
    }

    /// Read the firmware's status, and note the end of the run under way if it has
    /// ended.
    fn status(&self) -> civ::Result<Status> {
        let run = lock(&self.state).run;
        let s = Status::parse(&self.request(&Command::Status)?).map_err(RigError::Protocol)?;
        let mut st = lock(&self.state);
        if !s.tx && st.keyed && st.run == run {
            // The end is seen at the first status read after it, so a run read back
            // as ended this early really was cut short. A firmware keying at the
            // speed asked for takes the whole length; a fifth is allowed for
            // rounding.
            if let (Some(since), Some(end)) = (st.since, st.end) {
                let now = Instant::now();
                if now < since + (end - since).mul_f32(0.8) {
                    st.fault = Some(format!(
                        "the handheld stopped sending after {:.1} s of a {:.1} s run: cut \
                         short (its link timeout or transmit limit?)",
                        (now - since).as_secs_f32(),
                        (end - since).as_secs_f32()
                    ));
                }
            }
            st.ended();
        }
        Ok(s)
    }

    /// `STOP`. The firmware answers at once; its transmitter may still read on for
    /// a moment after, so callers confirm receive with `STATUS`.
    fn stop(&self) -> civ::Result<()> {
        let run = lock(&self.state).run;
        self.request(&Command::Stop)?;
        let mut st = lock(&self.state);
        if st.run == run {
            st.ended();
        }
        Ok(())
    }

    /// Stop run `run` for going on past its deadline, if it is still that run and
    /// keyed: the state stays locked throughout, so that no new run can start in
    /// between and be stopped instead. Keep-alives for it end here either way.
    fn stop_overdue(&self, run: u64) {
        let mut st = lock(&self.state);
        if !st.keyed || st.run != run || st.abandoned {
            return;
        }
        log::error!("the handheld is still sending past the end of its text: stopping it");
        let stopped = lock(&self.link).request(&Command::Stop);
        let why = "the handheld went on sending past the end of its text, and the node \
                   stopped it";
        match stopped {
            Ok(_) => {
                st.ended();
                st.fault = Some(why.into());
            }
            Err(e) => {
                log::error!(
                    "handheld: STOP failed ({e}); no longer keeping the link alive, so the \
                     firmware's link timeout ends the transmission"
                );
                st.abandoned = true;
                st.fault = Some(format!("{why}, but STOP failed ({e})"));
            }
        }
    }
}

/// While a run lasts, keep the firmware's link alive with `STATUS` every `every`;
/// at the run's deadline, stop it and stop keeping it alive.
fn keep_alive(shared: &Shared, every: Duration) {
    while !shared.closed.load(Ordering::SeqCst) {
        thread::sleep(every);
        let (run, overdue) = {
            let st = lock(&shared.state);
            if !st.keyed || st.abandoned {
                continue;
            }
            (st.run, st.deadline.is_none_or(|d| Instant::now() >= d))
        };
        if overdue {
            shared.stop_overdue(run);
        } else if let Err(e) = shared.status() {
            log::warn!("handheld: status during a keying run: {e}");
        }
    }
}

/// A handheld running the CW firmware. See the module notes.
pub struct Handheld {
    set: Settings,
    wpm: u32,
    shared: Arc<Shared>,
    hello: Hello,
    /// When the node started waiting for the frequency to clear, and when it last
    /// looked.
    busy_since: Option<Instant>,
    busy_looked: Option<Instant>,
    keeper: Option<JoinHandle<()>>,
    describe: String,
    /// When its `HELLO` was answered.
    hello_at: Instant,
    /// The hang test stopped the firmware: nothing more is sent to it.
    hung: bool,
}

/// How the bring-up's hang test went, short of the restart.
#[derive(Debug)]
pub enum HangTest {
    /// The firmware hung from about `at`, or may have: `confirmed` is false when
    /// the reply to `TEST HANG` was lost.
    Hung { at: Instant, confirmed: bool },
    /// It did not hang; `keyed` if a carrier may have gone out (it was stopped).
    Failed { error: anyhow::Error, keyed: bool },
}

impl Handheld {
    /// Take over the firmware behind `link`: check its `HELLO`, stop anything it is
    /// sending and confirm receive. Keys nothing.
    pub fn new(mut link: Link, set: Settings, wpm: u32) -> Result<Self> {
        let hello_at = Instant::now();
        let hello = link
            .request(&Command::Hello)
            .map_err(anyhow::Error::from)
            .and_then(|f| Hello::parse(&f).map_err(anyhow::Error::msg))
            .with_context(|| {
                format!(
                    "no hfnode CW firmware answering on {} (is it flashed with \
                     firmware/uv-k1, the radio on, and its USB-C port that one?)",
                    link.describe()
                )
            })?;
        check_hello(&hello)?;
        link.request(&Command::Stop)
            .context("stopping the handheld")?;
        let s = link
            .request(&Command::Status)
            .map_err(anyhow::Error::from)
            .and_then(|f| Status::parse(&f).map_err(anyhow::Error::msg))
            .context("reading the handheld's status")?;
        if s.tx {
            bail!("the handheld still reads transmitting after STOP");
        }
        let describe = format!(
            "handheld on {}: firmware {} (transmit limit {} s, link timeout {} ms)",
            link.describe(),
            hello.name,
            hello.tx_limit.as_secs(),
            hello.link_timeout.as_millis()
        );
        let shared = Arc::new(Shared {
            link: Mutex::new(link),
            state: Mutex::default(),
            closed: AtomicBool::new(false),
        });
        // Several keep-alives per link timeout, so that one lost line does not let
        // it expire.
        let every =
            (hello.link_timeout / 4).clamp(Duration::from_millis(10), Duration::from_millis(250));
        let keeper = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("handheld link".into())
                .spawn(move || keep_alive(&shared, every))?
        };
        Ok(Self {
            set,
            wpm,
            shared,
            hello,
            busy_since: None,
            busy_looked: None,
            keeper: Some(keeper),
            describe,
            hello_at,
            hung: false,
        })
    }

    /// When the firmware started, by its `HELLO` (its start-up before that not
    /// counted).
    pub fn started_at(&self) -> Option<Instant> {
        self.hello_at.checked_sub(self.hello.uptime)
    }

    /// Open the handheld configured in `cfg`. Keys nothing.
    pub fn open(cfg: &Config) -> Result<Self> {
        let Some(h) = &cfg.handheld else {
            bail!("no [handheld] section");
        };
        let set = Settings::from_config(cfg)?;
        let t = link::SerialTransport::open(&cfg.station.serial_port, h.baud)?;
        let rig = Self::new(
            Link::new(Box::new(t), set.reply_timeout),
            set,
            cfg.station.key_speed_wpm,
        )?;
        if rig.hello.tx_limit < Duration::from_secs(cfg.station.max_key_seconds) {
            log::warn!(
                "the firmware's transmit limit ({} s) is shorter than \
                 station.max_key_seconds ({} s): a long keying run would be cut short",
                rig.hello.tx_limit.as_secs(),
                cfg.station.max_key_seconds
            );
        }
        Ok(rig)
    }

    pub fn describe(&self) -> &str {
        &self.describe
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Read the firmware's status. Fails, once, if the last run did not end as it
    /// should have.
    pub fn status(&mut self) -> civ::Result<Status> {
        let s = self.shared.status()?;
        match lock(&self.shared.state).fault.take() {
            Some(f) => Err(RigError::Protocol(f)),
            None => Ok(s),
        }
    }

    /// The bring-up's link test: key `text`, which must last well past the
    /// firmware's link timeout, then send nothing at all until that timeout has
    /// passed, as if the node had died. The firmware must have stopped on its own by
    /// then; if it has not, it is stopped here and the test fails.
    pub fn link_test(&mut self, text: &str) -> Result<Duration> {
        let wait = self.hello.link_timeout + Duration::from_millis(300);
        if self.hello.tx_limit < wait + Duration::from_secs(1) {
            bail!(
                "the firmware's transmit limit ({} s) would end the run as soon as its link \
                 timeout: the test could not tell them apart",
                self.hello.tx_limit.as_secs()
            );
        }
        let length = self.set.dot(self.wpm) * cw::units(text);
        if length < wait + Duration::from_secs(1) {
            bail!(
                "{text:?} lasts {:.1} s at {} wpm, too short to outlast the link timeout",
                length.as_secs_f32(),
                self.wpm
            );
        }
        if lock(&self.shared.state).keyed {
            bail!("still transmitting");
        }
        let t0 = Instant::now();
        // Not marked keyed, so that nothing keeps the link alive.
        let sent = self.shared.request_cw(&Command::Cw {
            wpm: self.wpm,
            text: text.to_string(),
        });
        if let Err(e) = sent {
            let _ = self.shared.stop();
            bail!("the CW command failed: {e}");
        }
        thread::sleep(wait);
        let s = self.shared.status();
        lock(&self.shared.state)
            .on_air
            .push_back((t0, Instant::now()));
        match s {
            Ok(s) if !s.tx => Ok(wait),
            other => {
                self.shared.stop().context("stopping it")?;
                match other {
                    Ok(_) => bail!(
                        "the firmware was still sending {:.1} s after the node's last \
                         command: its link timeout did not stop it",
                        wait.as_secs_f32()
                    ),
                    Err(e) => bail!("reading its status: {e}"),
                }
            }
        }
    }

    /// Text for the hang test at the node's speed: long enough to outlast the
    /// watchdog by 2 s, within one keying run.
    pub fn hang_test_text(&self) -> Result<String> {
        let need = WATCHDOG_RESET + Duration::from_secs(2);
        let mut text = String::from("TEST ");
        while self.set.dot(self.wpm) * cw::units(&text) < need {
            if text.len() == MAX_CW_CHARS {
                bail!(
                    "no text of {MAX_CW_CHARS} characters lasts {} s at {} wpm",
                    need.as_secs(),
                    self.wpm
                );
            }
            text.push('0');
        }
        Ok(text)
    }

    /// The bring-up's hang test: key `text`, which must last well past
    /// [`WATCHDOG_RESET`], then have the firmware stop its main loop, as a hang or a
    /// crash would. Only its watchdog can end the transmission then: its own limits
    /// and `STOP` all run in that loop. Returns once the hang has begun, or may have.
    /// The reset drops the radio's USB port, so the handheld must then be opened
    /// again, and found restarted ([`Handheld::started_at`]) and on receive; nothing
    /// more is sent to this one, not even `STOP` when it is dropped. An error: nothing
    /// was keyed.
    pub fn hang_test(&mut self, text: &str) -> Result<HangTest> {
        let length = self.set.dot(self.wpm) * cw::units(text);
        if length < WATCHDOG_RESET + Duration::from_secs(2) {
            bail!(
                "{text:?} lasts {:.1} s at {} wpm, too short to outlast the watchdog",
                length.as_secs_f32(),
                self.wpm
            );
        }
        if lock(&self.shared.state).keyed {
            bail!("still transmitting");
        }
        let t0 = Instant::now();
        // Not marked keyed, so that nothing keeps the link alive: the firmware stops
        // reading at once anyway.
        let sent = self.shared.request_cw(&Command::Cw {
            wpm: self.wpm,
            text: text.to_string(),
        });
        if let Err(e) = sent {
            // A lost reply: it may have keyed.
            let keyed = matches!(e, RigError::Timeout);
            let _ = self.shared.stop();
            return Ok(HangTest::Failed {
                error: anyhow::anyhow!("the CW command failed: {e}"),
                keyed,
            });
        }
        let sent_hang = Instant::now();
        let hang = self.shared.request(&Command::TestHang);
        lock(&self.shared.state)
            .on_air
            .push_back((t0, Instant::now() + WATCHDOG_RESET));
        match hang {
            Ok(_) => {
                self.hung = true;
                self.shared.closed.store(true, Ordering::SeqCst);
                Ok(HangTest::Hung {
                    at: Instant::now(),
                    confirmed: true,
                })
            }
            // Lost: it may have hung all the same.
            Err(RigError::Timeout) => {
                self.hung = true;
                self.shared.closed.store(true, Ordering::SeqCst);
                Ok(HangTest::Hung {
                    at: sent_hang,
                    confirmed: false,
                })
            }
            Err(e) => {
                let stopped = self.shared.stop();
                Ok(HangTest::Failed {
                    error: match stopped {
                        Ok(()) => anyhow::anyhow!("the firmware did not take the hang test: {e}"),
                        Err(s) => anyhow::anyhow!(
                            "the firmware did not take the hang test ({e}), and stopping it \
                             failed: {s}"
                        ),
                    },
                    keyed: true,
                })
            }
        }
    }

    fn freq(&mut self) -> civ::Result<(u64, u64)> {
        proto::parse_freq(&self.shared.request(&Command::Freq)?).map_err(RigError::Protocol)
    }

    fn modes(&mut self) -> civ::Result<(String, String)> {
        proto::parse_mode(&self.shared.request(&Command::Mode)?).map_err(RigError::Protocol)
    }

    /// The duty cycle's wait before a run of `on_air` may be keyed.
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
        if self.set.busy_quiet.is_zero() {
            return Ok(Duration::ZERO);
        }
        let s = self.status()?;
        if s.tx {
            return Err(RigError::Protocol(
                "the handheld is transmitting without the node keying it".into(),
            ));
        }
        let now = Instant::now();
        // A wait the station gave up on (a storm, an error) is not this one.
        let looked = self.busy_looked.replace(now);
        if looked.is_none_or(|t| now - t > self.set.busy_quiet + Duration::from_secs(2)) {
            self.busy_since = None;
        }
        if s.quiet >= self.set.busy_quiet {
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
        Ok(self.set.busy_quiet - s.quiet)
    }
}

impl Rig for Handheld {
    fn frequency(&mut self) -> civ::Result<u64> {
        Ok(self.freq()?.0)
    }
    fn transmit_frequency(&mut self) -> civ::Result<u64> {
        Ok(self.freq()?.1)
    }
    /// A repeater offset, or anything else that moves transmit off receive.
    fn split_or_delta_tx(&mut self) -> civ::Result<bool> {
        let (rx, tx) = self.freq()?;
        Ok(rx != tx)
    }
    /// The firmware sets nothing: the radio must already receive and transmit on
    /// `hz`, as set at the radio.
    fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
        match self.freq()? {
            (rx, tx) if rx == hz && tx == hz => Ok(()),
            (rx, tx) => Err(RigError::Protocol(format!(
                "the handheld reads {rx} Hz receive, {tx} Hz transmit: set it to {hz} Hz \
                 simplex (no offset) at the radio"
            ))),
        }
    }
    /// The radio must be in CW, transmit and receive, as set at the radio.
    fn set_mode_cw(&mut self) -> civ::Result<()> {
        match self.modes()? {
            (tx, rx) if tx == "CW" && rx == "CW" => Ok(()),
            (tx, rx) => Err(RigError::Protocol(format!(
                "the handheld's mode reads {tx} transmit, {rx} receive: set CW at the radio \
                 (with dual watch off, so that both are this VFO)"
            ))),
        }
    }
    /// The level in `[handheld] power`, as set at the radio: a handheld has a few
    /// fixed levels.
    fn set_rf_power_watts(&mut self, _: u32) -> civ::Result<()> {
        let level = proto::parse_power(&self.shared.request(&Command::Power)?)
            .map_err(RigError::Protocol)?;
        if self.set.power.matches(&level) {
            return Ok(());
        }
        Err(RigError::Protocol(format!(
            "the handheld's power reads {level}, but handheld.power is {}: set it at the \
             radio",
            self.set.power
        )))
    }
    /// Sent with each `CW` command.
    fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
        if !(5..=50).contains(&wpm) {
            return Err(RigError::Protocol(format!(
                "{wpm} wpm: the firmware takes 5-50"
            )));
        }
        self.wpm = wpm;
        Ok(())
    }
    /// Break-in must be on, as set at the radio: without it the firmware's keyer
    /// only sounds the sidetone. It returns to receive at the end of each `CW`
    /// command's text.
    fn set_break_in(&mut self, on: bool) -> civ::Result<()> {
        if !on {
            return Ok(());
        }
        let on = proto::parse_breakin(&self.shared.request(&Command::Breakin)?)
            .map_err(RigError::Protocol)?;
        if on {
            return Ok(());
        }
        Err(RigError::Protocol(
            "break-in is off on the handheld: turn it on in its CW menu".into(),
        ))
    }
    fn set_break_in_delay(&mut self, _: f32) -> civ::Result<()> {
        Ok(())
    }
    fn dot_duration(&mut self) -> civ::Result<Duration> {
        Ok(self.set.dot(self.wpm))
    }
    /// No tuner: refused, so that nothing keys a carrier through here.
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

    /// Have the firmware key `text`, and return once it has started: the firmware
    /// answers once it has keyed. If the reply is lost the run may or may not have
    /// started, so the node sends `STOP`. Spaces at either end are left out: a
    /// word gap before the first element would outlast the firmware's wait for it
    /// at slow speeds.
    fn send_cw(&mut self, text: &str) -> civ::Result<()> {
        let text = text.trim_matches(' ').to_ascii_uppercase();
        if text.is_empty() || text.chars().count() > MAX_CW_CHARS {
            return Err(RigError::Protocol(format!(
                "{} characters: 1 to {MAX_CW_CHARS} per keying run",
                text.chars().count()
            )));
        }
        if let Some(c) = text.chars().find(|&c| !cw::is_sendable(c)) {
            return Err(RigError::Protocol(format!("{c:?} cannot be sent in Morse")));
        }
        let length = self.set.dot(self.wpm) * cw::units(&text);
        // Each character may start with the radio switching back to transmit.
        let starts = text.chars().filter(|&c| c != ' ').count() as u32;
        let on_air = length.mul_f32(self.set.time_scale) + TX_START_ALLOWANCE * starts;
        // The firmware would cut it short; slow speeds can need more than a minute.
        if on_air > self.hello.tx_limit {
            return Err(RigError::Protocol(format!(
                "{text:?} lasts {:.0} s at {} wpm, longer than the firmware's transmit limit \
                 ({} s): use a faster key_speed_wpm",
                on_air.as_secs_f32(),
                self.wpm,
                self.hello.tx_limit.as_secs()
            )));
        }
        let start_up = (TX_START_ALLOWANCE * starts).div_f32(self.set.time_scale.max(0.001));
        let cmd = Command::Cw {
            wpm: self.wpm,
            text,
        };
        let wait_until = Instant::now() + STOP_WATCH_WAIT;
        let sent = loop {
            let now = Instant::now();
            let mut st = lock(&self.shared.state);
            if st.keyed {
                return Err(RigError::Protocol("still transmitting".into()));
            }
            if let Some(f) = st.fault.take() {
                return Err(RigError::Protocol(f));
            }
            st.run += 1;
            st.keyed = true;
            st.abandoned = false;
            st.since = Some(now);
            st.end = Some(now + length);
            st.deadline =
                Some(now + (length + start_up + self.set.run_slack).min(self.set.max_run));
            // Under the state lock, so that the keep-alive thread takes this run up
            // only once it has been sent.
            let sent = lock(&self.shared.link).request_reply(&cmd);
            match sent {
                // Still watching its last stop: nothing keyed. Try again shortly,
                // with the run's times from then.
                Ok(proto::Reply::Err(code)) if code == "WAIT" && now < wait_until => {
                    st.keyed = false;
                    st.since = None;
                    st.end = None;
                    st.deadline = None;
                    drop(st);
                    log::debug!("handheld: still checking its last stop; trying again");
                    thread::sleep(Duration::from_millis(100));
                }
                Ok(proto::Reply::Ok(_)) => break Ok(()),
                Ok(proto::Reply::Err(code)) => break Err(link::refused(&cmd, &code)),
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = sent {
            log::error!("handheld: CW command failed ({e}); stopping in case it keyed");
            if let Err(s) = self.shared.stop() {
                log::error!("handheld: STOP failed too: {s}");
            }
            return Err(e);
        }
        Ok(())
    }

    fn stop_cw(&mut self) -> civ::Result<()> {
        self.shared.stop()
    }

    /// The firmware's own transmit state; an error, once, if the last run did not
    /// end as it should have.
    fn is_transmitting(&mut self) -> civ::Result<bool> {
        Ok(self.status()?.tx)
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

    /// The duty cycle first; once it allows the run, a clear frequency, so that
    /// time resting for the duty cycle does not count as waiting for the frequency.
    fn rest_needed(&mut self, keying: Duration) -> civ::Result<Duration> {
        let duty = self.duty_rest(keying)?;
        if !duty.is_zero() {
            log::info!(
                "duty cycle: a keying run of {:.1} s needs {:.1} s on receive first",
                keying.as_secs_f32(),
                duty.as_secs_f32()
            );
            return Ok(duty);
        }
        let busy = self.busy_rest()?;
        if !busy.is_zero() {
            log::info!("the frequency is in use: waiting for it to clear");
        }
        Ok(busy)
    }
}

impl Drop for Handheld {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        if !self.hung {
            if let Err(e) = self.shared.stop() {
                log::error!("handheld: STOP on closing failed: {e}");
            }
        }
        if let Some(t) = self.keeper.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests;
