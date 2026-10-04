//! The over-the-air grammar between the field operator and the node.
//!
//! Field to node (each transmission ends with an over: `K`, `KN`, `AR K`):
//!
//! | Message                         | Meaning                                    |
//! |---------------------------------|--------------------------------------------|
//! | `CALL seq code TX dest text`    | Open: send `text` to contact `dest`        |
//! | `CALL seq code RX`              | Open: read new inbound messages            |
//! | `CALL seq code WX [grid\|n]`     | Open: forecast for a grid, preset or last  |
//! | `OK seq code`                   | Commit the pending transaction             |
//! | `NO seq code`                   | Abort the pending transaction              |
//! | `AGN seq code [letter]`         | Repeat the last transmission, or a chunk   |
//!
//! Only the final over of a `TX` text is dropped, with one `AR` before it, so a
//! message can end in the word `K` when the over follows it. After `AGN`, a lone
//! `K` is the over and `K K` asks for chunk `K`.
//!
//! Decoded CW is noisy, so parsing is forgiving where the read-back protects the
//! operator (keywords, contact names and callsigns are snapped to the nearest legal
//! token by Morse-pattern distance) and strict where it does not (sequence numbers
//! and codes must decode exactly; the code check rejects anything else).

mod fuzzy;
mod reply;
mod text;

pub use fuzzy::{morse_distance, snap};
pub use reply::{chunk, chunk_count, Chunk, Reply, MAX_CHUNKS};
pub use text::sanitize;

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Tx {
        dest: String,
        text: String,
    },
    Rx,
    /// `None` when `WX` was sent alone; the node decides which place that means.
    Wx {
        place: Option<Place>,
    },
}

