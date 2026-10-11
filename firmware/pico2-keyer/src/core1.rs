//! Core 1: the decoder (`decoder_core`), listening to the radio's audio on GP26 and
//! passing what it hears to core 0 as lines (docs/keyer-protocol.md, version 5).
//!
//! Core 0 runs the keyer and is the only core that keys. Core 1 reaches nothing of
//! it: the two meet only in [`LINK`] (`keyer_core::cores`), whose rings and flags
//! core 0 reads without ever waiting, and core 0 trips the box if core 1 stops or
//! faults (`keyer_core::control::Core1Watch`). What keeps core 1 from holding the
//! key, in this file:
//!
//! - Core 0 sets up everything shared before it starts core 1: the clocks, USB,
//!   the watchdog, the pins, and the ADC sampling GP26 into [`RING`] through DMA
//!   channel 0 (`main`). Core 1 then only reads the timer, reads the DMA channel's
//!   position, clears the ADC's overflow flag and reads the ring ([`Board`]).
//! - Before anything else, core 1 turns on its memory protection unit ([`mpu`]):
//!   the system's default memory map off, and regions only for the flash it runs
//!   from, its own stack, [`LINK`], [`RING`], those ADC registers and, read-only,
//!   that DMA channel's registers and the timer's. Every other address faults on
//!   core 1: the SIO (whose `GPIO_OUT_SET` would close the key), IO_BANK0,
//!   PADS_BANK0, PWM, USB, the watchdog, RESETS, CLOCKS and the rest of SRAM,
//!   where core 0's memory is.
//! - Then it drops to unprivileged mode ([`drop_privilege`]), so that no stray
//!   write can reach the MPU's own registers, nor any in the Private Peripheral
//!   Bus, which the MPU does not cover.
//! - Its stack is bounded: rustos sets its `MSPLIM` to the bottom of [`STACK`]
//!   before core 1's first push, so an overflow faults instead of writing below it.
//! - A fault on core 1 runs the same handlers as on core 0 (one vector table), at
//!   HardFault priority, where the MPU is off (`MPU_CTRL.HFNMIENA` = 0): they open
//!   the key and the PTT first, stop the tone, then set [`keyer_core::cores::Link`]'s
//!   fault flag, which trips the box on core 0's next pass (`crate::safe_state`). A
//!   panic on core 1 gets there through a fault: its first store, to the SIO, is
//!   refused.
//! - Every pass of its loop moves the heartbeat. If it stops moving for
//!   `keyer_core::limits::CORE1_MS`, core 0 trips the box (`CORE1`), opens every
//!   output and stops feeding the watchdog, which resets both cores.
//! - Audio loss is not a fault: a halted DMA channel is reported (`GAP HALTED`) and
//!   the decoder stops, but its loop, and so the heartbeat, goes on.
//!
//! References: the RP2350 datasheet, and the Armv8-M Architecture Reference Manual
//! (Arm DDI0553B.z, "Armv8-M ARM").

use core::ops::Range;
use core::sync::atomic::Ordering;

use api::device::PinHandle;
use decoder_core::ADC_RATE;
use decoder_core::control::{AudioLoss, Boot, Control, Hardware};
use keyer_core::cores::{self, Link};
use pico2::adc::{AdcError, AdcRing, Rp2350Adc, Rp2350AdcStream};
use pico2::dma::Rp2350DmaChannel;
use pico2::multicore::{Core1Stack, Fifo};
use pico2::timer::Rp2350Timer;

/// What the two cores share, alone in its 32-byte blocks of memory so that one MPU
/// region covers exactly it ([`mpu::Region`]).
#[repr(C, align(32))]
pub struct Shared(pub Link);

pub static LINK: Shared = Shared(Link::new());

/// Core 1's stack: 64 kB. The decoder's state, about 11 kB, lives on it.
pub static STACK: Core1Stack<8192> = Core1Stack::new();

/// The ADC's ring of samples, 16 kB, filled by DMA for the whole boot: set up by
/// core 0 ([`start_audio`]), read by core 1.
static mut RING: AdcRing = AdcRing::new();

/// The DMA channel the ADC stream uses: core 1 may read its registers.
pub const DMA_CHANNEL: usize = 0;

