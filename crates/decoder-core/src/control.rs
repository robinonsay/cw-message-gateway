//! The firmware's loop, one pass at a time: [`Control::pass`] takes the ADC's
//! samples through the [`Frontend`] into the decoder and the monitor's
//! measurements, answers the commands that arrived, and queues what the box says
//! for USB, against [`Hardware`], the board. firmware/pico2-decoder calls it in a
//! loop that never waits; the tests here run it against a board in memory.
//!
//! Two modes (docs/decoder-protocol.md):
//!
//! - **text**, from power-up: the decoded characters as plain text, a space
//!   between words and a new line after [`NEWLINE_MS`] of quiet. Any serial
//!   terminal shows them.
//! - **hfnode**, after `HELLO`: framed lines only. `CH` carries the characters
//!   decoded, `ST` the decoder's state every [`BLOCK`] samples, `SL` the monitor's
//!   10 ms measurements, `GAP` any audio lost. Every one is stamped with the count
//!   of samples at [`RATE`] the box has taken so far, so that hfnode can place it
//!   in time. The box goes back to text mode on `TEXT`, when USB goes away, or when
//!   no valid line has arrived for [`LINK_TIMEOUT_MS`].
//!
//! Commands are framed lines, as the keyer box's are (`keyer_core::frame`), and are
//! answered in either mode.
//!
//! Each pass, in this order:
//!
//! 1. service USB and read the clock; if the USB link went away since the last
//!    pass, go back to text mode;
//! 2. take the lines that arrived and act on them, queueing the replies;
//! 3. in hfnode mode, go back to text mode if hfnode has gone quiet;
//! 4. take up to [`ADC_PER_PASS`] samples from the ADC through the frontend into
//!    the decoder (unless `HOLD`) and the monitor, queueing what they produce;
//! 5. send what fits;
//! 6. drive GP16-GP18 low again, and the LED on while the decoder hears a tone;
//! 7. feed the watchdog, unless the ADC's DMA has halted: then the watchdog resets
//!    the box.

use crate::frontend::Frontend;
use crate::{
    BANDWIDTH_HZ, BLOCK, DEFAULT_BANDWIDTH_HZ, DEFAULT_PITCH_HZ, LINK_TIMEOUT_MS, MAX_LINE, NAME,
    NEWLINE_MS, PITCH_HZ, RATE, SLICE, SLICES_PER_LINE, VERSION,
};
use core::fmt;
use cw::level::{self, db};
use cw::{Decoder, DecoderConfig, Event};
use keyer_core::frame::{self, LineReader};

/// The board, as the loop uses it.
pub trait Hardware {
    /// Milliseconds since the box started.
    fn now_ms(&self) -> u64;
    /// Service the USB controller.
    fn usb_poll(&mut self);
    /// A count that changes each time the USB link went away: a bus reset,
    /// suspend (which is also how a pulled cable looks), or the host deconfiguring
    /// it.
    fn link_epoch(&self) -> u32;
    /// Bytes received, into `buf`: how many.
    fn usb_read(&mut self, buf: &mut [u8]) -> usize;
    /// Queue `bytes` for the host: how many were taken.
    fn usb_write(&mut self, bytes: &[u8]) -> usize;
    /// ADC FIFO entries that have arrived, oldest first, into `buf`: how many; or
    /// why audio was lost.
    fn adc_read(&mut self, buf: &mut [u16]) -> Result<usize, AudioLoss>;
    /// Drive the keyer box's key, PTT and tone pins (GP16, GP17, GP18) low, as
    /// they have been since boot. Nothing ever drives them high.
    fn hold_outputs_low(&mut self);
    fn set_led(&mut self, on: bool);
    fn feed_watchdog(&mut self);
}

/// Why audio was lost (`GAP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioLoss {
    /// The loop fell a whole ring behind the ADC: samples were overwritten before
    /// they were read.
    Overrun,
    /// The ADC's own FIFO filled before the DMA emptied it.
    Fifo,
    /// The DMA channel halted on a bus error: no more audio will come. The box
    /// stops feeding its watchdog, which restarts it.
    Halted,
}

impl AudioLoss {
    fn word(self) -> &'static str {
        match self {
            Self::Overrun => "OVERRUN",
            Self::Fifo => "FIFO",
            Self::Halted => "HALTED",
        }
    }
}

