//! From the ADC to the decoder: 12-bit samples at [`ADC_RATE`], centred on
//! mid-scale by the box's bias resistors, to samples at [`RATE`] with full scale
//! 1.0, as hfnode's sound-card audio is.
//!
//! The box has no analog anti-alias filter (docs/decoder.md, "Wiring"): the ADC
//! samples four times faster than the decoder needs, and a 63-tap low-pass filter
//! (a windowed sinc, its half-amplitude point at 3 kHz) keeps everything the
//! decoder could mistake for a tone out of the decimated audio. It passes the band
//! every CW pitch is in ([`crate::PITCH_HZ`]) flat to within 0.1 dB, and attenuates
//! everything from 5 kHz up, all that decimation would fold back onto the pitches,
//! by at least 70 dB.
//! Then a 5 Hz high-pass takes off what is left of the bias, so the monitor's total
//! power is the audio's alone.
//!
//! A conversion the ADC flags as failed ([`ADC_ERROR`]) is replaced by the last
//! good one, keeping its place in time.

use crate::{ADC_RATE, DECIMATE, RATE};
use core::f32::consts::PI;

/// Bit 15 of an ADC FIFO entry: the conversion failed, and its value is
/// meaningless (RP2350 datasheet §12.4.3.4).
pub const ADC_ERROR: u16 = 1 << 15;

/// Bits 11:0 of an ADC FIFO entry: the result, 0 to 4095.
pub const ADC_VALUE: u16 = 0x0fff;

/// The ADC reading at the bias point, 1.65 V of 3.3 V.
const MID_SCALE: f32 = 2048.0;

/// Taps in the anti-alias filter. Odd, so that the filter is symmetric about one
/// tap and delays every frequency by the same 31 samples (about 1 ms).
pub const TAPS: usize = 63;

/// The filter's half-amplitude frequency, in hertz.
const CUTOFF_HZ: f32 = 3_000.0;

/// The high-pass's corner, in hertz.
const DC_HZ: f32 = 5.0;

/// The ADC's samples into the decoder's.
#[derive(Clone)]
pub struct Frontend {
    taps: [f32; TAPS],
    /// The last [`TAPS`] inputs, twice over, so that they are always in one slice:
    /// the newest at `pos + TAPS - 1`.
    history: [f32; 2 * TAPS],
    pos: usize,
    started: bool,
    /// Inputs since the last output.
    phase: usize,
    /// The last good ADC value.
    last: u16,
    dc: Option<f32>,
    dc_k: f32,
    /// Conversions flagged as failed, since the start.
    errors: u32,
}

impl Frontend {
    pub fn new() -> Self {
        Self {
            taps: taps(),
            history: [0.0; 2 * TAPS],
            pos: 0,
            started: false,
            phase: 0,
            last: MID_SCALE as u16,
            dc: None,
            dc_k: 1.0 - libm::expf(-2.0 * PI * DC_HZ / RATE as f32),
            errors: 0,
        }
    }

    /// One ADC FIFO entry in; every [`DECIMATE`]th call, a sample out.
    pub fn push(&mut self, raw: u16) -> Option<f32> {
        let value = if raw & ADC_ERROR != 0 {
            self.errors = self.errors.saturating_add(1);
            self.last
        } else {
            raw & ADC_VALUE
        };
        self.last = value;
        self.push_scaled((f32::from(value) - MID_SCALE) / MID_SCALE)
    }

    /// One sample in, full scale 1.0, at [`ADC_RATE`].
    fn push_scaled(&mut self, x: f32) -> Option<f32> {
        if !self.started {
            // As if the first sample had always been there, so that the filter does
            // not ramp up from silence to the bias and ring.
            self.started = true;
            self.history = [x; 2 * TAPS];
        }
        self.history[self.pos] = x;
        self.history[self.pos + TAPS] = x;
        self.pos = (self.pos + 1) % TAPS;
        self.phase += 1;
        if self.phase < DECIMATE {
            return None;
        }
        self.phase = 0;
        let window = &self.history[self.pos..self.pos + TAPS];
        let y: f32 = window.iter().zip(&self.taps).map(|(x, h)| x * h).sum();
        let dc = *self.dc.get_or_insert(y);
        let dc = dc + self.dc_k * (y - dc);
        self.dc = Some(dc);
        Some(y - dc)
    }

    /// Conversions the ADC flagged as failed since the start.
    pub fn errors(&self) -> u32 {
        self.errors
    }
}

impl Default for Frontend {
    fn default() -> Self {
        Self::new()
    }
}

