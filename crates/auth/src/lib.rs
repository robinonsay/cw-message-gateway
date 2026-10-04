//! Authentication for the field operator using precomputed one-time letter codes.
//!
//! A real HMAC cannot be computed by hand in the field, so the node's owner prints a
//! table of codes at home, one per sequence number, and carries it on paper. The node
//! holds the secret key and recomputes the code for any sequence number it is shown.
//!
//! Codes follow the HOTP construction of RFC 4226 (HMAC over an 8-byte big-endian
//! counter) with two deliberate differences: HMAC-SHA256 instead of HMAC-SHA1, and a
//! base-N letter encoding of the first 128 bits instead of the 31-bit decimal dynamic
//! truncation, because 8 letters carry about 38 bits and 31 bits would leave most of
//! the code space unused.
//!
//! This is authentication, not encryption: message content stays in the clear.

mod store;

pub use store::{replace_file, SeqStore};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;

/// Number of letters in every code.
pub const CODE_LEN: usize = 8;

/// The default code alphabet: every letter A–Z.
///
/// Which Morse-confusable letters to drop is an open item in the design, so the
/// alphabet is configurable via [`CodeBook::with_alphabet`].
pub const DEFAULT_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Smallest alphabet accepted. 16 letters still gives 16^8 = 2^32 codes.
pub const MIN_ALPHABET_LEN: usize = 16;

/// Secret key plus alphabet; turns a sequence number into its code.
#[derive(Clone)]
pub struct CodeBook {
    key: Vec<u8>,
    alphabet: Vec<u8>,
}

impl fmt::Debug for CodeBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the key.
        f.debug_struct("CodeBook")
            .field("alphabet", &String::from_utf8_lossy(&self.alphabet))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlphabetError {
    TooShort(usize),
    NotUppercaseLetter(char),
    Duplicate(char),
}

impl fmt::Display for AlphabetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort(n) => write!(
                f,
                "alphabet has {n} letters, need at least {MIN_ALPHABET_LEN}"
            ),
            Self::NotUppercaseLetter(c) => {
                write!(f, "alphabet character {c:?} is not an uppercase letter A-Z")
            }
            Self::Duplicate(c) => write!(f, "alphabet letter {c:?} appears more than once"),
        }
    }
}

impl std::error::Error for AlphabetError {}

impl CodeBook {
    pub fn new(key: &[u8]) -> Self {
        Self::with_alphabet(key, DEFAULT_ALPHABET).expect("default alphabet is valid")
    }

    pub fn with_alphabet(key: &[u8], alphabet: &str) -> Result<Self, AlphabetError> {
        let mut seen = [false; 26];
        for c in alphabet.chars() {
            if !c.is_ascii_uppercase() {
                return Err(AlphabetError::NotUppercaseLetter(c));
            }
            let i = (c as u8 - b'A') as usize;
            if seen[i] {
                return Err(AlphabetError::Duplicate(c));
            }
            seen[i] = true;
        }
        if alphabet.len() < MIN_ALPHABET_LEN {
            return Err(AlphabetError::TooShort(alphabet.len()));
        }
        Ok(Self {
            key: key.to_vec(),
            alphabet: alphabet.as_bytes().to_vec(),
        })
    }

    pub fn alphabet(&self) -> &str {
        std::str::from_utf8(&self.alphabet).expect("alphabet is ASCII")
    }

    /// The code for sequence number `seq`.
    pub fn code(&self, seq: u64) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&seq.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        // 128 bits against at most 26^8 ≈ 2^37.6 values: modulo bias is below 2^-90.
        let mut n = u128::from_be_bytes(digest[..16].try_into().expect("16 bytes"));
        let base = self.alphabet.len() as u128;
        let mut out = vec![0u8; CODE_LEN];
        for slot in out.iter_mut() {
            *slot = self.alphabet[(n % base) as usize];
            n /= base;
        }
        String::from_utf8(out).expect("alphabet is ASCII")
    }

    /// Codes for `count` consecutive sequence numbers starting at `from`, for printing.
    pub fn table(&self, from: u64, count: u64) -> impl Iterator<Item = (u64, String)> + '_ {
        (from..from.saturating_add(count)).map(move |seq| (seq, self.code(seq)))
    }

    /// Whether `code` is the code for `seq`, compared in constant time.
    pub fn matches(&self, seq: u64, code: &str) -> bool {
        let expected = self.code(seq);
        let got = normalize(code);
        if got.len() != expected.len() {
            return false;
        }
        expected
            .bytes()
            .zip(got.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

/// Uppercase and drop anything that is not a letter (spaces from printed grouping).
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The sequence number was not greater than the last one acted on (replay or stale).
    Stale { seq: u64, last_seq: u64 },
    /// The code does not match the sequence number.
    BadCode { seq: u64 },
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale { seq, last_seq } => {
                write!(f, "sequence {seq} is not after last used {last_seq}")
            }
            Self::BadCode { seq } => write!(f, "wrong code for sequence {seq}"),
        }
    }
}

