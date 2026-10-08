//! [`KeyerRig`]: [`Rig`] for any radio keyed by the keyer box and heard through its
//! headphone jack; or, with `[keyer] output = "ptt"`, an FM handheld whose PTT the
//! box holds while it keys an MCW tone into its microphone, heard through its
//! speaker jack.
//!
//! Nothing is set on the radio, so the settings the station applies are kept
//! (the frequency, only to log and check it) or ignored (power, break-in). What the
//! station asks of the radio's state is answered from two sources that do not
//! depend on each other: the box (`STATUS`: its key or PTT, its run, the PTT line)
//! and the [`Monitor`] listening to the radio's sidetone, or to a handheld's
//! receive noise. The radio counts as transmitting if either says so, or if the
//! audio cannot yet show its key (or PTT) open after a run.
//!
//! Once the audio has shown the radio's key held with the box's open, or the box
//! has come back from its watchdog firing (outside `hfnode keyer hangtest`), the
//! rig never again reads the radio as on receive: every status read is an error,
//! and the station latches its transmit inhibit, which only the owner clears.
//!
//! Before each run the rig waits ([`Rig::rest_needed`]) for the box's rest after
//! its last run and its duty budget, and for `[keyer] max_duty_percent` of the
//! last `duty_window_secs`. With a handheld, all of the PTT time counts (an FM
//! transmitter's carrier is on for all of it), and it also waits for a clear
//! channel.

use super::link::{refused, Link, Transport};
use super::monitor::{KeyState, Monitor, MAX_LAG, RX_BACK, STUCK_AFTER_RUN};
use super::proto::{Command, Hello, Reply, Status};
use super::{check_hello, Output, REPLY_TIMEOUT};
use anyhow::{anyhow, bail};
use civ::{Result, Rig, RigError};
use keyer_core::keyer::{Boot, Ended, Trip};
use keyer_core::morse::{self, Segments};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How often the node sends `STATUS` while a run lasts, and only then: well
/// inside the box's link timeout, so that a run stops soon after the node dies.
pub const KEEP_ALIVE: Duration = Duration::from_millis(250);
/// A run still under way this long past its Morse length is stopped, and the
/// transmission fails.
pub const RUN_SLACK: Duration = Duration::from_secs(1);
/// Past the box's link timeout, this much longer before its key counts as open
/// without its word for it.
const LINK_MARGIN: Duration = Duration::from_millis(500);
/// How long, past the run and the longest audio delay, to wait for the audio that
/// shows whether a run was heard. In real time, as is the margin below: how late
/// the capture's blocks arrive is up to the computer, whatever the scale.
const JUDGE_WAIT: Duration = Duration::from_millis(1500);
/// After the box opens its key, the audio shows the radio's key open (or held)
/// once it covers the longest audio delay and the stuck margin after it; this
/// leaves room for the capture's blocks to arrive.
const SETTLE_MARGIN: Duration = Duration::from_millis(500);
/// With the receiver quieted (a station on the channel), how long to wait before
/// asking again (radio time) ...
const QUIET_WAIT: Duration = Duration::from_secs(2);
/// ... and how long to wait in all before giving the transmission up.
const QUIET_GIVE_UP: Duration = Duration::from_secs(60);

/// An `MCW` run's PTT time: its Morse, with the box's lead and tail.
pub fn ptt_time(morse: Duration) -> Duration {
    morse
        + Duration::from_millis(u64::from(
            keyer_core::mcw::LEAD_MS + keyer_core::mcw::TAIL_MS,
        ))
}

/// What a box's trip means.
pub fn trip_note(trip: Trip) -> &'static str {
    match trip {
        Trip::None => "not tripped",
        Trip::Down => "its key (or tone) stayed on past its key-down limit",
        Trip::Pin => {
            "its own watch on its pins: the key or tone pin on past the key-down limit, or \
             the PTT pin down past the PTT limit"
        }
        Trip::Slow => "its control loop ran slow with the key or tone on",
        Trip::Ptt => "its PTT stayed down past its PTT limit",
        Trip::Line => {
            "the PTT line stayed low after it let the PTT up: something else holds the \
             radio's PTT, and the radio may still be transmitting"
        }
    }
}

