//! The rules of the Pico 2 keyer box, the small USB device that keys any radio's
//! key jack for `hfnode` (`station.rig = "keyer"`): the lines both sides send
//! ([`frame`]), the Morse timing it keys text with ([`morse`]), and the box itself as
//! a state machine with every limit it enforces ([`keyer`]).
//!
//! One copy of these rules: the box's firmware (firmware/pico2-keyer) runs
//! [`keyer::Keyer`] in its 1 ms control loop, and hfnode runs the same code in its
//! mock box for its tests and self-test, and uses [`frame`] and [`morse`] to talk to
//! the real one and to know when it keys each element. docs/keyer-protocol.md
//! describes the protocol for people.
//!
//! `no_std` and without an allocator, so that it builds for the RP2350 as it is.

#![cfg_attr(not(test), no_std)]

pub mod frame;
pub mod keyer;
pub mod morse;

/// Protocol version, reported by `HELLO`.
pub const VERSION: u32 = 1;

/// The box's name, the last field of `HELLO`.
pub const NAME: &str = "PICO2-KEYER";

/// Longest line either side sends, without its newline.
pub const MAX_LINE: usize = 80;

/// Most characters of text in one `CW` run (the IC-7300's keyer takes 30 too, so
/// the station's pieces fit both).
pub const MAX_TEXT: usize = 30;

/// Keying speeds the box accepts, in words per minute (PARIS).
pub const MIN_WPM: u32 = 5;
pub const MAX_WPM: u32 = 50;

/// The box's own limits, as it reports them in `HELLO`. hfnode refuses a box that
/// reports anything looser than these.
pub mod limits {
    /// Longest the key may stay down at once. No element is longer than a dash at
    /// [`crate::MIN_WPM`] (3 x 240 ms = 720 ms); a key-down past this opens the key
    /// and trips the box until it is power-cycled.
    pub const KEY_DOWN_MS: u32 = 1_000;
    /// Longest one `CW` run may last; text whose Morse length is over it is refused.
    pub const RUN_MS: u32 = 60_000;
    /// A run stops when no valid line has arrived for this long: the computer
    /// crashed, hfnode was killed, or the cable came out.
    pub const LINK_TIMEOUT_MS: u32 = 2_000;
    /// The firmware's hardware watchdog: its control loop must feed it this often.
    pub const WATCHDOG_MS: u32 = 500;
}