/// Why the box started, in `HELLO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// Power-up, the RUN pin, or a debugger.
    Power,
    /// Its watchdog fired.
    Watchdog,
    Other,
}

impl Boot {
    fn word(self) -> &'static str {
        match self {
            Self::Power => "POWER",
            Self::Watchdog => "WATCHDOG",
            Self::Other => "OTHER",
        }
    }
}

/// What the box decodes and measures at (`CONFIG`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The radio's CW pitch: the tone the decoder listens for.
    pub pitch_hz: u32,
    /// The decoder's filter bandwidth.
    pub bandwidth_hz: u32,
    /// The radio's sidetone pitch: where the monitor's measurements look.
    pub monitor_hz: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            pitch_hz: DEFAULT_PITCH_HZ,
            bandwidth_hz: DEFAULT_BANDWIDTH_HZ,
            monitor_hz: DEFAULT_PITCH_HZ,
        }
    }
}

impl Settings {
    fn decoder_config(&self) -> DecoderConfig {
        let mut c = DecoderConfig::new(RATE, self.pitch_hz as f32);
        c.bandwidth_hz = self.bandwidth_hz as f32;
        c
    }
}

/// ADC samples taken per pass at most: 4 ms at 32 kHz, so that no pass runs long
/// (the USB driver wants a pass every millisecond or so) while the loop still
/// catches up at once after a slow one.
pub const ADC_PER_PASS: usize = 128;

/// Bytes waiting for USB at most. A line that does not fit whole is dropped and
/// counted (`STATUS`); in text mode a character that does not fit is dropped.
pub const OUT_BYTES: usize = 4096;

/// Characters of decoded text one `CH` line carries at most.
const CH_TEXT: usize = 60;

/// Decoded events held between the decoder and the lines: more than one word can
/// produce (cw's `WORD_RUNS` key-down and key-up runs make at most 256
/// characters, and a word gap).
const EVENTS: usize = 600;

/// The id of every line the box sends on its own. hfnode never uses it.
const OWN: u8 = 0;

/// The box, between passes.
pub struct Control {
    boot: Boot,
    build: &'static str,
    hfnode: bool,
    settings: Settings,
    decoder: Decoder,
    frontend: Frontend,
    coeff: f32,
    /// `HOLD`: the decoder is not fed (the monitor still is).
    hold: bool,
    /// Samples at [`RATE`] taken so far.
    n: u64,
    slice: [f32; SLICE],
    slice_len: usize,
    /// The monitor's measurements not sent yet, in tenths of a dB (tone, total),
    /// and the sample count the first one starts at.
    slices: [(i32, i32); SLICES_PER_LINE],
    slices_len: usize,
    slices_from: u64,
    /// Samples until the next `ST`.
    block_left: usize,
    /// Decoded, not yet sent: characters, `_` for a word gap, `*` for a pattern not
    /// in the Morse table.
    events: [u8; EVENTS],
    events_len: usize,
    reader: LineReader,
    epoch: u32,
    /// When the last valid line arrived.
    heard_ms: u64,
    out: Out,
    /// Text mode: the last character written was a space, or nothing has been
    /// written since the last new line.
    text_spaced: bool,
    /// Text mode: something has been written since the last new line.
    text_dirty: bool,
    gaps: u32,
    halted: bool,
}

impl Control {
    /// A box that started for `boot`, running firmware `build`, its link as `hw`
    /// sees it now.
    pub fn new(boot: Boot, build: &'static str, hw: &impl Hardware) -> Self {
        let settings = Settings::default();
        Self {
            boot,
            build,
            hfnode: false,
            settings,
            decoder: Decoder::new(settings.decoder_config()),
            frontend: Frontend::new(),
            coeff: level::goertzel_coeff(settings.monitor_hz as f32, RATE),
            hold: false,
            n: 0,
            slice: [0.0; SLICE],
            slice_len: 0,
            slices: [(0, 0); SLICES_PER_LINE],
            slices_len: 0,
            slices_from: 0,
            block_left: BLOCK,
            events: [0; EVENTS],
            events_len: 0,
            reader: LineReader::new(),
            epoch: hw.link_epoch(),
            heard_ms: hw.now_ms(),
            out: Out::new(),
            text_spaced: true,
            text_dirty: false,
            gaps: 0,
            halted: false,
        }
    }

