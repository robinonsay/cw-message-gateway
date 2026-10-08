//! The firmware's control loop, one pass at a time: [`Control::pass`] moves USB
//! bytes to and from the [`Keyer`], the PTT line into it, and its outputs onto the
//! key, tone and PTT pins, against [`Hardware`], the board. firmware/pico2-keyer
//! calls it in a loop that never waits; the tests here run it against a board in
//! memory and check every pass.
//!
//! Each pass, in this order:
//!
//! 1. service USB, then read the clock, then the PTT line;
//! 2. if the USB link went away since the last pass, end any run: the key and the
//!    PTT open and the tone stops on this same pass;
//! 3. take the lines that arrived, queueing their replies;
//! 4. bring the keyer up to now;
//! 5. check the pins ([`PinGuard`]): trip the box if the key or the tone has been
//!    on for the key-down limit, or the PTT down for the PTT limit (an off shorter
//!    than [`limits::MIN_GAP_MS`] does not break either), or if this pass came more
//!    than [`limits::SLOW_PASS_MS`] after the last with the key or the tone on.
//!    This times the pins by the loop's own clock readings, not by the keyer's
//!    timeline;
//! 6. drive the key, the tone and the PTT, in that order, then the LED;
//! 7. send what replies fit;
//! 8. feed the watchdog, once, unless `TEST HANG` has stopped the loop.
//!
//! After `TEST HANG` takes effect, a pass only reads the clock and holds the pins
//! as they were, servicing nothing and never feeding the watchdog, so that the
//! watchdog must reset the chip. If it has not done so [`limits::HANG_OPEN_MS`]
//! after the hang, or once the key or tone has been on for the key-down limit or
//! the PTT down for the PTT limit, the pass opens them all itself and holds them
//! open.

use crate::frame::LineReader;
use crate::keyer::{Keyer, Trip};
use crate::limits;

/// The board, as the control loop uses it.
pub trait Hardware {
    /// Milliseconds since the box started.
    fn now_ms(&self) -> u64;
    /// Service the USB controller.
    fn usb_poll(&mut self);
    /// A count that changes each time the USB link went away: a bus reset, suspend
    /// (which is also how a pulled cable looks), or the host deconfiguring it.
    fn link_epoch(&self) -> u32;
    /// Bytes received, into `buf`: how many.
    fn usb_read(&mut self, buf: &mut [u8]) -> usize;
    /// Queue `bytes` for the host: how many were taken.
    fn usb_write(&mut self, bytes: &[u8]) -> usize;
    /// Drive the key pin: `true` closes the radio's key.
    fn set_key(&mut self, down: bool);
    /// Start (`true`) or stop the tone into the handheld's microphone.
    fn set_tone(&mut self, on: bool);
    /// Drive the PTT pin: `true` closes the handheld's PTT.
    fn set_ptt(&mut self, down: bool);
    /// Read the PTT line: `true` (high) while the handheld's PTT contact is open.
    fn ptt_line(&mut self) -> bool;
    fn set_led(&mut self, on: bool);
    fn feed_watchdog(&mut self);
}

/// The LED flashes at this half-period while the box is tripped.
pub const TRIP_FLASH_MS: u64 = 250;

/// An output pin as the loop drives it, timed by the loop's clock readings: the key
/// and tone together (one is on only while the other is off), or the PTT.
#[derive(Debug, Clone)]
pub struct PinGuard {
    limit_ms: u64,
    /// A slow pass with the pin high is a fault: the key and the tone, whose
    /// elements are timed to the millisecond. Not the PTT, whose limit is a minute
    /// and which the watchdog still bounds.
    slow: bool,
    /// When the pin went high, bridging lows shorter than [`limits::MIN_GAP_MS`];
    /// `None` once it has been low that long.
    high_from: Option<u64>,
    /// When it went low, if it is low after being high.
    low_from: Option<u64>,
    /// When it was last driven.
    last: u64,
}

