//! A handheld running the CW firmware, simulated behind a [`Transport`]: it keeps
//! the firmware's side of docs/handheld-protocol.md, including its own limits, and
//! can be made to misbehave. For tests; nothing here touches a radio.

use super::link::Transport;
use super::proto::{self, Hello};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Why keying ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// The whole text went out.
    Done,
    Stop,
    /// No valid line for the link timeout.
    Link,
    /// The firmware's own transmit limit.
    Limit,
}

/// One keying run, as the firmware made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub wpm: u32,
    pub on: Instant,
    /// `None` while still keyed.
    pub off: Option<(Instant, Off)>,
}

struct Key {
    run: usize,
    /// When the text will have gone out at the firmware's speed; `None` for PTT
    /// held by hand, which only `STOP` or the transmit limit ends here.
    end: Option<Instant>,
}

struct Fw {
    hello: Hello,
    time_scale: f32,
    rx_hz: u64,
    tx_hz: u64,
    /// Added to the transmit frequency whenever the frequency is set.
    tx_offset: i64,
    mode_cw: bool,
    power: String,
    key: Option<Key>,
    runs: Vec<Run>,
    last_valid: Instant,
    /// Squelch open until then.
    busy_until: Option<Instant>,
    /// Every valid line received, as (when, body).
    received: Vec<(Instant, String)>,
    out: VecDeque<String>,
    deaf: bool,
    silent: bool,
    lose_cw_reply: bool,
    ignore_stop: bool,
    endless: bool,
    no_link_watchdog: bool,
    garble: u32,
}

impl Fw {
    /// End keying if it should have ended by `now`, at the moment it should have.
    fn update(&mut self, now: Instant) {
        let Some(k) = &self.key else { return };
        let on = self.runs[k.run].on;
        let mut ends = vec![(on + self.hello.tx_limit, Off::Limit)];
        if let (Some(end), false) = (k.end, self.endless) {
            ends.push((end, Off::Done));
        }
        if k.end.is_some() && !self.no_link_watchdog {
            ends.push((self.last_valid + self.hello.link_timeout, Off::Link));
        }
        let (at, why) = ends.into_iter().min_by_key(|e| e.0).expect("one end");
        if at <= now {
            self.runs[k.run].off = Some((at, why));
            self.key = None;
        }
    }

    fn stop(&mut self, now: Instant, why: Off) {
        if let Some(k) = self.key.take() {
            self.runs[k.run].off = Some((now, why));
        }
    }

    fn quiet_ms(&self, now: Instant) -> u64 {
        match self.busy_until {
            Some(t) if now < t => 0,
            Some(t) => (now - t).as_millis().min(60_000) as u64,
            None => 60_000,
        }
    }

    fn start(&mut self, now: Instant, wpm: u32, text: &str) {
        let dot = Duration::from_secs_f32(1.2 / wpm as f32 / self.time_scale);
        self.runs.push(Run {
            text: text.to_string(),
            wpm,
            on: now,
            off: None,
        });
        self.key = Some(Key {
            run: self.runs.len() - 1,
            end: Some(now + dot * cw::units(text)),
        });
    }

    /// The reply to `body`, received at `now`.
    fn handle(&mut self, now: Instant, body: &str) -> Option<String> {
        let (name, args) = body.split_once(' ').unwrap_or((body, ""));
        let tx = self.key.is_some();
        Some(match (name, args) {
            ("HELLO", "") => format!(
                "OK HELLO {} {} {} {}",
                self.hello.version,
                self.hello.tx_limit.as_secs(),
                self.hello.link_timeout.as_millis(),
                self.hello.name
            ),
            ("STATUS", "") => format!("OK STATUS {} {}", tx as u8, self.quiet_ms(now)),
            ("FREQ", "") => format!("OK FREQ {} {}", self.rx_hz, self.tx_hz),
            ("FREQ", hz) => match hz.parse::<u64>() {
                _ if tx => "ERR FREQ TX".into(),
                Ok(hz) if super::band_of(hz).is_some() => {
                    (self.rx_hz, self.tx_hz) = (hz, hz.saturating_add_signed(self.tx_offset));
                    format!("OK FREQ {} {}", self.rx_hz, self.tx_hz)
                }
                _ => "ERR FREQ RANGE".into(),
            },
            ("MODE", "CW") => {
                self.mode_cw = true;
                "OK MODE CW".into()
            }
            ("MODE", _) => "ERR MODE MODE".into(),
            ("POWER", p @ ("LOW" | "MID" | "HIGH")) => {
                self.power = p.to_string();
                format!("OK POWER {p}")
            }
            ("CW", args) => {
                let (wpm, text) = args.split_once(' ').unwrap_or((args, ""));
                let wpm = wpm.parse::<u32>().unwrap_or(0);
                if tx {
                    "ERR CW TX".into()
                } else if !(5..=50).contains(&wpm) {
                    "ERR CW WPM".into()
                } else if text.is_empty() || text.chars().count() > civ::MAX_CW_CHARS {
                    "ERR CW LEN".into()
                } else if !text
                    .chars()
                    .all(|c| !c.is_ascii_lowercase() && cw::is_sendable(c))
                {
                    "ERR CW CHAR".into()
                } else if !self.mode_cw {
                    "ERR CW MODE".into()
                } else {
                    self.start(now, wpm, text);
                    if self.lose_cw_reply {
                        return None;
                    }
                    "OK CW".into()
                }
            }
            ("STOP", "") => {
                if !self.ignore_stop {
                    self.stop(now, Off::Stop);
                }
                "OK STOP".into()
            }
            (name, _) => format!("ERR {name} UNKNOWN"),
        })
    }
}

