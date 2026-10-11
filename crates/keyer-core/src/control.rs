//! The firmware's control loop, one pass at a time: [`Control::pass`] moves USB
//! bytes to and from the [`Keyer`], the PTT line into it, and its outputs onto the
//! key, tone and PTT pins, against [`Hardware`], the board. firmware/pico2-keyer
//! calls it in a loop that never waits; the tests here run it against a board in
//! memory and check every pass.
//!
//! Each pass, in this order:
//!
//! 1. service USB, then read the clock, the processor's own count, core 1's
//!    heartbeat and fault flag, and the PTT line;
//! 2. if the USB link went away since the last pass, end any run: the key and the
//!    PTT open and the tone stops on this same pass; tell core 1 the link's epoch;
//! 3. take the lines that arrived, queueing their replies: `RX` lines go to core 1
//!    (below), every other line to the keyer;
//! 4. bring the keyer up to now;
//! 5. check the pins ([`PinGuard`]): trip the box if the key or the tone has been
//!    on for the key-down limit, or the PTT down for the PTT limit (each the box's
//!    own, [`limits::KEY_DOWN_MS`] and [`limits::PTT_MS`], or the keyer's if
//!    shorter; an off shorter than [`limits::MIN_GAP_MS`] does not break either),
//!    or if this pass came more
//!    than [`limits::SLOW_PASS_MS`] after the last with the key or the tone on.
//!    This times the pins by the loop's own clock readings, not by the keyer's
//!    timeline;
//! 6. check the clock against the processor's count ([`ClockCheck`]): if the clock
//!    has stopped or slowed while the processor ran on, every time limit above has
//!    stopped with it, so the box trips (`CLOCK`), opening every output, and stops
//!    feeding its watchdog;
//! 7. check core 1 ([`Core1Watch`]): if a fault handler has run, or core 1 is
//!    running and its heartbeat has not moved for [`limits::CORE1_MS`], the box
//!    trips (`CORE1`) the same way, and stops feeding its watchdog;
//! 8. drive the key, the tone and the PTT, in that order, then the LED; hand core
//!    1 a bring-up test that came due (`TEST CORE1 HANG`, `TEST CORE1 FAULT`);
//! 9. send what replies fit, then at most one line from core 1 (below);
//! 10. save the box's trip, whether it is keying the transmitter, and its duty
//!     budget for the next boot ([`Hardware::save`], [`crate::keyer::Saved`]);
//! 11. feed the watchdog, once, unless `TEST HANG` has stopped the loop or the
//!     clock or core 1 check has failed.
//!
//! After `TEST HANG` takes effect, a pass only reads the clock and holds the pins
//! as they were, servicing nothing, saving nothing and never feeding the watchdog,
//! so that the watchdog must reset the chip. If it has not done so
//! [`limits::HANG_OPEN_MS`] after the hang, or once the key or tone has been on for
//! the key-down limit or the PTT down for the PTT limit, the pass opens them all
//! itself and holds them open.
//!
//! # The decoder on core 1 (protocol 5)
//!
//! The RP2350 has two processor cores. This loop runs on core 0 and is the only
//! thing that keys; the decoder (`decoder_core`) runs on core 1 and only listens.
//! The two meet in two byte rings and a few flags in memory, through the
//! `core1_*` methods of [`Hardware`], and this loop never waits on core 1:
//!
//! - A line `<id> RX <command>` from the host goes to core 1 as `<id> <command>`,
//!   framed again, and never to the keyer. The decoder's reply comes back as
//!   `<id> RX <reply>`. If core 1 is not running the box answers `ERR RX OFF` itself;
//!   if its ring is full, `ERR RX BUSY`; `RX` alone is `ERR RX ARGS`.
//! - The decoder's own lines (id `00`: `CH`, `ST`, `SL`, `GAP`) come to the host as
//!   `00 RX <line>` only after the host's `RX HELLO`, and until its `RX TEXT`, the
//!   USB link going away, or no valid line from the host for the link timeout: a
//!   host that never says `RX HELLO` sees none of them. Replies (any other id) are
//!   always passed on.
//! - A pass passes on at most one line from core 1, only a line that decodes and
//!   fits once `RX ` is added, after the keyer's own replies have gone, and only
//!   if the USB buffer then still has room for a whole keyer reply after it, so
//!   that a keyer reply never waits behind the decoder.
//! - Nothing from core 1 reaches the keyer: no `RX` line, nothing core 1 sends,
//!   and no flag but the heartbeat, the fault flag and the bytes this loop reads
//!   and checks.

