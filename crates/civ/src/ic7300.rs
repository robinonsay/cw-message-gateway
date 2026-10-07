//! The ICOM IC-7300 over CI-V on its USB serial port.
//!
//! Every command below cites ICOM's *IC-7300 Full Manual* (English, revision
//! `IC-7300_ENG_FM_12b`), Section 19 "CONTROL COMMAND": the data format on p. 19-2,
//! the command table on pp. 19-3 to 19-8 and the data content descriptions on
//! pp. 19-9 to 19-15. Quoted values are copied from those pages.
//! A text copy of the manual is kept in the project files at
//! `reference/IC-7300_ENG_FM_12b.txt`; Section 19 starts at line 8531 and the
//! command table at line 8771.
//!
//! Points from the manual that the node relies on:
//! - Frames are `FE FE 94 E0 Cn Sc Data FD`; the radio answers `FE FE E0 94 FB FD`
//!   (OK) or `... FA FD` (NG); 94h is the default transceiver address (p. 19-2).
//! - CW text sent with command 17 is transmitted only "if the [TRANSMIT] or an
//!   external TX switch is ON, or the Break-in function is ON" (footnote *2,
//!   p. 19-8), so the node turns semi break-in on.
//! - With "CI-V USB Echo Back" ON (command 1A 05 00 75, "00=ON, 01=OFF", p. 19-5)
//!   the radio repeats our own frames; the reader skips them either way.
//! - The semi break-in delay is set with 14 0F (p. 19-3), longer than a word gap,
//!   so the radio does not drop to receive in the gaps of a keyer message.
//!
//! Safety in this module: CW is keyed through the radio's own keyer (command 17),
//! [`Ic7300::set_transmit`] refuses `true` without sending anything (the driver never
//! sends `1C 00 01`), and every set command must be acknowledged with OK (FB) or it
//! is treated as failed. Input left over from an earlier command (a late reply after
//! a timeout) is discarded before each command, so it cannot be taken as the next
//! one's answer. Replies are checked for their exact length and values; anything
//! else is an error rather than a guess.
//!
//! The only frames that can make the radio transmit are `17` (CW text) and `1C 01 02`
//! (a tuner cycle). Every other command the driver can send either reads, sets a
//! value, or returns the radio to receive. Reads are only reachable through named
//! methods, so no caller can put an arbitrary frame on the wire.
//!
//! The USB serial port has two control lines, DTR and RTS, that the radio can be
//! set to treat as a transmit (USB SEND) or CW key (USB Keying (CW)) line: "DTR:
//! Uses the DTR terminal on the CI-V (PC) side" (USB SEND and USB Keying items,
//! p. 12-11; manual text lines 6895-6927). Opening a serial port can raise both
//! lines, so [`Ic7300::open`] drops them again straight away and fails if it cannot.
//! [`crate::preflight`] also reads those settings and refuses to go on unless they
//! are OFF. What each system does at open:
//!
//! - **Linux**: the tty layer raises DTR and RTS on open (the `cp210x` driver's
//!   `dtr_rts` hook), and lowers them on close (HUPCL).
//! - **macOS**: the first open of a port raises DTR and RTS (Apple's IOSerialFamily,
//!   `IOSerialBSDClient::initSession`, which creates the `/dev/cu.*` and `/dev/tty.*`
//!   files for a serial driver), and close lowers them (HUPCL). Read in Apple's
//!   published source; that Apple's and Silicon Labs' current CP210x drivers go
//!   through it is understood but not checked, and none of it has been measured on
//!   an IC-7300.
//! - **Windows**: the port is opened, then its line settings are applied with DTR
//!   and RTS control disabled (serialport's `SetCommState`), then both are cleared.
//!   Microsoft's sample serial driver brings the lines up on open as its saved
//!   settings say (enabled by default); whether Silicon Labs' CP210x driver does the
//!   same before the settings arrive is not documented.
//!
//! So on every system the lines may be up for the moment between the port opening
//! and the settings being applied. The radio's "Inhibit Timer at USB Connection"
//! (default ON) is meant for that moment: it holds off a SEND or keying signal for
//! a few seconds when "a virtual serial port communication is established" (manual
//! text lines 6928-6945). It only delays a line that stays up, though; what makes
//! the lines harmless is USB SEND and USB Keying set to OFF, which the preflight
//! requires before anything is written.

use crate::frame::{bcd_be, bcd_le, from_bcd_be, from_bcd_le, take_frame, Frame, CONTROLLER};
use crate::serial::{lower_control_lines, ControlLines};
use crate::{Result, Rig, RigError, MAX_CW_CHARS};
use std::io::{ErrorKind, Read, Write};
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

/// CI-V addresses the radio can be set to: "CI-V Address (Default: 94h) ... Range:
/// 02h ~ 94h ~ DFh" (p. 12-10; manual text line 6825).
pub const ADDRESS_RANGE: RangeInclusive<u8> = 0x02..=0xDF;

/// The IC-7300's default CI-V address, which is also what it answers to `19 00`
/// "Read the transceiver ID": "“94h” is the default address of IC-7300" (p. 12-10;
/// manual text line 6826).
pub const IC7300_ID: u8 = 0x94;

/// "CI-V USB Baud Rate ... Options: 4800, 9600, 19200, 38400, 57600, 115200 (bps),
/// or Auto" (p. 12-11; manual text lines 6869-6872).
pub const USB_BAUD_RATES: [u32; 6] = [4800, 9600, 19_200, 38_400, 57_600, 115_200];

/// Frequencies the driver will set: the receiver's coverage, "0.030000~74.800000"
/// MHz (p. 16-2; manual text line 8014). The 5-byte frequency data could not carry
/// 100 MHz or more anyway ("100 MHz digit: 0 (Fixed)", p. 19-9).
pub const FREQUENCY_RANGE_HZ: RangeInclusive<u64> = 30_000..=74_800_000;

/// Whether `baud` and `addr` are settings the radio offers.
pub fn check_link_settings(baud: u32, addr: u8) -> Result<()> {
    if !USB_BAUD_RATES.contains(&baud) {
        return Err(RigError::Protocol(format!(
            "baud {baud} is not one of the radio's CI-V USB rates {USB_BAUD_RATES:?}"
        )));
    }
    if !ADDRESS_RANGE.contains(&addr) {
        return Err(RigError::Protocol(format!(
            "CI-V address {addr:02X}h is outside the radio's 02h-DFh"
        )));
    }
    Ok(())
}

