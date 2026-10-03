//! The over-the-air grammar between the field operator and the node.
//!
//! Field to node (each transmission ends with `K`):
//!
//! | Message                         | Meaning                                    |
//! |---------------------------------|--------------------------------------------|
//! | `CALL seq code TX dest text`    | Open: send `text` to contact `dest`        |
//! | `CALL seq code RX`              | Open: read new inbound messages            |
//! | `CALL seq code WX [grid]`       | Open: weather forecast (home or grid)      |
//! | `OK seq code`                   | Commit the pending transaction             |
//! | `NO`                            | Abort the pending transaction              |
//! | `AGN [letter]`                  | Repeat the last transmission, or a chunk   |
//!
//! Decoded CW is noisy, so parsing is forgiving where the read-back protects the
//! operator (keywords, contact names and callsigns are snapped to the nearest legal
//! token by Morse-pattern distance) and strict where it does not (sequence numbers
//! and codes must decode exactly; the code check rejects anything else).

mod fuzzy;
mod reply;
mod text;

pub use fuzzy::{morse_distance, snap};
pub use reply::{chunk, Chunk, Reply};
pub use text::sanitize;

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Tx { dest: String, text: String },
    Rx,
    Wx { grid: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldMsg {
    Open { call: String, seq: u64, code: String, cmd: Command },
    Commit { seq: u64, code: String },
    Abort,
    Again { chunk: Option<char> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    /// Nothing recognisable as a callsign or command word at the start.
    NoStart,
    BadSeq(String),
    BadCode,
    /// The command word after the code is missing or ambiguous.
    BadCommand(String),
    UnknownContact(String),
    EmptyMessage,
    BadGrid(String),
    TrailingGarbage(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty transmission"),
            Self::NoStart => write!(f, "no known callsign or command word"),
            Self::BadSeq(t) => write!(f, "sequence number {t:?} is not a number"),
            Self::BadCode => write!(f, "code is not {} letters", auth_code_len()),
            Self::BadCommand(t) => write!(f, "unrecognised command {t:?}"),
            Self::UnknownContact(t) => write!(f, "unknown contact {t:?}"),
            Self::EmptyMessage => write!(f, "TX without message text"),
            Self::BadGrid(t) => write!(f, "{t:?} is not a Maidenhead grid square"),
            Self::TrailingGarbage(t) => write!(f, "unexpected {t:?} after command"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Length of an authentication code, in letters.
pub const CODE_LEN: usize = 8;

fn auth_code_len() -> usize {
    CODE_LEN
}

/// What the parser needs to know about this node.
#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    /// Callsigns allowed to open transactions.
    pub field_calls: Vec<String>,
    /// Contact names usable as `TX` destinations.
    pub contacts: Vec<String>,
}

/// Maximum Morse distance for snapping keywords, callsigns and contact names.
const KEYWORD_TOLERANCE: usize = 2;

/// Parse one decoded transmission.
pub fn parse(decoded: &str, vocab: &Vocabulary) -> Result<FieldMsg, ParseError> {
    let mut tokens: Vec<String> = decoded
        .split_whitespace()
        .map(|t| t.to_ascii_uppercase())
        .filter(|t| t.chars().all(|c| c != '*'))
        .collect();
    // Drop the trailing over/out prosign: K, KN, AR (+), SK.
    while matches!(tokens.last().map(String::as_str), Some("K" | "KN" | "+" | "AR" | "SK")) {
        tokens.pop();
    }
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }

    // Skip leading noise until something that starts a message.
    let starts = ["OK", "NO", "AGN"];
    let start = tokens.iter().position(|t| {
        snap(t, &starts, 0).is_some() || snap(t, &vocab.field_calls, KEYWORD_TOLERANCE).is_some()
    });
    let tokens = &tokens[start.ok_or(ParseError::NoStart)?..];
    let first = &tokens[0];

    if snap(first, &starts, 0) == Some("OK") {
        let (seq, code, rest) = seq_and_code(&tokens[1..])?;
        expect_end(rest)?;
        return Ok(FieldMsg::Commit { seq, code });
    }
    if snap(first, &starts, 0) == Some("NO") {
        expect_end(&tokens[1..])?;
        return Ok(FieldMsg::Abort);
    }
    if snap(first, &starts, 0) == Some("AGN") {
        let chunk = match &tokens[1..] {
            [] => None,
            [t] if t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()) => t.chars().next(),
            rest => return Err(ParseError::TrailingGarbage(rest.join(" "))),
        };
        return Ok(FieldMsg::Again { chunk });
    }

    let call = snap(first, &vocab.field_calls, KEYWORD_TOLERANCE).expect("start token matched").to_string();
    let (seq, code, rest) = seq_and_code(&tokens[1..])?;
    let (kw, args) = rest.split_first().ok_or_else(|| ParseError::BadCommand(String::new()))?;
    let cmd = match snap(kw, &["TX", "RX", "WX"], 1) {
        Some("TX") => {
            let (dest, words) = args.split_first().ok_or(ParseError::EmptyMessage)?;
            let dest = snap(dest, &vocab.contacts, KEYWORD_TOLERANCE)
                .ok_or_else(|| ParseError::UnknownContact(dest.clone()))?
                .to_string();
            if words.is_empty() {
                return Err(ParseError::EmptyMessage);
            }
            Command::Tx { dest, text: words.join(" ") }
        }
        Some("RX") => {
            expect_end(args)?;
            Command::Rx
        }
        Some("WX") => {
            let grid = match args {
                [] => None,
                [g] if is_grid(g) => Some(g.clone()),
                [g] => return Err(ParseError::BadGrid(g.clone())),
                rest => return Err(ParseError::TrailingGarbage(rest.join(" "))),
            };
            Command::Wx { grid }
        }
        _ => return Err(ParseError::BadCommand(kw.clone())),
    };
    Ok(FieldMsg::Open { call, seq, code, cmd })
}

/// Read `seq code` from the front of `tokens`. A code split across tokens by a
/// stretched gap (`KRTP QMLD`) is rejoined when the pieces add up to exactly 8 letters.
fn seq_and_code(tokens: &[String]) -> Result<(u64, String, &[String]), ParseError> {
    let (seq_tok, rest) = tokens.split_first().ok_or_else(|| ParseError::BadSeq(String::new()))?;
    let seq = if seq_tok.chars().all(|c| c.is_ascii_digit()) {
        seq_tok.parse().map_err(|_| ParseError::BadSeq(seq_tok.clone()))?
    } else {
        return Err(ParseError::BadSeq(seq_tok.clone()));
    };
    let mut code = String::new();
    for (i, t) in rest.iter().enumerate() {
        if !t.chars().all(|c| c.is_ascii_alphabetic()) {
            break;
        }
        code.push_str(t);
        if code.len() == CODE_LEN {
            return Ok((seq, code, &rest[i + 1..]));
        }
        if code.len() > CODE_LEN {
            break;
        }
    }
    Err(ParseError::BadCode)
}

fn expect_end(rest: &[String]) -> Result<(), ParseError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(ParseError::TrailingGarbage(rest.join(" ")))
    }
}

