//! An in-memory radio for tests and dry runs.

use crate::{Result, Rig, RigError, MAX_CW_CHARS};
use std::time::{Duration, Instant};

/// Simulated IC-7300. Keyed text "transmits" for as long as it would take at the
/// keyer speed; SWR and faults are settable.
#[derive(Debug)]
pub struct SimRig {
    pub frequency_hz: u64,
    pub cw_mode: bool,
    pub power_watts: u32,
    pub key_wpm: u32,
    pub break_in: bool,
    pub swr: f32,
    /// Every piece of text keyed, in order.
    pub sent: Vec<String>,
    pub tunes: u32,
    /// When set, the transmitter never drops back to receive by itself.
    pub stuck_key: bool,
    /// Simulated speed-up: keying takes `real time / time_scale`.
    pub time_scale: f32,
    tx_until: Option<Instant>,
    forced_tx: bool,
    tune_until: Option<Instant>,
}

impl Default for SimRig {
    fn default() -> Self {
        Self {
            frequency_hz: 7_030_000,
            cw_mode: false,
            power_watts: 100,
            key_wpm: 20,
            break_in: false,
            swr: 1.3,
            sent: Vec::new(),
            tunes: 0,
            stuck_key: false,
            time_scale: 1.0,
            tx_until: None,
            forced_tx: false,
            tune_until: None,
        }
    }
}

impl SimRig {
    pub fn new() -> Self {
        Self::default()
    }

    fn scaled(&self, ms: u64) -> Duration {
        Duration::from_millis((ms as f32 / self.time_scale.max(0.001)) as u64)
    }
}

impl Rig for SimRig {
    fn frequency(&mut self) -> Result<u64> {
        Ok(self.frequency_hz)
    }

    fn set_frequency(&mut self, hz: u64) -> Result<()> {
        self.frequency_hz = hz;
        Ok(())
    }

    fn set_mode_cw(&mut self) -> Result<()> {
        self.cw_mode = true;
        Ok(())
    }

    fn set_rf_power_watts(&mut self, watts: u32) -> Result<()> {
        self.power_watts = watts;
        Ok(())
    }

    fn set_key_speed(&mut self, wpm: u32) -> Result<()> {
        self.key_wpm = wpm;
        Ok(())
    }

    fn set_break_in(&mut self, on: bool) -> Result<()> {
        self.break_in = on;
        Ok(())
    }

    fn start_tune(&mut self) -> Result<()> {
        self.tunes += 1;
        self.tune_until = Some(Instant::now() + self.scaled(1500));
        Ok(())
    }

    fn tuner_busy(&mut self) -> Result<bool> {
        Ok(self.tune_until.is_some_and(|t| Instant::now() < t))
    }

    fn read_swr(&mut self) -> Result<f32> {
        Ok(self.swr)
    }

    fn send_cw(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_CW_CHARS {
            return Err(RigError::Rejected);
        }
        if !self.cw_mode || !self.break_in {
            // A real radio would not transmit; make the mistake visible in tests.
            return Err(RigError::Protocol("send_cw without CW mode and break-in".into()));
        }
        self.sent.push(text.to_string());
        let ms = cw::duration_ms(text, self.key_wpm);
        self.tx_until = Some(Instant::now() + self.scaled(ms));
        Ok(())
    }

    fn stop_cw(&mut self) -> Result<()> {
        self.tx_until = None;
        self.stuck_key = false;
        Ok(())
    }

    fn is_transmitting(&mut self) -> Result<bool> {
        Ok(self.forced_tx || self.stuck_key && self.tx_until.is_some() || self.tx_until.is_some_and(|t| Instant::now() < t))
    }

    fn set_transmit(&mut self, tx: bool) -> Result<()> {
        self.forced_tx = tx;
        if !tx {
            self.tx_until = None;
            self.stuck_key = false;
        }
        Ok(())
    }
}