/// The anti-alias filter: a sinc cut off at [`CUTOFF_HZ`], under a Blackman
/// window, scaled to a gain of exactly 1 at 0 Hz.
fn taps() -> [f32; TAPS] {
    let mut h = [0.0f32; TAPS];
    let mid = (TAPS - 1) as f32 / 2.0;
    let fc = 2.0 * CUTOFF_HZ / ADC_RATE as f32;
    for (n, tap) in h.iter_mut().enumerate() {
        let t = n as f32 - mid;
        let sinc = if t == 0.0 {
            fc
        } else {
            libm::sinf(PI * fc * t) / (PI * t)
        };
        let a = 2.0 * PI * n as f32 / (TAPS - 1) as f32;
        let window = 0.42 - 0.5 * libm::cosf(a) + 0.08 * libm::cosf(2.0 * a);
        *tap = sinc * window;
    }
    let sum: f32 = h.iter().sum();
    for tap in &mut h {
        *tap /= sum;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADC's readings of a sine of `amplitude` (full scale 1.0) at `hz`.
    fn adc_sine(amplitude: f32, hz: f32, n: usize) -> Vec<u16> {
        (0..n)
            .map(|i| {
                let x = amplitude * (2.0 * PI * hz * i as f32 / ADC_RATE as f32).sin();
                (MID_SCALE + x * MID_SCALE).round().clamp(0.0, 4095.0) as u16
            })
            .collect()
    }

    /// The filter's gain at `hz`, in dB, once it has settled: on samples as they
    /// are, since the ADC's 12 bits would add their own error at -70 dB or so.
    fn gain_db(hz: f32) -> f32 {
        let mut f = Frontend::new();
        let out: Vec<f32> = (0..3 * ADC_RATE as usize)
            // In f64: an f32 phase this large has jitter of its own at -50 dB.
            .map(|i| {
                let t = i as f64 / f64::from(ADC_RATE);
                (0.5 * (2.0 * std::f64::consts::PI * f64::from(hz) * t).sin()) as f32
            })
            .filter_map(|x| f.push_scaled(x))
            .collect();
        let tail = &out[out.len() - RATE as usize..];
        let ms = tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32;
        10.0 * (ms / 0.125).log10()
    }

    #[test]
    fn every_cw_pitch_passes_flat() {
        for hz in [300.0, 600.0, 900.0, 1500.0] {
            let g = gain_db(hz);
            assert!(g.abs() < 0.1, "{hz} Hz: {g:.3} dB");
        }
    }

    #[test]
    fn what_would_fold_onto_a_pitch_is_cut() {
        // After decimation to 8 kHz, 7.4, 8.6, 15.4 kHz... land on 600 Hz.
        for hz in [5_000.0, 6_500.0, 7_400.0, 8_600.0, 9_500.0, 15_400.0] {
            let g = gain_db(hz);
            assert!(g < -70.0, "{hz} Hz: {g:.1} dB");
        }
    }

    #[test]
    fn one_output_per_four_inputs_and_dc_is_removed() {
        let mut f = Frontend::new();
        let mut out = Vec::new();
        for i in 0..4 * RATE as usize {
            // Off mid-scale by 300 counts, as a bias resistor's tolerance might.
            if let Some(y) = f.push(2348) {
                out.push(y);
            }
            assert_eq!(out.len(), (i + 1) / DECIMATE);
        }
        // The first output starts the high-pass at the bias: no step to ring on.
        assert!(out[0].abs() < 1e-6, "{}", out[0]);
        assert!(out.iter().all(|y| y.abs() < 1e-3), "{:?}", &out[..8]);
    }

    #[test]
    fn a_failed_conversion_keeps_the_last_good_value() {
        let mut a = Frontend::new();
        let mut b = Frontend::new();
        let good = adc_sine(0.5, 600.0, 4000);
        let mut flagged = good.clone();
        for i in (100..4000).step_by(37) {
            flagged[i] = ADC_ERROR | 0x0abc;
        }
        let mut held = good.clone();
        for i in (100..4000).step_by(37) {
            held[i] = held[i - 1];
        }
        let x: Vec<f32> = flagged.iter().filter_map(|&r| a.push(r)).collect();
        let y: Vec<f32> = held.iter().filter_map(|&r| b.push(r)).collect();
        assert_eq!(x, y);
        assert_eq!(a.errors(), (100..4000).step_by(37).count() as u32);
        assert_eq!(b.errors(), 0);
    }

    #[test]
    fn the_filter_is_symmetric_with_unity_gain() {
        let h = taps();
        for n in 0..TAPS {
            assert!((h[n] - h[TAPS - 1 - n]).abs() < 1e-7);
        }
        assert!((h.iter().sum::<f32>() - 1.0).abs() < 1e-6);
    }
}
