//! Closed-loop self-test: a scripted field operator against the whole node and a
//! byte-level mock IC-7300, with no radio, sound card or network.
//!
//! ```text
//!  operator ──CW audio (Keyer + Noise)──► node::run ──CI-V bytes──► Ic7300 driver
//!     ▲                                                                 │
//!     └──────────── text the mock radio actually keyed ◄── civ::mock ◄──┘
//! ```
//!
//! The operator keys its transmissions as audio into the node's audio queue, in
//! 50 ms blocks, and listens to the mock radio: what it keyed and when it is
//! sounding. It reacts like a real operator: it waits for the frequency to be quiet,
//! keys, waits for the node's over, checks a read-back and repeats a transmission
//! that got no answer. Every scenario ends with the audio source closing, which
//! makes `node::run` return, and an overall time limit. While the mock radio is on
//! transmit the node hears nothing of the operator, as a real receiver would.
//!
//! **Time scale.** Everything runs `scale` times faster than real time so a
//! scenario takes a second or two: the mock radio keys at the scaled speed, the
//! audio is paced to it, and the station's timing constants are divided by it
//! ([`TimeScaled`] divides the driver's dot length, the only timing the station
//! reads from the radio), and so are the session's pending-commit timeout and `AGN`
//! window; the listening schedule follows a clock that runs in radio time. The CI-V
//! reply timeout, the watchdog tick and the forced receive retry pause stay in real
//! time, so faults on them take longer in radio time than they would on the air: at
//! `--scale 1` everything runs at its real speed. The audio is also held back while
//! the node has not taken what was sent (it is decoding, transmitting or waiting on
//! the radio), so a slow machine slows the operator down instead of failing
//! scenarios; lower the scale on a slow machine (`--scale`, or `HFNODE_E2E_SCALE`
//! for the tests).
//!
//! Codes come from [`TEST_KEY`], a fixed key for tests only.

mod any_radio;

pub use any_radio::{KeyerFault, MAX_SCALE as KEYER_MAX_SCALE};

use crate::audio::{self, Block, BlockReceiver, BlockSender};
use crate::commissioning::{self, Action, Stage};
use crate::config::Config;
use crate::inbox::{Message, State as MsgState};
use crate::node;
use crate::session::{SendError, Services, Session, WxError};
use crate::station::{InhibitNotice, Station, StationConfig};
use anyhow::{Context, Result};
use auth::{CodeBook, SeqStore};
use civ::ic7300::Ic7300;
use civ::mock::{
    is_read, Fault, Foldback, Menu, MockConfig, MockRadio, ReplyFault, Report, Settings,
};
use civ::Rig;
use cw::{Keyer, Noise};
use protocol::{parse, Vocabulary};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The key every self-test and test-vector code comes from. For tests only: anyone
/// can compute its codes, so a node using it accepts anyone.
pub const TEST_KEY: &[u8] = b"hfnode self-test key: TEST ONLY, never use on the air";
pub const NODE_CALL: &str = "N0DE";
pub const FIELD_CALL: &str = "W5XXX";
/// `last_seq` when a scenario starts: its first open uses line 42.
pub const START_SEQ: u64 = 41;
/// The `[weather] default_grid` of the scenarios' node.
pub const WX_DEFAULT_GRID: &str = "DL89";
/// Default time scale.
pub const DEFAULT_SCALE: f32 = 100.0;
/// Highest time scale. The driver's 500 ms CI-V reply timeout and its read-until-
/// quiet afterwards stay in real time; much above this they last longer in radio
/// time than the session's (time-scaled) 10-minute pending window, and a scenario
/// with a lost or late reply could no longer finish its transaction.
pub const MAX_SCALE: f32 = 200.0;

const SAMPLE_RATE: u32 = 8000;
const PITCH_HZ: f32 = 600.0;
/// 50 ms of audio, as the capture thread delivers it.
const BLOCK: usize = 400;
const BLOCK_SECS: f32 = 0.05;
/// Blocks the node may have queued before the operator waits for it: while the
/// radio is on transmit, the node's capture queue's 10 s, as on the air; otherwise
/// 0.4 s.
const AHEAD_TX: usize = 200;
const AHEAD: usize = 8;
/// Samples of the operator's audio kept queued ahead of a keyer-box radio's.
const KEYER_AHEAD: usize = 4 * BLOCK;
/// Real time the operator may fall behind its schedule and still catch up, sending
/// blocks back to back as the node takes them (never ahead of the schedule); beyond
/// it the schedule restarts from now, so a node that is slow to take the audio slows
/// the operator down instead.
const CATCH_UP: Duration = Duration::from_millis(50);
/// Seconds of audio. The operator waits this long after the frequency goes quiet
/// before keying.
const REACTION: f32 = 2.0;
/// No reply started this long after the operator's over: the node is silent.
const NO_REPLY: f32 = 15.0;
/// An over that stops this long without its `DE <call> K` was cut short.
const CUT_AFTER: f32 = 8.0;
/// How often the operator keys a transmission that gets no answer.
const TRIES: u32 = 3;
/// Listening after the script, for anything keyed late.
const TRAIL: f32 = 10.0;
/// Radio time a scenario may take at most.
const BUDGET: Duration = Duration::from_secs(1800);
/// Peak amplitude of the operator's signal and of the radio's sidetone.
const AMPLITUDE: f32 = 0.5;
const SIDETONE: f32 = 0.3;
/// Unix time at radio time zero, for the listening schedule: the top of a UTC hour,
/// so a scenario starts at the beginning of a window.
const CLOCK_START: u64 = 1_699_999_200;
/// The node's frequency in every scenario.
const FREQUENCY_HZ: u64 = 7_030_000;
/// How often someone at the radio nudges the dial, in blocks, with
/// [`RadioSetup::dial_nudges`].
const NUDGE_BLOCKS: u64 = 10;
/// The station's stuck margin ([`StationConfig::from_config`]), copied so that the
/// safety check does not move with it: a radio left on transmit after its last
/// element is forced back to receive within the break-in delay plus this.
const STUCK_MARGIN: Duration = Duration::from_secs(3);
/// The station's semi break-in delay in dots, copied likewise.
const BREAK_IN_DOTS: f32 = 10.0;
/// Seconds of radio time [`Step::WaitReceive`] waits at most, and real time: the
/// watchdog's tick and forced receive pauses run in real time.
const RECEIVE_WAIT: f32 = 300.0;
const RECEIVE_WAIT_REAL: Duration = Duration::from_secs(5);

/// The field operator's sending.
#[derive(Debug, Clone)]
pub struct Fist {
    pub wpm: f32,
    /// Signal-to-noise ratio in 2500 Hz; `None` for no noise at all.
    pub snr_db: Option<f32>,
    /// Hand-keying timing jitter, as [`Keyer::jitter`].
    pub jitter: f32,
    /// Stretch of character and word gaps, as [`Keyer::gap_stretch`].
    pub gap_stretch: f32,
    /// How far off the node's CW pitch the signal is.
    pub offset_hz: f32,
    pub seed: u64,
}

impl Default for Fist {
    fn default() -> Self {
        Self {
            wpm: 18.0,
            snr_db: Some(15.0),
            jitter: 0.03,
            gap_stretch: 1.0,
            offset_hz: 0.0,
            seed: 1,
        }
    }
}

/// The node's side: its keyer speed, chunking and what its gateways do.
#[derive(Debug, Clone)]
pub struct NodeSetup {
    pub key_wpm: u32,
    pub chunk_chars: usize,
    pub fail_send: bool,
    /// No route to any contact (`FAIL <seq> NO ROUTE`).
    pub no_route: bool,
    /// What every weather request gets instead of a forecast.
    pub weather_error: Option<WxError>,
    /// Inbound messages ready to read, as (contact, text).
    pub inbox: Vec<(String, String)>,
    /// Listening windows as (`every_minutes`, `window_minutes`), from the top of
    /// the hour at radio time zero; `None` listens all the time.
    pub schedule: Option<(u32, u32)>,
    /// The node starts with [`crate::station::INHIBIT_FILE`] already in its state
    /// directory, left by a fault before a restart.
    pub inhibited_at_start: bool,
    /// `schedule.check_minutes` and `schedule.retune_minutes`, in radio time.
    pub check_minutes: u32,
    pub retune_minutes: u32,
}

impl Default for NodeSetup {
    fn default() -> Self {
        Self {
            key_wpm: 18,
            chunk_chars: 60,
            fail_send: false,
            no_route: false,
            weather_error: None,
            inbox: Vec::new(),
            schedule: None,
            inhibited_at_start: false,
            check_minutes: 10,
            retune_minutes: 60,
        }
    }
}

/// The mock radio's setup.
#[derive(Debug, Clone)]
pub struct RadioSetup {
    pub echo: bool,
    pub swr: f32,
    pub foldback: Option<Foldback>,
    /// Faults armed before the node starts, so its first tune and `DE <call>`
    /// meet them first; to hit an answer, inject them with [`Step::Inject`].
    pub faults: Vec<Fault>,
    /// Mix the radio's own keying into the receive audio, as sidetone.
    pub sidetone: bool,
    /// Someone at the radio keeps nudging the dial 10 Hz up and back, so that CI-V
    /// Transceive frames arrive unasked all through the scenario.
    pub dial_nudges: bool,
    /// Any radio keyed through its key jack by the keyer box, its headphone audio
    /// heard through a sound card, instead of the mock IC-7300 (the fields above
    /// are the IC-7300's): see [`any_radio`].
    pub keyer: bool,
    /// The IC-7300's menu settings, as the node's preflight reads them.
    pub menu: Menu,
}

impl Default for RadioSetup {
    fn default() -> Self {
        Self {
            echo: true,
            swr: 1.2,
            foldback: None,
            faults: Vec::new(),
            sidetone: false,
            dial_nudges: false,
            keyer: false,
            menu: Menu::default(),
        }
    }
}

/// One thing the operator does. In texts, `{n}` is the code for line `n` (`{ng}` in
/// the two printed groups of four), and a trailing ` ~` keys a noise burst (a lone
/// dit) 1.5 s after the over.
#[derive(Debug, Clone)]
pub enum Step {
    /// Key an open and wait for the read-back, repeating the open (exactly, as the
    /// operating guide says) if none comes. A read-back other than `read_back` is
    /// answered `NO` on the next unused line and fails the scenario.
    Open {
        text: String,
        read_back: String,
    },
    /// Key `text`. With `Some(over)`, wait for exactly that answer, repeating
    /// `text` if none comes; with `None`, the node must stay silent.
    Say {
        text: String,
        expect: Option<String>,
    },
    /// Key `text` `tries` times; no try may get a complete answer (the node is
    /// locked out or inhibited). An over cut short is allowed.
    Unanswered {
        text: String,
        tries: u32,
    },
    /// The node's next over is lost in QRM: the operator does not copy it.
    MissNext,
    /// Radio-side changes, from here on: after the node's first tune and ID when
    /// it comes first in the script.
    Inject(Fault),
    SetSwr(f32),
    /// A hardware PTT timer (or a power cycle) ends a stuck transmit.
    ClearStuck,
    /// Seconds of listening.
    Wait(f32),
    /// Listen until the radio is back on receive, for at most [`RECEIVE_WAIT`] s of
    /// radio time or [`RECEIVE_WAIT_REAL`] of real time, whichever is longer.
    /// For timing the script only: an operator cannot hear a carrier-less transmit.
    WaitReceive,
    /// Listen until the node tunes at the top of its next window, and that is over.
    NextWindow,
    /// Someone at the radio changes it.
    Panel(Panel),
    /// The next open, in an [`Step::Open`] or [`Step::Exchange`], must get its
    /// read-back the first time it is keyed: after a long quiet spell, for example,
    /// a decoder that had drifted would need a repeat.
    FirstTry,
    /// The radio has run this many tuner cycles so far.
    Tunes(u32),
    /// A whole transaction on the next two unused lines, as the operating guide
    /// says to work one; see [`Exchange`].
    Exchange(Exchange),
    /// A fault at the keyer box or the radio it keys.
    Keyer(KeyerFault),
}

/// A change made at the radio's front panel, behind the node's back.
#[derive(Debug, Clone)]
pub enum Panel {
    /// Tune the dial to this frequency.
    Dial(u64),
    /// Select a mode and filter, as CI-V numbers them (p. 19-9).
    Mode(u8, u8),
    /// Switch split on, transmitting on this frequency, or off.
    Split(Option<u64>),
    /// Switch ∂TX on or off.
    DeltaTx(bool),
}

/// A transaction the operator works on the next two unused lines (from line 42):
/// key the open, repeating it exactly while no read-back comes; on the expected
/// read-back answer `OK`, repeating it while no result comes. A read-back other
/// than the expected one (text garbled into something that still parsed) is
/// answered `NO` on the line the `OK` would have used, and the operator starts
/// over on fresh lines, at most `restarts` times. The operator never commits a
/// read-back that is not exactly right.
#[derive(Debug, Clone)]
pub struct Exchange {
    /// What follows the code in the open: `TX MOM HOME SUN`, `RX`.
    pub request: String,
    /// The expected read-back; `{open}` stands for the open's line number.
    pub read_back: String,
    /// The expected result; `{commit}` stands for the commit's line number.
    pub result: String,
    pub restarts: u32,
}

/// One of the node's overs, as the radio keyed it.
#[derive(Debug, Clone, PartialEq)]
pub enum Over {
    Full(String),
    /// Stopped part-way: what was keyed is a proper prefix of this text.
    Cut(String),
}

#[derive(Debug, Clone)]
pub struct Expect {
    pub keyed: Vec<Over>,
    /// Messages the gateway sent, as (contact, text).
    pub sent: Vec<(String, String)>,
    /// Inbound message ids marked read.
    pub read: Vec<u64>,
    /// Weather requests, by grid square.
    pub weather: Vec<String>,
    pub last_seq: u64,
    /// Tuner cycles: one when the node starts listening (at start-up or a window),
    /// one before each reply due a re-tune, none after an inhibit.
    pub tunes: u32,
    /// The node forced the radio to receive (stopped the keyer with `17 FF`) while
    /// it ran, as it must after any fault and never otherwise.
    pub forced_receive: bool,
    /// The node inhibited transmitting until restart.
    pub inhibited: bool,
    /// `DE <call>` keyed right after a tune: one per tune that matched when the node
    /// started listening (at start-up or a window's start; a tune before a reply is
    /// followed by the reply).
    pub ids: u32,
    /// `DE <call>` keyed on its own inside overs, between chunks.
    pub mid_ids: u32,
    /// Text the node must have decoded and logged in `rx.log`, answered or not.
    pub heard: Vec<String>,
    /// The IC-7300's preflight refuses to start the node, naming this check: the
    /// node writes nothing to the radio, so nothing above applies.
    pub refused: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub about: String,
    pub fist: Fist,
    pub node: NodeSetup,
    pub radio: RadioSetup,
    pub script: Vec<Step>,
    pub expect: Expect,
}

/// Pass or fail for one check, with the reason.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub scenario: String,
    pub checks: Vec<Check>,
    /// Real time taken.
    pub wall: Duration,
    /// Radio time taken.
    pub radio_time: Duration,
    /// What the operator sent and heard.
    pub transcript: Vec<String>,
    /// What happened, beyond the checks.
    pub facts: Facts,
}

/// What a run did, as the sweep classifies it.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// Messages the gateway sent, as (contact, text).
    pub sent: Vec<(String, String)>,
    /// Inbound message ids marked read.
    pub read: Vec<u64>,
    /// Everything the operator keyed, codes filled in, in order.
    pub operator_sent: Vec<String>,
    /// What the node decoded, one entry per reception (its `rx.log`).
    pub received: Vec<String>,
    /// [`Step::Exchange`]s that ended with the expected result.
    pub exchanges_done: u32,
    /// Read-backs that were not the expected one, each answered `NO`.
    pub wrong_read_backs: u32,
    /// Times the operator started over on fresh lines.
    pub restarts: u32,
    /// Transmissions in [`Step::Exchange`]s beyond the first open and the first
    /// `OK` of each: repeats, `NO`s and the opens and `OK`s on fresh lines.
    pub extra_transmissions: u32,
    /// Why the script stopped early, if it did.
    pub script_failures: Vec<String>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.pass)
    }

    /// The failed checks, one line each, or `ok`.
    pub fn summary(&self) -> String {
        let failed: Vec<String> = self
            .checks
            .iter()
            .filter(|c| !c.pass)
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect();
        if failed.is_empty() {
            "ok".into()
        } else {
            failed.join("; ")
        }
    }

    /// Every check and the transcript.
    pub fn render(&self) -> String {
        let mut s = format!(
            "{} {} ({:.1} s, {:.0} s radio time)\n",
            self.scenario,
            if self.passed() { "PASS" } else { "FAIL" },
            self.wall.as_secs_f32(),
            self.radio_time.as_secs_f32()
        );
        for c in &self.checks {
            let _ = writeln!(
                s,
                "  [{}] {}: {}",
                if c.pass { "ok" } else { "FAIL" },
                c.name,
                c.detail
            );
        }
        for l in &self.transcript {
            let _ = writeln!(s, "    {l}");
        }
        if !self.facts.received.is_empty() {
            let _ = writeln!(s, "  the node decoded (rx.log):");
            for l in &self.facts.received {
                let _ = writeln!(s, "    {l}");
            }
        }
        s
    }
}

/// [`Services`] for tests: records what was sent, serves a fixed inbox and canned
/// weather.
#[derive(Debug, Default)]
pub struct FakeServices {
    pub sent: Vec<(String, String)>,
    pub inbox: Vec<Message>,
    /// Ids marked read, each once, in the order first marked.
    pub read: Vec<u64>,
    pub weather_calls: Vec<String>,
    pub fail_send: bool,
    pub no_route: bool,
    pub weather_error: Option<WxError>,
}

impl FakeServices {
    /// The forecast served for `grid`.
    pub fn forecast(grid: &str) -> String {
        format!("{grid} TDA SUNNY HI 75 TNGT CLEAR LO 50")
    }
}

impl Services for FakeServices {
    fn send_message(&mut self, dest: &str, _from_call: &str, text: &str) -> Result<(), SendError> {
        if self.no_route {
            return Err(SendError::NoRoute(
                "no Google Voice reply address yet".into(),
            ));
        }
        if self.fail_send {
            return Err(SendError::Gateway("gateway down".into()));
        }
        self.sent.push((dest.into(), text.into()));
        Ok(())
    }

    fn ready_messages(&mut self) -> Vec<Message> {
        self.inbox
            .iter()
            .filter(|m| m.state == MsgState::Ready)
            .cloned()
            .collect()
    }

    fn mark_read(&mut self, ids: &[u64]) {
        // Marking is idempotent, and an AGN of an RX result marks the same ones
        // again (so that a readout whose keying failed is marked when repeated):
        // record each id once.
        for id in ids {
            if !self.read.contains(id) {
                self.read.push(*id);
            }
        }
        for m in self.inbox.iter_mut().filter(|m| ids.contains(&m.id)) {
            m.state = MsgState::Read;
        }
    }

    fn weather(&mut self, grid: &str) -> Result<String, WxError> {
        self.weather_calls.push(grid.to_string());
        match &self.weather_error {
            Some(e) => Err(e.clone()),
            None => Ok(Self::forecast(grid)),
        }
    }
}

/// A [`Rig`] that passes every call to `inner` and divides the keyer's dot length
/// by the time scale, so that the station times keying to match a time-scaled mock
/// radio. Nothing else is changed: the driver still sends and parses every byte.
pub struct TimeScaled<R> {
    pub inner: R,
    pub scale: f32,
}

