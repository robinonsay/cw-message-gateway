//! The decoder box: a Raspberry Pi Pico 2 that listens to a radio's headphone audio
//! on GP26, decodes the Morse in it, and sends letters over USB serial.
//!
//! In a serial terminal it types what it hears; hfnode switches it to framed lines
//! with `HELLO` (docs/decoder-protocol.md). How to wire it is in docs/decoder.md.
//! Everything it does is in `decoder_core`, the same code its tests run against a
//! board in memory: this file is the board, and `decoder_core::control::Control` is
//! the loop.
//!
//! It never transmits. What keeps a radio's key, and a handheld's PTT, open if this
//! image is ever flashed onto the keyer box, in this file:
//!
//! - The keyer box's key, PTT and tone pins, GP16, GP17 and GP18, are driven low
//!   before anything else starts, as the keyer firmware drives its key and PTT:
//!   before the clocks, before USB. Until then, and while the chip resets, the
//!   pads' reset state and the keyer box's 4.7 kΩ pull-downs hold them low.
//! - Nothing drives them high: no code in this image writes the SIO's `GPIO_OUT`,
//!   `GPIO_OUT_SET` or `GPIO_OUT_XOR`, or gives those pins any function but SIO.
//!   check_pins.py checks the built image for exactly that. The LED and the
//!   regulator's mode pin are driven through their own pins' output overrides
//!   instead, so that the SIO's set register is never needed.
//! - Every pass of the loop drives GP16-GP18 low again
//!   (`Hardware::hold_outputs_low`).
//! - A panic, a HardFault or any unexpected exception drives them low first, with
//!   one store and nothing before it ([`safe_state`]; check_pins.py checks the
//!   order), then stops, so that the watchdog resets the box.
//! - The hardware watchdog (500 ms) is fed only at the end of each pass; if the loop
//!   stops, the chip resets, and the pads return to their reset state.

#![no_std]
#![no_main]

use api::gpio::Gpio;
use decoder_core::ADC_RATE;
use decoder_core::control::{AudioLoss, Boot, Control, Hardware};
use pico2::adc::{AdcError, AdcRing, Rp2350Adc, Rp2350AdcStream};
use pico2::clocks::Rp2350Clocks;
use pico2::common::board::Rp2350;
use pico2::dma::Rp2350Dma;
use pico2::gpio::gpio::Rp2350Gpio;
use pico2::timer::Rp2350Timer;
use pico2::usb::{Rp2350Usb, UsbDeviceConfig};
use pico2::watchdog::{ChipResetCause, ResetReason, Rp2350Watchdog};

pico2::entry!(main, safe_state = safe_state);

/// The keyer box's key pin, GP16 (the Pico 2's pin 21): held low.
const KEY_GPIO: usize = 16;
/// The keyer box's PTT pin, GP17 (pin 22): held low.
const PTT_GPIO: usize = 17;
/// The keyer box's tone pin, GP18 (pin 24): held low.
const TONE_GPIO: usize = 18;
/// GP16, GP17 and GP18, as bits of the SIO's GPIO registers.
const HELD_LOW: u32 = (1 << KEY_GPIO) | (1 << PTT_GPIO) | (1 << TONE_GPIO);
/// GP23, the Pico 2 regulator's power-save pin: high puts it in PWM mode, which
/// "can greatly reduce the inherent ripple of the SMPS at light load, and
/// therefore the ripple on the ADC supply" (Pico 2 datasheet §5.3).
const SMPS_GPIO: usize = 23;
/// The Pico 2's on-board LED, GP25: on while the decoder hears a tone.
const LED_GPIO: usize = 25;
/// How long the loop may stall before the watchdog resets the chip.
const WATCHDOG_MS: u32 = 500;

/// The ADC's ring of samples, 16 kB in `.bss`, filled by DMA for the whole boot.
static mut RING: AdcRing = AdcRing::new();

/// The USB identity. hfnode recognises the box by its product name. The VID and
/// PID are the Pico SDK's own for a USB serial port, as the keyer box's; the serial
/// number differs from the keyer box's (`1`), so that a computer with both
/// plugged in names their ports apart.
const USB: UsbDeviceConfig = UsbDeviceConfig {
    vendor_id: 0x2e8a,
    product_id: 0x0009,
    device_release: 0x0100,
    manufacturer: "hfnode",
    product: decoder_core::NAME,
    serial_number: "2",
};

/// Which build this is, in `HELLO`: the short commit CI built it from
/// (`DECODER_BUILD_ID`), or `-` in a build from a working tree.
const BUILD: &str = match option_env!("DECODER_BUILD_ID") {
    Some(b) => b,
    None => "-",
};

/// The board: the one place that touches the chip.
struct Board {
    timer: Rp2350Timer,
    usb: Rp2350Usb,
    watchdog: Rp2350Watchdog,
    audio: Rp2350AdcStream,
    led: bool,
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

    fn adc_read(&mut self, buf: &mut [u16]) -> Result<usize, AudioLoss> {
        self.audio.read(buf).map_err(|e| match e {
            AdcError::Overrun => AudioLoss::Overrun,
            AdcError::FifoOverflow => AudioLoss::Fifo,
            // A halted channel, or anything else: no more audio will come.
            _ => AudioLoss::Halted,
        })
    }

    fn hold_outputs_low(&mut self) {
        outputs_low();
    }

    fn set_led(&mut self, on: bool) {
        if on != self.led {
            self.led = on;
            Overridden::<LED_GPIO>::drive(on);
        }
    }

    fn feed_watchdog(&mut self) {
        self.watchdog.feed();
    }
}

