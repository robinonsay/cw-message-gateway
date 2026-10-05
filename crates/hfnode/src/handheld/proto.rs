//! The serial command set between `hfnode` and a handheld's CW firmware, version 1
//! (docs/handheld-protocol.md has the firmware's side of it, and
//! firmware/uv-k1/app/hfnode.c is that firmware).
//!
//! Every line is printable ASCII ending in `\n`: `<id> <body>*<cs>`, where `<id>` is
//! two hex digits the reply repeats, so a late reply to an earlier command is never
//! taken for the answer to this one, and `<cs>` is two hex digits, the XOR of every
//! byte before the `*`. A line whose checksum does not match is ignored by both
//! sides. Replies are `OK <command> [fields]` or `ERR <command> <code>`.
//!
//! Nothing in it sets the radio up: frequency, mode, power and break-in are read,
//! and set by hand at the radio. Only `CW` keys it.

use std::fmt;
use std::time::Duration;

pub const VERSION: u32 = 1;

/// Longest line either side sends, without the newline.
pub const MAX_LINE: usize = 80;

/// XOR of the bytes of `s`.
pub fn checksum(s: &str) -> u8 {
    s.bytes().fold(0, |a, b| a ^ b)
}

/// The line for `body` under `id`, without the newline.
pub fn encode(id: u8, body: &str) -> String {
    let s = format!("{id:02X} {body}");
    let cs = checksum(&s);
    format!("{s}*{cs:02X}")
}

/// The id and body of a received line (without its newline), if its checksum
/// matches.
pub fn decode(line: &str) -> Result<(u8, &str), String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.len() > MAX_LINE {
        return Err("line too long".into());
    }
    let (msg, cs) = line.rsplit_once('*').ok_or("no checksum")?;
    let cs = parse_hex(cs).ok_or("bad checksum field")?;
    if cs != checksum(msg) {
        return Err("checksum mismatch".into());
    }
    let (id, body) = msg.split_once(' ').ok_or("no id")?;
    let id = parse_hex(id).ok_or("bad id")?;
    Ok((id, body))
}

fn parse_hex(s: &str) -> Option<u8> {
    if s.len() != 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(s, 16).ok()
}

/// Transmit power, from the handheld's own levels: what `[handheld] power` says
/// the radio must be set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    /// Any of the radio's low levels, `LOW1` to `LOW5`.
    #[default]
    Low,
    Mid,
    High,
}

impl Power {
    /// Whether the level the firmware reads back (`OK POWER <level>`) is this one.
    pub fn matches(self, level: &str) -> bool {
        match self {
            Self::Low => matches!(level, "LOW1" | "LOW2" | "LOW3" | "LOW4" | "LOW5"),
            Self::Mid => level == "MID",
            Self::High => level == "HIGH",
        }
    }
}

impl fmt::Display for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Low => "LOW1-LOW5",
            Self::Mid => "MID",
            Self::High => "HIGH",
        })
    }
}

/// A command to the firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Protocol version and the firmware's own transmit limits.
    Hello,
    /// Transmit state and how long the frequency has been quiet.
    Status,
    /// Read the receive and transmit frequencies.
    Freq,
    /// Read the transmit and receive modes.
    Mode,
    /// Read the transmit power level.
    Power,
    /// Read whether break-in is on: without it the keyer only sounds the sidetone.
    Breakin,
    /// Key `text` at `wpm` with the firmware's keyer. Answered once keying has begun.
    Cw { wpm: u32, text: String },
    /// Stop keying at once.
    Stop,
    /// Bring-up only: stop the firmware's main loop during a run, so that its
    /// watchdog must reset the radio.
    TestHang,
}

impl Command {
    /// The word that starts the command and its reply.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Hello => "HELLO",
            Self::Status => "STATUS",
            Self::Freq => "FREQ",
            Self::Mode => "MODE",
            Self::Power => "POWER",
            Self::Breakin => "BREAKIN",
            Self::Cw { .. } => "CW",
            Self::Stop => "STOP",
            Self::TestHang => "TEST",
        }
    }

    pub fn body(&self) -> String {
        match self {
            Self::Cw { wpm, text } => format!("CW {wpm} {text}"),
            Self::TestHang => "TEST HANG".into(),
            _ => self.name().into(),
        }
    }

    /// Whether sending it twice does no more than sending it once, so that it may
    /// be sent again when its reply is lost. Not so for `CW`, which would key the
    /// text twice, nor for `TEST HANG`.
    pub fn repeatable(&self) -> bool {
        !matches!(self, Self::Cw { .. } | Self::TestHang)
    }
}

