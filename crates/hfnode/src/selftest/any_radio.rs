//! Self-test scenarios with any radio on the keyer box: the same operator and node
//! as with the mock IC-7300, the node driving a [`KeyerRig`] through the mock box
//! ([`crate::keyer::mock`], which runs the firmware's own keyer code) and the radio
//! it keys, whose headphone audio (the operator's signal and band noise on
//! receive, its sidetone on transmit) goes to the node's decoder and its sidetone
//! monitor together, as from a sound card.
//!
//! ```text
//!  operator ──CW audio──► radio ──headphone audio──► node::run ──CW lines──► box
//!     ▲                     ▲                                                 │
//!     │                     └──────────────── key line ◄──────────────────────┘
//!     └── what the box keyed, if the radio was connected ◄──┘
//! ```
//!
//! The radio's audio runs in real time on the box's clock, never held back for
//! the node, so these scenarios run at most [`MAX_SCALE`] times real time: the
//! sidetone monitor times the audio against the wall clock, as on the air, and
//! the box's link timeout (2 s) leaves a busy machine 0.4 s of real time to send
//! its keep-alive (a CI runner has held a test's thread up for longer than 0.2 s).
//!
//! **Pauses.** A busy machine (a CI runner) can stop the whole test process for a
//! moment. The box's clock and the node's both run on meanwhile, as they would if
//! the node's host stopped on the air, so a pause of half a second at 5x plays as
//! the host stopping for 2.5 s: the box's link timeout ends the run, as it should,
//! and the scenario fails through no fault of the node. Each run measures the
//! longest pause (its `machine` check); a run with a pause of [`PAUSE_LIMIT`] of
//! radio time or more is not judged, and [`super::run`] runs it again.

use super::*;
use crate::keyer::bench::{monitor_settings, rig_settings};
use crate::keyer::mock::{Clock, MockBox, MockRadio as Radio, RadioSettings, Run};
use crate::keyer::monitor::Monitor;
use crate::keyer::rig::KeyerRig;
use keyer_core::keyer::{Ended, Trip};
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// Real time the radio waits for the operator's next audio when it falls behind.
const FIELD_WAIT: Duration = Duration::from_millis(250);

/// The fastest the keyer scenarios run, whatever scale is asked for.
pub const MAX_SCALE: f32 = 5.0;

/// The longest pause of the whole test process, in radio time, that a keyer
/// scenario is judged through. Under the least slack its timing leaves: the box's
/// link timeout (2 s) against the node's keep-alive every 0.25 s, and the operator
/// answering 2 s after the radio goes quiet against the node coming back to
/// receive about 1 s after its last key-up.
pub const PAUSE_LIMIT: Duration = Duration::from_secs(1);

/// How often the pause meter looks at the clock (real time).
const PAUSE_TICK: Duration = Duration::from_millis(5);

/// Measures how long the whole process stops running: a thread that looks at the
/// clock every [`PAUSE_TICK`] and keeps the gaps past that. Nothing the node does
/// holds it up; only the machine can. The mock IC-7300's scenarios use it too.
pub(super) struct PauseMeter {
    stop: Arc<AtomicBool>,
    /// Every pause of [`PAUSE_TICK`] or more: how long (real time), and the radio
    /// time it ended.
    thread: Option<JoinHandle<Vec<(Duration, f64)>>>,
}

impl PauseMeter {
    pub(super) fn start(clock: Clock) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut pauses = Vec::new();
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(PAUSE_TICK);
                    let now = Instant::now();
                    let gap = (now - last).saturating_sub(PAUSE_TICK);
                    if gap >= PAUSE_TICK {
                        pauses.push((gap, clock.secs()));
                    }
                    last = now;
                }
                pauses
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stop measuring: every pause of [`PAUSE_TICK`] or more, as (real time, the
    /// radio time it ended).
    pub(super) fn pauses(mut self) -> Vec<(Duration, f64)> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }

    /// The `machine` check: whether the process ran without a pause too long for
    /// the scenario's timing at `scale`.
    fn check(self, scale: f32) -> Check {
        let (pause, at) = longest(&self.pauses());
        machine_check(pause, at, scale)
    }
}

