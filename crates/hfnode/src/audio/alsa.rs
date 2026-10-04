//! Capture through ALSA's `arecord`, on Linux.

use super::{Blocker, InputDevice};
use anyhow::{bail, Context, Result};
use std::io::Read;
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
    let out = Command::new("arecord")
        .arg("-L")
        .output()
        .context("running `arecord -L` (is alsa-utils installed?)")?;
    if !out.status.success() {
        bail!(
            "`arecord -L` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
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
