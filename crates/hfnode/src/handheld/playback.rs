//! Audio out to the handheld's microphone input: the Morse tone the node keys.
//!
//! The real output is [`crate::audio::Speaker`] (ALSA's `aplay` on Linux, cpal on
//! macOS and Windows); [`MockPlayback`] records what would be played, for tests.

use anyhow::{bail, Result};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Plays mono audio to the radio.
pub trait Playback: Send + Sync {
    /// The sample rate [`Playback::start`] takes.
    fn rate(&self) -> u32;
    /// Start playing `samples` and return without waiting for them to finish.
    fn start(&self, samples: Vec<f32>) -> Result<Box<dyn Playing>>;
    /// What plays the audio, for the log.
    fn describe(&self) -> String;
}

/// Audio being played.
pub trait Playing: Send {
    /// Every sample has been played, or playing stopped (an error, or
    /// [`Playing::stop`]).
    fn finished(&mut self) -> bool;
    /// Stop now. Calling it again does nothing.
    fn stop(&mut self);
}

/// One thing [`MockPlayback`] was asked to play.
#[derive(Debug, Clone)]
pub struct Played {
    /// When it started (real time).
    pub started: Instant,
    pub samples: Vec<f32>,
    /// When it was stopped before the end, if it was.
    pub stopped: Option<Instant>,
}

#[derive(Debug, Default)]
struct MockState {
    played: Vec<Played>,
    /// Fault: playing never finishes (a hung sound card).
    hang: bool,
    /// Fault: the output cannot be started.
    fail_start: bool,
}

/// Records what would be played; each "plays" for its length divided by
/// `time_scale`, in real time.
#[derive(Clone)]
pub struct MockPlayback {
    rate: u32,
    time_scale: f32,
    state: Arc<Mutex<MockState>>,
}

impl MockPlayback {
    pub fn new(rate: u32, time_scale: f32) -> Self {
        Self {
            rate,
            time_scale,
            state: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everything started so far.
    pub fn played(&self) -> Vec<Played> {
        self.lock().played.clone()
    }

    pub fn set_hang(&self, hang: bool) {
        self.lock().hang = hang;
    }

    pub fn set_fail_start(&self, fail: bool) {
        self.lock().fail_start = fail;
    }

    /// How long `n` samples take to play, in real time.
    pub fn real(&self, n: usize) -> Duration {
        Duration::from_secs_f64(n as f64 / f64::from(self.rate) / f64::from(self.time_scale))
    }
}

impl Playback for MockPlayback {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn start(&self, samples: Vec<f32>) -> Result<Box<dyn Playing>> {
        let mut s = self.lock();
        if s.fail_start {
            bail!("mock output: cannot start");
        }
        let length = self.real(samples.len());
        s.played.push(Played {
            started: Instant::now(),
            samples,
            stopped: None,
        });
        Ok(Box::new(MockPlaying {
            owner: self.clone(),
            index: s.played.len() - 1,
            end: Instant::now() + length,
            hang: s.hang,
        }))
    }

    fn describe(&self) -> String {
        "mock output".into()
    }
}

struct MockPlaying {
    owner: MockPlayback,
    index: usize,
    end: Instant,
    hang: bool,
}

impl Playing for MockPlaying {
    fn finished(&mut self) -> bool {
        let stopped = self.owner.lock().played[self.index].stopped.is_some();
        stopped || (!self.hang && Instant::now() >= self.end)
    }

    fn stop(&mut self) {
        let mut s = self.owner.lock();
        let p = &mut s.played[self.index];
        if p.stopped.is_none() && (self.hang || Instant::now() < self.end) {
            p.stopped = Some(Instant::now());
        }
    }
}