/// Commands, as `[command, sub-command...]`, with citations to Section 19 of the
/// IC-7300 Full Manual.
mod cmd {
    /// 03: "Read operating frequency" (p. 19-3). Data: 5 BCD bytes, 10 Hz/1 Hz digits
    /// first, 1000 MHz/100 MHz digits last and fixed at 0 (p. 19-9).
    pub const READ_FREQ: &[u8] = &[0x03];
    /// 05: "Set operating frequency" (p. 19-3), same data format (p. 19-9).
    pub const SET_FREQ: &[u8] = &[0x05];
    /// 06: "Operating mode selection for transceive" (p. 19-3). Data: mode, then
    /// filter; "03: CW", "01: FIL1" (p. 19-9).
    pub const SET_MODE: &[u8] = &[0x06];
    pub const MODE_CW: u8 = 0x03;
    pub const FILTER_1: u8 = 0x01;
    /// 14 0A: "Send/read [RF PWR] position (00 00=max. CCW, 02 55=max. CW)" (p. 19-3).
    pub const RF_POWER: &[u8] = &[0x14, 0x0A];
    /// 14 0C: "Send/read [KEY SPEED] level (00 00=6wpm, 02 55=48wpm)" (p. 19-3).
    pub const KEY_SPEED: &[u8] = &[0x14, 0x0C];
    /// 14 0F: "Send/read the Break-IN Delay setting (00 00=2.0d to 02 55=13.0d)"
    /// (p. 19-3).
    pub const BREAK_IN_DELAY: &[u8] = &[0x14, 0x0F];
    /// 15 11: "Read PO meter level (00 00=0%, 01 43=50%, 02 13=100%)" (p. 19-3).
    pub const PO_METER: &[u8] = &[0x15, 0x11];
    /// 15 12: "Read SWR meter level (00 00=SWR1.0, 00 48=SWR1.5, 00 80=SWR2.0,
    /// 01 20=SWR3.0)" (p. 19-3).
    pub const SWR_METER: &[u8] = &[0x15, 0x12];
    /// 16 47: "BK-IN function (00=BK-IN OFF, 01=Semi BK-IN ON, 02=Full BK-IN ON)"
    /// (p. 19-3).
    pub const BREAK_IN: &[u8] = &[0x16, 0x47];
    pub const BREAK_IN_OFF: u8 = 0x00;
    pub const BREAK_IN_SEMI: u8 = 0x01;
    /// 17: "Send CW messages" (p. 19-4): "Up to 30 characters" of the listed ASCII
    /// codes; "“FF” stops sending CW messages" (p. 19-13).
    pub const SEND_CW: &[u8] = &[0x17];
    pub const STOP_CW: u8 = 0xFF;
    /// 1C 00: "Send/read transceiver's status" "00" RX, "01" TX (p. 19-7).
    pub const TX_STATUS: &[u8] = &[0x1C, 0x00];
    /// 1C 01: "00=Send/read the antenna tuner OFF, 01=Send/read the antenna tuner ON,
    /// 02=Send/read to tuning" (p. 19-7).
    pub const TUNER: &[u8] = &[0x1C, 0x01];
    pub const TUNER_TUNE: u8 = 0x02;

    // Read only. Each of these is sent without data, which reads the item; none of
    // them is ever sent with data.

    /// 04: "Read operating mode" (p. 19-3). Data: mode, then filter, coded as for 06
    /// (p. 19-9).
    pub const READ_MODE: &[u8] = &[0x04];
    /// 0F: "Read Split setting (00=OFF, 01=ON)" (p. 19-3).
    pub const SPLIT: &[u8] = &[0x0F];
    /// 19 00: "Read the transceiver ID" (p. 19-4).
    pub const TRANSCEIVER_ID: &[u8] = &[0x19, 0x00];
    /// 21 02: "Send/read ∂TX setting (00=OFF, 01=ON)" (p. 19-7).
    pub const DELTA_TX: &[u8] = &[0x21, 0x02];
    /// 1A 05 00 29: "Send/read the Time-Out Timer setting (00=OFF, 01=3 min.,
    /// 02=5 min., 03=10min., 04=20 min., 05=30 min.)" (p. 19-4).
    pub const TIME_OUT_TIMER: &[u8] = &[0x1A, 0x05, 0x00, 0x29];
    /// 1A 05 00 71: "Send/read the CI-V transceive setting (00=OFF, 01=ON)" (p. 19-5).
    pub const CIV_TRANSCEIVE: &[u8] = &[0x1A, 0x05, 0x00, 0x71];
    /// 1A 05 00 74: "Send/read the CI-V USB port setting (00=Link to [REMOTE],
    /// 01=Unlink to [REMOTE]) (Read only)" (p. 19-5).
    pub const CIV_USB_PORT: &[u8] = &[0x1A, 0x05, 0x00, 0x74];
    /// 1A 05 00 75: "Send/read echo back setting for CI-V operation from USB
    /// (00=ON, 01=OFF)" (p. 19-5).
    pub const USB_ECHO_BACK: &[u8] = &[0x1A, 0x05, 0x00, 0x75];
    /// 1A 05 00 78: "Send/read transmission control line setting for USB (00=OFF,
    /// 01=DTR, 02=RTS)" (p. 19-5): the USB SEND item.
    pub const USB_SEND: &[u8] = &[0x1A, 0x05, 0x00, 0x78];
    /// 1A 05 00 79: "Send/read CW keying line setting for USB (00=OFF, 01= DTR,
    /// 02=RTS)" (p. 19-5).
    pub const USB_KEYING_CW: &[u8] = &[0x1A, 0x05, 0x00, 0x79];
    /// 1A 05 00 80: "Send/read RTTY (FSK) line setting for USB (00=OFF, 01=DTR,
    /// 02=RTS)" (p. 19-5).
    pub const USB_KEYING_RTTY: &[u8] = &[0x1A, 0x05, 0x00, 0x80];
    /// 1A 05 01 97: "Inhibit Timer at USB connection (00=OFF, 01=ON)" (p. 19-7).
    pub const USB_INHIBIT_TIMER: &[u8] = &[0x1A, 0x05, 0x01, 0x97];
    /// 1C 03: "Read transmit frequency" (p. 19-7), in the operating frequency format
    /// ("Command: 00, 03, 05, 1C 03", p. 19-9).
    pub const TX_FREQ: &[u8] = &[0x1C, 0x03];
    /// 1A 05 00 84: "Send/read peak hold set for meter *(00=OFF, 01=ON)" (p. 19-5).
    pub const METER_PEAK_HOLD: &[u8] = &[0x1A, 0x05, 0x00, 0x84];
    /// 1A 05 01 61: "Send/read CW keyer dot/dash ratio (28=1:1:2.8 to 45=1:1:4.5)"
    /// (p. 19-6).
    pub const KEYER_RATIO: &[u8] = &[0x1A, 0x05, 0x01, 0x61];
    /// 27 11: "Send/read the Scope wave data output (00=OFF, 01=ON)" (p. 19-14);
    /// with it and the scope ON the radio streams 27 00 waveform data unasked.
    pub const SCOPE_DATA_OUTPUT: &[u8] = &[0x27, 0x11];
}