impl<R: Rig> Rig for TimeScaled<R> {
    fn frequency(&mut self) -> civ::Result<u64> {
        self.inner.frequency()
    }
    fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
        self.inner.set_frequency(hz)
    }
    fn set_mode_cw(&mut self) -> civ::Result<()> {
        self.inner.set_mode_cw()
    }
    fn set_rf_power_watts(&mut self, watts: u32) -> civ::Result<()> {
        self.inner.set_rf_power_watts(watts)
    }
    fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
        self.inner.set_key_speed(wpm)
    }
    fn set_break_in(&mut self, on: bool) -> civ::Result<()> {
        self.inner.set_break_in(on)
    }
    fn set_break_in_delay(&mut self, dots: f32) -> civ::Result<()> {
        self.inner.set_break_in_delay(dots)
    }
    fn dot_duration(&mut self) -> civ::Result<Duration> {
        Ok(self.inner.dot_duration()?.div_f32(self.scale))
    }
    fn start_tune(&mut self) -> civ::Result<()> {
        self.inner.start_tune()
    }
    fn tuner_busy(&mut self) -> civ::Result<bool> {
        self.inner.tuner_busy()
    }
    fn tuner_matched(&mut self) -> civ::Result<bool> {
        self.inner.tuner_matched()
    }
    fn transmit_frequency(&mut self) -> civ::Result<u64> {
        self.inner.transmit_frequency()
    }
    fn split_or_delta_tx(&mut self) -> civ::Result<bool> {
        self.inner.split_or_delta_tx()
    }
    fn read_swr(&mut self) -> civ::Result<f32> {
        self.inner.read_swr()
    }
    fn read_po(&mut self) -> civ::Result<f32> {
        self.inner.read_po()
    }
    fn send_cw(&mut self, text: &str) -> civ::Result<()> {
        self.inner.send_cw(text)
    }
    fn stop_cw(&mut self) -> civ::Result<()> {
        self.inner.stop_cw()
    }
    fn is_transmitting(&mut self) -> civ::Result<bool> {
        self.inner.is_transmitting()
    }
    fn set_transmit(&mut self, tx: bool) -> civ::Result<()> {
        self.inner.set_transmit(tx)
    }
    fn has_tuner(&self) -> bool {
        self.inner.has_tuner()
    }
    fn has_meters(&self) -> bool {
        self.inner.has_meters()
    }
    fn rest_needed(&mut self, keying: Duration) -> civ::Result<Duration> {
        // The station's times are scaled, the rig's own in real time already.
        self.inner.rest_needed(keying)
    }
    fn keying_confirmed(&mut self) -> civ::Result<Option<bool>> {
        self.inner.keying_confirmed()
    }
    fn transmit_detail(&mut self) -> Option<String> {
        self.inner.transmit_detail()
    }
}

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> std::io::Result<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "hfnode-selftest-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A well-mixed 64-bit seed from a few numbers (SplitMix64).
fn mix(parts: &[u64]) -> u64 {
    let mut z = 0x9E37_79B9_7F4A_7C15u64;
    for &p in parts {
        z = (z ^ p).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
    }
    z
}

/// The highest line `n` whose code `{n}` (or `{ng}`) `text` asks for.
fn highest_line(text: &str) -> Option<u64> {
    text.split('{')
        .skip(1)
        .filter_map(|t| t.split_once('}'))
        .filter_map(|(n, _)| n.strip_suffix('g').unwrap_or(n).parse().ok())
        .max()
}

/// Replace `{n}` with the code for line `n`.
pub fn with_codes(text: &str, book: &CodeBook) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        out.push_str(&rest[..i]);
        let inner = &rest[i + 1..i + j];
        let (num, grouped) = match inner.strip_suffix('g') {
            Some(n) => (n, true),
            None => (inner, false),
        };
        match num.parse::<u64>() {
            Ok(n) if grouped => {
                let code = book.code(n);
                let (a, b) = code.split_at(code.len() / 2);
                out.push_str(&format!("{a} {b}"));
            }
            Ok(n) => out.push_str(&book.code(n)),
            Err(_) => out.push_str(&rest[i..=i + j]),
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}

/// The sample of an `n`-sample block starting at radio time `from` that falls at `t`.
fn at(from: Duration, t: Duration, n: usize) -> usize {
    ((t - from).as_secs_f32() / BLOCK_SECS * n as f32) as usize
}

/// What the operator made of the node's next over.
#[derive(Debug)]
enum Heard {
    Over(String),
    Cut(String),
    Nothing,
}

/// The radio the node drives: the mock IC-7300, or any radio on the keyer box.
enum AirRadio {
    Ic7300(MockRadio),
    Keyer(any_radio::KeyerAir),
}

impl AirRadio {
    fn ic7300(&self) -> Result<&MockRadio, String> {
        match self {
            Self::Ic7300(r) => Ok(r),
            Self::Keyer(_) => Err("this step needs the mock IC-7300".into()),
        }
    }

    fn keyer(&self) -> Result<&any_radio::KeyerAir, String> {
        match self {
            Self::Ic7300(_) => Err("this step needs the keyer box".into()),
            Self::Keyer(k) => Ok(k),
        }
    }

    fn transmitting(&self) -> bool {
        match self {
            Self::Ic7300(r) => r.transmitting(),
            Self::Keyer(k) => k.transmitting(),
        }
    }

    fn audible(&self) -> bool {
        match self {
            Self::Ic7300(r) => r.audible(),
            Self::Keyer(k) => k.audible(),
        }
    }

    fn tunes(&self) -> u32 {
        match self {
            Self::Ic7300(r) => r.tunes(),
            Self::Keyer(_) => 0,
        }
    }

    fn keyed_from(&self, from: usize) -> Vec<civ::mock::Keyed> {
        match self {
            Self::Ic7300(r) => r.keyed_from(from),
            Self::Keyer(k) => k.keyed_from(from),
        }
    }
}

/// The operator, on the air: the audio it puts into the node and what it hears
/// from the mock radio.
struct Air {
    radio: AirRadio,
    tx: Option<BlockSender>,
    book: CodeBook,
    fist: Fist,
    sigma: f32,
    sidetone: bool,
    /// The frequency to nudge the dial around, with [`RadioSetup::dial_nudges`].
    nudge_hz: Option<u64>,
    block_real: Duration,
    next_due: Instant,
    deadline: Instant,
    /// Blocks sent so far: the operator's clock.
    blocks: u64,
    /// The operator's own audio still to send.
    keying: VecDeque<f32>,
    /// Inside a transmission (lead-in included): its number and block count, which
    /// seed the noise, so the audio of each transmission does not depend on timing.
    tx_index: u64,
    in_tx: bool,
    tx_block: u64,
    /// Keyer messages already heard (or missed).
    heard: usize,
    deaf: bool,
    over_end: String,
    log: Vec<String>,
    /// The next unused line: above every line keyed so far, for [`Step::Exchange`]
    /// and a `NO`.
    line: u64,
    /// Everything keyed, codes filled in.
    sent: Vec<String>,
    exchanges_done: u32,
    wrong_read_backs: u32,
    restarts: u32,
    /// Transmissions in exchanges beyond the first open and the first `OK`.
    extra: u32,
    /// The node tunes when it starts listening (not if it starts with transmitting
    /// inhibited): the operator waits for that before calling.
    tune_at_start: bool,
    /// The next open may be keyed only once ([`Step::FirstTry`]).
    first_try: bool,
    /// Tuner cycles noted in the transcript so far.
    tunes_noted: u32,
}

impl Air {
    fn secs(&self) -> f32 {
        self.blocks as f32 * BLOCK_SECS
    }

    fn note(&mut self, s: impl AsRef<str>) {
        let line = format!("{:7.1}s {}", self.secs(), s.as_ref());
        self.log.push(line);
    }

