//! Morse code for the node: an audio CW decoder for what the radio hears, and a
//! synthesizer that produces realistic test signals (noise, hand-keyed timing jitter)
//! so the decoder can be exercised without a radio.
//!
//! The decoder and the Morse table build without the standard library and without a
//! heap (`default-features = false`), so the same decoder runs on a microcontroller:
//! feed it with [`Decoder::push_with`] and take each [`Event`] as it is decoded. The
//! default `std` feature adds the `Vec`/`String` interface hfnode uses
//! (`Decoder::push`, `DecodeEvent`, `events_to_text`), words of any length, the
//! Morse timing helpers and the synthesizer.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

pub mod decoder;
mod math;
pub mod morse;
// Also in the crate's own tests without `std`, which build with the standard library.
#[cfg(any(feature = "std", test))]
pub mod synth;

#[cfg(feature = "std")]
pub use decoder::{events_to_text, DecodeEvent};
pub use decoder::{ConfigError, Decoder, DecoderConfig, Event};
pub use morse::{decode_pattern, encode_char, is_sendable};
#[cfg(feature = "std")]
pub use morse::{duration_ms, mark_units, units};
#[cfg(feature = "std")]
pub use synth::{Keyer, Noise};
