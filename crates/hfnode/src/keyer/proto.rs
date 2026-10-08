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
    /// Hold the PTT and key `text` (1-30 characters) at `wpm` on the tone.
    Mcw {
        wpm: u32,
        text: String,
    },
    /// Bring-up only: the box takes `TEST HANG`, `TEST STUCK` or `TEST HOLD` only
    /// within 2 s of this, and once.
    TestArm,
    /// Bring-up only: stop the box's control loop at the next key-down, so that
    /// only its watchdog can open the key.
    TestHang,
    /// Bring-up only: hold the next key-down, so that the box's key-down limit
    /// must open the key (and trip the box).
    TestStuck,
    /// Tests only: hold the PTT after an `MCW` run's text, so that the box's PTT
    /// limit must open it (and trip the box).
    TestHold,
}

impl Command {
    /// The line body.
    pub fn body(&self) -> String {
        match self {
            Self::Hello => "HELLO".into(),
            Self::Status => "STATUS".into(),
            Self::Stop => "STOP".into(),
            Self::Cw { wpm, text } => format!("CW {wpm} {text}"),
            Self::Mcw { wpm, text } => format!("MCW {wpm} {text}"),
            Self::TestArm => "TEST ARM".into(),
            Self::TestHang => "TEST HANG".into(),
            Self::TestStuck => "TEST STUCK".into(),
            Self::TestHold => "TEST HOLD".into(),
        }
    }

    /// The first word, which the reply repeats.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Hello => "HELLO",
            Self::Status => "STATUS",
            Self::Stop => "STOP",
            Self::Cw { .. } => "CW",
            Self::Mcw { .. } => "MCW",
            Self::TestArm | Self::TestHang | Self::TestStuck | Self::TestHold => "TEST",
        }
    }

    /// Whether sending it twice does no harm, so that it may be sent again when
    /// its reply is lost. `CW`, `MCW` and the tests start something; `TEST ARM`
    /// only renews itself.
    pub fn repeatable(&self) -> bool {
        matches!(
            self,
            Self::Hello | Self::Status | Self::Stop | Self::TestArm
        )
    }
}

/// The box's answer: `OK <command> <fields>` or `ERR <command> <code>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The fields after `OK <command>` (for `TEST`, after `OK TEST ARM`, `OK TEST
    /// HANG`, `OK TEST STUCK` or `OK TEST HOLD`).
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
        ("OK", Command::TestArm) => body == "OK TEST ARM",
        ("OK", Command::TestHang) => body == "OK TEST HANG",
        ("OK", Command::TestStuck) => body == "OK TEST STUCK",
        ("OK", Command::TestHold) => body == "OK TEST HOLD",
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
        ("CW" | "MCW", "TRIP") => {
            "the box tripped (its key or tone stayed on, its PTT stayed down, or the PTT \
             line stayed low after it let go; STATUS says which); unplug it and plug it \
             in again"
        }
        ("CW" | "MCW", "RUN") => "a run is already under way",
        ("CW" | "MCW", "WPM") => "speed outside 5-50 wpm",
        ("CW" | "MCW", "LEN") => "1 to 30 characters per run",
        ("CW" | "MCW", "CHAR") => "a character the box cannot key",
        ("CW", "LIMIT") => "longer than the box's run limit at this speed",
        ("MCW", "LIMIT") => {
            "with its lead and tail, not under the box's run and PTT limits at this speed"
        }
        ("CW", "LINE") => "the PTT line has not yet been checked since the PTT last moved",
        ("MCW", "LINE") => {
            "the PTT line reads low: the PTT already held, or the radio off; or it has \
             not yet been checked since the PTT last moved"
        }
        ("CW" | "MCW", "REST") => "the box rests 1 s after each run",
        ("CW" | "MCW", "DUTY") => {
            "the box's duty budget is spent: the transmitter must stay unkeyed as long as \
             it was keyed (the PTT's whole time for MCW)"
        }
        ("TEST", "RUN") => "only during a run",
        ("TEST", "ARM") => "a test is taken only just after TEST ARM",
        (_, "UNKNOWN") => "the box does not know this command (older firmware?)",
        _ => return None,
    })
}