    /// Send one block of audio. To the mock IC-7300's node it goes paced to the time
    /// scale and held back while the node has not taken what was already sent; to
    /// a radio on the keyer box, whose headphone audio runs in real time as a sound
    /// card's would, a few blocks ahead of it.
    fn tick(&mut self) -> Result<(), String> {
        match &self.radio {
            AirRadio::Ic7300(_) => self.pace()?,
            AirRadio::Keyer(k) => {
                while k.queued() >= KEYER_AHEAD {
                    if Instant::now() > self.deadline {
                        return Err("scenario took too long".into());
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            }
        }
        let mut samples: Vec<f32> = (0..BLOCK)
            .map(|_| self.keying.pop_front().unwrap_or(0.0))
            .collect();
        if self.fist.snr_db.is_some() {
            let seed = if self.in_tx {
                mix(&[self.fist.seed, self.tx_index, self.tx_block])
            } else {
                mix(&[self.fist.seed, u64::MAX, self.blocks])
            };
            Noise::new(seed).add(&mut samples, self.sigma);
        }
        if self.in_tx {
            self.tx_block += 1;
        }
        match &self.radio {
            AirRadio::Ic7300(radio) => {
                let to = radio.now();
                let from = to.saturating_sub(Duration::from_secs_f32(BLOCK_SECS));
                // On transmit the radio receives nothing: the operator's signal is lost.
                for (a, b) in radio.transmit_between(from, to) {
                    let n = samples.len();
                    samples[at(from, a, n).min(n)..at(from, b, n).min(n)].fill(0.0);
                }
                if self.sidetone {
                    add_sidetone(radio, &mut samples, from, to);
                }
                let block = Block {
                    at: Instant::now(),
                    samples,
                };
                let tx = self.tx.as_ref().ok_or("audio closed")?;
                tx.send(block).map_err(|_| "the node stopped listening")?;
            }
            AirRadio::Keyer(k) => k.push(samples),
        }
        self.blocks += 1;
        let tunes = self.radio.tunes();
        if tunes > self.tunes_noted {
            self.tunes_noted = tunes;
            self.note("NODE  (tuning)");
        }
        Ok(())
    }

    /// Wait until the mock IC-7300's node is due its next block of audio.
    fn pace(&mut self) -> Result<(), String> {
        let AirRadio::Ic7300(radio) = &self.radio else {
            return Ok(());
        };
        let tx = self.tx.as_ref().ok_or("audio closed")?;
        loop {
            let now = Instant::now();
            if now > self.deadline {
                return Err("scenario took too long".into());
            }
            let ahead = if radio.transmitting() {
                AHEAD_TX
            } else {
                AHEAD
            };
            if now >= self.next_due && tx.queued() < ahead {
                break;
            }
            let wait = self
                .next_due
                .saturating_duration_since(now)
                .clamp(Duration::from_micros(50), Duration::from_micros(500));
            thread::sleep(wait);
        }
        // Keep to the schedule through a late wake-up (a coarse timer or a busy
        // machine, as on a macOS CI runner), catching up while the node takes the
        // audio; only a node that held the operator back for longer moves it.
        let now = Instant::now();
        let start = if now.saturating_duration_since(self.next_due) > CATCH_UP {
            now
        } else {
            self.next_due
        };
        self.next_due = start + self.block_real;

        if let Some(hz) = self
            .nudge_hz
            .filter(|_| self.blocks.is_multiple_of(NUDGE_BLOCKS))
        {
            radio.turn_dial(hz + 10);
            radio.turn_dial(hz);
        }
        Ok(())
    }

    /// Close the node's audio, which makes `node::run` return.
    fn close_audio(&mut self) {
        self.tx = None;
        if let AirRadio::Keyer(k) = &mut self.radio {
            k.close();
        }
    }

    fn idle(&mut self, secs: f32) -> Result<(), String> {
        let until = self.secs() + secs;
        while self.secs() < until {
            self.tick()?;
        }
        Ok(())
    }

    fn wait_quiet(&mut self) -> Result<(), String> {
        while self.radio.audible() {
            self.tick()?;
        }
        Ok(())
    }

    /// Wait for a quiet frequency, then key `text` after a short lead-in.
    fn key(&mut self, text: &str) -> Result<(), String> {
        self.line = self.line.max(highest_line(text).map_or(0, |n| n + 1));
        let text = with_codes(text, &self.book);
        self.wait_quiet()?;
        self.tx_index += 1;
        self.in_tx = true;
        self.tx_block = 0;
        self.idle(REACTION)?;
        let (body, burst) = match text.strip_suffix(" ~") {
            Some(t) => (t, true),
            None => (text.as_str(), false),
        };
        let mut k = Keyer::new(SAMPLE_RATE, PITCH_HZ + self.fist.offset_hz, self.fist.wpm);
        k.amplitude = AMPLITUDE;
        k.jitter = self.fist.jitter;
        k.gap_stretch = self.fist.gap_stretch;
        k.seed = mix(&[self.fist.seed, self.tx_index]);
        let mut audio = k.render(body, 0.0);
        if burst {
            audio.extend(std::iter::repeat_n(0.0, SAMPLE_RATE as usize * 3 / 2));
            audio.extend(k.render("E", 0.0));
        }
        self.note(format!("OP    {text}"));
        self.sent.push(body.to_string());
        self.keying.extend(audio);
        while !self.keying.is_empty() {
            self.tick()?;
        }
        self.in_tx = false;
        Ok(())
    }

    /// Wait for the node to start its next window (after `tunes` tuner cycles):
    /// its tune and, after a tune that matched, its `DE <call>`. The node takes no
    /// audio until both are over, so once it has taken a block sent after the tune
    /// started, it is listening again. Whatever it keyed meanwhile is the window's
    /// ID, not an answer.
    fn window_start(&mut self, tunes: u32) -> Result<(), String> {
        self.radio.ic7300()?;
        while self.radio.tunes() == tunes {
            self.tick()?;
        }
        self.tick()?;
        while self.tx.as_ref().is_some_and(|tx| tx.queued() > 0) {
            if Instant::now() > self.deadline {
                return Err("scenario took too long".into());
            }
            thread::sleep(Duration::from_micros(200));
        }
        self.wait_quiet()?;
        let keyed: Vec<String> = self
            .radio
            .keyed_from(self.heard)
            .into_iter()
            .filter(|k| k.on_air)
            .map(|k| k.text)
            .collect();
        self.heard = self.radio.keyed_from(0).len();
        match keyed.is_empty() {
            true => self.note("NODE  (no ID after the tune)"),
            false => self.note(format!("NODE  {}", keyed.join(" "))),
        }
        Ok(())
    }

    /// Wait for the node's next over: until it ends with `DE <call> K` and the
    /// frequency is quiet, or stops short, or none starts in time.
    fn listen(&mut self) -> Result<Heard, String> {
        let mut since = self.secs();
        let mut last_sound: Option<f32> = None;
        loop {
            self.tick()?;
            let now = self.secs();
            if self.radio.audible() {
                last_sound = Some(now);
                continue;
            }
            let pieces: Vec<_> = self
                .radio
                .keyed_from(self.heard)
                .into_iter()
                .filter(|k| k.on_air)
                .collect();
            // A station ID between chunks is not part of the over: the operator
            // copies around it.
            let id = format!("DE {NODE_CALL}");
            let text = pieces
                .iter()
                .enumerate()
                .filter(|(i, k)| k.text != id || i + 1 == pieces.len())
                .map(|(_, k)| if k.complete { &k.text } else { &k.sent })
                .filter(|t| !t.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let done = !pieces.is_empty()
                && pieces.iter().all(|k| k.complete)
                && text.ends_with(&self.over_end);
            let stalled = last_sound.is_some_and(|t| now - t > CUT_AFTER);
            if done || stalled && !pieces.is_empty() {
                self.heard = self.radio.keyed_from(0).len();
                if std::mem::take(&mut self.deaf) {
                    self.note(format!("NODE  (lost in QRM) {text}"));
                    since = now;
                    last_sound = None;
                    continue;
                }
                if done {
                    self.note(format!("NODE  {text}"));
                    return Ok(Heard::Over(text));
                }
                if text.is_empty() {
                    self.note("NODE  (cut short before the first character)");
                } else {
                    self.note(format!("NODE  {text} (cut short)"));
                }
                return Ok(Heard::Cut(text));
            }
            if stalled {
                // A carrier with no keyed text (the tuner): not an answer.
                last_sound = None;
                since = now;
            }
            if last_sound.is_none() && now - since > NO_REPLY {
                return Ok(Heard::Nothing);
            }
        }
    }

    fn step(&mut self, step: &Step) -> Result<(), String> {
        match step {
            Step::Open { text, read_back } => {
                let tries = self.open_tries();
                for _ in 0..tries {
                    self.key(text)?;
                    match self.listen()? {
                        Heard::Over(o) if o == *read_back => return Ok(()),
                        Heard::Over(o) => {
                            self.note("OP    (read-back wrong)");
                            let no = self.line;
                            self.key(&format!("NO {no} {{{no}}} K"))?;
                            self.listen()?;
                            return Err(format!("read-back {o:?}, expected {read_back:?}"));
                        }
                        Heard::Cut(_) | Heard::Nothing => self.note("OP    (no read-back)"),
                    }
                }
                Err(format!("no read-back to {text:?} after {tries} tries"))
            }
            Step::Say { text, expect } => {
                let tries = if expect.is_some() { TRIES } else { 1 };
                for _ in 0..tries {
                    self.key(text)?;
                    match (self.listen()?, expect) {
                        (Heard::Over(o), Some(e)) if o == *e => return Ok(()),
                        (Heard::Over(o), Some(e)) => {
                            return Err(format!("heard {o:?}, expected {e:?}"))
                        }
                        (Heard::Over(o), None) => {
                            return Err(format!("heard {o:?}, expected silence"))
                        }
                        (Heard::Cut(o), None) => {
                            return Err(format!("heard {o:?} (cut), expected silence"))
                        }
                        (Heard::Nothing, None) => {
                            self.note("NODE  (silence)");
                            return Ok(());
                        }
                        (_, Some(_)) => self.note("OP    (no answer)"),
                    }
                }
                Err(format!("no answer to {text:?} after {tries} tries"))
            }
            Step::Unanswered { text, tries } => {
                for _ in 0..*tries {
                    self.key(text)?;
                    match self.listen()? {
                        Heard::Over(o) => return Err(format!("heard {o:?}, expected no answer")),
                        Heard::Cut(_) => {}
                        Heard::Nothing => self.note("NODE  (silence)"),
                    }
                }
                Ok(())
            }
            Step::MissNext => {
                self.deaf = true;
                Ok(())
            }
            Step::Inject(f) => {
                self.note(format!("RADIO fault {f:?}"));
                self.radio.ic7300()?.inject(f.clone());
                Ok(())
            }
            Step::SetSwr(swr) => {
                self.note(format!("RADIO SWR now {swr}"));
                self.radio.ic7300()?.configure(|c| c.swr = *swr);
                Ok(())
            }
            Step::ClearStuck => {
                self.note("RADIO stuck transmit cleared (hardware timer)");
                match &self.radio {
                    AirRadio::Ic7300(r) => r.clear_stuck(),
                    AirRadio::Keyer(k) => k.clear_stuck(),
                }
                Ok(())
            }
            Step::Wait(s) => self.idle(*s),
            Step::WaitReceive => {
                let (until, t0) = (self.secs() + RECEIVE_WAIT, Instant::now());
                while self.radio.transmitting() {
                    if self.secs() > until && t0.elapsed() > RECEIVE_WAIT_REAL {
                        return Err(format!("radio still on transmit after {RECEIVE_WAIT} s"));
                    }
                    self.tick()?;
                }
                self.note("RADIO back on receive");
                Ok(())
            }
            Step::NextWindow => {
                let tunes = self.radio.tunes();
                self.window_start(tunes)?;
                self.note("NODE  (that was the next window)");
                Ok(())
            }
            Step::Panel(p) => {
                self.note(format!("RADIO front panel: {p:?}"));
                let radio = self.radio.ic7300()?;
                match *p {
                    Panel::Dial(hz) => radio.turn_dial(hz),
                    Panel::Mode(mode, filter) => radio.select_mode(mode, filter),
                    Panel::Split(tx_hz) => radio.set_split(tx_hz),
                    Panel::DeltaTx(on) => radio.set_delta_tx(on),
                }
                Ok(())
            }
            Step::FirstTry => {
                self.first_try = true;
                Ok(())
            }
            Step::Tunes(n) => match self.radio.tunes() {
                t if t == *n => Ok(()),
                t => Err(format!("{t} tuner cycles so far, expected {n}")),
            },
            Step::Exchange(x) => self.exchange(x),
            Step::Keyer(f) => {
                self.note(format!("RADIO {f}"));
                self.radio.keyer()?.fault(f);
                Ok(())
            }
        }
    }

    /// How many times the next open may be keyed.
    fn open_tries(&mut self) -> u32 {
        match std::mem::take(&mut self.first_try) {
            true => 1,
            false => TRIES,
        }
    }

    /// Work `x` on fresh lines, starting over after a wrong read-back.
    fn exchange(&mut self, x: &Exchange) -> Result<(), String> {
        for round in 0..=x.restarts {
            if round > 0 {
                self.restarts += 1;
                self.note("OP    (starting over on fresh lines)");
            }
            let (open, commit) = (self.line, self.line + 1);
            self.line += 2;
            let text = format!("{FIELD_CALL} {open} {{{open}}} {} K", x.request);
            let read_back = x.read_back.replace("{open}", &open.to_string());
            let mut heard = None;
            let tries = self.open_tries();
            for t in 0..tries {
                self.extra += u32::from(round > 0 || t > 0);
                self.key(&text)?;
                match self.listen()? {
                    Heard::Over(o) => {
                        heard = Some(o);
                        break;
                    }
                    Heard::Cut(_) | Heard::Nothing => self.note("OP    (no read-back)"),
                }
            }
            let Some(heard) = heard else {
                return Err(format!("no read-back to {text:?} after {tries} tries"));
            };
            if heard != read_back {
                self.wrong_read_backs += 1;
                self.note(format!("OP    (read-back wrong, expected {read_back:?})"));
                // Abort it; a fresh open would replace it anyway, so an unheard NO
                // is not repeated for long.
                let no = de("R NO");
                for _ in 0..2 {
                    self.extra += 1;
                    self.key(&format!("NO {commit} {{{commit}}} K"))?;
                    if matches!(self.listen()?, Heard::Over(o) if o == no) {
                        break;
                    }
                }
                continue;
            }
            let ok = format!("OK {commit} {{{commit}}} K");
            let result = x.result.replace("{commit}", &commit.to_string());
            for t in 0..TRIES {
                self.extra += u32::from(t > 0);
                self.key(&ok)?;
                match self.listen()? {
                    Heard::Over(o) if o == result => {
                        self.exchanges_done += 1;
                        return Ok(());
                    }
                    Heard::Over(o) => return Err(format!("heard {o:?}, expected {result:?}")),
                    Heard::Cut(_) | Heard::Nothing => self.note("OP    (no answer)"),
                }
            }
            return Err(format!("no answer to {ok:?} after {TRIES} tries"));
        }
        Err(format!("read-back wrong {} times: gave up", x.restarts + 1))
    }

    /// Run the script. The first failed step ends it, as an operator would give up.
    fn operate(&mut self, script: &[Step]) -> Vec<String> {
        let mut failures = Vec::new();
        // The node tunes at the start of its window and identifies; wait it out,
        // as the operating guide says. A node that starts inhibited does neither.
        let start = match self.tune_at_start {
            true => self.window_start(0),
            false => Ok(()),
        };
        if let Err(e) = start {
            return vec![e];
        }
        for (i, step) in script.iter().enumerate() {
            if let Err(e) = self.step(step) {
                failures.push(format!("step {}: {e}", i + 1));
                break;
            }
        }
        // Anything the node keys late still shows up in the keyed check.
        if let Err(e) = self.idle(TRAIL).and_then(|_| self.wait_quiet()) {
            failures.push(e);
        }
        failures
    }
}

/// Mix in the mock IC-7300's keying over `from..to`, the block's radio time.
fn add_sidetone(radio: &MockRadio, samples: &mut [f32], from: Duration, to: Duration) {
    let n = samples.len();
    let w = 2.0 * std::f32::consts::PI * PITCH_HZ / SAMPLE_RATE as f32;
    for (a, b) in radio.key_down_between(from, to) {
        let start = from.as_secs_f32() * SAMPLE_RATE as f32;
        for (i, s) in samples
            .iter_mut()
            .enumerate()
            .take(at(from, b, n).min(n))
            .skip(at(from, a, n))
        {
            *s += SIDETONE * (w * (start + i as f32)).sin();
        }
    }
}

fn config(s: &Scenario, dir: &Path, scale: f32) -> Result<Config> {
    let key = dir.join("test.key");
    std::fs::write(&key, TEST_KEY)?;
    // The paths are set after parsing, so that no character in them (a backslash,
    // a quote) has to be escaped for TOML.
    let mut cfg: Config = toml::from_str(&format!(
        r#"
        state_dir = ""
        [station]
        node_call = "{NODE_CALL}"
        field_calls = ["{FIELD_CALL}"]
        frequency_hz = {FREQUENCY_HZ}
        serial_port = "mock"
        power_watts = 40
        key_speed_wpm = {wpm}
        chunk_chars = {chunk}
        chunk_pause_ms = {pause}
        {rig}
        [audio]
        sample_rate = {SAMPLE_RATE}
        pitch_hz = {PITCH_HZ}
        [auth]
        key_file = ""
        [schedule]
        {schedule}
        check_minutes = {check}
        retune_minutes = {retune}
        [[contacts]]
        name = "MOM"
        address = "mom@example.com"
        [[contacts]]
        name = "BOB"
        address = "bob@example.com"
        [weather]
        default_grid = "{WX_DEFAULT_GRID}"
        user_agent = "hfnode selftest"
        [[weather.presets]]
        number = 1
        grid = "DL89IG"
        [[weather.presets]]
        number = 2
        grid = "DL89ME"
        {keyer}
        "#,
        wpm = s.node.key_wpm,
        chunk = s.node.chunk_chars,
        pause = ((2000.0 / scale) as u64).max(1),
        schedule = match s.node.schedule {
            Some((every, window)) => format!(
                "always = false\n        every_minutes = {every}\n        window_minutes = {window}"
            ),
            None => "always = true".into(),
        },
        check = s.node.check_minutes,
        retune = s.node.retune_minutes,
        // Room for 30 characters at 18 wpm, which the box needs.
        rig = match s.radio.keyer {
            true => "rig = \"keyer\"\n        max_key_seconds = 50",
            false => "",
        },
        keyer = match s.radio.keyer {
            true => "[keyer]\n        commissioned = \"done\"",
            false => "",
        },
    ))?;
    cfg.state_dir = dir.join("state");
    cfg.auth.key_file = key;
    cfg.validate()?;
    SeqStore::new(cfg.state_dir.join("last_seq")).save(START_SEQ)?;
    Ok(cfg)
}

/// The station's timing, divided by the time scale.
fn station_config(cfg: &Config, scale: f32) -> StationConfig {
    let mut sc = StationConfig::from_config(&cfg.station);
    for d in [
        &mut sc.max_key,
        &mut sc.swr_delay,
        &mut sc.swr_window,
        &mut sc.stuck_margin,
        &mut sc.tune_timeout,
        &mut sc.poll,
        &mut sc.id_interval,
    ] {
        *d = d.div_f32(scale);
    }
    sc
}

fn inbox(setup: &NodeSetup) -> Vec<Message> {
    setup
        .inbox
        .iter()
        .enumerate()
        .map(|(i, (from, text))| Message {
            id: i as u64 + 1,
            from: from.clone(),
            received_unix: 1_700_000_000 + i as u64,
            source_id: format!("selftest:{i}"),
            raw: text.clone(),
            screened: Some(text.clone()),
            state: MsgState::Ready,
        })
        .collect()
}

/// Match the radio's keyer messages against the expected overs.
fn match_overs(keyed: &[civ::mock::Keyed], expect: &[Over]) -> Result<(), String> {
    let pieces: Vec<_> = keyed.iter().filter(|k| k.on_air).collect();
    let join = |a: &str, b: &str| match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a} {b}"),
    };
    let mut i = 0;
    for (n, over) in expect.iter().enumerate() {
        let mut got = String::new();
        match over {
            Over::Full(t) => loop {
                let Some(p) = pieces.get(i) else {
                    return Err(format!("over {}: keyed {got:?}, expected {t:?}", n + 1));
                };
                got = join(&got, &p.text);
                i += 1;
                if !p.complete {
                    return Err(format!("over {}: cut short at {:?}", n + 1, p.sent));
                }
                if got == *t {
                    break;
                }
                if !t.starts_with(&format!("{got} ")) {
                    return Err(format!("over {}: keyed {got:?}, expected {t:?}", n + 1));
                }
            },
            Over::Cut(t) => {
                while let Some(p) = pieces.get(i) {
                    let more = join(&got, if p.complete { &p.text } else { &p.sent });
                    if !t.starts_with(&more) || more.len() >= t.len() {
                        if i == 0 || got.is_empty() {
                            return Err(format!(
                                "over {}: keyed {more:?}, expected part of {t:?}",
                                n + 1
                            ));
                        }
                        break;
                    }
                    got = more;
                    i += 1;
                    if !p.complete {
                        break;
                    }
                }
            }
        }
    }
    match pieces.get(i..) {
        Some(rest) if !rest.is_empty() => Err(format!(
            "keyed more than expected: {:?}",
            rest.iter().map(|p| p.text.as_str()).collect::<Vec<_>>()
        )),
        _ => Ok(()),
    }
}

fn describe(keyed: &[civ::mock::Keyed]) -> String {
    keyed
        .iter()
        .filter(|k| k.on_air)
        .map(|k| {
            if k.complete {
                format!("{:?}", k.text)
            } else {
                format!("{:?} (cut: {:?})", k.text, k.sent)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn check(name: &'static str, pass: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        pass,
        detail: detail.into(),
    }
}

/// What the node thread hands back when `node::run` returns.
type Finished<R> = (anyhow::Result<()>, Station<R>, Session, FakeServices);

/// Run one scenario at `scale` times real time.
pub fn run(s: &Scenario, scale: f32) -> Outcome {
    let t0 = Instant::now();
    let mut out = Outcome {
        scenario: s.name.clone(),
        checks: Vec::new(),
        wall: Duration::ZERO,
        radio_time: Duration::ZERO,
        transcript: Vec::new(),
        facts: Facts::default(),
    };
    if let Err(e) = run_inner(s, scale, &mut out) {
        out.checks.push(check("setup", false, format!("{e:#}")));
    }
    out.wall = t0.elapsed();
    out
}

fn run_inner(s: &Scenario, scale: f32, out: &mut Outcome) -> Result<()> {
    if s.radio.keyer {
        return any_radio::run_inner(s, scale, out);
    }
    let scale = scale.clamp(1.0, MAX_SCALE);
    let dir = Scratch::new(&s.name).context("scratch directory")?;
    let cfg = config(s, &dir.0, scale)?;
    let book = node::load_codebook(&cfg)?;
    let radio = MockRadio::new(MockConfig {
        time_scale: scale,
        echo: s.radio.echo,
        swr: s.radio.swr,
        foldback: s.radio.foldback,
        menu: s.radio.menu,
        ..MockConfig::default()
    });
    // As `hfnode run` opens the radio, on a station past every bring-up stage: the
    // read-only preflight, with the radio's Time-Out Timer required, before
    // anything is written.
    let opened = commissioning::open_for(Stage::Done, Action::Run, cfg.station.power_watts, || {
        Ok(Ic7300::with_port(radio.port(), cfg.station.civ_address))
    });
    let inner = match (opened, s.expect.refused) {
        (Ok(rig), None) => rig,
        (Err(e), None) => return Err(e.context("opening the mock radio")),
        (Ok(_), Some(item)) => {
            out.checks.push(check(
                "preflight",
                false,
                format!("passed; expected it to refuse to start on {item}"),
            ));
            return Ok(());
        }
        (Err(e), Some(item)) => {
            refused_checks(&radio, item, &format!("{e:#}"), out);
            return Ok(());
        }
    };
    for f in &s.radio.faults {
        radio.inject(f.clone());
    }
    let rig = TimeScaled { inner, scale };
    inhibit_at_start(s, &cfg)?;
    let station = Station::new(
        rig,
        station_config(&cfg, scale),
        Some(cfg.state_dir.join("health.csv")),
    );
    // As `hfnode run` registers its email alerts.
    let (alert_tx, alerts) = mpsc::channel();
    station.notify_inhibit(alert_tx);
    station.configure().context("configuring the mock radio")?;
    let (session, svc) = node_side(s, &cfg, scale)?;

    let (tx, rx) = audio::queue(usize::MAX);
    let clock_radio = radio.clone();
    // The listening schedule runs in radio time.
    let done = spawn_node(&cfg, station, session, svc, rx, move || {
        CLOCK_START + clock_radio.now().as_secs()
    });
    let mut air = air(s, AirRadio::Ic7300(radio.clone()), Some(tx), book, scale);
    let Some((station, session, svc)) = operate(s, &mut air, &done, out) else {
        return Ok(());
    };
    let inhibited = station.tx_inhibited();
    // How the node left the radio, before dropping the station forces receive (as
    // on shutdown) and would hide it.
    let left = radio.report();
    let settings = radio.settings();
    let stops = radio
        .commands()
        .iter()
        .filter(|(_, body)| body[..] == [0x17, 0xFF])
        .count();
    drop(station);
    let notices: Vec<_> = alerts.try_iter().collect();
    let r = radio.report();
    out.radio_time = r.now;

    let e = &s.expect;
    out.checks.push(keyed_check(&r.keyed, &e.keyed));

    let tunes_at: Vec<Duration> = radio
        .commands()
        .iter()
        .filter(|(_, b)| b[..] == [0x1C, 0x01, 0x02])
        .map(|(t, _)| *t)
        .collect();
    out.checks
        .push(station_id_check(&r.keyed, &tunes_at, e, scale));
    gateway_checks(&cfg, e, &session, &svc, out)?;

    out.checks.push(check(
        "ci-v",
        r.violations.is_empty(),
        if r.violations.is_empty() {
            format!(
                "{} commands, no protocol violations",
                radio.commands().len()
            )
        } else {
            r.violations
                .iter()
                .map(|v| format!("{:02X?}: {}", v.bytes, v.reason))
                .collect::<Vec<_>>()
                .join("; ")
        },
    ));

    out.checks.push(settings_check(&cfg, &settings));
    out.checks.push(forced_receive_check(e, stops, "17 FF"));
    out.checks
        .push(safety(&cfg, e, &left, &r, &settings, inhibited, scale));
    out.checks.push(alert_check(e, &s.node, &notices));
    reception_checks(&cfg, e, out);
    Ok(())
}

/// The node refused to start: the preflight named `item`, and only its reads went
/// to the radio.
fn refused_checks(radio: &MockRadio, item: &str, error: &str, out: &mut Outcome) {
    out.checks.push(check(
        "preflight",
        error.contains(item),
        format!("refused to start: {error}"),
    ));
    let cmds = radio.commands();
    let writes: Vec<_> = cmds.iter().filter(|(_, b)| !is_read(b)).collect();
    let r = radio.report();
    out.radio_time = r.now;
    let mut bad = Vec::new();
    if !writes.is_empty() {
        bad.push(format!("wrote {writes:02X?}"));
    }
    if !r.keyed.is_empty() || !r.transmissions.is_empty() || r.tunes > 0 {
        bad.push(format!(
            "{} pieces keyed, {} transmissions, {} tunes",
            r.keyed.len(),
            r.transmissions.len(),
            r.tunes
        ));
    }
    bad.extend(
        r.violations
            .iter()
            .map(|v| format!("{:02X?}: {}", v.bytes, v.reason)),
    );
    out.checks.push(check(
        "nothing written",
        bad.is_empty(),
        if bad.is_empty() {
            format!("{} reads, no writes, nothing keyed", cmds.len())
        } else {
            bad.join("; ")
        },
    ));
}

/// Leave the inhibit file a fault before a restart would, if the scenario says.
fn inhibit_at_start(s: &Scenario, cfg: &Config) -> Result<()> {
    if s.node.inhibited_at_start {
        std::fs::write(
            cfg.state_dir.join(crate::station::INHIBIT_FILE),
            format!("{CLOCK_START} radio not confirmed on receive (no reply from radio)\n"),
        )?;
    }
    Ok(())
}

/// The node's session, with its timeouts scaled, and the fake gateways.
fn node_side(s: &Scenario, cfg: &Config, scale: f32) -> Result<(Session, FakeServices)> {
    let mut sc = node::session_config(cfg);
    sc.pending_timeout = sc.pending_timeout.div_f32(scale);
    sc.again_window = sc.again_window.div_f32(scale);
    let session = node::build_session_with(cfg, sc)?;
    let svc = FakeServices {
        inbox: inbox(&s.node),
        fail_send: s.node.fail_send,
        no_route: s.node.no_route,
        weather_error: s.node.weather_error.clone(),
        ..FakeServices::default()
    };
    Ok((session, svc))
}

/// Run the node as `hfnode run` does, on a thread of its own, its listening
/// schedule on `clock`; it returns when its audio closes.
fn spawn_node<R: Rig + Send + 'static>(
    cfg: &Config,
    station: Station<R>,
    session: Session,
    svc: FakeServices,
    rx: BlockReceiver,
    clock: impl Fn() -> u64 + Send + 'static,
) -> mpsc::Receiver<Finished<R>> {
    let (done_tx, done_rx) = mpsc::channel::<Finished<R>>();
    let node_cfg = cfg.clone();
    thread::spawn(move || {
        let (mut station, mut session, mut svc) = (station, session, svc);
        let end =
            node::run_with_clock(&node_cfg, &mut station, &rx, &mut session, &mut svc, &clock);
        drop(rx);
        let _ = done_tx.send((end, station, session, svc));
    });
    done_rx
}

/// The operator for `s`, on `radio`.
fn air(s: &Scenario, radio: AirRadio, tx: Option<BlockSender>, book: CodeBook, scale: f32) -> Air {
    let snr = s.fist.snr_db.unwrap_or(f32::INFINITY);
    let tune_at_start = !s.node.inhibited_at_start && !s.radio.keyer;
    Air {
        radio,
        tx,
        book,
        fist: s.fist.clone(),
        sigma: Noise::sigma_for_snr(AMPLITUDE, snr, SAMPLE_RATE, 2500.0),
        sidetone: s.radio.sidetone,
        nudge_hz: s.radio.dial_nudges.then_some(FREQUENCY_HZ),
        block_real: Duration::from_secs_f32(BLOCK_SECS / scale),
        next_due: Instant::now(),
        deadline: Instant::now() + BUDGET.div_f32(scale) + Duration::from_secs(30),
        blocks: 0,
        keying: VecDeque::new(),
        tx_index: 0,
        in_tx: false,
        tx_block: 0,
        heard: 0,
        deaf: false,
        over_end: format!("DE {NODE_CALL} K"),
        log: Vec::new(),
        line: START_SEQ + 1,
        sent: Vec::new(),
        exchanges_done: 0,
        wrong_read_backs: 0,
        restarts: 0,
        extra: 0,
        tune_at_start,
        first_try: false,
        tunes_noted: 0,
    }
}

/// Work the script, close the node's audio and wait for `node::run` to return:
/// the `script` and `ended` checks, and what the node thread handed back.
fn operate<R: Rig + 'static>(
    s: &Scenario,
    air: &mut Air,
    done: &mpsc::Receiver<Finished<R>>,
    out: &mut Outcome,
) -> Option<(Station<R>, Session, FakeServices)> {
    let failures = air.operate(&s.script);
    // Closing the audio ends node::run.
    air.close_audio();
    out.transcript = std::mem::take(&mut air.log);
    out.facts = Facts {
        operator_sent: std::mem::take(&mut air.sent),
        exchanges_done: air.exchanges_done,
        wrong_read_backs: air.wrong_read_backs,
        restarts: air.restarts,
        extra_transmissions: air.extra,
        script_failures: failures.clone(),
        ..Facts::default()
    };
    out.checks.push(check(
        "script",
        failures.is_empty(),
        if failures.is_empty() {
            format!("{} steps", s.script.len())
        } else {
            failures.join("; ")
        },
    ));

    let Ok((end, station, session, svc)) = done.recv_timeout(Duration::from_secs(30)) else {
        out.checks.push(check(
            "ended",
            false,
            "node::run did not return within 30 s",
        ));
        return None;
    };
    let ended = matches!(&end, Err(e) if e.to_string().contains("audio source ended"));
    out.checks.push(check(
        "ended",
        ended,
        match &end {
            Ok(()) => "node::run returned Ok".to_string(),
            Err(e) => format!("{e:#}"),
        },
    ));
    Some((station, session, svc))
}

/// The node's overs, its station IDs aside, against `expect`.
fn keyed_check(keyed: &[civ::mock::Keyed], expect: &[Over]) -> Check {
    let id = format!("DE {NODE_CALL}");
    let overs: Vec<civ::mock::Keyed> = keyed.iter().filter(|k| k.text != id).cloned().collect();
    let matched = match_overs(&overs, expect);
    check(
        "keyed",
        matched.is_ok(),
        match matched {
            Ok(()) => format!("{} overs: {}", expect.len(), describe(keyed)),
            Err(why) => format!("{why}; radio keyed: {}", describe(keyed)),
        },
    )
}

/// What the gateways did, and the last line used, in memory and on disk.
fn gateway_checks(
    cfg: &Config,
    e: &Expect,
    session: &Session,
    svc: &FakeServices,
    out: &mut Outcome,
) -> Result<()> {
    let last_seq = session.last_seq();
    let stored = SeqStore::new(cfg.state_dir.join("last_seq")).load()?;
    out.facts.sent = svc.sent.clone();
    out.facts.read = svc.read.clone();
    let weather: Vec<String> = e.weather.clone();
    let gateway_ok = svc.sent == e.sent && svc.read == e.read && svc.weather_calls == weather;
    out.checks.push(check(
        "gateway",
        gateway_ok,
        format!(
            "sent {:?}, read {:?}, weather {:?}{}",
            svc.sent,
            svc.read,
            svc.weather_calls,
            if gateway_ok {
                String::new()
            } else {
                format!(
                    "; expected sent {:?}, read {:?}, weather {:?}",
                    e.sent, e.read, weather
                )
            }
        ),
    ));

    out.checks.push(check(
        "last_seq",
        last_seq == e.last_seq && stored == e.last_seq,
        format!("{last_seq} (stored {stored}), expected {}", e.last_seq),
    ));
    Ok(())
}

/// Whether the node made the radio stop (`what`: how) while it ran, as it must
/// after any fault and never otherwise.
fn forced_receive_check(e: &Expect, stops: usize, what: &str) -> Check {
    check(
        "forced receive",
        (stops > 0) == e.forced_receive,
        format!(
            "{what} sent {stops} times while the node ran, expected {}",
            if e.forced_receive { "some" } else { "none" }
        ),
    )
}

/// What the node decoded: everything expected, and never itself.
fn reception_checks(cfg: &Config, e: &Expect, out: &mut Outcome) {
    let rx_log = std::fs::read_to_string(cfg.state_dir.join("rx.log")).unwrap_or_default();
    // `<unix time>,<text>`.
    out.facts.received = rx_log
        .lines()
        .map(|l| l.split_once(',').map_or(l, |(_, t)| t).to_string())
        .collect();
    let unheard: Vec<&String> = e
        .heard
        .iter()
        .filter(|h| !out.facts.received.iter().any(|r| r.contains(h.as_str())))
        .collect();
    if !e.heard.is_empty() {
        out.checks.push(check(
            "heard",
            unheard.is_empty(),
            if unheard.is_empty() {
                format!("rx.log has all {} expected receptions", e.heard.len())
            } else {
                format!("not in rx.log: {unheard:?}")
            },
        ));
    }
    let own = format!("DE {NODE_CALL}");
    let echoes: Vec<&str> = rx_log.lines().filter(|l| l.contains(&own)).collect();
    out.checks.push(check(
        "self-decode",
        echoes.is_empty(),
        if echoes.is_empty() {
            format!(
                "{} receptions, none of the node's own",
                rx_log.lines().count()
            )
        } else {
            format!("node decoded itself: {echoes:?}")
        },
    ));
}

/// Whether a keyer piece ends with the node's callsign: an ID, or the end of an over.
fn ends_with_call(text: &str) -> bool {
    let w: Vec<&str> = text.split_whitespace().collect();
    match w[..] {
        [.., c] if c == NODE_CALL => true,
        [.., c, o] => c == NODE_CALL && ["K", "KN"].contains(&o),
        _ => false,
    }
}

/// The node's station IDs: `DE <call>` as the first piece after each tune that
/// matched (`tunes_at`, radio time), the expected number inside overs, and never
/// longer than the station's ID interval from the start of a transmission (or an
/// ID) to the end of the next ID. A piece cut short by a fault ends the stretch.
fn station_id_check(
    keyed: &[civ::mock::Keyed],
    tunes_at: &[Duration],
    e: &Expect,
    scale: f32,
) -> Check {
    let id = format!("DE {NODE_CALL}");
    let on_air: Vec<&civ::mock::Keyed> = keyed.iter().filter(|k| k.on_air).collect();
    let after_tune = |i: usize| {
        let next = tunes_at.get(i + 1).copied().unwrap_or(Duration::MAX);
        on_air
            .iter()
            .find(|k| k.accepted >= tunes_at[i] && k.accepted < next)
            .is_some_and(|k| k.text == id && k.complete)
    };
    let ids = (0..tunes_at.len()).filter(|&i| after_tune(i)).count() as u32;
    let mid_ids = on_air.iter().filter(|k| k.text == id).count() as u32 - ids;
    let (mut longest, mut from) = (Duration::ZERO, None);
    for k in &on_air {
        if !k.complete {
            from = None;
            continue;
        }
        let start = *from.get_or_insert(k.start);
        if ends_with_call(&k.text) {
            longest = longest.max(k.end.saturating_sub(start));
            from = None;
        }
    }
    let limit = (crate::station::ID_INTERVAL + Duration::from_secs_f32(1.0 + 0.02 * scale))
        .min(Duration::from_secs(600));
    let pass = ids == e.ids && mid_ids == e.mid_ids && longest <= limit;
    check(
        "station ID",
        pass,
        format!(
            "{ids} after {} tunes (expected {}), {mid_ids} inside overs (expected {}), \
             at most {:.0} s without one (limit {:.0} s)",
            tunes_at.len(),
            e.ids,
            e.mid_ids,
            longest.as_secs_f32(),
            limit.as_secs_f32()
        ),
    )
}

/// The radio as the node left it: on the configured frequency, in CW with FIL1, semi
/// break-in, and the power, keyer speed and break-in delay the node sets. Levels are
/// read back on the mock's own scales (p. 19-3): 0-100 W, 6-48 wpm, 2-13 dots.
fn settings_check(cfg: &Config, s: &Settings) -> Check {
    let st = &cfg.station;
    let watts = s.rf_power_level as f32 * 100.0 / 255.0;
    let wpm = 6.0 + s.key_speed_level as f32 * 42.0 / 255.0;
    let dots = 2.0 + s.break_in_delay_level as f32 * 11.0 / 255.0;
    let want_wpm = st.key_speed_wpm.clamp(6, 48) as f32;
    let mut bad = Vec::new();
    if s.frequency_hz != st.frequency_hz {
        bad.push(format!("{} Hz, not {}", s.frequency_hz, st.frequency_hz));
    }
    if (s.mode, s.filter) != (0x03, 0x01) {
        bad.push(format!(
            "mode {:02X} filter {:02X}, not CW FIL1",
            s.mode, s.filter
        ));
    }
    if s.break_in != 0x01 {
        bad.push(format!("BK-IN {:02X}, not semi", s.break_in));
    }
    // Within half a level of what was asked for.
    if (watts - st.power_watts as f32).abs() > 0.25 {
        bad.push(format!("{watts:.1} W, not {}", st.power_watts));
    }
    if (wpm - want_wpm).abs() > 0.1 {
        bad.push(format!("keyer {wpm:.1} wpm, not {want_wpm}"));
    }
    if (dots - BREAK_IN_DOTS).abs() > 0.03 {
        bad.push(format!(
            "break-in delay {dots:.2} dots, not {BREAK_IN_DOTS}"
        ));
    }
    let detail = format!(
        "{} Hz, CW FIL{}, {watts:.0} W, {wpm:.1} wpm, semi break-in {dots:.1} dots",
        s.frequency_hz, s.filter
    );
    if bad.is_empty() {
        check("settings", true, detail)
    } else {
        check("settings", false, format!("{}; {detail}", bad.join("; ")))
    }
}

/// The owner is told of an inhibit exactly once: when it latches, or at start-up
/// if the node started inhibited; and never without one.
fn alert_check(e: &Expect, node: &NodeSetup, notices: &[InhibitNotice]) -> Check {
    let pass = match notices {
        [] => !e.inhibited,
        [n] => e.inhibited && n.from_file == node.inhibited_at_start && !n.reason.is_empty(),
        _ => false,
    };
    let seen: Vec<String> = notices
        .iter()
        .map(|n| {
            format!(
                "{}: {}",
                if n.from_file {
                    "at start-up"
                } else {
                    "latched"
                },
                n.reason
            )
        })
        .collect();
    check(
        "alert",
        pass,
        format!(
            "{} inhibit notice(s){}, expected {}",
            notices.len(),
            if seen.is_empty() {
                String::new()
            } else {
                format!(" ({})", seen.join("; "))
            },
            match (e.inhibited, node.inhibited_at_start) {
                (false, _) => "none",
                (true, false) => "one, when it latches",
                (true, true) => "one, at start-up",
            }
        ),
    )
}

/// Bounds that hold whatever the scenario: transmit runs, duty, receive at the end
/// (as the node left it, in `left`), and the tuner cycles expected.
fn safety(
    cfg: &Config,
    e: &Expect,
    left: &Report,
    r: &Report,
    s: &Settings,
    inhibited: bool,
    scale: f32,
) -> Check {
    let mut bad = Vec::new();
    let max_key = Duration::from_secs(cfg.station.max_key_seconds);
    if r.max_key_down > max_key {
        bad.push(format!(
            "key down for {:.1} s at once (limit {} s)",
            r.max_key_down.as_secs_f32(),
            max_key.as_secs()
        ));
    }
    // A radio jammed on transmit is ended by the hardware timer, not the node.
    if !e.inhibited && r.max_tx > max_key {
        bad.push(format!(
            "on transmit for {:.1} s at once (limit {} s)",
            r.max_tx.as_secs_f32(),
            max_key.as_secs()
        ));
    }
    // The radio stays on transmit after its last element for the break-in delay;
    // anything longer is a stuck transmitter, which the node must end within its
    // stuck margin. Allow for the station's real-time polling and CI-V round trips,
    // which take `scale` times longer in radio time.
    let dot = 1.2 / (6.0 + s.key_speed_level as f32 * 42.0 / 255.0);
    let hang = dot * (2.0 + s.break_in_delay_level as f32 * 11.0 / 255.0);
    let overhang_limit = Duration::from_secs_f32(hang + 0.5 + 0.02 * scale) + STUCK_MARGIN;
    let mut overhang = Duration::ZERO;
    for t in &r.transmissions {
        let end = t.end.unwrap_or(r.now);
        let last = r
            .keyed
            .iter()
            .filter(|k| k.on_air && k.start >= t.start && k.start < end)
            .map(|k| k.end)
            .max();
        if let Some(last) = last {
            overhang = overhang.max(end.saturating_sub(last));
        }
    }
    if !e.inhibited && overhang > overhang_limit {
        bad.push(format!(
            "on transmit {:.1} s after the last element (limit {:.1} s: break-in delay and stuck margin)",
            overhang.as_secs_f32(),
            overhang_limit.as_secs_f32()
        ));
    }
    let duty = r.total_key_down.as_secs_f32() / r.now.as_secs_f32().max(1.0);
    if duty > 0.5 {
        bad.push(format!("key-down duty {:.0}%", duty * 100.0));
    }
    if left.transmitting || left.keyer_busy {
        bad.push("radio not back on receive when the node stopped".into());
    }
    if inhibited != e.inhibited {
        bad.push(format!(
            "transmit inhibited: {inhibited}, expected {}",
            e.inhibited
        ));
    }
    if r.tunes != e.tunes {
        bad.push(format!("tuned {} times, expected {}", r.tunes, e.tunes));
    }
    let detail = format!(
        "longest key-down {:.1} s, longest transmit {:.1} s, {:.1} s on transmit after the last element at most, {} transmissions, {:.0} s on transmit, duty {:.0}%",
        r.max_key_down.as_secs_f32(),
        r.max_tx.as_secs_f32(),
        overhang.as_secs_f32(),
        r.transmissions.len(),
        r.total_tx.as_secs_f32(),
        duty * 100.0
    );
    if bad.is_empty() {
        check("safety", true, detail)
    } else {
        check("safety", false, format!("{}; {detail}", bad.join("; ")))
    }
}

// ---------------------------------------------------------------------------
// The scenarios.

fn rb_tx(seq: u64, dest: &str, text: &str) -> String {
    format!("R {seq} TX {dest} {text} ? DE {NODE_CALL} K")
}

fn de(text: &str) -> String {
    format!("{text} DE {NODE_CALL} K")
}

fn full(texts: &[&String]) -> Vec<Over> {
    texts.iter().map(|t| Over::Full((*t).clone())).collect()
}

fn sent(dest: &str, text: &str) -> Vec<(String, String)> {
    vec![(dest.to_string(), text.to_string())]
}

fn base(name: &str, about: &str) -> Scenario {
    Scenario {
        name: name.into(),
        about: about.into(),
        fist: Fist {
            seed: mix(&[name.len() as u64, name.bytes().map(u64::from).sum()]),
            ..Fist::default()
        },
        node: NodeSetup::default(),
        radio: RadioSetup::default(),
        script: Vec::new(),
        expect: Expect {
            keyed: Vec::new(),
            sent: Vec::new(),
            read: Vec::new(),
            weather: Vec::new(),
            last_seq: START_SEQ,
            tunes: 1,
            forced_receive: false,
            inhibited: false,
            ids: 1,
            mid_ids: 0,
            heard: Vec::new(),
            refused: None,
        },
    }
}

/// A whole TX transaction on lines 42 and 43.
fn tx(name: &str, about: &str, dest: &str, text: &str) -> Scenario {
    let rb = rb_tx(42, dest, text);
    let done = de("SENT 43");
    Scenario {
        script: vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} TX {dest} {text} K"),
                read_back: rb.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(done.clone()),
            },
        ],
        expect: Expect {
            keyed: full(&[&rb, &done]),
            sent: sent(dest, text),
            last_seq: 43,
            ..base(name, about).expect
        },
        ..base(name, about)
    }
}

