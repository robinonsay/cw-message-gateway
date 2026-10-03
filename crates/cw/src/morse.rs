//! The International Morse code table (ITU-R M.1677-1).

/// (character, pattern). Prosigns are written as lowercase-free bracketed text by the
/// decoder only if they are not a plain character; the common ones map to ASCII
/// stand-ins that the IC-7300 keyer also uses: `=` is BT, `+` is AR.
const TABLE: &[(char, &str)] = &[
    ('A', ".-"),
    ('B', "-..."),
    ('C', "-.-."),
    ('D', "-.."),
    ('E', "."),
    ('F', "..-."),
    ('G', "--."),
    ('H', "...."),
    ('I', ".."),
    ('J', ".---"),
    ('K', "-.-"),
    ('L', ".-.."),
    ('M', "--"),
    ('N', "-."),
    ('O', "---"),
    ('P', ".--."),
    ('Q', "--.-"),
    ('R', ".-."),
    ('S', "..."),
    ('T', "-"),
    ('U', "..-"),
    ('V', "...-"),
    ('W', ".--"),
    ('X', "-..-"),
    ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"),
    ('1', ".----"),
    ('2', "..---"),
    ('3', "...--"),
    ('4', "....-"),
    ('5', "....."),
    ('6', "-...."),
    ('7', "--..."),
    ('8', "---.."),
    ('9', "----."),
    ('.', ".-.-.-"),
    (',', "--..--"),
    ('?', "..--.."),
    ('\'', ".----."),
    ('/', "-..-."),
    ('(', "-.--."),
    (')', "-.--.-"),
    (':', "---..."),
    ('=', "-...-"),
    ('+', ".-.-."),
    ('-', "-....-"),
    ('"', ".-..-."),
    ('@', ".--.-."),
];

/// The dot/dash pattern for `c` (case-insensitive), or `None` if Morse has no such character.
pub fn encode_char(c: char) -> Option<&'static str> {
    let c = c.to_ascii_uppercase();
    TABLE.iter().find(|(ch, _)| *ch == c).map(|(_, p)| *p)
}

/// The character for a dot/dash pattern, or `None` if it is not in the table.
pub fn decode_pattern(pattern: &str) -> Option<char> {
    TABLE.iter().find(|(_, p)| *p == pattern).map(|(c, _)| *c)
}

/// Whether `c` can be sent in Morse (spaces count as word gaps).
pub fn is_sendable(c: char) -> bool {
    c == ' ' || encode_char(c).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_a_bijection() {
        for (i, (c1, p1)) in TABLE.iter().enumerate() {
            for (c2, p2) in &TABLE[i + 1..] {
                assert_ne!(c1, c2);
                assert_ne!(p1, p2, "{c1} and {c2} share a pattern");
            }
            assert_eq!(decode_pattern(p1), Some(*c1));
        }
    }

    #[test]
    fn lookups() {
        assert_eq!(encode_char('k'), Some("-.-"));
        assert_eq!(decode_pattern("...---..."), None);
        assert!(is_sendable(' '));
        assert!(!is_sendable('#'));
    }
}
