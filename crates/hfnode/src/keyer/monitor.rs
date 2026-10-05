//! The sidetone monitor: what the radio's headphone audio says about its key.
//!
//! The keyer rig cannot ask the radio anything, so it listens instead. Every block
//! of captured audio comes here as it is captured (a tap ahead of the decoder's
//! queue, so blocks keep coming while the node is busy keying), cut into 10 ms
//! slices, each measured at the sidetone pitch. From that:
//!
//! - [`Monitor::judge`]: whether the radio was heard keying a run the box keyed,
//!   element by element: the sidetone must follow the box's timing, at whatever
//!   audio delay fits best (up to [`MAX_LAG`]), loud in the elements and quiet in
//!   the gaps. That shows the key cable, the radio's key setting, its sidetone and
//!   the audio are all working, which is also what the stuck-key checks need.
//! - [`Monitor::stuck`]: whether the radio's key is down when the box's is not.
//!   Either the sidetone carried on, without a break, after the box opened its key
//!   (a shorted optocoupler or cable), or a steady tone at the pitch has gone on for
//!   [`STEADY_TONE`], far longer than anyone tunes up on the frequency.
//! - [`Monitor::band`]: whether audio is arriving, whether the band noise is loud
//!   enough (`[keyer] min_level_dbfs`) that the node would hear its own sidetone and
//!   the field station, and whether a steady tone sits at the pitch right now (a
//!   carrier, or the radio's key closed): the node does not key then.
//!
//! Times here are radio time, in seconds since the monitor started: wall-clock
//! time multiplied by the time scale, which is 1 except in the self-tests, whose
//! radio and audio run faster than real time.

use keyer_core::morse::Segment;
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// Length of a slice: 10 ms of audio.
const SLICE_S: f64 = 0.010;
/// Slices kept: 2 minutes of audio.
const KEEP_S: f64 = 120.0;
/// Capture stamps used to place the audio in time: the last 30 s.
const STAMPS_S: f64 = 30.0;
/// A block this much later than the audio before it predicts means audio was
/// lost (the capture stalled or restarted): the timing starts again from it. In
/// real time, as are the two below: how late audio arrives is up to the computer,
/// whatever the scale.
const STAMP_JUMP_S: f64 = 1.0;
/// Every block for [`LOSS_CONFIRM_S`] this much later than the timing so far
/// means a little audio was lost (a sound card dropout, an overrun): the timing is
/// moved on by that much, from the first late block.
const LOSS_S: f64 = 0.05;
const LOSS_CONFIRM_S: f64 = 0.3;
/// The longest the audio may lag the box's keying: the radio's own keying delay,
/// the sound card's buffers and the capture pipe.
pub const MAX_LAG: Duration = Duration::from_millis(500);
const MAX_LAG_S: f64 = 0.5;
/// Lag search step: one slice.
const LAG_STEP_S: f64 = SLICE_S;
/// Slices whose centre is this close to an expected key change are left out: half
/// a slice, half a lag step, and the radio's keying edges.
const EDGE_S: f64 = 0.012;
/// Receive audio just before a run, counted as key-up.
const PRE_S: f64 = 0.25;
/// Key-up counted after the last element: this, or 3 dots if less (still inside
/// the gap before a next word).
const POST_S: f64 = 0.3;
/// Heard: the sidetone at least this far above the key-up audio (medians).
pub const MIN_CONTRAST_DB: f32 = 10.0;
/// Heard: at least this share of the key-down slices over the midway level, and of
/// the key-up slices under it.
pub const MIN_SHARE: f32 = 0.85;
/// Too few slices to judge (a run cut short at once).
const MIN_SLICES: usize = 3;
/// Stuck after a run: the tone held, unbroken, this long after the box opened
/// its key (and the audio delay).
pub const STUCK_AFTER_RUN: Duration = Duration::from_millis(500);
const STUCK_AFTER_RUN_S: f64 = 0.5;
/// A break in the tone at least this long after a run shows the key did open.
const RELEASE_S: f64 = 0.05;
/// Share of slices after a run that must carry the tone for it to count as held.
const HELD_SHARE: f32 = 0.9;
/// Stuck at any time: a steady tone at the pitch for this long.
pub const STEADY_TONE: Duration = Duration::from_secs(30);
const STEADY_TONE_S: f64 = 30.0;
/// Share of the steady-tone stretch that must carry it.
const STEADY_SHARE: f32 = 0.95;
/// A steady tone: at least this share of a slice's power at the pitch.
const MIN_PURITY: f32 = 0.6;
/// A steady tone: each slice within this of the one before. A carrier's level (and
/// a sidetone's) holds; band noise, even through a narrow filter, jumps about.
const STEADY_STEP_DB: f32 = 1.5;
/// A carrier now: the last this much audio a steady tone at the pitch ...
const CARRIER_S: f64 = 1.0;
/// ... in at least this share of it.
const CARRIER_SHARE: f32 = 0.9;
/// A tone this far under the sidetone heard in the last good run is not it.
const BELOW_SIDETONE_DB: f32 = 10.0;
/// Before any run was heard, a steady tone must be this far over `min_level_dbfs`.
const OVER_MIN_LEVEL_DB: f32 = 10.0;
/// Over the band's level when the run started: the sidetone, if the run gave no
/// level.
const OVER_RECEIVE_DB: f32 = 10.0;
/// Audio is arriving if a block came within this (wall-clock) time.
pub const AUDIO_FRESH: Duration = Duration::from_secs(1);
/// The band's level is measured over this much receive audio.
const LEVEL_S: f64 = 1.0;
/// ... of which at least this much must be there.
const LEVEL_MIN_S: f64 = 0.5;
/// Receive audio starts this long after the box opened its key and the audio
/// delay: the longest break-in delay (10 dots at 5 wpm is 2.4 s).
const RX_AFTER_S: f64 = 2.5;
/// A level measured this long ago (wall clock) still stands while the node keys
/// a transmission's pieces, which leave no receive audio between them.
pub const LEVEL_KEEPS: Duration = Duration::from_secs(120);

