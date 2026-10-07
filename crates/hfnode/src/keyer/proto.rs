//! The keyer box's commands as hfnode sends them, and its replies parsed: the
//! node's side of docs/keyer-protocol.md. The lines themselves are
//! [`keyer_core::frame`]'s, the same code the box runs.

use keyer_core::keyer::{Boot, Ended, Trip};
use std::time::Duration;

/// A command to the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Hello,
    Status,
    Stop,
    /// Key `text` (1-30 characters) at `wpm`.
    Cw {
        wpm: u32,
        text: String,
    },
    /// Bring-up only: stop the box's control loop at the next key-down, so that
    /// only its watchdog can open the key.
    TestHang,
    /// Bring-up only: hold the next key-down, so that the box's key-down limit
    /// must open the key (and trip the box).
    TestStuck,
}

impl Command {
    /// The line body.
    pub fn body(&self) -> String {
        match self {
            Self::Hello => "HELLO".into(),
            Self::Status => "STATUS".into(),
            Self::Stop => "STOP".into(),
            Self::Cw { wpm, text } => format!("CW {wpm} {text}"),
            Self::TestHang => "TEST HANG".into(),
            Self::TestStuck => "TEST STUCK".into(),
        }
    }

    /// The first word, which the reply repeats.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Hello => "HELLO",
            Self::Status => "STATUS",
            Self::Stop => "STOP",
            Self::Cw { .. } => "CW",
            Self::TestHang | Self::TestStuck => "TEST",
        }
    }

    /// Whether sending it twice does no harm, so that it may be sent again when
    /// its reply is lost. `CW` and the tests start something.
    pub fn repeatable(&self) -> bool {
        matches!(self, Self::Hello | Self::Status | Self::Stop)
    }
}

/// The box's answer: `OK <command> <fields>` or `ERR <command> <code>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The fields after `OK <command>` (for `TEST`, after `OK TEST HANG` or
    /// `OK TEST STUCK`).
    Ok(Vec<String>),
    Err(String),
}

/// Whether reply `body` answers `cmd` (and not some other command under the same
/// id, left from before the link was opened).
pub fn answers(cmd: &Command, body: &str) -> bool {
    let mut w = body.split(' ');
    let (Some(kind), Some(name)) = (w.next(), w.next()) else {
        return false;
    };
    match (kind, cmd) {
        ("OK", Command::TestHang) => body == "OK TEST HANG",
        ("OK", Command::TestStuck) => body == "OK TEST STUCK",
        ("OK" | "ERR", _) => name == cmd.name(),
        _ => false,
    }
}

/// Parse reply `body` to `cmd`, which [`answers`] it.
pub fn parse_reply(cmd: &Command, body: &str) -> Result<Reply, String> {
    let fields: Vec<&str> = body.split(' ').collect();
    match fields.as_slice() {
        ["ERR", _, code] => Ok(Reply::Err((*code).to_string())),
        ["OK", "TEST", _] => Ok(Reply::Ok(Vec::new())),
        ["OK", _, rest @ ..] => Ok(Reply::Ok(rest.iter().map(|s| s.to_string()).collect())),
        _ => Err(format!("reply {body:?} to {} not understood", cmd.name())),
    }
}

/// What a box code means, for the log and the operator.
pub fn explain(cmd: &Command, code: &str) -> Option<&'static str> {
    Some(match (cmd.name(), code) {
        ("CW", "TRIP") => {
            "the box tripped: its key stayed down past its limit; unplug it and plug it \
             in again"
        }
        ("CW", "RUN") => "a run is already under way",
        ("CW", "WPM") => "speed outside 5-50 wpm",
        ("CW", "LEN") => "1 to 30 characters per run",
        ("CW", "CHAR") => "a character the box cannot key",
        ("CW", "LIMIT") => "longer than the box's run limit at this speed",
        ("TEST", "RUN") => "only during a run",
        (_, "UNKNOWN") => "the box does not know this command (older firmware?)",
        _ => return None,
    })
}

/// The box's `HELLO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// Longest one `CW` run may last.
    pub run_limit: Duration,
    /// A run stops when no valid line has arrived for this long.
    pub link_timeout: Duration,
    /// Longest the key may stay down at once before the box trips.
    pub key_down_limit: Duration,
    /// Time since the box started.
    pub uptime: Duration,
    /// Why it last started.
    pub boot: Boot,
    pub name: String,
}

impl Hello {
    /// From the fields of `OK HELLO`.
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [version, run, link, down, uptime, boot, name] = f else {
            return Err(format!("HELLO has {} fields, not 7", f.len()));
        };
        let num = |s: &str, what: &str| {
            s.parse::<u64>()
                .map_err(|_| format!("HELLO {what} {s:?} is not a number"))
        };
        Ok(Self {
            version: num(version, "version")? as u32,
            run_limit: Duration::from_secs(num(run, "run limit")?),
            link_timeout: Duration::from_millis(num(link, "link timeout")?),
            key_down_limit: Duration::from_millis(num(down, "key-down limit")?),
            uptime: Duration::from_millis(num(uptime, "uptime")?),
            boot: Boot::parse(boot).ok_or_else(|| format!("HELLO boot {boot:?} unknown"))?,
            name: name.clone(),
        })
    }
}

