//! Radio control for the node.
//!
//! [`Rig`] is everything the node asks of the radio. [`sim::SimRig`] implements it
//! in memory for tests and `hfnode sim`; the IC-7300 implementation over CI-V lives
//! in [`ic7300`], and [`mock`] is a byte-level IC-7300 behind a fake serial port for
//! testing that implementation and the node above it. [`preflight`] holds the
//! read-only checks made before the node writes to a real radio, and [`ports`] finds
//! the radio's serial port without opening anything; [`serial`] lowers a port's DTR
//! and RTS lines as soon as it is open. A handheld running a CW firmware (no CI-V)
//! implements [`Rig`] in `hfnode`'s `handheld` module.

pub mod frame;
pub mod ic7300;
pub mod mock;
pub mod ports;
pub mod preflight;
pub mod serial;
pub mod sim;

use std::fmt;
use std::time::Duration;

/// Longest text the radio's keyer accepts in one command.
pub const MAX_CW_CHARS: usize = 30;

#[derive(Debug)]
pub enum RigError {
    Io(std::io::Error),
    Timeout,
    /// The radio answered NG (command rejected).
    Rejected,
    /// The radio answered with something we did not expect.
    Protocol(String),
}

impl fmt::Display for RigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "serial I/O: {e}"),
            Self::Timeout => write!(f, "no reply from radio"),
            Self::Rejected => write!(f, "radio rejected the command (NG)"),
            Self::Protocol(s) => write!(f, "unexpected reply: {s}"),
        }
    }
}

impl std::error::Error for RigError {}

impl From<std::io::Error> for RigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, RigError>;