/// What the monitor needs to know.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub sample_rate: u32,
    /// The radio's sidetone pitch.
    pub pitch_hz: f32,
    /// Quietest band noise the node keys with, in dBFS.
    pub min_level_dbfs: f32,
    /// Radio seconds per wall-clock second: 1, except in the self-tests.
    pub scale: f32,
}

#[derive(Debug, Clone, Copy)]
struct Slice {
    /// Radio time of the slice's centre.
    t: f64,
    /// Power at the pitch, as the mean square of a sine of that amplitude, dBFS.
    tone_db: f32,
    /// Mean square of the slice, dBFS.
    total_db: f32,
    /// Share of the slice's power at the pitch.
    purity: f32,
}

/// One `CW` run the box keyed, as the node expects it to sound.
#[derive(Debug, Clone)]
struct Run {
    id: u64,
    /// When the box took the run, radio time.
    start: f64,
    /// Key-down stretches, relative to `start`.
    downs: Vec<(f64, f64)>,
    /// The last element's end, relative to `start`.
    end: f64,
    dot: f64,
    /// The box opened the key early (`STOP`, or the run ended some other way),
    /// relative to `start`.
    opened: Option<f64>,
    /// After it, the tone broke off: no longer a candidate for a key held down.
    released: bool,
    judge: Option<Judge>,
    /// The band's level when the box took it (`Monitor::band`), dBFS.
    band_db: Option<f32>,
}

impl Run {
    /// When the key opened for good, relative to `start`.
    fn open_at(&self) -> f64 {
        self.opened.map_or(self.end, |o| o.min(self.end))
    }
}

/// Whether a run was heard, and how it sounded at the audio delay that fits best.
#[derive(Debug, Clone, PartialEq)]
pub struct Judge {
    pub heard: bool,
    /// The audio delay that fits best.
    pub lag: Duration,
    /// Median level at the pitch in the elements, and in the gaps around them, dBFS.
    pub tone_db: f32,
    pub gap_db: f32,
    /// Median level at the pitch just before the run.
    pub before_db: Option<f32>,
    /// Share of key-down slices over the midway level, and of key-up ones (in the
    /// run's gaps and after it) under.
    pub share_down: f32,
    pub share_up: f32,
    /// Share of the slices just before the run at least [`MIN_CONTRAST_DB`] under
    /// the sidetone (1 if there were none).
    pub share_before: f32,
    pub slices_down: usize,
    pub slices_up: usize,
}

