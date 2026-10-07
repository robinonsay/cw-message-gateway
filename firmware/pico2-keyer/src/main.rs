//! The keyer box: a Raspberry Pi Pico 2 that keys a radio's key jack for hfnode.
//!
//! hfnode sends it lines over USB serial (docs/keyer-protocol.md); it keys the
//! text in Morse on GP16, which drives an optocoupler across the radio's key jack
//! (docs/keyer.md). Every rule and limit is in `keyer_core::keyer::Keyer`, the same
//! code hfnode's mock box runs in its tests: this file only moves bytes between USB
//! and the `Keyer`, and the `Keyer`'s key onto the pin, in a loop that never waits.
//!
//! What keeps the key open, in this file:
//!
//! - GP16 is driven low (key open) before anything else starts: before the clocks,
//!   before USB. Until then, and while the chip resets, the pad's reset state and
//!   the 4.7 kΩ pull-down on the board hold it low.
//! - The hardware watchdog (500 ms) is fed only at the end of each pass of the
//!   loop. If the loop stops, the chip resets, and a watchdog reset puts the pads
//!   back in their reset state: the key opens.
//! - USB going away (bus reset, suspend, which is also how a pulled cable looks,
//!   deconfigured) ends any run at once. DTR and RTS are never read.
//! - A panic opens the key and stops the loop, so the watchdog resets the box.

#![no_std]
#![no_main]

use api::common::Write;
use api::gpio::Gpio;
use keyer_core::frame::LineReader;
use keyer_core::keyer::{Boot, Keyer, Limits, Trip};
use pico2::clocks::Rp2350Clocks;
use pico2::common::board::Rp2350;
use pico2::gpio::gpio::Rp2350Gpio;
use pico2::timer::Rp2350Timer;
use pico2::usb::{Rp2350Usb, UsbDeviceConfig};
use pico2::watchdog::{ChipResetCause, ResetReason, Rp2350Watchdog};

pico2::entry!(main);

/// The key pin, GP16 (the Pico 2's pin 21). Also in `panic` below, and in
/// docs/keyer.md's wiring.
const KEY_GPIO: u32 = 16;

/// The USB identity. hfnode recognises the box by its product name. The VID and
/// PID are the Pico SDK's own for a USB serial port (Raspberry Pi's usb-pid list:
/// 0x0009, "Raspberry Pi Pico SDK CDC UART").
const USB: UsbDeviceConfig = UsbDeviceConfig {
    vendor_id: 0x2e8a,
    product_id: 0x0009,
    device_release: 0x0100,
    manufacturer: "hfnode",
    product: keyer_core::NAME,
    serial_number: "1",
};

/// The LED flashes at this half-period while the box is tripped.
const TRIP_FLASH_MS: u64 = 250;

fn main() -> ! {
    let board = Rp2350::take().unwrap();
    let mut gpio = Rp2350Gpio::new(board.gpio);
    // First of all, the key: driven low, then the pad's isolation latch released.
    let mut key = gpio.output_from_handle(board.pins.gpio16).unwrap();
    let mut led = gpio.output_from_handle(board.pins.led).unwrap();

    let clocks = Rp2350Clocks::new(board.clocks);
    let timer = Rp2350Timer::new(board.timer, &clocks);
    let mut watchdog = Rp2350Watchdog::new(board.watchdog, &clocks);
    // Holds the USB pull-up off for about 10 ms so that the host sees a fresh
    // attach: before the watchdog starts.
    let mut usb = Rp2350Usb::new(board.usb, &clocks, &timer, USB);
    let boot = match watchdog.reset_reason() {
        ResetReason::WatchdogTimeout => Boot::Watchdog,
        ResetReason::ChipReset(
            ChipResetCause::PowerOn
            | ChipResetCause::RunPin
            | ChipResetCause::DebuggerReset
            | ChipResetCause::DebuggerRescue
            | ChipResetCause::RiscvDebuggerReset,
        ) => Boot::Power,
        _ => Boot::Other,
    };
    watchdog.start(keyer_core::limits::WATCHDOG_MS * 1000);

    let ms = |t: &Rp2350Timer| t.now() / 1000;
    let mut keyer = Keyer::new(Limits::BOX, boot, ms(&timer));
    let mut reader = LineReader::new();
    let mut out = Outbox::new();
    let mut epoch = usb.link_epoch();
    let mut buf = [0u8; 64];

    loop {
        usb.poll();
        let now = ms(&timer);

        if usb.link_epoch() != epoch {
            // Bus reset, suspend (a pulled cable looks like this), deconfigured:
            // whoever was sending is gone. Also seen once while first connecting.
            epoch = usb.link_epoch();
            keyer.link_lost(now, |_, _| {});
            reader.clear();
            out.clear();
        }

        let n = usb.read(&mut buf);
        for &b in &buf[..n] {
            if let Some(line) = reader.push(b)
                && let Some(reply) = keyer.handle_line(now, line)
            {
                out.push(reply.as_bytes());
            }
        }

        let down = keyer.poll(now);
        let _ = key.write(down);
        if keyer.hung() {
            // `TEST HANG`: stop here with the key as it is, so that the watchdog
            // has to reset the chip to open it (`hfnode keyer hangtest`).
            loop {
                core::hint::spin_loop();
            }
        }

        let lit = match keyer.trip() {
            Trip::None => down,
            _ => (now / TRIP_FLASH_MS).is_multiple_of(2),
        };
        let _ = led.write(lit);

        out.send(&mut usb);
        watchdog.feed();
    }
}

/// Replies waiting for room in the USB driver's buffer. hfnode sends one command
/// and waits for its reply, so this rarely holds more than one line.
struct Outbox {
    buf: [u8; 256],
    len: usize,
}

impl Outbox {
    const fn new() -> Self {
        Self {
            buf: [0; 256],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    /// Queue a line, adding its newline; a line that does not fit is dropped
    /// (hfnode times out and asks again, or stops).
    fn push(&mut self, line: &[u8]) {
        let end = self.len + line.len() + 1;
        if end <= self.buf.len() {
            self.buf[self.len..end - 1].copy_from_slice(line);
            self.buf[end - 1] = b'\n';
            self.len = end;
        }
    }

    fn send(&mut self, usb: &mut Rp2350Usb) {
        if self.len == 0 {
            return;
        }
        let n = usb.write(&self.buf[..self.len]);
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }
}

/// Open the key and stop. The watchdog, if it has started, resets the box; if
/// not, it stays here with the key open until it is unplugged.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SIO GPIO_OUT_CLR (RP2350 datasheet, SIO registers: 0xd0000000 + 0x020):
    // drives GP16 low if it is an output; harmless if it is not yet.
    const SIO_GPIO_OUT_CLR: *mut u32 = 0xd000_0020 as *mut u32;
    unsafe { SIO_GPIO_OUT_CLR.write_volatile(1 << KEY_GPIO) };
    loop {
        core::hint::spin_loop();
    }
}
