//! Audio in: the radio's USB codec, or WAV files for testing.
//!
//! Live capture has one backend per platform, chosen when the program is built:
//!
//! - **Linux** (and other Unix systems): ALSA, through `arecord` (alsa-utils,
//!   installed on Raspberry Pi OS). Running it instead of linking ALSA keeps the build
//!   free of native dependencies and makes the capture device easy to check by hand:
//!   `arecord -L` lists devices and `arecord -D <device> -f S16_LE -r 8000 -c 1
//!   test.wav` records a sample. ALSA's `plughw` devices deliver the rate asked for.
//! - **macOS** (Core Audio) and **Windows** (WASAPI), through the cpal crate. The
//!   device is read in its own format (typically 44.1 or 48 kHz, stereo), so that
//!   capturing never changes the sound card's settings for anything else, and
//!   converted to mono at `audio.sample_rate` by [`resample`].
//!
//! Either way the node receives mono blocks of about 50 ms at `audio.sample_rate`,
//! each stamped with when it was captured.

mod resample;
pub use resample::{mono, Resampler};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "audio/alsa.rs"]
mod backend;
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[path = "audio/native.rs"]
mod backend;

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Blocks held for a busy reader: 10 s of 50 ms blocks.
const CAPTURE_BLOCKS: usize = 200;

/// A block of captured audio, stamped when it was read from the sound card.
#[derive(Debug, Clone)]
pub struct Block {
    pub at: Instant,
    pub samples: Vec<f32>,
}

#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

#[derive(Debug)]
struct Queue {
    blocks: VecDeque<Block>,
    capacity: usize,
    sender: bool,
    receiver: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A queue of at most `capacity` audio blocks whose sender never waits: when the
/// receiver falls behind (the node is busy transmitting) the oldest block is
/// dropped. A blocked capture thread would stall `arecord`, and its pipe would
/// then deliver stale audio long after it was captured.
pub fn queue(capacity: usize) -> (BlockSender, BlockReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            blocks: VecDeque::new(),
            capacity: capacity.max(1),
            sender: true,
            receiver: true,
        }),
        ready: Condvar::new(),
    });
    (BlockSender(shared.clone()), BlockReceiver(shared))
}

pub struct BlockSender(Arc<Shared>);

impl BlockSender {
    /// Queue `block`, dropping the oldest one if the queue is full. Fails, returning
    /// the block, once the receiver is gone.
    pub fn send(&self, block: Block) -> Result<(), Block> {
        let mut q = self.0.lock();
        if !q.receiver {
            return Err(block);
        }
        if q.blocks.len() >= q.capacity {
            q.blocks.pop_front();
        }
        q.blocks.push_back(block);
        self.0.ready.notify_one();
        Ok(())
    }

    /// Blocks queued and not yet taken by the receiver.
    pub fn queued(&self) -> usize {
        self.0.lock().blocks.len()
    }
}

impl Drop for BlockSender {
    fn drop(&mut self) {
        self.0.lock().sender = false;
        self.0.ready.notify_all();
    }
}

pub struct BlockReceiver(Arc<Shared>);

