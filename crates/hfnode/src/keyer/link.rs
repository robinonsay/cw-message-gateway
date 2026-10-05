//! The serial link to the keyer box: lines out, replies matched back.

use super::proto::{self, Command, Reply};
use civ::RigError;
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

/// Moves lines to and from the box.
pub trait Transport: Send {
    /// Send `line` (without its newline).
    fn write_line(&mut self, line: &str) -> io::Result<()>;
    /// The next line received before `deadline`, without its newline, or `None`.
    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>>;
    /// Throw away whatever was received and not yet read.
    fn clear_input(&mut self) -> io::Result<()>;
    fn describe(&self) -> String;
}

/// The box's USB serial port (a virtual COM port: the speed means nothing).
///
/// Opened with DTR and RTS down, and held down for as long as the port is open.
/// The box does nothing with them, but the same port name on another computer, or
/// a typo, could be a radio's own USB port, where either line may be set to key the
/// transmitter (the IC-7300's USB SEND and USB Keying, docs/civ-audit.md).
/// Linux and macOS raise both lines when a port is opened; dropping DTR first never
/// passes through the state where only it is up.
///
/// If the port goes away (the box was unplugged, or reset by its watchdog and came
/// back as a new USB device), the next line written opens it again by name.
pub struct SerialTransport {
    port: Option<Box<dyn serialport::SerialPort>>,
    buf: Vec<u8>,
    name: String,
}

impl SerialTransport {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        Ok(Self {
            port: Some(open_port(path)?),
            buf: Vec::new(),
            name: path.to_string(),
        })
    }

    fn port(&mut self) -> io::Result<&mut Box<dyn serialport::SerialPort>> {
        if self.port.is_none() {
            // Not connected: nothing written can have reached the box.
            let p = open_port(&self.name)
                .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, format!("{e:#}")))?;
            log::info!("keyer box: {} open again", self.name);
            self.buf.clear();
            self.port = Some(p);
        }
        Ok(self.port.as_mut().expect("opened above"))
    }

    /// Note an I/O error: anything but a timeout drops the port, to be opened again.
    fn failed(&mut self, e: io::Error) -> io::Error {
        if e.kind() != io::ErrorKind::TimedOut && self.port.take().is_some() {
            log::warn!("keyer box: {} lost ({e})", self.name);
        }
        e
    }
}

fn open_port(path: &str) -> anyhow::Result<Box<dyn serialport::SerialPort>> {
    use anyhow::Context;
    let builder = serialport::new(path, 115_200)
        .flow_control(serialport::FlowControl::None)
        .dtr_on_open(false)
        .timeout(Duration::from_millis(20));
    #[cfg(unix)]
    let builder = builder.exclusive(true);
    let mut port = builder
        .open()
        .with_context(|| format!("opening the keyer box's serial port {path}"))?;
    port.write_data_terminal_ready(false)
        .and_then(|()| port.write_request_to_send(false))
        .with_context(|| format!("dropping DTR and RTS on {path}"))?;
    Ok(port)
}

