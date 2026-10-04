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
//!
//! Since the floor only moves while the key is up, a jump in the noise level (or a
//! steady carrier) would hold the key down, or chattering, indefinitely. A tone's
//! envelope is steady while noise's fluctuates, so a mark whose envelope varies like
//! noise is taken for a jump in the noise level as soon as it has lasted long enough
//! to tell (about 100 ms): what was heard since the last real gap is dropped and the
//! levels are learned afresh. The same happens if the key has not been up for a real
//! gap in longer than any character can last (a steady carrier).

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

    /// Check that the decoder can work with these settings. Out-of-range audio
    /// parameters do not fail loudly: they leave the decoder deaf (zero bandwidth),
    /// unstable (negative bandwidth) or tuned to an alias of the tone (pitch above
    /// Nyquist).
    pub fn validate(&self) -> Result<(), String> {
        let sr = self.sample_rate as f32;
        if !(8000..=192_000).contains(&self.sample_rate) {
            return Err(format!(
                "sample_rate {} must be 8000-192000",
                self.sample_rate
            ));
        }
        if !(100.0..sr / 2.0).contains(&self.pitch_hz) {
            return Err(format!(
                "pitch_hz {} must be at least 100 and below half the sample rate",
                self.pitch_hz
            ));
        }
        if !(10.0..sr / 2.0).contains(&self.bandwidth_hz) {
            return Err(format!(
                "bandwidth_hz {} must be at least 10 and below half the sample rate",
                self.bandwidth_hz
            ));
        }
        if !(self.min_wpm > 0.0
            && self.min_wpm <= self.initial_wpm
            && self.initial_wpm <= self.max_wpm)
        {
            return Err("speeds must satisfy 0 < min_wpm <= initial_wpm <= max_wpm".into());
        }
        Ok(())
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
    /// Ticks since the key was last up for at least `2 * min_dit`, and the length
    /// of `word` at that time.
    busy_ticks: u32,
    busy_word_len: usize,
    /// Detector magnitude inside the current mark, edges left out: (count, sum, sum
    /// of squares), and the latest samples, held back until they are clear of the
    /// mark's end.
    texture: (u32, f64, f64),
    texture_tail: VecDeque<f32>,
    /// Set while settling again after [`Decoder::relearn_levels`]: the mean envelope
    /// so far, whose lower half alone teaches the floor.
    relearn_mean: Option<f32>,
}