/// Where a `WX` forecast is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A 4- or 6-character Maidenhead locator, uppercased.
    Grid(String),
    /// One of the node's numbered presets (`WX 3`).
    Preset(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldMsg {
    Open {
        call: String,
        seq: u64,
        code: String,
        cmd: Command,
    },
    Commit {
        seq: u64,
        code: String,
    },
    Abort {
        seq: u64,
        code: String,
    },
    Again {
        seq: u64,
        code: String,
        chunk: Option<char>,
    },
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
    UnknownPreset(String),
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
            Self::UnknownPreset(t) => write!(f, "no weather preset {t}"),
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
    /// Weather preset numbers usable as `WX <n>`.
    pub presets: Vec<u32>,
}

/// Over prosigns as decoded: `K`, `KN` (run together it decodes as `(`), `AR`
/// (run together `+`) and `SK`. The node's over detection uses the same set.
pub const OVERS: [&str; 6] = ["K", "KN", "(", "+", "AR", "SK"];

/// Whether `t` is one of [`OVERS`].
pub fn is_over(t: &str) -> bool {
    OVERS.contains(&t)
}

/// Characters of one or two elements, which is what isolated noise bursts decode as.
const NOISE_CHARS: &str = "ETIANM";

/// Maximum Morse distance for snapping keywords, callsigns and contact names.
const KEYWORD_TOLERANCE: usize = 2;

/// Parse one decoded transmission.
pub fn parse(decoded: &str, vocab: &Vocabulary) -> Result<FieldMsg, ParseError> {
    let mut tokens: Vec<String> = decoded
        .split_whitespace()
        .map(|t| t.to_ascii_uppercase())
        .filter(|t| t.chars().all(|c| c != '*'))
        .collect();
    // Set aside the trailing run of over prosigns. Which of them really are the
    // over depends on the command: a final word K in TX text, chunk K after AGN.
    let body_len = tokens
        .iter()
        .rposition(|t| !is_over(t))
        .map_or(0, |i| i + 1);
    if body_len == 0 {
        return Err(ParseError::Empty);
    }
    let tail = tokens.split_off(body_len);

    // A noise burst just before the first word often decodes as a stray short
    // character glued onto it ("EOK"). Peel up to two such characters off a token
    // when what remains is a start word.
    let starts = ["OK", "NO", "AGN"];
    for t in tokens.iter_mut() {
        let is_start = |w: &str| {
            snap(w, &starts, 0).is_some()
                || snap(w, &vocab.field_calls, KEYWORD_TOLERANCE).is_some()
        };
        if is_start(t) {
            break;
        }
        let peel = t
            .chars()
            .take(2)
            .take_while(|c| NOISE_CHARS.contains(*c))
            .count();
        if let Some(n) = (1..=peel).find(|&n| is_start(&t[n..])) {
            *t = t[n..].to_string();
            break;
        }
    }
    // A stretched gap can split the callsign ("W 5XXX"): rejoin a pair of tokens
    // when together, but not alone, they match a field callsign.
    if let Some(i) = (0..tokens.len().saturating_sub(1)).find(|&i| {
        snap(&tokens[i], &vocab.field_calls, KEYWORD_TOLERANCE).is_none()
            && snap(
                &format!("{}{}", tokens[i], tokens[i + 1]),
                &vocab.field_calls,
                KEYWORD_TOLERANCE,
            )
            .is_some()
    }) {
        let joined = format!("{}{}", tokens[i], tokens[i + 1]);
        tokens.splice(i..i + 2, [joined]);
    }
    // Skip leading noise until something that starts a message. OK, NO and AGN
    // are ordinary words too, so they count only when nothing but noise comes
    // before them: otherwise a garbled callsign would turn a message ending in
    // "NO" into an abort.
    let start = tokens.iter().enumerate().position(|(i, t)| {
        snap(t, &vocab.field_calls, KEYWORD_TOLERANCE).is_some()
            || (snap(t, &starts, 0).is_some() && tokens[..i].iter().all(|n| is_noise(n)))
    });
    let mut tokens = tokens.split_off(start.ok_or(ParseError::NoStart)?);
    tokens.extend(tail);
    let tokens = &tokens[..];
    let first = &tokens[0];

    if snap(first, &starts, 0) == Some("OK") {
        let (seq, code, rest) = seq_and_code(&tokens[1..])?;
        expect_overs(rest)?;
        return Ok(FieldMsg::Commit { seq, code });
    }
    if snap(first, &starts, 0) == Some("NO") {
        let (seq, code, rest) = seq_and_code(&tokens[1..])?;
        expect_overs(rest)?;
        return Ok(FieldMsg::Abort { seq, code });
    }
    if snap(first, &starts, 0) == Some("AGN") {
        let (seq, code, rest) = seq_and_code(&tokens[1..])?;
        // A letter right after the code is the chunk, unless it is a K with nothing
        // after it: that K is the over. So AGN K K asks for chunk K.
        let is_letter = |t: &str| t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic());
        let (chunk, overs) = match rest {
            [l, overs @ ..] if is_letter(l) && (!overs.is_empty() || !is_over(l)) => {
                (l.chars().next(), overs)
            }
            overs => (None, overs),
        };
        expect_overs(overs)?;
        return Ok(FieldMsg::Again { seq, code, chunk });
    }

    let call = snap(first, &vocab.field_calls, KEYWORD_TOLERANCE)
        .expect("start token matched")
        .to_string();
    let (seq, code, rest) = seq_and_code(&tokens[1..])?;
    let (kw, args) = rest
        .split_first()
        .ok_or_else(|| ParseError::BadCommand(String::new()))?;
    let cmd = match snap(kw, &["TX", "RX", "WX"], 1) {
        Some("TX") => {
            let (dest, words) = args.split_first().ok_or(ParseError::EmptyMessage)?;
            let dest = snap(dest, &vocab.contacts, KEYWORD_TOLERANCE)
                .ok_or_else(|| ParseError::UnknownContact(dest.clone()))?
                .to_string();
            let words = strip_final_over(words);
            if words.is_empty() {
                return Err(ParseError::EmptyMessage);
            }
            Command::Tx {
                dest,
                text: words.join(" "),
            }
        }
        Some("RX") => {
            expect_overs(args)?;
            Command::Rx
        }
        Some("WX") => {
            let n = args.iter().rposition(|t| !is_over(t)).map_or(0, |i| i + 1);
            Command::Wx {
                place: wx_place(&args[..n], &vocab.presets)?,
            }
        }
        _ => return Err(ParseError::BadCommand(kw.clone())),
    };
    Ok(FieldMsg::Open {
        call,
        seq,
        code,
        cmd,
    })
}

