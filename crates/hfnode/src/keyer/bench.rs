//! `hfnode keyer ...`: checking the keyer box and the radio's audio, and the
//! bring-up tests (docs/keyer.md, "Bring-up"). The keying ones go through the
//! [`Station`], with every check `run` makes but the storm stand-down.

use super::link::SerialTransport;
use super::monitor::{self, Band, Judge, KeyState, Monitor};
use super::proto::Command;
use super::rig::{self, KeyerRig};
use super::Output;
use crate::audio::Capture;
use crate::config::Config;
use crate::session::Transmission;
use crate::station::{force_receive_or_latch, InhibitLatch, Station};
use anyhow::{bail, Context, Result};
use civ::Rig;
use keyer_core::keyer::{Boot, Trip};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sidetone pitch the node listens for.
pub fn pitch(cfg: &Config) -> f32 {
    cfg.keyer
        .as_ref()
        .and_then(|k| k.sidetone_hz)
        .unwrap_or(cfg.audio.pitch_hz)
}

fn min_level(cfg: &Config) -> f32 {
    cfg.keyer.as_ref().map_or(-65.0, |k| k.min_level_dbfs)
}

/// The monitor's settings from the configuration; `scale` is 1 but in the
/// self-tests.
pub fn monitor_settings(cfg: &Config, scale: f32) -> monitor::Settings {
    monitor::Settings {
        sample_rate: cfg.audio.sample_rate,
        pitch_hz: pitch(cfg),
        min_level_dbfs: min_level(cfg),
        scale,
        mode: match output(cfg) {
            Output::Key => monitor::Mode::Sidetone,
            Output::Ptt => monitor::Mode::Mute,
        },
    }
}

/// `[keyer] output`.
pub fn output(cfg: &Config) -> Output {
    cfg.keyer.as_ref().map_or(Output::Key, |k| k.output)
}

pub fn rig_settings(cfg: &Config, scale: f32) -> rig::Settings {
    rig::Settings {
        frequency_hz: cfg.station.frequency_hz,
        min_level_dbfs: min_level(cfg),
        scale,
        duty: cfg
            .keyer
            .as_ref()
            .map_or(0.5, |k| k.max_duty_percent as f32 / 100.0),
        duty_window: Duration::from_secs(cfg.keyer.as_ref().map_or(600, |k| k.duty_window_secs)),
        output: output(cfg),
    }
}

/// Start capturing the radio's audio, every block also going to a new monitor.
pub fn start_listening(cfg: &Config) -> Result<(Capture, Arc<Mutex<Monitor>>)> {
    let monitor = Arc::new(Mutex::new(Monitor::new(monitor_settings(cfg, 1.0))));
    let m = monitor.clone();
    let cap = Capture::start_with_tap(
        &cfg.audio.device,
        cfg.audio.sample_rate,
        Some(Box::new(move |at, samples| lock(&m).push(at, samples))),
    )?;
    Ok((cap, monitor))
}

/// The sidetone level `hfnode keyer sidetone` measured, kept in the state
/// directory: the level a tone at the pitch must come near to be the sidetone, so
/// that a quiet sidetone held on is not taken for band noise (the safety audit's
/// KB-2(ii)).
pub const SIDETONE_FILE: &str = "keyer-sidetone";

/// Keep `db` as the sidetone's level for later runs of the node.
pub fn save_sidetone(state_dir: &Path, db: f32) -> Result<()> {
    let f = state_dir.join(SIDETONE_FILE);
    std::fs::create_dir_all(state_dir)
        .and_then(|()| std::fs::write(&f, format!("{db}\n")))
        .with_context(|| format!("writing {}", f.display()))
}