use crate::frame::{self, Line, LineReader};
use crate::keyer::{Core1Test, Keyer, Trip};
use crate::{limits, MAX_LINE};

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
    /// Start (`true`) or stop the tone into the handheld's microphone.
    fn set_tone(&mut self, on: bool);
    /// Drive the PTT pin: `true` closes the handheld's PTT.
    fn set_ptt(&mut self, down: bool);
    /// Read the PTT line: `true` (high) while the handheld's PTT contact is open.
    fn ptt_line(&mut self) -> bool;
    fn set_led(&mut self, on: bool);
    fn feed_watchdog(&mut self);
    /// Bytes [`Hardware::usb_write`] would take now.
    fn usb_room(&self) -> usize;
    /// Whether core 1, the decoder, was started.
    fn core1_launched(&self) -> bool;
    /// Core 1's heartbeat: a count its loop moves on every pass.
    fn core1_beat(&self) -> u32;
    /// Whether a fault or panic handler has run on core 1. (The handlers set it
    /// on either core; on core 0 nothing runs after them to read it.)
    fn core1_fault(&self) -> bool;
    /// Tell core 1 the USB link's [`Hardware::link_epoch`].
    fn core1_epoch(&mut self, epoch: u32);
    /// Tell core 1 whether the key or the PTT is down, as last driven: the decoder
    /// does not listen then, nor just after, so that it never decodes the radio's
    /// own sidetone.
    fn core1_keyed(&mut self, down: bool);
    /// Queue `line`, its newline included, for core 1: whole or not at all;
    /// whether it was queued.
    fn core1_send(&mut self, line: &[u8]) -> bool;
    /// Bytes core 1 sent, into `buf`: how many.
    fn core1_read(&mut self, buf: &mut [u8]) -> usize;
    /// Have core 1 run a bring-up test.
    fn core1_test(&mut self, test: Core1Test);
}

/// The most a line takes in the USB buffer, its newline included: room kept for a
/// keyer reply behind any line from core 1.
pub const LINE_BYTES: usize = MAX_LINE + 1;

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

/// Core 1 as the loop watches it: its fault flag, and its heartbeat, which must
/// move at least every [`limits::CORE1_MS`] once core 1 is running (it was
/// started, or its heartbeat has been seen to move). A core 1 that was never
/// started and never moves is no decoder, and keying goes on without one.
#[derive(Debug, Clone)]
pub struct Core1Watch {
    running: bool,
    /// The heartbeat when it last moved, or when the watch began.
    beat: u32,
    /// When it last moved, or when the watch began.
    since: u64,
}

impl Core1Watch {
    /// A watch from `now`, core 1 started or not, its heartbeat at `beat`.
    pub fn new(now: u64, launched: bool, beat: u32) -> Self {
        Self {
            running: launched,
            beat,
            since: now,
        }
    }

    /// Core 1's heartbeat reads `beat` and its fault flag `fault` at `now`:
    /// [`Trip::Core1`] if it faulted, or if it is running and its heartbeat has
    /// not moved for [`limits::CORE1_MS`].
    pub fn check(&mut self, now: u64, beat: u32, fault: bool) -> Option<Trip> {
        if beat != self.beat {
            self.beat = beat;
            self.since = now;
            self.running = true;
        }
        let stalled = self.running && now.saturating_sub(self.since) >= u64::from(limits::CORE1_MS);
        (fault || stalled).then_some(Trip::Core1)
    }

    /// Whether core 1 is running: started, or seen to move.
    pub fn running(&self) -> bool {
        self.running
    }
}

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

    pub fn is_empty(&self) -> bool {
        self.len == 0
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
    clock: ClockCheck,
    core1: Core1Watch,
    /// The clock or core 1 check failed: the watchdog is not fed again, so that it
    /// resets the chip if its own clock still runs.
    starved: bool,
    /// When `TEST HANG` stopped the loop.
    hung_at: Option<u64>,
    /// Lines from core 1, a byte at a time.
    from_core1: LineReader,
    /// A line from core 1, framed for the host, waiting for room.
    forward: Option<Line>,
    /// The host said `RX HELLO`: the decoder's own lines go to it.
    rx_open: bool,
    /// When the last valid line from the host arrived.
    heard: u64,
}

