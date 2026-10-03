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
//! Safety in this module: [`Ic7300::set_transmit`] is only ever called with
//! `false` by the node, CW is keyed through the radio's own keyer (command 17), and
//! every set command must be acknowledged with OK (FB) or it is treated as failed.
//! Input left over from an earlier command (a late reply after a timeout) is
//! discarded before each command, so it cannot be taken as the next one's answer.

use crate::frame::{bcd_be, bcd_le, from_bcd_be, from_bcd_le, take_frame, Frame, CONTROLLER};
use crate::{Result, Rig, RigError, MAX_CW_CHARS};
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

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
    /// Open the radio's USB serial port. `baud` must match the radio's CI-V USB
    /// baud rate setting.
    pub fn open(path: &str, baud: u32, addr: u8) -> Result<Self> {
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| RigError::Io(std::io::Error::other(e)))?;
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
    /// until the link goes quiet (one read timeout with nothing), since the late
    /// reply may not have arrived yet.
    fn drain(&mut self) -> Result<()> {
        self.buf.clear();
        if std::mem::take(&mut self.resync) {
            let deadline = Instant::now() + self.timeout;
            let mut chunk = [0u8; 64];
            while Instant::now() < deadline {
                match self.port.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if is_quiet(&e) => break,
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
    fn transact(&mut self, body: &[u8], wanted: impl Fn(&Frame) -> bool) -> Result<Frame> {
        let out = Frame::new(self.addr, CONTROLLER, body).encode();
        self.drain()?;
        self.port.write_all(&out)?;
        self.port.flush()?;
        let deadline = Instant::now() + self.timeout;
        let mut chunk = [0u8; 64];
        loop {
            while let Some(f) = take_frame(&mut self.buf) {
                if f.from == self.addr && f.to == CONTROLLER {
                    if f.is_ng() {
                        return Err(RigError::Rejected);
                    }
                    if wanted(&f) {
                        return Ok(f);
                    }
                }
            }
            if Instant::now() > deadline {
                self.resync = true;
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
}

impl<P: Port> Rig for Ic7300<P> {
    fn frequency(&mut self) -> Result<u64> {
        let data = self.read(cmd::READ_FREQ)?;
        from_bcd_le(&data).ok_or_else(|| RigError::Protocol(format!("frequency {data:02X?}")))
    }

    fn set_frequency(&mut self, hz: u64) -> Result<()> {
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
        Ok(self.read(cmd::TUNER)? == [cmd::TUNER_TUNE])
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
        Ok(self.read(cmd::TX_STATUS)? != [0x00])
    }

    fn set_transmit(&mut self, tx: bool) -> Result<()> {
        self.set(cmd::TX_STATUS, &[u8::from(tx)])
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
            let n = buf.len().min(self.pending.len());
            for b in buf.iter_mut().take(n) {
                *b = self.pending.pop_front().unwrap();
            }
            if n == 0 {
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
                self.pending.extend(r);
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
    fn ng_and_silence_are_errors() {
        let mut r = radio(&[&[0xFA]], false);
        assert!(matches!(r.set_break_in(true), Err(RigError::Rejected)));
        let mut r = radio(&[], false);
        r.timeout = Duration::from_millis(20);
        assert!(matches!(r.frequency(), Err(RigError::Timeout)));
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
