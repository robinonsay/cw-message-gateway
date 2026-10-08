//! The firmware's control loop, one pass at a time: [`Control::pass`] moves USB
//! bytes to and from the [`Keyer`], and the keyer's key onto the key pin, against
//! [`Hardware`], the board. firmware/pico2-keyer calls it in a loop that never
//! waits; the tests here run it against a board in memory and check every pass.
//!
//! Each pass, in this order:
//!
//! 1. service USB, then read the clock, and the processor's own count;
//! 2. if the USB link went away since the last pass, end any run: the key opens on
//!    this same pass;
//! 3. take the lines that arrived, queueing their replies;
//! 4. bring the keyer up to now;
//! 5. check the key pin ([`PinGuard`]): trip the box if the pin has been high for
//!    the key-down limit (a key-up shorter than [`limits::MIN_GAP_MS`] does not
//!    break it), or if this pass came more than [`limits::SLOW_PASS_MS`] after the
//!    last with the pin high. This times the pin by the loop's own clock readings,
//!    not by the keyer's timeline;
//! 6. check the clock against the processor's count ([`ClockCheck`]): if the clock
//!    has stopped or slowed while the processor ran on, every time limit above has
//!    stopped with it, so the box trips (`CLOCK`) and stops feeding its watchdog;
//! 7. drive the key pin, then the LED;
//! 8. send what replies fit;
//! 9. save the box's trip, key and duty budget for the next boot
//!    ([`Hardware::save`], [`crate::keyer::Saved`]);
//! 10. feed the watchdog, once, unless `TEST HANG` has stopped the loop or the
//!     clock check has failed.
//!
//! After `TEST HANG` takes effect, a pass only reads the clock and holds the pin as
//! it was, servicing nothing, saving nothing and never feeding the watchdog, so
//! that the watchdog must reset the chip. If it has not done so
//! [`limits::HANG_OPEN_MS`] after the hang, or once the pin has been high for the
//! key-down limit, the pass opens the key itself and holds it open.

use crate::frame::LineReader;
use crate::keyer::{Keyer, Trip};
use crate::limits;

/// The board, as the control loop uses it.
pub trait Hardware {
    /// How much [`Hardware::cpu_cycles`] advances in a millisecond.
    const CPU_CYCLES_PER_MS: u32;

    /// Milliseconds since the box started.
    fn now_ms(&self) -> u64;
    /// The processor's own count, on a clock apart from [`Hardware::now_ms`]'s:
    /// it counts up [`Hardware::CPU_CYCLES_PER_MS`] a millisecond, modulo 2^24
    /// ([`CPU_CYCLES_MASK`]). The loop reads it at least once per wrap.
    fn cpu_cycles(&self) -> u32;
    /// Leave `words` for the next boot: they must survive a watchdog reset and
    /// be lost on a power-up ([`crate::keyer::Saved`]).
    fn save(&mut self, words: [u32; 2]);
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

/// [`Hardware::cpu_cycles`] counts modulo `CPU_CYCLES_MASK + 1`.
pub const CPU_CYCLES_MASK: u32 = 0x00ff_ffff;

/// The loop's clock checked against the processor's count ([`Hardware::cpu_cycles`]),
/// a window at a time: once either has run [`limits::CLOCK_CHECK_MS`], the clock
/// must have run at least half as long as the processor, and the processor at
/// least a quarter as long as the clock.
///
/// The first catches what would freeze every time limit at once while the loop
/// runs on: the clock's tick stopped, or paused by a debugger. The second catches
/// a check that is not working (the processor's count not running), so that it
/// fails as a trip, not as silence. It is the looser of the two because a pass
/// that took longer than the count's wrap (about 112 ms on the Pico 2) undercounts
/// the processor's time; no pass should take that long, but a slow pass with the
/// key up is not a reason to trip.
#[derive(Debug, Clone)]
pub struct ClockCheck {
    per_ms: u64,
    /// The count at the last check.
    last: u32,
    /// Processor cycles since the window began.
    cycles: u64,
    /// The clock when the window began.
    from: u64,
}

impl ClockCheck {
    /// A check whose window starts at `now` on the clock and `cycles` on the
    /// processor's count, which advances `per_ms` a millisecond.
    pub fn new(now: u64, cycles: u32, per_ms: u32) -> Self {
        Self {
            per_ms: u64::from(per_ms.max(1)),
            last: cycles & CPU_CYCLES_MASK,
            cycles: 0,
            from: now,
        }
    }

    /// The clock reads `now` and the count `cycles`: [`Trip::Clock`] if they
    /// disagree, as above.
    pub fn check(&mut self, now: u64, cycles: u32) -> Option<Trip> {
        let cycles = cycles & CPU_CYCLES_MASK;
        self.cycles += u64::from(cycles.wrapping_sub(self.last) & CPU_CYCLES_MASK);
        self.last = cycles;
        let cpu = self.cycles / self.per_ms;
        let clock = now.saturating_sub(self.from);
        let window = u64::from(limits::CLOCK_CHECK_MS);
        if cpu < window && clock < window {
            return None;
        }
        self.cycles = 0;
        self.from = now;
        (clock * 2 < cpu || cpu * 4 < clock).then_some(Trip::Clock)
    }
}

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
    clock: ClockCheck,
    /// The clock check failed: the watchdog is not fed again, so that it resets
    /// the chip if its own clock still runs.
    starved: bool,
    /// When `TEST HANG` stopped the loop.
    hung_at: Option<u64>,
}

impl Control {
    /// The loop for `keyer`, on `hw` with its key pin already driven low.
    pub fn new<H: Hardware>(keyer: Keyer, hw: &H) -> Self {
        let key_down_ms = keyer.limits().key_down_ms;
        let now = hw.now_ms();
        Self {
            keyer,
            reader: LineReader::new(),
            out: Outbox::new(),
            epoch: hw.link_epoch(),
            guard: PinGuard::new(now, key_down_ms),
            clock: ClockCheck::new(now, hw.cpu_cycles(), H::CPU_CYCLES_PER_MS),
            starved: false,
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

    /// Whether the clock check has failed, so that the loop no longer feeds the
    /// watchdog.
    pub fn starved(&self) -> bool {
        self.starved
    }

    /// One pass of the loop.
    pub fn pass(&mut self, hw: &mut impl Hardware) {
        if let Some(at) = self.hung_at {
            self.hung_pass(hw, at);
            return;
        }
        hw.usb_poll();
        let now = hw.now_ms();
        let clock = self.clock.check(now, hw.cpu_cycles());

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
        if let Some(why) = clock {
            self.keyer.trip_now(now, why, |_, _| {});
            down = self.keyer.key_down();
            self.starved = true;
        }
        hw.set_key(down);
        self.guard.driven(now, down);
        let lit = match self.keyer.trip() {
            Trip::None => down,
            _ => (now / TRIP_FLASH_MS).is_multiple_of(2),
        };
        hw.set_led(lit);
        self.out.send(hw);

        hw.save(self.keyer.saved(now).encode());
        if self.keyer.hung() {
            // `TEST HANG` (`hfnode keyer hangtest`): the loop stops here, with the
            // key as it is, so that the watchdog has to reset the chip to open it.
            self.hung_at = Some(now);
            return;
        }
        if !self.starved {
            hw.feed_watchdog();
        }
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
