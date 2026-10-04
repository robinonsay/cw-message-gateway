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
    /// Read-back of a `WX` request: the preset number if one was sent, and the grid
    /// the forecast will be for (`None` only when the node has no grid at all).
    ReadBackWx {
        seq: u64,
        preset: Option<u32>,
        grid: Option<String>,
    },
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
            Self::ReadBackWx { seq, preset, grid } => {
                let mut s = format!("R {seq} WX");
                if let Some(p) = preset {
                    s.push_str(&format!(" {p}"));
                }
                if let Some(g) = grid {
                    s.push_str(&format!(" {g}"));
                }
                s + " ?"
            }
            Self::Sent { seq } => format!("SENT {seq}"),
            Self::Failed { seq, reason } => format!("FAIL {seq} {}", sanitize(reason)),
            Self::Aborted => "R NO".to_string(),
            Self::NoMessages { seq } => format!("R {seq} NIL"),
        };
        format!("{body} DE {node_call} K")
    }
}

/// One piece of a long transmission. The operator can ask for it again with
/// `AGN <line> <code> <letter>` instead of the whole batch.
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

/// Most chunks in one transmission: one per letter A to Z.
pub const MAX_CHUNKS: usize = 26;

/// Split `text` into chunks of at most `max_chars` (on word boundaries where possible),
/// lettered A, B, C... At most [`MAX_CHUNKS`] chunks are produced; anything beyond is
/// dropped and the last chunk says `MORE`. Use [`chunk_count`] first to avoid that.
pub fn chunk(text: &str, max_chars: usize) -> Vec<Chunk> {
    let mut pieces = pieces(text, max_chars);
    if pieces.len() > MAX_CHUNKS {
        pieces.truncate(MAX_CHUNKS);
        pieces[MAX_CHUNKS - 1].push_str(" MORE");
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

/// How many chunks [`chunk`] would need for `text`, without the [`MAX_CHUNKS`] cap.
/// Anything above [`MAX_CHUNKS`] would be cut off.
pub fn chunk_count(text: &str, max_chars: usize) -> usize {
    pieces(text, max_chars).len()
}

fn pieces(text: &str, max_chars: usize) -> Vec<String> {
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
    pieces
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
        let wx = |preset, grid: Option<&str>| {
            Reply::ReadBackWx {
                seq: 46,
                preset,
                grid: grid.map(str::to_string),
            }
            .render("N0DE")
        };
        assert_eq!(wx(None, Some("DL89IG")), "R 46 WX DL89IG ? DE N0DE K");
        assert_eq!(wx(Some(3), Some("DL89IG")), "R 46 WX 3 DL89IG ? DE N0DE K");
        assert_eq!(wx(None, None), "R 46 WX ? DE N0DE K");
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

    #[test]
    fn counts_chunks_past_the_cap() {
        let text = "ABCDEFGH ".repeat(30);
        assert_eq!(chunk_count(&text, 8), 30);
        let c = chunk(&text, 8);
        assert_eq!(c.len(), MAX_CHUNKS);
        assert!(c[MAX_CHUNKS - 1].text.ends_with(" MORE"));
        assert_eq!(chunk_count("", 8), 0);
    }
}
