//! CI-V framing: `FE FE <to> <from> <cmd> [<sub>] [<data>...] FD`.
//!
//! The frame format, the OK (FB) / NG (FA) replies and the default controller address
//! (E0) are common to ICOM's CI-V transceivers; see the "Data format" section of the
//! IC-7300 CI-V reference guide.

pub const PREAMBLE: u8 = 0xFE;
pub const END: u8 = 0xFD;
pub const OK: u8 = 0xFB;
pub const NG: u8 = 0xFA;
/// Default controller (PC) address.
pub const CONTROLLER: u8 = 0xE0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub to: u8,
    pub from: u8,
    /// Command byte followed by sub-command and data, exactly as on the wire.
    pub body: Vec<u8>,
}

impl Frame {
    pub fn new(to: u8, from: u8, body: &[u8]) -> Self {
        Self {
            to,
            from,
            body: body.to_vec(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.body.len() + 5);
        v.extend([PREAMBLE, PREAMBLE, self.to, self.from]);
        v.extend(&self.body);
        v.push(END);
        v
    }

    pub fn is_ok(&self) -> bool {
        self.body == [OK]
    }

    pub fn is_ng(&self) -> bool {
        self.body == [NG]
    }
}

/// Pull the first complete frame out of `buf`, discarding any bytes before it.
/// Returns `None` (leaving a partial frame in place) if no complete frame is there.
pub fn take_frame(buf: &mut Vec<u8>) -> Option<Frame> {
    loop {
        let start = buf.windows(2).position(|w| w == [PREAMBLE, PREAMBLE])?;
        buf.drain(..start);
        // Skip extra preamble bytes.
        let mut i = 2;
        while i < buf.len() && buf[i] == PREAMBLE {
            i += 1;
        }
        let end = buf[i..].iter().position(|&b| b == END)? + i;
        let raw: Vec<u8> = buf.drain(..=end).collect();
        let inner = &raw[i..raw.len() - 1];
        if inner.len() >= 3 {
            return Some(Frame {
                to: inner[0],
                from: inner[1],
                body: inner[2..].to_vec(),
            });
        }
        // Too short to be a frame (noise or a collision): keep scanning.
    }
}

/// Encode `value` as `digits` BCD digits, least-significant byte first (the CI-V
/// frequency format: 1 Hz/10 Hz in the first byte).
pub fn bcd_le(value: u64, bytes: usize) -> Vec<u8> {
    let mut v = value;
    (0..bytes)
        .map(|_| {
            let lo = (v % 10) as u8;
            v /= 10;
            let hi = (v % 10) as u8;
            v /= 10;
            (hi << 4) | lo
        })
        .collect()
}

pub fn from_bcd_le(bytes: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for &b in bytes.iter().rev() {
        let (hi, lo) = (b >> 4, b & 0x0F);
        if hi > 9 || lo > 9 {
            return None;
        }
        v = v * 100 + (hi as u64) * 10 + lo as u64;
    }
    Some(v)
}

/// Big-endian BCD, as used for levels like `00 00`-`02 55`.
pub fn bcd_be(value: u64, bytes: usize) -> Vec<u8> {
    let mut v = bcd_le(value, bytes);
    v.reverse();
    v
}

pub fn from_bcd_be(bytes: &[u8]) -> Option<u64> {
    let mut b = bytes.to_vec();
    b.reverse();
    from_bcd_le(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_and_resync() {
        let f = Frame::new(0x94, CONTROLLER, &[0x03]);
        assert_eq!(f.encode(), [0xFE, 0xFE, 0x94, 0xE0, 0x03, 0xFD]);
        let mut buf = vec![0x00, 0x12];
        buf.extend(f.encode());
        buf.extend([0xFE, 0xFE, 0xE0, 0x94, 0xFB, 0xFD, 0xFE, 0xFE, 0xE0]);
        assert_eq!(take_frame(&mut buf), Some(f));
        let ok = take_frame(&mut buf).unwrap();
        assert!(ok.is_ok());
        assert_eq!(take_frame(&mut buf), None);
        assert_eq!(buf, [0xFE, 0xFE, 0xE0], "partial frame is kept");
    }

    #[test]
    fn bcd() {
        // 14.074 MHz = 0014074000 Hz -> 00 40 07 14 00 (LSB first).
        assert_eq!(bcd_le(14_074_000, 5), [0x00, 0x40, 0x07, 0x14, 0x00]);
        assert_eq!(
            from_bcd_le(&[0x00, 0x40, 0x07, 0x14, 0x00]),
            Some(14_074_000)
        );
        assert_eq!(bcd_be(128, 2), [0x01, 0x28]);
        assert_eq!(from_bcd_be(&[0x02, 0x55]), Some(255));
        assert_eq!(from_bcd_le(&[0xAB]), None);
    }
}