/// Applies the acceptance rule: a code is valid only if its sequence number is
/// greater than `last_seq`, which defeats replay while tolerating skipped lines.
#[derive(Debug)]
pub struct Verifier {
    book: CodeBook,
    last_seq: u64,
}

impl Verifier {
    pub fn new(book: CodeBook, last_seq: u64) -> Self {
        Self { book, last_seq }
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Check a fresh code without consuming it.
    pub fn check(&self, seq: u64, code: &str) -> Result<(), Reject> {
        if seq <= self.last_seq {
            return Err(Reject::Stale {
                seq,
                last_seq: self.last_seq,
            });
        }
        self.check_code_only(seq, code)
    }

    /// Check that `code` belongs to `seq` regardless of `last_seq`; used to recognise
    /// an idempotent retry of the commit that was just acted on.
    pub fn check_code_only(&self, seq: u64, code: &str) -> Result<(), Reject> {
        if self.book.matches(seq, code) {
            Ok(())
        } else {
            Err(Reject::BadCode { seq })
        }
    }

    /// Record that `seq` has been acted on. Never moves backwards.
    pub fn commit(&mut self, seq: u64) {
        self.last_seq = self.last_seq.max(seq);
    }
}

/// Format a code for the printed table in two groups of four, e.g. `KRTP QMLD`.
pub fn format_for_print(code: &str) -> String {
    let (a, b) = code.split_at(code.len() / 2);
    format!("{a} {b}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test key, never use in the field";

    #[test]
    fn codes_are_deterministic_letters_of_fixed_length() {
        let book = CodeBook::new(KEY);
        let a = book.code(42);
        assert_eq!(a, book.code(42));
        assert_eq!(a.len(), CODE_LEN);
        assert!(a.bytes().all(|b| b.is_ascii_uppercase()));
        assert_ne!(a, book.code(43));
        assert_ne!(a, CodeBook::new(b"another key").code(42));
    }

    #[test]
    fn codes_use_only_the_configured_alphabet() {
        let alphabet = "ABDGKMNOPRUWXYZQ";
        let book = CodeBook::with_alphabet(KEY, alphabet).unwrap();
        for (_, code) in book.table(0, 500) {
            assert!(code.chars().all(|c| alphabet.contains(c)), "{code}");
        }
    }

    #[test]
    fn letters_are_spread_over_the_alphabet() {
        let book = CodeBook::new(KEY);
        let mut counts = [0u32; 26];
        for (_, code) in book.table(0, 2000) {
            for b in code.bytes() {
                counts[(b - b'A') as usize] += 1;
            }
        }
        // 16000 letters, about 615 each; a broken encoder would leave many at zero.
        assert!(counts.iter().all(|&c| c > 450 && c < 800), "{counts:?}");
    }

    #[test]
    fn rejects_bad_alphabets() {
        assert_eq!(
            CodeBook::with_alphabet(KEY, "ABC").unwrap_err(),
            AlphabetError::TooShort(3)
        );
        assert_eq!(
            CodeBook::with_alphabet(KEY, "ABCDEFGHIJKLMNOa").unwrap_err(),
            AlphabetError::NotUppercaseLetter('a')
        );
        assert_eq!(
            CodeBook::with_alphabet(KEY, "ABCDEFGHIJKLMNOA").unwrap_err(),
            AlphabetError::Duplicate('A')
        );
    }

    #[test]
    fn match_ignores_case_and_print_grouping() {
        let book = CodeBook::new(KEY);
        let code = book.code(7);
        assert!(book.matches(7, &format_for_print(&code).to_lowercase()));
        assert!(!book.matches(8, &code));
        assert!(!book.matches(7, &code[..7]));
    }

    #[test]
    fn verifier_accepts_only_increasing_sequence_numbers() {
        let book = CodeBook::new(KEY);
        let mut v = Verifier::new(book.clone(), 41);
        assert_eq!(
            v.check(41, &book.code(41)),
            Err(Reject::Stale {
                seq: 41,
                last_seq: 41
            })
        );
        assert_eq!(
            v.check(42, &book.code(43)),
            Err(Reject::BadCode { seq: 42 })
        );
        // Skipping lines on the paper table is fine.
        assert_eq!(v.check(45, &book.code(45)), Ok(()));
        v.commit(45);
        assert_eq!(
            v.check(45, &book.code(45)),
            Err(Reject::Stale {
                seq: 45,
                last_seq: 45
            })
        );
        assert_eq!(v.check_code_only(45, &book.code(45)), Ok(()));
        v.commit(10);
        assert_eq!(v.last_seq(), 45);
    }
}