/// The simulated firmware. Clones share one radio: give one to the link as its
/// [`Transport`] and keep another to look at and to inject faults.
#[derive(Clone)]
pub struct MockFirmware {
    fw: Arc<Mutex<Fw>>,
}

impl MockFirmware {
    /// On 144.060 MHz, receive, with a 60 s transmit limit and a 2 s link timeout.
    /// Morse goes `time_scale` times faster than at its speed.
    pub fn new(time_scale: f32) -> Self {
        Self {
            fw: Arc::new(Mutex::new(Fw {
                hello: Hello {
                    version: proto::VERSION,
                    tx_limit: Duration::from_secs(60),
                    link_timeout: Duration::from_secs(2),
                    name: "MOCK-CW".into(),
                },
                time_scale,
                rx_hz: 144_060_000,
                tx_hz: 144_060_000,
                tx_offset: 0,
                mode_cw: false,
                power: "HIGH".into(),
                key: None,
                runs: Vec::new(),
                last_valid: Instant::now(),
                busy_until: None,
                received: Vec::new(),
                out: VecDeque::new(),
                deaf: false,
                silent: false,
                lose_cw_reply: false,
                ignore_stop: false,
                endless: false,
                no_link_watchdog: false,
                garble: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Fw> {
        let mut fw = self.fw.lock().unwrap_or_else(|e| e.into_inner());
        fw.update(Instant::now());
        fw
    }

    /// Change what `HELLO` reports, and the limits the firmware keeps.
    pub fn set_hello(&self, version: u32, tx_limit: Duration, link_timeout: Duration) {
        let mut fw = self.lock();
        fw.hello.version = version;
        fw.hello.tx_limit = tx_limit;
        fw.hello.link_timeout = link_timeout;
    }

    pub fn transmitting(&self) -> bool {
        self.lock().key.is_some()
    }

    /// Every keying run so far.
    pub fn runs(&self) -> Vec<Run> {
        self.lock().runs.clone()
    }

    /// Every valid line received, as (when, body).
    pub fn received(&self) -> Vec<(Instant, String)> {
        self.lock().received.clone()
    }

    pub fn frequencies(&self) -> (u64, u64) {
        let fw = self.lock();
        (fw.rx_hz, fw.tx_hz)
    }

    /// Set the transmit frequency apart from receive, as a repeater offset would.
    pub fn set_tx_hz(&self, hz: u64) {
        self.lock().tx_hz = hz;
    }

    pub fn power(&self) -> String {
        self.lock().power.clone()
    }

    pub fn mode_cw(&self) -> bool {
        self.lock().mode_cw
    }

    /// Someone on the frequency for `d` from now.
    pub fn set_busy_for(&self, d: Duration) {
        self.lock().busy_until = Some(Instant::now() + d);
    }

    /// Keep transmit `offset` Hz from receive, as a repeater offset would, even
    /// when the frequency is set.
    pub fn set_tx_offset(&self, offset: i64) {
        self.lock().tx_offset = offset;
    }

    /// Keyed without any command, until `STOP`: the radio's PTT held by hand.
    pub fn key_by_hand(&self) {
        let mut fw = self.lock();
        fw.start(Instant::now(), 20, "PTT");
        if let Some(k) = &mut fw.key {
            k.end = None;
        }
    }

    /// Receive nothing at all, as if the cable were pulled.
    pub fn set_deaf(&self, on: bool) {
        self.lock().deaf = on;
    }

    /// Answer nothing (commands are still carried out).
    pub fn set_silent(&self, on: bool) {
        self.lock().silent = on;
    }

    /// Key a `CW` command but lose its reply.
    pub fn set_lose_cw_reply(&self, on: bool) {
        self.lock().lose_cw_reply = on;
    }

    /// Answer `STOP` with OK but go on keying.
    pub fn set_ignore_stop(&self, on: bool) {
        self.lock().ignore_stop = on;
    }

    /// Never finish the text by itself.
    pub fn set_endless(&self, on: bool) {
        self.lock().endless = on;
    }

    pub fn set_no_link_watchdog(&self, on: bool) {
        self.lock().no_link_watchdog = on;
    }

    /// Damage the next `n` replies.
    pub fn garble_replies(&self, n: u32) {
        self.lock().garble = n;
    }

    /// Wait until keying has ended, up to `within`.
    pub fn wait_receive(&self, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if !self.transmitting() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        !self.transmitting()
    }
}

impl Transport for MockFirmware {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut fw = self.lock();
        let now = Instant::now();
        if fw.deaf {
            return Ok(());
        }
        let Ok((id, body)) = proto::decode(line) else {
            return Ok(());
        };
        fw.last_valid = now;
        fw.received.push((now, body.to_string()));
        if let Some(reply) = fw.handle(now, body) {
            if fw.silent {
                return Ok(());
            }
            let mut out = proto::encode(id, &reply);
            if fw.garble > 0 {
                fw.garble -= 1;
                out = out.replacen(' ', "  ", 1);
            }
            fw.out.push_back(out);
        }
        Ok(())
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>> {
        loop {
            if let Some(l) = self.lock().out.pop_front() {
                return Ok(Some(l));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(1)));
        }
    }

    fn clear_input(&mut self) -> io::Result<()> {
        self.lock().out.clear();
        Ok(())
    }

    fn describe(&self) -> String {
        "mock firmware".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(fw: &mut MockFirmware, id: u8, body: &str) -> Option<String> {
        fw.write_line(&proto::encode(id, body)).unwrap();
        fw.read_line(Instant::now())
            .unwrap()
            .map(|l| proto::decode(&l).unwrap().1.to_string())
    }

    #[test]
    fn keys_the_text_and_ends_by_itself() {
        let mut fw = MockFirmware::new(50.0);
        assert_eq!(send(&mut fw, 1, "MODE CW").as_deref(), Some("OK MODE CW"));
        assert_eq!(send(&mut fw, 2, "CW 20 TEST").as_deref(), Some("OK CW"));
        assert!(fw.transmitting());
        assert_eq!(send(&mut fw, 3, "CW 20 TEST").as_deref(), Some("ERR CW TX"));
        assert!(fw.wait_receive(Duration::from_secs(1)));
        assert_eq!(fw.runs()[0].off.unwrap().1, Off::Done);
    }

    #[test]
    fn its_link_watchdog_and_limit_stop_keying() {
        let mut fw = MockFirmware::new(1.0);
        fw.set_hello(1, Duration::from_secs(60), Duration::from_millis(50));
        // (Shorter than a node accepts, to keep the test quick.)
        send(&mut fw, 1, "MODE CW");
        send(&mut fw, 2, "CW 5 PARIS PARIS");
        assert!(fw.wait_receive(Duration::from_millis(500)));
        assert_eq!(fw.runs()[0].off.unwrap().1, Off::Link);

        fw.set_no_link_watchdog(true);
        fw.set_hello(1, Duration::from_millis(100), Duration::from_millis(50));
        send(&mut fw, 3, "CW 5 PARIS PARIS");
        assert!(fw.wait_receive(Duration::from_millis(500)));
        assert_eq!(fw.runs()[1].off.unwrap().1, Off::Limit);
    }

    #[test]
    fn refuses_what_the_protocol_does_not_allow() {
        let mut fw = MockFirmware::new(1.0);
        assert_eq!(
            send(&mut fw, 1, "CW 20 TEST").as_deref(),
            Some("ERR CW MODE")
        );
        send(&mut fw, 2, "MODE CW");
        assert_eq!(
            send(&mut fw, 3, "CW 20 test").as_deref(),
            Some("ERR CW CHAR")
        );
        assert_eq!(send(&mut fw, 4, "CW 2 TEST").as_deref(), Some("ERR CW WPM"));
        let long = format!("CW 20 {}", "E".repeat(31));
        assert_eq!(send(&mut fw, 5, &long).as_deref(), Some("ERR CW LEN"));
        assert_eq!(
            send(&mut fw, 6, "FREQ 146000").as_deref(),
            Some("ERR FREQ RANGE")
        );
        assert!(fw.runs().is_empty());
        // A damaged line is ignored entirely.
        fw.write_line("07 CW 20 TEST*00").unwrap();
        assert!(fw.read_line(Instant::now()).unwrap().is_none());
        assert!(fw.runs().is_empty());
    }
}