    /// One pass of the loop; see the module documentation.
    pub fn pass(&mut self, hw: &mut impl Hardware) {
        hw.usb_poll();
        let now = hw.now_ms();
        let epoch = hw.link_epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.reader.clear();
            self.hfnode = false;
        }

        let mut buf = [0u8; 64];
        let mut line = [0u8; MAX_LINE + 1];
        loop {
            let got = hw.usb_read(&mut buf);
            if got == 0 {
                break;
            }
            for &b in &buf[..got.min(buf.len())] {
                let Some(l) = self.reader.push(b) else {
                    continue;
                };
                let len = l.len();
                line[..len].copy_from_slice(l);
                if let Ok((id, body)) = frame::decode(&line[..len]) {
                    self.heard_ms = now;
                    self.command(id, body, now);
                }
            }
        }
        if self.hfnode && now.saturating_sub(self.heard_ms) >= LINK_TIMEOUT_MS {
            self.hfnode = false;
        }

        if !self.halted {
            let mut raw = [0u16; ADC_PER_PASS];
            match hw.adc_read(&mut raw) {
                Ok(got) => {
                    let mut x = [0.0f32; ADC_PER_PASS];
                    let mut k = 0;
                    for &r in &raw[..got.min(raw.len())] {
                        if let Some(y) = self.frontend.push(r) {
                            x[k] = y;
                            k += 1;
                        }
                    }
                    self.audio(&x[..k]);
                }
                Err(loss) => {
                    self.gaps = self.gaps.saturating_add(1);
                    self.halted = loss == AudioLoss::Halted;
                    if self.hfnode {
                        self.out
                            .line(OWN, format_args!("GAP {} {}", self.n, loss.word()));
                    }
                }
            }
        }