/// The longest of `pauses`, and the radio time it ended; none is zero.
pub(super) fn longest(pauses: &[(Duration, f64)]) -> (Duration, f64) {
    pauses
        .iter()
        .copied()
        .max_by_key(|p| p.0)
        .unwrap_or((Duration::ZERO, 0.0))
}

/// The `machine` check for a run whose longest pause was `pause` (real time),
/// ending at radio time `at`.
pub(super) fn machine_check(pause: Duration, at: f64, scale: f32) -> Check {
    machine_check_within(pause, at, scale, PAUSE_LIMIT.div_f32(scale))
}

/// The `machine` check for a run whose longest pause was `pause` (real time),
/// ending at radio time `at`, against `limit` (real time).
pub(super) fn machine_check_within(pause: Duration, at: f64, scale: f32, limit: Duration) -> Check {
    let detail = format!(
        "longest pause of the test process {:.2} s ({:.1} s radio time, at {at:.0} s); \
         limit {:.2} s",
        pause.as_secs_f32(),
        pause.mul_f32(scale).as_secs_f32(),
        limit.as_secs_f32()
    );
    if pause < limit {
        check("machine", true, detail)
    } else {
        check(
            "machine",
            false,
            format!(
                "{detail}: the machine stopped the test for longer than the scenario's \
                 timing allows at {scale}x, so the run says nothing about the node"
            ),
        )
    }
}

impl Drop for PauseMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fault at the keyer box or the radio it keys, from a [`Step::Keyer`] on.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyerFault {
    /// The cable from the box to the radio's key jack comes out (`true`) or goes
    /// back in: the box keys and the radio does not.
    CableOut(bool),
    /// The radio's key sticks down from the start of the node's next transmission
    /// (a shorted cable, a welded relay), until a timer at the radio lets go
    /// `secs` later.
    KeyStuck { secs: f64 },
    /// The box's USB cable is pulled (`true`) or plugged in again.
    Unplugged(bool),
}

impl fmt::Display for KeyerFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CableOut(true) => f.write_str("key cable out of the radio"),
            Self::CableOut(false) => f.write_str("key cable back in"),
            Self::KeyStuck { secs } => write!(
                f,
                "the radio's key will stick down at the next transmission, for {secs:.0} s"
            ),
            Self::Unplugged(true) => f.write_str("keyer box unplugged"),
            Self::Unplugged(false) => f.write_str("keyer box plugged in again"),
        }
    }
}

/// The box and the radio, as the operator hears them.
pub(super) struct KeyerAir {
    keyer_box: MockBox,
    /// Dropped to close the node's audio.
    radio: Option<Radio>,
    settings: Arc<Mutex<RadioSettings>>,
    /// The operator's audio, not yet heard by the radio.
    field: Arc<Mutex<VecDeque<f32>>>,
    /// Radio time spans with the key cable out; the last ends at infinity while
    /// it is out.
    cable_out: Mutex<Vec<(f64, f64)>>,
    /// [`KeyerFault::KeyStuck`] armed: for how long.
    stick: Mutex<Option<f64>>,
    /// Stuck until then (radio time).
    stuck_until: Mutex<Option<f64>>,
}

impl KeyerAir {
    fn now(&self) -> f64 {
        self.keyer_box.clock.secs()
    }

    /// The operator's audio queued for the radio, in samples.
    pub(super) fn queued(&self) -> usize {
        lock(&self.field).len()
    }

    /// The operator's next block of audio, band noise included.
    pub(super) fn push(&self, samples: Vec<f32>) {
        self.update_stuck();
        lock(&self.field).extend(samples);
    }

