//! A handheld as the node's radio (`station.rig = "handheld"`), for trying the whole
//! system out locally on 2 m before the IC-7300: a Quansheng UV-K1 or UV-K5 running
//! a CW firmware that keys a carrier on commands over its serial link, the way the
//! IC-7300's keyer takes CI-V commands. The command set is in [`proto`] and
//! docs/handheld-protocol.md; [`mock`] simulates a radio running it.
//!
//! The station's safety layer ([`crate::station`]) drives it like the IC-7300: the
//! software watchdog, keyer pieces of at most 30 characters with receive confirmed
//! after each, the transmit inhibit, the storm stand-down, and the radio set up and
//! checked (frequency, and transmit frequency equal to it) before every
//! transmission. What a handheld does not have, and what covers it here:
//!
//! - **No SWR or power meter.** The firmware's own transmit state
//!   ([`proto::Status`]) is read back instead, and a run read back as ended well
//!   before its text could have gone out fails the transmission. Each keying run is
//!   bounded four ways besides the station's watchdog: the node stops a run that goes
//!   on past the end of its text (plus `run_slack`, and never past
//!   `max_key_seconds` plus [`DEADMAN_MARGIN`]), and fails the transmission; while a
//!   run lasts the node sends `STATUS` often enough to keep the firmware's link
//!   timeout from expiring, and no longer, so that the link timeout ends the run if
//!   the node dies or the cable is pulled; the firmware has a transmit limit of its
//!   own of at most a minute; and the radio's own transmit time-out timer is the
//!   last backstop (docs/handheld.md).
//! - **No tuner**: a window start sets the radio up and checks it, and transmits
//!   nothing.
//! - **A small transmitter**: at most `max_duty_percent` of any `duty_window_secs`
//!   on the air; a long reply waits on receive between keying runs.
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
/// How long the node waits for each reply from the firmware.
pub const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

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
    /// Nothing has passed: `hfnode handheld check`, `setup` and `rx`, `listen` and
    /// `record` only; none of them keys the radio.
    #[default]
    None,
    /// `hfnode listen` decoded the other handheld's Morse correctly. Allows
    /// `hfnode handheld key` and `linktest`, with the operator at the radio.
    Listen,
    /// Short transmissions were heard correctly on the other handheld and read back
    /// as ended; the link test showed the firmware stopping on its own.
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
        Action::Key => (Stage::Listen, "this keying command"),
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
    if !(10..=100).contains(&h.max_duty_percent) {
        bail!("handheld.max_duty_percent must be 10-100");
    }
    if !(60..=3600).contains(&h.duty_window_secs) {
        bail!("handheld.duty_window_secs must be 60-3600");
    }
    // The longest keying run the station allows must fit in the budget, or it
    // could never be keyed.
    let budget = h.duty_window_secs * u64::from(h.max_duty_percent) / 100;
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

    /// `STOP`, which the firmware answers once its transmitter is off.
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
}

impl Handheld {
    /// Take over the firmware behind `link`: check its `HELLO`, stop anything it is
    /// sending and confirm receive. Keys nothing.
    pub fn new(mut link: Link, set: Settings, wpm: u32) -> Result<Self> {
        let hello = link
            .request(&Command::Hello)
            .map_err(anyhow::Error::from)
            .and_then(|f| Hello::parse(&f).map_err(anyhow::Error::msg))
            .with_context(|| {
                format!(
                    "no CW firmware answering on {} (is it flashed, the radio on, and the \
                     cable on that port at handheld.baud?)",
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
        })
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
        let sent = self.shared.request(&Command::Cw {
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

    fn freq(&mut self) -> civ::Result<(u64, u64)> {
        proto::parse_freq(&self.shared.request(&Command::Freq)?).map_err(RigError::Protocol)
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
    /// Receive and transmit on `hz`, read back.
    fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
        let f = self.shared.request(&Command::SetFreq(hz))?;
        match proto::parse_freq(&f).map_err(RigError::Protocol)? {
            (rx, tx) if rx == hz && tx == hz => Ok(()),
            (rx, tx) => Err(RigError::Protocol(format!(
                "set {hz} Hz, but the handheld reads {rx} Hz receive, {tx} Hz transmit"
            ))),
        }
    }
    fn set_mode_cw(&mut self) -> civ::Result<()> {
        self.shared.request(&Command::ModeCw).map(drop)
    }
    /// The level in `[handheld] power`: a handheld has a few fixed levels.
    fn set_rf_power_watts(&mut self, _: u32) -> civ::Result<()> {
        self.shared
            .request(&Command::Power(self.set.power))
            .map(drop)
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
    /// The firmware returns to receive at the end of each `CW` command's text.
    fn set_break_in(&mut self, _: bool) -> civ::Result<()> {
        Ok(())
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

    /// Have the firmware key `text`, and return once it has started. If the reply
    /// is lost the run may or may not have started, so the node sends `STOP`.
    fn send_cw(&mut self, text: &str) -> civ::Result<()> {
        let text = text.to_ascii_uppercase();
        if text.trim().is_empty() || text.chars().count() > MAX_CW_CHARS {
            return Err(RigError::Protocol(format!(
                "{} characters: 1 to {MAX_CW_CHARS} per keying run",
                text.chars().count()
            )));
        }
        if let Some(c) = text.chars().find(|&c| !cw::is_sendable(c)) {
            return Err(RigError::Protocol(format!("{c:?} cannot be sent in Morse")));
        }
        let now = Instant::now();
        let length = self.set.dot(self.wpm) * cw::units(&text);
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
        st.deadline = Some(now + (length + self.set.run_slack).min(self.set.max_run));
        // Under the state lock, so that the keep-alive thread takes this run up only
        // once it has been sent.
        let sent = lock(&self.shared.link).request(&Command::Cw {
            wpm: self.wpm,
            text,
        });
        drop(st);
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
                "duty cycle: {:.1} s on receive before the next keying run",
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
        if let Err(e) = self.shared.stop() {
            log::error!("handheld: STOP on closing failed: {e}");
        }
        if let Some(t) = self.keeper.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests;