/// Core 0, before starting core 1: the ADC sampling GP26 into [`RING`] through DMA
/// channel [`DMA_CHANNEL`], from here to the next reset (rustos's `pico2::adc`).
pub fn start_audio(
    adc: Rp2350Adc,
    pin: PinHandle<26>,
    channel: Rp2350DmaChannel<DMA_CHANNEL>,
) -> Result<Rp2350AdcStream, AdcError> {
    // SAFETY: the only reference ever made to RING, here, once: the board's ADC
    // handle exists once. The stream owns it from here. Through a raw pointer,
    // since a reference to a `static mut` cannot be taken directly.
    #[allow(clippy::deref_addrof)]
    let ring = unsafe { &mut *(&raw mut RING) };
    adc.stream_from_handle(pin, channel, ring, ADC_RATE)
}

/// What core 0 hands core 1.
pub struct Parts {
    pub timer: &'static Rp2350Timer,
    pub audio: Rp2350AdcStream,
    pub boot: Boot,
}

/// Written by core 0 before it starts core 1 ([`hand_over`]) and never again; taken
/// by core 1 before its MPU is on, after which it cannot reach it.
static mut PARTS: Option<Parts> = None;

/// Core 0, before starting core 1: what core 1 runs with.
pub fn hand_over(parts: Parts) {
    // SAFETY: core 1 has not started, and reads it only after the launch handshake,
    // which `Rp2350Core1::launch` begins with a `dsb`.
    unsafe { (&raw mut PARTS).write(Some(parts)) };
}

/// Core 1's code, from `Rp2350Core1::launch`.
pub fn main(_fifo: Fifo) -> ! {
    // SAFETY: core 0 wrote it before the launch and never touches it again.
    let Some(Parts { timer, audio, boot }) = (unsafe { (&raw mut PARTS).replace(None) }) else {
        // Never so: then no heartbeat, and core 0 trips the box.
        loop {
            core::hint::spin_loop();
        }
    };
    // SAFETY: core 1 is privileged until `drop_privilege`, and runs nothing else.
    unsafe {
        mpu::protect(&regions());
        drop_privilege();
    }

    let mut hw = Board { timer, audio };
    let mut control = Control::new(boot, crate::BUILD, &hw);
    let link = &LINK.0;
    let mut beat = 0u32;
    loop {
        match link.test.load(Ordering::Acquire) {
            cores::TEST_HANG => loop {
                core::hint::spin_loop();
            },
            cores::TEST_FAULT => refused_store(),
            _ => {}
        }
        control.pass(&mut hw);
        // Core 1 is the heartbeat's only writer.
        beat = beat.wrapping_add(1);
        link.beat.store(beat, Ordering::Release);
    }
}

/// The decoder's board on core 1: its "USB" is the two rings in [`LINK`].
struct Board {
    timer: &'static Rp2350Timer,
    audio: Rp2350AdcStream,
}

impl Hardware for Board {
    fn now_ms(&self) -> u64 {
        self.timer.now() / 1000
    }

    fn usb_poll(&mut self) {}

    fn link_epoch(&self) -> u32 {
        LINK.0.epoch.load(Ordering::Acquire)
    }

    fn usb_read(&mut self, buf: &mut [u8]) -> usize {
        LINK.0.to_core1.read(buf)
    }

    fn usb_write(&mut self, bytes: &[u8]) -> usize {
        LINK.0.from_core1.write(bytes)
    }

    fn adc_read(&mut self, buf: &mut [u16]) -> Result<usize, AudioLoss> {
        self.audio.read(buf).map_err(|e| match e {
            AdcError::Overrun => AudioLoss::Overrun,
            AdcError::FifoOverflow => AudioLoss::Fifo,
            // A halted channel, or anything else: no more audio will come.
            _ => AudioLoss::Halted,
        })
    }

    /// Core 0 drives the key, PTT and tone pins; core 1 cannot reach them.
    fn hold_outputs_low(&mut self) {}

    /// The LED is core 0's.
    fn set_led(&mut self, _on: bool) {}

    /// Only core 0 feeds the watchdog; core 0 watches core 1's heartbeat instead.
    fn feed_watchdog(&mut self) {}
}

/// `TEST CORE1 FAULT`: a store core 1's MPU must refuse, to the SIO's
/// `GPIO_OUT_CLR` (0xd0000000 + 0x020, RP2350 datasheet, Table 16). Were it let
/// through, it would only open the key and the PTT, and `hfnode keyer core1test`
/// would report that the box did not trip.
fn refused_store() {
    const SIO_GPIO_OUT_CLR: *mut u32 = 0xd000_0020 as *mut u32;
    unsafe { SIO_GPIO_OUT_CLR.write_volatile((1 << crate::KEY_GPIO) | (1 << crate::PTT_GPIO)) };
}