impl Transport for SerialTransport {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let r = self.port().and_then(|p| {
            p.write_all(format!("{line}\n").as_bytes())
                .and_then(|()| p.flush())
        });
        r.map_err(|e| self.failed(e))
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>> {
        let mut chunk = [0u8; 128];
        loop {
            if let Some(at) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=at).collect();
                return Ok(Some(String::from_utf8_lossy(&line[..at]).into_owned()));
            }
            if self.buf.len() > keyer_core::MAX_LINE + 2 {
                // No newline where there should have been one: noise.
                self.buf.clear();
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let r = self.port().and_then(|p| p.read(&mut chunk));
            match r {
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(self.failed(e)),
            }
        }
    }

    fn clear_input(&mut self) -> io::Result<()> {
        self.buf.clear();
        let r = self.port().and_then(|p| {
            p.clear(serialport::ClearBuffer::Input)
                .map_err(io::Error::other)
        });
        r.map_err(|e| self.failed(e))
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

/// Commands out and their replies back, one at a time.
pub struct Link {
    t: Box<dyn Transport>,
    next_id: u8,
    timeout: Duration,
    /// When the box last answered anything.
    answered: Option<Instant>,
    /// When a line was last written, or tried: the box's link timeout counts from
    /// the last line it took, which arrived no later than this.
    sent: Option<Instant>,
}

impl Link {
    /// `timeout`: how long to wait for each reply.
    pub fn new(t: Box<dyn Transport>, timeout: Duration) -> Self {
        Self {
            t,
            next_id: 1,
            timeout,
            answered: None,
            sent: None,
        }
    }

    pub fn describe(&self) -> String {
        self.t.describe()
    }

    /// When the box last answered a command.
    pub fn answered(&self) -> Option<Instant> {
        self.answered
    }

    /// When a line last went out to the box (whether or not it arrived).
    pub fn sent(&self) -> Option<Instant> {
        self.sent
    }

    /// Send `cmd` and return the fields of its `OK` reply. An `ERR` reply is an
    /// error naming its code. A command that may be repeated is sent up to three
    /// times while its reply is lost.
    pub fn request(&mut self, cmd: &Command) -> civ::Result<Vec<String>> {
        match self.request_reply(cmd)? {
            Reply::Ok(fields) => Ok(fields),
            Reply::Err(code) => Err(refused(cmd, &code)),
        }
    }

    /// As [`Link::request`], but an `ERR` reply is returned, not made an error.
    pub fn request_reply(&mut self, cmd: &Command) -> civ::Result<Reply> {
        let tries = if cmd.repeatable() { 3 } else { 1 };
        let mut last = RigError::Timeout;
        for attempt in 0..tries {
            if attempt > 0 {
                log::warn!("keyer box: no reply to {}, sending it again", cmd.name());
                thread::sleep(Duration::from_millis(10));
            }
            match self.once(cmd) {
                Ok(r) => return Ok(r),
                Err(e @ (RigError::Timeout | RigError::Io(_))) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn once(&mut self, cmd: &Command) -> civ::Result<Reply> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let line = proto::encode(id, cmd)
            .ok_or_else(|| RigError::Protocol(format!("{:?} does not fit a line", cmd.body())))?;
        self.t.clear_input().map_err(RigError::Io)?;
        let wrote = self.t.write_line(&line);
        // Even a failed write may have reached the box, unless the port was not
        // there to write to.
        if !matches!(&wrote, Err(e) if e.kind() == io::ErrorKind::NotConnected) {
            self.sent = Some(Instant::now());
        }
        wrote.map_err(RigError::Io)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let Some(line) = self.t.read_line(deadline).map_err(RigError::Io)? else {
                return Err(RigError::Timeout);
            };
            match keyer_core::frame::decode(line.as_bytes()) {
                // A reply under this id to another command was left from before this
                // link was opened (ids start again at 01): stale too.
                Ok((got, body)) if got == id && proto::answers(cmd, body) => {
                    self.answered = Some(Instant::now());
                    return proto::parse_reply(cmd, body).map_err(RigError::Protocol);
                }
                // A late reply to an earlier command.
                Ok(_) => log::debug!("keyer box: ignoring {line:?}"),
                Err(e) => log::debug!("keyer box: ignoring {line:?} ({e:?})"),
            }
        }
    }
}

/// The error for the box's `ERR <command> <code>` reply to `cmd`.
pub fn refused(cmd: &Command, code: &str) -> RigError {
    let why = proto::explain(cmd, code)
        .map(|w| format!(" ({w})"))
        .unwrap_or_default();
    RigError::Protocol(format!("the keyer box refused {}: {code}{why}", cmd.body()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Builds a reply line from the id of the line it answers.
    type MakeReply = Box<dyn Fn(u8) -> String + Send>;

    /// Answers each line with whatever the test queued for it.
    #[derive(Clone, Default)]
    struct Script {
        sent: Arc<Mutex<Vec<String>>>,
        replies: Arc<Mutex<VecDeque<Vec<MakeReply>>>>,
        pending: Arc<Mutex<VecDeque<String>>>,
    }

    impl Script {
        fn then(&self, lines: Vec<MakeReply>) {
            self.replies.lock().unwrap().push_back(lines);
        }
    }

    impl Transport for Script {
        fn write_line(&mut self, line: &str) -> io::Result<()> {
            self.sent.lock().unwrap().push(line.to_string());
            let (id, _) = keyer_core::frame::decode(line.as_bytes()).unwrap();
            if let Some(lines) = self.replies.lock().unwrap().pop_front() {
                let mut p = self.pending.lock().unwrap();
                p.extend(lines.iter().map(|f| f(id)));
            }
            Ok(())
        }
        fn read_line(&mut self, _: Instant) -> io::Result<Option<String>> {
            Ok(self.pending.lock().unwrap().pop_front())
        }
        fn clear_input(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn describe(&self) -> String {
            "script".into()
        }
    }

    fn line(id: u8, body: &str) -> String {
        keyer_core::frame::encode(id, format_args!("{body}"))
            .unwrap()
            .as_str()
            .to_string()
    }

    fn reply(body: &'static str) -> MakeReply {
        Box::new(move |id| line(id, body))
    }

    #[test]
    fn stale_and_damaged_lines_are_skipped_for_the_matching_reply() {
        let s = Script::default();
        s.then(vec![
            Box::new(|id| line(id.wrapping_sub(1), "OK STATUS 1 1 NONE NONE")),
            // Damaged on the way: one character changed, the checksum not.
            Box::new(|id| line(id, "OK STATUS 1 1 NONE NONE").replace("S 1", "S 0")),
            // Its id, to another command: left from before this link was opened.
            reply("OK STOP"),
            reply("OK STATUS 0 0 DONE NONE"),
        ]);
        let mut link = Link::new(Box::new(s.clone()), Duration::from_millis(50));
        assert!(link.answered().is_none());
        assert_eq!(
            link.request(&Command::Status).unwrap(),
            ["0", "0", "DONE", "NONE"]
        );
        assert!(link.answered().is_some());
    }

    #[test]
    fn a_lost_reply_is_asked_again_except_for_cw() {
        let s = Script::default();
        s.then(vec![]);
        s.then(vec![reply("OK STOP")]);
        let mut link = Link::new(Box::new(s.clone()), Duration::from_millis(10));
        assert!(link.request(&Command::Stop).is_ok());
        assert_eq!(s.sent.lock().unwrap().len(), 2);

        s.then(vec![]);
        s.then(vec![reply("OK CW")]);
        let cw = Command::Cw {
            wpm: 20,
            text: "TEST".into(),
        };
        assert!(matches!(link.request(&cw), Err(RigError::Timeout)));
        assert_eq!(s.sent.lock().unwrap().len(), 3, "CW sent once only");
        s.then(vec![]);
        assert!(matches!(
            link.request(&Command::TestHang),
            Err(RigError::Timeout)
        ));
        assert_eq!(s.sent.lock().unwrap().len(), 4, "TEST sent once only");
    }

    #[test]
    fn an_err_reply_is_an_error_with_its_code_explained() {
        let s = Script::default();
        s.then(vec![reply("ERR CW TRIP")]);
        s.then(vec![reply("ERR STOP WHAT")]);
        let mut link = Link::new(Box::new(s), Duration::from_millis(10));
        let cw = Command::Cw {
            wpm: 20,
            text: "TEST".into(),
        };
        let e = link.request(&cw).unwrap_err().to_string();
        assert!(e.contains("TRIP (the box tripped"), "{e}");
        let e = link.request(&Command::Stop).unwrap_err().to_string();
        assert!(e.ends_with("refused STOP: WHAT"), "{e}");
    }

    #[test]
    fn ids_never_repeat_back_to_back_and_skip_zero() {
        let s = Script::default();
        let mut link = Link::new(Box::new(s.clone()), Duration::from_millis(1));
        link.next_id = 255;
        let _ = link.request(&Command::Hello);
        let sent = s.sent.lock().unwrap().clone();
        let ids: Vec<u8> = sent
            .iter()
            .map(|l| keyer_core::frame::decode(l.as_bytes()).unwrap().0)
            .collect();
        assert_eq!(ids, vec![255, 1, 2]);
    }
}