/// What the node needs from the radio.
pub trait Rig: Send {
    fn frequency(&mut self) -> Result<u64>;
    fn set_frequency(&mut self, hz: u64) -> Result<()>;
    fn set_mode_cw(&mut self) -> Result<()>;
    /// RF power in watts (the radio's scale is mapped from 0-100 W).
    fn set_rf_power_watts(&mut self, watts: u32) -> Result<()>;
    /// Internal keyer speed for [`Rig::send_cw`].
    fn set_key_speed(&mut self, wpm: u32) -> Result<()>;
    /// Semi break-in on, so text sent with [`Rig::send_cw`] keys the transmitter.
    fn set_break_in(&mut self, on: bool) -> Result<()>;
    /// Semi break-in delay, in dots: how long the key must stay up before the radio
    /// drops back to receive. Above 7 dots (a word gap) the radio stays on transmit
    /// for a whole keyer message.
    fn set_break_in_delay(&mut self, dots: f32) -> Result<()>;
    /// Length of one dot at the keyer speed the radio is actually using (after
    /// its own range limits), so callers can time [`Rig::send_cw`] text.
    fn dot_duration(&mut self) -> Result<Duration>;
    /// Start an internal antenna tuner cycle.
    fn start_tune(&mut self) -> Result<()>;
    /// Whether a tuner cycle is still running.
    fn tuner_busy(&mut self) -> Result<bool>;
    /// After a tuner cycle: whether the tuner matched the load. One that cannot
    /// (SWR of 3:1 or more) does not report an error: "TUNE disappears and the
    /// tuning circuit is automatically bypassed" (p. 11-2). Rigs that cannot tell
    /// answer `true`, leaving the SWR check on the first transmission to catch it.
    fn tuner_matched(&mut self) -> Result<bool> {
        Ok(true)
    }
    /// The frequency the radio would transmit on: with split or ∂TX on it is not
    /// the operating frequency. Rigs that cannot tell answer the operating
    /// frequency.
    fn transmit_frequency(&mut self) -> Result<u64> {
        self.frequency()
    }
    /// Whether split or ∂TX is on, either of which moves the transmit frequency
    /// away from the one set. Rigs without them answer `false`.
    fn split_or_delta_tx(&mut self) -> Result<bool> {
        Ok(false)
    }
    /// SWR meter reading. Only meaningful while transmitting with the key down;
    /// with no RF out it reads 1.0.
    fn read_swr(&mut self) -> Result<f32>;
    /// Po (RF output) meter reading, in percent of full output.
    fn read_po(&mut self) -> Result<f32>;
    /// Key `text` (at most [`MAX_CW_CHARS`]) with the internal keyer.
    fn send_cw(&mut self, text: &str) -> Result<()>;
    /// Abort any keyer text still being sent.
    fn stop_cw(&mut self) -> Result<()>;
    fn is_transmitting(&mut self) -> Result<bool>;
    /// Force transmit on or off. The node only ever calls this with `false`, to
    /// make sure the radio is back on receive.
    fn set_transmit(&mut self, tx: bool) -> Result<()>;
    /// Whether the radio has an antenna tuner for [`Rig::start_tune`]. A rig without
    /// one (an FM handheld) answers `false`, and the node never asks it to tune.
    fn has_tuner(&self) -> bool {
        true
    }
    /// Whether [`Rig::read_swr`] and [`Rig::read_po`] measure the transmitter. A rig
    /// without meters answers `false`: the node then cannot check SWR or output,
    /// and relies on the rig's own limits instead.
    fn has_meters(&self) -> bool {
        true
    }
    /// How long the radio must stay on receive before keying a run that keys for
    /// `keying` (at most [`MAX_CW_CHARS`] of text): a handheld's duty-cycle limit,
    /// or a channel someone else is using. The node waits that long, on receive,
    /// and asks again. An error means not to key at all. Rigs with no such limit
    /// answer zero.
    fn rest_needed(&mut self, keying: Duration) -> Result<Duration> {
        let _ = keying;
        Ok(Duration::ZERO)
    }
    /// For a rig without meters, once the radio reads receive after a piece sent
    /// with [`Rig::send_cw`]: whether the rig saw by its own means that the radio
    /// keyed it (a keyer box listens for the radio's sidetone). `Some(false)` stops
    /// the transmission and keeps the node silent until its next window start, as
    /// no output on the Po meter does. Rigs that cannot tell answer `None`.
    fn keying_confirmed(&mut self) -> Result<Option<bool>> {
        Ok(None)
    }
    /// Why [`Rig::is_transmitting`] last answered `true`, if the rig can say more
    /// than that (a keyer box: the sidetone heard after its key opened). It goes
    /// into the reason an inhibit gives.
    fn transmit_detail(&mut self) -> Option<String> {
        None
    }
    /// How long after it is first told to stop the rig may need before it can
    /// show the radio back on receive, at the least: one that only hears its radio
    /// (a keyer box) waits for the audio to catch up. The station waits for this or
    /// the radio's longest break-in delay, whichever is longer.
    fn receive_settle(&self) -> Duration {
        Duration::ZERO
    }
    /// Whether the radio looks keyed with nothing keying it, from what the rig
    /// knows without asking the radio (a keyer box: a steady tone at the sidetone
    /// pitch): why. The station asks often while it is not keying, so it must be
    /// quick.
    fn held_key(&mut self) -> Option<String> {
        None
    }
    /// Whether [`Rig::is_transmitting`] is a quick read of the radio's own status
    /// (the IC-7300's 1C 00) that the station may make every second or so while it
    /// is not keying, to find a transmission it did not start. Rigs whose status
    /// is not that answer `false`.
    fn polls_status_while_idle(&self) -> bool {
        false
    }
    /// The radio's own limit on a transmission, as it is set now: `Some(ZERO)` if
    /// it is off, `None` if the rig has no such setting to read (its limits are its
    /// own, as a keyer box's or a handheld's are).
    fn time_out_timer(&mut self) -> Result<Option<Duration>> {
        Ok(None)
    }
    /// The RF power the radio is set to, in watts on the same 0-100 W scale as
    /// [`Rig::set_rf_power_watts`], read back from the radio; `None` if the rig
    /// cannot tell.
    fn rf_power_watts(&mut self) -> Result<Option<f32>> {
        Ok(None)
    }
    /// When the node inhibits transmitting, and after the radio could not be
    /// confirmed back on receive: everything the rig can still send to stop the
    /// radio transmitting again, on top of stopping the keyer and switching to
    /// receive (the IC-7300: break-in off, so that neither a
    /// keyer message nor a key held closed transmits, and its TX Inhibit function
    /// on). Rigs with nothing more to send do nothing.
    fn inhibit_transmit(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Split `text` into pieces the keyer accepts, on word boundaries where possible.
pub fn split_for_keyer(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        // Split on characters, not bytes, so text that is not ASCII cannot panic
        // here (the driver refuses to send it).
        while let Some((at, _)) = word.char_indices().nth(MAX_CW_CHARS) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let rest = word.split_off(at);
            out.push(std::mem::replace(&mut word, rest));
        }
        if !cur.is_empty() && cur.len() + 1 + word.len() > MAX_CW_CHARS {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyer_pieces_fit() {
        let p = split_for_keyer("R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K");
        assert!(p.iter().all(|s| s.len() <= MAX_CW_CHARS), "{p:?}");
        assert_eq!(
            p.join(" "),
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K"
        );
        let wide = split_for_keyer(&"é".repeat(40));
        assert_eq!(
            wide.iter().map(|p| p.chars().count()).collect::<Vec<_>>(),
            [30, 10]
        );
        let long = split_for_keyer(&"X".repeat(70));
        assert_eq!(
            long.iter().map(String::len).collect::<Vec<_>>(),
            [30, 30, 10]
        );
    }
}
