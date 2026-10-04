//! The text of an iMessage kept only in `message.attributedBody`: an
//! NSAttributedString archived by NSArchiver ("typedstream"). Newer versions of
//! macOS often leave `message.text` empty and keep the text only there.
//!
//! Only as much of the format is read as reaches the string: the class name
//! `NSString`, the bytes that follow it, a length and the UTF-8 text. The layout is
//! recalled from other programs that read Messages' database, not from Apple's
//! documentation, so anything unexpected is an error rather than a guess, and the
//! decoder reads only with bounds checks: a damaged blob can never panic the node.

#![deny(clippy::indexing_slicing)]

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No `NSString` class name, or not where one would be.
    NoString,
    /// The bytes after the class name are not the ones expected.
    Layout,
    /// The length is cut off, negative or of an unknown kind.
    Length,
    /// The text runs past the end of the blob.
    Truncated,
    /// The text is not UTF-8.
    NotUtf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DecodeError::NoString => "no NSString in attributedBody",
            DecodeError::Layout => "unexpected bytes after NSString",
            DecodeError::Length => "bad string length",
            DecodeError::Truncated => "string runs past the end",
            DecodeError::NotUtf8 => "string is not UTF-8",
        })
    }
}

impl std::error::Error for DecodeError {}

const CLASS: &[u8] = b"NSString";

/// The text in an `attributedBody` blob, without attachment placeholders (U+FFFC)
/// and trimmed.
pub fn decode_attributed_body(blob: &[u8]) -> Result<String, DecodeError> {
    let at = blob
        .windows(CLASS.len())
        .position(|w| w == CLASS)
        .ok_or(DecodeError::NoString)?;
    // The class name is written with its length in front.
    let before = at.checked_sub(1).and_then(|i| blob.get(i));
    if before != Some(&0x08) {
        return Err(DecodeError::NoString);
    }
    let after = at.checked_add(CLASS.len()).ok_or(DecodeError::Layout)?;
    // Class version and reference (not checked: they vary), then the start of the
    // object, a one-item type list and "+", a C string.
    let fixed = blob
        .get(after..after.checked_add(5).ok_or(DecodeError::Layout)?)
        .ok_or(DecodeError::Layout)?;
    if fixed.get(2..5) != Some(&[0x84, 0x01, 0x2B][..]) {
        return Err(DecodeError::Layout);
    }
    let tag_at = after.checked_add(5).ok_or(DecodeError::Length)?;
    let (len, start) = match blob.get(tag_at).copied().ok_or(DecodeError::Length)? {
        n @ 0x00..=0x7F => (i64::from(n), tag_at.checked_add(1)),
        0x81 => {
            let b = read::<2>(blob, tag_at)?;
            (i64::from(i16::from_le_bytes(b)), tag_at.checked_add(3))
        }
        0x82 => {
            let b = read::<4>(blob, tag_at)?;
            (i64::from(i32::from_le_bytes(b)), tag_at.checked_add(5))
        }
        _ => return Err(DecodeError::Length),
    };
    let start = start.ok_or(DecodeError::Length)?;
    let len = usize::try_from(len).map_err(|_| DecodeError::Length)?;
    let end = start.checked_add(len).ok_or(DecodeError::Truncated)?;
    let bytes = blob.get(start..end).ok_or(DecodeError::Truncated)?;
    let text = std::str::from_utf8(bytes).map_err(|_| DecodeError::NotUtf8)?;
    Ok(text.replace('\u{FFFC}', "").trim().to_string())
}

/// The `N` bytes after the length tag at `tag_at`.
fn read<const N: usize>(blob: &[u8], tag_at: usize) -> Result<[u8; N], DecodeError> {
    let from = tag_at.checked_add(1).ok_or(DecodeError::Length)?;
    let to = from.checked_add(N).ok_or(DecodeError::Length)?;
    blob.get(from..to)
        .and_then(|b| b.try_into().ok())
        .ok_or(DecodeError::Length)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    const PRE: &[u8] = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01\x40\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString";

    /// An attributedBody as Messages writes one (as far as it is known).
    pub(crate) fn blob(text: &[u8], backref: u8) -> Vec<u8> {
        let mut b = PRE.to_vec();
        b.extend([0x01, backref, 0x84, 0x01, 0x2B]);
        let n = text.len();
        if n < 0x80 {
            b.push(n as u8)
        } else if n <= 0x7FFF {
            b.push(0x81);
            b.extend((n as i16).to_le_bytes())
        } else {
            b.push(0x82);
            b.extend((n as i32).to_le_bytes())
        }
        b.extend(text);
        b.extend([0x86, 0x84]);
        b
    }

    #[test]
    fn round_trips() {
        for n in [0usize, 5, 127, 128, 255, 256, 40_000, 70_000] {
            // Exactly n bytes, with a 4-byte emoji where there is room.
            let text = match n {
                0..=3 => "x".repeat(n),
                _ => format!("😀{}", "x".repeat(n - 4)),
            };
            assert_eq!(text.len(), n);
            assert_eq!(
                decode_attributed_body(&blob(text.as_bytes(), 0x94)).unwrap(),
                text,
                "{n}"
            );
        }
        let mixed = "Ça va? 字 Привет 😀";
        assert_eq!(
            decode_attributed_body(&blob(mixed.as_bytes(), 0x94)).unwrap(),
            mixed
        );
        assert_eq!(
            decode_attributed_body(&blob("See you Sunday".as_bytes(), 0x95)).unwrap(),
            "See you Sunday"
        );
        assert_eq!(
            decode_attributed_body(&blob("\u{FFFC}photo\u{FFFC} ".as_bytes(), 0x94)).unwrap(),
            "photo"
        );
    }

    #[test]
    fn damaged_blobs_are_errors() {
        let good = blob(b"hello there", 0x94);
        let tag = PRE.len() + 5;
        let cases: Vec<(Vec<u8>, DecodeError)> = vec![
            (Vec::new(), DecodeError::NoString),
            (b"no class here".to_vec(), DecodeError::NoString),
            // Cut inside the length, then inside the text.
            (blob(&[b'x'; 300], 0x94)[..tag + 2].to_vec(), DecodeError::Length),
            (good[..tag + 4].to_vec(), DecodeError::Truncated),
            // A length one past the end.
            (
                {
                    let mut b = good[..tag + 1 + 11].to_vec();
                    b[tag] = 12;
                    b
                },
                DecodeError::Truncated,
            ),
            (
                {
                    let mut b = good.clone();
                    b.splice(tag..tag + 1, [0x81, 0xFF, 0xFF]);
                    b
                },
                DecodeError::Length,
            ),
            (
                {
                    let mut b = good.clone();
                    b[tag] = 0x83;
                    b
                },
                DecodeError::Length,
            ),
            (blob(b"\xC3\x28", 0x94), DecodeError::NotUtf8),
            (
                {
                    let mut b = good.clone();
                    b[PRE.len() - CLASS.len() - 1] = 0x07;
                    b
                },
                DecodeError::NoString,
            ),
            (
                {
                    let mut b = good.clone();
                    b[PRE.len() + 3] = 0x02;
                    b
                },
                DecodeError::Layout,
            ),
        ];
        for (i, (b, want)) in cases.iter().enumerate() {
            assert_eq!(decode_attributed_body(b).as_ref(), Err(want), "case {i}");
        }
        // Every prefix of a good blob is an error or the text, never a panic.
        for n in 0..good.len() {
            let _ = decode_attributed_body(&good[..n]);
        }
    }
}