/// What an `ERR` code means, for the operator.
pub fn explain(cmd: &Command, code: &str) -> Option<&'static str> {
    Some(match (cmd, code) {
        (Command::Cw { .. }, "MODE") => "the radio is not in CW: set CW on it",
        (Command::Cw { .. }, "BKIN") => {
            "break-in is off on the radio, so its keyer would only sound the sidetone: \
             turn it on in the CW menu"
        }
        (Command::Cw { .. }, "REFUSED") => {
            "the radio would not transmit: its TxLock setting for this channel, its \
             busy-channel lock with someone on the frequency, a frequency it does not \
             transmit on, a low battery, or the key or paddle in use"
        }
        (Command::Cw { .. }, "TX") => {
            "the radio is busy: transmitting, or recording or playing a CW memory"
        }
        (Command::Cw { .. }, "WAIT") => {
            "the firmware is still checking that its last stop turned the transmitter \
             off (for a second after it)"
        }
        (Command::Cw { .. }, "DUTY") => {
            "the firmware's own limit on time keyed: it has been transmitting for more \
             than half the time lately, and must rest on receive"
        }
        (Command::Cw { .. }, "CHECK") => {
            "the firmware never read the radio chip transmitting during its last run, so \
             it could not tell a transmitter stuck on: switch the radio off and on, and \
             do not leave it to the node until this is understood"
        }
        (Command::Cw { .. }, "STOP") => "stopped before it began",
        (Command::TestHang, "RUN") => "nothing was being sent",
        (_, "UNKNOWN") => "the firmware does not have this command: is it the hfnode build?",
        _ => return None,
    })
}

/// A reply: the fields after `OK <command>`, or the code after `ERR <command>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok(Vec<String>),
    Err(String),
}

/// Whether `body` is a reply (`OK` or `ERR`) to a command named as `cmd` is.
pub fn answers(cmd: &Command, body: &str) -> bool {
    body.split(' ').nth(1) == Some(cmd.name())
}

/// The reply in `body` to `cmd`.
pub fn parse_reply(cmd: &Command, body: &str) -> Result<Reply, String> {
    let mut words = body.split(' ');
    let kind = words.next().unwrap_or_default();
    if words.next() != Some(cmd.name()) {
        return Err(format!("reply {body:?} is not for {}", cmd.name()));
    }
    let rest: Vec<String> = words.map(str::to_string).collect();
    match kind {
        "OK" => Ok(Reply::Ok(rest)),
        "ERR" => Ok(Reply::Err(rest.join(" "))),
        _ => Err(format!("reply {body:?} is neither OK nor ERR")),
    }
}

/// `OK HELLO <version> <tx limit, s> <link timeout, ms> <uptime, ms> <firmware
/// name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// The firmware stops any one keying run after this long.
    pub tx_limit: Duration,
    /// The firmware stops keying when no valid line has come for this long.
    pub link_timeout: Duration,
    /// How long since the firmware started (its commands, after the radio's own
    /// start-up), by its own clock.
    pub uptime: Duration,
    pub name: String,
}

impl Hello {
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [version, tx, link, uptime, name @ ..] = f else {
            return Err(format!("HELLO reply {f:?}: too few fields"));
        };
        let num = |s: &str, what: &str| {
            s.parse::<u32>()
                .map_err(|_| format!("HELLO reply: {what} {s:?} is not a number"))
        };
        Ok(Self {
            version: num(version, "version")?,
            tx_limit: Duration::from_secs(num(tx, "transmit limit")?.into()),
            link_timeout: Duration::from_millis(num(link, "link timeout")?.into()),
            uptime: Duration::from_millis(num(uptime, "uptime")?.into()),
            name: name.join(" "),
        })
    }
}

/// `OK STATUS <tx> <quiet, ms>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Sending: from a `CW` being accepted until its text has gone out, a `STOP`,
    /// or one of the firmware's limits. The transmitter's real state, not a
    /// flag the node set.
    pub tx: bool,
    /// How long since the squelch was last open (someone else on the frequency),
    /// zero while it is.
    pub quiet: Duration,
}