/// The place after `WX`: nothing, a grid square or a preset number.
///
/// Neither is snapped to anything: the read-back shows the grid (and the preset
/// number) so the operator can catch a miscopy. A grid split in two by a stretched
/// gap (`DL89 IG`) is rejoined, as codes are, except when a whole 4-character grid
/// is followed by what a noise burst or a garbled `K` decodes as (`DL89 EE`,
/// `DL89 TA`): that would invent a subsquare, so it gets silence and the operator
/// repeats. A preset must match one the node has exactly; anything else gets
/// silence, like an unknown contact.
fn wx_place(args: &[String], presets: &[u32]) -> Result<Option<Place>, ParseError> {
    match args {
        [] => Ok(None),
        [g] if is_grid(g) => Ok(Some(Place::Grid(g.clone()))),
        [n] if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => n
            .parse::<u32>()
            .ok()
            // No leading zeros: "03" is not preset 3.
            .filter(|p| presets.contains(p) && p.to_string() == *n)
            .map(|p| Some(Place::Preset(p)))
            .ok_or_else(|| ParseError::UnknownPreset(n.clone())),
        [g] => Err(ParseError::BadGrid(g.clone())),
        [a, b] if is_grid(&format!("{a}{b}")) && !(is_grid(a) && is_noise(b)) => {
            Ok(Some(Place::Grid(format!("{a}{b}"))))
        }
        rest => Err(ParseError::TrailingGarbage(rest.join(" "))),
    }
}

/// A token that a noise burst could have produced: one or two short characters, or
/// the `DE` an operator may send before a callsign.
fn is_noise(t: &str) -> bool {
    t == "DE" || (t.len() <= 2 && t.chars().all(|c| NOISE_CHARS.contains(c)))
}