fn with_fist(mut s: Scenario, f: impl FnOnce(&mut Fist)) -> Scenario {
    f(&mut s.fist);
    s
}

fn with_radio(mut s: Scenario, f: impl FnOnce(&mut RadioSetup)) -> Scenario {
    f(&mut s.radio);
    s
}

/// The read-back long enough to be keyed in two pieces.
const LONG_TEXT: &str = "RUNNING LATE HOME SUN";

/// Every scenario, in the order they run.
pub fn scenarios() -> Vec<Scenario> {
    let mut v = vec![
        tx(
            "tx",
            "TX to a contact: open, read-back, OK, SENT",
            "MOM",
            LONG_TEXT,
        ),
        {
            let mut s = tx(
                "tx-gateway-down",
                "TX whose gateway fails: FAIL 43 GATEWAY, codes still used",
                "BOB",
                "CALL ME",
            );
            s.node.fail_send = true;
            let rb = rb_tx(42, "BOB", "CALL ME");
            let fail = de("FAIL 43 GATEWAY");
            s.script[1] = Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(fail.clone()),
            };
            s.expect.keyed = full(&[&rb, &fail]);
            s.expect.sent = Vec::new();
            s
        },
        {
            let mut s = tx(
                "tx-no-route",
                "TX to a contact the node cannot reach yet: FAIL 43 NO ROUTE",
                "MOM",
                "CALL ME",
            );
            s.node.no_route = true;
            let rb = rb_tx(42, "MOM", "CALL ME");
            let fail = de("FAIL 43 NO ROUTE");
            s.script[1] = Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(fail.clone()),
            };
            s.expect.keyed = full(&[&rb, &fail]);
            s.expect.sent = Vec::new();
            s
        },
    ];

    let rx = |name: &str, about: &str, inbox: &[(&str, &str)], count: &str, result: &str| {
        let rb = de(&format!("R 42 {count} ?"));
        let result = de(result);
        let mut s = base(name, about);
        s.node.inbox = inbox
            .iter()
            .map(|(f, t)| (f.to_string(), t.to_string()))
            .collect();
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} RX K"),
                read_back: rb.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(result.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb, &result]);
        s.expect.last_seq = 43;
        s
    };
    v.push(rx(
        "rx-empty",
        "RX with nothing waiting: 0 MSGS, then NIL",
        &[],
        "0 MSGS",
        "R 43 NIL",
    ));
    v.push({
        let mut s = rx(
            "rx-one",
            "RX with one message, read out and marked read",
            &[("MOM", "DRIVE SAFE CALL WHEN YOU CAN")],
            "1 MSG",
            "NR 1 FM MOM DRIVE SAFE CALL WHEN YOU CAN = A",
        );
        s.expect.read = vec![1];
        s
    });
    v.push({
        let mut s = rx(
            "rx-several",
            "RX with three messages in three 60-character chunks, then AGN B and AGN",
            &[
                ("MOM", "DRIVE SAFE CALL WHEN YOU CAN"),
                ("BOB", "THE GAME WAS POSTPONED TO NEXT SATURDAY AT NOON"),
                ("MOM", "LOVE YOU"),
            ],
            "3 MSGS",
            "NR 1 FM MOM DRIVE SAFE CALL WHEN YOU CAN NR 2 FM BOB THE = A \
             GAME WAS POSTPONED TO NEXT SATURDAY AT NOON NR 3 FM MOM LOVE = B \
             YOU = C",
        );
        let Over::Full(all) = s.expect.keyed[1].clone() else {
            unreachable!()
        };
        let b = de("GAME WAS POSTPONED TO NEXT SATURDAY AT NOON NR 3 FM MOM LOVE = B");
        s.script.push(Step::Say {
            text: "AGN 44 {44} B K".into(),
            expect: Some(b.clone()),
        });
        s.script.push(Step::Say {
            text: "AGN 45 {45} K".into(),
            expect: Some(all.clone()),
        });
        s.expect.keyed.extend(full(&[&b, &all]));
        s.expect.read = vec![1, 2, 3];
        s.expect.last_seq = 45;
        s
    });
    v.push({
        // 20-character chunks so that the cut is reached in a few minutes of
        // keying; the node keys at 25 wpm for the same reason.
        let long = "TEST ".repeat(150);
        let mut s = rx(
            "rx-long",
            "RX of a message too long for one readout: cut to 26 chunks, TRUNCATED, 1 MORE",
            &[("MOM", long.trim()), ("BOB", "SEE YOU SOON")],
            "2 MSGS",
            "",
        );
        s.node.chunk_chars = 20;
        s.node.key_wpm = 25;
        let mut chunks = vec!["NR 1 FM MOM TEST".to_string()];
        chunks.extend(std::iter::repeat_n("TEST TEST TEST TEST".to_string(), 24));
        chunks.push("TRUNCATED 1 MORE".into());
        let text: Vec<String> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = {}", (b'A' + i as u8) as char))
            .collect();
        let result = de(&text.join(" "));
        s.script[1] = Step::Say {
            text: "OK 43 {43} K".into(),
            expect: Some(result.clone()),
        };
        s.expect.keyed[1] = Over::Full(result);
        s.expect.read = vec![1];
        s
    });
    v.push({
        // 13 chunks of at most 20 characters, keyed at 25 wpm to keep it short.
        let text = "TEST ".repeat(50);
        let chunks = protocol::chunk(&format!("NR 1 FM MOM {}", text.trim()), 20);
        let all: Vec<String> = chunks.iter().map(protocol::Chunk::render).collect();
        let mut s = rx(
            "agn-chunk-k",
            "AGN K K asks for chunk K of a 13-chunk readout",
            &[("MOM", text.trim())],
            "1 MSG",
            &all.join(" "),
        );
        s.node.chunk_chars = 20;
        s.node.key_wpm = 25;
        let k = de(&all[10]);
        s.script.push(Step::Say {
            text: "AGN 44 {44} K K".into(),
            expect: Some(k.clone()),
        });
        s.expect.keyed.push(Over::Full(k));
        s.expect.read = vec![1];
        s.expect.last_seq = 44;
        s
    });
    v.push({
        // About 11 minutes of readout at the node's 18 wpm, longer than its 8-minute
        // ID interval: it identifies once between two chunks.
        let words = 260;
        let long = vec!["TEST"; words].join(" ");
        let mut s = rx(
            "rx-station-id",
            "RX readout longer than 8 minutes: DE N0DE on its own between two chunks, \
             which the operator copies around",
            &[("MOM", long.as_str())],
            "1 MSG",
            "",
        );
        let mut chunks = vec![format!("NR 1 FM MOM {}", ["TEST"; 9].join(" "))];
        let mut left = words - 9;
        while left > 0 {
            let n = left.min(12);
            chunks.push(vec!["TEST"; n].join(" "));
            left -= n;
        }
        let text: Vec<String> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = {}", (b'A' + i as u8) as char))
            .collect();
        let result = de(&text.join(" "));
        s.script[1] = Step::Say {
            text: "OK 43 {43} K".into(),
            expect: Some(result.clone()),
        };
        s.expect.keyed[1] = Over::Full(result);
        s.expect.read = vec![1];
        s.expect.mid_ids = 1;
        s
    });
    v.push({
        let inbox: Vec<(&str, &str)> = (0..6)
            .map(|i| (if i % 2 == 0 { "MOM" } else { "BOB" }, "HI"))
            .collect();
        let mut s = rx(
            "rx-max",
            "RX with six messages waiting: five are read out, then 1 MORE",
            &inbox,
            "6 MSGS",
            "NR 1 FM MOM HI NR 2 FM BOB HI NR 3 FM MOM HI NR 4 FM BOB HI = A \
             NR 5 FM MOM HI 1 MORE = B",
        );
        s.expect.read = vec![1, 2, 3, 4, 5];
        s
    });
    v.push({
        let mut s = rx(
            "rx-readout-refused",
            "RX whose readout the radio refuses (NG to every CW message after the read-back): \
             nothing is keyed, so the message stays unread",
            &[("MOM", "DRIVE SAFE CALL WHEN YOU CAN")],
            "1 MSG",
            "",
        );
        s.script[1] = Step::Unanswered {
            text: "OK 43 {43} K".into(),
            tries: 2,
        };
        // After the first tune and ID: the read-back gets through.
        s.script.insert(
            0,
            Step::Inject(Fault::Reply {
                cmd: vec![0x17],
                skip: 1,
                times: 100,
                kind: ReplyFault::Ng,
            }),
        );
        s.expect.keyed.truncate(1);
        s.expect.forced_receive = true;
        s
    });

    // `place` is what the operator keys after WX, `named` what the read-back says
    // after WX, and `grid` the square the forecast is fetched for.
    let wx = |name: &str, about: &str, place: &str, named: &str, grid: &str| {
        let rb = de(&format!("R 42 WX {named} ?"));
        let result = de(&format!("WX {} = A", FakeServices::forecast(grid)));
        let mut s = base(name, about);
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} WX {place} K").replace("  ", " "),
                read_back: rb.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(result.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb, &result]);
        s.expect.weather = vec![grid.to_string()];
        s.expect.last_seq = 43;
        s
    };
    v.push(wx(
        "wx-home",
        "WX with no place: the configured grid, named in the read-back",
        "",
        WX_DEFAULT_GRID,
        WX_DEFAULT_GRID,
    ));
    v.push(wx(
        "wx-grid",
        "WX for a grid square",
        "DL88",
        "DL88",
        "DL88",
    ));
    v.push(wx(
        "wx-grid6",
        "WX for a 6-character grid square",
        "DL88AF",
        "DL88AF",
        "DL88AF",
    ));
    v.push(wx(
        "wx-grid-split",
        "WX for a grid square keyed in two words: rejoined",
        "DL88 AF",
        "DL88AF",
        "DL88AF",
    ));
    v.push(wx(
        "wx-preset",
        "WX for preset 2: the read-back names its number and grid",
        "2",
        "2 DL89ME",
        "DL89ME",
    ));
    v.push({
        let mut s = base(
            "wx-last",
            "WX alone after a preset: the last place confirmed, named in the read-back",
        );
        let rb1 = de("R 42 WX 2 DL89ME ?");
        let rb2 = de("R 44 WX DL89ME ?");
        let result = de(&format!("WX {} = A", FakeServices::forecast("DL89ME")));
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} WX 2 K"),
                read_back: rb1.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(result.clone()),
            },
            Step::Open {
                text: format!("{FIELD_CALL} 44 {{44}} WX K"),
                read_back: rb2.clone(),
            },
            Step::Say {
                text: "OK 45 {45} K".into(),
                expect: Some(result.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb1, &result, &rb2, &result]);
        s.expect.weather = vec!["DL89ME".into(), "DL89ME".into()];
        s.expect.last_seq = 45;
        s
    });
    v.push({
        let mut s = base(
            "wx-unknown-preset",
            "WX for a preset the node does not have: silence, then the same line works",
        );
        let rb = de("R 42 WX 1 DL89IG ?");
        let result = de(&format!("WX {} = A", FakeServices::forecast("DL89IG")));
        s.script = vec![
            Step::Say {
                text: format!("{FIELD_CALL} 42 {{42}} WX 7 K"),
                expect: None,
            },
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} WX 1 K"),
                read_back: rb.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(result.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb, &result]);
        s.expect.weather = vec!["DL89IG".into()];
        s.expect.last_seq = 43;
        s
    });
    v.push({
        let mut s = wx(
            "wx-fail",
            "WX whose forecast cannot be fetched: FAIL 43 WX",
            "",
            WX_DEFAULT_GRID,
            WX_DEFAULT_GRID,
        );
        s.node.weather_error = Some(WxError::Unavailable("network down".into()));
        let fail = de("FAIL 43 WX");
        s.script[1] = Step::Say {
            text: "OK 43 {43} K".into(),
            expect: Some(fail.clone()),
        };
        s.expect.keyed[1] = Over::Full(fail);
        s
    });
    v.push({
        let mut s = wx(
            "wx-no-coverage",
            "WX for a grid outside NWS coverage: FAIL 43 WX NO COVERAGE",
            "IO91",
            "IO91",
            "IO91",
        );
        s.node.weather_error = Some(WxError::NoCoverage);
        let fail = de("FAIL 43 WX NO COVERAGE");
        s.script[1] = Step::Say {
            text: "OK 43 {43} K".into(),
            expect: Some(fail.clone()),
        };
        s.expect.keyed[1] = Over::Full(fail);
        s
    });

    v.push({
        let mut s = base(
            "no-abort",
            "NO on the next line after the read-back: R NO, and an OK after it commits nothing; \
             a bare NO is ignored",
        );
        let rb = rb_tx(42, "MOM", "WRONG WORDS");
        let no = de("R NO");
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} TX MOM WRONG WORDS K"),
                read_back: rb.clone(),
            },
            Step::Say {
                text: "NO K".into(),
                expect: None,
            },
            Step::Say {
                text: "NO 43 {43} K".into(),
                expect: Some(no.clone()),
            },
            Step::Say {
                text: "OK 44 {44} K".into(),
                expect: None,
            },
        ];
        s.expect.keyed = full(&[&rb, &no]);
        s.expect.last_seq = 43;
        s
    });
    v.push({
        let mut s = tx(
            "agn",
            "AGN on the next line repeats the last over, and a lost repeat is asked for again \
             on the same line; a bare AGN and AGN for a chunk that does not exist are ignored",
            "MOM",
            "HOME SUN",
        );
        let done = de("SENT 43");
        s.script.push(Step::MissNext);
        s.script.push(Step::Say {
            text: "AGN 44 {44} K".into(),
            expect: Some(done.clone()),
        });
        s.script.push(Step::Say {
            text: "AGN K".into(),
            expect: None,
        });
        s.script.push(Step::Say {
            text: "AGN 45 {45} C K".into(),
            expect: None,
        });
        s.expect.keyed.extend(full(&[&done, &done]));
        s.expect.last_seq = 45;
        // A clean signal: the AGNs that get silence are copied for certain, and
        // the line the last one used shows it.
        s.fist.snr_db = None;
        s
    });
    // The session's 10-minute windows, in radio time.
    let ten_minutes = 620.0;
    v.push({
        let mut s = tx(
            "pending-timeout",
            "an OK more than 10 minutes after the read-back gets silence; nothing is sent",
            "MOM",
            "HOME SUN",
        );
        s.script.insert(1, Step::Wait(ten_minutes));
        s.script[2] = Step::Say {
            text: "OK 43 {43} K".into(),
            expect: None,
        };
        s.expect.keyed.truncate(1);
        s.expect.sent = Vec::new();
        s.expect.last_seq = 42;
        s
    });
    v.push({
        let mut s = tx(
            "agn-window",
            "AGN and a repeated OK more than 10 minutes after the node's last over get silence",
            "MOM",
            "HOME SUN",
        );
        s.script.push(Step::Wait(ten_minutes));
        for text in ["AGN 44 {44} K", "OK 43 {43} K"] {
            s.script.push(Step::Say {
                text: text.into(),
                expect: None,
            });
        }
        // A clean signal: in noise the decoder's speed drifts over the ten
        // minutes and the first transmissions after it are miscopied, which
        // would also get silence. The AGN's used line shows it was copied.
        s.fist.snr_db = None;
        s.expect.last_seq = 44;
        s
    });
    v.push({
        let mut s = tx(
            "lost-result",
            "SENT is lost: the operator repeats the OK and gets SENT again, the message sent once",
            "MOM",
            "HOME SUN",
        );
        s.script.insert(1, Step::MissNext);
        s.expect.keyed.push(Over::Full(de("SENT 43")));
        s
    });
    // A one-minute listening window: the read-back ends about 39 s into it (after
    // the tune, the ID and the open), so an OK keyed 20 s later is heard and acted
    // on past its end. No noise: noise bursts heard while waiting teach the
    // decoder a wrong speed (it decodes them as dits), which can garble the next
    // OK; these scenarios are about when the node listens.
    let late = |name: &str, about: &str| {
        let mut s = tx(name, about, "MOM", "HOME SUN");
        s.node.schedule = Some((60, 1));
        s.fist.snr_db = None;
        s.script.insert(1, Step::Wait(20.0));
        s
    };
    v.push({
        let mut s = late(
            "lost-result-after-window",
            "the window ends before the OK and its SENT is lost: the node listens on, the \
             repeated OK gets SENT again, the message sent once",
        );
        s.script.insert(2, Step::MissNext);
        s.expect.keyed.push(Over::Full(de("SENT 43")));
        s
    });
    v.push({
        let mut s = late(
            "listening-ends-after-result",
            "past the window's end the node listens on while its result can be repeated (a \
             repeated OK gets SENT again), then stops: an open on fresh lines gets silence",
        );
        let done = de("SENT 43");
        s.script.extend([
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(done.clone()),
            },
            Step::Wait(ten_minutes),
            Step::Say {
                text: format!("{FIELD_CALL} 44 {{44}} TX MOM HOME SUN K"),
                expect: None,
            },
        ]);
        s.expect.keyed.push(Over::Full(done));
        s
    });
    v.push({
        let mut s = tx(
            "fresh-lines",
            "an open on fresh lines replaces the pending one; the old line's OK cannot commit it",
            "MOM",
            "HOME SUN",
        );
        let rb44 = rb_tx(44, "MOM", "HOME SUN");
        let done = de("SENT 45");
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} TX MOM WRONG WORDS K"),
                read_back: rb_tx(42, "MOM", "WRONG WORDS"),
            },
            Step::Open {
                text: format!("{FIELD_CALL} 44 {{44}} TX MOM HOME SUN K"),
                read_back: rb44.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: None,
            },
            Step::Say {
                text: "OK 45 {45} K".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb_tx(42, "MOM", "WRONG WORDS"), &rb44, &done]);
        s.expect.last_seq = 45;
        s
    });
    v.push({
        let mut s = tx(
            "code-in-groups",
            "codes sent in the two printed groups of four letters",
            "MOM",
            "HOME SUN",
        );
        s.script[0] = Step::Open {
            text: format!("{FIELD_CALL} 42 {{42g}} TX MOM HOME SUN K"),
            read_back: rb_tx(42, "MOM", "HOME SUN"),
        };
        s.script[1] = Step::Say {
            text: "OK 43 {43g} K".into(),
            expect: Some(de("SENT 43")),
        };
        s
    });
    v.push({
        let mut s = tx(
            "lost-read-back",
            "the read-back is lost: the operator repeats the open and gets it again, free",
            "MOM",
            "HOME SUN",
        );
        s.script.insert(0, Step::MissNext);
        let rb = rb_tx(42, "MOM", "HOME SUN");
        s.expect.keyed.insert(0, Over::Full(rb));
        s
    });
    v.push({
        let mut s = tx(
            "replayed-code",
            "a used open and a line below last_seq are both ignored",
            "MOM",
            "HOME SUN",
        );
        s.script.push(Step::Say {
            text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K"),
            expect: None,
        });
        s.script.push(Step::Say {
            text: format!("{FIELD_CALL} 41 {{41}} RX K"),
            expect: None,
        });
        s
    });
    v.push({
        let mut s = tx(
            "wrong-code",
            "an open with another line's code is ignored; the right one works",
            "MOM",
            "HOME SUN",
        );
        s.script.insert(
            0,
            Step::Say {
                text: format!("{FIELD_CALL} 42 {{43}} TX MOM HOME SUN K"),
                expect: None,
            },
        );
        s
    });
    v.push({
        let mut s = tx(
            "garbled-callsign",
            "a callsign off by one element is snapped; one far off is ignored",
            "MOM",
            "HOME SUN",
        );
        s.script[0] = Step::Open {
            text: "W5XKX 42 {42} TX MOM HOME SUN K".into(),
            read_back: rb_tx(42, "MOM", "HOME SUN"),
        };
        s.script.insert(
            0,
            Step::Say {
                text: "K1ABC 42 {42} TX MOM HOME SUN K".into(),
                expect: None,
            },
        );
        s
    });
    v.push({
        let mut s = tx(
            "trailing-noise",
            "a noise burst after each K is not taken as part of the message",
            "MOM",
            "HOME SUN",
        );
        s.script[0] = Step::Open {
            text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K ~"),
            read_back: rb_tx(42, "MOM", "HOME SUN"),
        };
        s.script[1] = Step::Say {
            text: "OK 43 {43} K ~".into(),
            expect: Some(de("SENT 43")),
        };
        s
    });
    v.push(tx(
        "over-word-mid-message",
        "SK, I and AM in the message text do not end it",
        "MOM",
        "BACK IN SK I AM IN A MINE TOWN",
    ));
    v.push(tx(
        "final-word-k",
        "a message whose last word is K, keyed before the over K: the word is kept",
        "MOM",
        "BRING VITAMIN K",
    ));
    v.push({
        let mut s = tx(
            "kn-over",
            "every transmission ends in KN keyed run together, with a noise burst after it",
            "MOM",
            "HOME SUN",
        );
        let done = de("SENT 43");
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN ( ~"),
                read_back: rb_tx(42, "MOM", "HOME SUN"),
            },
            Step::Say {
                text: "OK 43 {43} ( ~".into(),
                expect: Some(done.clone()),
            },
            Step::Say {
                text: "AGN 44 {44} ( ~".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed.push(Over::Full(done));
        s.expect.last_seq = 44;
        s
    });
    v.push({
        let mut s = tx(
            "agn-read-back",
            "AGN after the read-back repeats it and uses a line, so the OK comes on the line after",
            "MOM",
            "HOME SUN",
        );
        let rb = rb_tx(42, "MOM", "HOME SUN");
        let done = de("SENT 44");
        s.script = vec![
            s.script[0].clone(),
            Step::Say {
                text: "AGN 43 {43} K".into(),
                expect: Some(rb.clone()),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: None,
            },
            Step::Say {
                text: "OK 44 {44} K".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb, &rb, &done]);
        s.expect.last_seq = 44;
        s
    });

    for wpm in [10, 15, 20, 25, 30] {
        v.push(with_fist(
            tx(
                &format!("speed-{wpm}wpm"),
                &format!("TX sent at {wpm} wpm"),
                "MOM",
                LONG_TEXT,
            ),
            |f| f.wpm = wpm as f32,
        ));
    }
    for snr in [20, 6, 3, 0] {
        v.push(with_fist(
            tx(
                &format!("snr-{snr}db"),
                &format!("TX at {snr} dB SNR in 2500 Hz"),
                "BOB",
                "SEE YOU SUN",
            ),
            |f| f.snr_db = Some(snr as f32),
        ));
    }
    v.push(with_fist(
        tx(
            "hand-keyed",
            "a sloppy straight key: 12% timing jitter, stretched gaps, 25 Hz off pitch, 10 dB",
            "MOM",
            LONG_TEXT,
        ),
        |f| {
            f.wpm = 15.0;
            f.jitter = 0.12;
            f.gap_stretch = 1.4;
            f.offset_hz = 25.0;
            f.snr_db = Some(10.0);
        },
    ));
    v.push(with_radio(
        tx(
            "sidetone",
            "the radio's own keying in the receive audio is not decoded",
            "MOM",
            LONG_TEXT,
        ),
        |r| r.sidetone = true,
    ));
    v.push(with_radio(
        tx(
            "echo-off",
            "CI-V USB Echo Back OFF (the radio's default)",
            "MOM",
            "HOME SUN",
        ),
        |r| r.echo = false,
    ));

    // Radio faults.
    let open_long = format!("{FIELD_CALL} 42 {{42}} TX MOM {LONG_TEXT} K");
    let rb_long = rb_tx(42, "MOM", LONG_TEXT);
    v.push({
        let mut s = base(
            "fault-high-swr",
            "the antenna goes to SWR 3.5 after the window's tune: the read-back is cut off, \
             and nothing more is keyed this window",
        );
        s.script = vec![
            Step::SetSwr(3.5),
            Step::Unanswered {
                text: open_long.clone(),
                tries: 2,
            },
        ];
        s.expect.keyed = vec![Over::Cut(rb_long.clone())];
        s.expect.last_seq = 42;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = base(
            "fault-high-swr-mid-over",
            "the antenna goes to SWR 3.5 once the read-back's first piece is keyed: the SWR is \
             watched all through every piece, so the second is cut off, and nothing more is \
             keyed this window",
        );
        s.script = vec![
            Step::Inject(Fault::SwrAfter { skip: 1, swr: 3.5 }),
            Step::Unanswered {
                text: open_long.clone(),
                tries: 2,
            },
        ];
        s.expect.keyed = vec![Over::Cut(rb_long.clone())];
        s.expect.last_seq = 42;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = base(
            "fault-foldback",
            "the antenna goes bad after the window's tune and the radio folds its output back \
             to nothing: no SWR reading, so the node stops",
        );
        s.radio.foldback = Some(Foldback {
            above_swr: 3.0,
            fraction: 0.0,
        });
        s.script = vec![
            Step::SetSwr(4.0),
            Step::Unanswered {
                text: open_long.clone(),
                tries: 2,
            },
        ];
        s.expect.keyed = vec![Over::Cut(rb_long.clone())];
        s.expect.last_seq = 42;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = base(
            "fault-high-swr-next-window",
            "SWR 3.5 after the window's tune locks the node out of its window; the antenna \
             recovers, and at the next window the node tunes again, clears the lockout and works",
        );
        s.node.schedule = Some((15, 4));
        let rb44 = rb_tx(44, "MOM", "HOME SUN");
        let done = de("SENT 45");
        s.script = vec![
            Step::SetSwr(3.5),
            Step::Unanswered {
                text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K"),
                tries: 2,
            },
            Step::SetSwr(1.2),
            Step::NextWindow,
            Step::Open {
                text: format!("{FIELD_CALL} 44 {{44}} TX MOM HOME SUN K"),
                read_back: rb44.clone(),
            },
            Step::Say {
                text: "OK 45 {45} K".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed = vec![
            Over::Cut(rb_tx(42, "MOM", "HOME SUN")),
            Over::Full(rb44),
            Over::Full(done),
        ];
        s.expect.sent = sent("MOM", "HOME SUN");
        s.expect.last_seq = 45;
        s.expect.tunes = 2;
        s.expect.ids = 2;
        s.expect.forced_receive = true;
        s
    });
    v.push(with_radio(
        tx(
            "tuned-load",
            "an antenna at SWR 2.5, above the node's 2.0 limit: the tuner matches it at the start \
             of the window and the node transmits",
            "MOM",
            "HOME SUN",
        ),
        |r| r.swr = 2.5,
    ));
    v.push({
        let mut s = base(
            "fault-no-match",
            "an antenna at SWR 3.5, beyond the tuner's 3:1 range: the tuner bypasses itself \
             (1C 01 reads 00), and the node keys nothing this window",
        );
        s.radio.swr = 3.5;
        s.script = vec![Step::Unanswered {
            text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K"),
            tries: 2,
        }];
        s.expect.last_seq = 42;
        s.expect.ids = 0;
        s
    });
    let stuck = |name: &str, about: &str, carrier: bool| {
        let mut s = tx(name, about, "MOM", "HI");
        // After the first tune and ID: the read-back sticks.
        s.script.insert(
            0,
            Step::Inject(Fault::StickInTx {
                skip: 0,
                carrier,
                recoverable: true,
            }),
        );
        s.expect.forced_receive = true;
        s
    };
    v.push(stuck(
        "fault-stuck-tx",
        "the radio stays on transmit after the read-back; the node forces receive and carries on",
        false,
    ));
    v.push(stuck(
        "fault-stuck-key",
        "the key sticks down after the read-back; the node forces receive within the stuck margin",
        true,
    ));
    v.push({
        let mut s = tx(
            "fault-stuck-last-over",
            "the radio stays on transmit after the node's last over (SENT); the node forces receive",
            "MOM",
            "HI",
        );
        s.script.insert(
            1,
            Step::Inject(Fault::StickInTx {
                skip: 0,
                carrier: false,
                recoverable: true,
            }),
        );
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = tx(
            "fault-jammed-tx",
            "the radio stays on transmit whatever it is told: transmit is inhibited, nothing more is \
             keyed, and the OK goes unheard while the radio is on transmit",
            "MOM",
            "HI",
        );
        s.script[1] = Step::Unanswered {
            text: "OK 43 {43} K".into(),
            tries: 2,
        };
        s.script.push(Step::ClearStuck);
        s.script.insert(
            0,
            Step::Inject(Fault::StickInTx {
                skip: 0,
                carrier: false,
                recoverable: false,
            }),
        );
        s.expect.keyed = full(&[&rb_tx(42, "MOM", "HI")]);
        s.expect.sent = Vec::new();
        s.expect.last_seq = 42;
        s.expect.inhibited = true;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        // Refused once the read-back is out: the status read before the node's next
        // over, then the receive command and status read of more forced receive
        // tries than the node makes (about 9 at 1x real time, 3 when time-scaled);
        // the watchdog's later tries get through. NG rather than lost replies: a
        // lost one costs the driver's 500 ms reply timeout in real time, which
        // time-scaled is longer than the pending commit stays open.
        let refused = 1 + 12 * 2;
        let mut s = tx(
            "fault-status-refused",
            "the radio answers NG to its status commands for a while after the read-back: the \
             node cannot confirm receive before repeating it and inhibits, but the radio is on \
             receive, so the OK is heard and the commit reaches the gateway, unconfirmed",
            "MOM",
            "HI",
        );
        s.script = vec![
            s.script[0].clone(),
            Step::Inject(Fault::Reply {
                cmd: vec![0x1C, 0x00],
                skip: 0,
                times: refused,
                kind: ReplyFault::Ng,
            }),
            Step::Say {
                text: "AGN 43 {43} K".into(),
                expect: None,
            },
            Step::Unanswered {
                text: "OK 44 {44} K".into(),
                tries: 2,
            },
        ];
        s.expect.keyed = full(&[&rb_tx(42, "MOM", "HI")]);
        s.expect.last_seq = 44;
        s.expect.inhibited = true;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        // The radio refuses the stop and receive commands more often than the
        // node's forced receive tries (about 9 pairs at 1x real time, 3 when
        // time-scaled), so the node inhibits; only the watchdog's later tries can
        // take it off transmit.
        let refused = 12;
        let mut s = tx(
            "fault-watchdog",
            "the radio stays on transmit and refuses to unkey until the node has given up: the \
             watchdog keeps trying and takes it off transmit",
            "MOM",
            "HI",
        );
        // After the first tune and ID, so the node's set-up 1C 00 00 is past.
        s.script = vec![
            Step::Inject(Fault::StickInTx {
                skip: 0,
                carrier: false,
                recoverable: true,
            }),
            Step::Inject(Fault::Reply {
                cmd: vec![0x17, 0xFF],
                skip: 0,
                times: refused,
                kind: ReplyFault::Ng,
            }),
            Step::Inject(Fault::Reply {
                cmd: vec![0x1C, 0x00, 0x00],
                skip: 0,
                times: refused,
                kind: ReplyFault::Ng,
            }),
            s.script[0].clone(),
            Step::WaitReceive,
        ];
        s.expect.keyed = full(&[&rb_tx(42, "MOM", "HI")]);
        s.expect.sent = Vec::new();
        s.expect.last_seq = 42;
        s.expect.inhibited = true;
        s.expect.forced_receive = true;
        s
    });
    let civ = |name: &str, about: &str, cmd: &[u8], kind: ReplyFault, cut: bool| {
        let mut s = tx(name, about, "MOM", LONG_TEXT);
        // After the first tune and ID, which read the meters too.
        s.script.insert(
            0,
            Step::Inject(Fault::Reply {
                cmd: cmd.to_vec(),
                skip: 0,
                times: 1,
                kind,
            }),
        );
        if cut {
            s.expect.keyed.insert(0, Over::Cut(rb_long.clone()));
        }
        s.expect.forced_receive = true;
        s
    };
    v.push(civ(
        "fault-civ-ng",
        "the radio answers NG to the first CW message: nothing keyed, the operator repeats",
        &[0x17],
        ReplyFault::Ng,
        false,
    ));
    v.push(civ(
        "fault-civ-lost-reply",
        "the reply to the first CW message is lost: the node stops and forces receive",
        &[0x17],
        ReplyFault::Drop,
        true,
    ));
    v.push(civ(
        "fault-civ-late-reply",
        "an SWR reading arrives after the driver's timeout: the node stops, resynchronises and recovers",
        &[0x15, 0x12],
        ReplyFault::Delay(Duration::from_millis(700)),
        true,
    ));
    v.push({
        let mut s = civ(
            "transceive",
            "someone at the radio keeps nudging the dial: CI-V Transceive frames to 00h arrive \
             unasked, also while the driver resynchronises after a late SWR reading",
            &[0x15, 0x12],
            ReplyFault::Delay(Duration::from_millis(700)),
            true,
        );
        s.radio.dial_nudges = true;
        s
    });
    v.push({
        let mut s = base(
            "fault-tune-hang",
            "the tuner never reports done, and still reads tuning (1C 01 02) after the node \
             gives up on it and forces receive: the node inhibits transmitting and tells the \
             owner",
        );
        s.radio.faults.push(Fault::TuneNeverFinishes);
        s.script = vec![Step::Unanswered {
            text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K"),
            tries: 2,
        }];
        s.expect.last_seq = 42;
        s.expect.ids = 0;
        s.expect.forced_receive = true;
        s.expect.inhibited = true;
        s
    });
    v.push({
        let mut s = tx(
            "fault-tune-lost-reply",
            "the reply to the start-up tune command is lost: the node cannot trust that tune, \
             so it forces receive and keys nothing until it has tuned again, which it does \
             before its first reply",
            "MOM",
            "HOME SUN",
        );
        s.radio.faults.push(Fault::Reply {
            cmd: vec![0x1C, 0x01, 0x02],
            skip: 0,
            times: 1,
            kind: ReplyFault::Drop,
        });
        s.expect.tunes = 2;
        s.expect.ids = 0;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = base(
            "fault-inhibited-at-start",
            "the node starts with tx-inhibited left by an earlier fault: it does not tune or key, \
             and tells the owner once",
        );
        s.node.inhibited_at_start = true;
        s.script = vec![Step::Unanswered {
            text: format!("{FIELD_CALL} 42 {{42}} TX MOM HI K"),
            tries: 2,
        }];
        s.expect.last_seq = 42;
        s.expect.tunes = 0;
        s.expect.ids = 0;
        s.expect.inhibited = true;
        s
    });
    let refused = |name: &str, about: &str, item: &'static str, change: fn(&mut Menu)| {
        let mut s = base(name, about);
        change(&mut s.radio.menu);
        s.expect.tunes = 0;
        s.expect.ids = 0;
        s.expect.refused = Some(item);
        s
    };
    v.push(refused(
        "preflight-tot-off",
        "the radio's Time-Out Timer is OFF (its default): the node refuses to start, having \
         only read from the radio",
        "Time-Out Timer (CI-V)",
        |m| m.time_out_timer = 0x00,
    ));
    v.push(refused(
        "preflight-usb-send-dtr",
        "the radio is set to transmit while DTR is up (USB SEND = DTR): the node refuses to \
         start, having only read from the radio",
        "USB SEND",
        |m| m.usb_send = 0x01,
    ));

    // Listening all the time, as the node does by default.
    v.push({
        let mut s = base(
            "retune",
            "listening all the time: the node answers on its start-up tune, then tunes again \
             before a read-back once that tune is older than retune_minutes (10 here), and not \
             before the SENT that follows it",
        );
        s.node.retune_minutes = 10;
        let (rb42, sent43) = (rb_tx(42, "MOM", "HOME SUN"), de("SENT 43"));
        let (rb44, sent45) = (rb_tx(44, "BOB", "CALL ME"), de("SENT 45"));
        s.script = vec![
            Step::Open {
                text: format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K"),
                read_back: rb42.clone(),
            },
            Step::Say {
                text: "OK 43 {43} K".into(),
                expect: Some(sent43.clone()),
            },
            Step::Tunes(1),
            Step::Wait(600.0),
            Step::FirstTry,
            Step::Open {
                text: format!("{FIELD_CALL} 44 {{44}} TX BOB CALL ME K"),
                read_back: rb44.clone(),
            },
            Step::Tunes(2),
            Step::Say {
                text: "OK 45 {45} K".into(),
                expect: Some(sent45.clone()),
            },
        ];
        s.expect.keyed = full(&[&rb42, &sent43, &rb44, &sent45]);
        s.expect.sent = [sent("MOM", "HOME SUN"), sent("BOB", "CALL ME")].concat();
        s.expect.last_seq = 45;
        s.expect.tunes = 2;
        s
    });
    v.push({
        let mut s = base(
            "fault-high-swr-retune",
            "listening all the time, SWR 3.5 after the start-up tune locks the node out, also \
             once the antenna recovers; when retune_minutes (10 here) have passed, the node \
             tunes before its next read-back, clears the lockout and works",
        );
        s.node.retune_minutes = 10;
        let open42 = format!("{FIELD_CALL} 42 {{42}} TX MOM HOME SUN K");
        let rb44 = rb_tx(44, "MOM", "HOME SUN");
        let done = de("SENT 45");
        s.script = vec![
            Step::SetSwr(3.5),
            Step::Unanswered {
                text: open42.clone(),
                tries: 2,
            },
            Step::SetSwr(1.2),
            Step::Unanswered {
                text: open42,
                tries: 1,
            },
            Step::Wait(600.0),
            Step::FirstTry,
            Step::Open {
                text: format!("{FIELD_CALL} 44 {{44}} TX MOM HOME SUN K"),
                read_back: rb44.clone(),
            },
            Step::Say {
                text: "OK 45 {45} K".into(),
                expect: Some(done.clone()),
            },
        ];
        s.expect.keyed = vec![
            Over::Cut(rb_tx(42, "MOM", "HOME SUN")),
            Over::Full(rb44),
            Over::Full(done),
        ];
        s.expect.sent = sent("MOM", "HOME SUN");
        s.expect.last_seq = 45;
        s.expect.tunes = 2;
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = tx(
            "front-panel-split",
            "someone at the radio switches split on after the start-up tune: the node checks \
             before keying and keys nothing while it is on, then answers the same open once it \
             is off",
            "MOM",
            "HOME SUN",
        );
        let Step::Open { text, .. } = s.script[0].clone() else {
            unreachable!()
        };
        s.script.splice(
            0..0,
            [
                Step::Panel(Panel::Split(Some(FREQUENCY_HZ + 10_000))),
                Step::Unanswered { text, tries: 2 },
                Step::Panel(Panel::Split(None)),
            ],
        );
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = tx(
            "front-panel-delta-tx",
            "someone at the radio switches ∂TX on after the start-up tune: the node checks \
             before keying and keys nothing while it is on, then answers the same open once it \
             is off",
            "MOM",
            "HOME SUN",
        );
        let Step::Open { text, .. } = s.script[0].clone() else {
            unreachable!()
        };
        s.script.splice(
            0..0,
            [
                Step::Panel(Panel::DeltaTx(true)),
                Step::Unanswered { text, tries: 2 },
                Step::Panel(Panel::DeltaTx(false)),
            ],
        );
        s.expect.forced_receive = true;
        s
    });
    v.push({
        let mut s = base(
            "front-panel-idle",
            "someone at the radio tunes away and selects USB while the node is idle: within \
             check_minutes (10) the node sets the radio up again, transmitting nothing",
        );
        s.script = vec![
            Step::Panel(Panel::Dial(14_074_000)),
            Step::Panel(Panel::Mode(0x01, 0x01)),
            Step::Wait(660.0),
        ];
        s
    });
    v.push({
        let mut s = tx(
            "other-stations",
            "listening all the time through 15 minutes of band noise and other stations, some \
             calling the node, sending AGN and NO, or an open with a wrong code: all are heard, \
             nothing is keyed or tuned; then an exchange works, its open answered first time",
            "MOM",
            "HOME SUN",
        );
        let quiet = |text: &str| Step::Say {
            text: text.into(),
            expect: None,
        };
        s.script.splice(
            0..0,
            [
                quiet("CQ CQ CQ DE K1ABC K1ABC K"),
                Step::Wait(120.0),
                quiet("K1ABC DE W1XYZ GM OM UR RST 599 599 NAME ED HW? K1ABC DE W1XYZ K"),
                Step::Wait(120.0),
                quiet(&format!("{NODE_CALL} DE K1ABC QSL? K")),
                quiet("AGN K"),
                quiet("NO K"),
                Step::Wait(300.0),
                quiet(&format!("{FIELD_CALL} 42 ABCDEFGH TX MOM HI K")),
                Step::Wait(300.0),
                Step::FirstTry,
            ],
        );
        s.expect.heard = [
            "CQ CQ CQ DE K1ABC K1ABC K",
            "K1ABC DE W1XYZ GM OM UR RST 599 599 NAME ED HW? K1ABC DE W1XYZ K",
            &format!("{NODE_CALL} DE K1ABC QSL? K"),
            "AGN K",
            "NO K",
            &format!("{FIELD_CALL} 42 ABCDEFGH TX MOM HI K"),
        ]
        .map(String::from)
        .to_vec();
        s
    });
    let keyer = any_radio::scenarios(&v);
    v.extend(keyer);
    v
}

/// The scenario called `name`.
pub fn scenario(name: &str) -> Option<Scenario> {
    scenarios().into_iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// The sweep: complete exchanges over speed x SNR x keying, several trials each, to
// find where the node stops working rather than to check one point.

/// The field operator speeds the sweep runs by default: the decoder's 5-35 wpm.
pub const SWEEP_WPM: [f32; 10] = [5.0, 8.0, 10.0, 13.0, 15.0, 18.0, 20.0, 25.0, 30.0, 35.0];
/// The SNRs in 2500 Hz the sweep runs by default; `None` is no noise at all.
pub const SWEEP_SNR: [Option<f32>; 8] = [
    None,
    Some(20.0),
    Some(10.0),
    Some(6.0),
    Some(3.0),
    Some(0.0),
    Some(-3.0),
    Some(-6.0),
];
/// Trials per cell by default, each with its own noise and timing jitter.
pub const SWEEP_TRIALS: u32 = 3;
/// The message every sweep run sends.
pub const SWEEP_DEST: &str = "MOM";
pub const SWEEP_TEXT: &str = LONG_TEXT;
/// The inbound message a sweep run with RX reads out.
const SWEEP_INBOX: (&str, &str) = ("BOB", "DRIVE SAFE CALL WHEN YOU CAN");
/// Times the sweep's operator starts over on fresh lines after a wrong read-back.
const SWEEP_RESTARTS: u32 = 1;
/// The checks whose failure is a hard failure whatever the conditions: the node
/// broke a safety bound, sent the radio something the manual does not allow, left
/// it misconfigured, forced receive with no fault, decoded itself, stopped, or left
/// a transmission or a tune unidentified.
const HARD_CHECKS: [&str; 8] = [
    "setup",
    "ended",
    "ci-v",
    "settings",
    "forced receive",
    "safety",
    "self-decode",
    "station ID",
];

/// How the field operator keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keying {
    /// An electronic keyer or a computer: 2% timing jitter, on the node's pitch.
    Machine,
    /// A sloppy straight key, as the `hand-keyed` scenario: 12% timing jitter,
    /// character and word gaps stretched 1.4 times, 25 Hz off pitch.
    Hand,
}

impl Keying {
    pub const ALL: [Keying; 2] = [Keying::Machine, Keying::Hand];

    pub fn name(self) -> &'static str {
        match self {
            Keying::Machine => "machine",
            Keying::Hand => "hand",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Keying::Machine => "machine-keyed: 2% timing jitter, on pitch",
            Keying::Hand => "hand-keyed: 12% timing jitter, gaps stretched 1.4x, 25 Hz off pitch",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "machine" | "machine-keyed" => Some(Keying::Machine),
            "hand" | "hand-keyed" => Some(Keying::Hand),
            _ => None,
        }
    }

    fn apply(self, f: &mut Fist) {
        let (jitter, gap_stretch, offset_hz) = match self {
            Keying::Machine => (0.02, 1.0, 0.0),
            Keying::Hand => (0.12, 1.4, 25.0),
        };
        f.jitter = jitter;
        f.gap_stretch = gap_stretch;
        f.offset_hz = offset_hz;
    }
}