/// The box's `HELLO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// Longest one `CW` or `MCW` run may last.
    pub run_limit: Duration,
    /// A run stops when no valid line has arrived for this long.
    pub link_timeout: Duration,
    /// Longest the key may stay down at once before the box trips.
    pub key_down_limit: Duration,
    /// How long the key stays up after a run before the box takes another.
    pub rest: Duration,
    /// The most keyed time (the key or the PTT down) the box keeps in its duty
    /// budget.
    pub duty_budget: Duration,
    /// Longest the PTT may stay down at once before the box trips.
    pub ptt_limit: Duration,
    /// Time since the box started.
    pub uptime: Duration,
    /// Why it last started.
    pub boot: Boot,
    /// The firmware build (a git commit, or `-` for a build without one).
    pub build: String,
    pub name: String,
}

impl Hello {
    /// From the fields of `OK HELLO`. A box of protocol version 1 or 2 is refused
    /// here, by its field count: it has no rest and no duty budget (1), or no PTT
    /// limit (2).
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [version, run, link, down, rest, budget, ptt, uptime, boot, build, name] = f else {
            return Err(format!(
                "HELLO has {} fields, not 11 (older firmware? flash this hfnode's)",
                f.len()
            ));
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
            rest: Duration::from_millis(num(rest, "rest")?),
            duty_budget: Duration::from_secs(num(budget, "duty budget")?),
            ptt_limit: Duration::from_secs(num(ptt, "PTT limit")?),
            uptime: Duration::from_millis(num(uptime, "uptime")?),
            boot: Boot::parse(boot).ok_or_else(|| format!("HELLO boot {boot:?} unknown"))?,
            build: build.clone(),
            name: name.clone(),
        })
    }
}

/// The box's `STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// The key is down now.
    pub key: bool,
    /// A `CW` or `MCW` run is under way.
    pub run: bool,
    /// How the last run ended.
    pub ended: Ended,
    pub trip: Trip,
    /// How long until the rest after the last run is over.
    pub rest_left: Duration,
    /// The duty budget: the keyed time the box would take now.
    pub budget: Duration,
    /// The PTT is down now.
    pub ptt: bool,
    /// The PTT line reads high (the PTT contact open).
    pub line: bool,
}

