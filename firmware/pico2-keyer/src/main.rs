//! The keyer box: a Raspberry Pi Pico 2 that keys a radio's key jack for hfnode.
//!
//! hfnode sends it lines over USB serial (docs/keyer-protocol.md); it keys the
//! text in Morse on GP16, which drives an optocoupler across the radio's key jack
//! (docs/keyer.md). Every rule and limit is in `keyer_core`, the same code hfnode's
//! mock box runs in its tests: this file is the board, and
//! `keyer_core::control::Control` is the loop, so that the loop itself is tested
//! against a board in memory (`keyer_core::control::tests`).
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
//! - The loop watches the key pin by its own clock readings and trips the box if it
//!   has been high for the key-down limit, whatever the `Keyer`'s own timeline says
//!   (`keyer_core::control::PinGuard`).
//! - A panic opens the key and stops the loop, so the watchdog resets the box.

#![no_std]
#![no_main]

use api::common::Write;
use api::gpio::Gpio;
use keyer_core::control::{Control, Hardware};
use keyer_core::keyer::{Boot, Keyer, Limits};
use pico2::clocks::Rp2350Clocks;
use pico2::common::board::Rp2350;
use pico2::gpio::gpio::{Rp2350Gpio, Rp2350GpioOut};
use pico2::timer::Rp2350Timer;
use pico2::usb::{Rp2350Usb, UsbDeviceConfig};
use pico2::watchdog::{ChipResetCause, ResetReason, Rp2350Watchdog};

pico2::entry!(main);

/// The key pin, GP16 (the Pico 2's pin 21). Also in `panic` below, and in
/// docs/keyer.md's wiring.
const KEY_GPIO: usize = 16;
/// The Pico 2's on-board LED, GP25.
const LED_GPIO: usize = 25;

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

/// Which build this is, in `HELLO`: the short commit CI built it from
/// (`KEYER_BUILD_ID`), so that hfnode can check the box runs the firmware whose
/// UF2 was checked (`[keyer] firmware_build`). `-` in a build from a working tree.
const BUILD: &str = match option_env!("KEYER_BUILD_ID") {
    Some(b) => b,
    None => "-",
};

/// The board: the one place that touches the chip.
struct Board {
    timer: Rp2350Timer,
    usb: Rp2350Usb,
    watchdog: Rp2350Watchdog,
    key: Rp2350GpioOut<KEY_GPIO>,
    led: Rp2350GpioOut<LED_GPIO>,
}

impl Hardware for Board {
    fn now_ms(&self) -> u64 {
        self.timer.now() / 1000
    }

    fn usb_poll(&mut self) {
        self.usb.poll();
    }

    fn link_epoch(&self) -> u32 {
        self.usb.link_epoch()
    }

    fn usb_read(&mut self, buf: &mut [u8]) -> usize {
        self.usb.read(buf)
    }

    fn usb_write(&mut self, bytes: &[u8]) -> usize {
        self.usb.write(bytes)
    }

    fn set_key(&mut self, down: bool) {
        let _ = self.key.write(down);
    }

    fn set_led(&mut self, on: bool) {
        let _ = self.led.write(on);
    }

    fn feed_watchdog(&mut self) {
        self.watchdog.feed();
    }
}

fn main() -> ! {
    let board = Rp2350::take().unwrap();
    let mut gpio = Rp2350Gpio::new(board.gpio);
    // First of all, the key: driven low, then the pad's isolation latch released.
    let mut key = gpio.output_from_handle(board.pins.gpio16).unwrap();
    let _ = key.write(false);
    let led = gpio.output_from_handle(board.pins.led).unwrap();

    let clocks = Rp2350Clocks::new(board.clocks);
    let timer = Rp2350Timer::new(board.timer, &clocks);
    let mut watchdog = Rp2350Watchdog::new(board.watchdog, &clocks);
    // Holds the USB pull-up off for about 10 ms so that the host sees a fresh
    // attach: before the watchdog starts.
    let usb = Rp2350Usb::new(board.usb, &clocks, &timer, USB);
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

    let mut hw = Board {
        timer,
        usb,
        watchdog,
        key,
        led,
    };
    let keyer = Keyer::new(Limits::BOX, boot, hw.now_ms()).with_build(BUILD);
    let mut control = Control::new(keyer, &hw);
    loop {
        control.pass(&mut hw);
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
