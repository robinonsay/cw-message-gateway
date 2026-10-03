//! Transmit-side safety for unattended operation.
//!
//! Receive-only operation is harmless; the risks are on transmit. This layer is the
//! only code that keys the radio, and it enforces:
//!
//! - **Bounded keying runs.** Text goes out in keyer-sized pieces with a pause
//!   between segments, and nothing is sent while the radio still reports transmit.
//! - **Software watchdog.** A separate thread forces the radio back to receive if
//!   any one keying run lasts longer than `max_key_seconds`. It backs up, and does
//!   not replace, the hardware PTT timer in series with the keying line.
//! - **SWR check.** SWR is read shortly into the first transmission of each window;
//!   above the limit the node stops and stays silent until the next window.
//! - **Reduced power**, set at start-up.
//! - **Tuning** at start-up and at the top of each listening window.
//! - **A health log** of every tune and SWR reading, so a slow upward trend (a
//!   corroding connector, a loosened coil) shows up before it becomes a fault.

use crate::session::Transmission;
use civ::{split_for_keyer, Rig};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
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
    /// How long after keying starts to read the SWR meter.
    pub swr_delay: Duration,
    /// Extra time allowed beyond the computed keying time before the transmitter
    /// is declared stuck.
    pub stuck_margin: Duration,
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
            swr_delay: Duration::from_millis(400),
            stuck_margin: Duration::from_secs(3),
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
    /// The radio stayed on transmit too long and was forced back to receive.
    Stuck,
    Rig(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwrLockout => write!(f, "transmit locked out after high SWR"),
            Self::HighSwr(s) => write!(f, "SWR {s:.1} above limit"),
            Self::Stuck => write!(f, "transmitter did not return to receive"),
            Self::Rig(e) => write!(f, "radio error: {e}"),
        }
    }
}

impl From<civ::RigError> for TxError {
    fn from(e: civ::RigError) -> Self {
        Self::Rig(e.to_string())
    }
}

pub struct Station<R: Rig + 'static> {
    rig: Arc<Mutex<R>>,
    cfg: StationConfig,
    keying_since: Arc<Mutex<Option<Instant>>>,
    watchdog_fired: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    swr_lockout: bool,
    swr_checked: bool,
    health_log: Option<PathBuf>,
}

impl<R: Rig + 'static> Station<R> {
    pub fn new(rig: R, cfg: StationConfig, health_log: Option<PathBuf>) -> Self {
        let s = Self {
            rig: Arc::new(Mutex::new(rig)),
            cfg,
            keying_since: Arc::new(Mutex::new(None)),
            watchdog_fired: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            swr_lockout: false,
            swr_checked: false,
            health_log,
        };
        s.spawn_watchdog();
        s
    }

    pub fn rig(&self) -> Arc<Mutex<R>> {
        self.rig.clone()
    }

    fn spawn_watchdog(&self) {
        let (rig, since, fired, stop, max) = (
            self.rig.clone(),
            self.keying_since.clone(),
            self.watchdog_fired.clone(),
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
                    let mut r = rig.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = r.stop_cw();
                    let _ = r.set_transmit(false);
                    *since.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
        });
    }

    fn with_rig<T>(&self, f: impl FnOnce(&mut R) -> civ::Result<T>) -> civ::Result<T> {
        let mut r = self.rig.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut r)
    }

    fn health(&self, event: &str, value: &str) {
        log::info!("health: {event} {value}");
        if let Some(path) = &self.health_log {
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
    }

    /// Put the radio in the node's operating state.
    pub fn configure(&self) -> civ::Result<()> {
        let c = self.cfg.clone();
        self.with_rig(|r| {
            r.set_transmit(false)?;
            r.set_frequency(c.frequency_hz)?;
            r.set_mode_cw()?;
            r.set_rf_power_watts(c.power_watts)?;
            r.set_key_speed(c.key_speed_wpm)?;
            r.set_break_in(true)
        })
    }

    /// Run the internal tuner and wait for it to finish. Call at start-up and at
    /// the top of each listening window; clears any SWR lockout from the last window.
    pub fn start_window(&mut self) -> civ::Result<()> {
        self.swr_lockout = false;
        self.swr_checked = false;
        self.with_rig(|r| r.start_tune())?;
        let t0 = Instant::now();
        while self.with_rig(|r| r.tuner_busy())? {
            if t0.elapsed() > Duration::from_secs(15) {
                self.health("tune", "timeout");
                self.with_rig(|r| r.set_transmit(false))?;
                return Err(civ::RigError::Timeout);
            }
            thread::sleep(self.cfg.poll);
        }
        self.health("tune", &format!("{}ms", t0.elapsed().as_millis()));
        Ok(())
    }

    /// Key a transmission, enforcing every safety rule above.
    pub fn transmit(&mut self, tx: &Transmission) -> Result<(), TxError> {
        if self.swr_lockout {
            return Err(TxError::SwrLockout);
        }
        let result = self.transmit_inner(tx);
        // Whatever happened, make sure we end on receive.
        *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if result.is_err() {
            let _ = self.with_rig(|r| {
                r.stop_cw()?;
                r.set_transmit(false)
            });
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
                self.wait_for_receive(Duration::from_secs(2))?;
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                self.with_rig(|r| r.send_cw(&piece))?;
                let keying = Duration::from_millis(cw::duration_ms(&piece, self.cfg.key_speed_wpm));

                if !self.swr_checked {
                    thread::sleep(self.cfg.swr_delay.min(keying));
                    let swr = self.with_rig(|r| r.read_swr())?;
                    self.swr_checked = true;
                    self.health("swr", &format!("{swr:.2}"));
                    if swr > self.cfg.swr_limit {
                        self.swr_lockout = true;
                        log::error!(
                            "SWR {swr:.2} above {:.1}: silent until next window",
                            self.cfg.swr_limit
                        );
                        return Err(TxError::HighSwr(swr));
                    }
                }
                self.wait_for_receive(keying + self.cfg.stuck_margin)?;
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        Ok(())
    }

    /// Wait until the radio reports receive, or declare it stuck.
    fn wait_for_receive(&self, limit: Duration) -> Result<(), TxError> {
        let t0 = Instant::now();
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            if !self.with_rig(|r| r.is_transmitting())? {
                return Ok(());
            }
            if t0.elapsed() > limit {
                log::error!("radio still transmitting after {limit:?}; forcing receive");
                return Err(TxError::Stuck);
            }
            thread::sleep(self.cfg.poll);
        }
    }
}

impl<R: Rig + 'static> Drop for Station<R> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.with_rig(|r| {
            r.stop_cw()?;
            r.set_transmit(false)
        });
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
            swr_delay: Duration::from_millis(5),
            stuck_margin: Duration::from_millis(300),
            poll: Duration::from_millis(5),
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
        let r = rig.lock().unwrap();
        assert_eq!(r.power_watts, 40);
        assert!(r.cw_mode && r.break_in);
        assert_eq!(r.tunes, 1);
        assert!(r.sent.iter().all(|p| p.len() <= civ::MAX_CW_CHARS));
        assert_eq!(
            r.sent.join(" "),
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K SECOND = B"
        );
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
    fn stuck_transmitter_is_forced_to_receive() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        st.rig().lock().unwrap().stuck_key = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Stuck));
        assert!(!st.rig().lock().unwrap().is_transmitting().unwrap());
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
}
