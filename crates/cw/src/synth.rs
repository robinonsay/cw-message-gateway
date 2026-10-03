//! CW test-signal synthesis: keyed tones with shaped edges, optional hand-keying
//! jitter, and additive white Gaussian noise.

use crate::morse::encode_char;

/// Generates the audio a receiver would produce for keyed CW.
#[derive(Debug, Clone)]
pub struct Keyer {
    pub sample_rate: u32,
    /// Tone frequency in Hz, i.e. the receiver's CW pitch.
    pub pitch_hz: f32,
    pub wpm: f32,
    /// Peak amplitude of the tone.
    pub amplitude: f32,
    /// Rise and fall time of each element, in milliseconds.
    pub edge_ms: f32,
    /// Standard deviation of each element and gap length, as a fraction of its ideal
    /// length. 0 is perfect machine keying; 0.15 is a sloppy straight key.
    pub jitter: f32,
    /// Extra multiplier on character and word gaps, as hand keyers often stretch them.
    pub gap_stretch: f32,
    pub seed: u64,
}

impl Keyer {
    pub fn new(sample_rate: u32, pitch_hz: f32, wpm: f32) -> Self {
        Self {
            sample_rate,
            pitch_hz,
            wpm,
            amplitude: 0.5,
            edge_ms: 5.0,
            jitter: 0.0,
            gap_stretch: 1.0,
            seed: 1,
        }
    }

    /// Dit length in milliseconds (PARIS standard: 1200 / wpm).
    pub fn dit_ms(&self) -> f32 {
        1200.0 / self.wpm
    }

    /// Key-down/key-up runs for `text` as (is_mark, milliseconds).
    pub fn timing(&self, text: &str) -> Vec<(bool, f32)> {
        let mut rng = Rng::new(self.seed);
        let dit = self.dit_ms();
        let mut jit = |len: f32| (len * (1.0 + self.jitter * rng.gaussian())).max(dit * 0.2);
        let mut runs: Vec<(bool, f32)> = Vec::new();
        let words: Vec<&str> = text.split_whitespace().collect();
        for (wi, word) in words.iter().enumerate() {
            let chars: Vec<&str> = word.chars().filter_map(encode_char).collect();
            for (ci, pattern) in chars.iter().enumerate() {
                for (ei, el) in pattern.chars().enumerate() {
                    let units = if el == '.' { 1.0 } else { 3.0 };
                    runs.push((true, jit(units * dit)));
                    if ei + 1 < pattern.len() {
                        runs.push((false, jit(dit)));
                    }
                }
                if ci + 1 < chars.len() {
                    runs.push((false, jit(3.0 * dit * self.gap_stretch)));
                }
            }
            if wi + 1 < words.len() {
                runs.push((false, jit(7.0 * dit * self.gap_stretch)));
            }
        }
        runs
    }

    /// Audio samples for `text`, with `lead_ms` of silence before and after.
    pub fn render(&self, text: &str, lead_ms: f32) -> Vec<f32> {
        let sr = self.sample_rate as f32;
        let ms = |m: f32| (m * sr / 1000.0).round() as usize;
        let edge = ms(self.edge_ms).max(1);
        let mut out = vec![0.0; ms(lead_ms)];
        let w = 2.0 * std::f32::consts::PI * self.pitch_hz / sr;
        for (mark, len) in self.timing(text) {
            let n = ms(len);
            if !mark {
                out.extend(std::iter::repeat(0.0).take(n));
                continue;
            }
            for i in 0..n {
                // Raised-cosine edges keep the keying clean, as a real transmitter does.
                let shape = if i < edge {
                    0.5 - 0.5 * (std::f32::consts::PI * i as f32 / edge as f32).cos()
                } else if n - i <= edge {
                    0.5 - 0.5 * (std::f32::consts::PI * (n - i) as f32 / edge as f32).cos()
                } else {
                    1.0
                };
                let t = out.len() as f32;
                out.push(self.amplitude * shape * (w * t).sin());
            }
        }
        out.extend(std::iter::repeat(0.0).take(ms(lead_ms)));
        out
    }
}

/// Additive white Gaussian noise.
#[derive(Debug, Clone)]
pub struct Noise {
    rng: Rng,
}

impl Noise {
    pub fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed) }
    }

    /// Add noise with standard deviation `sigma` to every sample.
    pub fn add(&mut self, samples: &mut [f32], sigma: f32) {
        for s in samples {
            *s += sigma * self.rng.gaussian();
        }
    }

    /// Noise sigma that gives `snr_db` for a tone of peak `amplitude`, with the noise
    /// power measured in `bandwidth_hz` (2500 Hz is the usual SSB-bandwidth convention).
    pub fn sigma_for_snr(amplitude: f32, snr_db: f32, sample_rate: u32, bandwidth_hz: f32) -> f32 {
        let tone_power = amplitude * amplitude / 2.0;
        let noise_in_band = tone_power / 10f32.powf(snr_db / 10.0);
        // White noise spreads over sample_rate / 2; scale to the whole band.
        (noise_in_band * (sample_rate as f32 / 2.0) / bandwidth_hz).sqrt()
    }
}

/// Small deterministic PRNG (xorshift64*) so tests are reproducible without extra crates.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((v >> 40) as f32 + 0.5) / (1u64 << 24) as f32
    }

    /// Standard normal sample (Box–Muller).
    fn gaussian(&mut self) -> f32 {
        let u1 = self.next_f32();
        let u2 = self.next_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paris_is_fifty_units() {
        let k = Keyer::new(8000, 600.0, 20.0);
        // PARIS plus the trailing word gap is 50 units; without the final gap, 43.
        let total: f32 = k.timing("PARIS").iter().map(|(_, l)| l).sum();
        assert!((total - 43.0 * k.dit_ms()).abs() < 0.01, "{total}");
    }

    #[test]
    fn render_length_matches_timing() {
        let k = Keyer::new(8000, 600.0, 20.0);
        let audio = k.render("E E", 100.0);
        // dit + 7 dit gap + dit = 9 dits = 540 ms, plus 2 × 100 ms lead.
        let expected = (740.0 * 8.0) as usize;
        assert!((audio.len() as i64 - expected as i64).abs() <= 3, "{}", audio.len());
    }
}