/// One point of the sweep's grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub keying: Keying,
    pub wpm: f32,
    /// SNR in 2500 Hz; `None` for no noise.
    pub snr_db: Option<f32>,
}

impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} wpm {}",
            self.keying.name(),
            self.wpm,
            snr_head(self.snr_db)
        )
    }
}

/// `clean` or `6 dB`.
fn snr_head(snr: Option<f32>) -> String {
    snr.map_or("clean".into(), |s| format!("{s} dB"))
}

/// What a sweep's seeds do and do not fix, printed with every sweep.
pub const TIMING_NOTE: &str = "Seeds fix each trial's audio (noise and keying jitter), \
     not its outcome: the node and the mock radio run on the wall clock, so where the \
     node's transmissions fall against the operator's audio depends on thread \
     scheduling. A re-run repeats the success counts away from the edges, but the \
     extra transmissions and decode percentages vary a little, and a trial at an edge \
     can go either way, more so under a different load or --jobs.";

/// The region every trial must pass, or the sweep fails. It is set well inside the
/// edges the full sweep measured on 2026-10-04 (default grid, 3 trials per cell;
/// the same success counts in five sweeps but for one trial). Machine-keyed: every
/// trial passed down to -3 dB at 8-35 wpm; 5 wpm failed a trial at 20 dB and at
/// 10 dB yet passed from 6 to -3 dB; -6 dB let 1 of 30 runs through (8 wpm).
/// Hand-keyed: every trial passed down to -3 dB at 8-18, 25 and 30 wpm, to 0 dB at
/// 20 and 35 wpm (2/3 and 1/3 at -3 dB), to 3 dB at 5 wpm; -6 dB never passed.
/// Seeds do not fix outcomes ([`TIMING_NOTE`]), so the region stays clear of the
/// edges and flags a regression, not the luck of three trials at an edge.
pub fn should_pass(c: &Cell) -> bool {
    let snr = c.snr_db.unwrap_or(f32::INFINITY);
    match c.keying {
        Keying::Machine => (10.0..=30.0).contains(&c.wpm) && snr >= 6.0,
        Keying::Hand => (10.0..=25.0).contains(&c.wpm) && snr >= 10.0,
    }
}

