//! Capture through Core Audio (macOS) or WASAPI (Windows), with cpal.
//!
//! cpal is held at 0.16: from 0.17 its Core Audio backend links functions that only
//! exist from macOS 14.2 (for recording system output), so a build would not start
//! on an older Mac.
//!
//! The device is opened in its own default format, so that capturing never changes
//! the sound card's rate for other programs, and converted to mono at the decoder's
//! rate here. cpal streams cannot be moved between threads on every platform, so each
//! capture owns a thread that opens the stream, keeps it running and drops it.

use super::resample::Resampler;
use super::{pick_device, Blocker, InputDevice};
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::mpsc;
use std::thread;

pub const DEVICE_HINT: &str =
    "the input device's name, or part of it, as `hfnode devices` lists it, e.g. USB Audio CODEC";

/// The running capture. Dropping it stops it.
pub struct Source {
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Capture from the input device `device` names, converted to mono at `sample_rate`.
/// Fails here, not later, if the device cannot be found or started.
pub fn start(device: &str, sample_rate: u32, blocker: Blocker) -> Result<Source> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String>>(1);
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let fail = stop_tx.clone();
    let wanted = device.to_string();
    let thread = thread::Builder::new()
        .name("audio capture".into())
        .spawn(move || {
            let stream = match open(&wanted, sample_rate, blocker, fail) {
                Ok((stream, what)) => {
                    let _ = ready_tx.send(Ok(what));
                    stream
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Until the capture is dropped, or the stream fails. Dropping the stream
            // drops the queue's sender with it, so the node sees the audio end.
            let _ = stop_rx.recv();
            drop(stream);
        })
        .context("starting the audio capture thread")?;
    match ready_rx.recv() {
        Ok(Ok(what)) => {
            log::info!("audio: {what}");
            Ok(Source {
                stop: stop_tx,
                thread: Some(thread),
            })
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            bail!("the audio capture thread ended while starting")
        }
    }
}

fn find(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .map_err(|e| anyhow!("listing audio inputs: {e}"))?
        .collect();
    let names: Vec<String> = devices.iter().map(name_of).collect();
    let i = pick_device(&names, wanted)?;
    Ok(devices.into_iter().nth(i).expect("picked from this list"))
}

fn open(
    wanted: &str,
    sample_rate: u32,
    blocker: Blocker,
    fail: mpsc::Sender<()>,
) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = find(&host, wanted)?;
    let name = name_of(&device);
    let supported = device
        .default_input_config()
        .map_err(|e| anyhow!("reading the input format of {name:?}: {e}"))?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels);
    let rate = config.sample_rate.0;
    let what = format!(
        "{name:?} at {rate} Hz, {channels} channel(s), {format:?}, converted to {sample_rate} Hz mono"
    );
    let sink = Sink {
        channels,
        resampler: Resampler::new(rate, sample_rate),
        blocker,
        mono: Vec::new(),
        out: Vec::new(),
        fail: fail.clone(),
        ended: false,
    };
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, sink, fail),
        SampleFormat::I16 => build::<i16>(&device, &config, sink, fail),
        SampleFormat::I32 => build::<i32>(&device, &config, sink, fail),
        SampleFormat::I24 => build::<cpal::I24>(&device, &config, sink, fail),
        SampleFormat::U16 => build::<u16>(&device, &config, sink, fail),
        SampleFormat::F64 => build::<f64>(&device, &config, sink, fail),
        SampleFormat::I8 => build::<i8>(&device, &config, sink, fail),
        SampleFormat::U8 => build::<u8>(&device, &config, sink, fail),
        other => bail!("{name:?} delivers {other:?} samples, which hfnode does not read"),
    }
    .with_context(|| format!("opening {name:?} for capture"))?;
    stream
        .play()
        .map_err(|e| anyhow!("starting capture from {name:?}: {e}"))?;
    Ok((stream, what))
}

/// Where the sound card's callbacks go: to mono, to the decoder's rate, to blocks.
struct Sink {
    channels: usize,
    resampler: Resampler,
    blocker: Blocker,
    mono: Vec<f32>,
    out: Vec<f32>,
    fail: mpsc::Sender<()>,
    ended: bool,
}

impl Sink {
    fn take<T>(&mut self, data: &[T])
    where
        T: Sample,
        f32: FromSample<T>,
    {
        if self.ended {
            return;
        }
        let ch = self.channels.max(1);
        self.mono.clear();
        self.mono.extend(
            data.chunks_exact(ch)
                .map(|f| f.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() / ch as f32),
        );
        self.out.clear();
        self.resampler.process(&self.mono, &mut self.out);
        if !self.blocker.push(&self.out) {
            // Nobody is receiving: stop the stream.
            self.ended = true;
            let _ = self.fail.send(());
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sink: Sink,
    fail: mpsc::Sender<()>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| sink.take(data),
            move |e: cpal::StreamError| {
                log::error!("audio capture stopped: {e}");
                let _ = fail.send(());
            },
            None,
        )
        .map_err(|e| anyhow!("{e}"))
}

/// Every input device, with its default format.
pub fn input_devices() -> Result<Vec<InputDevice>> {
    let host = cpal::default_host();
    let default = host.default_input_device().map(|d| name_of(&d));
    let devices = host
        .input_devices()
        .map_err(|e| anyhow!("listing audio inputs: {e}"))?;
    Ok(devices
        .map(|d| {
            let name = name_of(&d);
            let mut detail = match d.default_input_config() {
                Ok(c) => format!(
                    "{} Hz, {} channel(s), {:?}",
                    c.sample_rate().0,
                    c.channels(),
                    c.sample_format()
                ),
                Err(e) => format!("format unknown: {e}"),
            };
            if default.as_deref() == Some(name.as_str()) {
                detail.push_str("; system default input");
            }
            InputDevice { name, detail }
        })
        .collect())
}

fn name_of(d: &cpal::Device) -> String {
    d.name().unwrap_or_else(|e| format!("(no name: {e})"))
}