    /// Stick the radio's key when the node next keys, if armed; let it go when its
    /// time is up.
    fn update_stuck(&self) {
        let now = self.now();
        let mut until = lock(&self.stuck_until);
        if until.is_some_and(|u| now >= u) {
            *until = None;
            lock(&self.settings).stuck_from = None;
        }
        let mut stick = lock(&self.stick);
        if let Some(secs) = *stick {
            if self.keyer_box.now().running() {
                lock(&self.settings).stuck_from = Some(now);
                *until = Some(now + secs);
                *stick = None;
            }
        }
    }

    fn cable_out_at(&self, t: f64) -> bool {
        lock(&self.cable_out)
            .iter()
            .any(|&(a, b)| (a..b).contains(&t))
    }

    fn stuck_now(&self) -> bool {
        let now = self.now();
        lock(&self.settings).stuck_from.is_some_and(|f| now >= f)
    }

    /// Whether the radio is connected to the box and switched on.
    fn keys_from_box(&self) -> bool {
        !lock(&self.settings).off && !self.cable_out_at(self.now())
    }

    /// The radio is on transmit: its key is down, or was within its break-in hang.
    pub(super) fn transmitting(&self) -> bool {
        if self.stuck_now() {
            return true;
        }
        if !self.keys_from_box() {
            return false;
        }
        let hang = lock(&self.settings).hang;
        let to = self.keyer_box.clock.ms();
        let from = to.saturating_sub((hang * 1000.0) as u64);
        !self.keyer_box.now().downs(from, to).is_empty()
    }

    /// Something to hear on the frequency: the radio keying a run from the box
    /// (between its elements too), or its key stuck down.
    pub(super) fn audible(&self) -> bool {
        self.stuck_now() || self.keys_from_box() && self.keyer_box.now().running()
    }

    /// The box's runs from the `from`-th on, as the mock IC-7300 reports its keyer
    /// messages. One keyed while the radio was not connected never went on the air.
    pub(super) fn keyed_from(&self, from: usize) -> Vec<civ::mock::Keyed> {
        let now = self.keyer_box.clock.ms();
        let off = lock(&self.settings).off;
        let b = self.keyer_box.now();
        b.runs
            .iter()
            .skip(from)
            .map(|r| self.keyed(r, &b.downs(r.start, r.end.unwrap_or(now)), now, off))
            .collect()
    }

    fn keyed(&self, r: &Run, downs: &[(u64, u64)], now: u64, off: bool) -> civ::mock::Keyed {
        let ms = Duration::from_millis;
        let end = r.end.unwrap_or(now);
        let complete = r.end.is_some() && r.ended == Ended::Done;
        let first = downs
            .iter()
            .map(|&(d, _)| d)
            .find(|&d| d >= r.start)
            .unwrap_or(r.start);
        civ::mock::Keyed {
            text: r.text.clone(),
            sent: if complete {
                r.text.clone()
            } else {
                r.sent_by(end)
            },
            accepted: ms(r.start),
            start: ms(first),
            end: ms(end),
            complete,
            on_air: !off && !self.cable_out_at(r.start as f64 / 1000.0),
        }
    }

    pub(super) fn fault(&self, f: &KeyerFault) {
        let now = self.now();
        match *f {
            KeyerFault::CableOut(out) => {
                lock(&self.settings).cable_out = out;
                let mut spans = lock(&self.cable_out);
                match (out, spans.last_mut()) {
                    (true, Some(&mut (_, b))) if b.is_infinite() => {}
                    (true, _) => spans.push((now, f64::INFINITY)),
                    (false, Some((_, b))) if b.is_infinite() => *b = now,
                    (false, _) => {}
                }
            }
            KeyerFault::KeyStuck { secs } => *lock(&self.stick) = Some(secs),
            KeyerFault::Unplugged(out) => self.keyer_box.unplug(out),
        }
    }

    pub(super) fn clear_stuck(&self) {
        lock(&self.settings).stuck_from = None;
        *lock(&self.stuck_until) = None;
    }

    /// Stop the radio's audio, which closes the node's.
    pub(super) fn close(&mut self) {
        self.radio = None;
    }
}

