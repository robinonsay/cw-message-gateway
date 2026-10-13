//! The Pico 2 decoder box: a receive-only Raspberry Pi Pico 2 that listens to a
//! radio's headphone audio, decodes the Morse in it, and sends letters over USB
//! serial (docs/decoder-protocol.md, docs/decoder.md). Plugged into any computer it
//! types what it hears into a serial terminal; to hfnode it sends the letters, the
//! decoder's state and the sidetone monitor's measurements in framed lines.
//!
//! One copy of the box's behaviour: firmware/pico2-decoder is the board (the ADC,
//! USB, the watchdog, the pins it holds low) and runs [`control::Control`] in a
//! loop that never waits, and the tests here run the same loop against a board in
//! memory, with synthesized Morse as the audio:
//!
//! - [`frontend`]: the ADC's samples at 32 kHz, filtered and decimated to the
//!   decoder's 8 kHz;
//! - [`control`]: the decoder ([`cw::Decoder`]), the monitor's 10 ms measurements
//!   ([`cw::level`]), the commands and the lines out, a pass at a time.
//!
//! It has no transmit path. The keyer box's key, PTT and tone pins (GP16, GP17,
//! GP18) are driven low by the firmware from boot and every pass
//! ([`control::Hardware::hold_outputs_low`]), never high, so that this image holds
//! a radio's key open even if it is flashed onto the keyer box by mistake.
//!
//! `no_std` and without an allocator, so that it builds for the RP2350 as it is.

#![cfg_attr(not(test), no_std)]

pub mod control;
pub mod frontend;

/// Protocol version, reported by `HELLO`.
pub const VERSION: u32 = 1;

/// The box's name, the last field of `HELLO`, and its USB product name.
pub const NAME: &str = "PICO2-DECODER";

/// Longest line either side sends, without its newline: the keyer box's, since
/// both frame their lines the same way (`keyer_core::frame`).
pub const MAX_LINE: usize = keyer_core::MAX_LINE;

/// The ADC's sample rate, in hertz.
pub const ADC_RATE: u32 = 32_000;

/// ADC samples per decoded sample.
pub const DECIMATE: usize = 4;

/// The decoder's sample rate, in hertz: every measurement and sample count the
/// box reports is at this rate.
pub const RATE: u32 = ADC_RATE / DECIMATE as u32;

/// Samples in one of the monitor's measurements: 10 ms, as hfnode's monitor cuts
/// its own audio.
pub const SLICE: usize = (RATE / 100) as usize;

/// Measurements per `SL` line: one line every 50 ms.
pub const SLICES_PER_LINE: usize = 5;

/// Samples between two `ST` lines: 50 ms.
pub const BLOCK: usize = (RATE / 20) as usize;

/// In hfnode mode, the box goes back to text mode when no valid line has arrived
/// for this long, as the keyer box ends a run: hfnode stopped or the cable is out.
pub const LINK_TIMEOUT_MS: u64 = 2_000;

/// In text mode, a new line once the decoder has heard nothing for this long after
/// the last character.
pub const NEWLINE_MS: u32 = 2_000;

/// The pitch the box decodes at, and the monitor measures at, until told
/// otherwise (`CONFIG`), in hertz: the IC-7300's default CW pitch.
pub const DEFAULT_PITCH_HZ: u32 = 600;

/// The decoder's filter bandwidth until told otherwise, in hertz: hfnode's
/// default (`[audio] bandwidth_hz`).
pub const DEFAULT_BANDWIDTH_HZ: u32 = 150;

/// Pitches `CONFIG` accepts, for the decoder and the monitor, in hertz: every CW
/// pitch a radio offers (the IC-7300's 300-900 Hz, the NR7Y firmware's up to
/// 800 Hz), and well inside the anti-alias filter's flat band ([`frontend`]).
pub const PITCH_HZ: core::ops::RangeInclusive<u32> = 300..=1500;

/// Decoder bandwidths `CONFIG` accepts, in hertz.
pub const BANDWIDTH_HZ: core::ops::RangeInclusive<u32> = 30..=500;

const _: () = assert!(RATE == 8_000 && SLICE == 80 && BLOCK == 400);
const _: () = assert!(BLOCK == SLICE * SLICES_PER_LINE);