/// The XIP window the image runs from: rustos's link.ld `FLASH`, 0x10000000, 4 MB
/// (RP2350 datasheet §2.2.2, Table 9). Writes there are dropped by the XIP
/// controller anyway; the region is read-only.
const FLASH: Range<usize> = 0x1000_0000..0x1040_0000;
/// The ADC's registers `CS` to `INTF` (0x400a0000, RP2350 datasheet §12.4.7, Table
/// 1119): [`Rp2350AdcStream::read`] reads and clears `FCS.OVER`.
const ADC: Range<usize> = 0x400a_0000..0x400a_0020;
/// DMA channel 0's registers `CH0_READ_ADDR` to `CH0_AL1_TRANS_COUNT_TRIG`
/// (0x50000000, §12.6.10, Table 1146), read-only: [`Rp2350AdcStream::read`] reads
/// `CH0_WRITE_ADDR` and `CH0_CTRL_TRIG`, neither with a side effect.
const DMA: Range<usize> = 0x5000_0000 + 0x40 * DMA_CHANNEL..0x5000_0020 + 0x40 * DMA_CHANNEL;
/// TIMER0's registers `ARMED` to `INTR` (0x400b0000 + 0x20, §12.8.5, Table 1226),
/// read-only: [`Rp2350Timer::now`] reads `TIMERAWH` and `TIMERAWL`, "no side
/// effects". Not `TIMELR` (0x0c), whose read latches `TIMEHR`.
const TIMER: Range<usize> = 0x400b_0020..0x400b_0040;

/// Everything core 1 may reach, and how.
fn regions() -> [mpu::Region; 7] {
    use mpu::{Access, Memory, Region};
    let link = &raw const LINK as usize;
    let ring = &raw const RING as usize;
    [
        Region::new(FLASH, Access::ReadExecute, Memory::Code),
        Region::new(STACK.range(), Access::ReadWrite, Memory::Sram),
        Region::new(
            link..link + size_of::<Shared>(),
            Access::ReadWrite,
            Memory::Sram,
        ),
        Region::new(
            ring..ring + size_of::<AdcRing>(),
            Access::ReadWrite,
            Memory::Sram,
        ),
        Region::new(ADC, Access::ReadWrite, Memory::Device),
        Region::new(DMA, Access::Read, Memory::Device),
        Region::new(TIMER, Access::Read, Memory::Device),
    ]
}

