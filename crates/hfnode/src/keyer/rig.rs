//! [`KeyerRig`]: [`Rig`] for any radio keyed by the keyer box and heard through its
//! headphone jack.
//!
//! Nothing is set on the radio, so the settings the station applies are kept
//! (the frequency, only to log and check it) or ignored (power, break-in). What the
//! station asks of the radio's state is answered from two sources that do not
//! depend on each other: the box (`STATUS`: its key, its run) and the
//! [`Monitor`] listening to the radio's sidetone. The radio counts as transmitting
//! if either says so, or if the audio cannot yet show its key open after a run.

use super::link::{refused, Link, Transport};
use super::monitor::{KeyState, Monitor, MAX_LAG, STUCK_AFTER_RUN};
use super::proto::{Command, Hello, Reply, Status};
use super::{check_hello, REPLY_TIMEOUT};
use anyhow::{anyhow, bail};
use civ::{Result, Rig, RigError};
use keyer_core::keyer::{Boot, Ended, Trip};
use keyer_core::morse::{self, Segments};
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

/// What the rig needs from the configuration.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Only logged and checked: the radio is tuned by hand.
    pub frequency_hz: u64,
    /// `[keyer] min_level_dbfs`.
    pub min_level_dbfs: f32,
    /// Radio time per wall-clock time: 1, except in the self-tests.
    pub scale: f32,
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
    }
    let why = match st.ended {
        Ended::Done | Ended::Stop => return,
        Ended::Link => "no line from the node for its link timeout",
        Ended::Usb => "the USB link dropped",
        Ended::Limit => "its run limit",
        Ended::Down => "its key-down limit; it has tripped: unplug it and plug it in again",
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
        match hello.boot {
            Boot::Watchdog => log::warn!(
                "keyer box: it last restarted because its watchdog fired (its control loop \
                 stalled), {:.0} s ago",
                hello.uptime.as_secs_f32()
            ),
            b => log::info!(
                "keyer box {} on {place}: started ({}) {:.0} s ago",
                hello.name,
                b.as_str(),
                hello.uptime.as_secs_f32()
            ),
        }
        link.request(&Command::Stop)
            .map_err(|e| anyhow!("keyer box: STOP: {e}"))?;
        let st = status(&mut link).map_err(|e| anyhow!("keyer box: STATUS: {e}"))?;
        if st.trip != Trip::None {
            bail!(
                "the keyer box has tripped (its key stayed down past its limit): unplug it \
                 and plug it in again"
            );
        }
        if st.busy() {
            bail!("the keyer box still reads its key down after STOP");
        }
        let link = Arc::new(Mutex::new(link));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let keep_alive = {
            let (link, shared, monitor, stop) =
                (link.clone(), shared.clone(), monitor.clone(), stop.clone());
            let every = KEEP_ALIVE.div_f32(s.scale);
            thread::spawn(move || keep_alive(&link, &shared, &monitor, &stop, every))
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
        })
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
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

    /// Bring-up only: `TEST HANG` or `TEST STUCK`, during a run.
    pub fn test(&mut self, cmd: &Command) -> Result<()> {
        if !matches!(cmd, Command::TestHang | Command::TestStuck) {
            return Err(RigError::Protocol(format!("{} is not a test", cmd.body())));
        }
        lock(&self.link).request(cmd).map(drop)
    }

    /// The box's link timeout, in wall-clock time.
    fn link_timeout(&self) -> Duration {
        self.hello.link_timeout.div_f32(self.s.scale)
    }

    /// The radio's key as the audio shows it, logging a held key once.
    fn key_state(&mut self) -> KeyState {
        let k = lock(&self.monitor).key_state();
        let held = matches!(k, KeyState::Held(_));
        if let KeyState::Held(why) = &k {
            if !self.held {
                log::error!("keyer: {why}");
            }
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
            Ok(_) if now > a.deadline => {
                let r = l.request(&Command::Stop);
                let at = Instant::now();
                let mut sh = lock(shared);
                sh.run = None;
                lock(monitor).key_opened(a.id, at);
                let msg = format!(
                    "the keyer box was still keying {:.1} s past the end of its run: stopped{}",
                    (now - a.ends).as_secs_f32(),
                    r.err().map(|e| format!(" (STOP: {e})")).unwrap_or_default()
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
        let text = text.trim().to_ascii_uppercase();
        let segs = Segments::of(text.as_bytes())
            .map_err(|e| RigError::Protocol(format!("the keyer box cannot key {text:?}: {e:?}")))?;
        let now = Instant::now();
        let band = lock(&self.monitor).band(now);
        if !band.audio {
            return Err(RigError::Protocol(
                "no audio from the radio: without its sidetone the node does not key".into(),
            ));
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
                     is the radio on, and its volume up?",
                    self.s.min_level_dbfs
                )))
            }
            Some(_) => {}
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
        if st.trip != Trip::None {
            return Err(refused(
                &Command::Cw {
                    wpm: self.wpm,
                    text,
                },
                "TRIP",
            ));
        }
        if st.busy() {
            return Err(RigError::Protocol(
                "the keyer box reads busy with no run from the node".into(),
            ));
        }
        let dot_ms = morse::dot_ms(self.wpm).map_err(|_| no("such speed"))?;
        let run = Duration::from_millis(u64::from(segs.units()) * u64::from(dot_ms));
        let cmd = Command::Cw {
            wpm: self.wpm,
            text,
        };
        let mut link = lock(&self.link);
        let t0 = Instant::now();
        let reply = link.request_reply(&cmd);
        if let Ok(Reply::Err(code)) = &reply {
            return Err(refused(&cmd, code));
        }
        // From here the box may be keying: CW is never sent twice, and a lost reply
        // may hide a run taken, so the run is watched in every case.
        let id = lock(&self.monitor).run_started(
            t0,
            Duration::from_millis(dot_ms.into()),
            segs.as_slice(),
        );
        let ends = t0 + run.div_f32(self.s.scale);
        let mut sh = lock(&self.shared);
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
                log::error!("keyer box: no reply to CW ({e}): stopping it in case it took it");
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
        if let Some(a) = lock(&self.shared).run.take() {
            let opened = match r {
                Ok(_) => now,
                Err(_) => now + self.link_timeout(),
            };
            lock(&self.monitor).key_opened(a.id, opened);
        }
        r.map(drop)
    }

    fn is_transmitting(&mut self) -> Result<bool> {
        let unreachable = match self.status() {
            Ok(st) if st.busy() => return Ok(true),
            // Its key is open, but it will key nothing more until it is power-cycled,
            // and it tripped because a key-down went on past its limit: an error each
            // time, so that the station inhibits transmitting and tells the owner.
            Ok(st) if st.trip != Trip::None => {
                return Err(RigError::Protocol(
                    "the keyer box has tripped (a key-down went on past its limit): check \
                     it, then unplug it and plug it in again"
                        .into(),
                ))
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
            KeyState::Held(_) | KeyState::Unsure => Ok(true),
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
        (MAX_LAG + STUCK_AFTER_RUN).div_f32(self.s.scale) + SETTLE_MARGIN
    }

    fn held_key(&mut self) -> Option<String> {
        match self.key_state() {
            KeyState::Held(why) => Some(why),
            _ => None,
        }
    }

    fn transmit_detail(&mut self) -> Option<String> {
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