impl Status {
    /// From the fields of `OK STATUS`.
    pub fn parse(f: &[String]) -> Result<Self, String> {
        let [key, run, ended, trip, rest, budget, ptt, line] = f else {
            return Err(format!("STATUS has {} fields, not 8", f.len()));
        };
        let ms = |s: &str, what: &str| {
            s.parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|_| format!("STATUS {what} {s:?} is not a number"))
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
            rest_left: ms(rest, "rest")?,
            budget: ms(budget, "budget")?,
            ptt: bit(ptt, "ptt")?,
            line: bit(line, "line")?,
        })
    }

    /// The box is keying, or may be about to.
    pub fn busy(&self) -> bool {
        self.key || self.ptt || self.run
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
        assert_eq!(h.rest, Duration::from_secs(1));
        assert_eq!(h.duty_budget, Duration::from_secs(60));
        assert_eq!(h.ptt_limit, Duration::from_secs(60));
        assert_eq!(h.uptime, Duration::from_millis(1500));
        assert_eq!(h.boot, Boot::Watchdog);
        assert_eq!(h.build, "-");
        assert_eq!(h.name, "PICO2-KEYER");
        // A watchdog reset comes up tripped.
        let cw = Command::Cw {
            wpm: 20,
            text: "TEST".into(),
        };
        assert_eq!(ask(2, &cw), Reply::Err("TRIP".into()));
        let Reply::Ok(f) = ask(3, &Command::Status) else {
            panic!()
        };
        assert_eq!(Status::parse(&f).unwrap().trip, Trip::Watchdog);
        // The budget starts empty after another restart: earn some first.
        let mut k = Keyer::new(Limits::BOX, Boot::Other, 1234);
        let line = encode(4, &cw).unwrap();
        let reply = k.handle_line(1500, line.as_bytes()).unwrap();
        let (_, body) = keyer_core::frame::decode(reply.as_bytes()).unwrap();
        assert_eq!(parse_reply(&cw, body).unwrap(), Reply::Err("DUTY".into()));
        let mut k = Keyer::new(Limits::BOX, Boot::Power, 1234);
        let mut ask = |id: u8, cmd: &Command| {
            let line = encode(id, cmd).unwrap();
            let reply = k.handle_line(1500, line.as_bytes()).unwrap();
            let (rid, body) = keyer_core::frame::decode(reply.as_bytes()).unwrap();
            assert_eq!(rid, id);
            assert!(answers(cmd, body), "{body}");
            parse_reply(cmd, body).unwrap()
        };
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
        assert_eq!(s.budget, Duration::from_secs(60));
        assert_eq!(ask(4, &cw), Reply::Err("RUN".into()));
        assert_eq!(ask(5, &Command::TestStuck), Reply::Err("ARM".into()));
        assert_eq!(ask(5, &Command::TestArm), Reply::Ok(Vec::new()));
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
                trip: Trip::None,
                rest_left: Duration::from_secs(1),
                budget: Duration::from_secs(60),
                ptt: false,
                line: false,
            }
        );
        // `MCW`, the line read high first, after the rest.
        k.set_line(true);
        let mut ask = |id: u8, cmd: &Command| {
            let line = encode(id, cmd).unwrap();
            let reply = k.handle_line(2600, line.as_bytes()).unwrap();
            let (_, body) = keyer_core::frame::decode(reply.as_bytes()).unwrap();
            assert!(answers(cmd, body), "{body}");
            parse_reply(cmd, body).unwrap()
        };
        let mcw = Command::Mcw {
            wpm: 20,
            text: "TEST".into(),
        };
        assert_eq!(ask(9, &mcw), Reply::Ok(Vec::new()));
        assert_eq!(ask(10, &Command::TestHold), Reply::Err("ARM".into()));
        assert_eq!(ask(10, &Command::TestArm), Reply::Ok(Vec::new()));
        assert_eq!(ask(10, &Command::TestHold), Reply::Ok(Vec::new()));
        let Reply::Ok(f) = ask(11, &Command::Status) else {
            panic!()
        };
        let s = Status::parse(&f).unwrap();
        assert!(s.ptt && s.run && !s.key && s.busy());
        assert_eq!(ask(12, &Command::Stop), Reply::Ok(Vec::new()));
        assert_eq!(ask(13, &mcw), Reply::Err("LINE".into()));
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
        assert!(Status::parse(&strings(&["1", "0", "DONE", "NONE"])).is_err());
        // Version 2's STATUS: no PTT and no line.
        assert!(Status::parse(&strings(&["0", "0", "DONE", "NONE", "0", "0"])).is_err());
        let status = ["0", "0", "DONE", "LINE", "0", "70", "0", "0"];
        assert!(Status::parse(&strings(&status)).is_ok());
        for (i, bad) in [(0, "2"), (2, "MAYBE"), (4, "-1"), (6, "2"), (7, "x")] {
            let mut st = status;
            st[i] = bad;
            assert!(Status::parse(&strings(&st)).is_err(), "{st:?}");
        }
        let hello = [
            "3", "60", "2000", "1000", "1000", "60", "60", "5", "POWER", "-", "X",
        ];
        assert!(Hello::parse(&strings(&hello)).is_ok());
        let mut h = hello;
        h[8] = "LUNCH";
        assert!(Hello::parse(&strings(&h)).is_err());
        let mut h = hello;
        h[2] = "2s";
        assert!(Hello::parse(&strings(&h)).is_err());
        // Version 1's and 2's HELLO.
        assert!(Hello::parse(&strings(&["1", "60", "2000", "1000", "5", "POWER", "X"])).is_err());
        assert!(Hello::parse(&strings(&[
            "2", "60", "2000", "1000", "1000", "60", "5", "POWER", "-", "X"
        ]))
        .is_err());
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
