//! Streaming CW decoder for the receiver's audio.
//!
//! Pipeline, run on every sample and evaluated once per millisecond:
//!
//! 1. Mix the audio down by the CW pitch to complex baseband and low-pass it. This is
//!    a narrow filter centred on the expected tone, which is where the weak-signal
//!    gain comes from: noise outside the passband never reaches the detector.
//! 2. Track the noise floor while the key is up and the signal peak while it is down,
//!    and decide key up/down with hysteresis between them. A squelch keeps pure noise
//!    from being decoded.
//! 3. Debounce: a flip shorter than a fraction of a dit is absorbed into the run it
//!    interrupted, which removes noise spikes and fades.
//! 4. Buffer each word, then classify its marks into dits and dahs by 2-means
//!    clustering of recent mark lengths (including the word's own), so speed is
//!    learned from the sender rather than configured, and hand-keyed timing that
//!    drifts during a transmission is followed. One- and two-mark "words" (noise
//!    bursts) do not train the speed estimate.
//! 5. Classify gaps against the learned dit length into element, character and word
//!    gaps, and look up each finished pattern in the Morse table.

use crate::morse::decode_pattern;
use std::collections::VecDeque;
use std::f32::consts::PI;

#[derive(Debug, Clone)]
pub struct DecoderConfig {
    pub sample_rate: u32,
    /// Expected tone frequency in Hz (the receiver's CW pitch setting).
    pub pitch_hz: f32,
    /// Detection filter bandwidth in Hz. Narrower is more sensitive but needs the
    /// sender to be closer to the pitch and limits the highest decodable speed.
    pub bandwidth_hz: f32,
    /// Speed assumed until enough marks have been heard to measure it.
    pub initial_wpm: f32,
    pub min_wpm: f32,
    pub max_wpm: f32,
    /// How many noise deviations above the floor a tone must rise to count.
    pub squelch_sigmas: f32,
    /// Absolute level (full scale = 1.0) below which nothing counts as a tone, so
    /// digital silence or filter leakage is never decoded.
    pub min_level: f32,
}

