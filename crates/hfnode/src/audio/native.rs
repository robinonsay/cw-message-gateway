//! Capture through Core Audio (macOS) or WASAPI (Windows), with cpal; and playback
//! to a handheld ([`Speaker`]).
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
use crate::handheld::playback::{Playback, Playing};
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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
    let (devices, names) = no_panic(|| {
        let devices: Vec<cpal::Device> = host
            .input_devices()
            .map_err(|e| anyhow!("listing audio inputs: {e}"))?
            .collect();
        let names: Vec<String> = devices.iter().map(name_of).collect();
        Ok((devices, names))
    })?;
    let i = pick_device(&names, wanted)?;
    Ok(devices.into_iter().nth(i).expect("picked from this list"))
}

/// cpal's Windows backend panics, rather than returning an error, if a device goes
/// away while it is being listed or named (the radio switched off at that moment).
fn no_panic<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        bail!("listing the audio inputs failed (did a device go away just then?); try again")
    })
}

/// Windows refuses a program that may not use the microphone with "Access is
/// denied" (E_ACCESSDENIED, 0x80070005) when the device is opened; say which setting
/// that is. (macOS gives such a program silence instead, which `Blocker` reports.)
fn privacy_hint(err: &str) -> &'static str {
    let e = err.to_ascii_lowercase();
    let denied = e.contains("access is denied") || e.contains("80070005");
    if cfg!(windows) && denied {
        ". Windows is not letting hfnode use the radio's audio: turn on Settings > \
         Privacy & security > Microphone > Microphone access, and Let desktop apps \
         access your microphone"
    } else {
        ""
    }
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
    let supported = device.default_input_config().map_err(|e| {
        anyhow!(
            "reading the input format of {name:?}: {e}{}",
            privacy_hint(&e.to_string())
        )
    })?;
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
    .map_err(|e| {
        anyhow!(
            "opening {name:?} for capture: {e:#}{}",
            privacy_hint(&e.to_string())
        )
    })?;
    stream.play().map_err(|e| {
        anyhow!(
            "starting capture from {name:?}: {e}{}",
            privacy_hint(&e.to_string())
        )
    })?;
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
    no_panic(list_inputs)
}

fn list_inputs() -> Result<Vec<InputDevice>> {
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

pub const OUTPUT_HINT: &str =
    "the output device's name, or part of it, as `hfnode devices` lists it";

/// Every output device, with its default format.
pub fn output_devices() -> Result<Vec<InputDevice>> {
    no_panic(|| {
        let host = cpal::default_host();
        let default = host.default_output_device().map(|d| name_of(&d));
        let devices = host
            .output_devices()
            .map_err(|e| anyhow!("listing audio outputs: {e}"))?;
        Ok(devices
            .map(|d| {
                let name = name_of(&d);
                let mut detail = match d.default_output_config() {
                    Ok(c) => format!(
                        "{} Hz, {} channel(s), {:?}",
                        c.sample_rate().0,
                        c.channels(),
                        c.sample_format()
                    ),
                    Err(e) => format!("format unknown: {e}"),
                };
                if default.as_deref() == Some(name.as_str()) {
                    detail.push_str("; system default output");
                }
                InputDevice { name, detail }
            })
            .collect())
    })
}

fn find_output(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let (devices, names) = no_panic(|| {
        let devices: Vec<cpal::Device> = host
            .output_devices()
            .map_err(|e| anyhow!("listing audio outputs: {e}"))?
            .collect();
        let names: Vec<String> = devices.iter().map(name_of).collect();
        Ok((devices, names))
    })?;
    let i = pick_device(&names, wanted)?;
    Ok(devices.into_iter().nth(i).expect("picked from this list"))
}

/// Extra time allowed after the last sample has been handed to the device, beyond
/// the latency the device reports, before the run counts as played.
const DRAIN_MARGIN: Duration = Duration::from_millis(30);

/// A handheld's sound output: the device is opened for each keying run in its own
/// default format (so other programs' settings are left alone), fed the run
/// (lead-in, Morse, tail) on every channel, and closed once the last sample has
/// been played out.
pub struct Speaker {
    wanted: String,
    name: String,
    rate: u32,
}

impl Speaker {
    /// Find the output `wanted` names and read its rate. Plays nothing.
    pub fn open(wanted: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = find_output(&host, wanted)?;
        let name = name_of(&device);
        let config = device
            .default_output_config()
            .map_err(|e| anyhow!("reading the output format of {name:?}: {e}"))?;
        Ok(Self {
            wanted: wanted.to_string(),
            name,
            rate: config.sample_rate().0,
        })
    }
}

/// Where a run has got to, shared with the sound card's callback.
#[derive(Default)]
struct RunState {
    /// Samples handed to the device so far.
    pos: AtomicUsize,
    /// Latest output latency the device reported, in microseconds.
    latency_us: AtomicU64,
    failed: AtomicBool,
    finished: AtomicBool,
}

impl Playback for Speaker {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn start(&self, samples: Vec<f32>) -> Result<Box<dyn Playing>> {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(1);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let state = Arc::new(RunState::default());
        let (wanted, rate, run) = (self.wanted.clone(), self.rate, state.clone());
        // cpal streams cannot be moved between threads on every platform: one
        // thread opens the stream, keeps it playing and drops it.
        thread::Builder::new()
            .name("audio out".into())
            .spawn(move || {
                let len = samples.len();
                let stream = match play(&wanted, rate, samples, run.clone()) {
                    Ok(s) => {
                        let _ = ready_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        run.finished.store(true, Ordering::SeqCst);
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                loop {
                    match stop_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if run.failed.load(Ordering::SeqCst) {
                        break;
                    }
                    if run.pos.load(Ordering::SeqCst) >= len {
                        // Handed over; let the device play it out.
                        let latency = Duration::from_micros(run.latency_us.load(Ordering::SeqCst));
                        let _ = stop_rx.recv_timeout(latency + DRAIN_MARGIN);
                        break;
                    }
                }
                drop(stream);
                run.finished.store(true, Ordering::SeqCst);
            })
            .context("starting the audio output thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(CpalRun {
                stop: Some(stop_tx),
                state,
            })),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("the audio output thread ended while starting"),
        }
    }

    fn describe(&self) -> String {
        format!("{:?} at {} Hz", self.name, self.rate)
    }
}

