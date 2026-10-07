//! The firmware's control loop, one pass at a time: [`Control::pass`] moves USB
//! bytes to and from the [`Keyer`], and the keyer's key onto the key pin, against
//! [`Hardware`], the board. firmware/pico2-keyer calls it in a loop that never
//! waits; the tests here run it against a board in memory and check every pass.
//!
//! Each pass, in this order:
//!
//! 1. service USB, then read the clock;
//! 2. if the USB link went away since the last pass, end any run: the key opens on
//!    this same pass;
//! 3. take the lines that arrived, queueing their replies;
//! 4. bring the keyer up to now;
//! 5. check the key pin ([`PinGuard`]): trip the box if the pin has been high for
//!    the key-down limit (a key-up shorter than [`limits::MIN_GAP_MS`] does not
//!    break it), or if this pass came more than [`limits::SLOW_PASS_MS`] after the
//!    last with the pin high. This times the pin by the loop's own clock readings,
//!    not by the keyer's timeline;
//! 6. drive the key pin, then the LED;
//! 7. send what replies fit;
//! 8. feed the watchdog, once, unless `TEST HANG` has stopped the loop.
//!
//! After `TEST HANG` takes effect, a pass only reads the clock and holds the pin as
//! it was, servicing nothing and never feeding the watchdog, so that the watchdog
//! must reset the chip. If it has not done so [`limits::HANG_OPEN_MS`] after the
//! hang, or once the pin has been high for the key-down limit, the pass opens the
//! key itself and holds it open.

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
    fn set_led(&mut self, on: bool);
    fn feed_watchdog(&mut self);
}

/// The LED flashes at this half-period while the box is tripped.
pub const TRIP_FLASH_MS: u64 = 250;

/// The key pin as the loop drives it, timed by the loop's clock readings.
#[derive(Debug, Clone)]
pub struct PinGuard {
    key_down_ms: u64,
    /// When the pin went high, bridging lows shorter than [`limits::MIN_GAP_MS`];
    /// `None` once it has been low that long.
    high_from: Option<u64>,
    /// When it went low, if it is low after being high.
    low_from: Option<u64>,
    /// When it was last driven.
    last: u64,
}

impl PinGuard {
    /// A pin driven low at `now`.
    pub fn new(now: u64, key_down_ms: u32) -> Self {
        Self {
            key_down_ms: u64::from(key_down_ms),
            high_from: None,
            low_from: None,
            last: now,
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
        if self.high() && now.saturating_sub(self.last) > u64::from(limits::SLOW_PASS_MS) {
            return Some(Trip::Slow);
        }
        (down && now.saturating_sub(self.from(now)) >= self.key_down_ms).then_some(Trip::Pin)
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

/// The control loop's state. See the module documentation.
pub struct Control {
    keyer: Keyer,
    reader: LineReader,
    out: Outbox,
    epoch: u32,
    guard: PinGuard,
    /// When `TEST HANG` stopped the loop.
    hung_at: Option<u64>,
}

impl Control {
    /// The loop for `keyer`, on `hw` with its key pin already driven low.
    pub fn new(keyer: Keyer, hw: &impl Hardware) -> Self {
        let key_down_ms = keyer.limits().key_down_ms;
        Self {
            keyer,
            reader: LineReader::new(),
            out: Outbox::new(),
            epoch: hw.link_epoch(),
            guard: PinGuard::new(hw.now_ms(), key_down_ms),
            hung_at: None,
        }
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

        let epoch = hw.link_epoch();
        if epoch != self.epoch {
            // Whoever was sending is gone. Also seen once while first connecting.
            self.epoch = epoch;
            self.keyer.link_lost(now, |_, _| {});
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

        let mut down = self.keyer.poll(now);
        if let Some(why) = self.guard.fault(now, down) {
            self.keyer.trip_now(now, why, |_, _| {});
            down = self.keyer.key_down();
        }
        hw.set_key(down);
        self.guard.driven(now, down);
        let lit = match self.keyer.trip() {
            Trip::None => down,
            _ => (now / TRIP_FLASH_MS).is_multiple_of(2),
        };
        hw.set_led(lit);
        self.out.send(hw);

        if self.keyer.hung() {
            // `TEST HANG` (`hfnode keyer hangtest`): the loop stops here, with the
            // key as it is, so that the watchdog has to reset the chip to open it.
            self.hung_at = Some(now);
            return;
        }
        hw.feed_watchdog();
    }

    /// A pass while hung: nothing serviced, the watchdog not fed; the key opened
    /// only if the watchdog has not acted in time.
    fn hung_pass(&mut self, hw: &mut impl Hardware, at: u64) {
        let now = hw.now_ms();
        let late = now >= at + u64::from(limits::HANG_OPEN_MS);
        if late || self.guard.high_for(now) >= u64::from(self.keyer.limits().key_down_ms) {
            self.guard.driven(now, false);
        }
        let down = self.guard.high();
        hw.set_key(down);
        hw.set_led(down);
    }
}

#[cfg(test)]
mod tests;
