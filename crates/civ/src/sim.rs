//! An in-memory radio for tests and dry runs.

use crate::{Result, Rig, RigError, MAX_CW_CHARS};
use std::time::{Duration, Instant};

/// Simulated IC-7300. Keyed text "transmits" element by element for as long as it
/// would take at the keyer speed, with semi break-in: the transmitter switches on
/// `tx_on_delay` after [`Rig::send_cw`], and drops back to receive once the key has
/// been up for the break-in delay, which can happen between words if that delay is
/// short. The meters read RF only while the key is down. SWR and faults are
/// settable.
#[derive(Debug)]
pub struct SimRig {
    pub frequency_hz: u64,
    pub cw_mode: bool,
    pub power_watts: u32,
    /// Keyer speed in use, limited to the radio's 6-48 wpm.
    pub key_wpm: u32,
    pub break_in: bool,
    /// Semi break-in delay in dots.
    pub break_in_delay_dots: f32,
    pub swr: f32,
    /// Every piece of text keyed, in order.
    pub sent: Vec<String>,
    pub tunes: u32,
    /// When set, the key sticks down once keying starts: the carrier stays on and
    /// the transmitter never drops back to receive by itself, until stopped.
    pub stuck_key: bool,
    /// Time from a CW message being accepted to the transmitter switching on and the
    /// keyer starting (simulated time).
    pub tx_on_delay: Duration,
    /// Fault: the stop-CW command times out and does nothing.
    pub stop_cw_fails: bool,
    /// Fault: the radio stays on transmit whatever it is told.
    pub tx_jammed: bool,
    /// Fault: the tuner cannot match the load and bypasses itself.
    pub tuner_bypassed: bool,
    /// Someone at the radio switched split on, transmitting on this frequency.
    pub split_tx_hz: Option<u64>,
    /// Something other than the node keys the radio (its TRANSMIT switch, a key at
    /// its jack): on transmit, with output, until switched to receive.
    pub keyed_elsewhere: bool,
    /// The radio's own Time-Out Timer; `Some(ZERO)` is OFF.
    pub time_out_timer: Option<Duration>,
    /// What the RF power reads back as, if not what was set.
    pub power_read_back: Option<f32>,
    /// What the Po meter reads with the key down, if not the power set.
    pub po_override: Option<f32>,
    /// [`Rig::inhibit_transmit`] was sent: break-in off and TX Inhibit on. Nothing
    /// keyed after it goes out.
    pub tx_inhibit: bool,
    /// Simulated speed-up: keying takes `real time / time_scale`.
    pub time_scale: f32,
    keying: Option<Keying>,
    forced_tx: bool,
    tune_until: Option<Instant>,
}

/// A CW message being sent: when the keyer starts, its key-down/key-up runs and
/// the break-in delay, all in real (scaled) time.
#[derive(Debug)]
struct Keying {
    start: Instant,
    runs: Vec<(bool, Duration)>,
    hang: Duration,
}

/// What the radio is doing at one moment.
#[derive(Debug, Default)]
struct Phase {
    /// The keyer still has text to send.
    busy: bool,
    key_down: bool,
    tx: bool,
}

impl Default for SimRig {
    fn default() -> Self {
        Self {
            frequency_hz: 7_030_000,
            cw_mode: false,
            power_watts: 100,
            key_wpm: 20,
            break_in: false,
            break_in_delay_dots: 7.5,
            swr: 1.3,
            sent: Vec::new(),
            tunes: 0,
            stuck_key: false,
            tx_on_delay: Duration::from_millis(20),
            stop_cw_fails: false,
            tx_jammed: false,
            tuner_bypassed: false,
            split_tx_hz: None,
            keyed_elsewhere: false,
            time_out_timer: Some(Duration::from_secs(180)),
            power_read_back: None,
            po_override: None,
            tx_inhibit: false,
            time_scale: 1.0,
            keying: None,
            forced_tx: false,
            tune_until: None,
        }
    }
}

impl SimRig {
    pub fn new() -> Self {
        Self::default()
    }

    fn scaled(&self, d: Duration) -> Duration {
        d.div_f32(self.time_scale.max(0.001))
    }

    fn dot(&self) -> Duration {
        Duration::from_secs_f32(1.2 / self.key_wpm as f32)
    }

    fn phase(&self, now: Instant) -> Phase {
        let Some(k) = &self.keying else {
            return Phase::default();
        };
        let Some(t) = now.checked_duration_since(k.start) else {
            return Phase {
                busy: true,
                ..Phase::default()
            };
        };
        let (mut at, mut last_mark_end) = (Duration::ZERO, None);
        for &(mark, len) in &k.runs {
            if t < at + len {
                if mark {
                    return Phase {
                        busy: true,
                        key_down: true,
                        tx: true,
                    };
                }
                break;
            }
            at += len;
            if mark {
                last_mark_end = Some(at);
            }
        }
        let total: Duration = k.runs.iter().map(|r| r.1).sum();
        Phase {
            busy: t < total,
            key_down: false,
            tx: last_mark_end.is_some_and(|e| t - e < k.hang),
        }
    }

