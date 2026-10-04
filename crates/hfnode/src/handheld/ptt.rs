//! The push-to-talk line of a handheld's sound-card cable.
//!
//! A cable such as the AIOC (All-In-One-Cable) or the Digirig Mobile keys the
//! handheld through a control line of its USB serial port, DTR or RTS. That suits a
//! node whose first worry is a stuck transmitter: the system drops both lines when
//! the port is closed, which includes the process ending in any way (a crash, a
//! kill) and the cable being unplugged, so a dead `hfnode` cannot leave the radio
//! keyed. A sound-card GPIO keyed over USB HID (the CM108 way) has no such release
//! and is not offered.
//!
//! What each cable treats as "keyed" ([`PttLine`]):
//!
//! - **AIOC** keys while DTR is up and RTS is down. Opening a port on Linux and
//!   macOS raises both lines, which does not key it, and [`SerialPtt`] always drops
//!   DTR before RTS, so it never passes through the keyed combination on the way
//!   down. (The AIOC's default PTT source as its documentation describes it; not
//!   measured here.)
//! - **RTS** keyed (the Digirig Mobile): opening the port on Linux and macOS raises
//!   RTS, which keys the radio for the moment until [`SerialPtt::open`] can drop it,
//!   a few milliseconds once at start-up. Windows opens the port with both lines
//!   off.
//! - **DTR** keyed: likewise with DTR.

use std::fmt;
use std::io;

/// Keys and releases the transmitter.
pub trait Ptt: Send {
    /// Key (`true`) or release (`false`). An error means the line may be in either
    /// state.
    fn set(&mut self, keyed: bool) -> io::Result<()>;
    /// What keys the radio, for the log.
    fn describe(&self) -> String;
}

/// Which control-line state keys the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PttLine {
    /// DTR up with RTS down keys; any other combination is released.
    Aioc,
    /// RTS up keys.
    Rts,
    /// DTR up keys.
    Dtr,
}

impl fmt::Display for PttLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Aioc => "DTR up with RTS down (AIOC)",
            Self::Rts => "RTS up",
            Self::Dtr => "DTR up",
        })
    }
}

/// The two control lines of a serial port.
pub trait Lines: Send {
    fn dtr(&mut self, up: bool) -> io::Result<()>;
    fn rts(&mut self, up: bool) -> io::Result<()>;
}

impl Lines for Box<dyn serialport::SerialPort> {
    fn dtr(&mut self, up: bool) -> io::Result<()> {
        self.write_data_terminal_ready(up).map_err(io::Error::other)
    }
    fn rts(&mut self, up: bool) -> io::Result<()> {
        self.write_request_to_send(up).map_err(io::Error::other)
    }
}

/// PTT on a serial port's DTR or RTS line. Created released; dropping it releases
/// the line, and closing the port (which the system also does when the process
/// ends) drops both lines.
pub struct SerialPtt<L: Lines> {
    lines: L,
    line: PttLine,
    name: String,
}

impl SerialPtt<Box<dyn serialport::SerialPort>> {
    /// Open `path` for PTT only (nothing is ever written to it) and release the
    /// line. Fails, with the port closed again, if the line cannot be released.
    pub fn open(path: &str, line: PttLine) -> anyhow::Result<Self> {
        use anyhow::Context;
        // The rate is irrelevant for the lines, but a port opened at 0 baud is a
        // hang-up on Unix: the system would drop both lines and ignore them.
        let builder = serialport::new(path, 9600)
            .flow_control(serialport::FlowControl::None)
            // DTR first: from both lines up (Linux and macOS at open), dropping DTR
            // first never passes through the AIOC's keyed state.
            .dtr_on_open(false)
            .timeout(std::time::Duration::from_millis(50));
        // Windows opens a COM port for one handle only (share mode 0) by itself.
        #[cfg(unix)]
        let builder = builder.exclusive(true);
        let port = builder
            .open()
            .with_context(|| format!("opening the PTT serial port {path}"))?;
        Self::new(port, line, path).with_context(|| format!("releasing PTT on {path}"))
    }
}

impl<L: Lines> SerialPtt<L> {
    /// Take over `lines` and release PTT.
    pub fn new(lines: L, line: PttLine, name: &str) -> io::Result<Self> {
        let mut p = Self {
            lines,
            line,
            name: name.to_string(),
        };
        p.release()?;
        Ok(p)
    }

    /// Both lines down. For the AIOC, DTR before RTS: with RTS still up the
    /// intermediate state (DTR down, RTS up) is released too.
    fn release(&mut self) -> io::Result<()> {
        match self.line {
            PttLine::Aioc | PttLine::Dtr => {
                self.lines.dtr(false)?;
                self.lines.rts(false)
            }
            PttLine::Rts => {
                self.lines.rts(false)?;
                self.lines.dtr(false)
            }
        }
    }

    fn key(&mut self) -> io::Result<()> {
        match self.line {
            // RTS is already down after a release; set it again in case something
            // else raised it, before DTR goes up.
            PttLine::Aioc => {
                self.lines.rts(false)?;
                self.lines.dtr(true)
            }
            PttLine::Rts => self.lines.rts(true),
            PttLine::Dtr => self.lines.dtr(true),
        }
    }
}