fn main() -> ! {
    let board = Rp2350::take().unwrap();
    let mut gpio = Rp2350Gpio::new(board.gpio);
    // First of all, the keyer box's key, PTT and tone pins: each driven low, then
    // its output enabled, then its pad's isolation released (rustos's
    // `output_from_handle`). Never written again except to drive them low.
    let _key = gpio.output_from_handle(board.pins.gpio16).unwrap();
    let _ptt = gpio.output_from_handle(board.pins.gpio17).unwrap();
    let _tone = gpio.output_from_handle(board.pins.gpio18).unwrap();
    Overridden::<SMPS_GPIO>::drive(true);
    Overridden::<LED_GPIO>::drive(false);

    let clocks = Rp2350Clocks::new(board.clocks);
    let timer = Rp2350Timer::new(board.timer, &clocks);
    let mut watchdog = Rp2350Watchdog::new(board.watchdog, &clocks);
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
    // From here a stall in bringing up the ADC or USB resets the box.
    watchdog.start(WATCHDOG_MS * 1000);
    let dma = Rp2350Dma::new(board.dma);
    let adc = Rp2350Adc::new(board.adc, &clocks).unwrap();
    // SAFETY: the only reference ever made to RING; the stream owns it from here.
    // Through a raw pointer, since a reference to a `static mut` cannot be taken
    // directly (clippy's suggestion would be that).
    #[allow(clippy::deref_addrof)]
    let ring = unsafe { &mut *(&raw mut RING) };
    let audio = adc
        .stream_from_handle(board.pins.gpio26, dma.ch0, ring, ADC_RATE)
        .unwrap();
    // Holds the USB pull-up off for about 10 ms so that the host sees a fresh
    // attach.
    let usb = Rp2350Usb::new(board.usb, &clocks, &timer, USB);

    let mut hw = Board {
        timer,
        usb,
        watchdog,
        audio,
        led: false,
    };
    let mut control = Control::new(boot, BUILD, &hw);
    loop {
        control.pass(&mut hw);
    }
}

/// What a panic, a HardFault and any unexpected exception do (the panic handler
/// below, and rustos's fault handlers through `entry!`'s `safe_state`) before they
/// stop and the watchdog, if it has started, resets the box: drive GP16-GP18 low,
/// with one register write and nothing before it. check_pins.py checks the order in
/// the built image.
fn safe_state() {
    outputs_low();
}

/// Drive GP16, GP17 and GP18 low: one store, no driver, no lock, nothing that can
/// fault. SIO `GPIO_OUT_CLR`, at 0xd0000000 + 0x020 (RP2350 datasheet, Table 24,
/// p64), clears the output level of each pin whose bit is written; harmless while
/// a pin is not yet an output.
fn outputs_low() {
    const SIO_GPIO_OUT_CLR: *mut u32 = 0xd000_0020 as *mut u32;
    unsafe { SIO_GPIO_OUT_CLR.write_volatile(HELD_LOW) };
}

/// A pin driven through its output overrides rather than the SIO, so that no code
/// in this image needs the SIO's `GPIO_OUT_SET`: only the LED and the regulator's
/// mode pin, never GP16-GP18 (a build error).
struct Overridden<const PIN: usize>;

impl<const PIN: usize> Overridden<PIN> {
    /// Referenced by [`Self::drive`], so it is evaluated.
    const ALLOWED: () = assert!(PIN == SMPS_GPIO || PIN == LED_GPIO);

    /// Drive the pin high or low. One store to `GPIOn_CTRL` (IO_BANK0 at
    /// 0x40028000, offset 4 + 8n; the layout of Table 650, p608, for every pin:
    /// GPIO23 Table 696, p646, GPIO25 Table 700, p650): `FUNCSEL` the null function
    /// (0x1f), `OUTOVER` drive high (0x3) or low (0x2), `OEOVER` enable output
    /// (0x3). Then the pad's isolation latch released through the pad register's
    /// atomic clear alias (PADS_BANK0 at 0x40038000, offset 4 + 4n, `ISO` bit 8;
    /// the layout of Table 852, p785: GPIO23 Table 875, p797, GPIO25 Table 877,
    /// p798; clear alias +0x3000, §2.1.3). The pad's output disable (`OD`) is off
    /// out of reset.
    fn drive(high: bool) {
        let () = Self::ALLOWED;
        const IO_BANK0: usize = 0x4002_8000;
        const PADS_BANK0: usize = 0x4003_8000;
        const CLR_ALIAS: usize = 0x3000;
        const FUNCSEL_NULL: u32 = 0x1f;
        const OUTOVER_LOW: u32 = 0x2 << 12;
        const OUTOVER_HIGH: u32 = 0x3 << 12;
        const OEOVER_ENABLE: u32 = 0x3 << 14;
        const PAD_ISO: u32 = 1 << 8;
        let ctrl = (IO_BANK0 + 4 + 8 * PIN) as *mut u32;
        let pad_clr = (PADS_BANK0 + CLR_ALIAS + 4 + 4 * PIN) as *mut u32;
        let out = if high { OUTOVER_HIGH } else { OUTOVER_LOW };
        unsafe {
            ctrl.write_volatile(OEOVER_ENABLE | out | FUNCSEL_NULL);
            pad_clr.write_volatile(PAD_ISO);
        }
    }
}

const _: () = assert!(SMPS_GPIO != KEY_GPIO && SMPS_GPIO != PTT_GPIO && SMPS_GPIO != TONE_GPIO);
const _: () = assert!(LED_GPIO != KEY_GPIO && LED_GPIO != PTT_GPIO && LED_GPIO != TONE_GPIO);

/// Drive GP16-GP18 low and stop. The watchdog, if it has started, resets the box;
/// if not, it stays here with them low until it is unplugged.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    safe_state();
    loop {
        core::hint::spin_loop();
    }
}