/// Five BCD bytes, 1 Hz and 10 Hz digits first, the last holding the 1000 MHz and
/// 100 MHz digits, "0 (Fixed)" (p. 19-9).
fn parse_frequency(data: &[u8]) -> Result<u64> {
    match from_bcd_le(data) {
        Some(hz) if data.len() == 5 && data[4] == 0x00 => Ok(hz),
        _ => Err(RigError::Protocol(format!("frequency {data:02X?}"))),
    }
}

/// A USB control-line setting (1A 05 00 78, 00 79, 00 80): "00=OFF, 01=DTR, 02=RTS"
/// (p. 19-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbLine {
    Off,
    Dtr,
    Rts,
}

/// What the radio is set to do with its USB serial control lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbLines {
    /// USB SEND: transmit while the line is raised.
    pub send: UsbLine,
    /// USB Keying (CW): key down while the line is raised.
    pub keying_cw: UsbLine,
    /// USB Keying (RTTY): FSK keying.
    pub keying_rtty: UsbLine,
}

/// SWR meter calibration points (meter value, SWR) from the 15 12 row (p. 19-3).
const SWR_POINTS: [(f32, f32); 4] = [(0.0, 1.0), (48.0, 1.5), (80.0, 2.0), (120.0, 3.0)];

/// Po meter calibration points (meter value, percent) from the 15 11 row (p. 19-3).
const PO_POINTS: [(f32, f32); 3] = [(0.0, 0.0), (143.0, 50.0), (213.0, 100.0)];

/// Linear interpolation between calibration points, extrapolating the last segment.
fn interpolate(points: &[(f32, f32)], level: u16) -> f32 {
    let x = level as f32;
    for w in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if x <= x1 {
            return y0 + (x - x0) * (y1 - y0) / (x1 - x0);
        }
    }
    let ((x0, y0), (x1, y1)) = (points[points.len() - 2], points[points.len() - 1]);
    y1 + (x - x1) * (y1 - y0) / (x1 - x0)
}

/// Convert a 15 12 meter reading (0-255) to SWR by linear interpolation between
/// ICOM's published points, extrapolating the last segment above 120.
pub fn swr_from_meter(level: u16) -> f32 {
    interpolate(&SWR_POINTS, level)
}

/// Convert a 15 11 meter reading (0-255) to percent of full output, the same way.
pub fn po_from_meter(level: u16) -> f32 {
    interpolate(&PO_POINTS, level)
}

/// Keyer speed in wpm to the 14 0C level (00 00 = 6 wpm ... 02 55 = 48 wpm).
pub fn key_speed_level(wpm: u32) -> u16 {
    let wpm = wpm.clamp(6, 48) as f32;
    ((wpm - 6.0) * 255.0 / 42.0).round() as u16
}

/// The keyer speed in wpm that a 14 0C level gives, on the same linear scale as
/// [`key_speed_level`].
pub fn key_speed_wpm(level: u16) -> f32 {
    6.0 + level.min(255) as f32 * 42.0 / 255.0
}

/// Break-in delay in dots to the 14 0F level (00 00 = 2.0 dots ... 02 55 = 13.0
/// dots). The manual gives only the end points; the linear mapping is an assumption
/// to confirm on the radio's BKIN DELAY display during hardware testing.
pub fn break_in_delay_level(dots: f32) -> u16 {
    ((dots.clamp(2.0, 13.0) - 2.0) * 255.0 / 11.0).round() as u16
}

/// Watts to the 14 0A level. The manual defines this as the [RF PWR] knob position
/// (00 00 = fully counter-clockwise, 02 55 = fully clockwise), not as watts, so the
/// linear mapping to 0-100 W is an assumption to confirm against the Po meter
/// (15 11) during hardware testing.
pub fn power_level(watts: u32) -> u16 {
    ((watts.min(100) as f32) * 255.0 / 100.0).round() as u16
}

/// The serial link to the radio.
pub trait Port: Read + Write + Send {
    /// Throw away everything received but not yet read.
    fn discard_input(&mut self) -> std::io::Result<()>;
}

impl Port for Box<dyn serialport::SerialPort> {
    fn discard_input(&mut self) -> std::io::Result<()> {
        self.clear(serialport::ClearBuffer::Input)
            .map_err(std::io::Error::other)
    }
}

pub struct Ic7300<P: Port = Box<dyn serialport::SerialPort>> {
    port: P,
    addr: u8,
    buf: Vec<u8>,
    timeout: Duration,
    /// Set after a timeout: the late reply may still be on its way, so the link is
    /// read until quiet before the next command.
    resync: bool,
    /// The 14 0C level last set, for [`Rig::dot_duration`].
    key_level: Option<u16>,
}

impl Ic7300 {
    /// Open the radio's USB serial port, 8 data bits, no parity, 1 stop bit, no flow
    /// control, for this process only. `baud` must match the radio's CI-V USB baud
    /// rate setting.
    ///
    /// DTR and RTS are dropped as soon as the port is open, and the port is not used
    /// if that fails: with USB SEND or USB Keying (CW) set to DTR or RTS, a raised
    /// line would key the transmitter for as long as the port stays open (pp. 12-11;
    /// manual text lines 6895-6927). The system may still raise them for a moment
    /// while opening (see the module notes for Linux, macOS and Windows); the radio's
    /// "Inhibit Timer at USB Connection" (default ON, line 6928) delays a signal
    /// then by a few seconds, and [`crate::preflight`] refuses the radio unless the
    /// settings themselves are OFF, which is what makes the lines harmless.
    pub fn open(path: &str, baud: u32, addr: u8) -> Result<Self> {
        Self::open_with(
            || {
                let builder = serialport::new(path, baud)
                    .data_bits(serialport::DataBits::Eight)
                    .parity(serialport::Parity::None)
                    .stop_bits(serialport::StopBits::One)
                    .flow_control(serialport::FlowControl::None)
                    .dtr_on_open(false)
                    .timeout(Duration::from_millis(50));
                // Windows opens a COM port for one handle only (share mode 0) by itself.
                #[cfg(unix)]
                let builder = builder.exclusive(true);
                builder
                    .open()
                    .map_err(|e| RigError::Io(std::io::Error::other(e)))
            },
            baud,
            addr,
        )
    }
}

