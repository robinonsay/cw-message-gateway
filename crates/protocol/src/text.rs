//! Making arbitrary text sendable in CW.

use cw::is_sendable;

/// Uppercase `text`, map common characters Morse lacks onto ones it has, drop the rest,
/// and collapse whitespace. The result contains only characters in the Morse table
/// and single spaces.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let mapped: &str = match c {
            '!' => ".",
            ';' => ",",
            '&' => " AND ",
            '%' => " PCT ",
            '#' => " NR ",
            '\u{2018}' | '\u{2019}' => "'",
            '\u{201C}' | '\u{201D}' => "\"",
            '\u{2013}' | '\u{2014}' | '_' => "-",
            '\n' | '\r' | '\t' => " ",
            _ => "",
        };
        if !mapped.is_empty() {
            out.push_str(mapped);
        } else {
            let u = c.to_ascii_uppercase();
            if u.is_ascii() && is_sendable(u) {
                out.push(u);
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(
            sanitize("Hi Robin!  Dinner @ 6 & bring 2% milk 😀\nok?"),
            "HI ROBIN. DINNER @ 6 AND BRING 2 PCT MILK OK?"
        );
        assert_eq!(sanitize("it’s—fine"), "IT'S-FINE");
    }
}
