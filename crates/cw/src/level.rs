//! How loud a stretch of audio is at one pitch and in all, the measurement hfnode's
//! sidetone monitor makes on every 10 ms of receive audio. One copy, so that the
//! Pico 2 decoder box, which sends these measurements to hfnode, and hfnode's own
//! monitor compute them the same way.

use crate::math;
use core::f32::consts::PI;

/// The Goertzel coefficient for `pitch_hz` at `sample_rate`: `2 cos(2 pi f / fs)`.
pub fn goertzel_coeff(pitch_hz: f32, sample_rate: u32) -> f32 {
    let w = 2.0 * PI * pitch_hz / sample_rate as f32;
    2.0 * math::cos(w)
}

/// Power at one pitch and in all of a stretch of audio, as mean squares (full scale
/// is 1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// Power at the pitch, as the mean square of a sine of that amplitude: a sine of
    /// amplitude A at the pitch reads A^2 / 2, the same as its `total`.
    pub tone: f32,
    /// Mean square of the samples.
    pub total: f32,
}

impl Level {
    /// The share of the power at the pitch, 0 to 1.
    pub fn purity(&self) -> f32 {
        if self.total > 0.0 {
            (self.tone / self.total).min(1.0)
        } else {
            0.0
        }
    }
}

/// Measure `x` at the pitch whose [`goertzel_coeff`] is `coeff`.
pub fn measure(x: &[f32], coeff: f32) -> Level {
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    let mut sum_sq = 0.0f32;
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
        sum_sq += v * v;
    }
    let n = x.len() as f32;
    let mag_sq = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    // A sine of amplitude A at the pitch gives |X| = A n / 2: report A^2 / 2, its
    // mean square, so that the two levels compare.
    Level {
        tone: 2.0 * mag_sq / (n * n),
        total: sum_sq / n,
    }
}

/// `p` in decibels, with a floor far below anything audible so that silence is a
/// number.
pub fn db(p: f32) -> f32 {
    10.0 * math::log10(p + 1e-12)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, hz: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amplitude * (2.0 * PI * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn a_tone_at_the_pitch_reads_as_its_mean_square() {
        let coeff = goertzel_coeff(600.0, 8000);
        let l = measure(&sine(0.5, 600.0, 8000, 80), coeff);
        // A^2 / 2 = 0.125 = -9.03 dB, both ways.
        assert!((db(l.tone) - db(0.125)).abs() < 0.1, "{l:?}");
        assert!((db(l.total) - db(0.125)).abs() < 0.1, "{l:?}");
        assert!(l.purity() > 0.97, "{l:?}");
    }

    #[test]
    fn a_tone_off_the_pitch_is_not_counted_at_it() {
        let coeff = goertzel_coeff(600.0, 8000);
        let l = measure(&sine(0.5, 1200.0, 8000, 80), coeff);
        assert!(db(l.tone) < db(l.total) - 20.0, "{l:?}");
        assert!(l.purity() < 0.01, "{l:?}");
    }

    #[test]
    fn silence_has_a_level() {
        let l = measure(&[0.0; 80], goertzel_coeff(600.0, 8000));
        assert_eq!((l.tone, l.total, l.purity()), (0.0, 0.0, 0.0));
        assert!((db(0.0) + 120.0).abs() < 1e-3);
    }
}