/// What the rig needs from the configuration.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Only logged and checked: the radio is tuned by hand.
    pub frequency_hz: u64,
    /// `[keyer] min_level_dbfs`.
    pub min_level_dbfs: f32,
    /// Radio time per wall-clock time: 1, except in the self-tests.
    pub scale: f32,
    /// `[keyer] max_duty_percent`, as a share.
    pub duty: f32,
    /// `[keyer] duty_window_secs`, radio time.
    pub duty_window: Duration,
    /// `[keyer] output`: the key line (`CW`), or a handheld's PTT (`MCW`).
    pub output: Output,
}

/// A run the box took.
#[derive(Debug, Clone, Copy)]
struct Active {
    /// The monitor's id for it.
    id: u64,
    /// When its Morse ends, and when the node stops it if it has not.
    ends: Instant,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct Shared {
    /// The run under way, kept alive by `STATUS` until the box reads it over.
    run: Option<Active>,
    /// The last run's monitor id.
    last: Option<u64>,
    /// Something that fails the transmission, reported once.
    failure: Option<String>,
    /// The box's key-downs (with a handheld, its PTT time) as keyed, for the duty
    /// window (wall clock).
    on_air: VecDeque<(Instant, Instant)>,
}

impl Shared {
    /// The box's key opened at `at`, ending a run early: its key-downs from then on
    /// were never keyed.
    fn cut(&mut self, at: Instant) {
        for (s, e) in self.on_air.iter_mut() {
            if *e > at {
                *e = at.max(*s);
            }
        }
        self.on_air.retain(|(s, e)| e > s);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn status(link: &mut Link) -> Result<Status> {
    let f = link.request(&Command::Status)?;
    Status::parse(&f).map_err(RigError::Protocol)
}

/// The box reads `st`, no longer keying, at `now`: end the run it had, if any.
fn ended(shared: &Mutex<Shared>, monitor: &Mutex<Monitor>, st: &Status, now: Instant) {
    let mut sh = lock(shared);
    let Some(a) = sh.run.take() else {
        return;
    };
    if now < a.ends {
        lock(monitor).key_opened(a.id, now);
        sh.cut(now);
    }
    let why = match st.ended {
        Ended::Done | Ended::Stop => return,
        Ended::Link => "no line from the node for its link timeout",
        Ended::Usb => "the USB link dropped",
        Ended::Limit => "its run limit",
        Ended::Down => {
            "it tripped (its key-down limit, or its watch on its pins): unplug it and plug it \
             in again"
        }
        Ended::Ptt => "its PTT limit; it has tripped: unplug it and plug it in again",
        Ended::Line => {
            "the PTT line did not read low after it closed the PTT: the radio was not keyed \
             (the cable out, the radio off, or the line not wired)"
        }
        Ended::None => "no run at all (did it restart?)",
    };
    let msg = format!("the keyer box ended the run early: {why}");
    log::error!("{msg}");
    sh.failure.get_or_insert(msg);
}

/// See the module documentation.
pub struct KeyerRig {
    link: Arc<Mutex<Link>>,
    monitor: Arc<Mutex<Monitor>>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    keep_alive: Option<JoinHandle<()>>,
    s: Settings,
    hello: Hello,
    frequency_hz: u64,
    wpm: u32,
    /// Whether the last answer about the key was that it is held at the radio.
    held: bool,
    /// Why the radio is never again taken to be on receive: its key was seen held
    /// at the radio, or the box's watchdog fired. See the module documentation.
    fault: Option<String>,
    /// Why nothing is keyed until the box is plugged in again: it last started other
    /// than by being plugged in (as after flashing it).
    replug: Option<String>,
    /// The box last read its PTT line low with its PTT up.
    line_low: bool,
    /// Waiting for the channel to clear since then.
    quiet_since: Option<Instant>,
}

impl KeyerRig {
    /// Greet the box on `t` and check it: its protocol and limits, its key open, not
    /// tripped. `monitor` must be getting the radio's audio.
    pub fn open(
        t: Box<dyn Transport>,
        monitor: Arc<Mutex<Monitor>>,
        s: Settings,
    ) -> anyhow::Result<Self> {
        let mut link = Link::new(t, REPLY_TIMEOUT);
        let place = link.describe();
        let f = link
            .request(&Command::Hello)
            .map_err(|e| anyhow!("no HELLO from the keyer box on {place}: {e}"))?;
        let hello = Hello::parse(&f).map_err(|e| anyhow!("keyer box on {place}: {e}"))?;
        check_hello(&hello)?;
        let (mut fault, mut replug) = (None, None);
        match hello.boot {
            Boot::Watchdog => {
                let msg = format!(
                    "the keyer box last restarted because its watchdog fired, {:.0} s ago: its \
                     control loop stalled. Unless that was `hfnode keyer hangtest`, report it; \
                     either way, unplug the box and plug it in again",
                    hello.uptime.as_secs_f32()
                );
                log::error!("{msg}");
                fault = Some(msg);
            }
            Boot::Other => {
                let msg = format!(
                    "the keyer box last started other than by being plugged in ({:.0} s ago; \
                     right after flashing it, that is expected): unplug it and plug it in \
                     again before keying",
                    hello.uptime.as_secs_f32()
                );
                log::warn!("{msg}");
                replug = Some(msg);
            }
            Boot::Power => log::info!(
                "keyer box {} build {} on {place}: plugged in {:.0} s ago",
                hello.name,
                hello.build,
                hello.uptime.as_secs_f32()
            ),
        }
        link.request(&Command::Stop)
            .map_err(|e| anyhow!("keyer box: STOP: {e}"))?;
        let st = status(&mut link).map_err(|e| anyhow!("keyer box: STATUS: {e}"))?;
        if st.trip != Trip::None {
            bail!(
                "the keyer box has tripped ({}): unplug it and plug it in again",
                trip_note(st.trip)
            );
        }
        if st.busy() {
            bail!("the keyer box still reads its key or PTT down after STOP");
        }
        if s.output == Output::Ptt && !st.line {
            log::warn!(
                "keyer box: the PTT line reads low: the radio is off, the cable is out, or \
                 the PTT is held; nothing is keyed until it reads high"
            );
        }
        let link = Arc::new(Mutex::new(link));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let keep_alive = {
            let (link, shared, monitor, stop) =
                (link.clone(), shared.clone(), monitor.clone(), stop.clone());
            let every = KEEP_ALIVE.div_f32(s.scale);
            let link_timeout = hello.link_timeout.div_f32(s.scale);
            thread::spawn(move || keep_alive(&link, &shared, &monitor, &stop, every, link_timeout))
        };
        Ok(Self {
            link,
            monitor,
            shared,
            stop,
            keep_alive: Some(keep_alive),
            s,
            hello,
            frequency_hz: s.frequency_hz,
            wpm: 20,
            held: false,
            fault,
            replug,
            line_low: false,
            quiet_since: None,
        })
    }

    /// Why the rig refuses to key, if it does: see [`KeyerRig::fault`] and
    /// `replug`.
    pub fn refusal(&self) -> Option<&str> {
        self.fault.as_deref().or(self.replug.as_deref())
    }

    /// Wait, as the station does before each run, until the box and the duty
    /// window allow keying for `keying`.
    pub fn wait_rest(&mut self, keying: Duration) -> Result<()> {
        loop {
            let rest = self.rest_needed(keying)?;
            if rest.is_zero() {
                return Ok(());
            }
            thread::sleep(rest);
        }
    }

    /// The duty window's wait before a run that keys for `keying` (wall clock).
    fn duty_rest(&self, keying: Duration) -> Result<Duration> {
        let window = self.s.duty_window.div_f32(self.s.scale);
        let allows = window.mul_f32(self.s.duty);
        if keying > allows {
            return Err(RigError::Protocol(format!(
                "a keying run of {:.0} s is more than keyer.max_duty_percent allows",
                keying.mul_f32(self.s.scale).as_secs_f32()
            )));
        }
        let now = Instant::now();
        let mut sh = lock(&self.shared);
        sh.on_air
            .retain(|&(_, e)| now.saturating_duration_since(e) < window);
        // Key-down time in the window ending `t` from now, nothing keyed in
        // between; it only falls as `t` grows. The run itself counts whole.
        let used = |t: Duration| -> Duration {
            let to = now + t;
            let from = to.checked_sub(window);
            sh.on_air
                .iter()
                .map(|&(s, e)| {
                    let s = from.map_or(s, |f| s.max(f));
                    e.min(to).saturating_duration_since(s)
                })
                .sum()
        };
        if used(Duration::ZERO) + keying <= allows {
            return Ok(Duration::ZERO);
        }
        let (mut lo, mut hi) = (Duration::ZERO, window);
        while hi - lo > Duration::from_millis(10).div_f32(self.s.scale) {
            let mid = lo + (hi - lo) / 2;
            if used(mid) + keying <= allows {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(hi)
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn output(&self) -> Output {
        self.s.output
    }

    /// Wall-clock time the box keeps the radio on transmit for runs whose Morse
    /// lasts `keying` in all: with a handheld, `runs` runs' PTT lead and tail too.
    fn on_air_for(&self, keying: Duration, runs: u32) -> Duration {
        match self.s.output {
            Output::Key => keying,
            Output::Ptt => keying + (ptt_time(Duration::ZERO) * runs).div_f32(self.s.scale),
        }
    }

    pub fn monitor(&self) -> Arc<Mutex<Monitor>> {
        self.monitor.clone()
    }

    /// The monitor's id for the last run keyed.
    pub fn last_run(&self) -> Option<u64> {
        lock(&self.shared).last
    }

    /// `HELLO` again, as after the box restarted.
    pub fn hello_again(&mut self) -> Result<Hello> {
        let f = lock(&self.link).request(&Command::Hello)?;
        let h = Hello::parse(&f).map_err(RigError::Protocol)?;
        check_hello(&h).map_err(|e| RigError::Protocol(e.to_string()))?;
        self.hello = h.clone();
        Ok(h)
    }

    /// The box's `STATUS`.
    pub fn status(&mut self) -> Result<Status> {
        let st = status(&mut lock(&self.link))?;
        if !st.busy() {
            ended(&self.shared, &self.monitor, &st, Instant::now());
        }
        Ok(st)
    }

    /// Bring-up only: `TEST HANG` or `TEST STUCK` (or, in tests, `TEST HOLD`),
    /// during a run, each after the `TEST ARM` the box needs just before it.
    pub fn test(&mut self, cmd: &Command) -> Result<()> {
        if !matches!(
            cmd,
            Command::TestHang | Command::TestStuck | Command::TestHold
        ) {
            return Err(RigError::Protocol(format!("{} is not a test", cmd.body())));
        }
        let mut l = lock(&self.link);
        l.request(&Command::TestArm)?;
        l.request(cmd).map(drop)
    }

    /// `hfnode keyer linktest`: key a run longer than the box's link timeout, then
    /// say nothing. The box must open its key by itself and report the run ended by
    /// the link going quiet (`LINK`). Returns how long the node waited.
    ///
    /// This is the one check that the node dying, its USB cable coming out or its
    /// host going to sleep leaves the radio on receive, and nothing else tests it
    /// (the safety audit's KB-11).
    pub fn link_test(&mut self, text: &str) -> Result<Duration> {
        let wait = self.link_timeout() + LINK_MARGIN.div_f32(self.s.scale);
        let segs = Segments::of(text.as_bytes())
            .map_err(|e| RigError::Protocol(format!("the keyer box cannot key {text:?}: {e:?}")))?;
        let dot_ms = morse::dot_ms(self.wpm).map_err(|_| no("such speed"))?;
        let keying = Duration::from_millis(u64::from(segs.units()) * u64::from(dot_ms));
        if keying.div_f32(self.s.scale) < wait + Duration::from_secs(1).div_f32(self.s.scale) {
            return Err(RigError::Protocol(format!(
                "{text:?} keys for only {:.1} s at {} wpm: too short to outlast the box's link \
                 timeout, so the test could not tell a stop from the text running out",
                keying.as_secs_f32(),
                self.wpm
            )));
        }
        self.wait_rest(keying)?;
        self.send_cw(text)?;
        // Nothing more goes to the box: with no keep-alive the run is now the box's
        // own to end. Its `STATUS` below is the first line it hears after that.
        let run = lock(&self.shared).run.take();
        thread::sleep(wait);
        let st = self.status()?;
        if let Some(a) = run {
            let opened = lock(&self.link).sent().unwrap_or(Instant::now()) + self.link_timeout();
            lock(&self.shared).cut(opened);
            lock(&self.monitor).key_opened(a.id, opened);
        }
        if st.busy() {
            let _ = self.stop_cw();
            return Err(RigError::Protocol(format!(
                "the box was still keying {:.1} s after the node's last line: its link timeout \
                 did not open the key",
                wait.as_secs_f32()
            )));
        }
        if st.ended != Ended::Link {
            return Err(RigError::Protocol(format!(
                "the box opened its key but says the run ended {}, not by the link going quiet: \
                 the run was too short, or something else stopped it",
                st.ended.as_str()
            )));
        }
        Ok(wait)
    }

    /// The box's link timeout, in wall-clock time.
    fn link_timeout(&self) -> Duration {
        self.hello.link_timeout.div_f32(self.s.scale)
    }

    /// The radio's key as the audio shows it, logging a held key once. A held
    /// key is never forgotten: see [`KeyerRig::fault`].
    fn key_state(&mut self) -> KeyState {
        let k = lock(&self.monitor).key_state();
        let held = matches!(k, KeyState::Held(_));
        if let KeyState::Held(why) = &k {
            if !self.held {
                log::error!("keyer: {why}");
            }
            self.fault.get_or_insert_with(|| why.clone());
        }
        self.held = held;
        k
    }
}

/// While a run lasts, `STATUS` every `every`: the box's link timeout ends a run
/// if these stop. A run still going past its deadline is stopped.
fn keep_alive(
    link: &Mutex<Link>,
    shared: &Mutex<Shared>,
    monitor: &Mutex<Monitor>,
    stop: &AtomicBool,
    every: Duration,
    link_timeout: Duration,
) {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(every);
        let Some(a) = lock(shared).run else {
            continue;
        };
        let mut l = lock(link);
        // The run may have been stopped while this thread waited for the link.
        if lock(shared).run.is_none_or(|r| r.id != a.id) {
            continue;
        }
        let now = Instant::now();
        match status(&mut l) {
            Ok(st) if !st.busy() => ended(shared, monitor, &st, now),
            // Past its deadline, still keying or not answering: STOP, once, and no
            // more keep-alives for it, so that the box's link timeout ends the run
            // if the STOP is lost.
            r if now > a.deadline => {
                let stop = l.request(&Command::Stop);
                let at = Instant::now();
                let opened = match stop {
                    Ok(_) => at,
                    Err(_) => at + link_timeout,
                };
                let mut sh = lock(shared);
                sh.run = None;
                sh.cut(at);
                lock(monitor).key_opened(a.id, opened);
                let state = match r {
                    Ok(_) => "still keying".to_string(),
                    Err(e) => format!("not answering ({e})"),
                };
                let msg = format!(
                    "the keyer box was {state} {:.1} s past the end of its run: stopped{}",
                    (now - a.ends).as_secs_f32(),
                    stop.err()
                        .map(|e| format!(" (STOP: {e})"))
                        .unwrap_or_default()
                );
                log::error!("{msg}");
                sh.failure.get_or_insert(msg);
            }
            Ok(_) => {}
            Err(e) => log::warn!("keyer box: keep-alive STATUS: {e}"),
        }
    }
}

impl Drop for KeyerRig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = lock(&self.link).request(&Command::Stop);
        if let Some(h) = self.keep_alive.take() {
            let _ = h.join();
        }
    }
}

fn no(what: &str) -> RigError {
    RigError::Protocol(format!("the keyer rig has no {what}"))
}

impl Rig for KeyerRig {
    fn frequency(&mut self) -> Result<u64> {
        Ok(self.frequency_hz)
    }

    /// The radio is tuned by hand: the frequency is only kept, to log.
    fn set_frequency(&mut self, hz: u64) -> Result<()> {
        self.frequency_hz = hz;
        Ok(())
    }

    fn set_mode_cw(&mut self) -> Result<()> {
        Ok(())
    }

    /// Set at the radio.
    fn set_rf_power_watts(&mut self, _watts: u32) -> Result<()> {
        Ok(())
    }

    fn set_key_speed(&mut self, wpm: u32) -> Result<()> {
        if !(keyer_core::MIN_WPM..=keyer_core::MAX_WPM).contains(&wpm) {
            return Err(RigError::Protocol(format!(
                "the keyer box keys {}-{} wpm, not {wpm}",
                keyer_core::MIN_WPM,
                keyer_core::MAX_WPM
            )));
        }
        self.wpm = wpm;
        Ok(())
    }

    /// Set at the radio.
    fn set_break_in(&mut self, _on: bool) -> Result<()> {
        Ok(())
    }

    fn set_break_in_delay(&mut self, _dots: f32) -> Result<()> {
        Ok(())
    }

    fn dot_duration(&mut self) -> Result<Duration> {
        let ms = morse::dot_ms(self.wpm).map_err(|_| no("such speed"))?;
        Ok(Duration::from_millis(ms.into()).div_f32(self.s.scale))
    }

    fn start_tune(&mut self) -> Result<()> {
        Err(no("tuner"))
    }

    fn tuner_busy(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn read_swr(&mut self) -> Result<f32> {
        Err(no("SWR meter"))
    }

    fn read_po(&mut self) -> Result<f32> {
        Err(no("power meter"))
    }

    fn send_cw(&mut self, text: &str) -> Result<()> {
        if let Some(why) = self.refusal() {
            return Err(RigError::Protocol(why.to_string()));
        }
        let text = text.trim().to_ascii_uppercase();
        let segs = Segments::of(text.as_bytes())
            .map_err(|e| RigError::Protocol(format!("the keyer box cannot key {text:?}: {e:?}")))?;
        let ptt = self.s.output == Output::Ptt;
        let now = Instant::now();
        let band = lock(&self.monitor).band(now);
        if !band.audio {
            return Err(RigError::Protocol(if ptt {
                "no audio from the radio: without its receive noise the node does not key".into()
            } else {
                "no audio from the radio: without its sidetone the node does not key".into()
            }));
        }
        // First: a carrier is also why the band's level may not be known.
        if let Some(db) = band.carrier_db {
            return Err(RigError::Protocol(format!(
                "a steady tone at the sidetone pitch ({db:.0} dBFS): a station's carrier on \
                 the frequency, or the radio's key closed at the radio; not keying over it"
            )));
        }
        match band.level_db {
            None => {
                return Err(RigError::Protocol(
                    "the band has not been heard long enough yet to key".into(),
                ))
            }
            Some(db) if db < self.s.min_level_dbfs => {
                return Err(RigError::Protocol(format!(
                    "the radio's audio is at {db:.0} dBFS, under keyer.min_level_dbfs {:.0}: \
                     is the radio on, {}and its volume up?",
                    self.s.min_level_dbfs,
                    if ptt {
                        "its squelch open (SQL 0), "
                    } else {
                        ""
                    }
                )))
            }
            Some(_) => {}
        }
        if ptt && lock(&self.monitor).quieted() {
            return Err(RigError::Protocol(
                "the radio's receive noise has dropped: a station is on the channel; not \
                 keying over it"
                    .into(),
            ));
        }
        match self.key_state() {
            KeyState::Open => {}
            KeyState::Unsure => {
                return Err(RigError::Protocol(
                    "the audio does not yet show the radio's key open".into(),
                ))
            }
            KeyState::Held(why) => return Err(RigError::Protocol(why)),
        }
        if lock(&self.shared).run.is_some() {
            return Err(RigError::Protocol("a keying run is still under way".into()));
        }
        let st = self.status()?;
        let wpm = self.wpm;
        let cmd = if ptt {
            Command::Mcw { wpm, text }
        } else {
            Command::Cw { wpm, text }
        };
        if st.trip != Trip::None {
            return Err(refused(&cmd, "TRIP"));
        }
        if st.busy() {
            return Err(RigError::Protocol(
                "the keyer box reads busy with no run from the node".into(),
            ));
        }
        if ptt && !st.line {
            return Err(refused(&cmd, "LINE"));
        }
        let dot_ms = morse::dot_ms(self.wpm).map_err(|_| no("such speed"))?;
        let morse = Duration::from_millis(u64::from(segs.units()) * u64::from(dot_ms));
        // With a handheld the box's PTT is down, and the radio's carrier on, for the
        // whole run, its lead and tail too.
        let run = if ptt { ptt_time(morse) } else { morse };
        // The station waits these out first (`rest_needed`): the box would refuse.
        let down = if ptt {
            run
        } else {
            let down: u32 = segs
                .as_slice()
                .iter()
                .filter(|s| s.down)
                .map(|s| u32::from(s.units))
                .sum();
            Duration::from_millis(u64::from(down) * u64::from(dot_ms))
        };
        if !st.rest_left.is_zero() || st.budget < down {
            return Err(RigError::Protocol(format!(
                "the keyer box is resting: {} ms of its rest after the last run to go, {:.1} s \
                 of duty budget for {:.1} s of key-down",
                st.rest_left.as_millis(),
                st.budget.as_secs_f32(),
                down.as_secs_f32()
            )));
        }
        let mut link = lock(&self.link);
        let t0 = Instant::now();
        let reply = link.request_reply(&cmd);
        if let Ok(Reply::Err(code)) = &reply {
            return Err(refused(&cmd, code));
        }
        // From here the box may be keying: CW is never sent twice, and a lost reply
        // may hide a run taken, so the run is watched in every case.
        let id = if ptt {
            lock(&self.monitor).ptt_started(t0, run)
        } else {
            lock(&self.monitor).run_started(
                t0,
                Duration::from_millis(dot_ms.into()),
                segs.as_slice(),
            )
        };
        let ends = t0 + run.div_f32(self.s.scale);
        let mut sh = lock(&self.shared);
        if ptt {
            sh.on_air.push_back((t0, ends));
        } else {
            let dot = Duration::from_millis(dot_ms.into()).div_f32(self.s.scale);
            let mut t = t0;
            for seg in segs.as_slice() {
                let len = dot * u32::from(seg.units);
                if seg.down {
                    sh.on_air.push_back((t, t + len));
                }
                t += len;
            }
        }
        sh.last = Some(id);
        sh.failure = None;
        sh.run = Some(Active {
            id,
            ends,
            deadline: ends + RUN_SLACK.div_f32(self.s.scale),
        });
        drop(sh);
        drop(link);
        match reply {
            Ok(_) => Ok(()),
            Err(e) => {
                log::error!(
                    "keyer box: no reply to {} ({e}): stopping it in case it took it",
                    cmd.name()
                );
                Err(e)
            }
        }
    }

    fn stop_cw(&mut self) -> Result<()> {
        let mut link = lock(&self.link);
        let r = link.request(&Command::Stop);
        let now = Instant::now();
        // Stop keeping a run alive either way: if STOP did not arrive, the box's
        // link timeout ends the run.
        let mut sh = lock(&self.shared);
        if let Some(a) = sh.run.take() {
            let opened = match r {
                Ok(_) => now,
                Err(_) => now + self.link_timeout(),
            };
            sh.cut(opened);
            lock(&self.monitor).key_opened(a.id, opened);
        }
        drop(sh);
        r.map(drop)
    }

    fn is_transmitting(&mut self) -> Result<bool> {
        if let Some(f) = &self.fault {
            return Err(RigError::Protocol(f.clone()));
        }
        let ptt = self.s.output == Output::Ptt;
        self.line_low = false;
        let unreachable = match self.status() {
            Ok(st) if st.busy() => return Ok(true),
            // Its key is open, but it will key nothing more until it is power-cycled,
            // and it tripped because a limit was reached: an error each time, so that
            // the station inhibits transmitting and tells the owner.
            Ok(st) if st.trip != Trip::None => {
                return Err(RigError::Protocol(format!(
                    "the keyer box has tripped ({}): check it and the radio, then unplug the \
                     box and plug it in again",
                    trip_note(st.trip)
                )))
            }
            // Its PTT is up, but the line reads it held (or the radio is off): until
            // it reads high, the radio counts as transmitting.
            Ok(st) if ptt && !st.line => {
                self.line_low = true;
                return Ok(true);
            }
            Ok(_) => false,
            Err(e) => {
                // The box opens its key by itself a link timeout after the last line
                // it took: wait that long, then ask again, and if it still does not
                // answer, its key is open and the audio says whether the radio's is.
                let sent = lock(&self.link).sent();
                let quiet = self.link_timeout() + LINK_MARGIN.div_f32(self.s.scale);
                if let Some(wait) =
                    sent.and_then(|t| (t + quiet).checked_duration_since(Instant::now()))
                {
                    log::warn!(
                        "keyer box: no answer ({e}); waiting {:.1} s for its link timeout",
                        wait.as_secs_f32()
                    );
                    thread::sleep(wait);
                }
                match self.status() {
                    Ok(st) if st.busy() => return Ok(true),
                    Ok(_) => false,
                    Err(e) => {
                        log::warn!("keyer box: no answer ({e}); its key is open by now");
                        true
                    }
                }
            }
        };
        // The box is not keying. If its last run ended other than as asked (seen
        // here or by the keep-alive), the transmission fails, once.
        if let Some(f) = lock(&self.shared).failure.take() {
            return Err(RigError::Protocol(f));
        }
        let now = Instant::now();
        match self.key_state() {
            // Not "still transmitting", which a key released in time would answer
            // by the station's deadline: an error, for the station to inhibit on.
            KeyState::Held(why) => Err(RigError::Protocol(why)),
            KeyState::Unsure => Ok(true),
            KeyState::Open => {
                if unreachable && !lock(&self.monitor).band(now).audio {
                    return Err(RigError::Protocol(
                        "no answer from the keyer box, and no audio from the radio".into(),
                    ));
                }
                Ok(false)
            }
        }
    }

    fn set_transmit(&mut self, tx: bool) -> Result<()> {
        if tx {
            return Err(RigError::Protocol(
                "the keyer rig never puts the radio on transmit".into(),
            ));
        }
        self.stop_cw()
    }

    fn has_tuner(&self) -> bool {
        false
    }

    fn receive_settle(&self) -> Duration {
        match self.s.output {
            Output::Key => (MAX_LAG + STUCK_AFTER_RUN).div_f32(self.s.scale) + SETTLE_MARGIN,
            // From the end of the Morse: the box's tail, the audio delay and the
            // radio's switch back to receive.
            Output::Ptt => {
                (ptt_time(Duration::ZERO) + MAX_LAG + RX_BACK).div_f32(self.s.scale) + SETTLE_MARGIN
            }
        }
    }

    fn held_key(&mut self) -> Option<String> {
        if let Some(f) = &self.fault {
            return Some(f.clone());
        }
        match self.key_state() {
            KeyState::Held(why) => Some(why),
            _ => None,
        }
    }

    /// The box's rest after its last run and its duty budget, then the duty window
    /// (`[keyer] max_duty_percent`); `keying` counts as all key-down, and with a
    /// handheld so do two runs' PTT lead and tail (`keying` holds at most two runs:
    /// an ID and a piece). Then, with a handheld, a clear channel.
    fn rest_needed(&mut self, keying: Duration) -> Result<Duration> {
        if let Some(why) = self.refusal() {
            return Err(RigError::Protocol(why.to_string()));
        }
        let scale = self.s.scale;
        let keying = self.on_air_for(keying, 2);
        let on_box = keying.mul_f32(scale);
        if on_box > self.hello.duty_budget {
            return Err(RigError::Protocol(format!(
                "a keying run of {:.0} s is more than the keyer box's duty budget",
                on_box.as_secs_f32()
            )));
        }
        let st = self.status()?;
        // The budget is earned back 1 ms for each ms the key is up.
        let on_box_wait = st.rest_left.max(on_box.saturating_sub(st.budget));
        let wait = on_box_wait.div_f32(scale).max(self.duty_rest(keying)?);
        if !wait.is_zero() {
            log::info!(
                "keyer: a keying run of {:.1} s needs {:.1} s on receive first (the box's rest \
                 and duty budget, keyer.max_duty_percent)",
                on_box.as_secs_f32(),
                wait.mul_f32(scale).as_secs_f32()
            );
            return Ok(wait);
        }
        if self.s.output != Output::Ptt || !lock(&self.monitor).quieted() {
            self.quiet_since = None;
            return Ok(Duration::ZERO);
        }
        let since = *self.quiet_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= QUIET_GIVE_UP.div_f32(scale) {
            self.quiet_since = None;
            return Err(RigError::Protocol(format!(
                "the radio's receive noise has stayed quiet for {} s: a station on the \
                 channel, or the radio off or its squelch closed; not keying",
                QUIET_GIVE_UP.as_secs()
            )));
        }
        log::info!("the channel is in use: waiting for it to clear");
        Ok(QUIET_WAIT.div_f32(scale))
    }

    fn transmit_detail(&mut self) -> Option<String> {
        if self.line_low {
            return Some(
                "the keyer box's PTT is up but its PTT line reads low: the PTT is held at \
                 the radio (a shorted optocoupler or cable), or the radio is off"
                    .into(),
            );
        }
        match self.key_state() {
            KeyState::Held(why) => Some(why),
            KeyState::Unsure => {
                Some("the audio does not yet show the radio's key open after the box's".into())
            }
            KeyState::Open => None,
        }
    }

    fn has_meters(&self) -> bool {
        false
    }

    fn keying_confirmed(&mut self) -> Result<Option<bool>> {
        let Some(id) = lock(&self.shared).last else {
            return Ok(None);
        };
        let deadline = Instant::now() + MAX_LAG.div_f32(self.s.scale) + JUDGE_WAIT;
        loop {
            if let Some(j) = lock(&self.monitor).judge(id) {
                match j.why_not() {
                    None => log::info!("keyer: {j}"),
                    Some(why) => log::error!("keyer: {j}: {why}"),
                }
                return Ok(Some(j.heard));
            }
            if Instant::now() >= deadline {
                log::error!("keyer: no audio covering the last run: not heard");
                return Ok(Some(false));
            }
            thread::sleep(Duration::from_millis(20).div_f32(self.s.scale));
        }
    }
}

#[cfg(test)]
mod tests;