impl PinGuard {
    /// The key or tone pin, driven low at `now`, held to `key_down_ms`.
    pub fn new(now: u64, key_down_ms: u32) -> Self {
        Self {
            limit_ms: u64::from(key_down_ms),
            slow: true,
            high_from: None,
            low_from: None,
            last: now,
        }
    }

    /// The PTT pin, driven low at `now`, held to `ptt_ms`.
    pub fn ptt(now: u64, ptt_ms: u32) -> Self {
        Self {
            slow: false,
            ..Self::new(now, ptt_ms)
        }
    }

    /// Whether the pin is high now.
    pub fn high(&self) -> bool {
        self.high_from.is_some() && self.low_from.is_none()
    }

    /// When a pin high at `now` went high, bridging short lows.
    fn from(&self, now: u64) -> u64 {
        match (self.high_from, self.low_from) {
            (Some(h), None) => h,
            (Some(h), Some(l)) if now.saturating_sub(l) < u64::from(limits::MIN_GAP_MS) => h,
            _ => now,
        }
    }

    /// How long the pin has been high at `now`, bridging short lows; 0 if low.
    pub fn high_for(&self, now: u64) -> u64 {
        if self.high() {
            now.saturating_sub(self.from(now))
        } else {
            0
        }
    }

    /// Before the pin is driven `down` at `now`: why the box must trip instead, if
    /// it must.
    pub fn fault(&self, now: u64, down: bool) -> Option<Trip> {
        if self.slow
            && self.high()
            && now.saturating_sub(self.last) > u64::from(limits::SLOW_PASS_MS)
        {
            return Some(Trip::Slow);
        }
        (down && now.saturating_sub(self.from(now)) >= self.limit_ms).then_some(Trip::Pin)
    }

    /// Whether the pin has been high for its limit at `now`.
    fn over(&self, now: u64) -> bool {
        self.high_for(now) >= self.limit_ms
    }

    /// The pin was driven `down` at `now`.
    pub fn driven(&mut self, now: u64, down: bool) {
        if down {
            self.high_from = Some(self.from(now));
            self.low_from = None;
        } else if self.high() {
            self.low_from = Some(now);
        }
        self.last = self.last.max(now);
    }
}

/// Replies waiting for room in the USB driver's buffer. hfnode sends one command
/// and waits for its reply, so this rarely holds more than one line.
pub struct Outbox {
    buf: [u8; 256],
    len: usize,
}

impl Outbox {
    pub const fn new() -> Self {
        Self {
            buf: [0; 256],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Queue a line, adding its newline; a line that does not fit is dropped
    /// (hfnode times out and asks again, or stops).
    pub fn push(&mut self, line: &[u8]) {
        let end = self.len + line.len() + 1;
        if end <= self.buf.len() {
            self.buf[self.len..end - 1].copy_from_slice(line);
            self.buf[end - 1] = b'\n';
            self.len = end;
        }
    }

    /// Hand the USB driver what it takes.
    pub fn send(&mut self, hw: &mut impl Hardware) {
        if self.len == 0 {
            return;
        }
        let n = hw.usb_write(&self.buf[..self.len]).min(self.len);
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }
}

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

/// The outputs as the loop drives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outputs {
    pub key: bool,
    pub tone: bool,
    pub ptt: bool,
}

impl Outputs {
    pub const OFF: Self = Self {
        key: false,
        tone: false,
        ptt: false,
    };

    fn of(k: &Keyer) -> Self {
        Self {
            key: k.key_down(),
            tone: k.tone(),
            ptt: k.ptt(),
        }
    }