const MARK_HISTORY: usize = 24;
/// Ticks of audio before the floor estimate is trusted.
const SETTLE_TICKS: u32 = 100;
/// Ticks left out at each end of a mark when judging its texture: the filter's rise
/// and fall, plus the debounce at the end.
const TEXTURE_EDGE_TICKS: usize = 20;
/// Inside ticks needed before a mark's texture is judged.
const TEXTURE_MIN_TICKS: u32 = 60;
/// Coefficient of variation of the magnitude above which a mark is noise. Band
/// noise gives a Rayleigh envelope (0.52); a tone at the lowest usable SNR (-4 dB in
/// 2500 Hz) stays below about 0.35. Only fast elements that noise has already run
/// together go above it.
const NOISE_CV: f64 = 0.4;

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
            busy_ticks: 0,
            busy_word_len: 0,
            texture: (0, 0.0, 0.0),
            texture_tail: VecDeque::new(),
            relearn_mean: None,
            cfg,
        }
    }

    /// Forget the signal and noise levels, the key state and any partly received
    /// word, as for a new decoder, but keep the learned speed (dit estimate, mark
    /// history and character-gap stretch). For use after the receiver has been
    /// muted or its levels changed, e.g. after transmitting, so the next word is not
    /// decoded at the initial speed.
    pub fn reset_levels(&mut self) {
        let fresh = Decoder::new(self.cfg.clone());
        *self = Decoder {
            dit_ms: self.dit_ms,
            char_gap_ms: self.char_gap_ms,
            marks: std::mem::take(&mut self.marks),
            working: (0, self.dit_ms),
            phase: self.phase,
            ..fresh
        };
    }

    /// Go back to the initial speed estimate (dit length, mark history and
    /// character-gap stretch), keeping the levels and anything being received. For
    /// a decoder that has listened to band noise or other stations for minutes:
    /// noise words of three or more marks teach it a speed no sender is using, and
    /// the next caller's first words would then be split wrongly.
    pub fn reset_speed(&mut self) {
        let dit_ms = 1200.0 / self.cfg.initial_wpm;
        self.dit_ms = dit_ms;
        self.char_gap_ms = 3.0 * dit_ms;
        self.marks.clear();
        self.working = (0, dit_ms);
    }

    /// Whether something is being received that has not been returned yet: the key
    /// is down, or a word is buffered awaiting its word gap (see [`Decoder::flush`]).
    pub fn has_partial(&self) -> bool {
        self.on || !self.word.is_empty()
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
            // One NaN, or a sample so large that its square overflows, would poison
            // every filter and level for good; zero or clamp it (full scale is 1.0),
            // keeping the time base.
            let x = if x.is_finite() {
                x.clamp(-4.0, 4.0)
            } else {
                0.0
            };
            self.phase = (self.phase + self.phase_step).rem_euclid(2.0 * PI);
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
        if let Some(mean) = &mut self.relearn_mean {
            // Learning the levels again, perhaps while a station is keying: only the
            // lower half of the envelope teaches the floor, and only samples near the
            // floor teach the deviation, so the tone is not taken for noise and
            // squelched.
            *mean += 0.05 * (self.env - *mean);
            if self.env <= *mean {
                self.floor += 0.1 * (self.env - self.floor);
            }
            if self.env > 0.5 * self.floor && self.env < 2.0 * self.floor {
                self.dev += 0.05 * ((self.env - self.floor).abs() - self.dev);
            }
            if self.ticks_seen <= SETTLE_TICKS {
                self.peak = self.floor;
                return;
            }
            self.relearn_mean = None;
        } else if self.ticks_seen <= SETTLE_TICKS {
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
            // deviation samples are clipped, so stray tones barely move either. The
            // clip allows at least min_level so a deviation learned on digital
            // silence (zero) can still grow when band noise arrives.
            let a = if env < self.floor { 0.02 } else { 0.002 };
            self.floor += a * (env - self.floor);
            let d = (env - self.floor)
                .abs()
                .min(4.0 * self.dev + self.cfg.min_level);
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

        // Noise held above a stale floor.
        if self.on {
            if self.run_ticks as usize > glitch as usize + TEXTURE_EDGE_TICKS {
                self.texture_tail.push_back(mag);
            }
            while self.texture_tail.len() > glitch as usize + TEXTURE_EDGE_TICKS {
                let m = f64::from(self.texture_tail.pop_front().unwrap_or(0.0));
                self.texture.0 += 1;
                self.texture.1 += m;
                self.texture.2 += m * m;
            }
            if self.texture.0 >= TEXTURE_MIN_TICKS && self.mark_is_noise() {
                self.relearn_levels(events);
                return;
            }
        } else if self.texture.0 > 0 || !self.texture_tail.is_empty() {
            self.texture = (0, 0.0, 0.0);
            self.texture_tail.clear();
        }

        // Stale levels: no real gap for longer than any character lasts.
        if !self.on && self.run_ticks as f32 >= 2.0 * min_dit {
            self.busy_ticks = 0;
            self.busy_word_len = self.word.len();
        } else {
            self.busy_ticks += 1;
            if self.busy_ticks as f32 > self.stale_ms() {
                self.relearn_levels(events);
                return;
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

    /// Whether the inside of the current mark fluctuates like noise.
    fn mark_is_noise(&self) -> bool {
        let (n, sum, sq) = self.texture;
        let mean = sum / f64::from(n);
        let var = (sq / f64::from(n) - mean * mean).max(0.0);
        mean > 0.0 && var.sqrt() > NOISE_CV * mean
    }

    /// Longest real CW can go without the key up for `2 * min_dit`: a dah at the
    /// slowest speed, or the longest character (0, 19 units) at the speed whose
    /// element gaps are that short, with 30% to spare for hand keying.
    fn stale_ms(&self) -> f32 {
        let min_dit = 1200.0 / self.cfg.max_wpm;
        let max_dit = 1200.0 / self.cfg.min_wpm;
        (4.0 * max_dit).max(1.3 * 19.0 * 2.0 * min_dit)
    }

    /// The noise level jumped or a carrier came up while the floor was frozen. Drop
    /// the runs heard since the last real gap so they neither decode nor train the
    /// speed, finish the word before them, and learn the levels again from here (a
    /// station may already be keying, so see `relearn_mean`).
    fn relearn_levels(&mut self, events: &mut Vec<DecodeEvent>) {
        self.word.truncate(self.busy_word_len);
        if !self.word.is_empty() {
            self.finish_word(events);
            events.push(DecodeEvent::WordGap);
        }
        self.on = false;
        self.raw_on = false;
        self.run_ticks = 0;
        self.candidate_ticks = 0;
        self.busy_ticks = 0;
        self.busy_word_len = 0;
        self.texture = (0, 0.0, 0.0);
        self.texture_tail.clear();
        self.floor = self.env;
        self.relearn_mean = Some(self.env);
        self.ticks_seen = 0;
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

    /// Quiet band for `quiet_s`, then the noise jumps by `step_db` and stays up, and
    /// `msg` starts `delay_s` after the step, sent at `wpm`.
    fn noise_step(
        wpm: f32,
        quiet_s: f32,
        step_db: f32,
        delay_s: f32,
        msg: &str,
        seed: u64,
    ) -> Vec<f32> {
        let k = Keyer::new(SR, 600.0, wpm);
        let quiet = (quiet_s * SR as f32) as usize;
        let mut audio = vec![0.0; quiet];
        audio.extend(k.render(msg, delay_s * 1000.0));
        let sigma = 0.005;
        let mut noise = Noise::new(seed);
        noise.add(&mut audio[..quiet], sigma);
        noise.add(&mut audio[quiet..], sigma * 10f32.powf(step_db / 20.0));
        audio
    }

    /// `msg` decoded as words of its own, ignoring lone letters from noise bursts
    /// before it (the node hands those to the protocol separately).
    fn ends_with_message(text: &str, msg: &str) -> bool {
        text == msg || text.ends_with(&format!(" {msg}"))
    }

    #[test]
    fn noise_step_does_not_latch_a_false_mark() {
        const SHORT: &str = "OK 43 WBNFHJGC K";
        for (seed, step_db) in [(1, 10.0), (2, 10.0), (3, 15.0), (5, 20.0)] {
            let audio = noise_step(18.0, 5.0, step_db, 4.0, SHORT, seed);
            let (text, est) = decode(&audio, DecoderConfig::new(SR, 600.0));
            assert!(
                ends_with_message(&text, SHORT),
                "+{step_db} dB step, seed {seed}: {text:?}"
            );
            assert!((est - 18.0).abs() < 2.0, "estimated {est} wpm");
        }
        // A decoder that settled on digital silence, then hears band noise.
        for (seed, step_db) in [(1, 0.0), (2, 10.0)] {
            let mut audio = vec![0.0; SR as usize / 8];
            audio.extend(noise_step(18.0, 0.0, step_db, 4.0, SHORT, seed));
            let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
            assert!(
                ends_with_message(&text, SHORT),
                "noise +{step_db} dB after silence, seed {seed}: {text:?}"
            );
        }
    }

    /// `msg` decoded as a run of words of its own, whatever noise letters come
    /// before or after it.
    fn contains_message(text: &str, msg: &str) -> bool {
        let words: Vec<&str> = text.split(' ').collect();
        let want: Vec<&str> = msg.split(' ').collect();
        words.windows(want.len()).any(|w| w == want.as_slice())
    }

    #[test]
    fn noise_step_just_before_a_message() {
        const SHORT: &str = "OK 43 WBNFHJGC K";
        // The message starts a second after the step: sooner than the noise can be
        // told from a carrier by length alone.
        for step_db in [10.0, 20.0, 30.0] {
            for seed in 1..=3 {
                let audio = noise_step(18.0, 5.0, step_db, 1.0, SHORT, seed);
                let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
                assert!(
                    ends_with_message(&text, SHORT),
                    "+{step_db} dB step, seed {seed}: {text:?}"
                );
            }
        }
        // A fresh decoder, as at a window opening: it settles on digital silence,
        // then band noise arrives with the message close behind.
        for wpm in [18.0, 25.0] {
            for delay_s in [1.0, 1.4] {
                for seed in 1..=4 {
                    let mut audio = vec![0.0; SR as usize / 8];
                    audio.extend(noise_step(wpm, 0.0, 0.0, delay_s, SHORT, seed));
                    let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
                    assert!(
                        ends_with_message(&text, SHORT),
                        "{wpm} wpm {delay_s} s after the noise, seed {seed}: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn noise_step_does_not_train_the_speed() {
        // A small step holds the key down for a few hundred ms at a time. Those
        // marks must not be learned as dahs, or the message's words run together
        // long after.
        const SHORT: &str = "OK 43 WBNFHJGC K";
        for (wpm, delay_s, seed) in [(25.0, 15.0, 3), (25.0, 30.0, 3), (32.0, 15.0, 1)] {
            let audio = noise_step(wpm, 5.0, 6.0, delay_s, SHORT, seed);
            let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
            assert!(
                contains_message(&text, SHORT),
                "{wpm} wpm, {delay_s} s after a +6 dB step: {text:?}"
            );
        }
    }

    #[test]
    fn steady_carrier_is_not_decoded() {
        // Someone tunes up on frequency for 3 s; the message follows a second later.
        const SHORT: &str = "OK 43 WBNFHJGC K";
        let k = Keyer::new(SR, 600.0, 18.0);
        let mut audio: Vec<f32> = (0..SR as usize * 3)
            .map(|i| k.amplitude * (2.0 * PI * 600.0 * i as f32 / SR as f32).sin())
            .collect();
        audio.extend(k.render(SHORT, 1000.0));
        Noise::new(3).add(
            &mut audio,
            Noise::sigma_for_snr(k.amplitude, 10.0, SR, 2500.0),
        );
        let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
        assert_eq!(text, SHORT);
    }

    #[test]
    fn survives_non_finite_samples() {
        let k = Keyer::new(SR, 600.0, 18.0);
        let mut audio = vec![f32::NAN];
        audio.extend(k.render(MSG, 300.0));
        audio[SR as usize] = f32::INFINITY;
        let (text, _) = decode(&audio, DecoderConfig::new(SR, 600.0));
        assert_eq!(text, MSG);
    }

    #[test]
    fn survives_huge_samples() {
        // Finite, but its square overflows to infinity.
        let k = Keyer::new(SR, 600.0, 18.0);
        for big in [3e38, f32::MAX, f32::MIN] {
            let mut audio = vec![big];
            audio.extend(k.render(MSG, 300.0));
            let mut d = Decoder::new(DecoderConfig::new(SR, 600.0));
            let mut events = d.push(&audio);
            events.extend(d.push(&vec![0.0; SR as usize * 2]));
            assert_eq!(events_to_text(&events), MSG, "after {big}");
            let (floor, dev, peak) = d.levels();
            assert!(floor.is_finite() && dev.is_finite() && peak.is_finite());
        }
    }

    #[test]
    fn mixer_phase_stays_bounded() {
        // A step above 2*PI (pitch above the sample rate) aliases to the same tone;
        // the phase must wrap rather than grow until f32 loses the fraction.
        let k = Keyer::new(SR, 600.0, 18.0);
        let mut d = Decoder::new(DecoderConfig::new(SR, SR as f32 * 3.0 + 600.0));
        d.push(&k.render(MSG, 300.0));
        assert!((0.0..2.0 * PI).contains(&d.phase), "phase {}", d.phase);
    }

    #[test]
    fn validates_audio_parameters() {
        assert!(DecoderConfig::new(8000, 600.0).validate().is_ok());
        assert!(DecoderConfig::new(48_000, 700.0).validate().is_ok());
        let bad = |f: fn(&mut DecoderConfig)| {
            let mut c = DecoderConfig::new(8000, 600.0);
            f(&mut c);
            c.validate().is_err()
        };
        assert!(bad(|c| c.sample_rate = 0));
        assert!(bad(|c| c.sample_rate = 400_000));
        assert!(bad(|c| c.pitch_hz = 4000.0));
        assert!(bad(|c| c.pitch_hz = 50.0));
        assert!(bad(|c| c.pitch_hz = f32::NAN));
        assert!(bad(|c| c.bandwidth_hz = 0.0));
        assert!(bad(|c| c.bandwidth_hz = -150.0));
        assert!(bad(|c| c.bandwidth_hz = f32::NAN));
        assert!(bad(|c| c.min_wpm = 0.0));
    }

    #[test]
    fn reset_levels_keeps_speed() {
        // Learn a slow sender, then reset as the node does after transmitting.
        let k = Keyer::new(SR, 600.0, 6.0);
        let mut d = Decoder::new(DecoderConfig::new(SR, 600.0));
        d.push(&k.render("TEST DE W5XXX K", 300.0));
        d.flush();
        let wpm = d.wpm();
        assert!((wpm - 6.0).abs() < 1.0, "{wpm}");
        d.push(&k.render("OK", 300.0)[..SR as usize]);
        assert!(d.has_partial());
        d.reset_levels();
        assert!(!d.has_partial() && !d.key_down());
        assert_eq!(d.wpm(), wpm);
        // The first word after the reset is not split at the initial speed.
        const SHORT: &str = "OK 43 WBNFHJGC K";
        let mut events = d.push(&k.render(SHORT, 500.0));
        events.extend(d.push(&vec![0.0; SR as usize * 3]));
        assert!(!d.has_partial());
        assert_eq!(events_to_text(&events), SHORT);
        let mut fresh = Decoder::new(DecoderConfig::new(SR, 600.0));
        let mut events = fresh.push(&k.render(SHORT, 500.0));
        events.extend(fresh.flush());
        assert_ne!(
            events_to_text(&events),
            SHORT,
            "fresh decoder should split it"
        );
    }

    #[test]
    fn reset_speed_forgets_what_noise_taught() {
        const CALL: &str = "W5XXX 44 WMYDPUDR TX BOB CALL ME K";
        let sigma = Noise::sigma_for_snr(0.5, 15.0, SR, 2500.0);
        let mut k = Keyer::new(SR, 600.0, 18.0);
        k.jitter = 0.03;
        // Five minutes of band noise, then a call heard through the same noise.
        let quiet = SR as usize * 302;
        let mut garbled = 0;
        // Seeds whose noise taught the decoder a wrong speed when this was written.
        for seed in [2u64, 4, 5] {
            let mut audio = vec![0.0; quiet];
            audio.extend(k.render(CALL, 0.0));
            audio.extend(vec![0.0; SR as usize * 4]);
            Noise::new(seed * 7919).add(&mut audio, sigma);
            let heard = |reset: bool| {
                let mut d = Decoder::new(DecoderConfig::new(SR, 600.0));
                let mut events = Vec::new();
                for (i, b) in audio.chunks(400).enumerate() {
                    if reset && i * 400 == SR as usize * 300 && !d.has_partial() {
                        d.reset_speed();
                        assert_eq!(d.wpm(), 15.0);
                    }
                    events.extend(d.push(b));
                }
                events.extend(d.flush());
                events_to_text(&events)
            };
            garbled += usize::from(!heard(false).contains(CALL));
            let after_reset = heard(true);
            assert!(after_reset.contains(CALL), "seed {seed}: {after_reset}");
        }
        // Without the reset the noise does harm, or this test shows nothing.
        assert!(garbled > 0);
    }

    #[test]
    fn ignores_off_frequency_station() {
        let k = Keyer::new(SR, 1500.0, 18.0);
        let (text, _) = decode(&k.render(MSG, 300.0), DecoderConfig::new(SR, 600.0));
        assert!(text.is_empty(), "{text:?}");
    }
}