/// The box's `STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// The key is down now.
    pub key: bool,
    /// A `CW` run is under way.
    pub run: bool,
    /// How the last run ended.
    pub ended: Ended,
    pub trip: Trip,
}

impl Status {
    /// From the fields of `OK STATUS`.
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [key, run, ended, trip] = f else {
            return Err(format!("STATUS has {} fields, not 4", f.len()));
        };
        let bit = |s: &str, what: &str| match s {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(format!("STATUS {what} {s:?} is not 0 or 1")),
        };
        Ok(Self {
            key: bit(key, "key")?,
            run: bit(run, "run")?,
            ended: Ended::parse(ended).ok_or_else(|| format!("STATUS ended {ended:?} unknown"))?,
            trip: Trip::parse(trip).ok_or_else(|| format!("STATUS trip {trip:?} unknown"))?,
        })
    }

    /// The box is keying, or may be about to.
    pub fn busy(&self) -> bool {
        self.key || self.run
    }
}

/// The line for `cmd` under `id`, without the newline.
pub fn encode(id: u8, cmd: &Command) -> Option<String> {
    keyer_core::frame::encode(id, format_args!("{}", cmd.body())).map(|l| l.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyer_core::keyer::{Keyer, Limits};

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Every reply the box's own code gives parses here.
    #[test]
    fn the_box_s_replies_parse() {
        let mut k = Keyer::new(Limits::BOX, Boot::Watchdog, 1234);
        let mut ask = |id: u8, cmd: &Command| {
            let line = encode(id, cmd).unwrap();
            let reply = k.handle_line(1500, line.as_bytes()).unwrap();
            let (rid, body) = keyer_core::frame::decode(reply.as_bytes()).unwrap();
            assert_eq!(rid, id);
            assert!(answers(cmd, body), "{body}");
            parse_reply(cmd, body).unwrap()
        };
        let Reply::Ok(f) = ask(1, &Command::Hello) else {
            panic!()
        };
        let h = Hello::parse(&f).unwrap();
        assert_eq!(h.version, keyer_core::VERSION);
        assert_eq!(h.run_limit, Duration::from_secs(60));
        assert_eq!(h.link_timeout, Duration::from_secs(2));
        assert_eq!(h.key_down_limit, Duration::from_secs(1));
        assert_eq!(h.uptime, Duration::from_millis(1500));
        assert_eq!(h.boot, Boot::Watchdog);
        assert_eq!(h.name, "PICO2-KEYER");
        let cw = Command::Cw {
            wpm: 20,
            text: "TEST".into(),
        };
        assert_eq!(ask(2, &cw), Reply::Ok(Vec::new()));
        let Reply::Ok(f) = ask(3, &Command::Status) else {
            panic!()
        };
        let s = Status::parse(&f).unwrap();
        assert!(s.key && s.run && s.busy());
        assert_eq!((s.ended, s.trip), (Ended::None, Trip::None));
        assert_eq!(ask(4, &cw), Reply::Err("RUN".into()));
        assert_eq!(ask(5, &Command::TestStuck), Reply::Ok(Vec::new()));
        assert_eq!(ask(6, &Command::Stop), Reply::Ok(Vec::new()));
        assert_eq!(ask(7, &Command::TestHang), Reply::Err("RUN".into()));
        let Reply::Ok(f) = ask(8, &Command::Status) else {
            panic!()
        };
        assert_eq!(
            Status::parse(&f).unwrap(),
            Status {
                key: false,
                run: false,
                ended: Ended::Stop,
                trip: Trip::None
            }
        );
    }

    #[test]
    fn replies_to_other_commands_are_not_taken() {
        assert!(!answers(&Command::Status, "OK STOP"));
        assert!(!answers(&Command::TestHang, "OK TEST STUCK"));
        assert!(answers(&Command::TestHang, "ERR TEST RUN"));
        assert!(answers(
            &Command::Cw {
                wpm: 20,
                text: "E".into()
            },
            "ERR CW TRIP"
        ));
        assert!(!answers(&Command::Hello, "HELLO"));
        assert!(!answers(&Command::Hello, "NO HELLO"));
    }

    #[test]
    fn bad_fields_are_errors() {
        assert!(Status::parse(&strings(&["1", "0", "DONE"])).is_err());
        assert!(Status::parse(&strings(&["2", "0", "DONE", "NONE"])).is_err());
        assert!(Status::parse(&strings(&["0", "0", "MAYBE", "NONE"])).is_err());
        assert!(Hello::parse(&strings(&["1", "60", "2000", "1000", "5", "LUNCH", "X"])).is_err());
        assert!(Hello::parse(&strings(&["1", "60", "2s", "1000", "5", "POWER", "X"])).is_err());
        assert!(parse_reply(&Command::Stop, "OK").is_err());
    }

    #[test]
    fn codes_are_explained() {
        let cw = Command::Cw {
            wpm: 20,
            text: "E".into(),
        };
        assert!(explain(&cw, "TRIP").unwrap().contains("unplug"));
        assert!(explain(&Command::Status, "UNKNOWN").is_some());
        assert!(explain(&cw, "XYZ").is_none());
    }
}