/// Thread mode unprivileged from here on: `CONTROL.nPRIV` = 1 (Armv8-M ARM, D1.2.13
/// CONTROL), kept by `msr` only from privileged code (C2.4.126 MSR, "if
/// CurrentModeIsPrivileged() then CONTROL.nPRIV = R[n][0]"), so core 1 cannot undo
/// it. Unprivileged code cannot write the MPU's registers ("Privileged access
/// permitted only", D1.2.168 MPU_CTRL), nor `MSPLIM` (C2.4.126). The handlers run
/// privileged, as all exceptions do.
///
/// # Safety
///
/// After [`mpu::protect`], whose regions allow unprivileged access.
unsafe fn drop_privilege() {
    unsafe {
        core::arch::asm!(
            "mrs {c}, CONTROL",
            "orr {c}, {c}, #1",
            "msr CONTROL, {c}",
            // "a context synchronization event", so what follows runs unprivileged.
            "isb",
            c = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Core 1's memory protection unit (Armv8-M ARM, B10 and D1.2.165-D1.2.176). Each
/// core has its own; the RP2350's Cortex-M33s have eight Secure regions each (RP2350
/// datasheet §3, "8× Secure MPU regions"), and core 1 runs Secure, as the bootrom
/// leaves it.
mod mpu {
    use core::ops::Range;

    const MPU_CTRL: *mut u32 = 0xe000_ed94 as *mut u32;
    const MPU_RNR: *mut u32 = 0xe000_ed98 as *mut u32;
    const MPU_RBAR: *mut u32 = 0xe000_ed9c as *mut u32;
    const MPU_RLAR: *mut u32 = 0xe000_eda0 as *mut u32;
    const MPU_MAIR0: *mut u32 = 0xe000_edc0 as *mut u32;
    /// "8× Secure MPU regions".
    const REGIONS: u32 = 8;

    /// `MPU_CTRL.ENABLE`, bit 0. `HFNMIENA`, bit 1, and `PRIVDEFENA`, bit 2, stay 0:
    /// "MPU disabled for these accesses" by the HardFault and NMI handlers, and "Any
    /// instruction or data access that does not access a defined memory region
    /// faults" (D1.2.168).
    const CTRL_ENABLE: u32 = 1 << 0;

    /// How a region may be used.
    #[derive(Clone, Copy)]
    pub enum Access {
        /// Read and execute.
        ReadExecute,
        /// Read and write, never execute.
        ReadWrite,
        /// Read only, never execute.
        Read,
    }

    /// What a region is: the attributes the system memory map gives that kind of
    /// memory (Armv8-M ARM, B8.1), so that nothing but the permissions changes.
    #[derive(Clone, Copy)]
    pub enum Memory {
        /// "Code ... Normal ... WT RA", flash: `MAIR` attribute 0b1010_1010, Normal
        /// memory, Write-Through Non-transient, read-allocate (D1.2.165 MAIR_ATTR).
        Code = 0,
        /// "SRAM ... Normal ... WBWA RA": 0b1111_1111, Write-Back Non-transient,
        /// read- and write-allocate.
        Sram = 1,
        /// "Peripheral ... Device, nGnRE": 0b0000_0100.
        Device = 2,
    }

    /// `MPU_MAIR0`: `Attr0` to `Attr2`, one per [`Memory`] (D1.2.169).
    const MAIR0: u32 = 0xaa | (0xff << 8) | (0x04 << 16);

    /// One region: its addresses, whole 32-byte blocks (a region's base is "bits
    /// [31:5] of the lower inclusive limit", and its limit address is that of the
    /// last block, D1.2.171 and D1.2.173).
    pub struct Region {
        rbar: u32,
        rlar: u32,
    }

    impl Region {
        /// `range` must start and end on 32-byte boundaries. If it does not, the
        /// region has no access at all (`rlar` 0, not enabled): core 1 then faults
        /// on what it needs, and core 0 trips the box.
        pub fn new(range: Range<usize>, access: Access, memory: Memory) -> Self {
            if !range.start.is_multiple_of(32)
                || !range.end.is_multiple_of(32)
                || range.end <= range.start
            {
                return Self { rbar: 0, rlar: 0 };
            }
            // `MPU_RBAR` (D1.2.171): `SH`, bits 4:3, 0b00 "Non-shareable", as the
            // memory map's; `AP`, bits 2:1, 0b01 "Read/write by any privilege level"
            // or 0b11 "Read-only by any privilege level"; `XN`, bit 0, "Execution not
            // permitted".
            let (ap, xn) = match access {
                Access::ReadExecute => (0b11, 0),
                Access::ReadWrite => (0b01, 1),
                Access::Read => (0b11, 1),
            };
            // `MPU_RLAR` (D1.2.173): `LIMIT`, bits 31:5; `AttrIndx`, bits 3:1; `EN`,
            // bit 0.
            Self {
                rbar: range.start as u32 | (ap << 1) | xn,
                rlar: (range.end - 32) as u32 | ((memory as u32) << 1) | 1,
            }
        }
    }

    /// Program `regions` into regions 0 onwards, turn off any others, and enable
    /// the MPU. Regions must not overlap: an address in two "faults"
    /// ("Flag error if multiple regions match", E2.1.306 MPUCheck).
    ///
    /// # Safety
    ///
    /// Core 1, privileged, before it touches anything the regions leave out.
    pub unsafe fn protect<const N: usize>(regions: &[Region; N]) {
        const { assert!(N as u32 <= REGIONS) };
        unsafe {
            MPU_CTRL.write_volatile(0);
            sync();
            MPU_MAIR0.write_volatile(MAIR0);
            for r in 0..REGIONS {
                MPU_RNR.write_volatile(r);
                let (rbar, rlar) = match regions.get(r as usize) {
                    Some(region) => (region.rbar, region.rlar),
                    None => (0, 0),
                };
                // Disable the region before its base moves.
                MPU_RLAR.write_volatile(0);
                MPU_RBAR.write_volatile(rbar);
                MPU_RLAR.write_volatile(rlar);
            }
            MPU_CTRL.write_volatile(CTRL_ENABLE);
            sync();
        }
    }

    /// `dsb` then `isb`: the writes have reached the MPU, and what follows is
    /// fetched and run under it.
    fn sync() {
        unsafe { core::arch::asm!("dsb", "isb", options(nostack, preserves_flags)) };
    }
}
