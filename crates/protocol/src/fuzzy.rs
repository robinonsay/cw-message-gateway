//! Snapping noisy decodes onto a small vocabulary by Morse-pattern distance.
//!
//! Plain letter edit distance is the wrong metric for CW: `E` and `T` are one element
//! each and confusing them is common, while `E` and `5` look unrelated as letters but
//! differ only in four extra dits. Comparing the dot/dash patterns, with a character
//! gap marker between letters, measures how many elements the decoder got wrong.

use cw::encode_char;

/// Element-level edit distance between two decoded words.
pub fn morse_distance(a: &str, b: &str) -> usize {
    levenshtein(&pattern(a), &pattern(b))
}

fn pattern(word: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, c) in word.chars().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.extend(encode_char(c).unwrap_or("?").bytes());
    }
    out
}

fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The vocabulary entry nearest to `token`, if it is within `tolerance` and strictly
/// nearer than every other entry. Ties are refused: guessing between two legal
/// tokens is how a garbled decode would turn into the wrong action.
pub fn snap<'v, S: AsRef<str>>(token: &str, vocab: &'v [S], tolerance: usize) -> Option<&'v str> {
    let mut best: Option<(&str, usize)> = None;
    let mut tie = false;
    for v in vocab {
        let v = v.as_ref();
        let d = if v.eq_ignore_ascii_case(token) {
            0
        } else {
            morse_distance(&token.to_ascii_uppercase(), &v.to_ascii_uppercase())
        };
        match best {
            Some((_, bd)) if d > bd => {}
            Some((_, bd)) if d == bd => tie = true,
            _ => {
                best = Some((v, d));
                tie = false;
            }
        }
    }
    match best {
        Some((v, d)) if d <= tolerance && !tie => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_elements() {
        assert_eq!(morse_distance("E", "E"), 0);
        assert_eq!(morse_distance("E", "T"), 1);
        assert_eq!(morse_distance("S", "H"), 1);
        assert_eq!(morse_distance("TX", "RX"), 2);
    }

    #[test]
    fn snap_refuses_ties_and_far_tokens() {
        let kws = ["TX", "RX", "WX"];
        assert_eq!(snap("TX", &kws, 1), Some("TX"));
        // A (.-) is one element from both W (.--) and R (.-.): ambiguous, refused.
        assert_eq!(snap("AX", &kws, 1), None);
        assert_eq!(snap("QQ", &kws, 1), None);
        // "EX" (. -..-) is one element from both TX and... nothing else here: RX
        // (.-. -..-) is 2 away and WX (.-- -..-) is 2 away; TX is 1 away.
        assert_eq!(snap("EX", &kws, 1), Some("TX"));
        assert_eq!(snap("AB", &["AC", "AD"], 2), None);
    }
}