/// Run a keyer scenario: [`super::run_inner`] for any radio on the keyer box.
pub(super) fn run_inner(s: &Scenario, scale: f32, out: &mut Outcome) -> Result<()> {
    let scale = scale.clamp(1.0, MAX_SCALE);
    let dir = Scratch::new(&s.name).context("scratch directory")?;
    let cfg = config(s, &dir.0, scale)?;
    let book = node::load_codebook(&cfg)?;
    let clock = Clock::new(scale);
    let pauses = PauseMeter::start(clock);
    let keyer_box = MockBox::new(clock);
    let monitor = Arc::new(Mutex::new(Monitor::starting_at(
        monitor_settings(&cfg, scale),
        clock.epoch,
    )));
    let mut rs = RadioSettings::new(SAMPLE_RATE, PITCH_HZ);
    rs.sidetone = SIDETONE;
    // The operator's audio brings the band noise, if the scenario has any.
    if s.fist.snr_db.is_some() {
        rs.noise = 0.0;
    }
    let field = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let (tx, rx) = audio::queue(usize::MAX);
    // The operator keeps its audio a few blocks ahead of the radio's. A busy
    // machine can hold its thread up for longer than that: the radio then waits for
    // it, up to FIELD_WAIT, rather than hear a gap in the middle of its keying
    // (which miscopies the word). Its audio reaches the node that much later, as
    // from a busy sound card, with no sample lost. Before the operator's first
    // block there is nothing to wait for.
    let feed = {
        let field = field.clone();
        let mut started = false;
        Box::new(move |_: f64, n: usize| {
            let end = Instant::now() + FIELD_WAIT;
            loop {
                let mut q = lock(&field);
                started |= !q.is_empty();
                if q.len() >= n || !started || Instant::now() >= end {
                    return (0..n).map(|_| q.pop_front().unwrap_or(0.0)).collect();
                }
                drop(q);
                thread::sleep(Duration::from_micros(200));
            }
        })
    };
    let radio = Radio::start(rs, keyer_box.clone(), monitor.clone(), Some(tx), Some(feed));
    let settings = radio.settings.clone();
    let rig = KeyerRig::open(keyer_box.transport(), monitor, rig_settings(&cfg, scale))
        .context("greeting the mock keyer box")?;
    inhibit_at_start(s, &cfg)?;
    let station = Station::new(
        rig,
        station_config(&cfg, scale),
        Some(cfg.state_dir.join("health.csv")),
    );
    let (alert_tx, alerts) = mpsc::channel();
    station.notify_inhibit(alert_tx);
    station
        .configure()
        .context("setting up the mock keyer box")?;
    let (session, svc) = node_side(s, &cfg, scale)?;
    let first_line = keyer_box.now().lines.len();

    let done = spawn_node(&cfg, station, session, svc, rx, move || {
        CLOCK_START + clock.secs() as u64
    });
    let k = KeyerAir {
        keyer_box: keyer_box.clone(),
        radio: Some(radio),
        settings,
        field,
        cable_out: Mutex::new(Vec::new()),
        stick: Mutex::new(None),
        stuck_until: Mutex::new(None),
    };
    let mut air = air(s, AirRadio::Keyer(k), None, book, scale);
    let ran = operate(s, &mut air, &done, out);
    out.checks.push(pauses.check(scale));
    let Some((station, session, svc)) = ran else {
        return Ok(());
    };
    let inhibited = station.tx_inhibited();
    // How the node left the box, before dropping the station stops it.
    let (left_keying, stops) = {
        let b = keyer_box.now();
        let stops = b.lines[first_line..]
            .iter()
            .filter(|l| l.contains(" STOP*"))
            .count();
        (b.key_down() || b.running(), stops)
    };
    drop(station);
    let notices: Vec<_> = alerts.try_iter().collect();
    let AirRadio::Keyer(k) = &air.radio else {
        unreachable!("a keyer scenario's operator is on the keyer box");
    };
    let keyed = k.keyed_from(0);
    out.radio_time = Duration::from_secs_f64(clock.secs());

    let e = &s.expect;
    out.checks.push(keyed_check(&keyed, &e.keyed));
    out.checks.push(station_id_check(&keyed, &[], e, scale));
    gateway_checks(&cfg, e, &session, &svc, out)?;
    out.checks.push(forced_receive_check(e, stops, "STOP"));
    out.checks
        .push(box_safety(&cfg, e, &keyer_box, left_keying, inhibited));
    out.checks.push(alert_check(e, &s.node, &notices));
    reception_checks(&cfg, e, out);
    // On a failure, what the station recorded (why a transmission failed, among
    // others): a test keeps no log of the node's.
    if !out.passed() {
        if let Ok(h) = std::fs::read_to_string(cfg.state_dir.join("health.csv")) {
            out.transcript.extend(
                h.lines()
                    .filter_map(|l| l.split_once(','))
                    .map(|(_, ev)| format!("HEALTH {}", ev.replacen(',', ": ", 1))),
            );
        }
    }
    Ok(())
}

