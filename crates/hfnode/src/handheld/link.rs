//! The serial link to the handheld's firmware: lines out, replies matched back.

use super::proto::{self, Command, Reply};
use civ::RigError;
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

/// Moves lines to and from the firmware.
pub trait Transport: Send {
    /// Send `line` (without its newline).
    fn write_line(&mut self, line: &str) -> io::Result<()>;
    /// The next line received before `deadline`, without its newline, or `None`.
    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>>;
    /// Throw away whatever was received and not yet read.
    fn clear_input(&mut self) -> io::Result<()>;
    fn describe(&self) -> String;
}

/// A USB serial port.
///
/// Opened with DTR and RTS down. A cable that also carries the handheld's audio,
/// such as the AIOC, keys the radio's PTT with those lines (the AIOC: DTR up with RTS
/// down), so the node holds them down for as long as the port is open, and the
/// system drops them when it closes. Linux and macOS raise both lines when a port
/// is opened, which the AIOC does not take as keyed; dropping DTR first never
/// passes through its keyed state. (The AIOC's PTT lines as its documentation
/// describes them, from memory; not measured here.)
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
    name: String,
}

impl SerialTransport {
    pub fn open(path: &str, baud: u32) -> anyhow::Result<Self> {
        use anyhow::Context;
        let builder = serialport::new(path, baud)
            .flow_control(serialport::FlowControl::None)
            .dtr_on_open(false)
            .timeout(Duration::from_millis(20));
        #[cfg(unix)]
        let builder = builder.exclusive(true);
        let mut port = builder
            .open()
            .with_context(|| format!("opening the handheld's serial port {path}"))?;
        port.write_data_terminal_ready(false)
            .and_then(|()| port.write_request_to_send(false))
            .with_context(|| format!("dropping DTR and RTS on {path}"))?;
        Ok(Self {
            port,
            buf: Vec::new(),
            name: path.to_string(),
        })
    }
}

impl Transport for SerialTransport {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.port.write_all(format!("{line}\n").as_bytes())?;
        self.port.flush()
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>> {
        let mut chunk = [0u8; 128];
        loop {
            if let Some(at) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=at).collect();
                return Ok(Some(String::from_utf8_lossy(&line[..at]).into_owned()));
            }
            if self.buf.len() > proto::MAX_LINE + 2 {
                // No newline where there should have been one: noise.
                self.buf.clear();
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.port.read(&mut chunk) {
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn clear_input(&mut self) -> io::Result<()> {
        self.buf.clear();
        self.port
            .clear(serialport::ClearBuffer::Input)
            .map_err(io::Error::other)
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
}

impl Link {
    /// `timeout`: how long to wait for each reply.
    pub fn new(t: Box<dyn Transport>, timeout: Duration) -> Self {
        Self {
            t,
            next_id: 1,
            timeout,
        }
    }

    pub fn describe(&self) -> String {
        self.t.describe()
    }

    /// Send `cmd` and return the fields of its `OK` reply. An `ERR` reply is an
    /// error naming its code. A command that may be repeated is sent up to three
    /// times while its reply is lost.
    pub fn request(&mut self, cmd: &Command) -> civ::Result<Vec<String>> {
        let tries = if cmd.repeatable() { 3 } else { 1 };
        let mut last = RigError::Timeout;
        for attempt in 0..tries {
            if attempt > 0 {
                log::warn!("handheld: no reply to {}, sending it again", cmd.name());
                thread::sleep(Duration::from_millis(20));
            }
            match self.once(cmd) {
                Ok(Reply::Ok(fields)) => return Ok(fields),
                Ok(Reply::Err(code)) => {
                    return Err(RigError::Protocol(format!(
                        "the handheld refused {}: {code}",
                        cmd.body()
                    )))
                }
                Err(e @ RigError::Timeout) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn once(&mut self, cmd: &Command) -> civ::Result<Reply> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.t.clear_input().map_err(RigError::Io)?;
        self.t
            .write_line(&proto::encode(id, &cmd.body()))
            .map_err(RigError::Io)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let Some(line) = self.t.read_line(deadline).map_err(RigError::Io)? else {
                return Err(RigError::Timeout);
            };
            match proto::decode(&line) {
                Ok((got, body)) if got == id => {
                    return proto::parse_reply(cmd, body).map_err(RigError::Protocol)
                }
                // A late reply to an earlier command.
                Ok(_) => log::debug!("handheld: ignoring {line:?}"),
                Err(e) => log::debug!("handheld: ignoring {line:?} ({e})"),
            }
        }
    }
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
        /// Lines to give back after each write, built from that write's id.
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
            let (id, _) = proto::decode(line).unwrap();
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

    fn reply(body: &'static str) -> MakeReply {
        Box::new(move |id| proto::encode(id, body))
    }

    #[test]
    fn stale_and_damaged_lines_are_skipped_for_the_matching_reply() {
        let s = Script::default();
        s.then(vec![
            Box::new(|id| proto::encode(id.wrapping_sub(1), "OK STATUS 1 0")),
            // Damaged on the way: one character changed, the checksum not.
            Box::new(|id| proto::encode(id, "OK STATUS 1 0").replace("S 1", "S 0")),
            reply("OK STATUS 0 900"),
        ]);
        let mut link = Link::new(Box::new(s.clone()), Duration::from_millis(50));
        assert_eq!(
            link.request(&Command::Status).unwrap(),
            vec!["0".to_string(), "900".to_string()]
        );
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
    }

    #[test]
    fn an_err_reply_is_an_error_with_its_code() {
        let s = Script::default();
        s.then(vec![reply("ERR FREQ RANGE")]);
        let mut link = Link::new(Box::new(s), Duration::from_millis(10));
        let e = link.request(&Command::SetFreq(1)).unwrap_err().to_string();
        assert!(e.contains("RANGE"), "{e}");
    }

    #[test]
    fn ids_never_repeat_back_to_back_and_skip_zero() {
        let s = Script::default();
        let mut link = Link::new(Box::new(s.clone()), Duration::from_millis(1));
        link.next_id = 255;
        let _ = link.request(&Command::Hello);
        let sent = s.sent.lock().unwrap().clone();
        let ids: Vec<u8> = sent.iter().map(|l| proto::decode(l).unwrap().0).collect();
        assert_eq!(ids, vec![255, 1, 2]);
    }
}