impl Control {
    /// The loop for `keyer`, on `hw` with its key and PTT pins already driven low
    /// and its tone stopped.
    pub fn new<H: Hardware>(keyer: Keyer, hw: &H) -> Self {
        let l = keyer.limits();
        let now = hw.now_ms();
        Self {
            keyer,
            reader: LineReader::new(),
            out: Outbox::new(),
            epoch: hw.link_epoch(),
            // Never longer than the box's own limits, whatever limits the keyer
            // was given: these watches do not rely on the keyer's.
            guard: PinGuard::new(now, l.key_down_ms.min(limits::KEY_DOWN_MS)),
            ptt_guard: PinGuard::ptt(now, l.ptt_ms.min(limits::PTT_MS)),
            pins: Outputs::OFF,
            clock: ClockCheck::new(now, hw.cpu_cycles(), H::CPU_CYCLES_PER_MS),
            core1: Core1Watch::new(now, hw.core1_launched(), hw.core1_beat()),
            starved: false,
            hung_at: None,
            from_core1: LineReader::new(),
            forward: None,
            rx_open: false,
            heard: now,
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

    /// Whether the clock or core 1 check has failed, so that the loop no longer
    /// feeds the watchdog.
    pub fn starved(&self) -> bool {
        self.starved
    }

    /// Whether the decoder's own lines go to the host (`RX HELLO`).
    pub fn rx_open(&self) -> bool {
        self.rx_open
    }

    /// Whether core 1 is running and the box has not tripped for it: what `RX`
    /// lines need.
    fn decoder_up(&self) -> bool {
        self.core1.running() && self.keyer.trip() != Trip::Core1
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
        let core1 = self.core1.check(now, hw.core1_beat(), hw.core1_fault());
        let line = hw.ptt_line();
        self.keyer.set_line(line);

        let epoch = hw.link_epoch();
        if epoch != self.epoch {
            // Whoever was sending is gone. Also seen once while first connecting.
            self.epoch = epoch;
            self.keyer.link_lost(now, |_, _, _| {});
            self.reader.clear();
            self.out.clear();
            self.rx_open = false;
            self.forward = None;
        }
        hw.core1_epoch(epoch);

        let mut buf = [0u8; 64];
        let n = hw.usb_read(&mut buf).min(buf.len());
        let mut copy = [0u8; MAX_LINE + 1];
        for &b in &buf[..n] {
            let Some(l) = self.reader.push(b) else {
                continue;
            };
            let line = &mut copy[..l.len()];
            line.copy_from_slice(l);
            let reply = match frame::decode(line) {
                Ok((id, body)) if body == "RX" || body.starts_with("RX ") => {
                    self.heard = now;
                    self.rx_command(hw, id, &body[2..])
                }
                _ => self.keyer.handle_line(now, line),
            };
            if let Some(reply) = reply {
                // Only a line that decoded is answered.
                self.heard = now;
                self.out.push(reply.as_bytes());
            }
        }
        if self.rx_open && now.saturating_sub(self.heard) >= u64::from(limits::LINK_TIMEOUT_MS) {
            self.rx_open = false;
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
        // The clock first: with it stopped, core 1's watch has stopped too.
        if let Some(why) = clock.or(core1) {
            self.keyer.trip_now(now, why, |_, _, _| {});
            pins = Outputs::of(&self.keyer);
            self.starved = true;
        }
        pins.drive(hw);
        self.pins = pins;
        hw.core1_keyed(pins.key || pins.ptt);
        self.guard.driven(now, pins.key || pins.tone);
        self.ptt_guard.driven(now, pins.ptt);
        let lit = match self.keyer.trip() {
            Trip::None => pins.key || pins.ptt,
            _ => (now / TRIP_FLASH_MS).is_multiple_of(2),
        };
        hw.set_led(lit);
        if let Some(test) = self.keyer.core1_test() {
            hw.core1_test(test);
        }
        self.out.send(hw);
        self.forward_from_core1(hw);

        hw.save(self.keyer.saved(now).encode());
        if self.keyer.hung() {
            // `TEST HANG` (`hfnode keyer hangtest`): the loop stops here, with the
            // outputs as they are, so that the watchdog has to reset the chip to
            // open them.
            self.hung_at = Some(now);
            return;
        }
        if !self.starved {
            hw.feed_watchdog();
        }
    }

    /// `RX <command>` from the host under `id` (`args` is what follows `RX`): to
    /// core 1, or the box's own answer.
    fn rx_command(&mut self, hw: &mut impl Hardware, id: u8, args: &str) -> Option<Line> {
        let Some(cmd) = args.strip_prefix(' ').filter(|c| !c.is_empty()) else {
            return frame::encode(id, format_args!("ERR RX ARGS"));
        };
        if !self.decoder_up() {
            return frame::encode(id, format_args!("ERR RX OFF"));
        }
        // Shorter than the line it came in, so it fits.
        let line = frame::encode(id, format_args!("{cmd}"))?;
        let mut buf = [0u8; MAX_LINE + 1];
        buf[..line.len()].copy_from_slice(line.as_bytes());
        buf[line.len()] = b'\n';
        if !hw.core1_send(&buf[..=line.len()]) {
            return frame::encode(id, format_args!("ERR RX BUSY"));
        }
        match cmd {
            "HELLO" => self.rx_open = true,
            "TEXT" => self.rx_open = false,
            _ => {}
        }
        None
    }

    /// At most one line from core 1 to the host, framed `<id> RX <line>`: once the
    /// keyer's replies have all gone, and if the USB buffer still has room for a
    /// whole keyer reply after it. Reads at most a line's worth of bytes a pass.
    fn forward_from_core1<H: Hardware>(&mut self, hw: &mut H) {
        if !self.decoder_up() {
            self.forward = None;
            return;
        }
        let mut budget = LINE_BYTES;
        while self.forward.is_none() && budget > 0 {
            let mut b = [0u8; 1];
            if hw.core1_read(&mut b) == 0 {
                break;
            }
            budget -= 1;
            if let Some(l) = self.from_core1.push(b[0]) {
                self.forward = match frame::decode(l) {
                    // The decoder's own lines only while the host asked for them.
                    Ok((0, _)) if !self.rx_open => None,
                    Ok((id, body)) => frame::encode(id, format_args!("RX {body}")),
                    Err(_) => None,
                };
            }
        }
        let Some(l) = &self.forward else {
            return;
        };
        if self.out.is_empty() && hw.usb_room() >= l.len() + 1 + LINE_BYTES {
            self.out.push(l.as_bytes());
            self.forward = None;
            self.out.send(hw);
        }
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