impl BlockReceiver {
    /// The oldest queued block, waiting up to `timeout` for one. `Disconnected` once
    /// the sender is gone and the queue is empty.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Block, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut q = self.0.lock();
        loop {
            if let Some(b) = q.blocks.pop_front() {
                return Ok(b);
            }
            if !q.sender {
                return Err(RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
            q = self
                .0
                .ready
                .wait_timeout(q, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The oldest queued block, waiting for one; `None` once the sender is gone.
    pub fn recv(&self) -> Option<Block> {
        loop {
            match self.recv_timeout(Duration::from_secs(60)) {
                Ok(b) => return Some(b),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

impl Drop for BlockReceiver {
    fn drop(&mut self) {
        self.0.lock().receiver = false;
    }
}

/// Live capture from the radio's sound card. Dropping it stops capturing.
pub struct Capture {
    pub samples: BlockReceiver,
    _source: backend::Source,
}

impl Capture {
    /// Start capturing from `device` (see [`DEVICE_HINT`]), delivered as mono at
    /// `sample_rate`. Samples arrive in blocks of about 50 ms; if they are not taken,
    /// only the newest 10 s are kept.
    pub fn start(device: &str, sample_rate: u32) -> Result<Self> {
        let (tx, rx) = queue(CAPTURE_BLOCKS);
        let source = backend::start(device, sample_rate, Blocker::new(tx, sample_rate, device))?;
        Ok(Self {
            samples: rx,
            _source: source,
        })
    }
}

/// What `audio.device` names on this platform.
pub const DEVICE_HINT: &str = backend::DEVICE_HINT;

/// An audio input this computer offers.
#[derive(Debug, Clone, PartialEq)]
pub struct InputDevice {
    /// What to put in `audio.device`.
    pub name: String,
    /// Description, format, whether it is the system default.
    pub detail: String,
}

impl InputDevice {
    /// Whether this looks like the IC-7300's USB codec ("USB Audio CODEC").
    pub fn looks_like_radio(&self) -> bool {
        let both = format!("{} {}", self.name, self.detail).to_ascii_lowercase();
        both.contains("usb audio codec") || both.contains("card=codec")
    }
}

/// The audio inputs this computer offers, as `audio.device` would name them.
pub fn input_devices() -> Result<Vec<InputDevice>> {
    backend::input_devices()
}

/// The one of `names` that `wanted` picks: the only exact match (ignoring case), or
/// else the only name containing it. Several matches is an error rather than a guess,
/// so that the node never decodes the wrong sound card (a built-in microphone, or a
/// second radio's codec with the same name).
pub fn pick_device(names: &[String], wanted: &str) -> Result<usize> {
    let w = wanted.trim().to_lowercase();
    if w.is_empty() {
        bail!("audio.device is empty; {DEVICE_HINT}");
    }
    let exact: Vec<usize> = (0..names.len())
        .filter(|&i| names[i].trim().to_lowercase() == w)
        .collect();
    let hits: Vec<usize> = if exact.is_empty() {
        (0..names.len())
            .filter(|&i| names[i].to_lowercase().contains(&w))
            .collect()
    } else {
        exact
    };
    let list = |ix: &mut dyn Iterator<Item = usize>| {
        ix.map(|i| format!("\n  {:?}", names[i]))
            .collect::<String>()
    };
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => {
            // A Linux (ALSA) name left in a config copied to a Mac or Windows PC.
            let alsa = ["hw:", "plughw:", "sysdefault:", "dsnoop:"]
                .iter()
                .any(|p| w.starts_with(p))
                || w.contains("card=");
            bail!(
                "no audio input matches {wanted:?}.{} Inputs on this computer:{}\n\
                 (is the radio on and its USB cable connected? `hfnode devices` lists them too)",
                if alsa {
                    " That is a Linux (ALSA) device name; here audio.device is the \
                     input's name or part of it, such as \"USB Audio CODEC\", which is \
                     also what it is when left out."
                } else {
                    ""
                },
                if names.is_empty() {
                    "\n  (none)".to_string()
                } else {
                    list(&mut (0..names.len()))
                }
            )
        }
        _ => bail!(
            "{wanted:?} matches more than one audio input; use more of the name, or \
             unplug the other device:{}",
            list(&mut hits.iter().copied())
        ),
    }
}

/// How long the start of a capture is watched for digital silence.
const SILENCE_CHECK_SECS: usize = 3;

/// Cuts captured mono audio, already at the decoder's rate, into blocks of about
/// 50 ms for the queue, stamping each with the time it was completed.
///
/// Also warns, once, if the first few seconds are exact zeros. A radio's audio always
/// carries some noise, so that means the audio is not reaching the node: on macOS a
/// program not allowed to use the microphone gets silence rather than an error (on
/// Windows it is normally refused with "access denied", reported when the capture
/// starts), and an AF output level of 0 does the same.
pub(crate) struct Blocker {
    tx: BlockSender,
    block: usize,
    pending: Vec<f32>,
    device: String,
    /// Samples left to watch for silence; 0 once done.
    watch: usize,
    heard: bool,
}

impl Blocker {
    pub(crate) fn new(tx: BlockSender, sample_rate: u32, device: &str) -> Self {
        Self {
            tx,
            block: (sample_rate as usize / 20).max(1),
            pending: Vec::new(),
            device: device.to_string(),
            watch: sample_rate as usize * SILENCE_CHECK_SECS,
            heard: false,
        }
    }

    /// Samples per block.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub(crate) fn block_len(&self) -> usize {
        self.block
    }

    /// Add captured samples. Returns false once nobody is receiving any more.
    pub(crate) fn push(&mut self, samples: &[f32]) -> bool {
        if self.watch > 0 {
            let n = samples.len().min(self.watch);
            self.heard |= samples[..n].iter().any(|&s| s != 0.0);
            self.watch -= n;
            if self.watch == 0 && !self.heard {
                log::warn!(
                    "audio from {:?} has been exact silence for {SILENCE_CHECK_SECS} s: the \
                     radio's audio is not reaching hfnode. On macOS allow microphone access \
                     for the program running hfnode (System Settings > Privacy & Security > \
                     Microphone); on Windows turn on Settings > Privacy & security > \
                     Microphone > Let desktop apps access your microphone; and check the \
                     radio's ACC/USB AF output level",
                    self.device
                );
            }
        }
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= self.block {
            let block = Block {
                at: Instant::now(),
                samples: self.pending[start..start + self.block].to_vec(),
            };
            start += self.block;
            if self.tx.send(block).is_err() {
                return false;
            }
        }
        self.pending.drain(..start);
        true
    }
}

/// Read a WAV file as mono f32 samples (channels are averaged). Returns the samples
/// and the sample rate.
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    if interleaved.is_empty() {
        bail!("{} has no audio", path.display());
    }
    let mono = interleaved
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    Ok((mono, spec.sample_rate))
}

pub fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn wav_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        let s: Vec<f32> = (0..800).map(|i| (i as f32 / 10.0).sin() * 0.5).collect();
        write_wav(&p, &s, 8000).unwrap();
        let (back, sr) = read_wav(&p).unwrap();
        assert_eq!(sr, 8000);
        assert!(back.iter().zip(&s).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    fn block(n: usize) -> Block {
        Block {
            at: Instant::now(),
            samples: vec![n as f32],
        }
    }

    #[test]
    fn full_queue_drops_oldest_without_waiting() {
        let (tx, rx) = queue(3);
        // Nobody is receiving: every send still returns at once.
        for n in 0..10 {
            tx.send(block(n)).unwrap();
        }
        let got: Vec<f32> = (0..3)
            .map(|_| rx.recv_timeout(Duration::ZERO).unwrap().samples[0])
            .collect();
        assert_eq!(got, [7.0, 8.0, 9.0]);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1)).unwrap_err(),
            RecvTimeoutError::Timeout
        );
    }

    #[test]
    fn queue_ends_with_sender_and_receiver() {
        let (tx, rx) = queue(4);
        tx.send(block(1)).unwrap();
        drop(tx);
        // What was queued is still delivered first.
        assert_eq!(rx.recv().unwrap().samples, [1.0]);
        assert!(rx.recv().is_none());

        let (tx, rx) = queue(4);
        drop(rx);
        assert!(tx.send(block(2)).is_err());
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn device_is_picked_by_exact_name_or_the_only_partial_match() {
        let n = names(&[
            "MacBook Pro Microphone",
            "USB Audio CODEC",
            "Microphone (USB Audio CODEC)",
        ]);
        // An exact match wins over the other name that contains it.
        assert_eq!(pick_device(&n, "usb audio codec").unwrap(), 1);
        assert_eq!(
            pick_device(&n, " Microphone (USB Audio CODEC) ").unwrap(),
            2
        );
        assert_eq!(pick_device(&n, "macbook").unwrap(), 0);
        assert!(pick_device(&n, "Microphone").is_err(), "two contain it");
        assert!(pick_device(&n, "  ").is_err());
    }

    #[test]
    fn two_inputs_with_the_same_name_are_refused_not_guessed() {
        let n = names(&["USB Audio CODEC", "Built-in Microphone", "USB Audio CODEC"]);
        let err = pick_device(&n, "USB Audio CODEC").unwrap_err().to_string();
        assert!(err.contains("more than one"), "{err}");
        let n = names(&[
            "Microphone (USB Audio CODEC)",
            "Microphone (2- USB Audio CODEC)",
        ]);
        assert!(pick_device(&n, "USB Audio CODEC").is_err());
        assert_eq!(pick_device(&n, "2- USB Audio CODEC").unwrap(), 1);
    }

    #[test]
    fn a_linux_device_name_on_another_system_says_so() {
        let n = names(&["USB Audio CODEC"]);
        let err = pick_device(&n, "plughw:CARD=CODEC,DEV=0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Linux (ALSA) device name"), "{err}");
        assert!(err.contains("\"USB Audio CODEC\""), "{err}");
        let err = pick_device(&n, "Line In").unwrap_err().to_string();
        assert!(!err.contains("ALSA"), "{err}");
        assert!(pick_device(&[], "USB")
            .unwrap_err()
            .to_string()
            .contains("(none)"));
    }

    #[test]
    fn silence_is_reported_once_and_noise_is_not() {
        // Exact zeros for the watched seconds, then the watch ends either way; the
        // blocks still flow.
        let (tx, rx) = queue(1000);
        let mut b = Blocker::new(tx, 1000, "test");
        assert!(b.push(&vec![0.0; 3000]));
        assert_eq!(b.watch, 0);
        assert!(!b.heard);
        let (tx2, _rx2) = queue(1000);
        let mut b2 = Blocker::new(tx2, 1000, "test");
        let mut noise = vec![0.0; 3000];
        noise[2999] = 1e-4;
        assert!(b2.push(&noise));
        assert!(b2.heard);
        let mut got = 0;
        while let Ok(blk) = rx.recv_timeout(Duration::from_millis(1)) {
            got += blk.samples.len();
        }
        assert_eq!(got, 3000);
    }

    #[test]
    fn receiver_wakes_for_a_block() {
        let (tx, rx) = queue(4);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send(block(5)).unwrap();
        });
        let b = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(b.samples, [5.0]);
        t.join().unwrap();
    }
}
