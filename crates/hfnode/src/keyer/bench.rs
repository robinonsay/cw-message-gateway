//! `hfnode keyer ...`: checking the keyer box and the radio's audio, and the
//! bring-up tests (docs/keyer.md, "Bring-up"). The keying ones go through the
//! [`Station`], with every check `run` makes but the storm stand-down.

use super::link::SerialTransport;
use super::monitor::{self, Band, Judge, KeyState, Monitor};
use super::proto::Command;
use super::rig::{self, KeyerRig};
use crate::audio::Capture;
use crate::config::Config;
use crate::session::Transmission;
use crate::station::Station;
use anyhow::{bail, Context, Result};
use civ::Rig;
use keyer_core::keyer::{Boot, Trip};
use std::fmt::Write as _;
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
    }
}

pub fn rig_settings(cfg: &Config, scale: f32) -> rig::Settings {
    rig::Settings {
        frequency_hz: cfg.station.frequency_hz,
        min_level_dbfs: min_level(cfg),
        scale,
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

/// Open the box on `station.serial_port`, with `monitor` getting the radio's audio.
pub fn open_rig(cfg: &Config, monitor: Arc<Mutex<Monitor>>) -> Result<KeyerRig> {
    let t = SerialTransport::open(&cfg.station.serial_port)?;
    KeyerRig::open(Box::new(t), monitor, rig_settings(cfg, 1.0))
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
    let _ = writeln!(
        out,
        "box: {} protocol {}, run limit {} s, key-down limit {} ms, link timeout {} ms",
        h.name,
        h.version,
        h.run_limit.as_secs(),
        h.key_down_limit.as_millis(),
        h.link_timeout.as_millis()
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
    match rig.status() {
        Ok(st) => {
            let _ = writeln!(
                out,
                "box: key {}, {}, last run ended {}{}",
                if st.key { "DOWN" } else { "up" },
                if st.run { "keying a run" } else { "idle" },
                st.ended.as_str(),
                if st.trip == Trip::None {
                    String::new()
                } else {
                    ok = false;
                    ", TRIPPED: unplug the box and plug it in again".into()
                }
            );
            ok &= !st.busy();
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
                 turn the radio's volume up (or the sound card's input level)"
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
        if let Some(db) = self.band_db {
            let _ = writeln!(out, "band {db:.0} dBFS on receive");
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

/// What a box test saw.
#[derive(Debug)]
pub struct TestReport {
    /// The longest the radio's sidetone sounded without a break.
    pub longest: Duration,
    /// Its limit for the test to pass.
    pub limit: Duration,
    pub passed: bool,
    pub notes: Vec<String>,
}

/// Text keyed during a box test: dashes, so that the box is mid-element soon.
const TEST_TEXT: &str = "TTTT TTTT";

/// `hfnode keyer hangtest`: the box's control loop hangs mid-run with its key
/// down; its watchdog must reset it and open the key within its 0.5 s. Then the
/// node identifies, through the station.
pub fn hangtest(st: &mut Station<KeyerRig>, id: &str, scale: f32) -> Result<TestReport> {
    st.open_window().map_err(anyhow::Error::msg)?;
    let rig = st.rig();
    let mut r = lock(&rig);
    let m = r.monitor();
    let t0 = Instant::now();
    r.send_cw(TEST_TEXT)?;
    thread::sleep(Duration::from_millis(100).div_f32(scale));
    r.test(&Command::TestHang)?;
    let mut notes = Vec::new();
    // The node finds out: the run ended early, the box came back from its reset.
    let end = Instant::now() + Duration::from_secs(10).div_f32(scale);
    loop {
        match r.is_transmitting() {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => notes.push(format!("node: {e}")),
        }
        if Instant::now() >= end {
            bail!("the radio still reads transmitting 10 s after the hang");
        }
        thread::sleep(Duration::from_millis(50).div_f32(scale));
    }
    let longest = lock(&m).longest_tone(t0);
    let watchdog = Duration::from_millis(keyer_core::limits::WATCHDOG_MS.into());
    let limit = watchdog + Duration::from_millis(250);
    // The box comes back as a new USB device after its reset.
    let back = Instant::now() + Duration::from_secs(5).div_f32(scale);
    let hello = loop {
        match r.hello_again() {
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
    drop(r);
    let passed = restarted && longest <= limit;
    if passed {
        key(st, id)?;
        notes.push(format!("identified: {id}"));
    } else {
        notes.push(format!(
            "not identified: key `{id}` with `hfnode keyer key` once the box is checked"
        ));
    }
    Ok(TestReport {
        longest,
        limit,
        passed,
        notes,
    })
}

/// `hfnode keyer stucktest`: the node identifies first (the box cannot key again
/// after this test until it is plugged in again), then the box holds an element
/// down; its key-down limit must open the key within its 1 s, and trip.
pub fn stucktest(st: &mut Station<KeyerRig>, id: &str, scale: f32) -> Result<TestReport> {
    key(st, id)?;
    let rig = st.rig();
    let mut r = lock(&rig);
    let m = r.monitor();
    let t0 = Instant::now();
    r.send_cw(TEST_TEXT)?;
    r.test(&Command::TestStuck)?;
    let mut notes = vec![format!("identified first: {id}")];
    let end = Instant::now() + Duration::from_secs(10).div_f32(scale);
    // Not `is_transmitting`, which reads a tripped box as an error, for the
    // station to inhibit on: here the trip is what the test is for.
    loop {
        match r.status() {
            Ok(st) if !st.busy() => match lock(&m).key_state() {
                KeyState::Open => break,
                KeyState::Held(why) => {
                    notes.push(format!("node: {why}"));
                    break;
                }
                KeyState::Unsure => {}
            },
            Ok(_) => {}
            Err(e) => notes.push(format!("node: {e}")),
        }
        if Instant::now() >= end {
            bail!("the radio still reads transmitting 10 s after the stuck-key test");
        }
        thread::sleep(Duration::from_millis(50).div_f32(scale));
    }
    let longest = lock(&m).longest_tone(t0);
    let limit =
        Duration::from_millis(keyer_core::limits::KEY_DOWN_MS.into()) + Duration::from_millis(250);
    let tripped = r.status().context("STATUS after the test")?.trip == Trip::Down;
    if tripped {
        notes.push("the box tripped: unplug it and plug it in again before keying".into());
    } else {
        notes.push("the box did not trip".into());
    }
    Ok(TestReport {
        longest,
        limit,
        passed: tripped && longest <= limit,
        notes,
    })
}

#[cfg(test)]
mod tests;
