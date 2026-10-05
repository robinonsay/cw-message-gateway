//! A handheld running the CW firmware, simulated behind a [`Transport`]: it keeps
//! the firmware's side of docs/handheld-protocol.md (firmware/uv-k1/app/hfnode.c),
//! including its own limits and its watchdog, and can be made to misbehave. For
//! tests; nothing here touches a radio.

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
    /// The watchdog reset the radio, its main loop having stopped.
    Watchdog,
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
    /// As set at the radio: the firmware only reads them.
    rx_hz: u64,
    tx_hz: u64,
    tx_mode: String,
    rx_mode: String,
    power: String,
    break_in: bool,
    key: Option<Key>,
    runs: Vec<Run>,
    last_valid: Instant,
    /// Squelch open until then.
    busy_until: Option<Instant>,
    /// Every valid line received, as (when, body).
    received: Vec<(Instant, String)>,
    out: VecDeque<String>,
    /// The main loop stopped (`TEST HANG`) until the watchdog resets the radio
    /// then; `None` inside if the watchdog never does.
    hung: Option<Option<Instant>>,
    /// From the main loop stopping to the reset.
    watchdog: Option<Duration>,
    deaf: bool,
    silent: bool,
    lose_cw_reply: bool,
    refuse_tx: bool,
    ignore_stop: bool,
    endless: bool,
    no_link_watchdog: bool,
    garble: u32,
}

