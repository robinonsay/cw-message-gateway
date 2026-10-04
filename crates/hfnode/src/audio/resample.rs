//! Sample-rate conversion for sound cards that cannot be asked for the decoder's rate.
//!
//! On Linux, ALSA's `plughw` devices convert to whatever `arecord` asks for. Core Audio
//! (macOS) and WASAPI (Windows) are read at the device's own rate and channel count
//! instead, typically 44.1 or 48 kHz stereo, so that capturing never changes the sound
//! card's settings for other programs; this module brings that down to the mono rate
//! the decoder runs at.
//!
//! The converter is a windowed-sinc interpolator: each output sample is the input
//! low-pass filtered below the lower Nyquist frequency of the two rates (so a strong
//! signal above it cannot alias onto the CW pitch) and evaluated at that output
//! sample's exact time. The filter is a Kaiser-windowed sinc held in a table and
//! interpolated between table points.

use std::f64::consts::PI;

/// Filter half-width, in samples of the lower of the two rates.
const ZERO_CROSSINGS: f64 = 16.0;
/// Pass band edge as a fraction of the lower Nyquist frequency.
const PASS: f64 = 0.9;
/// Kaiser window shape: about 80 dB of stop-band rejection.
const KAISER_BETA: f64 = 8.0;
/// Table points per input sample.
const PHASES: usize = 64;

/// Average interleaved frames of `channels` samples to mono, appending to `out`.
pub fn mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    let ch = channels.max(1);
    out.extend(
        interleaved
            .chunks_exact(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32),
    );
}

/// Streaming sample-rate converter, mono.
#[derive(Debug, Clone)]
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Filter half-width, in input samples.
    half: f64,
    /// The filter at `PHASES` points per input sample, from `-half` to `half`.
    table: Vec<f32>,
    /// Input not yet entirely used.
    buf: Vec<f32>,
    /// Where in `buf` the next output sample falls.
    pos: f64,
    passthrough: bool,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        let (from, to) = (f64::from(from_hz.max(1)), f64::from(to_hz.max(1)));
        let step = from / to;
        // Cut-off in cycles per input sample, below both Nyquist frequencies.
        let cutoff = 0.5 * PASS * (to / from).min(1.0);
        let half = (ZERO_CROSSINGS * step.max(1.0)).ceil();
        let n = (2.0 * half * PHASES as f64) as usize + 1;
        let i0b = bessel_i0(KAISER_BETA);
        let table = (0..n)
            .map(|i| {
                let t = i as f64 / PHASES as f64 - half;
                let x = 2.0 * cutoff * t;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                // Kaiser window, shifted to reach exactly zero at the ends so that a
                // tap falling just inside or just outside the span adds nothing.
                let r = t / half;
                let w =
                    (bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) - 1.0) / (i0b - 1.0);
                (2.0 * cutoff * sinc * w) as f32
            })
            .collect();
        Self {
            step,
            half,
            table,
            // History before the first sample is silence, and the first output
            // sample falls on the first input sample.
            buf: vec![0.0; half as usize],
            pos: half,
            passthrough: from_hz == to_hz,
        }
    }

    /// Convert `input`, appending the output samples it completes to `out`. Output
    /// lags input by the filter's half-width (under 1 ms at 44.1 kHz and up).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let half = self.half;
        while self.pos + half < self.buf.len() as f64 {
            let first = (self.pos - half).ceil().max(0.0) as usize;
            let last = ((self.pos + half).floor() as usize).min(self.buf.len() - 1);
            let mut acc = 0.0f64;
            let mut gain = 0.0f64;
            for (k, &x) in self.buf[first..=last].iter().enumerate() {
                let h = f64::from(self.kernel(self.pos - (first + k) as f64));
                acc += f64::from(x) * h;
                gain += h;
            }
            // Normalising by the taps' sum keeps a steady level the same at every
            // fractional position.
            out.push(if gain.abs() > 1e-9 {
                (acc / gain) as f32
            } else {
                0.0
            });
            self.pos += self.step;
        }
        // Keep only what later output samples still need.
        let keep_from = ((self.pos - half).ceil().max(0.0) as usize).min(self.buf.len());
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }

    /// The filter at `t` input samples from its centre.
    fn kernel(&self, t: f64) -> f32 {
        let x = (t + self.half) * PHASES as f64;
        if x < 0.0 {
            return 0.0;
        }
        let i = x.floor() as usize;
        let frac = (x - i as f64) as f32;
        match (self.table.get(i), self.table.get(i + 1)) {
            (Some(&a), Some(&b)) => a + (b - a) * frac,
            (Some(&a), None) => a,
            _ => 0.0,
        }
    }
}

