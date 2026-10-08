//! The keyer rig on the mock box and radio, under a station: for tests.

use super::mock::{Clock, MockBox, MockRadio, RadioSettings};
use super::monitor::{self, Monitor};
use super::rig::{KeyerRig, Settings};
use crate::session::Transmission;
use crate::station::{Station, StationConfig, ID_INTERVAL, INHIBIT_FILE};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Radio time runs this much faster than real time. Not faster: the box's link
/// timeout (2 s) is then 0.4 s of real time, and a test machine busy with other
/// tests can hold the keep-alive's thread up for longer than 0.1 s.
pub const SCALE: f32 = 5.0;

pub fn radio_secs(s: f64) -> Duration {
    Duration::from_secs_f64(s / f64::from(SCALE))
}

/// The station's settings, with radio time running `scale` times faster than
/// real time.
pub fn cfg_at(scale: f32) -> StationConfig {
    let secs = |s: f64| Duration::from_secs_f64(s / f64::from(scale));
    StationConfig {
        frequency_hz: 7_030_000,
        power_watts: 5,
        key_speed_wpm: 20,
        max_key: secs(60.0),
        swr_limit: 2.0,
        segment_pause: secs(0.5),
        swr_delay: secs(0.05),
        swr_window: secs(1.0),
        swr_min_po: 2.0,
        break_in_delay_dots: 10.0,
        stuck_margin: secs(3.0),
        tune_timeout: secs(20.0),
        poll: secs(0.1),
        station_id: "DE N0DE".into(),
        id_interval: ID_INTERVAL.div_f32(scale),
    }
}

pub fn tx(segments: &[&str]) -> Transmission {
    Transmission {
        segments: segments.iter().map(|s| s.to_string()).collect(),
        read_ids: Vec::new(),
    }
}

pub struct Bench {
    pub station: Station<KeyerRig>,
    pub keyer_box: MockBox,
    pub radio: MockRadio,
    pub dir: tempfile::TempDir,
}

impl Bench {
    pub fn inhibit_file(&self) -> PathBuf {
        self.dir.path().join(INHIBIT_FILE)
    }

    /// Radio time now.
    pub fn now(&self) -> f64 {
        self.keyer_box.clock.secs()
    }

    pub fn cw_lines(&self) -> usize {
        self.keyer_box
            .now()
            .lines
            .iter()
            .filter(|l| l.contains(" CW "))
            .count()
    }
}

/// The mock box and radio set up as `tweak` says, a station on them, and a few
/// seconds of band heard.
pub fn bench(tweak: impl FnOnce(&mut RadioSettings)) -> Bench {
    bench_at(SCALE, tweak)
}

/// [`bench`] with radio time running `scale` times faster than real time: 1 for
/// what depends on real-time waits.
pub fn bench_at(scale: f32, tweak: impl FnOnce(&mut RadioSettings)) -> Bench {
    bench_with(scale, tweak, |_| {})
}

/// [`bench_at`], with `before_open` done to the box, the radio already listening
/// to it, before the node's rig opens it.
pub fn bench_with(
    scale: f32,
    tweak: impl FnOnce(&mut RadioSettings),
    before_open: impl FnOnce(&MockBox),
) -> Bench {
    let clock = Clock::new(scale);
    let keyer_box = MockBox::new(clock);
    let monitor = Arc::new(Mutex::new(Monitor::starting_at(
        monitor::Settings {
            sample_rate: 8000,
            pitch_hz: 600.0,
            min_level_dbfs: -65.0,
            scale,
        },
        clock.epoch,
    )));
    let mut rs = RadioSettings::new(8000, 600.0);
    tweak(&mut rs);
    let radio = MockRadio::start(rs, keyer_box.clone(), monitor.clone(), None, None);
    before_open(&keyer_box);
    let rig = KeyerRig::open(
        keyer_box.transport(),
        monitor,
        Settings {
            frequency_hz: 7_030_000,
            min_level_dbfs: -65.0,
            scale,
            duty: 0.5,
            duty_window: Duration::from_secs(600),
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let station = Station::new(rig, cfg_at(scale), Some(dir.path().join("health.csv")));
    station.configure().unwrap();
    thread::sleep(Duration::from_secs_f64(2.5 / f64::from(scale)));
    Bench {
        station,
        keyer_box,
        radio,
        dir,
    }
}