/// Bounds that hold whatever the scenario: the box's key-downs and runs within
/// its limits and the station's, never tripped, its key open when the node
/// stopped, and the duty cycle.
fn box_safety(
    cfg: &Config,
    e: &Expect,
    keyer_box: &MockBox,
    left_keying: bool,
    inhibited: bool,
) -> Check {
    let b = keyer_box.now();
    let now = keyer_box.clock.ms();
    let downs = b.downs(0, now);
    let longest_down = downs.iter().map(|&(d, u)| u - d).max().unwrap_or(0);
    let total_down: u64 = downs.iter().map(|&(d, u)| u - d).sum();
    let longest_run = b
        .runs
        .iter()
        .map(|r| r.end.unwrap_or(now) - r.start)
        .max()
        .unwrap_or(0);
    let mut bad = Vec::new();
    if longest_down > u64::from(keyer_core::limits::KEY_DOWN_MS) {
        bad.push(format!("key down for {longest_down} ms at once"));
    }
    if longest_run > cfg.station.max_key_seconds * 1000 {
        bad.push(format!(
            "a run of {:.1} s (station.max_key_seconds {})",
            longest_run as f32 / 1000.0,
            cfg.station.max_key_seconds
        ));
    }
    if b.trip() != Trip::None {
        bad.push("the box tripped".into());
    }
    if left_keying {
        bad.push("the box was keying when the node stopped".into());
    }
    let duty = total_down as f32 / now.max(1000) as f32;
    if duty > 0.5 {
        bad.push(format!("key-down duty {:.0}%", duty * 100.0));
    }
    if inhibited != e.inhibited {
        bad.push(format!(
            "transmit inhibited: {inhibited}, expected {}",
            e.inhibited
        ));
    }
    let detail = format!(
        "{} runs, longest {:.1} s, longest key-down {longest_down} ms, duty {:.0}%, {} restarts",
        b.runs.len(),
        longest_run as f32 / 1000.0,
        duty * 100.0,
        b.resets
    );
    if bad.is_empty() {
        check("safety", true, detail)
    } else {
        check("safety", false, format!("{}; {detail}", bad.join("; ")))
    }
}

/// `s` on any radio through the keyer box, which has no tuner: no tune at
/// start-up, so no ID after one either.
fn on_keyer(mut s: Scenario, name: &str, about: &str) -> Scenario {
    s.name = name.into();
    s.about = about.into();
    s.radio.keyer = true;
    s.expect.tunes = 0;
    s.expect.ids = 0;
    // The operator's signal clean, over the radio's own band noise. These are
    // about keying: copying a noisy signal is for the IC-7300 scenarios and the
    // sweep, where the node takes the audio at its own pace. Here it arrives on
    // the radio's clock, so how a rare miscopy falls depends on the machine's load.
    s.fist.snr_db = None;
    s
}

