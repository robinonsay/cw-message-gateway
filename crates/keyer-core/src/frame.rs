//! Lines between hfnode and the box: printable ASCII ending in `\n` (a `\r` before
//! it is ignored), at most [`MAX_LINE`] characters before it:
//!
//! ```text
//! <id> <body>*<cs>
//! ```
//!
//! `<id>` is two uppercase hex digits that the reply repeats, so a late reply to an
//! earlier command is never taken for the answer to this one; `<cs>` is two
//! uppercase hex digits, the XOR of every byte before the `*`. The same format as
//! the handheld firmware's (docs/handheld-protocol.md). A line that does not decode
//! is ignored by both sides: no reply, nothing done.

use crate::MAX_LINE;
use core::fmt;

/// XOR of `bytes`.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |a, b| a ^ b)
}

/// A line, without its newline, in a fixed buffer.
#[derive(Clone, PartialEq, Eq)]
pub struct Line {
    buf: [u8; MAX_LINE],
    len: usize,
}

impl Line {
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_LINE],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII is ever written (`write_str` refuses anything else).
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl fmt::Write for Line {
    /// Fails, leaving the line as it was, if `s` would make it too long or is not
    /// printable ASCII.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let b = s.as_bytes();
        if self.len + b.len() > MAX_LINE || !b.iter().all(|&c| (0x20..=0x7E).contains(&c)) {
            return Err(fmt::Error);
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        Ok(())
    }
}

/// The line for `body` under `id`, without the newline; `None` if it would be
/// longer than [`MAX_LINE`] or `body` is not printable ASCII.
pub fn encode(id: u8, body: fmt::Arguments<'_>) -> Option<Line> {
    use fmt::Write;
    let mut l = Line::new();
    write!(l, "{id:02X} ").ok()?;
    l.write_fmt(body).ok()?;
    let cs = checksum(l.as_bytes());
    write!(l, "*{cs:02X}").ok()?;
    Some(l)
}

/// Why a received line was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooLong,
    NotAscii,
    NoChecksum,
    BadChecksum,
    NoId,
}

/// The id and body of a received line (without its newline; a trailing `\r` is
/// dropped), if it is well formed and its checksum matches.
pub fn decode(line: &[u8]) -> Result<(u8, &str), FrameError> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_LINE {
        return Err(FrameError::TooLong);
    }
    if !line.iter().all(|&c| (0x20..=0x7E).contains(&c)) {
        return Err(FrameError::NotAscii);
    }
    let star = line
        .iter()
        .rposition(|&c| c == b'*')
        .ok_or(FrameError::NoChecksum)?;
    let (msg, cs) = (&line[..star], &line[star + 1..]);
    let cs = hex2(cs).ok_or(FrameError::NoChecksum)?;
    if cs != checksum(msg) {
        return Err(FrameError::BadChecksum);
    }
    if msg.len() < 4 || msg[2] != b' ' {
        return Err(FrameError::NoId);
    }
    let id = hex2(&msg[..2]).ok_or(FrameError::NoId)?;
    // Printable ASCII, checked above.
    let body = core::str::from_utf8(&msg[3..]).map_err(|_| FrameError::NotAscii)?;
    Ok((id, body))
}

/// Two uppercase hex digits.
fn hex2(b: &[u8]) -> Option<u8> {
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    match b {
        [h, l] => Some(d(*h)? << 4 | d(*l)?),
        _ => None,
    }
}

/// Collects received bytes into lines. A line longer than [`MAX_LINE`] (a `\r`
/// before the newline allowed for) is dropped whole, up to its newline.
pub struct LineReader {
    buf: [u8; MAX_LINE + 1],
    len: usize,
    overflow: bool,
}