/// [`should_pass`], in words.
pub const SHOULD_PASS: &str = "machine-keyed 10-30 wpm at 6 dB and above (and clean); \
     hand-keyed 10-25 wpm at 10 dB and above (and clean)";

/// The grid, the trials per cell and what each run does.
#[derive(Debug, Clone)]
pub struct SweepSpec {
    pub wpms: Vec<f32>,
    pub snrs: Vec<Option<f32>>,
    pub keyings: Vec<Keying>,
    pub trials: u32,
    /// Follow the TX exchange with an RX exchange reading out one message.
    pub rx: bool,
}

impl Default for SweepSpec {
    fn default() -> Self {
        Self {
            wpms: SWEEP_WPM.to_vec(),
            snrs: SWEEP_SNR.to_vec(),
            keyings: Keying::ALL.to_vec(),
            trials: SWEEP_TRIALS,
            rx: false,
        }
    }
}

impl SweepSpec {
    /// Speeds slowest first, SNRs clean first and then strongest first, no
    /// duplicates.
    pub fn normalized(mut self) -> Self {
        self.wpms.sort_by(f32::total_cmp);
        self.wpms.dedup();
        let key = |s: &Option<f32>| -s.unwrap_or(f32::INFINITY);
        self.snrs.sort_by(|a, b| key(a).total_cmp(&key(b)));
        self.snrs.dedup();
        let mut k = Vec::new();
        for x in self.keyings {
            if !k.contains(&x) {
                k.push(x);
            }
        }
        self.keyings = k;
        self
    }

