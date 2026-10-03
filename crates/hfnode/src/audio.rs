//! Audio in: the radio's USB codec via ALSA `arecord`, or WAV files for testing.
//!
//! `arecord` (alsa-utils, installed on Raspberry Pi OS) is used instead of linking
//! ALSA directly, which keeps the build free of native dependencies and makes the
//! capture device easy to check by hand: `arecord -L` lists devices and
//! `arecord -D <device> -f S16_LE -r 8000 -c 1 test.wav` records a sample.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
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

/// Live capture. Dropping it stops `arecord`.
pub struct Capture {
    child: Child,
    pub samples: BlockReceiver,
}

impl Capture {
    /// Start capturing mono 16-bit audio at `sample_rate` from ALSA `device`.
    /// Samples arrive in blocks of about 50 ms; if they are not taken, only the
    /// newest 10 s are kept.
    pub fn start(device: &str, sample_rate: u32) -> Result<Self> {
        let mut child = Command::new("arecord")
            .args([
                "-q", "-D", device, "-f", "S16_LE", "-c", "1", "-t", "raw", "-r",
            ])
            .arg(sample_rate.to_string())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting arecord (is alsa-utils installed?)")?;
        let mut out = child.stdout.take().expect("piped stdout");
        let (tx, rx) = queue(CAPTURE_BLOCKS);
        let block = (sample_rate as usize / 20).max(1) * 2;
        thread::spawn(move || {
            let mut buf = vec![0u8; block];
            loop {
                if out.read_exact(&mut buf).is_err() {
                    log::error!("audio capture ended");
                    break;
                }
                let at = Instant::now();
                let samples = buf
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
                if tx.send(Block { at, samples }).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, samples: rx })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