impl<P: Port + ControlLines> Ic7300<P> {
    /// [`Ic7300::open`] with the port that `open` opens: the link settings are
    /// checked before `open` is called, and DTR and then RTS are lowered
    /// ([`crate::serial::lower_control_lines`]) before the driver is returned; if
    /// either cannot be lowered, the port is closed again and nothing is written.
    pub fn open_with(open: impl FnOnce() -> Result<P>, baud: u32, addr: u8) -> Result<Self> {
        check_link_settings(baud, addr)?;
        let mut port = open()?;
        lower_control_lines(&mut port)?;
        Ok(Self::with_port(port, addr))
    }
}

fn is_quiet(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

impl<P: Port> Ic7300<P> {
    pub fn with_port(port: P, addr: u8) -> Self {
        Self {
            port,
            addr,
            buf: Vec::new(),
            timeout: Duration::from_millis(500),
            resync: false,
            key_level: None,
        }
    }

    /// Discard anything already received, so that a reply to an earlier command
    /// cannot be read as the answer to the next one. After a timeout, first read
    /// until the link has been quiet for a whole reply timeout, since the late reply
    /// may not have arrived yet (an OK carries no command echo, so a late one would
    /// pass for the next set command's). A link that never goes quiet is read for
    /// at most four timeouts.
    fn drain(&mut self) -> Result<()> {
        self.buf.clear();
        if std::mem::take(&mut self.resync) {
            let give_up = Instant::now() + self.timeout * 4;
            let mut quiet_until = Instant::now() + self.timeout;
            let mut chunk = [0u8; 64];
            while Instant::now() < quiet_until.min(give_up) {
                match self.port.read(&mut chunk) {
                    Ok(0) => {}
                    Ok(_) => quiet_until = Instant::now() + self.timeout,
                    Err(e) if is_quiet(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.port.discard_input()?;
        Ok(())
    }

    /// Send `body` and return the first reply that `wanted` accepts, or NG. Echoes
    /// of our own frame (USB echo back), frames addressed elsewhere and replies
    /// that do not fit this command (left over from an earlier one) are skipped.
    ///
    /// Every frame sent and received is logged at trace level (`RUST_LOG=civ=trace`)
    /// with the time since the command went out, for checking on the bench what the
    /// manual leaves open (what 1C 00 and 1C 01 read while keying and tuning, when
    /// the OK to 17 arrives).
    fn transact(&mut self, body: &[u8], wanted: impl Fn(&Frame) -> bool) -> Result<Frame> {
        let out = Frame::new(self.addr, CONTROLLER, body).encode();
        self.drain()?;
        // Once anything may have gone out, a failure can leave a reply on its way.
        self.resync = true;
        self.port.write_all(&out)?;
        self.port.flush()?;
        let sent = Instant::now();
        log::trace!("CI-V > {out:02X?}");
        let deadline = sent + self.timeout;
        let mut chunk = [0u8; 64];
        loop {
            while let Some(f) = take_frame(&mut self.buf) {
                log::trace!(
                    "CI-V < {:02X?} after {} ms",
                    f.encode(),
                    sent.elapsed().as_millis()
                );
                if f.from == self.addr && f.to == CONTROLLER {
                    if f.is_ng() {
                        self.resync = false;
                        return Err(RigError::Rejected);
                    }
                    if wanted(&f) {
                        self.resync = false;
                        return Ok(f);
                    }
                }
            }
            if Instant::now() > deadline {
                return Err(RigError::Timeout);
            }
            match self.port.read(&mut chunk) {
                Ok(0) => {}
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if is_quiet(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn port(&self) -> &P {
        &self.port
    }

    /// A set command: the radio must answer OK.
    fn set(&mut self, cmd: &[u8], data: &[u8]) -> Result<()> {
        let mut body = cmd.to_vec();
        body.extend_from_slice(data);
        self.transact(&body, Frame::is_ok).map(|_| ())
    }

    /// A read command: returns the data after the echoed command bytes.
    fn read(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        let reply = self.transact(cmd, |f| f.body.starts_with(cmd))?;
        Ok(reply.body[cmd.len()..].to_vec())
    }

    /// Read a level or meter value: exactly two BCD bytes, 00 00 to 02 55.
    fn read_level(&mut self, cmd: &[u8]) -> Result<u16> {
        let data = self.read(cmd)?;
        match from_bcd_be(&data) {
            Some(v) if data.len() == 2 && v <= 255 => Ok(v as u16),
            _ => Err(RigError::Protocol(format!("{cmd:02X?} level {data:02X?}"))),
        }
    }

    /// Read a one-byte setting whose value must be in `valid`.
    fn read_byte(&mut self, cmd: &[u8], valid: RangeInclusive<u8>) -> Result<u8> {
        match self.read(cmd)?[..] {
            [v] if valid.contains(&v) => Ok(v),
            ref data => Err(RigError::Protocol(format!("{cmd:02X?} value {data:02X?}"))),
        }
    }

    fn read_usb_line(&mut self, cmd: &[u8]) -> Result<UsbLine> {
        Ok(match self.read_byte(cmd, 0x00..=0x02)? {
            0x00 => UsbLine::Off,
            0x01 => UsbLine::Dtr,
            _ => UsbLine::Rts,
        })
    }

    /// 19 00: the transceiver ID; an IC-7300 answers [`IC7300_ID`].
    pub fn transceiver_id(&mut self) -> Result<u8> {
        self.read_byte(cmd::TRANSCEIVER_ID, 0x00..=0xFF)
    }

    /// 04: operating mode and filter, as raw codes ("03: CW", "07: CW-R"; filter
    /// "01: FIL1" to "03: FIL3", p. 19-9).
    pub fn read_mode(&mut self) -> Result<(u8, u8)> {
        match self.read(cmd::READ_MODE)?[..] {
            [mode @ (0x00..=0x05 | 0x07 | 0x08), filter @ 0x01..=0x03] => Ok((mode, filter)),
            ref data => Err(RigError::Protocol(format!("mode {data:02X?}"))),
        }
    }

    /// 0F: whether split is on (transmit on the other VFO).
    pub fn split(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::SPLIT, 0x00..=0x01)? == 0x01)
    }

    /// 21 02: whether ∂TX is on (transmit frequency shifted from the dial).
    pub fn delta_tx(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::DELTA_TX, 0x00..=0x01)? == 0x01)
    }

    /// 14 0A: the [RF PWR] level, 0-255.
    pub fn rf_power_level(&mut self) -> Result<u16> {
        self.read_level(cmd::RF_POWER)
    }

    /// 14 0C: the [KEY SPEED] level, 0-255.
    pub fn key_speed_level(&mut self) -> Result<u16> {
        self.read_level(cmd::KEY_SPEED)
    }

    /// 14 0F: the Break-IN Delay level, 0-255.
    pub fn break_in_delay_level(&mut self) -> Result<u16> {
        self.read_level(cmd::BREAK_IN_DELAY)
    }

    /// 16 47: "00=BK-IN OFF, 01=Semi BK-IN ON, 02=Full BK-IN ON".
    pub fn break_in(&mut self) -> Result<u8> {
        self.read_byte(cmd::BREAK_IN, 0x00..=0x02)
    }

    /// 1C 01: "00" tuner off, "01" on, "02" tuning.
    pub fn tuner_state(&mut self) -> Result<u8> {
        self.read_byte(cmd::TUNER, 0x00..=0x02)
    }

    /// 1A 05 00 29: the Time-Out Timer (CI-V) setting, 0 (OFF) or 1-5 (3 to 30 min).
    pub fn time_out_timer(&mut self) -> Result<u8> {
        self.read_byte(cmd::TIME_OUT_TIMER, 0x00..=0x05)
    }

    /// 1A 05 00 78, 00 79, 00 80: the USB control-line settings.
    pub fn usb_lines(&mut self) -> Result<UsbLines> {
        Ok(UsbLines {
            send: self.read_usb_line(cmd::USB_SEND)?,
            keying_cw: self.read_usb_line(cmd::USB_KEYING_CW)?,
            keying_rtty: self.read_usb_line(cmd::USB_KEYING_RTTY)?,
        })
    }

    /// 1A 05 01 97: whether the Inhibit Timer at USB Connection is on.
    pub fn usb_inhibit_timer(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::USB_INHIBIT_TIMER, 0x00..=0x01)? == 0x01)
    }

    /// 1A 05 00 75, raw: "00=ON, 01=OFF" by the command table (p. 19-5). Reported,
    /// not relied on: the driver works with echo on or off.
    pub fn usb_echo_back_raw(&mut self) -> Result<u8> {
        self.read_byte(cmd::USB_ECHO_BACK, 0x00..=0x01)
    }

    /// 1C 03: the frequency the radio would transmit on, which differs from the
    /// operating frequency with split or ∂TX on.
    pub fn transmit_frequency(&mut self) -> Result<u64> {
        let data = self.read(cmd::TX_FREQ)?;
        parse_frequency(&data)
    }

    /// 27 11: whether scope waveform data output is on.
    pub fn scope_data_output(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::SCOPE_DATA_OUTPUT, 0x00..=0x01)? == 0x01)
    }

    /// 1A 05 00 84: whether meter peak hold is on.
    pub fn meter_peak_hold(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::METER_PEAK_HOLD, 0x00..=0x01)? == 0x01)
    }

    /// 1A 05 01 61: the keyer's dash length in dots, 2.8 to 4.5 (BCD "28" to "45").
    pub fn keyer_ratio(&mut self) -> Result<f32> {
        let v = self.read_byte(cmd::KEYER_RATIO, 0x28..=0x45)?;
        match from_bcd_be(&[v]) {
            Some(n) => Ok(n as f32 / 10.0),
            None => Err(RigError::Protocol(format!("keyer ratio {v:02X}"))),
        }
    }

    /// 1A 05 00 71: whether CI-V Transceive is on.
    pub fn civ_transceive(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::CIV_TRANSCEIVE, 0x00..=0x01)? == 0x01)
    }

    /// 1A 05 00 74: whether the USB CI-V port is on its own ("Unlink from
    /// [REMOTE]"), so that only this port's controller talks to the radio and the
    /// USB baud rate and echo items apply (p. 12-10, line 6853).
    pub fn civ_usb_unlinked(&mut self) -> Result<bool> {
        Ok(self.read_byte(cmd::CIV_USB_PORT, 0x00..=0x01)? == 0x01)
    }
}