fn play(wanted: &str, rate: u32, samples: Vec<f32>, run: Arc<RunState>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = find_output(&host, wanted)?;
    let name = name_of(&device);
    let supported = device
        .default_output_config()
        .map_err(|e| anyhow!("reading the output format of {name:?}: {e}"))?;
    let format = supported.sample_format();
    let config = supported.config();
    if config.sample_rate.0 != rate {
        bail!(
            "{name:?} changed its rate from {rate} to {} Hz; start hfnode again",
            config.sample_rate.0
        );
    }
    let stream = match format {
        SampleFormat::F32 => build_out::<f32>(&device, &config, samples, run),
        SampleFormat::I16 => build_out::<i16>(&device, &config, samples, run),
        SampleFormat::I32 => build_out::<i32>(&device, &config, samples, run),
        SampleFormat::I24 => build_out::<cpal::I24>(&device, &config, samples, run),
        SampleFormat::U16 => build_out::<u16>(&device, &config, samples, run),
        SampleFormat::F64 => build_out::<f64>(&device, &config, samples, run),
        SampleFormat::I8 => build_out::<i8>(&device, &config, samples, run),
        SampleFormat::U8 => build_out::<u8>(&device, &config, samples, run),
        other => bail!("{name:?} takes {other:?} samples, which hfnode does not write"),
    }
    .map_err(|e| anyhow!("opening {name:?} for output: {e:#}"))?;
    stream
        .play()
        .map_err(|e| anyhow!("starting output to {name:?}: {e}"))?;
    Ok(stream)
}

fn build_out<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Vec<f32>,
    run: Arc<RunState>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels).max(1);
    let failed = run.clone();
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                    run.latency_us
                        .store(latency.as_micros() as u64, Ordering::SeqCst);
                }
                let mut pos = run.pos.load(Ordering::SeqCst);
                for frame in data.chunks_mut(channels) {
                    let s = samples.get(pos).copied().unwrap_or(0.0);
                    pos = (pos + 1).min(samples.len());
                    frame.fill(T::from_sample(s));
                }
                run.pos.store(pos, Ordering::SeqCst);
            },
            move |e: cpal::StreamError| {
                log::error!("audio output stopped: {e}");
                failed.failed.store(true, Ordering::SeqCst);
            },
            None,
        )
        .map_err(|e| anyhow!("{e}"))
}

struct CpalRun {
    stop: Option<mpsc::Sender<()>>,
    state: Arc<RunState>,
}

impl Playing for CpalRun {
    fn finished(&mut self) -> bool {
        self.state.finished.load(Ordering::SeqCst)
    }

    /// Dropping the sender ends the output thread's wait at once.
    fn stop(&mut self) {
        self.stop.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_names_the_windows_microphone_setting() {
        let h = privacy_hint("Access is denied. (0x80070005)");
        assert_eq!(h.contains("Let desktop apps access"), cfg!(windows), "{h}");
        assert!(privacy_hint("The device is in use by another application").is_empty());
    }
}