    pub fn cells(&self) -> Vec<Cell> {
        let mut v = Vec::new();
        for &keying in &self.keyings {
            for &wpm in &self.wpms {
                for &snr_db in &self.snrs {
                    v.push(Cell {
                        keying,
                        wpm,
                        snr_db,
                    });
                }
            }
        }
        v
    }

    /// Every run: each cell, `trials` times.
    pub fn runs(&self) -> Vec<(Cell, u32)> {
        self.cells()
            .into_iter()
            .flat_map(|c| (0..self.trials).map(move |t| (c, t)))
            .collect()
    }
}

/// The scenario for one trial of `cell`: a TX exchange of [`SWEEP_TEXT`] to
/// [`SWEEP_DEST`] (open, read-back, `OK`, `SENT`) and, with `rx`, an RX exchange
/// reading out one waiting message, each worked as [`Exchange`] says.
pub fn sweep_scenario(cell: &Cell, trial: u32, rx: bool) -> Scenario {
    let name = format!(
        "sweep-{}-{}wpm-{}-{trial}",
        cell.keying.name(),
        cell.wpm,
        cell.snr_db.map_or("clean".into(), |s| format!("{s}db"))
    );
    let mut s = base(&name, &format!("{cell}, trial {}", trial + 1));
    s.fist = Fist {
        wpm: cell.wpm,
        snr_db: cell.snr_db,
        seed: mix(&[
            0x0053_5745_4550,
            cell.keying as u64,
            u64::from(cell.wpm.to_bits()),
            cell.snr_db.map_or(u64::MAX, |x| u64::from(x.to_bits())),
            u64::from(trial),
        ]),
        ..Fist::default()
    };
    cell.keying.apply(&mut s.fist);
    s.script = vec![Step::Exchange(Exchange {
        request: format!("TX {SWEEP_DEST} {SWEEP_TEXT}"),
        read_back: format!("R {{open}} TX {SWEEP_DEST} {SWEEP_TEXT} ? DE {NODE_CALL} K"),
        result: de("SENT {commit}"),
        restarts: SWEEP_RESTARTS,
    })];
    // What a run without a repeat or restart keys; the sweep classifies runs by
    // their facts, not by these.
    let rb = rb_tx(42, SWEEP_DEST, SWEEP_TEXT);
    let done = de("SENT 43");
    s.expect.keyed = full(&[&rb, &done]);
    s.expect.sent = sent(SWEEP_DEST, SWEEP_TEXT);
    s.expect.last_seq = 43;
    if rx {
        let (from, text) = SWEEP_INBOX;
        s.node.inbox = vec![(from.into(), text.into())];
        let result = format!("NR 1 FM {from} {text} = A");
        s.script.push(Step::Exchange(Exchange {
            request: "RX".into(),
            read_back: de("R {open} 1 MSG ?"),
            result: de(&result),
            restarts: SWEEP_RESTARTS,
        }));
        s.expect
            .keyed
            .extend(full(&[&de("R 44 1 MSG ?"), &de(&result)]));
        s.expect.read = vec![1];
        s.expect.last_seq = 45;
    }
    s
}

/// One sweep run, classified.
#[derive(Debug, Clone)]
pub struct TrialResult {
    pub cell: Cell,
    pub trial: u32,
    /// Seeds the trial's noise and keying jitter: its audio, not its outcome. The
    /// node and the mock radio run on the wall clock (the mock's radio time is real
    /// time times the scale, the node's transmit guard uses `Instant`), so where the
    /// node's transmissions fall against the operator's audio depends on thread
    /// scheduling, and a trial near an edge can go the other way when re-run with
    /// the same seed. See [`TIMING_NOTE`].
    pub seed: u64,
    /// The exact message reached the gateway, once and nothing else did, the
    /// operator heard `SENT` for it (and with RX, heard the readout exactly and the
    /// message was marked read), with no hard failure.
    pub success: bool,
    /// The gateway sent the exact message at least once.
    pub delivered: bool,
    /// Messages the gateway sent that are not the one intended, and repeats of it:
    /// a serious failure, never a success.
    pub wrong_delivered: Vec<(String, String)>,
    /// Everything the operator keyed, `NO` included.
    pub transmissions: u32,
    /// Transmissions beyond the open and `OK` an exchange needs when nothing is
    /// lost or garbled: repeats, `NO`s, and the transmissions on fresh lines.
    pub repeats: u32,
    pub restarts: u32,
    /// Read-backs that parsed but were not the message (garbled text), which the
    /// operator caught and answered `NO`.
    pub wrong_read_backs: u32,
    /// What the node decoded: receptions in all, operator transmissions decoded
    /// exactly, and receptions that were not exactly one of them (garbled, split,
    /// or noise).
    pub receptions: u32,
    pub decoded_exact: u32,
    pub decode_mismatches: u32,
    /// Operator transmissions found word for word inside a reception, with noise
    /// decoded as extra characters around them allowed.
    pub decoded_intact: u32,
    /// Hard failures: `check: detail`.
    pub hard: Vec<String>,
    /// The run hit the self-test's time limit.
    pub timed_out: bool,
    /// Why it is not a success, empty if it is.
    pub why: String,
    pub wall: Duration,
    pub radio_time: Duration,
}

impl TrialResult {
    /// A safety violation or a wrong message delivered: a failure whatever the SNR.
    pub fn hard_failure(&self) -> bool {
        !self.hard.is_empty() || !self.wrong_delivered.is_empty()
    }
}

/// Classify a sweep run from its outcome.
pub fn classify(cell: &Cell, trial: u32, seed: u64, rx: bool, out: &Outcome) -> TrialResult {
    let f = &out.facts;
    let intended = (SWEEP_DEST.to_string(), SWEEP_TEXT.to_string());
    let right = f.sent.iter().filter(|m| **m == intended).count();
    let mut wrong_delivered: Vec<_> = f.sent.iter().filter(|m| **m != intended).cloned().collect();
    // The node must never send a message twice either.
    wrong_delivered.extend(std::iter::repeat_n(intended, right.saturating_sub(1)));
    let hard: Vec<String> = out
        .checks
        .iter()
        .filter(|c| !c.pass && HARD_CHECKS.contains(&c.name))
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    let exchanges = if rx { 2 } else { 1 };
    let read_ok = if rx { f.read == [1] } else { f.read.is_empty() };
    let why = if !hard.is_empty() {
        hard.join("; ")
    } else if !wrong_delivered.is_empty() {
        format!("WRONG MESSAGE DELIVERED: {wrong_delivered:?}")
    } else if let Some(e) = f.script_failures.first() {
        e.clone()
    } else if f.exchanges_done < exchanges {
        "the exchange did not finish".into()
    } else if right == 0 {
        "SENT keyed but no message reached the gateway".into()
    } else if !read_ok {
        format!("messages marked read: {:?}", f.read)
    } else {
        String::new()
    };
    let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let sent: Vec<Vec<String>> = f.operator_sent.iter().map(|t| words(t)).collect();
    // Each reception matches one transmission at most.
    let matched = |fits: &dyn Fn(&[String], &[String]) -> bool| {
        let mut pool: Vec<Vec<String>> = f.received.iter().map(|r| words(r)).collect();
        let mut n = 0;
        for t in &sent {
            if let Some(i) = pool.iter().position(|r| fits(r, t)) {
                pool.remove(i);
                n += 1;
            }
        }
        (n, pool.len() as u32)
    };
    let (exact, unmatched) = matched(&|r, t| r == t);
    let (intact, _) = matched(&|r, t| !t.is_empty() && r.windows(t.len()).any(|w| w == t));
    let transmissions = f.operator_sent.len() as u32;
    TrialResult {
        cell: *cell,
        trial,
        seed,
        success: why.is_empty(),
        delivered: right > 0,
        wrong_delivered,
        transmissions,
        repeats: f.extra_transmissions,
        restarts: f.restarts,
        wrong_read_backs: f.wrong_read_backs,
        receptions: f.received.len() as u32,
        decoded_exact: exact,
        decode_mismatches: unmatched,
        decoded_intact: intact,
        hard,
        timed_out: f
            .script_failures
            .iter()
            .any(|e| e.contains("took too long")),
        why,
        wall: out.wall,
        radio_time: out.radio_time,
    }
}

/// Run trial `trial` of `cell`.
pub fn run_trial(cell: &Cell, trial: u32, rx: bool, scale: f32) -> (TrialResult, Outcome) {
    let s = sweep_scenario(cell, trial, rx);
    let out = run(&s, scale);
    (classify(cell, trial, s.fist.seed, rx, &out), out)
}

/// Run every trial of `spec`, `jobs` at once, calling `each` with the number done
/// so far as each finishes. Results come back in [`SweepSpec::runs`] order.
pub fn sweep(
    spec: &SweepSpec,
    scale: f32,
    jobs: usize,
    each: &(dyn Fn(usize, &TrialResult, &Outcome) + Sync),
) -> Vec<TrialResult> {
    sweep_runs(&spec.runs(), spec.rx, scale, jobs, each)
}

/// [`sweep`] over any list of (cell, trial) runs, in that order.
pub fn sweep_runs(
    runs: &[(Cell, u32)],
    rx: bool,
    scale: f32,
    jobs: usize,
    each: &(dyn Fn(usize, &TrialResult, &Outcome) + Sync),
) -> Vec<TrialResult> {
    // The slowest speeds take longest: start them first.
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by(|&a, &b| runs[a].0.wpm.total_cmp(&runs[b].0.wpm));
    let next = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let results = std::sync::Mutex::new(vec![None; runs.len()]);
    thread::scope(|sc| {
        for _ in 0..jobs.clamp(1, runs.len().max(1)) {
            sc.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed) as usize;
                let Some(&i) = order.get(k) else { break };
                let (cell, trial) = &runs[i];
                let (r, out) = run_trial(cell, *trial, rx, scale);
                each(done.fetch_add(1, Ordering::Relaxed) as usize + 1, &r, &out);
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .flatten()
        .collect()
}

/// A cell's trials, added up.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    trials: u32,
    successes: u32,
    repeats_in_successes: u32,
    wrong_read_backs: u32,
    wrong_delivered: u32,
    hard: u32,
    timed_out: u32,
    transmissions: u32,
    decoded_exact: u32,
    decoded_intact: u32,
}

impl Tally {
    fn of(results: &[TrialResult], cell: &Cell) -> Self {
        let mut t = Tally::default();
        for r in results.iter().filter(|r| r.cell == *cell) {
            t.trials += 1;
            if r.success {
                t.successes += 1;
                t.repeats_in_successes += r.repeats;
            }
            t.wrong_read_backs += r.wrong_read_backs;
            t.wrong_delivered += r.wrong_delivered.len() as u32;
            t.hard += u32::from(!r.hard.is_empty());
            t.timed_out += u32::from(r.timed_out);
            t.transmissions += r.transmissions;
            t.decoded_exact += r.decoded_exact;
            t.decoded_intact += r.decoded_intact;
        }
        t
    }

    fn all_pass(&self) -> bool {
        self.trials > 0 && self.successes == self.trials
    }

    /// Repeated transmissions per successful trial, if any success needed one.
    fn repeats(&self) -> Option<f32> {
        (self.successes > 0 && self.repeats_in_successes > 0)
            .then(|| self.repeats_in_successes as f32 / self.successes as f32)
    }

    fn text(&self, region: bool) -> String {
        if self.trials == 0 {
            return "-".into();
        }
        let mut s = format!("{}/{}", self.successes, self.trials);
        if let Some(r) = self.repeats() {
            let _ = write!(s, "+{r:.1}");
        }
        if self.wrong_read_backs > 0 {
            s.push('w');
        }
        if self.timed_out > 0 {
            s.push('T');
        }
        if self.wrong_delivered > 0 {
            s.push_str("W!");
        }
        if self.hard > 0 {
            s.push_str("S!");
        }
        if region {
            format!("[{s}]")
        } else {
            format!(" {s}")
        }
    }
}

/// Where a sweep went wrong: hard failures anywhere, and failures in the
/// should-pass region.
#[derive(Debug)]
pub struct Verdict<'a> {
    pub hard: Vec<&'a TrialResult>,
    pub region_failures: Vec<&'a TrialResult>,
}

impl Verdict<'_> {
    pub fn ok(&self) -> bool {
        self.hard.is_empty() && self.region_failures.is_empty()
    }
}

pub fn verdict(results: &[TrialResult]) -> Verdict<'_> {
    Verdict {
        hard: results.iter().filter(|r| r.hard_failure()).collect(),
        region_failures: results
            .iter()
            .filter(|r| should_pass(&r.cell) && !r.success)
            .collect(),
    }
}

/// The matrices, the edges, the hard failures and the verdict.
pub fn render_sweep(spec: &SweepSpec, results: &[TrialResult]) -> String {
    let mut s = String::new();
    let col = 12;
    for &k in &spec.keyings {
        let _ = writeln!(s, "{}", k.about());
        let mut head = format!("{:>5} |", "wpm");
        for &snr in &spec.snrs {
            let _ = write!(head, " {:<w$}", snr_head(snr), w = col - 1);
        }
        let _ = writeln!(s, "{}", head.trim_end());
        let _ = writeln!(s, "{}", "-".repeat(head.trim_end().len()));
        for &wpm in &spec.wpms {
            let mut line = format!("{wpm:>5} |");
            for &snr_db in &spec.snrs {
                let c = Cell {
                    keying: k,
                    wpm,
                    snr_db,
                };
                let _ = write!(
                    line,
                    "{:<col$}",
                    Tally::of(results, &c).text(should_pass(&c))
                );
            }
            let _ = writeln!(s, "{}", line.trim_end());
        }
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "  operator transmissions the node decoded ({}), %: exactly / word for word \
             with noise characters around",
            k.name()
        );
        let mut head = format!("{:>5} |", "wpm");
        for &snr in &spec.snrs {
            let _ = write!(head, " {:>8}", snr_head(snr));
        }
        let _ = writeln!(s, "{head}");
        for &wpm in &spec.wpms {
            let mut line = format!("{wpm:>5} |");
            for &snr_db in &spec.snrs {
                let t = Tally::of(
                    results,
                    &Cell {
                        keying: k,
                        wpm,
                        snr_db,
                    },
                );
                if t.transmissions == 0 {
                    let _ = write!(line, " {:>8}", "-");
                } else {
                    let pct = |n: u32| (100.0 * n as f32 / t.transmissions as f32).round();
                    let cell = format!("{}/{}", pct(t.decoded_exact), pct(t.decoded_intact));
                    let _ = write!(line, " {cell:>8}");
                }
            }
            let _ = writeln!(s, "{line}");
        }
        let _ = writeln!(s);
        s.push_str(&render_edges(spec, results, k));
        let _ = writeln!(s);
    }
    let _ = writeln!(
        s,
        "cells: successes/trials; +n.n extra transmissions per success, on average (repeats, NO and the restart) \
         (an exchange with none is 2 transmissions: open and OK); w a garbled read-back \
         that still parsed, caught by the operator (NO, then fresh lines); T hit the \
         time limit; W! WRONG MESSAGE DELIVERED; S! SAFETY VIOLATION; [ ] should-pass \
         region ({SHOULD_PASS})."
    );
    let _ = writeln!(s, "{TIMING_NOTE}");
    let n = results.len();
    let ok = results.iter().filter(|r| r.success).count();
    let sum = |f: fn(&TrialResult) -> u32| results.iter().map(f).sum::<u32>();
    let _ = writeln!(
        s,
        "\n{n} runs: {ok} succeeded, {} failed. {} garbled read-backs caught by the \
         operator, {} wrong messages delivered, {} runs with a safety violation, {} \
         timed out.",
        n - ok,
        sum(|r| r.wrong_read_backs),
        sum(|r| r.wrong_delivered.len() as u32),
        results.iter().filter(|r| !r.hard.is_empty()).count(),
        results.iter().filter(|r| r.timed_out).count(),
    );
    let v = verdict(results);
    for r in &v.hard {
        let _ = writeln!(
            s,
            "\n!!! HARD FAILURE: {} trial {} (audio seed {:#x}): {}",
            r.cell,
            r.trial + 1,
            r.seed,
            r.why
        );
    }
    for r in &v.region_failures {
        if !r.hard_failure() {
            let _ = writeln!(
                s,
                "\n!!! SHOULD-PASS FAILURE: {} trial {} (audio seed {:#x}): {}",
                r.cell,
                r.trial + 1,
                r.seed,
                r.why
            );
        }
    }
    let _ = writeln!(
        s,
        "\n{}",
        if v.ok() {
            "verdict: PASS (no hard failures; every should-pass trial succeeded)".to_string()
        } else {
            format!(
                "verdict: FAIL ({} hard failures, {} should-pass trials failed)",
                v.hard.len(),
                v.region_failures.len()
            )
        }
    );
    s
}

/// The edges for one keying: per speed the lowest SNR down to which every trial
/// passes and where repeats start; at 10 dB the slowest and fastest passing speed.
fn render_edges(spec: &SweepSpec, results: &[TrialResult], keying: Keying) -> String {
    let mut s = format!("  edges ({}):\n", keying.name());
    let tally = |wpm: f32, snr_db: Option<f32>| {
        Tally::of(
            results,
            &Cell {
                keying,
                wpm,
                snr_db,
            },
        )
    };
    for &wpm in &spec.wpms {
        // From clean down, while every trial passes.
        let mut lowest = None;
        for &snr in &spec.snrs {
            if !tally(wpm, snr).all_pass() {
                break;
            }
            lowest = Some(snr);
        }
        let below: Vec<String> = spec
            .snrs
            .iter()
            .skip_while(|&&snr| Some(snr) != lowest)
            .skip(usize::from(lowest.is_some()))
            .filter(|&&snr| tally(wpm, snr).all_pass())
            .map(|&snr| snr_head(snr))
            .collect();
        let repeats = spec
            .snrs
            .iter()
            .find(|&&snr| tally(wpm, snr).repeats().is_some())
            .map(|&snr| format!("repeats from {}", snr_head(snr)))
            .unwrap_or_else(|| "no repeats".into());
        let _ = writeln!(
            s,
            "    {wpm:>4} wpm: all trials pass down to {}{}; {repeats}",
            lowest.map_or_else(
                || format!(
                    "(none: fails at {})",
                    spec.snrs.first().map_or("-".into(), |&x| snr_head(x))
                ),
                snr_head
            ),
            if below.is_empty() {
                String::new()
            } else {
                format!(" (and again at {})", below.join(", "))
            }
        );
    }
    if spec.snrs.contains(&Some(10.0)) {
        let pass: Vec<f32> = spec
            .wpms
            .iter()
            .copied()
            .filter(|&w| tally(w, Some(10.0)).all_pass())
            .collect();
        match (pass.first(), pass.last()) {
            (Some(lo), Some(hi)) => {
                let gaps: Vec<String> = spec
                    .wpms
                    .iter()
                    .filter(|&&w| w > *lo && w < *hi && !pass.contains(&w))
                    .map(|w| format!("{w}"))
                    .collect();
                let _ = writeln!(
                    s,
                    "    at 10 dB: slowest passing {lo} wpm, fastest passing {hi} wpm{}",
                    if gaps.is_empty() {
                        String::new()
                    } else {
                        format!(" (but not {} wpm)", gaps.join(", "))
                    }
                );
            }
            _ => {
                let _ = writeln!(s, "    at 10 dB: no speed passes every trial");
            }
        }
    }
    s
}