impl DecoderConfig {
    pub fn new(sample_rate: u32, pitch_hz: f32) -> Self {
        Self {
            sample_rate,
            pitch_hz,
            bandwidth_hz: 150.0,
            initial_wpm: 15.0,
            min_wpm: 5.0,
            max_wpm: 35.0,
            squelch_sigmas: 4.0,
            min_level: 1e-3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeEvent {
    Char(char),
    /// A dot/dash pattern that is not in the Morse table.
    Unknown(String),
    WordGap,
}

#[derive(Debug)]
pub struct Decoder {
    cfg: DecoderConfig,
    // Mixer and filter.
    phase: f32,
    phase_step: f32,
    alpha: f32,
    i_lp: [f32; 3],
    q_lp: [f32; 3],
    samples_per_tick: u32,
    sample_in_tick: u32,
    // Level tracking.
    env: f32,
    floor: f32,
    dev: f32,
    peak: f32,
    ticks_seen: u32,
    raw_on: bool,
    // Debounced runs.
    on: bool,
    run_ticks: u32,
    candidate_ticks: u32,
    // Timing.
    dit_ms: f32,
    char_gap_ms: f32,
    marks: VecDeque<f32>,
    /// Debounced (is_mark, ms) runs of the word being received, decoded when it ends
    /// so that the speed estimate has seen every mark of the word first.
    word: Vec<(bool, f32)>,
    /// Cached (marks in word, dit estimate including them).
    working: (usize, f32),
    idle_ms: u64,
}

const MARK_HISTORY: usize = 24;
/// Ticks of audio before the floor estimate is trusted.
const SETTLE_TICKS: u32 = 100;

impl Decoder {
    pub fn new(cfg: DecoderConfig) -> Self {
        let sr = cfg.sample_rate as f32;
        let fc = cfg.bandwidth_hz / 2.0;
        let dit_ms = 1200.0 / cfg.initial_wpm;
        Self {
            phase: 0.0,
            phase_step: 2.0 * PI * cfg.pitch_hz / sr,
            alpha: 1.0 - (-2.0 * PI * fc / sr).exp(),
            i_lp: [0.0; 3],
            q_lp: [0.0; 3],
            samples_per_tick: (cfg.sample_rate / 1000).max(1),
            sample_in_tick: 0,
            env: 0.0,
            floor: 0.0,
            dev: 0.0,
            peak: 0.0,
            ticks_seen: 0,
            raw_on: false,
            on: false,
            run_ticks: 0,
            candidate_ticks: 0,
            dit_ms,
            char_gap_ms: 3.0 * dit_ms,
            marks: VecDeque::with_capacity(MARK_HISTORY),
            word: Vec::new(),
            working: (0, dit_ms),
            idle_ms: 0,
            cfg,
        }
    }

    /// Current speed estimate in words per minute.
    pub fn wpm(&self) -> f32 {
        1200.0 / self.dit_ms
    }

    /// Milliseconds since the key was last down (0 while a tone is present).
    pub fn idle_ms(&self) -> u64 {
        self.idle_ms
    }

    /// (noise floor, noise deviation, signal peak) of the detector envelope, for logging.
    pub fn levels(&self) -> (f32, f32, f32) {
        (self.floor, self.dev, self.peak)
    }

    /// Whether a tone is currently detected.
    pub fn key_down(&self) -> bool {
        self.on
    }

    /// Feed audio; returns whatever was decoded.
    pub fn push(&mut self, samples: &[f32]) -> Vec<DecodeEvent> {
        let mut events = Vec::new();
        for &x in samples {
            self.phase += self.phase_step;
            if self.phase > 2.0 * PI {
                self.phase -= 2.0 * PI;
            }
            let (s, c) = self.phase.sin_cos();
            let mut i = x * c;
            let mut q = -x * s;
            for k in 0..3 {
                self.i_lp[k] += self.alpha * (i - self.i_lp[k]);
                self.q_lp[k] += self.alpha * (q - self.q_lp[k]);
                i = self.i_lp[k];
                q = self.q_lp[k];
            }
            self.sample_in_tick += 1;
            if self.sample_in_tick >= self.samples_per_tick {
                self.sample_in_tick = 0;
                let mag = 2.0 * (i * i + q * q).sqrt();
                self.tick(mag, &mut events);
            }
        }
        events
    }

    /// Decode whatever is buffered, as if a word gap had just been heard.
    pub fn flush(&mut self) -> Vec<DecodeEvent> {
        let mut events = Vec::new();
        self.finish_word(&mut events);
        events
    }

    fn tick(&mut self, mag: f32, events: &mut Vec<DecodeEvent>) {
        // Post-detection smoothing over a fraction of a dit tames the noise
        // fluctuation of the envelope without blurring the keying.
        let tau = (0.12 * self.dit_ms).clamp(2.0, 12.0);
        self.env += (mag - self.env) / tau;

        self.ticks_seen = self.ticks_seen.saturating_add(1);
        if self.ticks_seen <= SETTLE_TICKS {
            // Filter settling: just learn the floor.
            self.floor += 0.05 * (self.env - self.floor);
            self.dev += 0.05 * ((self.env - self.floor).abs() - self.dev);
            self.peak = self.floor;
            return;
        }

        // Key decision with hysteresis between the noise floor and the signal peak.
        let noise_gate = (self.floor + self.cfg.squelch_sigmas * self.dev).max(self.cfg.min_level);
        let span = (self.peak - self.floor).max(0.0);
        let env = self.env;
        self.raw_on = if self.raw_on {
            env > (self.floor + 0.35 * span).max(self.floor + 0.6 * (noise_gate - self.floor))
        } else {
            env > (self.floor + 0.5 * span).max(noise_gate)
        };

        // Level tracking: floor and deviation only with key up, peak with key down.
        if self.raw_on {
            let a = if env > self.peak { 0.2 } else { 0.01 };
            self.peak += a * (env - self.peak);
        } else if !self.on && self.run_ticks as f32 > 0.6 * self.dit_ms {
            // Only well into a gap, so the decaying tail of the last element is not
            // mistaken for noise. The floor falls quickly and rises slowly, and
            // deviation samples are clipped, so stray tones barely move either.
            let a = if env < self.floor { 0.02 } else { 0.002 };
            self.floor += a * (env - self.floor);
            let d = (env - self.floor).abs().min(4.0 * self.dev + 1e-6);
            self.dev += 0.005 * (d - self.dev);
            // Let the peak relax towards the floor over a few seconds of silence so
            // a weaker station after a strong one is still heard.
            self.peak += 0.0005 * (self.floor - self.peak);
        }

        // Debounce.
        let min_dit = 1200.0 / self.cfg.max_wpm;
        let glitch = (0.3 * self.dit_ms).clamp(0.4 * min_dit, 40.0) as u32;
        if self.raw_on == self.on {
            self.run_ticks += 1 + self.candidate_ticks;
            self.candidate_ticks = 0;
        } else {
            self.candidate_ticks += 1;
            if self.candidate_ticks >= glitch {
                let (was_on, len) = (self.on, self.run_ticks as f32);
                self.on = self.raw_on;
                self.run_ticks = self.candidate_ticks;
                self.candidate_ticks = 0;
                if was_on {
                    self.word.push((true, len));
                } else if !self.word.is_empty() {
                    if len >= 2.0 * self.dit_ms {
                        self.char_gap_ms += 0.2 * (len - self.char_gap_ms);
                    }
                    self.word.push((false, len));
                }
            }
        }

        if self.on {
            self.idle_ms = 0;
        } else {
            self.idle_ms = self.run_ticks as u64;
            if !self.word.is_empty() && self.run_ticks as f32 >= self.word_gap_threshold() {
                self.finish_word(events);
                events.push(DecodeEvent::WordGap);
            }
        }
    }

    fn word_gap_threshold(&mut self) -> f32 {
        // Midway between a 3-unit and 7-unit gap, pushed out when the sender is
        // known to stretch character gaps, as many hand keyers do.
        let dit = self.working_dit();
        (1.6 * self.char_gap_ms).clamp(5.0 * dit, 6.5 * dit)
    }

    /// Learn speed from a finished word. Words of one or two marks are left out:
    /// isolated noise bursts decode as E or T, and letting them in would drag the
    /// speed estimate towards whatever length the noise happens to have.
    fn learn_speed(&mut self, runs: &[(bool, f32)]) {
        let marks: Vec<f32> = runs.iter().filter(|r| r.0).map(|r| r.1).collect();
        if marks.len() < 3 {
            return;
        }
        // A burst of noise blips is shorter than any real dit.
        let mut sorted = marks.clone();
        sorted.sort_by(f32::total_cmp);
        if sorted[sorted.len() / 2] < 0.6 * 1200.0 / self.cfg.max_wpm {
            return;
        }
        for ms in marks {
            if self.marks.len() == MARK_HISTORY {
                self.marks.pop_front();
            }
            self.marks.push_back(ms);
            self.update_speed();
        }
    }

    fn update_speed(&mut self) {
        let marks: Vec<f32> = self.marks.iter().copied().collect();
        self.dit_ms = self.estimate_dit(&marks, self.dit_ms);
    }

    /// Estimate the dit length from mark lengths with 1-D 2-means on log length.
    fn estimate_dit(&self, marks: &[f32], current: f32) -> f32 {
        let logs: Vec<f32> = marks.iter().map(|m| m.max(1.0).ln()).collect();
        let (lo, hi) = logs
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        let min_dit = 1200.0 / self.cfg.max_wpm;
        let max_dit = 1200.0 / self.cfg.min_wpm;
        if logs.len() >= 3 && hi - lo > 1.8f32.ln() {
            // Two populations (dits and dahs) present.
            let mut split = (lo + hi) / 2.0;
            let (mut short, mut long) = (lo, hi);
            for _ in 0..8 {
                let (s, l): (Vec<f32>, Vec<f32>) = logs.iter().partition(|&&x| x < split);
                if s.is_empty() || l.is_empty() {
                    break;
                }
                short = s.iter().sum::<f32>() / s.len() as f32;
                long = l.iter().sum::<f32>() / l.len() as f32;
                split = (short + long) / 2.0;
            }
            ((short.exp() + long.exp() / 3.0) / 2.0).clamp(min_dit, max_dit)
        } else if let Some(&last) = marks.last() {
            // One population so far: decide whether it is dits or dahs relative to
            // the current estimate and nudge towards it.
            let est = if last < 2.0 * current {
                last
            } else {
                last / 3.0
            };
            (0.7 * current + 0.3 * est).clamp(min_dit, max_dit)
        } else {
            current
        }
    }

    /// Dit length to use for gaps in the word being received: once it has three
    /// marks, they count alongside the history, so a sender much slower or faster
    /// than the last one does not have their first word cut into pieces.
    fn working_dit(&mut self) -> f32 {
        let n = self.word.iter().filter(|r| r.0).count();
        if n < 3 {
            return self.dit_ms;
        }
        if self.working.0 != n {
            let mut marks: Vec<f32> = self.marks.iter().copied().collect();
            marks.extend(self.word.iter().filter(|r| r.0).map(|r| r.1));
            self.working = (n, self.estimate_dit(&marks, self.dit_ms));
        }
        self.working.1
    }

    fn finish_word(&mut self, events: &mut Vec<DecodeEvent>) {
        let runs = std::mem::take(&mut self.word);
        self.working = (0, self.dit_ms);
        self.learn_speed(&runs);
        let mut pattern = String::new();
        for (is_mark, ms) in runs {
            if is_mark {
                pattern.push(if ms < 2.0 * self.dit_ms { '.' } else { '-' });
            } else if ms >= 2.0 * self.dit_ms {
                Self::emit(&mut pattern, events);
            }
        }
        Self::emit(&mut pattern, events);
    }

    fn emit(pattern: &mut String, events: &mut Vec<DecodeEvent>) {
        if pattern.is_empty() {
            return;
        }
        let p = std::mem::take(pattern);
        events.push(match decode_pattern(&p) {
            Some(c) => DecodeEvent::Char(c),
            None => DecodeEvent::Unknown(p),
        });
    }
}

/// Render decode events as text: characters, `*` for unknown patterns, spaces for gaps.
pub fn events_to_text(events: &[DecodeEvent]) -> String {
    let mut s = String::new();
    for e in events {
        match e {
            DecodeEvent::Char(c) => s.push(*c),
            DecodeEvent::Unknown(_) => s.push('*'),
            DecodeEvent::WordGap => {
                if !s.is_empty() && !s.ends_with(' ') {
                    s.push(' ')
                }
            }
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Keyer, Noise};

    const SR: u32 = 8000;
    const MSG: &str = "W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K";

    fn decode(audio: &[f32], cfg: DecoderConfig) -> (String, f32) {
        let mut d = Decoder::new(cfg);
        let mut events = Vec::new();
        // Feed in irregular chunks as a sound card would.
        for chunk in audio.chunks(317) {
            events.extend(d.push(chunk));
        }
        events.extend(d.push(&vec![0.0; SR as usize * 2]));
        events.extend(d.flush());
        (events_to_text(&events), d.wpm())
    }

    fn char_errors(a: &str, b: &str) -> usize {
        // Levenshtein distance.
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut cur = vec![i; b.len() + 1];
            for j in 1..=b.len() {
                cur[j] = (prev[j] + 1)
                    .min(cur[j - 1] + 1)
                    .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
            }
            prev = cur;
        }
        prev[b.len()]
    }

    #[test]
    fn decodes_clean_machine_cw_at_several_speeds() {
        for wpm in [8.0, 13.0, 20.0, 28.0, 34.0] {
            let k = Keyer::new(SR, 600.0, wpm);
            let (text, est) = decode(&k.render(MSG, 300.0), DecoderConfig::new(SR, 600.0));
            assert_eq!(text, MSG, "at {wpm} wpm");
            assert!(
                (est - wpm).abs() / wpm < 0.15,
                "estimated {est} wpm for {wpm}"
            );
        }
    }

    #[test]
    fn decodes_noisy_signal() {
        // SNR is measured in 2500 Hz. Below about -4 dB errors climb quickly; see
        // `examples/sweep.rs` for the curve.
        for (seed, snr) in [(1, 0.0), (2, -3.0), (3, -3.0)] {
            let k = Keyer::new(SR, 600.0, 18.0);
            let mut audio = k.render(MSG, 500.0);
            Noise::new(seed).add(
                &mut audio,
                Noise::sigma_for_snr(k.amplitude, snr, SR, 2500.0),
            );
            let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
            let errs = char_errors(&text, MSG);
            assert!(errs <= 2, "SNR {snr} dB: {errs} errors in {text:?}");
        }
    }

    #[test]
    fn decodes_sloppy_hand_keying() {
        let mut k = Keyer::new(SR, 600.0, 15.0);
        k.jitter = 0.12;
        k.gap_stretch = 1.4;
        k.seed = 7;
        let mut audio = k.render(MSG, 500.0);
        Noise::new(9).add(
            &mut audio,
            Noise::sigma_for_snr(k.amplitude, 3.0, SR, 2500.0),
        );
        let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
        let errs = char_errors(&text, MSG);
        assert!(errs <= 3, "{errs} errors in {text:?}");
    }

    #[test]
    fn tolerates_tuning_offset() {
        let k = Keyer::new(SR, 640.0, 18.0);
        let (text, _) = decode(&k.render(MSG, 300.0), DecoderConfig::new(SR, 600.0));
        assert_eq!(text, MSG);
    }

    #[test]
    fn stays_quiet_on_pure_noise() {
        let mut audio = vec![0.0; SR as usize * 20];
        Noise::new(5).add(&mut audio, 0.1);
        let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
        // An occasional lone E from a noise burst is tolerated; the protocol layer
        // ignores anything that does not parse as a command.
        let letters = text.chars().filter(|c| *c != ' ').count();
        assert!(letters <= 2, "decoded noise as {text:?}");
    }

    #[test]
    fn ignores_off_frequency_station() {
        let k = Keyer::new(SR, 1500.0, 18.0);
        let (text, _) = decode(&k.render(MSG, 300.0), DecoderConfig::new(SR, 600.0));
        assert!(text.is_empty(), "{text:?}");
    }
}
