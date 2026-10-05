//! The Morse the box keys: the International Morse code table (ITU-R M.1677-1, the
//! same characters as hfnode's decoder and the IC-7300's keyer), and standard
//! timing: a dot of `1200 / wpm` ms (PARIS, rounded down to whole ms), a dash of 3,
//! 1 between the elements of a character, 3 between characters, 7 between words.
//!
//! hfnode computes the same timeline to know when the box keys each element.

use crate::{MAX_TEXT, MAX_WPM, MIN_WPM};

/// (character, pattern).
const TABLE: &[(u8, &[u8])] = &[
    (b'A', b".-"),
    (b'B', b"-..."),
    (b'C', b"-.-."),
    (b'D', b"-.."),
    (b'E', b"."),
    (b'F', b"..-."),
    (b'G', b"--."),
    (b'H', b"...."),
    (b'I', b".."),
    (b'J', b".---"),
    (b'K', b"-.-"),
    (b'L', b".-.."),
    (b'M', b"--"),
    (b'N', b"-."),
    (b'O', b"---"),
    (b'P', b".--."),
    (b'Q', b"--.-"),
    (b'R', b".-."),
    (b'S', b"..."),
    (b'T', b"-"),
    (b'U', b"..-"),
    (b'V', b"...-"),
    (b'W', b".--"),
    (b'X', b"-..-"),
    (b'Y', b"-.--"),
    (b'Z', b"--.."),
    (b'0', b"-----"),
    (b'1', b".----"),
    (b'2', b"..---"),
    (b'3', b"...--"),
    (b'4', b"....-"),
    (b'5', b"....."),
    (b'6', b"-...."),
    (b'7', b"--..."),
    (b'8', b"---.."),
    (b'9', b"----."),
    (b'.', b".-.-.-"),
    (b',', b"--..--"),
    (b'?', b"..--.."),
    (b'\'', b".----."),
    (b'/', b"-..-."),
    (b'(', b"-.--."),
    (b')', b"-.--.-"),
    (b':', b"---..."),
    (b'=', b"-...-"),
    (b'+', b".-.-."),
    (b'-', b"-....-"),
    (b'"', b".-..-."),
    (b'@', b".--.-."),
];

/// The most elements in one character.
pub const MAX_ELEMENTS: usize = 6;

/// The most segments (elements and the gaps between them) of [`MAX_TEXT`]
/// characters: each character at most 6 elements and 5 gaps, and a gap after it.
pub const MAX_SEGMENTS: usize = MAX_TEXT * (2 * MAX_ELEMENTS);

/// The dot/dash pattern for `c` (upper case only, as the box takes it).
pub fn pattern(c: u8) -> Option<&'static [u8]> {
    TABLE.iter().find(|(ch, _)| *ch == c).map(|(_, p)| *p)
}

/// Why text cannot be keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextError {
    /// Empty (or only spaces), or more than [`MAX_TEXT`] characters.
    Len,
    /// A character not in the table and not a space (lower case included).
    Char,
}

/// Why a speed cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WpmError;

/// The dot length at `wpm`, in whole milliseconds.
pub fn dot_ms(wpm: u32) -> Result<u32, WpmError> {
    if (MIN_WPM..=MAX_WPM).contains(&wpm) {
        Ok(1200 / wpm)
    } else {
        Err(WpmError)
    }
}

/// One stretch of the key's state: down for an element, or up for a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub down: bool,
    /// Length in dots: 1 or 3 down; 1, 3 or 7 up.
    pub units: u8,
}

/// The segments of `text`, from its first element to its last (spaces at either
/// end and runs of spaces count once, as a word gap, between words only).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Segments {
    segs: [Segment; MAX_SEGMENTS],
    len: usize,
}

impl core::fmt::Debug for Segments {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl Segments {
    /// Check `text` (1 to [`MAX_TEXT`] characters, upper case, from the table or
    /// spaces, at least one not a space) and lay out its segments.
    pub fn of(text: &[u8]) -> Result<Self, TextError> {
        if text.is_empty() || text.len() > MAX_TEXT || text.iter().all(|&c| c == b' ') {
            return Err(TextError::Len);
        }
        let mut s = Segments {
            segs: [Segment {
                down: false,
                units: 0,
            }; MAX_SEGMENTS],
            len: 0,
        };
        // The gap owed before the next element: 0 at the start, then 1, 3 or 7.
        let mut gap = 0u8;
        for &c in text {
            if c == b' ' {
                if gap > 0 {
                    gap = 7;
                }
                continue;
            }
            let p = pattern(c).ok_or(TextError::Char)?;
            for (i, &e) in p.iter().enumerate() {
                if i > 0 {
                    gap = 1;
                }
                if gap > 0 {
                    s.push(false, gap);
                }
                s.push(true, if e == b'.' { 1 } else { 3 });
            }
            gap = 3;
        }
        Ok(s)
    }