impl<L: Lines> Ptt for SerialPtt<L> {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        if keyed {
            self.key()
        } else {
            self.release()
        }
    }

    fn describe(&self) -> String {
        format!("{} on {}", self.line, self.name)
    }
}

impl<L: Lines> Drop for SerialPtt<L> {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            log::error!("releasing PTT on {}: {e}", self.name);
        }
    }
}

/// PTT for tests: records every change, and can be made to fail.
#[derive(Clone, Default)]
pub struct MockPtt {
    state: std::sync::Arc<std::sync::Mutex<MockPttState>>,
}

#[derive(Default)]
struct MockPttState {
    /// Every successful change, with when it was made.
    events: Vec<(std::time::Instant, bool)>,
    keyed: bool,
    /// Releases still to fail.
    fail_releases: u32,
    fail_key: bool,
}

impl MockPtt {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockPttState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every change made, in order, as (when, keyed).
    pub fn events(&self) -> Vec<(std::time::Instant, bool)> {
        self.lock().events.clone()
    }

    /// Whether the line is keyed now.
    pub fn keyed(&self) -> bool {
        self.lock().keyed
    }

    /// Fail the next `n` releases (the line stays as it was).
    pub fn fail_releases(&self, n: u32) {
        self.lock().fail_releases = n;
    }

    /// Fail every attempt to key.
    pub fn fail_key(&self, fail: bool) {
        self.lock().fail_key = fail;
    }
}

impl Ptt for MockPtt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        let mut s = self.lock();
        if !keyed && s.fail_releases > 0 {
            s.fail_releases -= 1;
            return Err(io::Error::other("mock: release failed"));
        }
        if keyed && s.fail_key {
            return Err(io::Error::other("mock: key failed"));
        }
        if s.keyed != keyed || s.events.is_empty() {
            s.events.push((std::time::Instant::now(), keyed));
        }
        s.keyed = keyed;
        Ok(())
    }

    fn describe(&self) -> String {
        "mock PTT".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records the line states after each change.
    #[derive(Clone)]
    struct Recorder {
        state: Arc<Mutex<Vec<(bool, bool)>>>,
        now: (bool, bool),
        fail_dtr: bool,
    }

    impl Recorder {
        /// Starting with both lines up, as Linux and macOS leave them at open.
        fn opened() -> Self {
            Self {
                state: Arc::new(Mutex::new(vec![(true, true)])),
                now: (true, true),
                fail_dtr: false,
            }
        }
        fn states(&self) -> Vec<(bool, bool)> {
            self.state.lock().unwrap().clone()
        }
    }

    impl Lines for Recorder {
        fn dtr(&mut self, up: bool) -> io::Result<()> {
            if self.fail_dtr {
                return Err(io::Error::other("device gone"));
            }
            self.now.0 = up;
            self.state.lock().unwrap().push(self.now);
            Ok(())
        }
        fn rts(&mut self, up: bool) -> io::Result<()> {
            self.now.1 = up;
            self.state.lock().unwrap().push(self.now);
            Ok(())
        }
    }

    fn keyed(line: PttLine, (dtr, rts): (bool, bool)) -> bool {
        match line {
            PttLine::Aioc => dtr && !rts,
            PttLine::Rts => rts,
            PttLine::Dtr => dtr,
        }
    }

    #[test]
    fn an_aioc_is_never_keyed_while_opening_or_releasing() {
        let rec = Recorder::opened();
        let mut p = SerialPtt::new(rec.clone(), PttLine::Aioc, "test").unwrap();
        let opening = rec.states();
        assert!(
            opening.iter().all(|&s| !keyed(PttLine::Aioc, s)),
            "{opening:?}"
        );
        assert_eq!(*opening.last().unwrap(), (false, false));
        p.set(true).unwrap();
        assert!(keyed(PttLine::Aioc, *rec.states().last().unwrap()));
        let before = rec.states().len();
        p.set(false).unwrap();
        let releasing = &rec.states()[before..];
        assert!(
            releasing.iter().all(|&s| !keyed(PttLine::Aioc, s)),
            "{releasing:?}"
        );
    }

    #[test]
    fn each_line_style_keys_and_releases() {
        for line in [PttLine::Aioc, PttLine::Rts, PttLine::Dtr] {
            let rec = Recorder::opened();
            let mut p = SerialPtt::new(rec.clone(), line, "test").unwrap();
            assert!(!keyed(line, *rec.states().last().unwrap()), "{line:?}");
            p.set(true).unwrap();
            assert!(keyed(line, *rec.states().last().unwrap()), "{line:?}");
            p.set(false).unwrap();
            assert_eq!(*rec.states().last().unwrap(), (false, false), "{line:?}");
        }
    }

    #[test]
    fn dropping_releases() {
        let rec = Recorder::opened();
        let mut p = SerialPtt::new(rec.clone(), PttLine::Rts, "test").unwrap();
        p.set(true).unwrap();
        drop(p);
        assert_eq!(*rec.states().last().unwrap(), (false, false));
    }

    #[test]
    fn a_port_whose_line_cannot_be_released_is_refused() {
        let mut rec = Recorder::opened();
        rec.fail_dtr = true;
        assert!(SerialPtt::new(rec, PttLine::Aioc, "test").is_err());
    }
}
