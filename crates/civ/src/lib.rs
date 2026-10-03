//! Radio control for the node.
//!
//! [`Rig`] is everything the node asks of the radio. [`sim::SimRig`] implements it
//! in memory for tests and `hfnode sim`; the IC-7300 implementation over CI-V lives
//! in [`ic7300`].

pub mod frame;
pub mod ic7300;
pub mod sim;

use std::fmt;

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
    /// Start an internal antenna tuner cycle.
    fn start_tune(&mut self) -> Result<()>;
    /// Whether a tuner cycle is still running.
    fn tuner_busy(&mut self) -> Result<bool>;
    /// SWR meter reading. Only meaningful while transmitting.
    fn read_swr(&mut self) -> Result<f32>;
    /// Key `text` (at most [`MAX_CW_CHARS`]) with the internal keyer.
    fn send_cw(&mut self, text: &str) -> Result<()>;
    /// Abort any keyer text still being sent.
    fn stop_cw(&mut self) -> Result<()>;
    fn is_transmitting(&mut self) -> Result<bool>;
    /// Force transmit on or off. The node only ever calls this with `false`, to
    /// make sure the radio is back on receive.
    fn set_transmit(&mut self, tx: bool) -> Result<()>;
}

/// Split `text` into pieces the keyer accepts, on word boundaries where possible.
pub fn split_for_keyer(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.len() > MAX_CW_CHARS {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let rest = word.split_off(MAX_CW_CHARS);
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
        let long = split_for_keyer(&"X".repeat(70));
        assert_eq!(
            long.iter().map(String::len).collect::<Vec<_>>(),
            [30, 30, 10]
        );
    }
}