/// A 4- or 6-character Maidenhead locator such as `DL88` or `DL88AF`.
pub fn is_grid(s: &str) -> bool {
    let b = s.as_bytes();
    let field = |c: u8| (b'A'..=b'R').contains(&c);
    let square = |c: u8| c.is_ascii_digit();
    let sub = |c: u8| (b'A'..=b'X').contains(&c.to_ascii_uppercase());
    match b.len() {
        4 => field(b[0]) && field(b[1]) && square(b[2]) && square(b[3]),
        6 => field(b[0]) && field(b[1]) && square(b[2]) && square(b[3]) && sub(b[4]) && sub(b[5]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocabulary {
        Vocabulary {
            field_calls: vec!["W5XXX".into()],
            contacts: vec!["MOM".into(), "DAD".into(), "BOB".into()],
        }
    }

    #[test]
    fn parses_the_spec_examples() {
        assert_eq!(
            parse("W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K", &vocab()),
            Ok(FieldMsg::Open {
                call: "W5XXX".into(),
                seq: 42,
                code: "KRTPQMLD".into(),
                cmd: Command::Tx { dest: "MOM".into(), text: "RUNNING LATE HOME SUN".into() },
            })
        );
        assert_eq!(
            parse("OK 43 WBNFHJGC K", &vocab()),
            Ok(FieldMsg::Commit { seq: 43, code: "WBNFHJGC".into() })
        );
        assert_eq!(
            parse("W5XXX 44 ABCDEFGH RX K", &vocab()).unwrap(),
            FieldMsg::Open { call: "W5XXX".into(), seq: 44, code: "ABCDEFGH".into(), cmd: Command::Rx }
        );
        assert_eq!(parse("NO K", &vocab()), Ok(FieldMsg::Abort));
        assert_eq!(parse("AGN", &vocab()), Ok(FieldMsg::Again { chunk: None }));
        assert_eq!(parse("AGN B K", &vocab()), Ok(FieldMsg::Again { chunk: Some('B') }));
    }

    #[test]
    fn weather_with_and_without_grid() {
        let open = |s| match parse(s, &vocab()) {
            Ok(FieldMsg::Open { cmd, .. }) => cmd,
            other => panic!("{other:?}"),
        };
        assert_eq!(open("W5XXX 46 ABCDEFGH WX K"), Command::Wx { grid: None });
        assert_eq!(open("W5XXX 46 ABCDEFGH WX DL88 K"), Command::Wx { grid: Some("DL88".into()) });
        assert_eq!(parse("W5XXX 46 ABCDEFGH WX ZZ99 K", &vocab()), Err(ParseError::BadGrid("ZZ99".into())));
    }

    #[test]
    fn tolerates_decode_noise() {
        // Leading junk, DE before the call, a one-element error in the callsign
        // (X decoded as K), the code split by a long gap, and O decoded as P.
        let msg = parse("E T DE W5XKX 42 KRTP QMLD TX MPM HI K", &vocab()).unwrap();
        assert_eq!(
            msg,
            FieldMsg::Open {
                call: "W5XXX".into(),
                seq: 42,
                code: "KRTPQMLD".into(),
                cmd: Command::Tx { dest: "MOM".into(), text: "HI".into() },
            }
        );
    }

    #[test]
    fn rejects_what_it_cannot_trust() {
        let v = vocab();
        assert_eq!(parse("", &v), Err(ParseError::Empty));
        assert_eq!(parse("CQ CQ DE N0CALL K", &v), Err(ParseError::NoStart));
        assert_eq!(parse("W5XXX 4Z KRTPQMLD RX K", &v), Err(ParseError::BadSeq("4Z".into())));
        assert_eq!(parse("W5XXX 42 KRTPQML RX K", &v), Err(ParseError::BadCode));
        assert_eq!(parse("W5XXX 42 KRTPQMLD QQ K", &v), Err(ParseError::BadCommand("QQ".into())));
        assert_eq!(parse("W5XXX 42 KRTPQMLD TX ZEUS HI K", &v), Err(ParseError::UnknownContact("ZEUS".into())));
        assert_eq!(parse("W5XXX 42 KRTPQMLD TX MOM K", &v), Err(ParseError::EmptyMessage));
        assert_eq!(parse("OK 43 WBNFHJGC EXTRA K", &v), Err(ParseError::TrailingGarbage("EXTRA".into())));
        // Unknown patterns (decoded as '*') are dropped, not guessed.
        assert_eq!(parse("OK 43 WBNF*JGC K", &v), Err(ParseError::BadCode));
    }

    #[test]
    fn grids() {
        assert!(is_grid("DL88"));
        assert!(is_grid("DL88af"));
        assert!(!is_grid("DL8"));
        assert!(!is_grid("SL88"));
    }
}
