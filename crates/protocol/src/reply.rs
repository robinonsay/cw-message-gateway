//! What the node transmits.

use crate::text::sanitize;

/// The node's replies, rendered by [`Reply::render`] into sendable CW text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Read-back of a `TX` request.
    ReadBackTx {
        seq: u64,
        dest: String,
        text: String,
    },
    /// Read-back of an `RX` request: how many messages are waiting.
    ReadBackRx { seq: u64, count: usize },
    /// Read-back of a `WX` request.
    ReadBackWx { seq: u64, grid: Option<String> },
    /// The message was handed to the email/SMS gateway.
    Sent { seq: u64 },
    /// The commit was accepted but the action failed; the operator may retry with
    /// fresh codes.
    Failed { seq: u64, reason: String },
    /// The pending transaction was dropped.
    Aborted,
    /// Nothing waiting after `OK` on an `RX`.
    NoMessages { seq: u64 },
}

impl Reply {
    /// The text to key, ending with the node's identification and `K`.
    ///
    /// Every transmission carries `DE <callsign>`, which satisfies the
    /// identification rule (47 CFR 97.119) without the node tracking ID timing.
    pub fn render(&self, node_call: &str) -> String {
        let body = match self {
            Self::ReadBackTx { seq, dest, text } => {
                format!("R {seq} TX {dest} {} ?", sanitize(text))
            }
            Self::ReadBackRx { seq, count } => {
                format!(
                    "R {seq} {count} {} ?",
                    if *count == 1 { "MSG" } else { "MSGS" }
                )
            }
            Self::ReadBackWx { seq, grid } => match grid {
                Some(g) => format!("R {seq} WX {g} ?"),
                None => format!("R {seq} WX ?"),
            },
            Self::Sent { seq } => format!("SENT {seq}"),
            Self::Failed { seq, reason } => format!("FAIL {seq} {}", sanitize(reason)),
            Self::Aborted => "R NO".to_string(),
            Self::NoMessages { seq } => format!("R {seq} NIL"),
        };
        format!("{body} DE {node_call} K")
    }
}

/// One piece of a long transmission. The operator can ask for it again with
/// `AGN <letter>` instead of the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub letter: char,
    pub text: String,
}

impl Chunk {
    /// `<text> = <letter>`: the chunk ends with BT and its letter, as the spec asks.
    pub fn render(&self) -> String {
        format!("{} = {}", self.text, self.letter)
    }
}

/// Split `text` into chunks of at most `max_chars` (on word boundaries where possible),
/// lettered A, B, C... At most 26 chunks are produced; anything beyond is dropped and
/// the last chunk says `MORE`.
pub fn chunk(text: &str, max_chars: usize) -> Vec<Chunk> {
    let max_chars = max_chars.max(8);
    let text = sanitize(text);
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        // Hard-split words longer than a whole chunk.
        while word.len() > max_chars {
            let rest = word.split_off(max_chars);
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
            }
            pieces.push(std::mem::replace(&mut word, rest));
        }
        if !cur.is_empty() && cur.len() + 1 + word.len() > max_chars {
            pieces.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    if pieces.len() > 26 {
        pieces.truncate(26);
        pieces[25].push_str(" MORE");
    }
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, text)| Chunk {
            letter: (b'A' + i as u8) as char,
            text,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_spec_examples() {
        let r = Reply::ReadBackTx {
            seq: 42,
            dest: "MOM".into(),
            text: "running late home sun".into(),
        };
        assert_eq!(
            r.render("N0DE"),
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K"
        );
        assert_eq!(
            Reply::ReadBackRx { seq: 44, count: 3 }.render("N0DE"),
            "R 44 3 MSGS ? DE N0DE K"
        );
        assert_eq!(Reply::Sent { seq: 43 }.render("N0DE"), "SENT 43 DE N0DE K");
    }

    #[test]
    fn chunks_on_word_boundaries() {
        let c = chunk("the quick brown fox jumps over the lazy dog", 15);
        let texts: Vec<_> = c.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["THE QUICK BROWN", "FOX JUMPS OVER", "THE LAZY DOG"]);
        assert_eq!(c[1].letter, 'B');
        assert_eq!(c[1].render(), "FOX JUMPS OVER = B");
        assert!(chunk("", 10).is_empty());
        let long = chunk("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 10);
        assert_eq!(
            long.iter().map(|c| c.text.len()).collect::<Vec<_>>(),
            [10, 10, 6]
        );
    }
}