impl Status {
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [tx, quiet] = f else {
            return Err(format!("STATUS reply {f:?}: expected two fields"));
        };
        let tx = match tx.as_str() {
            "0" => false,
            "1" => true,
            _ => return Err(format!("STATUS reply: transmit {tx:?} is not 0 or 1")),
        };
        let quiet = quiet
            .parse::<u64>()
            .map_err(|_| format!("STATUS reply: quiet {quiet:?} is not a number"))?;
        Ok(Self {
            tx,
            quiet: Duration::from_millis(quiet),
        })
    }
}

/// `OK FREQ <receive Hz> <transmit Hz>`.
pub fn parse_freq(f: &[String]) -> Result<(u64, u64), String> {
    let [rx, tx] = f else {
        return Err(format!("FREQ reply {f:?}: expected two fields"));
    };
    let hz = |s: &str| {
        s.parse::<u64>()
            .map_err(|_| format!("FREQ reply: {s:?} is not a frequency"))
    };
    Ok((hz(rx)?, hz(tx)?))
}

/// `OK MODE <transmit mode> <receive mode>`: `CW`, `FM`, `AM`, `USB`, ... or
/// `OTHER`.
pub fn parse_mode(f: &[String]) -> Result<(String, String), String> {
    match f {
        [tx, rx] => Ok((tx.clone(), rx.clone())),
        _ => Err(format!("MODE reply {f:?}: expected two fields")),
    }
}

/// `OK POWER <level>`: `LOW1` to `LOW5`, `MID`, `HIGH`, `USER` or `OTHER`.
pub fn parse_power(f: &[String]) -> Result<String, String> {
    match f {
        [level] => Ok(level.clone()),
        _ => Err(format!("POWER reply {f:?}: expected one field")),
    }
}