/// Read `seq code` from the front of `tokens`. A code split across tokens by a
/// stretched gap (`KRTP QMLD`) is rejoined when the pieces add up to exactly 8 letters.
fn seq_and_code(tokens: &[String]) -> Result<(u64, String, &[String]), ParseError> {
    let (seq_tok, rest) = tokens
        .split_first()
        .ok_or_else(|| ParseError::BadSeq(String::new()))?;
    let seq = if seq_tok.chars().all(|c| c.is_ascii_digit()) {
        seq_tok
            .parse()
            .map_err(|_| ParseError::BadSeq(seq_tok.clone()))?
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

/// After a command's last field only over prosigns may follow, any number of them
/// (`K`, `AR K`, `+ K`, `K K`, `KN`, `(`).
fn expect_overs(rest: &[String]) -> Result<(), ParseError> {
    if rest.iter().all(|t| is_over(t)) {
        Ok(())
    } else {
        Err(ParseError::TrailingGarbage(rest.join(" ")))
    }
}

/// TX text without its over: the last word if it is an over prosign, and an `AR`
/// (or run-together `+`) just before it, the end-of-message sign. Nothing more, so
/// a message can end in the word K, AR, KN or SK when the over follows it
/// (`VITAMIN K K`, `LITTLE ROCK AR AR K`).
fn strip_final_over(words: &[String]) -> &[String] {
    let mut n = words.len();
    if n > 0 && is_over(&words[n - 1]) {
        n -= 1;
        if n > 0 && matches!(words[n - 1].as_str(), "AR" | "+") {
            n -= 1;
        }
    }
    &words[..n]
}

/// A 4- or 6-character Maidenhead locator such as `DL89` or `DL89IG`.
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
            presets: vec![1, 2, 12],
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
                cmd: Command::Tx {
                    dest: "MOM".into(),
                    text: "RUNNING LATE HOME SUN".into()
                },
            })
        );
        assert_eq!(
            parse("OK 43 WBNFHJGC K", &vocab()),
            Ok(FieldMsg::Commit {
                seq: 43,
                code: "WBNFHJGC".into()
            })
        );
        assert_eq!(
            parse("W5XXX 44 ABCDEFGH RX K", &vocab()).unwrap(),
            FieldMsg::Open {
                call: "W5XXX".into(),
                seq: 44,
                code: "ABCDEFGH".into(),
                cmd: Command::Rx
            }
        );
        assert_eq!(
            parse("NO 45 ABCDEFGH K", &vocab()),
            Ok(FieldMsg::Abort {
                seq: 45,
                code: "ABCDEFGH".into()
            })
        );
        assert_eq!(
            parse("AGN 46 ABCDEFGH", &vocab()),
            Ok(FieldMsg::Again {
                seq: 46,
                code: "ABCDEFGH".into(),
                chunk: None
            })
        );
        assert_eq!(
            parse("AGN 46 ABCD EFGH B K", &vocab()),
            Ok(FieldMsg::Again {
                seq: 46,
                code: "ABCDEFGH".into(),
                chunk: Some('B')
            })
        );
    }

    #[test]
    fn weather_with_and_without_grid() {
        let open = |s| match parse(s, &vocab()) {
            Ok(FieldMsg::Open { cmd, .. }) => cmd,
            other => panic!("{other:?}"),
        };
        let grid = |g: &str| Command::Wx {
            place: Some(Place::Grid(g.into())),
        };
        assert_eq!(open("W5XXX 46 ABCDEFGH WX K"), Command::Wx { place: None });
        assert_eq!(open("W5XXX 46 ABCDEFGH WX DL88 K"), grid("DL88"));
        assert_eq!(open("W5XXX 46 ABCDEFGH WX dl88af K"), grid("DL88AF"));
        assert_eq!(
            parse("W5XXX 46 ABCDEFGH WX ZZ99 K", &vocab()),
            Err(ParseError::BadGrid("ZZ99".into()))
        );
        assert_eq!(
            parse("W5XXX 46 ABCDEFGH WX DL88 ZZ K", &vocab()),
            Err(ParseError::TrailingGarbage("DL88 ZZ".into()))
        );
    }

    #[test]
    fn weather_grid_split_by_a_long_gap_is_rejoined() {
        let open = |s| match parse(s, &vocab()) {
            Ok(FieldMsg::Open { cmd, .. }) => cmd,
            other => panic!("{other:?}"),
        };
        for text in [
            "W5XXX 46 ABCDEFGH WX DL88 AF K",
            "W5XXX 46 ABCDEFGH WX DL 88AF K",
            "W5XXX 46 ABCDEFGH WX DL8 8AF K",
        ] {
            assert_eq!(
                open(text),
                Command::Wx {
                    place: Some(Place::Grid("DL88AF".into()))
                },
                "{text}"
            );
        }
        assert_eq!(
            open("W5XXX 46 ABCDEFGH WX DL 88 K"),
            Command::Wx {
                place: Some(Place::Grid("DL88".into()))
            }
        );
        // Three pieces are too many to guess at.
        assert!(parse("W5XXX 46 ABCDEFGH WX DL 88 AF K", &vocab()).is_err());
        // Noise after a whole 4-character grid is not made into a subsquare.
        for text in [
            "W5XXX 46 ABCDEFGH WX DL89 EE K",
            "W5XXX 46 ABCDEFGH WX DL89 IT K",
            "W5XXX 46 ABCDEFGH WX DL89 TA",
            "W5XXX 46 ABCDEFGH WX DL89 NT",
        ] {
            assert!(
                matches!(parse(text, &vocab()), Err(ParseError::TrailingGarbage(_))),
                "{text}"
            );
        }
        // A split inside the 4-character part is still rejoined whatever follows.
        assert_eq!(
            open("W5XXX 46 ABCDEFGH WX DL8 9ME K"),
            Command::Wx {
                place: Some(Place::Grid("DL89ME".into()))
            }
        );
    }

    #[test]
    fn weather_presets_must_match_exactly() {
        let open = |s| match parse(s, &vocab()) {
            Ok(FieldMsg::Open { cmd, .. }) => cmd,
            other => panic!("{other:?}"),
        };
        let preset = |n| Command::Wx {
            place: Some(Place::Preset(n)),
        };
        assert_eq!(open("W5XXX 46 ABCDEFGH WX 2 K"), preset(2));
        assert_eq!(open("W5XXX 46 ABCDEFGH WX 12 K"), preset(12));
        for (text, token) in [
            ("W5XXX 46 ABCDEFGH WX 3 K", "3"),
            ("W5XXX 46 ABCDEFGH WX 02 K", "02"),
            (
                "W5XXX 46 ABCDEFGH WX 99999999999999999999 K",
                "99999999999999999999",
            ),
        ] {
            assert_eq!(
                parse(text, &vocab()),
                Err(ParseError::UnknownPreset(token.into())),
                "{text}"
            );
        }
        // A preset split by a gap is not rejoined: "1 2" could be 1 or 12.
        assert!(parse("W5XXX 46 ABCDEFGH WX 1 2 K", &vocab()).is_err());
        // With no presets configured every number is unknown.
        let none = Vocabulary {
            presets: Vec::new(),
            ..vocab()
        };
        assert_eq!(
            parse("W5XXX 46 ABCDEFGH WX 1 K", &none),
            Err(ParseError::UnknownPreset("1".into()))
        );
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
                cmd: Command::Tx {
                    dest: "MOM".into(),
                    text: "HI".into()
                },
            }
        );
    }

    #[test]
    fn peels_noise_glued_to_the_first_word() {
        let v = vocab();
        assert_eq!(
            parse("EOK 43 WBNFHJGC K", &v),
            Ok(FieldMsg::Commit {
                seq: 43,
                code: "WBNFHJGC".into()
            })
        );
        assert!(matches!(
            parse("TW5XXX 44 ABCDEFGH RX K", &v),
            Ok(FieldMsg::Open { .. })
        ));
        // A callsign split by a long gap is rejoined.
        assert!(matches!(
            parse("E W 5XXX 44 ABCDEFGH RX K", &v),
            Ok(FieldMsg::Open { .. })
        ));
        // Only short noise characters are peeled.
        assert_eq!(parse("QOK 43 WBNFHJGC K", &v), Err(ParseError::NoStart));
    }

    #[test]
    fn message_words_never_become_start_words() {
        let v = vocab();
        // The callsign is garbled beyond tolerance: the NO, AGN or OK in the
        // message text must not turn the open into an abort, repeat or commit.
        for text in [
            "W5XX 42 KRTPQMLD TX MOM SAY NO K",
            "W5XX 42 KRTPQMLD TX MOM SAY AGN K",
            "W5XX 42 KRTPQMLD TX MOM SAY NO 43 WBNFHJGC K",
            "W5XX 42 KRTPQMLD TX MOM OK 43 WBNFHJGC K",
            "CQ NO K",
        ] {
            assert_eq!(parse(text, &v), Err(ParseError::NoStart), "{text}");
        }
        // Leading noise is still skipped.
        assert_eq!(
            parse("E T NO 45 ABCDEFGH K", &v),
            Ok(FieldMsg::Abort {
                seq: 45,
                code: "ABCDEFGH".into()
            })
        );
        assert_eq!(
            parse("IE AGN 46 ABCDEFGH B K", &v),
            Ok(FieldMsg::Again {
                seq: 46,
                code: "ABCDEFGH".into(),
                chunk: Some('B')
            })
        );
    }

    #[test]
    fn rejects_what_it_cannot_trust() {
        let v = vocab();
        assert_eq!(parse("", &v), Err(ParseError::Empty));
        assert_eq!(parse("CQ CQ DE N0CALL K", &v), Err(ParseError::NoStart));
        assert_eq!(
            parse("W5XXX 4Z KRTPQMLD RX K", &v),
            Err(ParseError::BadSeq("4Z".into()))
        );
        assert_eq!(parse("W5XXX 42 KRTPQML RX K", &v), Err(ParseError::BadCode));
        assert_eq!(
            parse("W5XXX 42 KRTPQMLD QQ K", &v),
            Err(ParseError::BadCommand("QQ".into()))
        );
        assert_eq!(
            parse("W5XXX 42 KRTPQMLD TX ZEUS HI K", &v),
            Err(ParseError::UnknownContact("ZEUS".into()))
        );
        assert_eq!(
            parse("W5XXX 42 KRTPQMLD TX MOM K", &v),
            Err(ParseError::EmptyMessage)
        );
        assert_eq!(
            parse("OK 43 WBNFHJGC EXTRA K", &v),
            Err(ParseError::TrailingGarbage("EXTRA K".into()))
        );
        // Unknown patterns (decoded as '*') are dropped, not guessed.
        assert_eq!(parse("OK 43 WBNF*JGC K", &v), Err(ParseError::BadCode));
    }

    #[test]
    fn no_and_agn_need_a_line_and_its_code() {
        let v = vocab();
        for (text, err) in [
            ("NO K", ParseError::BadSeq("K".into())),
            ("NO", ParseError::BadSeq(String::new())),
            ("AGN K", ParseError::BadSeq("K".into())),
            ("AGN B K", ParseError::BadSeq("B".into())),
            ("NO 45 ABCDEF K", ParseError::BadCode),
            // A chunk letter glued to the code cannot be told apart from it.
            ("AGN 46 ABCDEFGHB K", ParseError::BadCode),
            (
                "NO 45 ABCDEFGH B K",
                ParseError::TrailingGarbage("B K".into()),
            ),
            (
                "AGN 46 ABCDEFGH B C K",
                ParseError::TrailingGarbage("C K".into()),
            ),
        ] {
            assert_eq!(parse(text, &v), Err(err), "{text}");
        }
        // The code in any number of pieces, noise glued to the keyword.
        assert_eq!(
            parse("ENO 45 ABC DE FGH K", &v),
            Ok(FieldMsg::Abort {
                seq: 45,
                code: "ABCDEFGH".into()
            })
        );
    }

    #[test]
    fn agn_k_k_asks_for_chunk_k() {
        let v = vocab();
        let chunk = |tail: &str| match parse(&format!("AGN 46 ABCDEFGH {tail}"), &v) {
            Ok(FieldMsg::Again { chunk, .. }) => chunk,
            other => panic!("{tail}: {other:?}"),
        };
        for tail in ["", "K", "KN", "(", "AR K", "+ K", "SK"] {
            assert_eq!(chunk(tail), None, "{tail}");
        }
        for tail in ["K K", "K KN", "K (", "K AR K"] {
            assert_eq!(chunk(tail), Some('K'), "{tail}");
        }
        for tail in ["B", "B K", "B AR K", "B ("] {
            assert_eq!(chunk(tail), Some('B'), "{tail}");
        }
    }

    #[test]
    fn tx_text_loses_its_over_but_keeps_a_final_word_k() {
        let v = vocab();
        let text = |end: &str| match parse(&format!("W5XXX 42 ABCDEFGH TX MOM {end}"), &v) {
            Ok(FieldMsg::Open {
                cmd: Command::Tx { text, .. },
                ..
            }) => text,
            other => panic!("{end}: {other:?}"),
        };
        for (end, want) in [
            ("BRING VITAMIN K K", "BRING VITAMIN K"),
            ("BRING VITAMIN K (", "BRING VITAMIN K"),
            ("BRING VITAMIN K", "BRING VITAMIN"),
            ("HOME SUN AR K", "HOME SUN"),
            ("HOME SUN + K", "HOME SUN"),
            ("HOME SUN + (", "HOME SUN"),
            ("HOME SUN (", "HOME SUN"),
            ("HOME SUN SK", "HOME SUN"),
            ("HOME SUN", "HOME SUN"),
            ("LITTLE ROCK AR AR K", "LITTLE ROCK AR"),
            ("BACK IN SK I AM K", "BACK IN SK I AM"),
            ("A ( B K", "A ( B"),
            ("K K", "K"),
        ] {
            assert_eq!(text(end), want, "{end}");
        }
        assert_eq!(
            parse("W5XXX 42 ABCDEFGH TX MOM AR K", &v),
            Err(ParseError::EmptyMessage)
        );
    }

    #[test]
    fn kn_and_doubled_overs_end_every_command() {
        let v = vocab();
        for end in ["", "K", "(", "KN", "AR K", "+ K", "K K", "KN K", "SK"] {
            assert!(
                matches!(
                    parse(&format!("OK 43 ABCDEFGH {end}"), &v),
                    Ok(FieldMsg::Commit { .. })
                ),
                "OK {end}"
            );
            assert!(
                matches!(
                    parse(&format!("NO 45 ABCDEFGH {end}"), &v),
                    Ok(FieldMsg::Abort { .. })
                ),
                "NO {end}"
            );
            assert!(
                matches!(
                    parse(&format!("W5XXX 44 ABCDEFGH RX {end}"), &v),
                    Ok(FieldMsg::Open {
                        cmd: Command::Rx,
                        ..
                    })
                ),
                "RX {end}"
            );
            assert!(
                matches!(
                    parse(&format!("W5XXX 44 ABCDEFGH WX DL88 {end}"), &v),
                    Ok(FieldMsg::Open {
                        cmd: Command::Wx { place: Some(_) },
                        ..
                    })
                ),
                "WX {end}"
            );
        }
        // Words after the over are not an over.
        assert_eq!(
            parse("OK 43 ABCDEFGH K TU", &v),
            Err(ParseError::TrailingGarbage("K TU".into()))
        );
        // A code whose last letter a long gap split off, then the over.
        assert_eq!(
            parse("OK 43 ABCDEFG K K", &v),
            Ok(FieldMsg::Commit {
                seq: 43,
                code: "ABCDEFGK".into()
            })
        );
        assert_eq!(parse("K (", &v), Err(ParseError::Empty));
    }

    #[test]
    fn grids() {
        assert!(is_grid("DL88"));
        assert!(is_grid("DL88af"));
        assert!(!is_grid("DL8"));
        assert!(!is_grid("SL88"));
    }
}