        self.out.send(hw);
        hw.hold_outputs_low();
        hw.set_led(self.decoder.key_down());
        if !self.halted {
            hw.feed_watchdog();
        }
    }

    /// Decoded samples, at [`RATE`]: into the decoder and the monitor, `ST` at each
    /// block's end.
    fn audio(&mut self, mut x: &[f32]) {
        while !x.is_empty() {
            let take = x.len().min(self.block_left);
            let (now, rest) = x.split_at(take);
            x = rest;
            if !self.hold {
                let (events, len) = (&mut self.events, &mut self.events_len);
                self.decoder.push_with(now, |e| keep_event(events, len, e));
            }
            for (i, &v) in now.iter().enumerate() {
                self.measure(v, self.n + i as u64);
            }
            self.n += take as u64;
            self.block_left -= take;
            self.send_events();
            if self.block_left == 0 {
                self.block_left = BLOCK;
                self.block_end();
            }
        }
    }

    /// One sample, the `index`th since the start (from 0), into the monitor's
    /// current 10 ms measurement.
    fn measure(&mut self, v: f32, index: u64) {
        self.slice[self.slice_len] = v;
        self.slice_len += 1;
        if self.slice_len < SLICE {
            return;
        }
        self.slice_len = 0;
        let l = level::measure(&self.slice, self.coeff);
        if self.slices_len == 0 {
            // The samples taken before this measurement's first.
            self.slices_from = index + 1 - SLICE as u64;
        }
        self.slices[self.slices_len] = (tenths(db(l.tone)), tenths(db(l.total)));
        self.slices_len += 1;
        if self.slices_len == SLICES_PER_LINE {
            self.slices_len = 0;
            if self.hfnode {
                let s = self.slices;
                self.out.line(
                    OWN,
                    format_args!(
                        "SL {} {},{} {},{} {},{} {},{} {},{}",
                        self.slices_from,
                        s[0].0,
                        s[0].1,
                        s[1].0,
                        s[1].1,
                        s[2].0,
                        s[2].1,
                        s[3].0,
                        s[3].1,
                        s[4].0,
                        s[4].1
                    ),
                );
            }
        }
    }

    /// The end of a block: `ST` in hfnode mode; in text mode, a new line once the
    /// decoder has been quiet long enough.
    fn block_end(&mut self) {
        let d = &self.decoder;
        if self.hfnode {
            let (key, partial, idle) = (d.key_down() as u8, d.has_partial() as u8, d.idle_ms());
            let wpm = tenths(d.wpm());
            self.out.line(
                OWN,
                format_args!("ST {} {key} {partial} {idle} {wpm}", self.n),
            );
        } else if self.text_dirty
            && !d.key_down()
            && !d.has_partial()
            && d.idle_ms() >= u64::from(NEWLINE_MS)
            && self.out.push(b"\r\n")
        {
            self.text_dirty = false;
            self.text_spaced = true;
        }
    }

    /// Whatever has been decoded: `CH` lines in hfnode mode, text in text mode.
    fn send_events(&mut self) {
        let len = core::mem::take(&mut self.events_len);
        if len == 0 {
            return;
        }
        let events = self.events;
        if self.hfnode {
            for chunk in events[..len].chunks(CH_TEXT) {
                // Printable ASCII only (`keep_event`).
                let text = core::str::from_utf8(chunk).unwrap_or("");
                self.out.line(OWN, format_args!("CH {} {text}", self.n));
            }
            return;
        }
        for &e in &events[..len] {
            let c = match e {
                b'_' if self.text_spaced => continue,
                b'_' => b' ',
                c => c,
            };
            if self.out.push(&[c]) {
                self.text_spaced = c == b' ';
                self.text_dirty = true;
            }
        }
    }

    fn command(&mut self, id: u8, body: &str, now: u64) {
        let mut words = body.split(' ');
        let word = words.next().unwrap_or("");
        let n = self.n;
        match (word, words.next(), words.next(), words.next(), words.next()) {
            ("HELLO", None, ..) => {
                self.hfnode = true;
                let (boot, build) = (self.boot.word(), self.build);
                self.out.line(
                    id,
                    format_args!(
                        "OK HELLO {VERSION} {} {RATE} {SLICE} {now} {boot} {build} {NAME}",
                        crate::ADC_RATE
                    ),
                );
            }
            ("STATUS", None, ..) => {
                let s = self.settings;
                let mode = if self.hfnode { "HFNODE" } else { "TEXT" };
                let (hold, lost, gaps, errors) = (
                    self.hold as u8,
                    self.out.lost,
                    self.gaps,
                    self.frontend.errors(),
                );
                self.out.line(
                    id,
                    format_args!(
                        "OK STATUS {mode} {hold} {} {} {} {n} {lost} {gaps} {errors}",
                        s.pitch_hz, s.bandwidth_hz, s.monitor_hz
                    ),
                );
            }
            ("CONFIG", Some(p), Some(b), Some(m), None) => {
                let (Ok(p), Ok(b), Ok(m)) = (p.parse(), b.parse(), m.parse()) else {
                    self.out.line(id, format_args!("ERR CONFIG ARGS"));
                    return;
                };
                let s = Settings {
                    pitch_hz: p,
                    bandwidth_hz: b,
                    monitor_hz: m,
                };
                if !PITCH_HZ.contains(&p)
                    || !BANDWIDTH_HZ.contains(&b)
                    || !PITCH_HZ.contains(&m)
                    || s.decoder_config().check().is_err()
                {
                    self.out.line(id, format_args!("ERR CONFIG RANGE"));
                    return;
                }
                self.settings = s;
                self.coeff = level::goertzel_coeff(m as f32, RATE);
                self.new_decoder();
                self.out.line(id, format_args!("OK CONFIG {n}"));
            }
            ("NEW", None, ..) => {
                self.new_decoder();
                self.out.line(id, format_args!("OK NEW {n}"));
            }
            ("FLUSH", None, ..) => {
                let (events, len) = (&mut self.events, &mut self.events_len);
                self.decoder.flush_with(|e| keep_event(events, len, e));
                self.send_events();
                self.out.line(id, format_args!("OK FLUSH {n}"));
            }
            ("RESET", Some("LEVELS"), None, ..) => {
                self.decoder.reset_levels();
                self.out.line(id, format_args!("OK RESET LEVELS {n}"));
            }
            ("RESET", Some("SPEED"), None, ..) => {
                self.decoder.reset_speed();
                self.out.line(id, format_args!("OK RESET SPEED {n}"));
            }
            ("HOLD", None, ..) => {
                self.hold = true;
                self.out.line(id, format_args!("OK HOLD {n}"));
            }
            ("RESUME", None, ..) => {
                // What the decoder heard before the hold no longer matches the
                // levels after it: the node transmitted in between.
                self.decoder.reset_levels();
                self.hold = false;
                self.out.line(id, format_args!("OK RESUME {n}"));
            }
            ("TEXT", None, ..) => {
                self.out.line(id, format_args!("OK TEXT"));
                self.hfnode = false;
            }
            (
                "HELLO" | "STATUS" | "CONFIG" | "NEW" | "FLUSH" | "RESET" | "HOLD" | "RESUME"
                | "TEXT",
                ..,
            ) => self.out.line(id, format_args!("ERR {word} ARGS")),
            _ => {
                // The word as received is printable ASCII; keep the reply a line.
                let word = word.get(..word.len().min(16)).unwrap_or("");
                self.out.line(id, format_args!("ERR {word} UNKNOWN"));
            }
        }
    }

    /// A fresh decoder with the current settings, as hfnode starts one at each
    /// listening window: what the old one was in the middle of is dropped.
    fn new_decoder(&mut self) {
        self.decoder = Decoder::new(self.settings.decoder_config());
        self.events_len = 0;
    }

    /// Whether the box is in hfnode mode.
    pub fn is_hfnode(&self) -> bool {
        self.hfnode
    }

    /// Samples at [`RATE`] taken so far.
    pub fn samples(&self) -> u64 {
        self.n
    }

    /// The decoder, as the loop runs it.
    pub fn decoder(&self) -> &Decoder {
        &self.decoder
    }

    /// Lines dropped because the USB queue was full.
    pub fn lost_lines(&self) -> u32 {
        self.out.lost
    }
}