impl<P: Port> Rig for Ic7300<P> {
    fn frequency(&mut self) -> Result<u64> {
        let data = self.read(cmd::READ_FREQ)?;
        parse_frequency(&data)
    }

    fn set_frequency(&mut self, hz: u64) -> Result<()> {
        if !FREQUENCY_RANGE_HZ.contains(&hz) {
            return Err(RigError::Protocol(format!(
                "{hz} Hz is outside the radio's 30 kHz-74.8 MHz"
            )));
        }
        self.set(cmd::SET_FREQ, &bcd_le(hz, 5))
    }

    fn set_mode_cw(&mut self) -> Result<()> {
        self.set(cmd::SET_MODE, &[cmd::MODE_CW, cmd::FILTER_1])
    }

    fn set_rf_power_watts(&mut self, watts: u32) -> Result<()> {
        self.set(cmd::RF_POWER, &bcd_be(power_level(watts) as u64, 2))
    }

    fn set_key_speed(&mut self, wpm: u32) -> Result<()> {
        let level = key_speed_level(wpm);
        self.key_level = None;
        self.set(cmd::KEY_SPEED, &bcd_be(level as u64, 2))?;
        self.key_level = Some(level);
        Ok(())
    }

    fn set_break_in(&mut self, on: bool) -> Result<()> {
        self.set(
            cmd::BREAK_IN,
            &[if on {
                cmd::BREAK_IN_SEMI
            } else {
                cmd::BREAK_IN_OFF
            }],
        )
    }

    fn set_break_in_delay(&mut self, dots: f32) -> Result<()> {
        self.set(
            cmd::BREAK_IN_DELAY,
            &bcd_be(break_in_delay_level(dots) as u64, 2),
        )
    }

    fn dot_duration(&mut self) -> Result<Duration> {
        let level = match self.key_level {
            Some(l) => l,
            None => {
                let l = self.read_level(cmd::KEY_SPEED)?;
                self.key_level = Some(l);
                l
            }
        };
        Ok(Duration::from_secs_f32(1.2 / key_speed_wpm(level)))
    }

    fn start_tune(&mut self) -> Result<()> {
        self.set(cmd::TUNER, &[cmd::TUNER_TUNE])
    }

    fn tuner_busy(&mut self) -> Result<bool> {
        Ok(self.tuner_state()? == cmd::TUNER_TUNE)
    }