impl Judge {
    pub fn contrast_db(&self) -> f32 {
        self.tone_db - self.gap_db
    }

    /// Why it was not heard, for the log and the operator.
    pub fn why_not(&self) -> Option<String> {
        if self.heard {
            return None;
        }
        Some(
            if self.slices_down < MIN_SLICES || self.slices_up < MIN_SLICES {
                "too little audio to judge".to_string()
            } else if self.share_before < MIN_SHARE {
                format!(
                    "the sidetone pitch was already loud before the box keyed ({:.0} dBFS): a \
                     carrier on the frequency, or the key closed at the radio",
                    self.before_db.unwrap_or(f32::NAN)
                )
            } else if self.contrast_db() < MIN_CONTRAST_DB {
                format!(
                    "the elements were only {:.0} dB over the gaps (need {MIN_CONTRAST_DB:.0}): \
                     no sidetone, or it does not stop between elements",
                    self.contrast_db()
                )
            } else {
                format!(
                    "the sidetone did not follow the box's keying ({:.0} % of elements, \
                     {:.0} % of gaps as expected): is the radio set to a straight key?",
                    self.share_down * 100.0,
                    self.share_up * 100.0
                )
            },
        )
    }
}

impl fmt::Display for Judge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (delay {} ms, sidetone {:.0} dBFS, gaps {:.0} dBFS, {:.0} % / {:.0} %)",
            if self.heard { "heard" } else { "not heard" },
            self.lag.as_millis(),
            self.tone_db,
            self.gap_db,
            self.share_down * 100.0,
            self.share_up * 100.0
        )
    }
}

/// What the audio says about the band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    /// A block came within [`AUDIO_FRESH`].
    pub audio: bool,
    /// The level of the receive audio in dBFS (median over a second): measured now,
    /// or within [`LEVEL_KEEPS`] if the node has been keying since. A carrier is not
    /// the band: while one is heard the level from before it stands.
    pub level_db: Option<f32>,
    /// A steady tone at the pitch over the last second, and its level: a station's
    /// carrier on the frequency, or the radio's key closed at the radio. The node
    /// does not key over it.
    pub carrier_db: Option<f32>,
}

/// What the audio says about the radio's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// Open, as far as the audio shows.
    Open,
    /// The audio does not yet reach far enough past the end of the last run to
    /// show the key open at the radio.
    Unsure,
    /// Closed at the radio while the box's key is open: why it looks that way.
    Held(String),
}

/// The sidetone monitor. See the module documentation.
pub struct Monitor {
    s: Settings,
    t0: Instant,
    slice_len: usize,
    coeff: f32,
    /// Samples not yet a whole slice.
    pending: Vec<f32>,
    /// Samples cut into slices so far.
    n: u64,
    /// Each block's capture: (its radio time, the radio time its first sample would
    /// have if it had been captured with no delay at all, that sample's index).
    stamps: VecDeque<(f64, f64, u64)>,
    base: f64,
    slices: VecDeque<Slice>,
    last_block: Option<Instant>,
    runs: VecDeque<Run>,
    next_id: u64,
    /// The sidetone level of the last run heard.
    sidetone_db: Option<f32>,
    /// The last receive level measured, and when (wall clock).
    level: Option<(Instant, f32)>,
    /// Bring-up: raw audio kept from the sample with this index on.
    raw: Option<(u64, Vec<f32>)>,
}

impl Monitor {
    pub fn new(s: Settings) -> Self {
        Self::starting_at(s, Instant::now())
    }

    /// A monitor whose radio time starts at `t0`.
    pub fn starting_at(s: Settings, t0: Instant) -> Self {
        let slice_len = ((f64::from(s.sample_rate) * SLICE_S).round() as usize).max(8);
        let w = 2.0 * std::f32::consts::PI * s.pitch_hz / s.sample_rate as f32;
        Self {
            s,
            t0,
            slice_len,
            coeff: 2.0 * w.cos(),
            pending: Vec::new(),
            n: 0,
            stamps: VecDeque::new(),
            base: 0.0,
            slices: VecDeque::new(),
            last_block: None,
            runs: VecDeque::new(),
            next_id: 1,
            sidetone_db: None,
            level: None,
            raw: None,
        }
    }