/// The sidetone level a `sidetone` check measured before, if one did.
pub fn load_sidetone(state_dir: &Path) -> Option<f32> {
    std::fs::read_to_string(state_dir.join(SIDETONE_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Open the box on `station.serial_port`, with `monitor` getting the radio's audio.
pub fn open_rig(cfg: &Config, monitor: Arc<Mutex<Monitor>>) -> Result<KeyerRig> {
    if let Some(db) = load_sidetone(&cfg.state_dir) {
        lock(&monitor).set_known_sidetone(db);
    }
    let t = SerialTransport::open(&cfg.station.serial_port)?;
    let rig = KeyerRig::open(Box::new(t), monitor, rig_settings(cfg, 1.0))?;
    // The firmware the owner checked, if hfnode.toml names one.
    super::check_build(
        rig.hello(),
        cfg.keyer.as_ref().and_then(|k| k.firmware_build.as_deref()),
    )?;
    Ok(rig)
}

/// Wait up to `timeout` for the monitor to have heard enough band to know its
/// level, or a carrier at the pitch.
pub fn wait_for_band(monitor: &Mutex<Monitor>, timeout: Duration) -> Band {
    let end = Instant::now() + timeout;
    loop {
        let b = lock(monitor).band(Instant::now());
        if b.level_db.is_some() || b.carrier_db.is_some() || Instant::now() >= end {
            return b;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `hfnode keyer check`: the box and the band, keying nothing. Returns the report
/// and whether everything passed.
pub fn check(rig: &mut KeyerRig, min_level_dbfs: f32) -> (String, bool) {
    let mut out = String::new();
    let mut ok = true;
    let h = rig.hello().clone();
    let ptt = rig.output() == Output::Ptt;
    let _ = writeln!(
        out,
        "box: {} protocol {}, run limit {} s, PTT limit {} s, key-down limit {} ms, link \
         timeout {} ms",
        h.name,
        h.version,
        h.run_limit.as_secs(),
        h.ptt_limit.as_secs(),
        h.key_down_limit.as_millis(),
        h.link_timeout.as_millis()
    );
    let _ = writeln!(
        out,
        "box: rest {} ms between runs, duty budget {} s, build {}",
        h.rest.as_millis(),
        h.duty_budget.as_secs(),
        h.build
    );
    let _ = writeln!(
        out,
        "box: up {:.0} s, last started by {}{}",
        h.uptime.as_secs_f32(),
        h.boot.as_str(),
        if h.boot == Boot::Watchdog {
            " (its control loop stalled: if this was not a hang test, report it)"
        } else {
            ""
        }
    );
    // A box that will key nothing until it has been looked at: said here, before any
    // of the audio checks, because nothing else will get past it.
    if let Some(why) = rig.refusal() {
        ok = false;
        let _ = writeln!(out, "box: {why}");
    }
    match rig.status() {
        Ok(st) => {
            let _ = writeln!(
                out,
                "box: key {}, PTT {}, {}, last run ended {}{}",
                if st.key { "DOWN" } else { "up" },
                if st.ptt { "DOWN" } else { "up" },
                if st.run { "keying a run" } else { "idle" },
                st.ended.as_str(),
                if st.trip == Trip::None {
                    String::new()
                } else {
                    ok = false;
                    format!(
                        ", TRIPPED ({}): unplug the box and plug it in again",
                        super::trip_text(st.trip)
                    )
                }
            );
            let _ = writeln!(
                out,
                "box: {} ms of rest to go, {:.0} s of duty budget",
                st.rest_left.as_millis(),
                st.budget.as_secs_f32()
            );
            ok &= !st.busy();
            if ptt {
                if st.line {
                    let _ = writeln!(out, "box: PTT line high (the PTT contact open): ok");
                } else {
                    ok = false;
                    let _ = writeln!(
                        out,
                        "box: PTT line LOW: the radio is off, or something holds its PTT (the \
                         cable, a shorted optocoupler); nothing is keyed until it reads high"
                    );
                }
            }
        }
        Err(e) => {
            ok = false;
            let _ = writeln!(out, "box: STATUS failed: {e}");
        }
    }
    let monitor = rig.monitor();
    let band = wait_for_band(&monitor, Duration::from_secs(5));
    if !band.audio {
        ok = false;
        let _ = writeln!(
            out,
            "audio: none arriving: check [audio] device and the cable"
        );
    }
    match band.level_db {
        Some(db) if db >= min_level_dbfs => {
            let _ = writeln!(
                out,
                "audio: band at {db:.0} dBFS (keyer.min_level_dbfs {min_level_dbfs:.0}): ok"
            );
        }
        Some(db) => {
            ok = false;
            let _ = writeln!(
                out,
                "audio: band at {db:.0} dBFS, under keyer.min_level_dbfs {min_level_dbfs:.0}: \
                 turn the radio's volume up (or the sound card's input level){}",
                if ptt {
                    "; the handheld's squelch must be open (SQL 0)"
                } else {
                    ""
                }
            );
        }
        None => {
            ok = false;
            let _ = writeln!(out, "audio: no level yet");
        }
    }
    if let Some(db) = band.carrier_db {
        ok = false;
        let _ = writeln!(out, "audio: {}", carrier_note(db));
    }
    if let KeyState::Held(why) = lock(&monitor).key_state() {
        ok = false;
        let _ = writeln!(out, "audio: {why}");
    }
    if ptt && lock(&monitor).quieted() {
        let _ = writeln!(
            out,
            "audio: the receive noise has dropped: a station is on the channel (the node \
             waits for it to clear before keying)"
        );
    }
    (out, ok)
}

/// What a steady tone at the pitch may be.
pub fn carrier_note(db: f32) -> String {
    format!(
        "a steady tone at the sidetone pitch ({db:.0} dBFS): the radio's key may be closed at \
         the radio (a shorted optocoupler or key cable), or a station's carrier is on the \
         frequency; the node does not key over it"
    )
}

/// `hfnode keyer rx`: the box's key open, then the radio's, as far as its audio
/// shows within `wait`. A key this cannot confirm open (the box's, or the radio's:
/// no audio to show it, or a steady tone at the pitch, which is how a key closed at
/// the radio sounds) latches the transmit inhibit in `state_dir`, so that the node
/// keys nothing more until someone has looked at the radio (the safety audit's
/// KB-2).
pub fn rx(rig: &mut KeyerRig, state_dir: &Path, wait: Duration) -> Result<()> {
    let inhibit = InhibitLatch::in_dir(state_dir);
    force_receive_or_latch(rig, &inhibit).context("the radio's key is not confirmed open")?;
    let band = wait_for_band(&rig.monitor(), wait);
    let why = if !band.audio {
        Some("no audio from the radio".to_string())
    } else {
        band.carrier_db.map(carrier_note)
    };
    if let Some(why) = why {
        let why = format!("the radio's key is not confirmed open: {why}");
        inhibit.latch(&why);
        bail!("{why}");
    }
    Ok(())
}

/// The last run's judgement, once the audio covers it.
fn last_judge(st: &Station<KeyerRig>) -> Option<Judge> {
    let rig = st.rig();
    let r = lock(&rig);
    let id = r.last_run()?;
    let m = r.monitor();
    drop(r);
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(j) = lock(&m).judge(id) {
            return Some(j);
        }
        if Instant::now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Key `text` through the station: the band and the radio's key checked first,
/// the run heard after, the radio confirmed back on receive.
pub fn key(st: &mut Station<KeyerRig>, text: &str) -> Result<Option<Judge>> {
    st.open_window().map_err(anyhow::Error::msg)?;
    st.transmit(&Transmission {
        segments: vec![text.to_string()],
        read_ids: Vec::new(),
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(last_judge(st))
}

/// The sidetone must be at least this far over the band noise for `sidetone` to
/// pass: more than the 10 dB within which the node takes a tone for the sidetone.
pub const MIN_SIDETONE_MARGIN_DB: f32 = 15.0;

/// What `hfnode keyer sidetone` measured.
#[derive(Debug)]
pub struct SidetoneReport {
    pub judge: Judge,
    pub pitch_hz: Option<f32>,
    pub band_db: Option<f32>,
}

impl SidetoneReport {
    /// The report, with what to change; and whether it all passed.
    pub fn explain(&self, configured_hz: f32) -> (String, bool) {
        let j = &self.judge;
        let mut out = String::new();
        let mut ok = j.heard;
        let _ = writeln!(out, "keying {j}");
        let _ = writeln!(
            out,
            "audio delay {} ms; sidetone {:.0} dBFS, {:.0} dB over the gaps",
            j.lag.as_millis(),
            j.tone_db,
            j.contrast_db()
        );
        if let Some(why) = j.why_not() {
            let _ = writeln!(out, "not heard: {why}");
        }
        // A sidetone only a little over the band noise is not enough to tell a key
        // held at the radio from the band (the safety audit's KB-2(ii)): the node
        // only takes a tone for the sidetone within 10 dB of the level measured
        // here, so the margin has to be larger than that.
        match self.band_db {
            Some(db) if j.tone_db - db < MIN_SIDETONE_MARGIN_DB => {
                ok = false;
                let _ = writeln!(
                    out,
                    "band {db:.0} dBFS on receive: the sidetone is only {:.0} dB over it, under \
                     the {MIN_SIDETONE_MARGIN_DB:.0} dB needed to tell a key held at the radio \
                     from the band; turn the radio's sidetone level up (its monitor level), or \
                     the volume down",
                    j.tone_db - db
                );
            }
            Some(db) => {
                let _ = writeln!(
                    out,
                    "band {db:.0} dBFS on receive: the sidetone is {:.0} dB over it: ok",
                    j.tone_db - db
                );
            }
            None => {
                ok = false;
                let _ = writeln!(
                    out,
                    "no band level measured: the sidetone's margin over the band is not known"
                );
            }
        }
        if j.tone_db > -6.0 {
            ok = false;
            let _ = writeln!(
                out,
                "the sidetone is close to clipping: turn the sidetone level (or the volume) down"
            );
        }
        match self.pitch_hz {
            Some(p) if (p - configured_hz).abs() > 30.0 => {
                ok = false;
                let _ = writeln!(
                    out,
                    "sidetone pitch {p:.0} Hz, not {configured_hz:.0}: set keyer.sidetone_hz = \
                     {p:.0} (or the radio's sidetone pitch to {configured_hz:.0})"
                );
            }
            Some(p) => {
                let _ = writeln!(out, "sidetone pitch {p:.0} Hz: ok");
            }
            None => {
                let _ = writeln!(out, "sidetone pitch: not measured");
            }
        }
        (out, ok)
    }
}

/// `hfnode keyer sidetone`: key `id` (the node's `DE <call>`) and measure the
/// sidetone.
pub fn sidetone(st: &mut Station<KeyerRig>, id: &str) -> Result<SidetoneReport> {
    let m = lock(&st.rig()).monitor();
    let band_db = lock(&m).band(Instant::now()).level_db;
    lock(&m).record(true);
    let keyed = key(st, id);
    let rid = lock(&st.rig()).last_run();
    let judge = last_judge(st);
    let pitch_hz = rid.and_then(|id| lock(&m).pitch(id));
    lock(&m).record(false);
    let judge = match (keyed, judge) {
        (_, Some(j)) => j,
        (Err(e), None) => return Err(e),
        (Ok(_), None) => bail!("no audio covering the keying"),
    };
    Ok(SidetoneReport {
        judge,
        pitch_hz,
        band_db,
    })
}

/// With a handheld, room for its switch to transmit and back to receive in a
/// box test's limit: the bench measures it (docs/keyer.md).
const PTT_SWITCH_MARGIN: Duration = Duration::from_millis(600);

/// What a box test allows on top of the box's own limit: for a handheld, its
/// lead (the PTT is held through it) and its switching.
fn test_allowance(r: &KeyerRig) -> Duration {
    match r.output() {
        Output::Key => Duration::ZERO,
        Output::Ptt => Duration::from_millis(keyer_core::mcw::LEAD_MS.into()) + PTT_SWITCH_MARGIN,
    }
}

/// What a box test saw.
#[derive(Debug)]
pub struct TestReport {
    /// The longest the radio was on the air without a break, as its audio shows:
    /// its sidetone, or a handheld's receive noise gone quiet.
    pub longest: Duration,
    /// Its limit for the test to pass.
    pub limit: Duration,
    pub passed: bool,
    pub notes: Vec<String>,
}

/// Text keyed during a box test: dashes, so that the box is mid-element soon.
const TEST_TEXT: &str = "TTTT TTTT";

/// A box test must have measured at least this much unbroken sidetone. Less than
/// that and the audio never showed the key down, so nothing was measured and the
/// test proves nothing (the safety audit's KB-3: `longest: 0ns ... passed: true`).
pub const MIN_TEST_TONE: Duration = Duration::from_millis(250);

/// What a test may take from its first key-down to the radio's key reading open:
/// enough for the box's own limit and the audio behind it, not enough for a test
/// that measured nothing to pass on patience.
const TEST_SPAN: Duration = Duration::from_secs(6);

/// Printed before every test that holds the radio's key down, and again if one
/// times out. See docs/keyer.md, "Stopping it by hand".
pub const MANUAL_STOP: &str = "This test holds the radio's key (or a handheld's PTT) down on \
     purpose. If the radio keeps transmitting: pull the key plug (a handheld's K-plug) out of the \
     radio, then switch the radio off. Do that first and read the output afterwards.";

/// What to tell the operator if a test leaves the radio transmitting.
pub const STOP_NOW: &str = "THE RADIO MAY STILL BE TRANSMITTING: pull the key plug (a \
     handheld's K-plug) out of the radio now, then switch the radio off.";

/// The node's `DE <call>`, keyed and heard, before a test: the sidetone's level and
/// the audio delay come from it, and without them [`Monitor::longest_tone`] has
/// only `min_level_dbfs` to go on and a test can pass having measured nothing.
fn identify_first(st: &mut Station<KeyerRig>, id: &str) -> Result<(Judge, Vec<String>)> {
    let judge = key(st, id)?.context("no audio covering the identification")?;
    if !judge.heard {
        bail!(
            "the identification was not heard ({}): the test cannot measure the sidetone, so it \
             would prove nothing; run `hfnode keyer sidetone` first",
            judge.why_not().unwrap_or_else(|| "see above".into())
        );
    }
    Ok((judge, vec![format!("identified first: {id}")]))
}

/// Wait out the box's rest and the duty window (and, with a handheld, a busy
/// channel) before keying again.
fn rest(st: &Station<KeyerRig>, keying: Duration) -> Result<()> {
    let rig = st.rig();
    let mut r = lock(&rig);
    r.wait_rest(keying)?;
    Ok(())
}

/// `hfnode keyer hangtest`: the box's control loop hangs mid-run with its key
/// down; its watchdog must reset it and open the key within its 0.5 s, and it must
/// come back tripped (`WATCHDOG`), keying nothing until it is plugged in again.
///
/// The node identifies first, so that the sidetone is measured before the key is
/// held down, and the rig lock is free between each look at the box: the operator's
/// Ctrl-C must get through while the key is down (the safety audit's KB-3).
pub fn hangtest(st: &mut Station<KeyerRig>, id: &str, scale: f32) -> Result<TestReport> {
    let (_, mut notes) = identify_first(st, id)?;
    let rig = st.rig();
    let watchdog = Duration::from_millis(keyer_core::limits::WATCHDOG_MS.into());
    let limit = watchdog + Duration::from_millis(250) + test_allowance(&lock(&rig));
    let m = lock(&rig).monitor();
    rest(st, Duration::from_secs(2))?;
    let t0 = Instant::now();
    lock(&rig).send_cw(TEST_TEXT)?;
    thread::sleep(Duration::from_millis(100).div_f32(scale));
    // A box that hangs before its reply gets out has still taken the command.
    if let Err(e) = lock(&rig).test(&Command::TestHang) {
        notes.push(format!(
            "no reply to TEST HANG ({e}): it may still have taken it"
        ));
    }
    // The node finds out: the run ended early, the box came back from its reset.
    // Not `is_transmitting`, which reads the tripped box it comes back as an error,
    // for the station to inhibit on: here the restart is what the test is for.
    let end = t0 + TEST_SPAN.div_f32(scale);
    let open = loop {
        let status = lock(&rig).status();
        match status {
            Ok(st) if !st.busy() => match lock(&m).key_state() {
                KeyState::Open => break Instant::now(),
                KeyState::Held(why) => {
                    notes.push(format!("node: {why}"));
                    break Instant::now();
                }
                KeyState::Unsure => {}
            },
            Ok(_) => {}
            Err(e) => notes.push(format!("node: {e}")),
        }
        if Instant::now() >= end {
            notes.push(STOP_NOW.into());
            bail!(
                "the radio still reads transmitting {:.0} s after the hang. {STOP_NOW}",
                TEST_SPAN.as_secs_f32()
            );
        }
        thread::sleep(Duration::from_millis(50).div_f32(scale));
    };
    let longest = lock(&m).longest_on_air(t0);
    let span = open.saturating_duration_since(t0).mul_f32(scale);
    // The box comes back as a new USB device after its reset.
    let back = Instant::now() + Duration::from_secs(5).div_f32(scale);
    let hello = loop {
        match lock(&rig).hello_again() {
            Ok(h) => break Some(h),
            Err(_) if Instant::now() < back => {
                thread::sleep(Duration::from_millis(100).div_f32(scale))
            }
            Err(e) => {
                notes.push(format!("box not back after its reset: {e}"));
                break None;
            }
        }
    };
    let restarted = hello.as_ref().is_some_and(|h| h.boot == Boot::Watchdog);
    if !restarted {
        notes.push("the box did not report a watchdog restart".into());
    }
    let trip = if hello.is_some() {
        lock(&rig).status().ok().map(|st| st.trip)
    } else {
        None
    };
    let tripped = trip == Some(Trip::Watchdog);
    if tripped {
        notes.push(
            "the box came back from its watchdog reset tripped (WATCHDOG), as it must: unplug it \
             and plug it in again before keying"
                .into(),
        );
    } else {
        notes.push(format!(
            "the box came back from its reset {}, not tripped WATCHDOG: after its watchdog fires \
             it must key nothing until it is plugged in again",
            trip.map_or("unread", |t| t.as_str())
        ));
    }
    let measured = check_measured(longest, span, &mut notes);
    Ok(TestReport {
        longest,
        limit,
        passed: restarted && tripped && measured && longest <= limit,
        notes,
    })
}

/// Whether the test measured the key down at all, and within its span.
fn check_measured(longest: Duration, span: Duration, notes: &mut Vec<String>) -> bool {
    if longest < MIN_TEST_TONE {
        notes.push(format!(
            "nothing measured: the longest the audio showed the radio on the air (its sidetone, or \
             a handheld's receive noise gone) was {} ms, under the {} ms a key held down must \
             give; check the audio (`hfnode keyer sidetone`) and run the test again",
            longest.as_millis(),
            MIN_TEST_TONE.as_millis()
        ));
        return false;
    }
    if span > TEST_SPAN {
        notes.push(format!(
            "the key read open only {:.1} s after the test started, past the {:.0} s a test may \
             take: the sidetone measurement does not cover it",
            span.as_secs_f32(),
            TEST_SPAN.as_secs_f32()
        ));
        return false;
    }
    true
}

/// `hfnode keyer stucktest`: the node identifies first (the box cannot key again
/// after this test until it is plugged in again), then the box holds an element
/// down; its key-down limit must open the key within its 1 s, and trip.
pub fn stucktest(st: &mut Station<KeyerRig>, id: &str, scale: f32) -> Result<TestReport> {
    let (_, mut notes) = identify_first(st, id)?;
    let rig = st.rig();
    let limit = Duration::from_millis(keyer_core::limits::KEY_DOWN_MS.into())
        + Duration::from_millis(250)
        + test_allowance(&lock(&rig));
    let m = lock(&rig).monitor();
    rest(st, Duration::from_secs(2))?;
    let t0 = Instant::now();
    lock(&rig).send_cw(TEST_TEXT)?;
    if let Err(e) = lock(&rig).test(&Command::TestStuck) {
        notes.push(format!(
            "no reply to TEST STUCK ({e}): it may still have taken it"
        ));
    }
    let end = t0 + TEST_SPAN.div_f32(scale);
    // Not `is_transmitting`, which reads a tripped box as an error, for the
    // station to inhibit on: here the trip is what the test is for.
    let open = loop {
        let status = lock(&rig).status();
        match status {
            Ok(st) if !st.busy() => match lock(&m).key_state() {
                KeyState::Open => break Instant::now(),
                KeyState::Held(why) => {
                    notes.push(format!("node: {why}"));
                    break Instant::now();
                }
                KeyState::Unsure => {}
            },
            Ok(_) => {}
            Err(e) => notes.push(format!("node: {e}")),
        }
        if Instant::now() >= end {
            bail!(
                "the radio still reads transmitting {:.0} s after the stuck-key test. {STOP_NOW}",
                TEST_SPAN.as_secs_f32()
            );
        }
        thread::sleep(Duration::from_millis(50).div_f32(scale));
    };
    let longest = lock(&m).longest_on_air(t0);
    let span = open.saturating_duration_since(t0).mul_f32(scale);
    let tripped = lock(&rig).status().context("STATUS after the test")?.trip == Trip::Down;
    if tripped {
        notes.push("the box tripped: unplug it and plug it in again before keying".into());
    } else {
        notes.push("the box did not trip".into());
    }
    let measured = check_measured(longest, span, &mut notes);
    Ok(TestReport {
        longest,
        limit,
        passed: tripped && measured && longest <= limit,
        notes,
    })
}

#[cfg(test)]
mod tests;
