//! Audio in: the radio's USB codec via ALSA `arecord`, or WAV files for testing.
//!
//! `arecord` (alsa-utils, installed on Raspberry Pi OS) is used instead of linking
//! ALSA directly, which keeps the build free of native dependencies and makes the
//! capture device easy to check by hand: `arecord -L` lists devices and
//! `arecord -D <device> -f S16_LE -r 8000 -c 1 test.wav` records a sample.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// Live capture. Dropping it stops `arecord`.
pub struct Capture {
    child: Child,
    pub samples: Receiver<Vec<f32>>,
}

impl Capture {
    /// Start capturing mono 16-bit audio at `sample_rate` from ALSA `device`.
    /// Samples arrive in blocks of about 50 ms.
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
        let (tx, rx) = mpsc::sync_channel(200);
        let block = (sample_rate as usize / 20).max(1) * 2;
        thread::spawn(move || {
            let mut buf = vec![0u8; block];
            loop {
                if out.read_exact(&mut buf).is_err() {
                    log::error!("audio capture ended");
                    break;
                }
                let samples = buf
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
                if tx.send(samples).is_err() {
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
}