    fn drive(self, hw: &mut impl Hardware) {
        hw.set_key(self.key);
        hw.set_tone(self.tone);
        hw.set_ptt(self.ptt);
    }
}

/// The control loop's state. See the module documentation.
pub struct Control {
    keyer: Keyer,
    reader: LineReader,
    out: Outbox,
    epoch: u32,
    /// The key and the tone.
    guard: PinGuard,
    ptt_guard: PinGuard,
    /// The outputs as last driven.
    pins: Outputs,
    /// When `TEST HANG` stopped the loop.
    hung_at: Option<u64>,
}

impl Control {
    /// The loop for `keyer`, on `hw` with its key and PTT pins already driven low
    /// and its tone stopped.
    pub fn new(keyer: Keyer, hw: &impl Hardware) -> Self {
        let l = keyer.limits();
        let now = hw.now_ms();
        Self {
            keyer,
            reader: LineReader::new(),
            out: Outbox::new(),
            epoch: hw.link_epoch(),
            guard: PinGuard::new(now, l.key_down_ms),
            ptt_guard: PinGuard::ptt(now, l.ptt_ms),
            pins: Outputs::OFF,
            hung_at: None,
        }
    }

    /// The outputs as last driven.
    pub fn pins(&self) -> Outputs {
        self.pins
    }

    pub fn keyer(&self) -> &Keyer {
        &self.keyer
    }

    /// Whether `TEST HANG` has stopped the loop.
    pub fn hung(&self) -> bool {
        self.hung_at.is_some()
    }

    /// One pass of the loop.
    pub fn pass(&mut self, hw: &mut impl Hardware) {
        if let Some(at) = self.hung_at {
            self.hung_pass(hw, at);
            return;
        }
        hw.usb_poll();
        let now = hw.now_ms();
        let line = hw.ptt_line();
        self.keyer.set_line(line);

        let epoch = hw.link_epoch();
        if epoch != self.epoch {
            // Whoever was sending is gone. Also seen once while first connecting.
            self.epoch = epoch;
            self.keyer.link_lost(now, |_, _, _| {});
            self.reader.clear();
            self.out.clear();
        }

        let mut buf = [0u8; 64];
        let n = hw.usb_read(&mut buf).min(buf.len());
        for &b in &buf[..n] {
            if let Some(line) = self.reader.push(b) {
                if let Some(reply) = self.keyer.handle_line(now, line) {
                    self.out.push(reply.as_bytes());
                }
            }
        }

        self.keyer.poll(now);
        let mut pins = Outputs::of(&self.keyer);
        let fault = self
            .guard
            .fault(now, pins.key || pins.tone)
            .or_else(|| self.ptt_guard.fault(now, pins.ptt));
        if let Some(why) = fault {
            self.keyer.trip_now(now, why, |_, _, _| {});
            pins = Outputs::of(&self.keyer);
        }
        pins.drive(hw);
        self.pins = pins;
        self.guard.driven(now, pins.key || pins.tone);
        self.ptt_guard.driven(now, pins.ptt);
        let lit = match self.keyer.trip() {
            Trip::None => pins.key || pins.ptt,
            _ => (now / TRIP_FLASH_MS).is_multiple_of(2),
        };
        hw.set_led(lit);
        self.out.send(hw);

        if self.keyer.hung() {
            // `TEST HANG` (`hfnode keyer hangtest`): the loop stops here, with the
            // outputs as they are, so that the watchdog has to reset the chip to
            // open them.
            self.hung_at = Some(now);
            return;
        }
        hw.feed_watchdog();
    }

    /// A pass while hung: nothing serviced, the watchdog not fed; the outputs
    /// opened only if the watchdog has not acted in time.
    fn hung_pass(&mut self, hw: &mut impl Hardware, at: u64) {
        let now = hw.now_ms();
        let late = now >= at + u64::from(limits::HANG_OPEN_MS);
        if late || self.guard.over(now) || self.ptt_guard.over(now) {
            self.pins = Outputs::OFF;
            self.guard.driven(now, false);
            self.ptt_guard.driven(now, false);
        }
        self.pins.drive(hw);
        hw.set_led(self.pins.key || self.pins.ptt);
    }
}

#[cfg(test)]
mod tests;