/// The raw results, one row per run.
pub fn sweep_csv(results: &[TrialResult]) -> String {
    let mut s = String::from(
        "keying,wpm,snr_db,trial,audio_seed,success,delivered,wrong_delivered,safety_violation,\
         timed_out,transmissions,repeats,restarts,wrong_read_backs,receptions,decoded_exact,\
         decode_mismatches,decoded_intact,should_pass,wall_s,radio_s,why\n",
    );
    for r in results {
        let _ = writeln!(
            s,
            "{},{},{},{},{:#x},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.2},{:.0},\"{}\"",
            r.cell.keying.name(),
            r.cell.wpm,
            r.cell.snr_db.map_or("clean".into(), |x| x.to_string()),
            r.trial + 1,
            r.seed,
            r.success,
            r.delivered,
            r.wrong_delivered.len(),
            !r.hard.is_empty(),
            r.timed_out,
            r.transmissions,
            r.repeats,
            r.restarts,
            r.wrong_read_backs,
            r.receptions,
            r.decoded_exact,
            r.decode_mismatches,
            r.decoded_intact,
            should_pass(&r.cell),
            r.wall.as_secs_f32(),
            r.radio_time.as_secs_f32(),
            r.why.replace('"', "\"\"")
        );
    }
    s
}

// ---------------------------------------------------------------------------
// Test vectors: the field operator's side, as audio files.

/// One field transmission.
#[derive(Debug, Clone)]
pub struct Vector {
    pub label: &'static str,
    /// With `{n}` placeholders.
    pub text: &'static str,
    /// What a node configured as in the manifest answers, played in order.
    pub reply: &'static str,
}

/// The field transmissions of a session covering the grammar, in order.
pub fn vectors() -> Vec<Vector> {
    let v = |label, text, reply| Vector { label, text, reply };
    vec![
        v(
            "open-tx",
            "W5XXX 42 {42} TX MOM RUNNING LATE HOME SUN K",
            "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K",
        ),
        v("commit-tx", "OK 43 {43} K", "SENT 43 DE N0DE K"),
        v("again", "AGN 44 {44} K", "SENT 43 DE N0DE K"),
        v(
            "replayed-open",
            "W5XXX 42 {42} TX MOM RUNNING LATE HOME SUN K",
            "(silence: line 42 is used)",
        ),
        v("open-rx", "W5XXX 45 {45} RX K", "R 45 <n> MSGS ? DE N0DE K"),
        v(
            "commit-rx",
            "OK 46 {46} K",
            "the messages, or R 46 NIL DE N0DE K",
        ),
        v(
            "again-chunk",
            "AGN 47 {47} A K",
            "chunk A again (silence if there was none)",
        ),
        v("open-wx", "W5XXX 48 {48} WX K", "R 48 WX DL89 ? DE N0DE K"),
        v("commit-wx", "OK 49 {49} K", "the forecast"),
        v(
            "open-wx-grid",
            "W5XXX 50 {50} WX DL88 K",
            "R 50 WX DL88 ? DE N0DE K",
        ),
        v(
            "open-wx-preset",
            "W5XXX 51 {51} WX 2 K",
            "R 51 WX 2 DL89ME ? DE N0DE K",
        ),
        v("abort", "NO 52 {52} K", "R NO DE N0DE K"),
        v(
            "stale-line",
            "W5XXX 41 {41} RX K",
            "(silence: line 41 is below last_seq)",
        ),
        v(
            "wrong-code",
            "W5XXX 53 {54} RX K",
            "(silence: the code is line 54's)",
        ),
        v(
            "garbled-call",
            "W5XKX 53 {53} TX MOM HI K",
            "R 53 TX MOM HI ? DE N0DE K",
        ),
        v(
            "over-word",
            "W5XXX 54 {54} TX MOM BACK IN SK I AM IN A MINE TOWN K",
            "R 54 TX MOM BACK IN SK I AM IN A MINE TOWN ? DE N0DE K",
        ),
        v("commit-over-word", "OK 55 {55} K", "SENT 55 DE N0DE K"),
        v(
            "open-kn",
            "W5XXX 56 {56} TX MOM BRING VITAMIN K (",
            "R 56 TX MOM BRING VITAMIN K ? DE N0DE K",
        ),
        v("commit-kn", "OK 57 {57} (", "SENT 57 DE N0DE K"),
    ]
}

/// What `audio` decodes to, as `hfnode decode` shows it.
pub fn decode_text(audio: &[f32], sample_rate: u32, pitch: f32) -> String {
    let mut d = cw::Decoder::new(cw::DecoderConfig::new(sample_rate, pitch));
    let mut ev = d.push(audio);
    ev.extend(d.flush());
    cw::events_to_text(&ev).trim().to_string()
}

/// What the node makes of `text` with the manifest's scratch config.
fn parsed(text: &str) -> Option<protocol::FieldMsg> {
    let vocab = Vocabulary {
        field_calls: vec![FIELD_CALL.into()],
        contacts: vec!["MOM".into(), "BOB".into()],
        presets: vec![1, 2],
    };
    parse(text, &vocab).ok()
}

/// The manifest's reply for a vector that does not decode to what was sent.
pub const NOT_THIS_REPLY: &str = "(not this reply: the noise changes what the node reads)";

/// Write every vector at each speed and noise level as a WAV file in `dir`, with
/// `manifest.txt` and the test key. Returns the files written.
pub fn write_vectors(
    dir: &Path,
    wpms: &[f32],
    snrs: &[Option<f32>],
    jitter: f32,
    pitch: f32,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let book = CodeBook::new(TEST_KEY);
    let key = dir.join("test-only.key");
    std::fs::write(&key, TEST_KEY)?;
    let mut manifest = format!(
        "# hfnode test vectors: the field operator's side of a session, as CW audio.\n\
         #\n\
         # TEST ONLY. The codes come from the fixed self-test key in test-only.key,\n\
         # which anyone can compute. Never use it as a node's key on the air.\n\
         #\n\
         # Decode-only check:  hfnode decode <file> --pitch {pitch}\n\
         #   Each file decodes to its `decodes_as` column: for a clean file exactly the\n\
         #   `text`, for a noisy one with the decoder's errors at that SNR. Where those\n\
         #   errors change what the node reads (a lost or garbled callsign, number or\n\
         #   code; a noise character it cannot strip), `reply` says so instead of\n\
         #   giving one: expect silence or a different reply from that file.\n\
         # Against a bench node: use a scratch config with node_call = \"{NODE_CALL}\",\n\
         #   field_calls = [\"{FIELD_CALL}\"], contacts MOM and BOB, weather.default_grid\n\
         #   = \"{WX_DEFAULT_GRID}\" with presets 1 = DL89IG and 2 = DL89ME, auth.key_file =\n\
         #   test-only.key, and a scratch state_dir whose last_seq is {START_SEQ} (or no\n\
         #   last_seq file at all) and no wx_last.json. Play the files of one speed and\n\
         #   noise level in order, waiting for each reply; `reply` is what the node\n\
         #   should key.\n\
         #\n\
         # Mono 16-bit {SAMPLE_RATE} Hz, {pitch} Hz tone, 1 s of silence before and after.\n\
         #\n\
         # file\twpm\tsnr_db\ttext\tdecodes_as\treply\n"
    );
    let mut files = Vec::new();
    for &wpm in wpms {
        for &snr in snrs {
            let level = snr.map_or("clean".to_string(), |s| format!("{s}db"));
            for (i, vec) in vectors().iter().enumerate() {
                let text = with_codes(vec.text, &book);
                let mut k = Keyer::new(SAMPLE_RATE, pitch, wpm);
                k.amplitude = AMPLITUDE;
                k.jitter = jitter;
                k.seed = mix(&[i as u64, wpm.to_bits() as u64]);
                let mut audio = k.render(&text, 1000.0);
                if let Some(snr) = snr {
                    Noise::new(mix(&[i as u64, wpm.to_bits() as u64, snr.to_bits() as u64])).add(
                        &mut audio,
                        Noise::sigma_for_snr(AMPLITUDE, snr, SAMPLE_RATE, 2500.0),
                    );
                }
                let name = format!("{:02}-{}-{wpm}wpm-{level}.wav", i + 1, vec.label);
                let path = dir.join(&name);
                audio::write_wav(&path, &audio, SAMPLE_RATE)?;
                // As read back from the file, which `hfnode decode` decodes.
                let (written, rate) = audio::read_wav(&path)?;
                let decoded = decode_text(&written, rate, pitch);
                let reply = if parsed(&decoded) == parsed(&text) {
                    vec.reply
                } else {
                    NOT_THIS_REPLY
                };
                let _ = writeln!(
                    manifest,
                    "{name}\t{wpm}\t{}\t{text}\t{decoded}\t{reply}",
                    snr.map_or("-".to_string(), |s| s.to_string()),
                );
                files.push(path);
            }
        }
    }
    std::fs::write(dir.join("manifest.txt"), manifest)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use civ::mock::Keyed;

    /// Played in order into a node configured as the manifest says, each vector
    /// gets the reply the manifest gives (where that is a literal over or silence).
    #[test]
    fn test_vectors_get_the_manifest_replies_when_played_in_order() {
        use crate::session::Outcome as Answer;
        let dir = Scratch::new("vectors").unwrap();
        let cfg = config(&base("vectors", "manifest config"), &dir.0, 1.0).unwrap();
        let mut session = node::build_session(&cfg).unwrap();
        let mut svc = FakeServices::default();
        let book = CodeBook::new(TEST_KEY);
        let t0 = Instant::now();
        for (i, v) in vectors().iter().enumerate() {
            let got = session.handle(
                &with_codes(v.text, &book),
                t0 + Duration::from_secs(10 * i as u64),
                &mut svc,
            );
            if v.reply.starts_with("(silence") {
                assert!(matches!(got, Answer::Silent(_)), "{}: {got:?}", v.label);
            } else if !v
                .reply
                .contains(|c: char| c == '<' || c.is_ascii_lowercase())
            {
                match got {
                    Answer::Transmit(t) => assert_eq!(t.text(), v.reply, "{}", v.label),
                    Answer::Silent(why) => panic!("{}: silent ({why})", v.label),
                }
            }
        }
    }

    #[test]
    fn codes_are_substituted() {
        let book = CodeBook::new(TEST_KEY);
        assert_eq!(
            with_codes("W5XXX 42 {42} TX MOM {X} K", &book),
            format!("W5XXX 42 {} TX MOM {{X}} K", book.code(42))
        );
    }

    fn piece(text: &str, sent: &str, complete: bool) -> Keyed {
        Keyed {
            text: text.into(),
            sent: sent.into(),
            accepted: Duration::ZERO,
            start: Duration::ZERO,
            end: Duration::ZERO,
            complete,
            on_air: true,
        }
    }

    #[test]
    fn overs_are_matched_piece_by_piece() {
        let rb = "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K".to_string();
        let a = piece(
            "R 42 TX MOM RUNNING LATE HOME",
            "R 42 TX MOM RUNNING LATE HOME",
            true,
        );
        let b = piece("SUN ? DE N0DE K", "SUN ? DE N0DE K", true);
        let s = piece("SENT 43 DE N0DE K", "SENT 43 DE N0DE K", true);
        let full = [Over::Full(rb.clone()), Over::Full(s.text.clone())];
        assert!(match_overs(&[a.clone(), b.clone(), s.clone()], &full).is_ok());
        assert!(
            match_overs(&[a.clone(), b.clone()], &full).is_err(),
            "missing"
        );
        assert!(
            match_overs(&[a.clone(), b.clone(), s.clone(), s.clone()], &full).is_err(),
            "extra"
        );
        // Cut part-way through a piece, or at a piece boundary.
        let cut = piece("R 42 TX MOM RUNNING LATE HOME", "R 42 TX M", false);
        let mut with_cut = vec![Over::Cut(rb.clone())];
        with_cut.extend(full.clone());
        assert!(match_overs(&[cut, a.clone(), b.clone(), s.clone()], &with_cut).is_ok());
        assert!(match_overs(&[a.clone(), a.clone(), b.clone(), s.clone()], &with_cut).is_ok());
        assert!(
            match_overs(&[a.clone(), b.clone(), s.clone()], &with_cut).is_err(),
            "not cut"
        );
        // A cut where a whole over was expected.
        let cut = piece("R 42 TX MOM RUNNING LATE HOME", "R 42", false);
        assert!(match_overs(&[cut, b, s], &full).is_err());
    }

    #[test]
    fn station_ids_are_counted_and_bounded() {
        let s = |secs: u64| Duration::from_secs(secs);
        let at = |text: &str, from: u64, to: u64| Keyed {
            accepted: s(from),
            start: s(from),
            end: s(to),
            ..piece(text, text, true)
        };
        let mut e = base("ids", "").expect;
        // A window ID after the tune at 0 s, an over, and a long over with an ID.
        let mut keyed = vec![
            at("DE N0DE", 3, 7),
            at("R 42 1 MSG ? DE N0DE K", 40, 50),
            at("NR 1 FM MOM TEST = A", 70, 300),
            at("DE N0DE", 302, 306),
            at("TEST = B DE N0DE K", 308, 700),
        ];
        let tunes = [s(0)];
        assert!(
            !station_id_check(&keyed, &tunes, &e, 100.0).pass,
            "mid ID not expected"
        );
        e.mid_ids = 1;
        assert!(station_id_check(&keyed, &tunes, &e, 100.0).pass);
        // No window ID after a tune that matched.
        assert!(!station_id_check(&keyed[1..], &tunes, &e, 100.0).pass);
        // Too long without one: 70 s to 700 s.
        keyed.remove(3);
        e.mid_ids = 0;
        let c = station_id_check(&keyed, &tunes, &e, 100.0);
        assert!(!c.pass && c.detail.contains("630 s"), "{}", c.detail);
        // An over cut short by a fault ends the stretch.
        keyed[2].complete = false;
        assert!(station_id_check(&keyed, &tunes, &e, 100.0).pass);
    }

    /// An outcome whose checks all pass, as a clean sweep run's would.
    fn sweep_outcome(sent: &[(&str, &str)]) -> Outcome {
        let rb = "W5XXX 42 ZWEXVXGQ TX MOM RUNNING LATE HOME SUN K".to_string();
        let ok = "OK 43 WSQIZJVX K".to_string();
        Outcome {
            scenario: "x".into(),
            checks: ["script", "ended", "keyed", "gateway", "ci-v", "safety"]
                .into_iter()
                .map(|n| check(n, true, ""))
                .collect(),
            wall: Duration::ZERO,
            radio_time: Duration::ZERO,
            transcript: Vec::new(),
            facts: Facts {
                sent: sent
                    .iter()
                    .map(|(d, t)| (d.to_string(), t.to_string()))
                    .collect(),
                operator_sent: vec![rb.clone(), ok.clone()],
                extra_transmissions: 0,
                received: vec![format!("E E {rb}"), ok],
                exchanges_done: 1,
                ..Facts::default()
            },
        }
    }

    const CELL: Cell = Cell {
        keying: Keying::Machine,
        wpm: 18.0,
        snr_db: Some(10.0),
    };

    #[test]
    fn a_sweep_success_is_the_exact_message_delivered_once() {
        let r = classify(&CELL, 0, 1, false, &sweep_outcome(&[("MOM", SWEEP_TEXT)]));
        assert!(r.success && r.delivered && !r.hard_failure(), "{r:?}");
        assert_eq!((r.transmissions, r.repeats), (2, 0));
        // The open came with noise in front of it: intact, not exact.
        assert_eq!(
            (r.decoded_exact, r.decoded_intact, r.decode_mismatches),
            (1, 2, 1)
        );
    }

    #[test]
    fn a_wrong_or_repeated_message_is_never_a_success() {
        for sent in [
            vec![("MOM", "RUNNING LAEE HOME SUN")],
            vec![("BOB", SWEEP_TEXT)],
            vec![("MOM", SWEEP_TEXT), ("MOM", SWEEP_TEXT)],
            vec![("MOM", SWEEP_TEXT), ("MOM", "RUNNING LATE")],
        ] {
            let r = classify(&CELL, 0, 1, false, &sweep_outcome(&sent));
            assert!(!r.success, "{sent:?}");
            assert!(r.hard_failure(), "{sent:?}");
            assert_eq!(r.wrong_delivered.len(), 1, "{sent:?}");
            assert!(r.why.contains("WRONG MESSAGE DELIVERED"), "{}", r.why);
            let text = render_sweep(
                &SweepSpec {
                    wpms: vec![18.0],
                    snrs: vec![Some(10.0)],
                    keyings: vec![Keying::Machine],
                    trials: 1,
                    rx: false,
                },
                &[r],
            );
            assert!(text.contains("[0/1W!]"), "{text}");
            assert!(text.contains("verdict: FAIL"), "{text}");
        }
    }

    #[test]
    fn a_safety_violation_is_a_hard_failure_whatever_the_snr() {
        let mut out = sweep_outcome(&[("MOM", SWEEP_TEXT)]);
        out.checks.push(check("safety", false, "key down for 99 s"));
        let cell = Cell {
            snr_db: Some(-6.0),
            ..CELL
        };
        let r = classify(&cell, 0, 1, false, &out);
        assert!(!r.success && r.hard_failure());
        assert!(!should_pass(&cell));
        let v = [r];
        assert!(!verdict(&v).ok());
        // Failing the checks a sweep run does not hold to is no hard failure.
        let mut out = sweep_outcome(&[]);
        out.checks
            .push(check("keyed", false, "not what a clean run keys"));
        out.facts.script_failures = vec!["no read-back".into()];
        out.facts.exchanges_done = 0;
        let r = classify(&cell, 0, 1, false, &out);
        assert!(!r.success && !r.hard_failure() && !r.delivered);
        assert!(verdict(&[r]).ok());
    }

    #[test]
    fn a_missing_station_id_is_a_hard_failure() {
        // Identifying does not depend on decoding: no noise excuses a missing ID.
        let mut out = sweep_outcome(&[("MOM", SWEEP_TEXT)]);
        out.checks
            .push(check("station ID", false, "0 after 1 tunes (expected 1)"));
        let cell = Cell {
            snr_db: Some(-6.0),
            ..CELL
        };
        let r = classify(&cell, 0, 1, false, &out);
        assert!(!r.success && r.hard_failure());
        assert!(!verdict(&[r]).ok());
    }

    #[test]
    fn sweep_grid_and_csv() {
        let spec = SweepSpec {
            wpms: vec![20.0, 5.0, 20.0],
            snrs: vec![Some(-3.0), None, Some(10.0)],
            keyings: vec![Keying::Hand, Keying::Hand],
            trials: 2,
            rx: false,
        }
        .normalized();
        assert_eq!(spec.wpms, [5.0, 20.0]);
        assert_eq!(spec.snrs, [None, Some(10.0), Some(-3.0)]);
        assert_eq!(spec.keyings, [Keying::Hand]);
        assert_eq!(spec.runs().len(), 12);
        let mut r = classify(&CELL, 0, 1, false, &sweep_outcome(&[]));
        r.why = "said \"no\", twice".into();
        let csv = sweep_csv(&[r]);
        assert_eq!(csv.lines().count(), 2);
        // The seed fixes the audio only; the column says so.
        assert!(
            csv.starts_with("keying,wpm,snr_db,trial,audio_seed,"),
            "{csv}"
        );
        assert!(csv.ends_with(",\"said \"\"no\"\", twice\"\n"), "{csv}");
        assert_eq!(
            csv.lines().next().unwrap().split(',').count(),
            csv.lines().nth(1).unwrap().split(',').count() - 1
        );
    }
}