/// One decoded event onto the end of `events`, if there is room.
fn keep_event(events: &mut [u8; EVENTS], len: &mut usize, e: Event<'_>) {
    let c = match e {
        Event::Char(c) if c.is_ascii_graphic() && c != '_' && c != '*' => c as u8,
        Event::Char(_) | Event::Unknown(_) => b'*',
        Event::WordGap => b'_',
    };
    if *len < EVENTS {
        events[*len] = c;
        *len += 1;
    }
}

/// `x` in tenths, rounded to the nearest.
fn tenths(x: f32) -> i32 {
    libm::roundf(x * 10.0) as i32
}

/// Bytes waiting for USB.
struct Out {
    buf: [u8; OUT_BYTES],
    head: usize,
    len: usize,
    lost: u32,
}

impl Out {
    const fn new() -> Self {
        Self {
            buf: [0; OUT_BYTES],
            head: 0,
            len: 0,
            lost: 0,
        }
    }

    /// `bytes`, whole, if they fit.
    fn push(&mut self, bytes: &[u8]) -> bool {
        if OUT_BYTES - self.len < bytes.len() {
            return false;
        }
        for &b in bytes {
            self.buf[(self.head + self.len) % OUT_BYTES] = b;
            self.len += 1;
        }
        true
    }

    /// A line and its newline, whole, if they fit.
    fn push_line(&mut self, line: &[u8]) -> bool {
        if OUT_BYTES - self.len < line.len() + 1 {
            return false;
        }
        self.push(line) && self.push(b"\n")
    }

    /// One framed line, whole or not at all.
    fn line(&mut self, id: u8, body: fmt::Arguments<'_>) {
        match frame::encode(id, body) {
            Some(l) if self.push_line(l.as_bytes()) => {}
            _ => self.lost = self.lost.saturating_add(1),
        }
    }

    /// As much as USB takes.
    fn send(&mut self, hw: &mut impl Hardware) {
        while self.len > 0 {
            let run = self.len.min(OUT_BYTES - self.head);
            let took = hw.usb_write(&self.buf[self.head..self.head + run]).min(run);
            self.head = (self.head + took) % OUT_BYTES;
            self.len -= took;
            if took < run {
                break;
            }
        }
    }
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control")
            .field("hfnode", &self.hfnode)
            .field("settings", &self.settings)
            .field("hold", &self.hold)
            .field("n", &self.n)
            .field("gaps", &self.gaps)
            .field("lost", &self.out.lost)
            .field("halted", &self.halted)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