    pub fn settings(&self) -> Settings {
        self.s
    }

    /// Radio time of `at`.
    fn radio(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.t0).as_secs_f64() * f64::from(self.s.scale)
    }

    /// `s` seconds of real time, in radio seconds.
    fn real(&self, s: f64) -> f64 {
        s * f64::from(self.s.scale)
    }

    /// Radio time of the newest audio.
    fn latest(&self) -> Option<f64> {
        self.slices.back().map(|s| s.t + SLICE_S / 2.0)
    }

    /// A block of captured audio, completed at `at`.
    pub fn push(&mut self, at: Instant, samples: &[f32]) {
        self.last_block = Some(at);
        let sr = f64::from(self.s.sample_rate.max(1));
        let at_r = self.radio(at);
        let end = self.n + (self.pending.len() + samples.len()) as u64;
        // The block's samples were captured no later than `at`: the earliest any
        // block implies for the first sample is the best estimate of the delay-free
        // timing, kept over the last 30 s so that the sound card's clock drifting
        // against the computer's does not add up.
        let first = at_r - end as f64 / sr;
        if self.stamps.is_empty() || first > self.base + self.real(STAMP_JUMP_S) {
            if !self.stamps.is_empty() {
                log::warn!("sidetone monitor: audio was lost; timing it again");
            }
            self.stamps.clear();
        }
        self.stamps
            .push_back((at_r, first, end - samples.len() as u64));
        while self
            .stamps
            .front()
            .is_some_and(|&(t, _, _)| t < at_r - STAMPS_S)
        {
            self.stamps.pop_front();
        }
        let base = self
            .stamps
            .iter()
            .map(|&(_, f, _)| f)
            .fold(f64::INFINITY, f64::min);
        self.base = base;
        self.check_loss(at_r);

        if let Some((_, raw)) = self.raw.as_mut() {
            let room = (KEEP_S * sr) as usize;
            if raw.len() < room {
                raw.extend_from_slice(&samples[..samples.len().min(room - raw.len())]);
            }
        }
        self.pending.extend_from_slice(samples);
        let mut used = 0;
        while self.pending.len() - used >= self.slice_len {
            let chunk = &self.pending[used..used + self.slice_len];
            let t = self.base + (self.n as f64 + self.slice_len as f64 / 2.0) / sr;
            let slice = measure(chunk, self.coeff, t);
            self.slices.push_back(slice);
            self.n += self.slice_len as u64;
            used += self.slice_len;
        }
        self.pending.drain(..used);
        while self.slices.front().is_some_and(|s| s.t < at_r - KEEP_S) {
            self.slices.pop_front();
        }
    }

    /// A little audio lost shows as every block since arriving that much later than
    /// the samples count for, while the earliest-ever timing in `base` stays put: then
    /// the timing moves on by that much from the first late block, and the slices
    /// cut since then move with it. Without this they would sit up to a second too
    /// early, and the runs keyed over them would not be heard, for 30 s. Audio held
    /// up and then delivered all at once is not lost: its blocks come less and less
    /// late, then on time, and the timing stays.
    fn check_loss(&mut self, at_r: f64) {
        let (confirm, loss) = (self.real(LOSS_CONFIRM_S), self.real(LOSS_S));
        // The blocks since the last one on time, newest first.
        let base = self.base;
        let run: Vec<(f64, f64, u64)> = self
            .stamps
            .iter()
            .rev()
            .take_while(|&&(_, f, _)| f - base > loss)
            .copied()
            .collect();
        let Some(&(since, _, from_n)) = run.last() else {
            return;
        };
        if run.len() < 2 || run.len() == self.stamps.len() || at_r - since < confirm {
            // Too few blocks to tell, nothing older to compare with, or not late
            // for long enough yet.
            return;
        }
        // Late by the same amount over the last `confirm`, not catching up.
        let (late, most_late) = run
            .iter()
            .take_while(|&&(t, _, _)| t >= at_r - confirm)
            .fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(lo, hi), &(_, f, _)| (lo.min(f), hi.max(f)),
            );
        if most_late - late > loss {
            return;
        }
        let shift = late - self.base;
        log::warn!(
            "sidetone monitor: {:.0} ms of audio was lost; timing it from there",
            shift * 1000.0
        );
        let sr = f64::from(self.s.sample_rate.max(1));
        let from_t = self.base + from_n as f64 / sr;
        for s in self.slices.iter_mut().rev() {
            if s.t < from_t {
                break;
            }
            s.t += shift;
        }
        let keep = self.stamps.len() - run.len();
        self.stamps.drain(..keep);
        self.base = late;
    }

    /// The box took a `CW` run at `start` (no later than then): `segs` keyed at a
    /// dot of `dot` (radio time). Returns the run's id.
    pub fn run_started(&mut self, start: Instant, dot: Duration, segs: &[Segment]) -> u64 {
        let dot = dot.as_secs_f64();
        let mut t = 0.0;
        let mut downs = Vec::new();
        for s in segs {
            let len = f64::from(s.units) * dot;
            if s.down {
                downs.push((t, t + len));
            }
            t += len;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.runs.push_back(Run {
            id,
            start: self.radio(start),
            downs,
            end: t,
            dot,
            opened: None,
            released: false,
            judge: None,
            band_db: self.level.map(|(_, db)| db),
        });
        while self.runs.len() > 4 {
            self.runs.pop_front();
        }
        id
    }

    /// The box opened its key at `at` (or before), ending run `id` early.
    pub fn key_opened(&mut self, id: u64, at: Instant) {
        let t = self.radio(at);
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == id) {
            let rel = (t - r.start).max(0.0);
            r.opened = Some(r.opened.map_or(rel, |o| o.min(rel)));
        }
    }

    /// Whether run `id` was heard; `None` until the audio covers it (and its delay).
    pub fn judge(&mut self, id: u64) -> Option<Judge> {
        let latest = self.latest()?;
        let idx = self.runs.iter().position(|r| r.id == id)?;
        let run = &self.runs[idx];
        if let Some(j) = &run.judge {
            return Some(j.clone());
        }
        let post = POST_S.min(3.0 * run.dot);
        let open = run.open_at();
        if latest < run.start + open + post + MAX_LAG_S {
            return None;
        }
        let j = judge_run(&self.slices, run, post);
        if j.heard {
            self.sidetone_db = Some(j.tone_db);
        }
        self.runs[idx].judge = Some(j.clone());
        Some(j)
    }

    /// What the audio says about the radio's key, while the box's is open.
    pub fn key_state(&mut self) -> KeyState {
        match self.after_run() {
            KeyState::Open => self.steady_tone().map_or(KeyState::Open, KeyState::Held),
            other => other,
        }
    }

    /// After the last run: the sidetone went on, unbroken, after the box opened its
    /// key; or the audio does not yet reach far enough past it to tell.
    fn after_run(&mut self) -> KeyState {
        let Some(idx) = self.runs.len().checked_sub(1) else {
            return KeyState::Open;
        };
        let run = &self.runs[idx];
        if run.released {
            return KeyState::Open;
        }
        let open = run.start + run.open_at();
        // Before the run was judged, or if it was not heard, the delay is not
        // known: the longest there may be; and the key counts as open only once the
        // tone at the pitch drops near the band's level from before the run. Not
        // the audio just before the run: if the key was already closed at the
        // radio, that is the sidetone itself.
        let (lag, threshold) = match &run.judge {
            Some(j) if j.heard => (j.lag.as_secs_f64(), (j.tone_db + j.gap_db) / 2.0),
            _ => (
                MAX_LAG_S,
                run.band_db
                    .or_else(|| median_before(&self.slices, run.start))
                    .unwrap_or(self.s.min_level_dbfs)
                    + OVER_RECEIVE_DB,
            ),
        };
        let from = open + lag + EDGE_S;
        // A break as long as RELEASE_S anywhere since: the key did open.
        let need = (RELEASE_S / SLICE_S).round() as usize;
        let (mut quiet, mut n, mut toned) = (0usize, 0usize, 0usize);
        for s in self.slices.iter().filter(|s| s.t >= from) {
            n += 1;
            if s.tone_db < threshold {
                quiet += 1;
                if quiet >= need {
                    self.runs[idx].released = true;
                    return KeyState::Open;
                }
            } else {
                toned += 1;
                quiet = 0;
            }
        }
        let heard_for = self.latest().map_or(0.0, |l| l - from);
        if heard_for < STUCK_AFTER_RUN_S {
            // Not yet; and with the audio gone (the sound card dropped out) the box's
            // word alone does not show the radio's key open.
            return KeyState::Unsure;
        }
        if toned as f32 >= HELD_SHARE * n as f32 {
            KeyState::Held(format!(
                "the sidetone went on for {heard_for:.1} s after the box opened its key: the \
                 key is closed at the radio (a shorted optocoupler or key cable?)"
            ))
        } else {
            self.runs[idx].released = true;
            KeyState::Open
        }
    }

    /// A steady tone at the pitch for [`STEADY_TONE`].
    fn steady_tone(&self) -> Option<String> {
        let latest = self.latest()?;
        let from = latest - STEADY_TONE_S;
        let first = self.slices.front()?;
        if first.t > from + SLICE_S {
            return None;
        }
        let floor = match self.sidetone_db {
            Some(db) => db - BELOW_SIDETONE_DB,
            None => self.s.min_level_dbfs + OVER_MIN_LEVEL_DB,
        };
        let (n, toned) = self.steady_count(from, floor);
        (n > 0 && toned as f32 >= STEADY_SHARE * n as f32).then(|| {
            format!(
                "a steady tone at the sidetone pitch for {} s: the key is closed at the radio, \
                 or a carrier sits on the frequency",
                STEADY_TONE.as_secs()
            )
        })
    }

    /// Slices since `from` that are a steady tone at the pitch, at `floor` or over:
    /// (slices, steady ones).
    fn steady_count(&self, from: f64, floor: f32) -> (usize, usize) {
        let (mut n, mut steady) = (0usize, 0usize);
        let mut prev: Option<f32> = None;
        for s in self.slices.iter().filter(|s| s.t >= from) {
            n += 1;
            let holds = prev.is_none_or(|p| (s.tone_db - p).abs() <= STEADY_STEP_DB);
            if holds && s.purity >= MIN_PURITY && s.tone_db >= floor {
                steady += 1;
            }
            prev = Some(s.tone_db);
        }
        (n, steady)
    }

    /// A steady tone at the pitch over the last [`CARRIER_S`] of audio: its level;
    /// `None` if there is not that much audio yet to tell.
    fn carrier(&self) -> Option<Option<f32>> {
        let latest = self.latest()?;
        let from = latest - CARRIER_S;
        if self.slices.front()?.t > from + SLICE_S {
            return None;
        }
        let (n, steady) = self.steady_count(from, self.s.min_level_dbfs + OVER_MIN_LEVEL_DB);
        if n == 0 || (steady as f32) < CARRIER_SHARE * n as f32 {
            return Some(None);
        }
        let mut v: Vec<f32> = self
            .slices
            .iter()
            .filter(|s| s.t >= from)
            .map(|s| s.tone_db)
            .collect();
        Some(median(&mut v))
    }

    /// Audio arriving, the band's level, and a carrier at the pitch.
    pub fn band(&mut self, now: Instant) -> Band {
        let audio = self
            .last_block
            .is_some_and(|b| now.saturating_duration_since(b) <= AUDIO_FRESH);
        // Only audio still arriving says how the band sounds now, and only once
        // there is enough of it to tell the band from a carrier.
        let carrier = self.carrier().filter(|_| audio);
        if let Some(db) = self.receive_level().filter(|_| carrier == Some(None)) {
            self.level = Some((now, db));
        }
        let carrier_db = carrier.flatten();
        let level_db = self
            .level
            .filter(|&(at, _)| now.saturating_duration_since(at) <= LEVEL_KEEPS)
            .map(|(_, db)| db);
        Band {
            audio,
            level_db,
            carrier_db,
        }
    }

    /// Median level over the last second of audio, if it is all receive audio. A
    /// steady tone at the pitch is not the band, so its slices are left out: a
    /// carrier, a key closed at the radio, or the field station's elements.
    fn receive_level(&self) -> Option<f32> {
        let latest = self.latest()?;
        let rx_from = self.runs.back().map_or(f64::NEG_INFINITY, |r| {
            r.start + r.open_at() + MAX_LAG_S + RX_AFTER_S
        });
        let from = (latest - LEVEL_S).max(rx_from);
        if latest - from < LEVEL_MIN_S {
            return None;
        }
        let mut v = Vec::new();
        let mut prev: Option<f32> = None;
        for s in self.slices.iter().filter(|s| s.t >= from) {
            let holds = prev.is_some_and(|p| (s.tone_db - p).abs() <= STEADY_STEP_DB);
            if !(holds && s.purity >= MIN_PURITY) {
                v.push(s.total_db);
            }
            prev = Some(s.tone_db);
        }
        if (v.len() as f64) * SLICE_S < LEVEL_MIN_S {
            return None;
        }
        median(&mut v)
    }

    /// The sidetone level of the last run heard, dBFS.
    pub fn sidetone_db(&self) -> Option<f32> {
        self.sidetone_db
    }

    /// Bring-up: keep the raw audio from now on (at most two minutes of it), for
    /// [`Monitor::pitch`]; or stop and drop it.
    pub fn record(&mut self, on: bool) {
        self.raw = on.then(|| (self.n + self.pending.len() as u64, Vec::new()));
    }

    /// The sidetone's pitch in run `id` (recorded, and judged): the frequency from
    /// 300 to 1200 Hz with the most power in its elements, to 5 Hz.
    pub fn pitch(&self, id: u64) -> Option<f32> {
        let (first, raw) = self.raw.as_ref()?;
        let run = self.runs.iter().find(|r| r.id == id)?;
        let lag = run.judge.as_ref()?.lag.as_secs_f64();
        let sr = f64::from(self.s.sample_rate);
        let open = run.open_at();
        // Each element's samples, inside its edges.
        let parts: Vec<&[f32]> = run
            .downs
            .iter()
            .filter(|&&(s, _)| s < open)
            .filter_map(|&(s, e)| {
                // Sample index of radio time `t` into the run, at its delay.
                let at = |t: f64| ((run.start + lag + t - self.base) * sr) as i64 - *first as i64;
                let (a, b) = (at(s + 0.015), at(e.min(open) - 0.015));
                (a >= 0 && b > a && (b as usize) <= raw.len()).then(|| &raw[a as usize..b as usize])
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        let mut best: Option<(f32, f32)> = None;
        for step in 0..=180 {
            let f = 300.0 + 5.0 * step as f32;
            let coeff = 2.0 * (2.0 * std::f32::consts::PI * f / self.s.sample_rate as f32).cos();
            let p: f32 = parts.iter().map(|x| measure(x, coeff, 0.0).purity).sum();
            if best.is_none_or(|(_, b)| p > b) {
                best = Some((f, p));
            }
        }
        best.map(|(f, _)| f)
    }

    /// The longest unbroken stretch of sidetone (as loud as in the last run heard,
    /// less 10 dB) since `from`: how long a key stayed closed at the radio.
    pub fn longest_tone(&self, from: Instant) -> Duration {
        let from = self.radio(from);
        let floor = match self.sidetone_db {
            Some(db) => db - BELOW_SIDETONE_DB,
            None => self.s.min_level_dbfs + OVER_MIN_LEVEL_DB,
        };
        let (mut run, mut longest) = (0usize, 0usize);
        for s in self.slices.iter().filter(|s| s.t >= from) {
            if s.tone_db >= floor {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        Duration::from_secs_f64(longest as f64 * SLICE_S)
    }
}

/// Goertzel at the pitch over one slice.
fn measure(x: &[f32], coeff: f32, t: f64) -> Slice {
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
    let tone = 2.0 * mag_sq / (n * n);
    let total = sum_sq / n;
    Slice {
        t,
        tone_db: db(tone),
        total_db: db(total),
        purity: if total > 0.0 {
            (tone / total).min(1.0)
        } else {
            0.0
        },
    }
}

fn db(p: f32) -> f32 {
    10.0 * (p + 1e-12).log10()
}

fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

/// Median level at the pitch over the receive audio just before `start`.
fn median_before(slices: &VecDeque<Slice>, start: f64) -> Option<f32> {
    let mut v: Vec<f32> = slices
        .iter()
        .filter(|s| s.t >= start - PRE_S && s.t <= start - EDGE_S)
        .map(|s| s.tone_db)
        .collect();
    median(&mut v)
}

/// Where a slice falls in a run, at some delay.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    Down,
    Up,
    Before,
    Neither,
}

fn part(run: &Run, open: f64, post: f64, rel: f64) -> Part {
    if rel < -PRE_S {
        return Part::Neither;
    }
    if rel <= -EDGE_S {
        return Part::Before;
    }
    if rel > open + post {
        return Part::Neither;
    }
    if rel >= open + EDGE_S {
        return Part::Up;
    }
    // Inside the run: the key-down stretch holding `rel`, or the gap.
    let i = run.downs.partition_point(|&(_, e)| e <= rel);
    let near = |t: f64| (rel - t).abs() < EDGE_S;
    match run.downs.get(i) {
        Some(&(s, e)) if rel >= s => {
            if near(s) || near(e.min(open)) {
                Part::Neither
            } else {
                Part::Down
            }
        }
        next => {
            let prev_end = i.checked_sub(1).map_or(0.0, |p| run.downs[p].1);
            let next_start = next.map_or(open, |&(s, _)| s.min(open));
            if near(prev_end) || near(next_start) {
                Part::Neither
            } else {
                Part::Up
            }
        }
    }
}

fn judge_run(slices: &VecDeque<Slice>, run: &Run, post: f64) -> Judge {
    let open = run.open_at();
    let lo = run.start - PRE_S;
    let hi = run.start + open + post + MAX_LAG_S;
    let near: Vec<&Slice> = slices.iter().filter(|s| s.t >= lo && s.t <= hi).collect();
    let mut best: Option<(f32, f32, Judge)> = None;
    let steps = (MAX_LAG_S / LAG_STEP_S).round() as usize;
    for step in 0..=steps {
        let lag = step as f64 * LAG_STEP_S;
        // The audio before the run is scored on its own: with semi break-in the
        // receiver is muted in the run's gaps but not before it, so band noise there
        // may sit over the midway level and still be no sidetone.
        let (mut down, mut up, mut before) = (Vec::new(), Vec::new(), Vec::new());
        for s in &near {
            match part(run, open, post, s.t - run.start - lag) {
                Part::Down => down.push(s.tone_db),
                Part::Up => up.push(s.tone_db),
                Part::Before => before.push(s.tone_db),
                Part::Neither => {}
            }
        }
        let (nd, nu) = (down.len(), up.len());
        let (Some(tone), Some(gap)) = (median(&mut down), median(&mut up)) else {
            continue;
        };
        let mid = (tone + gap) / 2.0;
        let share_down = down.iter().filter(|&&v| v >= mid).count() as f32 / nd as f32;
        let share_up = up.iter().filter(|&&v| v < mid).count() as f32 / nu as f32;
        let share_before = if before.is_empty() {
            1.0
        } else {
            before
                .iter()
                .filter(|&&v| v < tone - MIN_CONTRAST_DB)
                .count() as f32
                / before.len() as f32
        };
        let j = Judge {
            heard: nd >= MIN_SLICES
                && nu >= MIN_SLICES
                && tone - gap >= MIN_CONTRAST_DB
                && share_down >= MIN_SHARE
                && share_up >= MIN_SHARE
                && share_before >= MIN_SHARE,
            lag: Duration::from_secs_f64(lag),
            tone_db: tone,
            gap_db: gap,
            before_db: median(&mut before),
            share_down,
            share_up,
            share_before,
            slices_down: nd,
            slices_up: nu,
        };
        let fit = share_down.min(share_up).min(share_before);
        let better = match &best {
            None => true,
            Some((f, c, b)) => {
                (j.heard && !b.heard)
                    || (j.heard == b.heard
                        && (fit > *f + 1e-6 || ((fit - *f).abs() <= 1e-6 && tone - gap > *c)))
            }
        };
        if better {
            best = Some((fit, tone - gap, j));
        }
    }
    best.map(|(_, _, j)| j).unwrap_or(Judge {
        heard: false,
        lag: Duration::ZERO,
        tone_db: f32::NEG_INFINITY,
        gap_db: f32::NEG_INFINITY,
        before_db: None,
        share_down: 0.0,
        share_up: 0.0,
        share_before: 0.0,
        slices_down: 0,
        slices_up: 0,
    })
}

#[cfg(test)]
mod tests;