/// `OK BREAKIN <0 or 1>`.
pub fn parse_breakin(f: &[String]) -> Result<bool, String> {
    match f {
        [on] if on == "1" => Ok(true),
        [off] if off == "0" => Ok(false),
        _ => Err(format!("BREAKIN reply {f:?}: expected 0 or 1")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn lines_round_trip_with_their_checksum() {
        let line = encode(0x2A, "CW 20 CQ DE N0CALL");
        assert!(line.starts_with("2A CW 20 CQ DE N0CALL*"));
        assert_eq!(decode(&line), Ok((0x2A, "CW 20 CQ DE N0CALL")));
        assert_eq!(
            decode(&format!("{line}\r")),
            Ok((0x2A, "CW 20 CQ DE N0CALL"))
        );
    }

    #[test]
    fn a_damaged_line_is_refused() {
        let line = encode(1, "STOP");
        // One character changed, in the body or the checksum.
        let bad = line.replace("STOP", "STOQ");
        assert!(decode(&bad).is_err());
        let mut cs = line.clone();
        let last = cs.pop().unwrap();
        cs.push(if last == '0' { '1' } else { '0' });
        assert!(decode(&cs).is_err());
        assert!(decode("01 STOP").is_err(), "no checksum");
        assert!(decode(&"A".repeat(MAX_LINE + 1)).is_err());
    }

    #[test]
    fn a_known_line_has_this_checksum() {
        // Worked by hand for docs/handheld-protocol.md.
        assert_eq!(encode(1, "STATUS"), "01 STATUS*35");
    }

    #[test]
    fn the_documented_example_lines_are_correct() {
        let doc = include_str!("../../../../docs/handheld-protocol.md");
        let lines: Vec<&str> = doc
            .lines()
            .filter_map(|l| l.strip_prefix("node: ").or(l.strip_prefix("fw:   ")))
            .collect();
        assert_eq!(lines.len(), 22);
        for l in lines {
            let (id, body) = decode(l).unwrap_or_else(|e| panic!("{l}: {e}"));
            assert_eq!(encode(id, body), l);
            if let Some(f) = body.strip_prefix("OK HELLO ") {
                let f: Vec<String> = f.split(' ').map(str::to_string).collect();
                let h = Hello::parse(&f).unwrap();
                assert_eq!(h.name, "NR7Y-CW HFNODE");
            }
        }
    }

    #[test]
    fn replies_must_answer_the_command_sent() {
        assert_eq!(
            parse_reply(&Command::Status, "OK STATUS 0 1500"),
            Ok(Reply::Ok(words("0 1500")))
        );
        assert_eq!(
            parse_reply(&Command::Stop, "OK STOP"),
            Ok(Reply::Ok(Vec::new()))
        );
        assert_eq!(
            parse_reply(
                &Command::Cw {
                    wpm: 20,
                    text: "X".into()
                },
                "ERR CW TX"
            ),
            Ok(Reply::Err("TX".into()))
        );
        assert!(parse_reply(&Command::Stop, "OK STATUS 0 0").is_err());
        assert!(parse_reply(&Command::Stop, "MAYBE STOP").is_err());
    }

    #[test]
    fn reads_hello_status_and_freq() {
        assert_eq!(
            Hello::parse(&words("1 60 2000 12340 UV-K1 CW")),
            Ok(Hello {
                version: 1,
                tx_limit: Duration::from_secs(60),
                link_timeout: Duration::from_millis(2000),
                uptime: Duration::from_millis(12340),
                name: "UV-K1 CW".into(),
            })
        );
        assert!(Hello::parse(&words("1 60 2000")).is_err());
        assert!(Hello::parse(&words("1 60 2000 UV-K1 CW")).is_err());
        assert_eq!(
            Status::parse(&words("1 0")),
            Ok(Status {
                tx: true,
                quiet: Duration::ZERO
            })
        );
        assert!(Status::parse(&words("2 0")).is_err());
        assert!(Status::parse(&words("0")).is_err());
        assert_eq!(
            parse_freq(&words("144060000 144060000")),
            Ok((144_060_000, 144_060_000))
        );
        assert!(parse_freq(&words("144060000")).is_err());
    }

    #[test]
    fn reads_mode_power_and_break_in() {
        assert_eq!(parse_mode(&words("CW FM")), Ok(("CW".into(), "FM".into())));
        assert!(parse_mode(&words("CW")).is_err());
        assert_eq!(parse_power(&words("LOW3")), Ok("LOW3".into()));
        assert!(parse_power(&words("LOW 3")).is_err());
        assert_eq!(parse_breakin(&words("1")), Ok(true));
        assert_eq!(parse_breakin(&words("0")), Ok(false));
        assert!(parse_breakin(&words("2")).is_err());
        assert!(Power::Low.matches("LOW1") && Power::Low.matches("LOW5"));
        assert!(!Power::Low.matches("LOW") && !Power::Low.matches("MID"));
        assert!(Power::Mid.matches("MID") && !Power::Mid.matches("HIGH"));
        assert!(Power::High.matches("HIGH") && !Power::High.matches("USER"));
    }

    #[test]
    fn only_cw_and_the_hang_test_are_not_repeated() {
        assert!(Command::Stop.repeatable());
        assert!(Command::Freq.repeatable());
        assert!(!Command::TestHang.repeatable());
        assert!(!Command::Cw {
            wpm: 20,
            text: "E".into()
        }
        .repeatable());
        assert_eq!(Command::TestHang.body(), "TEST HANG");
        assert_eq!(
            parse_reply(&Command::TestHang, "OK TEST HANG"),
            Ok(Reply::Ok(words("HANG")))
        );
        assert_eq!(Command::Breakin.body(), "BREAKIN");
    }

    /// The firmware in firmware/uv-k1, read as text: its limits and character set
    /// must be the node's.
    mod firmware {
        use super::super::*;

        const HFNODE_C: &str = include_str!("../../../../firmware/uv-k1/app/hfnode.c");
        const LINE_C: &str = include_str!("../../../../firmware/uv-k1/app/hfnode_line.c");
        const LINE_H: &str = include_str!("../../../../firmware/uv-k1/app/hfnode_line.h");
        const PATCH: &str = include_str!("../../../../firmware/uv-k1/nr7y-hfnode.patch");

        /// The value of `#define <name> <value>` in `src`, the only one.
        fn define(src: &str, name: &str) -> u64 {
            let found: Vec<u64> = src
                .lines()
                .filter_map(|l| {
                    let mut w = l.split_whitespace();
                    (w.next() == Some("#define") && w.next() == Some(name))
                        .then(|| w.next().unwrap().trim_end_matches('u').parse().unwrap())
                })
                .collect();
            match found[..] {
                [v] => v,
                _ => panic!("{} #define {name}", found.len()),
            }
        }

        #[test]
        fn its_limits_are_the_ones_the_node_accepts() {
            assert_eq!(define(HFNODE_C, "HF_VERSION"), u64::from(VERSION));
            let tx_limit = Duration::from_secs(define(HFNODE_C, "HF_TX_LIMIT_S"));
            assert!(tx_limit <= crate::handheld::MAX_FIRMWARE_TX_LIMIT);
            let link = Duration::from_millis(define(HFNODE_C, "HF_LINK_TIMEOUT_MS"));
            assert!(
                (crate::handheld::MIN_LINK_TIMEOUT..=crate::handheld::MAX_LINK_TIMEOUT)
                    .contains(&link)
            );
            assert_eq!(define(HFNODE_C, "HF_TEXT_MAX"), civ::MAX_CW_CHARS as u64);
            assert_eq!(define(HFNODE_C, "HF_WPM_MIN"), 5);
            assert_eq!(define(HFNODE_C, "HF_WPM_MAX"), 50);
            assert_eq!(define(LINE_H, "HF_LINE_MAX"), MAX_LINE as u64);
            // The CW reply comes once keying has begun, well within the node's wait.
            let start = Duration::from_millis(define(HFNODE_C, "HF_KEY_START_MS"));
            assert!(start + Duration::from_millis(100) < crate::handheld::REPLY_TIMEOUT);
            // Its watchdog: the main loop stale this long, then the watchdog's own
            // count of a 32768 Hz clock divided by 32.
            let stale = Duration::from_millis(define(HFNODE_C, "HF_WDG_STALE_MS"));
            let count =
                Duration::from_millis(define(HFNODE_C, "HF_WDG_RELOAD") * 32 * 1000 / 32768);
            assert!(
                (stale + count).abs_diff(crate::handheld::WATCHDOG_RESET)
                    < Duration::from_millis(100)
            );
            // The node waits out its watch of a stop, and stays within its key-down
            // budget.
            let watch = Duration::from_millis(define(HFNODE_C, "HF_STOP_WATCH_MS"));
            assert!(watch + Duration::from_millis(300) <= crate::handheld::STOP_WATCH_WAIT);
            let budget = Duration::from_millis(define(HFNODE_C, "HF_DUTY_REFUSE_MS"));
            assert!(crate::handheld::MAX_DUTY_BUDGET + Duration::from_secs(10) <= budget);
        }

        #[test]
        fn it_keys_exactly_the_characters_the_node_sends() {
            let (_, rest) = LINE_C.split_once("strchr(\"").expect("the character list");
            let (set, _) = rest.split_once("\", c)").expect("its end");
            let set = set.replace("\\\"", "\"");
            for b in 0x20u8..0x7F {
                let c = b as char;
                let node = !c.is_ascii_lowercase() && cw::is_sendable(c);
                let fw = c.is_ascii_uppercase() || c.is_ascii_digit() || set.contains(c);
                assert_eq!(node, fw, "{c:?}");
            }
        }

        #[test]
        fn the_morse_it_adds_is_the_nodes() {
            // `{'c', length, pattern}`: the first element in the lowest bit, 1 a dah.
            // Only the entries the patch adds, inside its `#ifdef ENABLE_HFNODE`.
            let mut inside = false;
            let added_lines = PATCH.lines().filter(|l| {
                match *l {
                    "+#ifdef ENABLE_HFNODE" => inside = true,
                    "+#endif" => inside = false,
                    _ => {}
                }
                inside
            });
            let mut added = 0;
            for l in added_lines.filter_map(|l| l.strip_prefix("+\t{'")) {
                let (c, rest) = if let Some(r) = l.strip_prefix("\\''") {
                    ('\'', r)
                } else {
                    let mut it = l.chars();
                    let c = it.next().unwrap();
                    (c, it.as_str().strip_prefix('\'').unwrap())
                };
                let fields: Vec<&str> = rest
                    .trim_start_matches(", ")
                    .split(['}', ','])
                    .map(str::trim)
                    .collect();
                let len: usize = fields[0].parse().unwrap();
                let bits = u32::from_str_radix(fields[1].trim_start_matches("0b"), 2).unwrap();
                let pattern: String = (0..len)
                    .map(|i| if bits >> i & 1 == 1 { '-' } else { '.' })
                    .collect();
                assert_eq!(cw::encode_char(c), Some(pattern.as_str()), "{c:?}");
                added += 1;
            }
            assert_eq!(added, 5);
        }
    }
}