impl Fw {
    /// End keying if it should have ended by `now`, at the moment it should have.
    fn update(&mut self, now: Instant) {
        if let Some(hung) = self.hung {
            // Nothing runs but the watchdog: the carrier stays as it was.
            match hung {
                Some(reset) if reset <= now => {
                    if let Some(k) = self.key.take() {
                        self.runs[k.run].off = Some((reset, Off::Watchdog));
                    }
                    self.hung = None;
                    self.out.clear();
                    self.last_valid = reset;
                }
                _ => return,
            }
        }
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
        let (name, args) = match body.split_once(' ') {
            Some((n, a)) => (n, Some(a)),
            None => (body, None),
        };
        let tx = self.key.is_some();
        Some(match (name, args) {
            ("HELLO", None) => format!(
                "OK HELLO {} {} {} {}",
                self.hello.version,
                self.hello.tx_limit.as_secs(),
                self.hello.link_timeout.as_millis(),
                self.hello.name
            ),
            ("STATUS", None) => format!("OK STATUS {} {}", tx as u8, self.quiet_ms(now)),
            ("FREQ", None) => format!("OK FREQ {} {}", self.rx_hz, self.tx_hz),
            ("MODE", None) => format!("OK MODE {} {}", self.tx_mode, self.rx_mode),
            ("POWER", None) => format!("OK POWER {}", self.power),
            ("BREAKIN", None) => format!("OK BREAKIN {}", self.break_in as u8),
            ("CW", Some(args)) => {
                let Some((wpm, text)) = args.split_once(' ') else {
                    return Some("ERR CW LEN".into());
                };
                let wpm = wpm.parse::<u32>().unwrap_or(0);
                if !(5..=50).contains(&wpm) {
                    "ERR CW WPM".into()
                } else if text.is_empty() || text.chars().count() > civ::MAX_CW_CHARS {
                    "ERR CW LEN".into()
                } else if !text
                    .chars()
                    .all(|c| !c.is_ascii_lowercase() && cw::is_sendable(c))
                {
                    "ERR CW CHAR".into()
                } else if self.tx_mode != "CW" {
                    "ERR CW MODE".into()
                } else if !self.break_in {
                    "ERR CW BKIN".into()
                } else if tx {
                    "ERR CW TX".into()
                } else if self.refuse_tx {
                    "ERR CW REFUSED".into()
                } else {
                    self.start(now, wpm, text);
                    if self.lose_cw_reply {
                        return None;
                    }
                    "OK CW".into()
                }
            }
            ("STOP", None) => {
                if !self.ignore_stop {
                    self.stop(now, Off::Stop);
                }
                "OK STOP".into()
            }
            ("TEST", Some("HANG")) => match &self.key {
                Some(k) if k.end.is_some() => {
                    self.hung = Some(self.watchdog.map(|w| now + w));
                    "OK TEST HANG".into()
                }
                _ => "ERR TEST RUN".into(),
            },
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
    /// Set up as the operator leaves it for the node: 144.060 MHz simplex, CW,
    /// power LOW1, break-in on, on receive; a 60 s transmit limit, a 2 s link
    /// timeout, and a watchdog that resets it 3 s after its main loop stops. Morse
    /// goes `time_scale` times faster than at its speed.
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
                tx_mode: "CW".into(),
                rx_mode: "CW".into(),
                power: "LOW1".into(),
                break_in: true,
                key: None,
                runs: Vec::new(),
                last_valid: Instant::now(),
                busy_until: None,
                received: Vec::new(),
                out: VecDeque::new(),
                hung: None,
                watchdog: Some(Duration::from_secs(3)),
                deaf: false,
                silent: false,
                lose_cw_reply: false,
                refuse_tx: false,
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

    /// Set the frequencies at the radio.
    pub fn set_frequencies(&self, rx_hz: u64, tx_hz: u64) {
        let mut fw = self.lock();
        (fw.rx_hz, fw.tx_hz) = (rx_hz, tx_hz);
    }

    /// Set the transmit frequency apart from receive, as a repeater offset would.
    pub fn set_tx_hz(&self, hz: u64) {
        self.lock().tx_hz = hz;
    }

    /// Set the transmit and receive modes at the radio (`CW`, `FM`, ...).
    pub fn set_modes(&self, tx: &str, rx: &str) {
        let mut fw = self.lock();
        (fw.tx_mode, fw.rx_mode) = (tx.into(), rx.into());
    }

    /// Set the power level at the radio (`LOW1`, `MID`, ...).
    pub fn set_power(&self, level: &str) {
        self.lock().power = level.into();
    }

    pub fn set_break_in(&self, on: bool) {
        self.lock().break_in = on;
    }

    /// Answer `CW` with `ERR CW REFUSED`: the radio would not transmit.
    pub fn set_refuse_tx(&self, on: bool) {
        self.lock().refuse_tx = on;
    }

    /// How long from `TEST HANG` to the watchdog's reset; `None`: no reset.
    pub fn set_watchdog(&self, after: Option<Duration>) {
        self.lock().watchdog = after;
    }

    /// Hung, its main loop stopped: until the watchdog resets it.
    pub fn hung(&self) -> bool {
        self.lock().hung.is_some()
    }

    /// Someone on the frequency for `d` from now.
    pub fn set_busy_for(&self, d: Duration) {
        self.lock().busy_until = Some(Instant::now() + d);
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
        if fw.deaf || fw.hung.is_some() {
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
        assert_eq!(send(&mut fw, 1, "MODE").as_deref(), Some("OK MODE CW CW"));
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
    fn a_hang_is_ended_by_the_watchdog_alone() {
        let mut fw = MockFirmware::new(1.0);
        fw.set_watchdog(Some(Duration::from_millis(100)));
        assert_eq!(
            send(&mut fw, 1, "TEST HANG").as_deref(),
            Some("ERR TEST RUN")
        );
        send(&mut fw, 2, "CW 5 PARIS PARIS");
        assert_eq!(
            send(&mut fw, 3, "TEST HANG").as_deref(),
            Some("OK TEST HANG")
        );
        // Deaf and keyed until the reset.
        assert_eq!(send(&mut fw, 4, "STOP"), None);
        assert!(fw.hung() && fw.transmitting());
        assert!(fw.wait_receive(Duration::from_millis(500)));
        assert_eq!(fw.runs()[0].off.unwrap().1, Off::Watchdog);
        assert_eq!(
            send(&mut fw, 5, "STATUS").as_deref(),
            Some("OK STATUS 0 60000")
        );
    }

    #[test]
    fn refuses_what_the_protocol_does_not_allow() {
        let mut fw = MockFirmware::new(1.0);
        fw.set_modes("FM", "FM");
        assert_eq!(
            send(&mut fw, 1, "CW 20 TEST").as_deref(),
            Some("ERR CW MODE")
        );
        fw.set_modes("CW", "CW");
        fw.set_break_in(false);
        assert_eq!(
            send(&mut fw, 2, "CW 20 TEST").as_deref(),
            Some("ERR CW BKIN")
        );
        fw.set_break_in(true);
        assert_eq!(
            send(&mut fw, 3, "CW 20 test").as_deref(),
            Some("ERR CW CHAR")
        );
        assert_eq!(send(&mut fw, 4, "CW 2 TEST").as_deref(), Some("ERR CW WPM"));
        let long = format!("CW 20 {}", "E".repeat(31));
        assert_eq!(send(&mut fw, 5, &long).as_deref(), Some("ERR CW LEN"));
        assert_eq!(
            send(&mut fw, 6, "FREQ 146000").as_deref(),
            Some("ERR FREQ UNKNOWN"),
            "nothing is set from a line"
        );
        fw.set_refuse_tx(true);
        assert_eq!(
            send(&mut fw, 7, "CW 20 TEST").as_deref(),
            Some("ERR CW REFUSED")
        );
        assert!(fw.runs().is_empty());
        // A damaged line is ignored entirely.
        fw.set_refuse_tx(false);
        fw.write_line("08 CW 20 TEST*00").unwrap();
        assert!(fw.read_line(Instant::now()).unwrap().is_none());
        assert!(fw.runs().is_empty());
    }
}