impl LineReader {
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_LINE + 1],
            len: 0,
            overflow: false,
        }
    }

    /// Add one byte; at a newline, the line it ends (without the newline), unless
    /// it was too long.
    pub fn push(&mut self, b: u8) -> Option<&[u8]> {
        if b == b'\n' {
            let (len, overflow) = (self.len, self.overflow);
            self.len = 0;
            self.overflow = false;
            return (!overflow).then_some(&self.buf[..len]);
        }
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        } else {
            self.overflow = true;
        }
        None
    }

    /// Forget a part-line, as after the link was lost.
    pub fn clear(&mut self) {
        self.len = 0;
        self.overflow = false;
    }
}

impl Default for LineReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_known_lines() {
        let l = encode(1, format_args!("STATUS")).unwrap();
        // The handheld protocol's own example, same format.
        assert_eq!(l.as_str(), "01 STATUS*35");
        assert_eq!(decode(l.as_bytes()), Ok((1, "STATUS")));
        let l = encode(0xAB, format_args!("CW {} {}", 20, "CQ DE N0CALL")).unwrap();
        assert_eq!(decode(l.as_bytes()), Ok((0xAB, "CW 20 CQ DE N0CALL")));
        let mut crlf = l.as_bytes().to_vec();
        crlf.push(b'\r');
        assert_eq!(decode(&crlf), Ok((0xAB, "CW 20 CQ DE N0CALL")));
    }

    #[test]
    fn damaged_lines_do_not_decode() {
        let good = encode(7, format_args!("CW 20 TEST")).unwrap();
        let s = good.as_str();
        // Every single-character change is caught, the checksum's own included.
        for i in 0..s.len() {
            let mut b = s.as_bytes().to_vec();
            for c in *b" 0AZ*~" {
                if b[i] == c {
                    continue;
                }
                b[i] = c;
                let d = decode(&b);
                assert!(
                    d.is_err() || d == Ok((7, "CW 20 TEST")) && b == s.as_bytes(),
                    "{:?} decoded as {d:?}",
                    String::from_utf8_lossy(&b)
                );
                b[i] = s.as_bytes()[i];
            }
        }
        assert_eq!(decode(b"07 CW 20 TEST"), Err(FrameError::NoChecksum));
        assert_eq!(decode(b"7 STATUS*0E"), Err(FrameError::BadChecksum));
        assert_eq!(
            decode(b"07 STOP*2f"),
            Err(FrameError::NoChecksum),
            "lower-case hex"
        );
        assert_eq!(decode("07 \u{e9}*00".as_bytes()), Err(FrameError::NotAscii));
        assert_eq!(decode(&[b'A'; 81]), Err(FrameError::TooLong));
        assert_eq!(decode(b""), Err(FrameError::NoChecksum));
    }

    #[test]
    fn encode_refuses_what_does_not_fit() {
        assert!(encode(1, format_args!("{}", "X".repeat(74))).is_some());
        assert!(encode(1, format_args!("{}", "X".repeat(75))).is_none());
        assert!(encode(1, format_args!("TAB\tHERE")).is_none());
        assert_eq!(
            encode(1, format_args!("{}", "X".repeat(74))).unwrap().len(),
            80
        );
    }

    #[test]
    fn reader_splits_lines_and_drops_long_ones() {
        let mut r = LineReader::new();
        let mut got = Vec::new();
        let mut feed = |r: &mut LineReader, bytes: &[u8]| {
            for &b in bytes {
                if let Some(l) = r.push(b) {
                    got.push(String::from_utf8_lossy(l).into_owned());
                }
            }
        };
        feed(&mut r, b"01 A*00\n02 B*00\r\n");
        feed(&mut r, &[b'X'; 200]);
        feed(&mut r, b"\n03 C*00\n");
        let long_ok = [b'Y'; 81];
        feed(&mut r, &long_ok);
        feed(&mut r, b"\n");
        assert_eq!(
            got,
            vec![
                "01 A*00".to_string(),
                "02 B*00\r".to_string(),
                "03 C*00".to_string(),
                "Y".repeat(81)
            ]
        );
    }
}
