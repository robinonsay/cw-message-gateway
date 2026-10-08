//! The rules of the Pico 2 keyer box, the small USB device that keys any radio's
//! key jack for `hfnode` (`station.rig = "keyer"`), or holds an FM handheld's PTT and
//! keys a tone into its microphone: the lines both sides send ([`frame`]), the Morse
//! timing it keys text with ([`morse`]), and the box itself as a state machine with
//! every limit it enforces ([`keyer`]).
//!
//! One copy of these rules: the box's firmware (firmware/pico2-keyer) runs
//! [`control::Control`], and so [`keyer::Keyer`], in a loop that never waits (each
//! pass takes microseconds; the USB driver needs one at least every millisecond),
//! and hfnode runs the same [`keyer::Keyer`] in its mock box for its tests and
//! self-test, and uses [`frame`] and [`morse`] to talk to the real one and to know
//! when it keys each element. docs/keyer-protocol.md describes the protocol for
//! people.
//!
//! `no_std` and without an allocator, so that it builds for the RP2350 as it is.

#![cfg_attr(not(test), no_std)]

pub mod control;
pub mod frame;
pub mod keyer;
pub mod morse;

/// Protocol version, reported by `HELLO`. 3: `STATUS` reports the `CLOCK` and
/// `WATCHDOG` trips. 4: `MCW`, the PTT output, the PTT line and `TEST HOLD`.
pub const VERSION: u32 = 4;

/// The box's name, the last field of `HELLO`.
pub const NAME: &str = "PICO2-KEYER";

/// Longest line either side sends, without its newline.
pub const MAX_LINE: usize = 96;

/// Most characters of text in one `CW` or `MCW` run (the IC-7300's keyer takes 30
/// too, so the station's pieces fit both).
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
    /// Longest one `CW` or `MCW` run may last; text whose Morse length is over it
    /// is refused.
    pub const RUN_MS: u32 = 60_000;
    /// Longest the PTT may stay down at once, whatever holds it there: no longer
    /// than the run limit (the safety audit's handheld constraint C3). `MCW`
    /// refuses a run that would reach it; PTT down past it opens the PTT and trips
    /// the box until it is power-cycled. The key-down limit still holds each tone
    /// element to [`KEY_DOWN_MS`].
    pub const PTT_MS: u32 = 60_000;
    /// A run stops when no valid line has arrived for this long: the computer
    /// crashed, hfnode was killed, or the cable came out.
    pub const LINK_TIMEOUT_MS: u32 = 2_000;
    /// The firmware's hardware watchdog: its control loop must feed it this often.
    pub const WATCHDOG_MS: u32 = 500;
    /// After a run ends, however it ends, the key stays up at least this long
    /// before the box takes another: runs sent back to back cannot hold the key
    /// down between them.
    pub const REST_MS: u32 = 1_000;
    /// A key-up shorter than this, one dot at [`crate::MAX_WPM`], does not count as
    /// the key opening: the key-down limit goes on timing across it.
    pub const MIN_GAP_MS: u32 = 1200 / crate::MAX_WPM;
    /// The duty budget: every millisecond the transmitter is keyed (the key down,
    /// or the PTT down) spends one, every millisecond it is not earns one back, up
    /// to this much. A run is refused while the budget holds less than the time it
    /// keys the transmitter: the key-down time of a `CW` run, the whole PTT time of
    /// an `MCW` run (a steady carrier). Over any stretch of time the transmitter is
    /// keyed at most half of it plus half of this: at most 55% of any 10 minutes,
    /// and 50% in the long run. The budget starts full only when the box was
    /// powered up.
    pub const DUTY_BUDGET_MS: u32 = 60_000;
    /// `TEST HANG` and `TEST STUCK` are taken only this soon after `TEST ARM`.
    pub const ARM_MS: u32 = 2_000;
    /// A pass of the firmware's control loop that takes longer than this with the
    /// key or the tone on trips the box ([`crate::control`]): the pin may have
    /// stayed on longer than the box's own timeline shows.
    pub const SLOW_PASS_MS: u32 = 10;
    /// If the watchdog has not reset the box this long after `TEST HANG` stopped
    /// its control loop, the loop opens the key and the PTT and stops the tone
    /// itself ([`crate::control`]).
    pub const HANG_OPEN_MS: u32 = WATCHDOG_MS + 500;
    /// The control loop checks its clock against a second one, the processor's
    /// own count, every time either has run this long ([`crate::control`]): the
    /// clock must have run at least half as long as the processor, and the
    /// processor's count at least a quarter as long as the clock, or the box
    /// trips (`CLOCK`) and stops feeding its watchdog.
    pub const CLOCK_CHECK_MS: u32 = 50;
    /// After a restart that kept the box's saved state ([`crate::keyer::Saved`]),
    /// a key that was down when the state was last saved counts as having stayed
    /// down this much longer: as long as the key can stay down after the control
    /// loop stops, before the watchdog resets the chip or the loop opens the key
    /// itself ([`HANG_OPEN_MS`]). It is taken from the duty budget.
    pub const RESTART_CHARGE_MS: u32 = HANG_OPEN_MS;
}

/// The timing of an `MCW` run, around the Morse.
pub mod mcw {
    /// PTT down this long before the first element: the radio switching to
    /// transmit and the far receiver's squelch opening.
    pub const LEAD_MS: u32 = 500;
    /// PTT held this long after the last element, so it is not clipped.
    pub const TAIL_MS: u32 = 200;
    /// The PTT line must show the PTT's last move this long after it: low after
    /// the box closes the PTT (or the run ends: the radio is not keyed), high after
    /// it opens it (or the box trips: something else holds the PTT, and the radio
    /// may still be transmitting).
    pub const LINE_MS: u32 = 100;
}
