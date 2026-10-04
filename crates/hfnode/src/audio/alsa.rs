//! Capture through ALSA's `arecord`, and playback to a handheld through `aplay`, on
//! Linux.

use super::{Blocker, InputDevice};
use crate::handheld::playback::{Playback, Playing};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;

pub const DEVICE_HINT: &str =
    "an ALSA capture device as `arecord -L` lists it, e.g. plughw:CARD=CODEC,DEV=0";

/// The running `arecord`. Dropping it stops it.
pub struct Source {
    child: Child,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Capture mono 16-bit audio at `sample_rate` from ALSA `device`.
pub fn start(device: &str, sample_rate: u32, mut blocker: Blocker) -> Result<Source> {
    let mut child = Command::new("arecord")
        .args([
            "-q", "-D", device, "-f", "S16_LE", "-c", "1", "-t", "raw", "-r",
        ])
        .arg(sample_rate.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .context("starting arecord (is alsa-utils installed?)")?;
    let mut out = child.stdout.take().expect("piped stdout");
    let block = blocker.block_len() * 2;
    thread::spawn(move || {
        let mut buf = vec![0u8; block];
        loop {
            if out.read_exact(&mut buf).is_err() {
                log::error!("audio capture ended");
                break;
            }
            let samples: Vec<f32> = buf
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b) as f32 / 32768.0)
                .collect();
            if !blocker.push(&samples) {
                break;
            }
        }
    });
    Ok(Source { child })
}

/// The capture devices `arecord -L` lists.
pub fn input_devices() -> Result<Vec<InputDevice>> {
    list_devices("arecord")
}

/// The playback devices `aplay -L` lists.
pub fn output_devices() -> Result<Vec<InputDevice>> {
    list_devices("aplay")
}

fn list_devices(program: &str) -> Result<Vec<InputDevice>> {
    let out = Command::new(program)
        .arg("-L")
        .output()
        .with_context(|| format!("running `{program} -L` (is alsa-utils installed?)"))?;
    if !out.status.success() {
        bail!(
            "`{program} -L` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

pub const OUTPUT_HINT: &str =
    "an ALSA playback device as `aplay -L` lists it, e.g. plughw:CARD=AllInOneCable,DEV=0";

/// Rate the tone is rendered at; `plughw` devices convert it.
const PLAY_RATE: u32 = 48_000;

/// A handheld's sound output, played through `aplay`: one `aplay` per keying run,
/// fed the whole run (lead-in, Morse, tail) and closed, so that it exits once the
/// last sample has been played out (`aplay` drains its buffer at the end).
pub struct Speaker {
    device: String,
}

impl Speaker {
    /// Check that `aplay` runs; the device itself is opened by each run.
    pub fn open(device: &str) -> Result<Self> {
        let ok = Command::new("aplay")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running `aplay` (is alsa-utils installed?)")?
            .success();
        if !ok {
            bail!("`aplay --version` failed");
        }
        Ok(Self {
            device: device.to_string(),
        })
    }
}

impl Playback for Speaker {
    fn rate(&self) -> u32 {
        PLAY_RATE
    }

    fn start(&self, samples: Vec<f32>) -> Result<Box<dyn Playing>> {
        let mut child = Command::new("aplay")
            .args(["-q", "-D"])
            .arg(&self.device)
            .args(["-t", "raw", "-f", "S16_LE", "-c", "1", "-r"])
            .arg(PLAY_RATE.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("starting aplay (is alsa-utils installed?)")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
            .collect();
        // Writing blocks while aplay plays; closing stdin (the end of this thread)
        // tells it the run is over. Killing aplay ends the write with an error.
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        Ok(Box::new(AplayRun {
            child,
            writer: Some(writer),
            done: false,
        }))
    }

    fn describe(&self) -> String {
        format!("{} (aplay)", self.device)
    }
}

struct AplayRun {
    child: Child,
    writer: Option<thread::JoinHandle<()>>,
    done: bool,
}

impl Playing for AplayRun {
    fn finished(&mut self) -> bool {
        if !self.done {
            match self.child.try_wait() {
                Ok(None) => return false,
                Ok(Some(status)) if !status.success() => {
                    log::error!("aplay ended with {status}");
                }
                Ok(Some(_)) => {}
                Err(e) => log::error!("waiting for aplay: {e}"),
            }
            self.done = true;
        }
        true
    }

    fn stop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.done = true;
        }
    }
}

impl Drop for AplayRun {
    fn drop(&mut self) {
        self.stop();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

/// `arecord -L` output: a device name at the start of a line, then indented lines
/// describing it.
fn parse_list(text: &str) -> Vec<InputDevice> {
    let mut devices: Vec<InputDevice> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let Some(d) = devices.last_mut() {
                if !d.detail.is_empty() {
                    d.detail.push_str("; ");
                }
                d.detail.push_str(line.trim());
            }
        } else {
            devices.push(InputDevice {
                name: line.trim().to_string(),
                detail: String::new(),
            });
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_arecord_device_list() {
        let text = "null\n    Discard all samples (playback) or generate zero samples (capture)\n\
                    default\n    Default ALSA Output\n\
                    plughw:CARD=CODEC,DEV=0\n    USB Audio CODEC, USB Audio\n    \
                    Hardware device with all software conversions\n";
        let d = parse_list(text);
        assert_eq!(d.len(), 3);
        assert_eq!(d[2].name, "plughw:CARD=CODEC,DEV=0");
        assert_eq!(
            d[2].detail,
            "USB Audio CODEC, USB Audio; Hardware device with all software conversions"
        );
        assert!(d[2].looks_like_radio());
        assert!(!d[1].looks_like_radio());
    }
}
