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
//!   radio's status. If it cannot be confirmed, transmitting is inhibited until the
//!   node is restarted.
//! - **Software watchdog.** A separate thread forces the radio back to receive if
//!   any one keying run lasts longer than `max_key_seconds`, and keeps trying until
//!   receive is confirmed. It backs up, and does not replace, the hardware PTT
//!   timer in series with the keying line.
//! - **SWR check.** SWR is sampled repeatedly during the first second or so of
//!   keying, counting only samples taken with the Po meter showing output, and the
//!   highest is used; above the limit the node stops and stays silent until the
//!   next window. Until one such sample is obtained, every piece is sampled.
//! - **Reduced power**, set at start-up.
//! - **Tuning** at start-up and at the top of each listening window.
//! - **A health log** of every tune and SWR reading, so a slow upward trend (a
//!   corroding connector, a loosened coil) shows up before it becomes a fault.

use crate::session::Transmission;
use civ::{split_for_keyer, Rig, RigError};
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
            swr_delay: Duration::from_millis(50),
            swr_window: Duration::from_secs(1),
            // A quarter of the set power: well clear of key-up (0) and of the
            // CW envelope's rise and fall.
            swr_min_po: (c.power_watts as f32 * 0.25).max(2.0),
            // 10 dots: 3 dots more than a word gap. At most 2 s (at 6 wpm), which
            // the 3 s stuck margin covers.
            break_in_delay_dots: 10.0,
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
    /// The radio could not be confirmed back on receive; nothing more is sent until
    /// the node is restarted.
    Inhibited,
    Rig(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwrLockout => write!(f, "transmit locked out after high SWR"),
            Self::HighSwr(s) => write!(f, "SWR {s:.1} above limit"),
            Self::Stuck => write!(f, "transmitter did not return to receive"),
            Self::Inhibited => write!(
                f,
                "radio not confirmed on receive: transmit inhibited until restart"
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

/// How many times [`force_receive`] tries before giving up.
const FORCE_RX_ATTEMPTS: u32 = 3;

/// Put the radio on receive and confirm it: stop the keyer and switch to receive
/// (each sent whether or not the other worked), then read the transmit status.
/// Repeated a few times; an error means receive could not be confirmed.
pub fn force_receive<R: Rig + ?Sized>(r: &mut R) -> civ::Result<()> {
    let mut last = RigError::Timeout;
    for attempt in 0..FORCE_RX_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(100));
        }
        if let Err(e) = r.stop_cw() {
            log::warn!("forcing receive: stop CW: {e}");
        }
        if let Err(e) = r.set_transmit(false) {
            log::warn!("forcing receive: set receive: {e}");
        }
        match r.is_transmitting() {
            Ok(false) => return Ok(()),
            Ok(true) => last = RigError::Protocol("radio still reports transmit".into()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// [`force_receive`], latching `inhibit` if receive is not confirmed.
fn force_receive_or_inhibit<R: Rig>(rig: &Mutex<R>, inhibit: &AtomicBool) -> Result<(), TxError> {
    let mut r = rig.lock().unwrap_or_else(|e| e.into_inner());
    force_receive(&mut *r).map_err(|e| {
        inhibit.store(true, Ordering::SeqCst);
        log::error!("radio not confirmed on receive ({e}): transmit inhibited until restart");
        TxError::Inhibited
    })
}

pub struct Station<R: Rig + 'static> {
    rig: Arc<Mutex<R>>,
    cfg: StationConfig,
    keying_since: Arc<Mutex<Option<Instant>>>,
    watchdog_fired: Arc<AtomicBool>,
    /// Latched when the radio could not be confirmed on receive; never cleared.
    tx_inhibit: Arc<AtomicBool>,
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
            tx_inhibit: Arc::new(AtomicBool::new(false)),
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

    /// Whether transmitting has been inhibited until restart.
    pub fn tx_inhibited(&self) -> bool {
        self.tx_inhibit.load(Ordering::SeqCst)
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
            r.set_break_in_delay(c.break_in_delay_dots)?;
            r.set_break_in(true)
        })
    }

    /// Run the internal tuner and wait for it to finish. Call at start-up and at
    /// the top of each listening window; clears any SWR lockout from the last window.
    pub fn start_window(&mut self) -> civ::Result<()> {
        if self.tx_inhibited() {
            // Tuning transmits.
            return Err(RigError::Protocol(TxError::Inhibited.to_string()));
        }
        self.swr_lockout = false;
        self.swr_checked = false;
        self.with_rig(|r| r.start_tune())?;
        let t0 = Instant::now();
        while self.with_rig(|r| r.tuner_busy())? {
            if t0.elapsed() > Duration::from_secs(15) {
                self.health("tune", "timeout");
                self.force_rx()
                    .map_err(|e| RigError::Protocol(e.to_string()))?;
                return Err(civ::RigError::Timeout);
            }
            thread::sleep(self.cfg.poll);
        }
        self.health("tune", &format!("{}ms", t0.elapsed().as_millis()));
        Ok(())
    }

    /// Key a transmission, enforcing every safety rule above.
    pub fn transmit(&mut self, tx: &Transmission) -> Result<(), TxError> {
        if self.tx_inhibited() {
            return Err(TxError::Inhibited);
        }
        if self.swr_lockout {
            return Err(TxError::SwrLockout);
        }
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
        for (si, segment) in tx.segments.iter().enumerate() {
            if si > 0 {
                thread::sleep(self.cfg.segment_pause);
            }
            for piece in split_for_keyer(segment) {
                self.wait_for_receive(Instant::now() + Duration::from_secs(2))?;
                // Timed at the speed the radio's keyer is really using.
                let dot = self.with_rig(|r| r.dot_duration())?;
                let keying = dot * cw::units(&piece);
                let hang = dot.mul_f32(self.cfg.break_in_delay_dots);
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                self.with_rig(|r| r.send_cw(&piece))?;
                let sent = Instant::now();

                if !self.swr_checked {
                    self.check_swr(sent, keying + hang)?;
                }
                // The radio's status says nothing about the keyer until the whole
                // piece has had time to go out (it reads receive before semi
                // break-in has switched over), so wait that long first.
                self.sleep_until(sent + keying)?;
                self.wait_for_receive(sent + keying + hang + self.cfg.stuck_margin)?;
                *self.keying_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        Ok(())
    }

    /// Sample SWR from `swr_delay` after `sent` until `swr_window` or `on_air` has
    /// passed, using only samples with the Po meter showing output on both sides of
    /// the SWR reading (key-up reads as SWR 1.0). Marks SWR as checked only if
    /// there was such a sample.
    fn check_swr(&mut self, sent: Instant, on_air: Duration) -> Result<(), TxError> {
        let end = sent + self.cfg.swr_window.min(on_air);
        let min_po = self.cfg.swr_min_po;
        let mut worst: Option<f32> = None;
        self.sleep_until(sent + self.cfg.swr_delay)?;
        while Instant::now() < end {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            let sample = self.with_rig(|r| {
                let before = r.read_po()?;
                let swr = r.read_swr()?;
                let after = r.read_po()?;
                Ok((before.min(after) >= min_po).then_some(swr))
            })?;
            if let Some(swr) = sample {
                worst = Some(worst.map_or(swr, |w| w.max(swr)));
                if swr > self.cfg.swr_limit {
                    self.health("swr", &format!("{swr:.2}"));
                    self.swr_lockout = true;
                    log::error!(
                        "SWR {swr:.2} above {:.1}: silent until next window",
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
            }
            None => log::warn!("no SWR reading with the key down; will sample the next piece"),
        }
        Ok(())
    }

    /// Sleep until `t`, stopping early if the watchdog fires.
    fn sleep_until(&self, t: Instant) -> Result<(), TxError> {
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
            let now = Instant::now();
            if now >= t {
                return Ok(());
            }
            thread::sleep((t - now).min(self.cfg.poll));
        }
    }

    /// Wait until the radio reports receive, or declare it stuck at `deadline`.
    fn wait_for_receive(&self, deadline: Instant) -> Result<(), TxError> {
        loop {
            if self.watchdog_fired.load(Ordering::SeqCst) {
                return Err(TxError::Stuck);
            }
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
            poll: Duration::from_millis(2),
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
        // Keying ends long after send_cw returns; the watchdog must still see it.
        let mut rig = fast_rig();
        rig.tx_on_delay = Duration::from_millis(1500);
        let mut st = Station::new(rig, cfg(), None);
        st.configure().unwrap();
        let since = st.keying_since.clone();
        let rig = st.rig();
        let watcher = thread::spawn(move || {
            let mut unwatched = 0;
            for _ in 0..200 {
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
        st.transmit(&tx(&["TEST"])).unwrap();
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
    fn swr_is_checked_only_with_output_present() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // No sample reaches the Po threshold: not checked, so sampled next time.
        st.cfg.swr_min_po = 1000.0;
        st.transmit(&tx(&["TEST"])).unwrap();
        assert!(!st.swr_checked);
        st.cfg.swr_min_po = 10.0;
        st.rig().lock().unwrap().swr = 3.5;
        assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::HighSwr(3.5)));
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

    #[test]
    fn unconfirmed_receive_inhibits_transmit() {
        let mut st = Station::new(fast_rig(), cfg(), None);
        st.configure().unwrap();
        // Stuck on transmit before anything is keyed: nothing is sent, and forcing
        // receive fails.
        st.rig().lock().unwrap().tx_jammed = true;
        assert_eq!(st.transmit(&tx(&["E"])), Err(TxError::Inhibited));
        assert!(st.tx_inhibited());
        assert!(
            st.keying_since.lock().unwrap().is_some(),
            "watchdog retries"
        );
        // Even once the radio recovers, nothing more is keyed until restart.
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
        st.rig().lock().unwrap().tx_jammed = true;
        *st.keying_since.lock().unwrap() = Some(Instant::now());
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