    /// The tuner reads "01" (ON) after a match, "00" (OFF) once it has bypassed
    /// itself (1C 01, p. 19-7; p. 11-2).
    fn tuner_matched(&mut self) -> Result<bool> {
        Ok(self.tuner_state()? == 0x01)
    }

    /// 1C 03 "Read transmit frequency" (p. 19-7).
    fn transmit_frequency(&mut self) -> Result<u64> {
        Ic7300::transmit_frequency(self)
    }

    /// 0F "Read Split setting" (p. 19-3) and 21 02 "Send/read ∂TX setting" (p. 19-7).
    fn split_or_delta_tx(&mut self) -> Result<bool> {
        Ok(self.split()? || self.delta_tx()?)
    }

    fn read_swr(&mut self) -> Result<f32> {
        Ok(swr_from_meter(self.read_level(cmd::SWR_METER)?))
    }

    fn read_po(&mut self) -> Result<f32> {
        Ok(po_from_meter(self.read_level(cmd::PO_METER)?))
    }

    fn send_cw(&mut self, text: &str) -> Result<()> {
        if text.is_empty() || text.len() > MAX_CW_CHARS || !text.chars().all(cw::is_sendable) {
            return Err(RigError::Protocol(format!("cannot key {text:?}")));
        }
        self.set(cmd::SEND_CW, text.to_ascii_uppercase().as_bytes())
    }

    fn stop_cw(&mut self) -> Result<()> {
        self.set(cmd::SEND_CW, &[cmd::STOP_CW])
    }

    fn is_transmitting(&mut self) -> Result<bool> {
        // "00" receive, "01" transmit (p. 19-7). Anything else is an error, which
        // callers treat as "not confirmed on receive".
        Ok(self.read_byte(cmd::TX_STATUS, 0x00..=0x01)? == 0x01)
    }