/// The keyer scenarios; some are IC-7300 ones (`ic7300`) on the keyer box.
pub(super) fn scenarios(ic7300: &[Scenario]) -> Vec<Scenario> {
    let like = |name: &str| {
        ic7300
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no scenario {name}"))
    };
    let open = format!("{FIELD_CALL} 42 {{42}} TX MOM HI K");
    let read_back = rb_tx(42, "MOM", "HI");
    let done = de("SENT 43");
    let mut v = vec![
        on_keyer(
            like("tx"),
            "keyer-tx",
            "TX on any radio through the keyer box: every piece heard back in the sidetone",
        ),
        on_keyer(
            like("rx-long"),
            "keyer-rx-long",
            "a 26-chunk readout through the keyer box, one run per piece, IDs between chunks",
        ),
        {
            let mut s = on_keyer(
                like("agn"),
                "keyer-agn",
                "AGN through the keyer box repeats the last over; bare and unknown AGNs ignored",
            );
            // Its AGNs that get silence must be copied for certain (see `agn`), but
            // the decoder prints a stray E in band noise a few times a minute, and
            // one just before a call turns its A into a U. So the operator brings
            // the band noise instead of the radio, as loud (27 dB under its
            // signal): the noise before and in each call is then the same however
            // long the node took over the one before.
            s.fist.snr_db = Some(27.0);
            s
        },
    ];
    v.push({
        let mut s = on_keyer(
            like("tx"),
            "keyer-cable-out",
            "the key cable is out of the radio: the read-back is not heard in the sidetone, so \
             the node keys nothing more until its retune (10 minutes on); cable back in, a \
             fresh exchange then goes through",
        );
        s.node.retune_minutes = 10;
        s.script = vec![
            Step::Keyer(KeyerFault::CableOut(true)),
            Step::Unanswered {
                text: open.clone(),
                tries: 2,
            },
            Step::Keyer(KeyerFault::CableOut(false)),
            Step::Wait(600.0),
            Step::Exchange(Exchange {
                request: "TX MOM HI".into(),
                read_back: rb_tx(43, "MOM", "HI").replace("43", "{open}"),
                result: de("SENT {commit}"),
                restarts: 0,
            }),
        ];
        let (rb, done) = (rb_tx(43, "MOM", "HI"), de("SENT 44"));
        s.expect.keyed = full(&[&rb, &done]);
        s.expect.sent = sent("MOM", "HI");
        s.expect.last_seq = 44;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = on_keyer(
            like("tx"),
            "keyer-stuck-key",
            "the radio's key sticks down as the node keys its result: the box opens its key but \
             the sidetone goes on, so the node inhibits transmitting and alerts the owner; a \
             timer at the radio lets go and nothing more is keyed",
        );
        s.script = vec![
            Step::Open {
                text: open.clone(),
                read_back: read_back.clone(),
            },
            Step::Keyer(KeyerFault::KeyStuck { secs: 60.0 }),
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(done.clone()),
            },
            Step::Unanswered {
                text: "AGN 44 {44} K".into(),
                tries: 1,
            },
        ];
        s.expect.keyed = full(&[&read_back, &done]);
        s.expect.sent = sent("MOM", "HI");
        s.expect.last_seq = 44;
        s.expect.forced_receive = true;
        s.expect.inhibited = true;
        s
    });
    v.push({
        let mut s = on_keyer(
            like("tx"),
            "keyer-box-unplugged",
            "the box's USB cable is pulled while the node listens: its read-back fails without \
             keying anything or inhibiting; plugged in again, the open repeated gets its \
             read-back",
        );
        s.script = vec![
            Step::Wait(5.0),
            Step::Keyer(KeyerFault::Unplugged(true)),
            Step::Unanswered {
                text: open.clone(),
                tries: 1,
            },
            Step::Keyer(KeyerFault::Unplugged(false)),
            Step::Wait(5.0),
            Step::Open {
                text: open,
                read_back: read_back.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed = full(&[&read_back, &done]);
        s.expect.sent = sent("MOM", "HI");
        // The node's STOP cannot reach an unplugged box, whose key opened when it
        // lost power.
        s.expect.forced_receive = false;
        s
    });
    v
}
