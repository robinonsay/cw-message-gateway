//! Morse code for the node: an audio CW decoder for what the IC-7300 hears, and a
//! synthesizer that produces realistic test signals (noise, hand-keyed timing jitter)
//! so the decoder can be exercised without a radio.

pub mod decoder;
pub mod morse;
pub mod synth;

pub use decoder::{events_to_text, DecodeEvent, Decoder, DecoderConfig};
pub use morse::{decode_pattern, encode_char, is_sendable};
pub use synth::{Keyer, Noise};