    /// Only `false` is accepted: `1C 00 01` would hold the transmitter on with no
    /// time limit in the driver, and the node never needs it (CW is keyed with 17).
    fn set_transmit(&mut self, tx: bool) -> Result<()> {
        if tx {
            return Err(RigError::Protocol(
                "refusing to force transmit (1C 00 01)".into(),
            ));
        }
        self.set(cmd::TX_STATUS, &[0x00])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A fake serial port: records what is written, and answers each frame from a
    /// script (with optional USB echo).
    struct Script {
        written: Vec<u8>,
        replies: VecDeque<Vec<u8>>,
        pending: VecDeque<u8>,
        echo: bool,
        /// Bytes still on their way: not seen by `discard_input`, readable (ahead of
        /// anything sent after them) on the next read.
        late: Vec<u8>,
        /// Bytes that arrive at a given time, in order.
        timed: VecDeque<(Instant, Vec<u8>)>,
        /// When set, each scripted reply arrives this long after its command.
        reply_delay: Option<Duration>,
    }

    impl Port for Script {
        fn discard_input(&mut self) -> std::io::Result<()> {
            self.pending.clear();
            Ok(())
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            for b in self.late.drain(..).rev() {
                self.pending.push_front(b);
            }
            while self.timed.front().is_some_and(|t| t.0 <= Instant::now()) {
                self.pending.extend(self.timed.pop_front().unwrap().1);
            }
            let n = buf.len().min(self.pending.len());
            for b in buf.iter_mut().take(n) {
                *b = self.pending.pop_front().unwrap();
            }
            if n == 0 {
                // Like the serial port's read timeout, shortened.
                std::thread::sleep(Duration::from_millis(5));
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            Ok(n)
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            if self.echo {
                self.pending.extend(buf);
            }
            if let Some(r) = self.replies.pop_front() {
                match self.reply_delay {
                    Some(d) => self.timed.push_back((Instant::now() + d, r)),
                    None => self.pending.extend(r),
                }
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn radio(replies: &[&[u8]], echo: bool) -> Ic7300<Script> {
        let replies = replies
            .iter()
            .map(|b| Frame::new(CONTROLLER, 0x94, b).encode())
            .collect();
        Ic7300::with_port(
            Script {
                written: vec![],
                replies,
                pending: VecDeque::new(),
                echo,
                late: vec![],
                timed: VecDeque::new(),
                reply_delay: None,
            },
            0x94,
        )
    }

    fn reply(body: &[u8]) -> Vec<u8> {
        Frame::new(CONTROLLER, 0x94, body).encode()
    }

    const OK: &[u8] = &[0xFB];

    #[test]
    fn frames_on_the_wire() {
        let mut r = radio(&[OK, OK, OK, OK, OK, OK, OK, OK], false);
        r.set_frequency(7_030_000).unwrap();
        r.set_mode_cw().unwrap();
        r.set_rf_power_watts(40).unwrap();
        r.set_key_speed(18).unwrap();
        r.set_break_in(true).unwrap();
        r.send_cw("CQ DE N0CALL").unwrap();
        r.stop_cw().unwrap();
        r.set_break_in_delay(10.0).unwrap();
        let w = r.port.written.clone();
        let mut buf = w.clone();
        let frames: Vec<Vec<u8>> = std::iter::from_fn(|| take_frame(&mut buf))
            .map(|f| f.body)
            .collect();
        assert_eq!(frames[0], [0x05, 0x00, 0x00, 0x03, 0x07, 0x00]);
        assert_eq!(frames[1], [0x06, 0x03, 0x01]);
        assert_eq!(frames[2], [0x14, 0x0A, 0x01, 0x02]); // 40 W -> level 102
        assert_eq!(frames[3], [0x14, 0x0C, 0x00, 0x73]); // 18 wpm -> level 73
        assert_eq!(frames[4], [0x16, 0x47, 0x01]);
        assert_eq!(&frames[5][..1], [0x17]);
        assert_eq!(&frames[5][1..], b"CQ DE N0CALL");
        assert_eq!(frames[6], [0x17, 0xFF]);
        assert_eq!(frames[7], [0x14, 0x0F, 0x01, 0x85]); // 10.0 dots -> level 185
        assert_eq!(&w[..4], [0xFE, 0xFE, 0x94, 0xE0]);
        // The keyer speed actually set (level 73) gives the dot length.
        let dot = r.dot_duration().unwrap();
        assert!((dot.as_secs_f32() - 1.2 / key_speed_wpm(73)).abs() < 1e-6);
    }

    #[test]
    fn transmit_control_and_read_frames_on_the_wire() {
        let mut r = radio(
            &[
                OK,
                OK,
                &[0x1C, 0x00, 0x00],
                &[0x1C, 0x01, 0x02],
                &[0x03, 0x00, 0x00, 0x03, 0x07, 0x00],
                &[0x1C, 0x03, 0x00, 0x00, 0x03, 0x07, 0x00],
                &[0x15, 0x11, 0x01, 0x43],
                &[0x15, 0x12, 0x00, 0x48],
                &[0x19, 0x00, 0x94],
            ],
            false,
        );
        r.set_transmit(false).unwrap();
        r.start_tune().unwrap();
        assert!(!r.is_transmitting().unwrap());
        assert!(r.tuner_busy().unwrap());
        assert_eq!(r.frequency().unwrap(), 7_030_000);
        assert_eq!(r.transmit_frequency().unwrap(), 7_030_000);
        assert!((r.read_po().unwrap() - 50.0).abs() < 1e-3);
        assert!((r.read_swr().unwrap() - 1.5).abs() < 1e-6);
        assert_eq!(r.transceiver_id().unwrap(), 0x94);
        let mut buf = r.port.written.clone();
        let frames: Vec<Vec<u8>> = std::iter::from_fn(|| take_frame(&mut buf))
            .map(|f| f.body)
            .collect();
        let expected: [&[u8]; 9] = [
            &[0x1C, 0x00, 0x00], // receive; 1C 00 01 is never sent
            &[0x1C, 0x01, 0x02], // one tuner cycle
            &[0x1C, 0x00],
            &[0x1C, 0x01],
            &[0x03],
            &[0x1C, 0x03],
            &[0x15, 0x11],
            &[0x15, 0x12],
            &[0x19, 0x00],
        ];
        assert_eq!(frames, expected);
    }

    #[test]
    fn dot_length_follows_the_radio_speed() {
        // Unknown speed: read 14 0C. 02 55 = 48 wpm.
        let mut r = radio(&[&[0x14, 0x0C, 0x02, 0x55], OK], false);
        assert!((r.dot_duration().unwrap().as_secs_f32() - 0.025).abs() < 1e-6);
        // Above the radio's range the keyer runs at 48 wpm, not 60.
        r.set_key_speed(60).unwrap();
        assert!((r.dot_duration().unwrap().as_secs_f32() - 0.025).abs() < 1e-6);
    }

    #[test]
    fn reads_with_and_without_usb_echo() {
        for echo in [false, true] {
            let mut r = radio(
                &[
                    &[0x03, 0x00, 0x00, 0x03, 0x07, 0x00],
                    &[0x15, 0x12, 0x00, 0x80],
                    &[0x1C, 0x00, 0x01],
                ],
                echo,
            );
            assert_eq!(r.frequency().unwrap(), 7_030_000);
            assert!((r.read_swr().unwrap() - 2.0).abs() < 1e-6);
            assert!(r.is_transmitting().unwrap());
        }
    }

    #[test]
    fn meter_replies_must_be_two_bytes() {
        let mut r = radio(
            &[
                &[0x15, 0x12],
                &[0x15, 0x12, 0x00],
                &[0x15, 0x12, 0x00, 0x00, 0x00],
                &[0x15, 0x12, 0x03, 0x00],
                &[0x15, 0x11, 0x02, 0x13],
                &[0x15, 0x11, 0x00],
            ],
            false,
        );
        for _ in 0..4 {
            assert!(matches!(r.read_swr(), Err(RigError::Protocol(_))));
        }
        assert!((r.read_po().unwrap() - 100.0).abs() < 1e-4);
        assert!(matches!(r.read_po(), Err(RigError::Protocol(_))));
    }

    #[test]
    fn stale_input_is_not_taken_as_the_reply() {
        // A late OK from a command that timed out is waiting in the buffer; the
        // real answer to this command is NG.
        let mut r = radio(&[&[0xFA]], false);
        r.port.pending.extend(reply(OK));
        assert!(matches!(r.set_transmit(false), Err(RigError::Rejected)));

        // A stale status reply arrives just ahead of the OK: it is skipped, not
        // reported as a protocol error.
        let mut r = radio(&[], false);
        let mut both = reply(&[0x1C, 0x00, 0x01]);
        both.extend(reply(OK));
        r.port.replies.push_back(both);
        r.stop_cw().unwrap();

        // Likewise a stale OK ahead of the data a read asked for.
        let mut r = radio(&[], false);
        let mut both = reply(OK);
        both.extend(reply(&[0x1C, 0x00, 0x00]));
        r.port.replies.push_back(both);
        assert!(!r.is_transmitting().unwrap());
    }

    #[test]
    fn link_is_drained_after_a_timeout() {
        let mut r = radio(&[], false);
        r.timeout = Duration::from_millis(20);
        assert!(matches!(r.stop_cw(), Err(RigError::Timeout)));
        // The late OK to stop_cw turns up after the timeout, while the next command
        // is being prepared; the radio rejects that next command.
        r.port.late = reply(OK);
        r.port.replies.push_back(reply(&[0xFA]));
        assert!(matches!(r.set_transmit(false), Err(RigError::Rejected)));
    }

    #[test]
    fn late_reply_after_a_pause_is_not_taken_as_the_next_one() {
        // The driver's own timeout, so that a slow test machine has the same margins
        // as the driver: hundreds of milliseconds.
        let mut r = radio(&[], false);
        r.timeout = Duration::from_millis(500);
        // The OK to stop_cw arrives 800 ms after it was sent: after the timeout and
        // after many quiet serial reads.
        r.port
            .timed
            .push_back((Instant::now() + Duration::from_millis(800), reply(OK)));
        assert!(matches!(r.stop_cw(), Err(RigError::Timeout)));
        // The radio answers the next command NG, slowly.
        r.port.reply_delay = Some(Duration::from_millis(150));
        r.port.replies.push_back(reply(&[0xFA]));
        assert!(matches!(r.set_transmit(false), Err(RigError::Rejected)));
    }

    #[test]
    fn ng_and_silence_are_errors() {
        let mut r = radio(&[&[0xFA]], false);
        assert!(matches!(r.set_break_in(true), Err(RigError::Rejected)));
        let mut r = radio(&[], false);
        r.timeout = Duration::from_millis(20);
        assert!(matches!(r.frequency(), Err(RigError::Timeout)));
    }

    #[test]
    fn never_forces_transmit_on() {
        let mut r = radio(&[OK, OK], false);
        assert!(matches!(r.set_transmit(true), Err(RigError::Protocol(_))));
        assert!(r.port.written.is_empty(), "nothing sent");
        r.set_transmit(false).unwrap();
        assert_eq!(
            r.port.written,
            [0xFE, 0xFE, 0x94, 0xE0, 0x1C, 0x00, 0x00, 0xFD]
        );
    }

    #[test]
    fn frequencies_outside_the_radio_are_not_sent() {
        let mut r = radio(&[OK, OK], false);
        for hz in [0, 29_999, 74_800_001, 100_000_000, 10_000_000_000] {
            assert!(r.set_frequency(hz).is_err(), "{hz}");
        }
        assert!(r.port.written.is_empty());
        r.set_frequency(30_000).unwrap();
        r.set_frequency(74_800_000).unwrap();
        let mut w = r.port.written.clone();
        let bodies: Vec<_> = std::iter::from_fn(|| take_frame(&mut w))
            .map(|f| f.body)
            .collect();
        assert_eq!(bodies[0], [0x05, 0x00, 0x00, 0x03, 0x00, 0x00]);
        assert_eq!(bodies[1], [0x05, 0x00, 0x00, 0x80, 0x74, 0x00]);
    }

    #[test]
    fn replies_must_have_the_documented_shape() {
        let mut r = radio(
            &[
                &[0x03, 0x00, 0x00, 0x03, 0x07],             // 4 bytes
                &[0x03, 0x00, 0x00, 0x03, 0x07, 0x00, 0x00], // 6 bytes
                &[0x03, 0x00, 0x00, 0x03, 0x07, 0x01],       // 100 MHz digit
                &[0x1C, 0x00],
                &[0x1C, 0x00, 0x02],
                &[0x1C, 0x00, 0x00, 0x00],
                &[0x1C, 0x01, 0x03],
                &[0x1C, 0x01],
            ],
            false,
        );
        for _ in 0..3 {
            assert!(matches!(r.frequency(), Err(RigError::Protocol(_))));
        }
        for _ in 0..3 {
            assert!(matches!(r.is_transmitting(), Err(RigError::Protocol(_))));
        }
        for _ in 0..2 {
            assert!(matches!(r.tuner_busy(), Err(RigError::Protocol(_))));
        }
    }

    #[test]
    fn link_settings_are_the_radios() {
        for baud in USB_BAUD_RATES {
            check_link_settings(baud, 0x94).unwrap();
        }
        assert!(check_link_settings(115_201, 0x94).is_err());
        assert!(check_link_settings(0, 0x94).is_err());
        check_link_settings(19_200, 0x02).unwrap();
        check_link_settings(19_200, 0xDF).unwrap();
        for addr in [0x00, 0x01, 0xE0, 0xFD, 0xFE] {
            assert!(check_link_settings(19_200, addr).is_err(), "{addr:02X}");
        }
        // Valid settings: the open itself is tried, and fails on a missing port.
        assert!(matches!(
            Ic7300::open("/nonexistent", 19_200, 0x94),
            Err(RigError::Io(_))
        ));
    }

    /// A port that logs, in order, its opening, what is done to its control lines
    /// and every write, and fails to lower a line where told to.
    struct Opened {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail_dtr: bool,
        fail_rts: bool,
    }

    impl Opened {
        fn note(&self, what: impl Into<String>) {
            self.log.lock().unwrap().push(what.into());
        }
    }

    impl ControlLines for Opened {
        fn set_dtr(&mut self, level: bool) -> std::io::Result<()> {
            self.note(format!("DTR {}", if level { "up" } else { "down" }));
            match self.fail_dtr {
                true => Err(std::io::Error::other("DTR stuck")),
                false => Ok(()),
            }
        }
        fn set_rts(&mut self, level: bool) -> std::io::Result<()> {
            self.note(format!("RTS {}", if level { "up" } else { "down" }));
            match self.fail_rts {
                true => Err(std::io::Error::other("RTS stuck")),
                false => Ok(()),
            }
        }
    }

    impl Port for Opened {
        fn discard_input(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for Opened {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
    }

    impl Write for Opened {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.note(format!("write {buf:02X?}"));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Open through [`Ic7300::open_with`]; what happened, in order, by the time it
    /// returned.
    fn open_logged(fail_dtr: bool, fail_rts: bool, addr: u8) -> (Result<()>, Vec<String>) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port_log = log.clone();
        let r = Ic7300::open_with(
            move || {
                port_log.lock().unwrap().push("open".into());
                Ok(Opened {
                    log: port_log,
                    fail_dtr,
                    fail_rts,
                })
            },
            115_200,
            addr,
        );
        let snapshot = log.lock().unwrap().clone();
        (r.map(drop), snapshot)
    }

    #[test]
    fn open_lowers_dtr_then_rts_before_the_driver_is_returned() {
        let (r, log) = open_logged(false, false, 0x94);
        r.unwrap();
        assert_eq!(log, ["open", "DTR down", "RTS down"]);
    }

    #[test]
    fn open_fails_if_either_line_cannot_be_lowered() {
        let (r, log) = open_logged(true, false, 0x94);
        assert!(r.unwrap_err().to_string().contains("DTR"));
        assert_eq!(log, ["open", "DTR down"], "nothing after DTR fails");
        let (r, log) = open_logged(false, true, 0x94);
        assert!(r.unwrap_err().to_string().contains("RTS"));
        assert_eq!(log, ["open", "DTR down", "RTS down"]);
    }

    #[test]
    fn open_refuses_bad_link_settings_before_opening() {
        let (r, log) = open_logged(false, false, 0xE0);
        assert!(r.is_err());
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn refuses_unkeyable_text() {
        let mut r = radio(&[OK], false);
        assert!(r.send_cw(&"E".repeat(31)).is_err());
        assert!(r.send_cw("HI #1").is_err());
        assert!(r.port.written.is_empty());
    }

    #[test]
    fn scales() {
        assert_eq!(swr_from_meter(0), 1.0);
        assert!((swr_from_meter(64) - 1.75).abs() < 1e-6);
        assert_eq!(swr_from_meter(120), 3.0);
        assert!(swr_from_meter(160) > 3.0);
        assert_eq!(po_from_meter(0), 0.0);
        assert_eq!(po_from_meter(143), 50.0);
        assert_eq!(po_from_meter(213), 100.0);
        assert_eq!(break_in_delay_level(2.0), 0);
        assert_eq!(break_in_delay_level(13.0), 255);
        assert_eq!(break_in_delay_level(10.0), 185);
        assert_eq!(key_speed_level(6), 0);
        assert_eq!(key_speed_level(48), 255);
        assert_eq!(power_level(100), 255);
    }
}