/// Modified Bessel function of the first kind, order zero (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    /// Frequency from the spacing of upward zero crossings.
    fn frequency(s: &[f32], rate: u32) -> f32 {
        let ups: Vec<usize> = s
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
            .map(|(i, _)| i)
            .collect();
        let cycles = (ups.len() - 1) as f32;
        cycles * rate as f32 / (ups[ups.len() - 1] - ups[0]) as f32
    }

    fn convert(input: &[f32], from: u32, to: u32) -> Vec<f32> {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        r.process(input, &mut out);
        out
    }

    #[test]
    fn keeps_a_cw_tone_and_its_pitch() {
        for from in [48_000, 44_100, 32_000, 16_000, 11_025] {
            let out = convert(&tone(600.0, from, 2.0, 0.5), from, 8000);
            let want = 2.0 * 8000.0;
            assert!(
                (out.len() as f32 - want).abs() < 40.0,
                "{from} Hz: {} samples",
                out.len()
            );
            // Past the start-up transient.
            let steady = &out[400..out.len() - 400];
            let f = frequency(steady, 8000);
            assert!((f - 600.0).abs() < 0.5, "{from} Hz: {f} Hz");
            let level = rms(steady) / (0.5 / 2f32.sqrt());
            assert!((level - 1.0).abs() < 0.01, "{from} Hz: level {level}");
        }
    }

    #[test]
    fn rejects_what_would_alias_onto_the_pitch() {
        // 7400 Hz at 48 kHz lands on 600 Hz at 8 kHz unless filtered out first.
        for (hz, from) in [(7400.0, 48_000), (8600.0, 48_000), (7400.0, 44_100)] {
            let out = convert(&tone(hz, from, 1.0, 0.5), from, 8000);
            let leak = rms(&out[400..out.len() - 400]) / (0.5 / 2f32.sqrt());
            assert!(leak < 1e-3, "{hz} Hz from {from} Hz leaks {leak}");
        }
    }

    #[test]
    fn same_rate_is_untouched() {
        let s = tone(600.0, 8000, 0.1, 0.3);
        assert_eq!(convert(&s, 8000, 8000), s);
    }

    #[test]
    fn upsampling_keeps_the_tone() {
        let out = convert(&tone(600.0, 8000, 1.0, 0.5), 8000, 16_000);
        let steady = &out[800..out.len() - 800];
        assert!((frequency(steady, 16_000) - 600.0).abs() < 0.5);
        assert!((rms(steady) / (0.5 / 2f32.sqrt()) - 1.0).abs() < 0.01);
    }

    #[test]
    fn pieces_give_the_same_result_as_one_block() {
        let s = tone(600.0, 44_100, 1.0, 0.5);
        let whole = convert(&s, 44_100, 8000);
        let mut r = Resampler::new(44_100, 8000);
        let mut pieces = Vec::new();
        // Uneven callback sizes, as sound cards deliver them.
        let mut at = 0;
        for n in [1, 7, 512, 441, 3, 1024, 4410].iter().cycle() {
            if at >= s.len() {
                break;
            }
            let end = (at + n).min(s.len());
            r.process(&s[at..end], &mut pieces);
            at = end;
        }
        assert_eq!(pieces.len(), whole.len());
        let worst = pieces
            .iter()
            .zip(&whole)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "differs by {worst}");
    }

    #[test]
    fn stereo_to_mono_averages_frames() {
        let mut out = Vec::new();
        mono(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0]);
        out.clear();
        mono(&[0.25, 0.5], 1, &mut out);
        assert_eq!(out, [0.25, 0.5]);
    }

    #[test]
    fn decodes_cw_captured_at_48_khz() {
        let k = cw::Keyer::new(48_000, 600.0, 18.0);
        let mut s = k.render("CQ DE N0CALL K", 500.0);
        cw::Noise::new(7).add(
            &mut s,
            cw::Noise::sigma_for_snr(k.amplitude, 10.0, 48_000, 2500.0),
        );
        let mut out = Vec::new();
        let mut stereo = Vec::with_capacity(s.len() * 2);
        for x in &s {
            stereo.extend([*x, *x]);
        }
        let mut m = Vec::new();
        mono(&stereo, 2, &mut m);
        Resampler::new(48_000, 8000).process(&m, &mut out);
        let mut d = cw::Decoder::new(cw::DecoderConfig::new(8000, 600.0));
        let mut ev = d.push(&out);
        ev.extend(d.flush());
        assert_eq!(cw::events_to_text(&ev).trim(), "CQ DE N0CALL K");
    }
}