    fn key_down(&self, now: Instant) -> bool {
        let stuck = self.stuck_key && self.keying.as_ref().is_some_and(|k| now >= k.start);
        stuck || self.keyed_elsewhere || self.phase(now).key_down
    }

    /// Whether the keyer still has text to send.
    pub fn keyer_busy(&self) -> bool {
        self.phase(Instant::now()).busy
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
        self.key_wpm = wpm.clamp(6, 48);
        Ok(())
    }

    fn set_break_in(&mut self, on: bool) -> Result<()> {
        self.break_in = on;
        Ok(())
    }

    fn set_break_in_delay(&mut self, dots: f32) -> Result<()> {
        self.break_in_delay_dots = dots.clamp(2.0, 13.0);
        Ok(())
    }

    fn dot_duration(&mut self) -> Result<Duration> {
        Ok(self.scaled(self.dot()))
    }

    fn start_tune(&mut self) -> Result<()> {
        self.tunes += 1;
        self.tune_until = Some(Instant::now() + self.scaled(Duration::from_millis(1500)));
        Ok(())
    }

    fn tuner_busy(&mut self) -> Result<bool> {
        Ok(self.tune_until.is_some_and(|t| Instant::now() < t))
    }

    fn tuner_matched(&mut self) -> Result<bool> {
        Ok(!self.tuner_bypassed)
    }

    fn transmit_frequency(&mut self) -> Result<u64> {
        Ok(self.split_tx_hz.unwrap_or(self.frequency_hz))
    }

    fn split_or_delta_tx(&mut self) -> Result<bool> {
        Ok(self.split_tx_hz.is_some())
    }

    fn read_swr(&mut self) -> Result<f32> {
        Ok(if self.key_down(Instant::now()) {
            self.swr
        } else {
            1.0
        })
    }

    fn read_po(&mut self) -> Result<f32> {
        Ok(if self.key_down(Instant::now()) {
            self.po_override.unwrap_or(self.power_watts as f32)
        } else {
            0.0
        })
    }

    fn send_cw(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_CW_CHARS {
            return Err(RigError::Rejected);
        }
        if !self.cw_mode || !self.break_in || self.tx_inhibit {
            // A real radio would not transmit; make the mistake visible in tests.
            return Err(RigError::Protocol(
                "send_cw without CW mode and break-in".into(),
            ));
        }
        if self.keyer_busy() {
            // Likewise for text sent before the last message has finished.
            return Err(RigError::Protocol(
                "send_cw while the keyer is still sending".into(),
            ));
        }
        self.sent.push(text.to_string());
        let runs = cw::Keyer::new(8000, 600.0, self.key_wpm as f32)
            .timing(text)
            .into_iter()
            .map(|(mark, ms)| (mark, self.scaled(Duration::from_secs_f32(ms / 1000.0))))
            .collect();
        self.keying = Some(Keying {
            start: Instant::now() + self.scaled(self.tx_on_delay),
            runs,
            hang: self.scaled(self.dot().mul_f32(self.break_in_delay_dots)),
        });
        Ok(())
    }

    fn stop_cw(&mut self) -> Result<()> {
        if self.stop_cw_fails {
            return Err(RigError::Timeout);
        }
        self.keying = None;
        self.stuck_key = false;
        Ok(())
    }

    fn is_transmitting(&mut self) -> Result<bool> {
        Ok(self.tx_jammed
            || self.forced_tx
            || self.keyed_elsewhere
            || self.stuck_key && self.keying.is_some()
            || self.phase(Instant::now()).tx)
    }

    fn set_transmit(&mut self, tx: bool) -> Result<()> {
        self.forced_tx = tx;
        if !tx {
            self.keying = None;
            self.stuck_key = false;
            self.keyed_elsewhere = false;
        }
        Ok(())
    }

    fn polls_status_while_idle(&self) -> bool {
        true
    }

    fn time_out_timer(&mut self) -> Result<Option<Duration>> {
        Ok(self.time_out_timer)
    }

    fn rf_power_watts(&mut self) -> Result<Option<f32>> {
        Ok(Some(
            self.power_read_back.unwrap_or(self.power_watts as f32),
        ))
    }

    fn inhibit_transmit(&mut self) -> Result<()> {
        self.break_in = false;
        self.tx_inhibit = true;
        Ok(())
    }
}