    fn push(&mut self, down: bool, units: u8) {
        // Cannot overflow: at most 6 elements and 6 gaps per character.
        self.segs[self.len] = Segment { down, units };
        self.len += 1;
    }

    pub fn as_slice(&self) -> &[Segment] {
        &self.segs[..self.len]
    }

    /// Length in dots, first element to last.
    pub fn units(&self) -> u32 {
        self.as_slice().iter().map(|s| u32::from(s.units)).sum()
    }
}

/// How long `text` takes to key at `wpm`, in ms, first element to last.
pub fn run_ms(text: &[u8], wpm: u32) -> Result<u32, RunError> {
    let dot = dot_ms(wpm).map_err(|_| RunError::Wpm)?;
    let segs = Segments::of(text).map_err(RunError::Text)?;
    Ok(segs.units() * dot)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunError {
    Wpm,
    Text(TextError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_table_as_the_decoder() {
        for (c, p) in TABLE {
            assert_eq!(
                cw::morse::encode_char(*c as char).map(str::as_bytes),
                Some(*p),
                "{}",
                *c as char
            );
        }
        let ours: Vec<u8> = TABLE.iter().map(|(c, _)| *c).collect();
        for c in (0x20u8..0x7F).filter(|c| !c.is_ascii_lowercase()) {
            assert_eq!(
                ours.contains(&c),
                cw::morse::encode_char(c as char).is_some(),
                "{}",
                c as char
            );
        }
        assert!(TABLE.iter().all(|(_, p)| p.len() <= MAX_ELEMENTS));
    }

    #[test]
    fn timing_matches_the_decoder_crate() {
        for text in [
            "PARIS",
            "PARIS PARIS",
            "E",
            "T",
            "R 42 TX MOM ? DE N0DE K",
            "0000",
        ] {
            let s = Segments::of(text.as_bytes()).unwrap();
            assert_eq!(s.units(), cw::morse::units(text), "{text}");
        }
        assert_eq!(run_ms(b"PARIS", 20), Ok(43 * 60));
        assert_eq!(run_ms(b"  PARIS   PARIS ", 20), run_ms(b"PARIS PARIS", 20));
    }

    #[test]
    fn segments_alternate_and_never_hold_down_long() {
        let s = Segments::of(b"KN ?\"@ 0").unwrap();
        let v = s.as_slice();
        assert!(v.first().unwrap().down && v.last().unwrap().down);
        for w in v.windows(2) {
            assert_ne!(w[0].down, w[1].down);
        }
        assert!(v.iter().all(|s| !s.down || s.units <= 3));
        assert_eq!(
            Segments::of(b"A B").unwrap().as_slice(),
            &[
                Segment {
                    down: true,
                    units: 1
                },
                Segment {
                    down: false,
                    units: 1
                },
                Segment {
                    down: true,
                    units: 3
                },
                Segment {
                    down: false,
                    units: 7
                },
                Segment {
                    down: true,
                    units: 3
                },
                Segment {
                    down: false,
                    units: 1
                },
                Segment {
                    down: true,
                    units: 1
                },
                Segment {
                    down: false,
                    units: 1
                },
                Segment {
                    down: true,
                    units: 1
                },
                Segment {
                    down: false,
                    units: 1
                },
                Segment {
                    down: true,
                    units: 1
                },
            ]
        );
        // The longest text fits.
        let worst = [b'@'; MAX_TEXT];
        assert_eq!(
            Segments::of(&worst).unwrap().as_slice().len(),
            MAX_SEGMENTS - 1
        );
    }

    #[test]
    fn bad_text_and_speed() {
        assert_eq!(Segments::of(b""), Err(TextError::Len));
        assert_eq!(Segments::of(b"   "), Err(TextError::Len));
        assert_eq!(Segments::of(&[b'E'; 31]), Err(TextError::Len));
        assert!(Segments::of(&[b'E'; 30]).is_ok());
        assert_eq!(Segments::of(b"cq"), Err(TextError::Char));
        assert_eq!(Segments::of(b"A#B"), Err(TextError::Char));
        assert_eq!(dot_ms(4), Err(WpmError));
        assert_eq!(dot_ms(51), Err(WpmError));
        assert_eq!(dot_ms(5), Ok(240));
        assert_eq!(dot_ms(18), Ok(66));
        assert_eq!(dot_ms(50), Ok(24));
    }
}
