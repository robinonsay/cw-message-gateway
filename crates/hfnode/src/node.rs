//! The running node: audio in, decode, session, transmit, on a listening schedule.
//!
//! By default the node listens all the time ([`crate::config::Schedule`]). It tunes
//! when it starts listening, at start-up or at the top of a window, and identifies
//! that tune with `DE <call>` if it matched. It tunes again before a reply once
//! that tune is older than `retune_minutes`, and the reply identifies it, so a node
//! that listens all day puts out no carriers of its own until there is something
//! to send. While it hears nothing it sets the radio up again and checks it every
//! `check_minutes`, without transmitting, and the station does the same before
//! every transmission. The decoder also goes back to its starting speed after a
//! quiet minute, so hours of band noise do not garble the next caller's first
//! words.

use crate::audio::{Block, BlockReceiver};
use crate::config::Config;
use crate::gateway::{self, filter, LiveServices};
use crate::inbox::Inbox;
use crate::places::LastPlaces;
use crate::session::{Outcome, Services, Session, SessionConfig};
use crate::station::Station;
use anyhow::{Context, Result};
use auth::{CodeBook, SeqStore, Verifier};
use civ::Rig;
use cw::{events_to_text, DecodeEvent, Decoder, DecoderConfig};
use protocol::{sanitize, Vocabulary, OVERS};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub fn load_codebook(cfg: &Config) -> Result<CodeBook> {
    let key = std::fs::read(&cfg.auth.key_file)
        .with_context(|| format!("reading {}", cfg.auth.key_file.display()))?;
    if key.len() < 16 {
        anyhow::bail!("{} is too short to be a key", cfg.auth.key_file.display());
    }
    Ok(match &cfg.auth.alphabet {
        Some(a) => CodeBook::with_alphabet(&key, a)?,
        None => CodeBook::new(&key),
    })
}

pub fn session_config(cfg: &Config) -> SessionConfig {
    SessionConfig {
        node_call: cfg.station.node_call.to_ascii_uppercase(),
        pending_timeout: Duration::from_secs(cfg.pending_timeout_secs),
        chunk_chars: cfg.station.chunk_chars,
        max_rx_messages: 5,
        again_window: Duration::from_secs(cfg.pending_timeout_secs.max(600)),
        wx_default_grid: cfg
            .weather
            .as_ref()
            .map(|w| w.default_grid.to_ascii_uppercase()),
        wx_presets: cfg.weather_presets(),
    }
}

pub fn build_session(cfg: &Config) -> Result<Session> {
    build_session_with(cfg, session_config(cfg))
}

/// [`build_session`] with the session's settings given, e.g. time-scaled for tests.
pub fn build_session_with(cfg: &Config, sc: SessionConfig) -> Result<Session> {
    let book = load_codebook(cfg)?;
    let store = SeqStore::new(cfg.state_dir.join("last_seq"));
    let last = store.load().context("loading last_seq")?;
    log::info!("last_seq is {last}");
    Ok(Session::new(
        sc,
        Vocabulary {
            field_calls: cfg
                .station
                .field_calls
                .iter()
                .map(|c| c.to_ascii_uppercase())
                .collect(),
            contacts: cfg.contact_names(),
            presets: cfg.weather_presets().into_iter().map(|(n, _)| n).collect(),
        },
        Verifier::new(book, last),
        store,
        LastPlaces::open(cfg.state_dir.join("wx_last.json")),
    ))
}

pub fn open_inbox(cfg: &Config) -> Result<Arc<Mutex<Inbox>>> {
    Ok(Arc::new(Mutex::new(Inbox::open(
        cfg.state_dir.join("inbox.json"),
    )?)))
}

/// Times the filter may time out on one message before it is withheld. Each try
/// holds up the mail check, and a model too slow for a message stays too slow.
const MAX_FILTER_TIMEOUTS: u32 = 3;

/// Screen every unscreened message. Messages stay unscreened (and are never keyed)
/// if the filter cannot be reached; a message the filter has timed out on
/// [`MAX_FILTER_TIMEOUTS`] times, counted in `timeouts`, is withheld.
pub fn screen_inbox(
    cfg: &Config,
    inbox: &Arc<Mutex<Inbox>>,
    filter: Option<&filter::Screener>,
    timeouts: &mut HashMap<u64, u32>,
) {
    let pending = inbox.lock().map(|i| i.unscreened()).unwrap_or_default();
    for m in pending {
        let on_air = sanitize(&m.raw);
        let screened = if !cfg.filter.enabled {
            on_air
        } else if let Some(f) = filter {
            match f.screen(&m.from, &on_air) {
                Ok(v) => {
                    log::info!(
                        "filter: message {} from {}: {:?} ({})",
                        m.id,
                        m.from,
                        v.action,
                        v.reason
                    );
                    filter::apply(&on_air, &v)
                }
                Err(e) if filter::timed_out(&e) => {
                    let n = timeouts.entry(m.id).or_default();
                    *n += 1;
                    if *n < MAX_FILTER_TIMEOUTS {
                        log::warn!("filter timed out ({n}), message {} held: {e:#}", m.id);
                        continue;
                    }
                    log::warn!("filter timed out {n} times, message {} withheld", m.id);
                    timeouts.remove(&m.id);
                    filter::WITHHELD.to_string()
                }
                Err(e) => {
                    log::warn!("filter unavailable, message {} held: {e:#}", m.id);
                    continue;
                }
            }
        } else {
            continue;
        };
        if let Ok(mut i) = inbox.lock() {
            if let Err(e) = i.set_screened(m.id, &screened) {
                log::error!("inbox write failed: {e:#}");
            }
        }
    }
}

/// Poll email and screen new messages until the process exits.
pub fn spawn_inbound(cfg: Config, inbox: Arc<Mutex<Inbox>>) {
    let Some(email) = cfg.email.clone() else {
        return;
    };
    thread::spawn(move || {
        let filter = if cfg.filter.enabled {
            match filter::Screener::new(&cfg.filter) {
                Ok(f) => {
                    log::info!("inbound filter: {}", f.describe());
                    Some(f)
                }
                Err(e) => {
                    log::error!(
                        "inbound filter not available, inbound messages will be held: {e:#}"
                    );
                    None
                }
            }
        } else {
            log::warn!("inbound filter disabled: third-party text will be transmitted unscreened");
            None
        };
        // Mail ignored under an earlier configuration is considered again once.
        let mut retry_ignored = true;
        let mut timeouts = HashMap::new();
        loop {
            match gateway::email::poll_imap(&email, &cfg.contacts, &inbox, retry_ignored) {
                Ok(n) => {
                    retry_ignored = false;
                    if n > 0 {
                        log::info!("{n} new inbound message(s)");
                    }
                }
                Err(e) => log::warn!("IMAP poll failed: {e:#}"),
            }
            screen_inbox(&cfg, &inbox, filter.as_ref(), &mut timeouts);
            thread::sleep(Duration::from_secs(email.poll_secs.max(30)));
        }
    });
}

pub fn live_services(cfg: &Config, inbox: Arc<Mutex<Inbox>>) -> Result<LiveServices> {
    let field_call = cfg.station.field_calls.first().cloned().unwrap_or_default();
    let mailer = match &cfg.email {
        Some(e) => Some(gateway::email::Mailer::new(e, &field_call)?),
        None => None,
    };
    let weather = cfg.weather.as_ref().map(gateway::weather::Nws::new);
    Ok(LiveServices {
        cfg: cfg.clone(),
        inbox,
        mailer,
        weather,
    })
}

fn decoder_for(cfg: &Config) -> Decoder {
    let mut d = DecoderConfig::new(cfg.audio.sample_rate, cfg.audio.pitch_hz);
    d.bandwidth_hz = cfg.audio.bandwidth_hz;
    Decoder::new(d)
}

/// Audio still discarded once the radio is back on receive after transmitting or
/// tuning, while the receiver and its AGC recover. It also covers the part of the
/// first block read afterwards that was captured before.
const RX_RECOVERY_MS: u64 = 250;

/// Audio, in ms, after which an idle decoder goes back to its initial speed
/// estimate ([`Decoder::reset_speed`]). Minutes of band noise teach it a speed no
/// sender is using, which garbles the start of the next call; a minute of noise is
/// too little to do that.
const SPEED_REFRESH_MS: u64 = 60_000;

/// Discards audio captured while the node was transmitting or tuning.
#[derive(Debug, Default)]
struct TxGuard {
    /// Blocks read before this were captured while transmitting or tuning.
    until: Option<Instant>,
    /// Samples still to discard after that.
    recovery: usize,
}

impl TxGuard {
    /// Transmitting or tuning ended at `at`; discard `recovery` more samples after it.
    fn ended(&mut self, at: Instant, recovery: usize) {
        self.until = Some(at);
        self.recovery = recovery;
    }

    /// Whether `block` may be decoded. The recovery margin is counted in samples
    /// rather than wall time, so it holds however fast the audio is delivered.
    fn keep(&mut self, block: &Block) -> bool {
        if self.until.is_some_and(|t| block.at < t) {
            return false;
        }
        if self.recovery > 0 {
            self.recovery = self.recovery.saturating_sub(block.samples.len());
            return false;
        }
        true
    }
}

/// Silence before a word, in dits, below which it follows the word before at the
/// sender's rhythm (a word gap is 7 dits; hand keyers stretch it): it is more of
/// the message, not a noise burst.
const RHYTHM_DITS: u64 = 12;

/// A word an isolated noise burst could have produced: one or two characters of
/// one or two elements.
fn is_noise_word(w: &str) -> bool {
    w.len() <= 2 && w.chars().all(|c| "ETIANM".contains(c))
}

/// If the last real word decoded is an over prosign followed by a word gap, the
/// number of events up to and including that gap. Anything after it is noise.
fn over_end(events: &[DecodeEvent]) -> Option<usize> {
    let mut over = None;
    let mut word = String::new();
    for (i, e) in events.iter().enumerate() {
        match e {
            DecodeEvent::Char(c) => word.push(*c),
            DecodeEvent::Unknown(_) => word.push('*'),
            DecodeEvent::WordGap if word.is_empty() => {}
            DecodeEvent::WordGap => {
                if OVERS.contains(&word.as_str()) {
                    over = Some(i + 1);
                } else if !is_noise_word(&word) {
                    over = None;
                }
                word.clear();
            }
        }
    }
    // A word without its gap yet (after a flush) ends the message only as noise.
    if is_noise_word(&word) || word.is_empty() {
        over
    } else {
        None
    }
}

/// Whether to listen: in a scheduled window, and past its end while `held` (the
/// session still owes the field operator something, see [`held_for`]) or while a
/// transmission heard is still being received. Only the schedule opens a window:
/// opening one tunes.
fn listening(
    scheduled: bool,
    held: bool,
    was_open: bool,
    decoder: &Decoder,
    events: &[DecodeEvent],
) -> bool {
    scheduled || (was_open && (held || decoder.has_partial() || !events.is_empty()))
}

/// What the node still listens for past the end of its window, if anything: the
/// commit (or `NO`) of a pending transaction, or a repeated `OK` or `AGN` while the
/// last result can still be had again. The result is worth waiting for only if the
/// station could key it; a pending commit is acted on (and emailed) either way.
fn held_for<R: Rig + 'static>(
    session: &Session,
    station: &Station<R>,
    now: Instant,
) -> Option<&'static str> {
    if session.has_pending(now) {
        Some("for the OK or NO of the pending transaction")
    } else if station.can_transmit() && session.result_repeatable(now) {
        Some("for a repeated OK or AGN of the last result")
    } else {
        None
    }
}

/// Seconds from `since` to `now` on the node's clock. A clock set back (a time
/// sync after boot) counts as a long time, so that what is due is not put off.
fn secs_since(since: u64, now: u64) -> u64 {
    now.checked_sub(since).unwrap_or(u64::MAX)
}

fn log_rx(rx_log: &Path, text: &str) {
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(rx_log)
        .and_then(|mut f| writeln!(f, "{},{text}", gateway::unix_now()));
}

/// The main loop. Returns only on a fatal error or when the audio source ends.
pub fn run<R: Rig + 'static>(
    cfg: &Config,
    station: &mut Station<R>,
    audio: &BlockReceiver,
    session: &mut Session,
    svc: &mut dyn Services,
) -> Result<()> {
    run_with_clock(cfg, station, audio, session, svc, &gateway::unix_now)
}

/// [`run`], with the listening schedule following `clock` (Unix seconds) instead of
/// the system clock, e.g. a time-scaled one in tests.
pub fn run_with_clock<R: Rig + 'static>(
    cfg: &Config,
    station: &mut Station<R>,
    audio: &BlockReceiver,
    session: &mut Session,
    svc: &mut dyn Services,
    clock: &dyn Fn() -> u64,
) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let rx_log = cfg.state_dir.join("rx.log");
    let sample_rate = u64::from(cfg.audio.sample_rate.max(1));
    let recovery = (sample_rate * RX_RECOVERY_MS / 1000) as usize;
    let eom = cfg.audio.end_of_message_ms;
    let mut decoder = decoder_for(cfg);
    let mut events: Vec<DecodeEvent> = Vec::new();
    // (over_end of `events`, ms of audio since it was decoded, whether every word
    // since came after a longer silence than the sender's word gaps).
    let mut over: Option<(usize, u64, bool)> = None;
    // Longest key-up time seen since `events` last grew.
    let mut max_idle: u64 = 0;
    let mut guard = TxGuard::default();
    let mut was_open = false;
    // Whether the schedule was open at the last block: a window starts when it
    // opens, also while the session still holds the last one open.
    let mut was_scheduled = false;
    // What the node listens for past the end of its window, as last logged.
    let mut overtime: Option<&str> = None;
    let check_secs = u64::from(cfg.schedule.check_minutes) * 60;
    let retune_secs = u64::from(cfg.schedule.retune_minutes) * 60;
    // Clock times of the last tune (whatever came of it, once the tuner started)
    // and of the last time the radio was set up and checked.
    let mut tuned_at: Option<u64> = None;
    let mut checked_at = 0;
    // Audio since the decoder last started from its initial speed, or last heard
    // the node transmit (the reply that follows keeps the sender's speed).
    let mut speed_age_ms: u64 = 0;
    loop {
        let block = match audio.recv_timeout(Duration::from_secs(5)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => {
                log::warn!("no audio for 5 s");
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("audio source ended"),
        };
        if !guard.keep(&block) {
            continue;
        }
        let now = Instant::now();
        let scheduled = cfg.schedule.is_open(clock());
        let hold = held_for(session, station, now);
        let open = listening(scheduled, hold.is_some(), was_open, &decoder, &events);
        let starting = scheduled && !was_scheduled;
        was_scheduled = scheduled;
        // Tell the owner why the node still listens, and may key, outside its
        // schedule: once, and again whenever that changes.
        let waiting = if open && !scheduled { hold } else { None };
        if let Some(why) = waiting {
            if overtime != waiting {
                log::info!("window over: still listening {why}");
            }
        }
        overtime = waiting;
        if starting {
            if was_open {
                // Held open into the next window: it starts as any other, with the
                // radio set up, tuned and identified again. The tune covers anything
                // being heard now.
                log::info!("listening window open (the last one was still held open)");
                events.extend(decoder.flush());
                let text = events_to_text(&events);
                if !text.is_empty() {
                    log::warn!("window start, not handled: {text}");
                    log_rx(&rx_log, &text);
                }
            } else if cfg.schedule.always {
                log::info!("listening");
            } else {
                log::info!("listening window open");
            }
            // Tune, and identify the tune's carrier if it matched.
            if let Err(e) = station.open_window() {
                log::error!("{e}");
            }
            (tuned_at, checked_at) = (station.tuner_ran().then(clock), clock());
            guard.ended(Instant::now(), recovery);
            decoder = decoder_for(cfg);
            speed_age_ms = 0;
            events.clear();
            over = None;
            was_open = true;
            continue;
        }
        if !open {
            if was_open {
                // Normally nothing is left (a reception keeps the window open), but
                // never drop decoded text without a trace.
                events.extend(decoder.flush());
                let text = events_to_text(&events);
                events.clear();
                over = None;
                if !text.is_empty() {
                    log::warn!("window closed, not handled: {text}");
                    log_rx(&rx_log, &text);
                }
                log::info!("listening window closed");
            }
            was_open = false;
            continue;
        }

        // Nothing being received or waited for: a moment to see that the radio is
        // still on the node's frequency and mode, in case someone at the front panel,
        // another program or a power cycle has changed it. Nothing is transmitted,
        // and no audio is dropped.
        let idle = !decoder.key_down()
            && !decoder.has_partial()
            && events.is_empty()
            && !session.has_pending(now);
        if idle && secs_since(checked_at, clock()) >= check_secs {
            if let Err(e) = station.check() {
                log::error!("radio check failed: {e}");
            }
            checked_at = clock();
        }
        if idle && speed_age_ms >= SPEED_REFRESH_MS {
            decoder.reset_speed();
            speed_age_ms = 0;
        }

        let before = events.len();
        events.extend(decoder.push(&block.samples));
        let block_ms = block.samples.len() as u64 * 1000 / sample_rate;
        speed_age_ms += block_ms;
        // The silence before the words just decoded: short if they follow the last
        // ones at the sender's rhythm.
        let gap = max_idle;
        max_idle = max_idle.max(decoder.idle_ms());
        let grew = events.len() > before;
        if grew {
            max_idle = decoder.idle_ms();
        }
        let rhythm_ms = (RHYTHM_DITS as f32 * 1200.0 / decoder.wpm()) as u64;
        over = match (over_end(&events), over) {
            (Some(n), Some((m, ms, isolated))) if n == m => {
                Some((n, ms + block_ms, isolated && !(grew && gap < rhythm_ms)))
            }
            (Some(n), _) => Some((n, 0, true)),
            (None, _) => None,
        };
        let heard = !events.is_empty() || decoder.key_down();
        let quiet = !decoder.key_down() && decoder.idle_ms() >= eom;
        // After an over prosign, isolated noise bursts do not hold the message
        // open. Short words that follow it in rhythm (I AM, AR as a state) may be
        // more of the message, so then only real quiet ends it.
        let over_quiet =
            !decoder.has_partial() && over.is_some_and(|(_, ms, isolated)| isolated && ms >= eom);
        if heard && (quiet || over_quiet) {
            events.extend(decoder.flush());
            if let Some(n) = over_end(&events) {
                let noise = events_to_text(&events[n..]);
                if !noise.is_empty() {
                    log::info!("ignored after over: {noise}");
                }
                events.truncate(n);
            }
            let text = events_to_text(&events);
            events.clear();
            over = None;
            if text.is_empty() {
                continue;
            }
            log::info!("heard: {text}");
            log_rx(&rx_log, &text);
            match session.handle(&text, Instant::now(), svc) {
                Outcome::Silent(why) => log::info!("no reply: {why}"),
                Outcome::Transmit(t) => {
                    if tuned_at.is_none_or(|at| secs_since(at, clock()) >= retune_secs) {
                        // The last tune is too old to trust, and any lockout since it
                        // is due to end: tune again first. Doing it here rather than on
                        // a timer puts the tuner's carrier where the frequency is in
                        // use, just before a transmission that identifies the node.
                        log::info!(
                            "last tune over {} minutes ago: tuning before the reply",
                            cfg.schedule.retune_minutes
                        );
                        if let Err(e) = station.start_window() {
                            log::error!("tune before the reply failed: {e}");
                        }
                        // A tune that never started is tried again before the next.
                        tuned_at = station.tuner_ran().then(clock);
                    }
                    log::info!("sending: {}", t.text());
                    // Sets the radio up and checks it before keying.
                    checked_at = clock();
                    match station.transmit(&t) {
                        // Only messages that actually went out are marked read.
                        Ok(()) if !t.read_ids.is_empty() => svc.mark_read(&t.read_ids),
                        Ok(()) => {}
                        Err(e) => log::error!("transmit failed: {e}"),
                    }
                    // Discard whatever was captured while transmitting, and relearn
                    // the levels, but keep the field operator's speed: their reply
                    // follows at once.
                    guard.ended(Instant::now(), recovery);
                    decoder.reset_levels();
                    speed_age_ms = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio;
    use crate::inbox::Message;
    use crate::session::WxError;
    use crate::station::StationConfig;
    use civ::sim::SimRig;
    use cw::{Keyer, Noise};
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Default)]
    struct Fake {
        sent: Vec<(String, String)>,
    }

    impl Services for Fake {
        fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String> {
            self.sent.push((dest.into(), text.into()));
            Ok(())
        }
        fn ready_messages(&mut self) -> Vec<Message> {
            Vec::new()
        }
        fn mark_read(&mut self, _: &[u64]) {}
        fn weather(&mut self, _: &str) -> Result<String, WxError> {
            Ok("SUNNY".into())
        }
    }

    struct Heard {
        sent: Vec<(String, String)>,
        keyed: String,
        /// Tuner cycles: one per window start.
        tunes: u32,
        rx_log: String,
        last_seq: u64,
    }

    const KEY: &[u8] = b"node unit test key 0123456789";

    /// How [`run_node_with`] runs the node.
    #[derive(Default)]
    struct Opts {
        /// The scheduled window ends once this much of the audio has been
        /// delivered; `None` listens all the time.
        window_ends_ms: Option<u64>,
        /// The next window starts once this much of the audio has been delivered.
        next_window_ms: Option<u64>,
        /// Replaces the session's `AGN` and repeated-commit window.
        again_window: Option<Duration>,
        /// The tuner cannot match the antenna: the station is locked out from the
        /// window's start and keys nothing.
        tuner_bypassed: bool,
    }

    /// Unix time at the top of an hour: inside the default window (minutes 0-9).
    const WINDOW_OPEN: u64 = 1_699_999_200;

    /// Run the node on `audio` (8 kHz) with a simulated radio (time scale 100, or
    /// `HFNODE_E2E_SCALE` as for the mock-radio tests), delivering 50 ms blocks as
    /// fast as the node takes them.
    fn run_node(audio: Vec<f32>) -> Heard {
        run_node_with(audio, Opts::default())
    }

    fn run_node_with(audio: Vec<f32>, opts: Opts) -> Heard {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.key");
        std::fs::write(&key, KEY).unwrap();
        // The paths are set after parsing, so that nothing in them needs escaping.
        let mut cfg: Config = toml::from_str(&format!(
            r#"
            state_dir = ""
            [station]
            node_call = "N0DE"
            field_calls = ["W5XXX"]
            frequency_hz = 7030000
            serial_port = "/dev/null"
            chunk_pause_ms = 10
            [audio]
            end_of_message_ms = 2500
            [auth]
            key_file = ""
            [schedule]
            {schedule}
            [[contacts]]
            name = "MOM"
            address = "mom@example.com"
            "#,
            schedule = match opts.window_ends_ms {
                Some(_) => {
                    "always = false\n            every_minutes = 60\n            window_minutes = 10"
                }
                None => "always = true",
            },
        ))
        .unwrap();
        cfg.state_dir = dir.path().join("state");
        cfg.auth.key_file = key.clone();
        cfg.validate().unwrap();
        assert_eq!(cfg.audio.sample_rate, 8000);

        let mut rig = SimRig::new();
        rig.time_scale = std::env::var("HFNODE_E2E_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100.0);
        rig.tuner_bypassed = opts.tuner_bypassed;
        let mut sc = StationConfig::from_config(&cfg.station);
        sc.poll = Duration::from_millis(2);
        // The start-up `DE N0DE` is on the air for about 35 ms at 100x. The SWR
        // check samples it the moment it is keyed, with no sleep before that a
        // slow machine (GitHub's macOS runner) can wake from too late to see any
        // output, which would lock the node out. The station's own tests cover
        // switch-on delays.
        rig.tx_on_delay = Duration::ZERO;
        sc.swr_delay = Duration::ZERO;
        let mut station = Station::new(rig, sc, None);
        station.configure().unwrap();

        let (tx, rx) = audio::queue(usize::MAX);
        let blocks: Vec<Vec<f32>> = audio.chunks(400).map(<[f32]>::to_vec).collect();
        let radio = station.rig();
        // Where the schedule is, by the audio delivered: 0 in the first window, 1
        // after it, 2 in the next one.
        let phase = Arc::new(AtomicU8::new(0));
        let set_phase = phase.clone();
        let ends_block = opts.window_ends_ms.map(|ms| ms / 50);
        let next_block = opts.next_window_ms.map(|ms| ms / 50);
        thread::spawn(move || {
            for (i, samples) in blocks.into_iter().enumerate() {
                let i = i as u64;
                let p = match (ends_block, next_block) {
                    (_, Some(b)) if i >= b => 2,
                    (Some(b), _) if i >= b => 1,
                    _ => 0,
                };
                set_phase.store(p, Ordering::SeqCst);
                // Paced by the node, not the wall clock: the next block goes out
                // only once the node has taken the last, and the operator's audio
                // stands still while the radio tunes or transmits, as the silence
                // after a read-back would on the air. A busy test machine then
                // changes how long the test takes, not which audio the node hears.
                while tx.queued() > 0 || {
                    let mut r = radio.lock().unwrap();
                    r.is_transmitting().unwrap_or(true) || r.tuner_busy().unwrap_or(true)
                } {
                    thread::sleep(Duration::from_micros(200));
                }
                let b = Block {
                    at: Instant::now(),
                    samples,
                };
                if tx.send(b).is_err() {
                    break;
                }
            }
        });
        let mut sc = session_config(&cfg);
        if let Some(w) = opts.again_window {
            sc.again_window = w;
        }
        let mut session = build_session_with(&cfg, sc).unwrap();
        let mut svc = Fake::default();
        let clock = move || match phase.load(Ordering::SeqCst) {
            0 => WINDOW_OPEN,
            1 => WINDOW_OPEN + 30 * 60,
            _ => WINDOW_OPEN + 60 * 60,
        };
        let end =
            run_with_clock(&cfg, &mut station, &rx, &mut session, &mut svc, &clock).unwrap_err();
        assert!(end.to_string().contains("audio source ended"));
        let (keyed, tunes) = {
            let rig = station.rig();
            let r = rig.lock().unwrap();
            (r.sent.join(" "), r.tunes)
        };
        Heard {
            sent: svc.sent,
            keyed,
            tunes,
            rx_log: std::fs::read_to_string(cfg.state_dir.join("rx.log")).unwrap_or_default(),
            last_seq: session.last_seq(),
        }
    }

    fn code(seq: u64) -> String {
        CodeBook::new(KEY).code(seq)
    }

    fn ms(n: u64) -> Vec<f32> {
        vec![0.0; 8 * n as usize]
    }

    fn noisy(audio: Vec<f32>, k: &Keyer, snr_db: f32) -> Vec<f32> {
        noisy_with(audio, k, snr_db, 5)
    }

    fn noisy_with(mut audio: Vec<f32>, k: &Keyer, snr_db: f32, seed: u64) -> Vec<f32> {
        Noise::new(seed).add(
            &mut audio,
            Noise::sigma_for_snr(k.amplitude, snr_db, 8000, 2500.0),
        );
        audio
    }

    fn read_back_ms() -> u64 {
        cw::duration_ms("R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K", 18)
    }

    #[test]
    fn noise_after_the_over_is_not_part_of_the_message() {
        let k = Keyer::new(8000, 610.0, 18.0);
        // A noise burst decodes as a lone E after each K.
        let burst = k.render("E", 0.0);
        let mut audio = k.render(
            &format!("W5XXX 42 {} TX MOM RUNNING LATE HOME SUN K", code(42)),
            3000.0,
        );
        audio.extend(ms(1500));
        audio.extend(&burst);
        audio.extend(ms(read_back_ms() + 6000));
        audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
        audio.extend(ms(1500));
        audio.extend(&burst);
        audio.extend(ms(8000));
        let h = run_node(noisy(audio, &k, 15.0));
        assert_eq!(h.sent, [("MOM".into(), "RUNNING LATE HOME SUN".into())]);
        assert_eq!(h.last_seq, 43);
        assert!(!h.rx_log.contains("K E"), "{}", h.rx_log);
    }

    #[test]
    fn kn_ends_a_transmission_and_a_final_word_k_is_kept() {
        let k = Keyer::new(8000, 610.0, 18.0);
        let burst = k.render("E", 0.0);
        // The message ends in the word K, then the over K; the commit ends in KN
        // keyed run together. A noise burst follows each.
        let mut audio = k.render(
            &format!("W5XXX 42 {} TX MOM BRING VITAMIN K K", code(42)),
            3000.0,
        );
        audio.extend(ms(1500));
        audio.extend(&burst);
        audio.extend(ms(cw::duration_ms(
            "R 42 TX MOM BRING VITAMIN K ? DE N0DE K",
            18,
        ) + 6000));
        audio.extend(k.render(&format!("OK 43 {} (", code(43)), 0.0));
        audio.extend(ms(1500));
        audio.extend(&burst);
        audio.extend(ms(8000));
        let h = run_node(noisy(audio, &k, 15.0));
        assert_eq!(
            h.sent,
            [("MOM".into(), "BRING VITAMIN K".into())],
            "{}",
            h.rx_log
        );
        // After the window's ID.
        assert_eq!(
            h.keyed,
            "DE N0DE R 42 TX MOM BRING VITAMIN K ? DE N0DE K SENT 43 DE N0DE K"
        );
    }

    #[test]
    fn short_words_after_an_over_word_are_still_the_message() {
        // AR (a state), SK and K are over words, and I, AM, IN, A... could be noise
        // bursts; sent in rhythm, they are the operator still sending.
        for (wpm, text) in [
            (10.0, "HOME FROM AR I AM IN A CAB"),
            (18.0, "BACK IN SK I AM IN A MINE TOWN"),
            (8.0, "OK K I AM AT TENT"),
        ] {
            let k = Keyer::new(8000, 610.0, wpm);
            let mut audio = k.render(&format!("W5XXX 42 {} TX MOM {text} K", code(42)), 3000.0);
            let read_back = format!("R 42 TX MOM {text} ? DE N0DE K");
            // A long read-back keyed in two pieces: leave room for the keying
            // overhead when the test machine is busy.
            audio.extend(ms(cw::duration_ms(&read_back, 18) + 12000));
            audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
            audio.extend(ms(8000));
            // Clean audio: how much of the lead a busy test machine skips while
            // tuning must not decide which noise the fresh decoder starts on.
            let h = run_node(audio);
            assert_eq!(
                h.sent,
                [("MOM".into(), text.into())],
                "{text}: {}",
                h.rx_log
            );
            assert!(
                h.keyed.starts_with(&format!("DE N0DE {read_back}")),
                "{}",
                h.keyed
            );
        }
    }

    #[test]
    fn slow_operator_is_decoded_after_a_reply() {
        let k = Keyer::new(8000, 610.0, 6.0);
        let mut audio = k.render(&format!("W5XXX 42 {} TX MOM HI K", code(42)), 3000.0);
        audio.extend(ms(cw::duration_ms("R 42 TX MOM HI ? DE N0DE K", 18) + 6000));
        audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
        audio.extend(ms(8000));
        let h = run_node(noisy(audio, &k, 15.0));
        assert_eq!(h.sent, [("MOM".into(), "HI".into())], "{}", h.rx_log);
        // The start-up tune is identified, then the read-back and the result.
        assert_eq!(
            h.keyed,
            "DE N0DE R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K"
        );
    }

    /// An open in the window, then, once it has ended, the OK and a repeat of it,
    /// each after the node's reply. Returns the audio and when the window ends.
    fn late_commit_and_repeat() -> (Vec<f32>, u64) {
        let k = Keyer::new(8000, 610.0, 18.0);
        let mut audio = k.render(&format!("W5XXX 42 {} TX MOM HI K", code(42)), 3000.0);
        audio.extend(ms(cw::duration_ms("R 42 TX MOM HI ? DE N0DE K", 18) + 6000));
        let window_ends = audio.len() as u64 / 8;
        for _ in 0..2 {
            audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
            audio.extend(ms(cw::duration_ms("SENT 43 DE N0DE K", 18) + 6000));
        }
        audio.extend(ms(2000));
        (noisy(audio, &k, 15.0), window_ends)
    }

    fn oks_heard(h: &Heard) -> usize {
        h.rx_log.lines().filter(|l| l.contains("OK 43")).count()
    }

    #[test]
    fn a_result_can_be_repeated_after_the_window_ends() {
        let (audio, ends) = late_commit_and_repeat();
        let h = run_node_with(
            audio,
            Opts {
                window_ends_ms: Some(ends),
                ..Opts::default()
            },
        );
        assert_eq!(h.sent, [("MOM".into(), "HI".into())], "{}", h.rx_log);
        assert_eq!(
            h.keyed,
            "DE N0DE R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K SENT 43 DE N0DE K"
        );
        assert_eq!(oks_heard(&h), 2, "{}", h.rx_log);
        assert_eq!(h.tunes, 1);
    }

    #[test]
    fn a_window_held_open_into_the_next_still_starts_with_a_tune() {
        let (audio, ends) = late_commit_and_repeat();
        // The next window starts while the result can still be repeated, just
        // after the SENT.
        let next = ends + cw::duration_ms("OK 43 AAAAAAAA K", 18) + 4000;
        let h = run_node_with(
            audio,
            Opts {
                window_ends_ms: Some(ends),
                next_window_ms: Some(next),
                ..Opts::default()
            },
        );
        assert_eq!(h.tunes, 2, "{}", h.rx_log);
        // Each window's tune is identified.
        assert_eq!(
            h.keyed,
            "DE N0DE R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K DE N0DE SENT 43 DE N0DE K"
        );
        assert_eq!(h.sent, [("MOM".into(), "HI".into())]);
    }

    #[test]
    fn listening_ends_once_the_result_cannot_be_repeated() {
        let (audio, ends) = late_commit_and_repeat();
        let h = run_node_with(
            audio,
            Opts {
                window_ends_ms: Some(ends),
                again_window: Some(Duration::ZERO),
                ..Opts::default()
            },
        );
        assert_eq!(h.sent, [("MOM".into(), "HI".into())], "{}", h.rx_log);
        assert_eq!(
            h.keyed,
            "DE N0DE R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K"
        );
        assert_eq!(
            oks_heard(&h),
            1,
            "the repeat is not even decoded: {}",
            h.rx_log
        );
    }

    #[test]
    fn a_locked_out_station_does_not_listen_on_for_its_result() {
        let (audio, ends) = late_commit_and_repeat();
        let h = run_node_with(
            audio,
            Opts {
                window_ends_ms: Some(ends),
                tuner_bypassed: true,
                ..Opts::default()
            },
        );
        // Unchanged: the pending transaction holds the window, and the commit is
        // acted on although nothing can be keyed (not even the window's ID).
        assert_eq!(h.sent, [("MOM".into(), "HI".into())], "{}", h.rx_log);
        assert_eq!(h.keyed, "");
        assert_eq!(oks_heard(&h), 1, "{}", h.rx_log);
    }

    #[test]
    fn a_call_after_minutes_of_band_noise_is_heard() {
        // Noise that, heard for five minutes, would teach the decoder a speed no
        // one is sending at, so the call's first words split wrongly.
        let mut k = Keyer::new(8000, 610.0, 18.0);
        k.jitter = 0.03;
        for seed in [2u64, 4, 5] {
            let mut audio = ms(302_000);
            audio.extend(k.render(&format!("W5XXX 42 {} TX MOM HI K", code(42)), 0.0));
            audio.extend(ms(cw::duration_ms("R 42 TX MOM HI ? DE N0DE K", 18) + 6000));
            audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
            audio.extend(ms(8000));
            let h = run_node(noisy_with(audio, &k, 15.0, seed * 7919));
            assert_eq!(
                h.sent,
                [("MOM".into(), "HI".into())],
                "{seed}: {}",
                h.rx_log
            );
        }
    }

    #[test]
    fn over_end_finds_the_over_and_ignores_noise_after_it() {
        fn ev(text: &str) -> Vec<DecodeEvent> {
            let mut v = Vec::new();
            for c in text.chars() {
                v.push(match c {
                    ' ' => DecodeEvent::WordGap,
                    '*' => DecodeEvent::Unknown("........".into()),
                    c => DecodeEvent::Char(c),
                });
            }
            v
        }
        assert_eq!(over_end(&ev("OK 43 ABCDEFGH K ")), Some(17));
        assert_eq!(over_end(&ev("OK 43 ABCDEFGH K E I ")), Some(17));
        assert_eq!(over_end(&ev("OK 43 ABCDEFGH K E")), Some(17));
        assert_eq!(over_end(&ev("HOME SUN AR ")), Some(12));
        // No gap after the K yet: it may still become another word.
        assert_eq!(over_end(&ev("OK 43 ABCDEFGH K")), None);
        // KN keyed run together decodes as "(".
        assert_eq!(over_end(&ev("OK 43 ABCDEFGH ( E ")), Some(17));
        // Real words after a K: it was not the over (AGN <line> <code> K K asks
        // for chunk K).
        assert_eq!(over_end(&ev("AGN K ")), Some(6));
        assert_eq!(over_end(&ev("AGN K K ")), Some(8));
        assert_eq!(over_end(&ev("AGN 44 ABCDEFGH K K E ")), Some(20));
        assert_eq!(over_end(&ev("TX MOM K SEE ")), None);
        assert_eq!(over_end(&ev("TX MOM K * ")), None);
        assert_eq!(over_end(&ev("")), None);
    }

    #[test]
    fn a_clock_set_back_makes_checks_due() {
        assert_eq!(secs_since(1000, 1600), 600);
        assert_eq!(secs_since(1000, 1000), 0);
        assert_eq!(secs_since(1000, 400), u64::MAX);
    }

    #[test]
    fn tx_guard_skips_audio_from_transmit_and_recovery() {
        let t0 = Instant::now();
        let at = |ms| Block {
            at: t0 + Duration::from_millis(ms),
            samples: vec![0.0; 400],
        };
        let mut g = TxGuard::default();
        assert!(g.keep(&at(0)));
        g.ended(t0 + Duration::from_millis(30_000), 1000);
        // Read late, but captured while transmitting.
        assert!(!g.keep(&at(10_000)));
        assert!(!g.keep(&at(29_999)));
        // The receiver recovering: 1000 samples, three blocks.
        assert!(!g.keep(&at(30_000)));
        assert!(!g.keep(&at(30_050)));
        assert!(!g.keep(&at(30_100)));
        assert!(g.keep(&at(30_150)));
        assert!(g.keep(&at(30_200)));
    }

    #[test]
    fn a_reception_keeps_the_window_open() {
        let mut d = Decoder::new(DecoderConfig::new(8000, 610.0));
        let k = Keyer::new(8000, 610.0, 18.0);
        // Mid-character: nothing returned yet.
        let mut a = ms(500);
        a.extend(k.render("T", 0.0));
        a.truncate(a.len() - 8 * 20);
        assert!(d.push(&a).is_empty());
        assert!(d.has_partial());
        assert!(listening(false, false, true, &d, &[]));
        // Decoded words waiting for the end of the message.
        let idle = Decoder::new(DecoderConfig::new(8000, 610.0));
        assert!(listening(
            false,
            false,
            true,
            &idle,
            &[DecodeEvent::Char('W')]
        ));
        // Nothing in progress: the window closes.
        assert!(!listening(false, false, true, &idle, &[]));
        // A reception never opens a window by itself.
        assert!(!listening(
            false,
            false,
            false,
            &d,
            &[DecodeEvent::Char('W')]
        ));
        assert!(listening(true, false, false, &idle, &[]));
        // Past the window's end the session holds it open, but never opens one.
        assert!(listening(false, true, true, &idle, &[]));
        assert!(!listening(false, true, false, &idle, &[]));
    }

    #[test]
    fn a_message_the_filter_keeps_timing_out_on_is_withheld() {
        // One server that accepts and never answers, one that is not there.
        let slow = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_url = format!("http://{}", slow.local_addr().unwrap());
        thread::spawn(move || {
            let held: Vec<_> = slow.incoming().collect();
            drop(held);
        });
        let gone_url = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        // Refusal can take seconds on some systems, so only the slow server gets the
        // short timeout.
        for (url, timeout, withheld) in [(slow_url, 1, true), (gone_url, 60, false)] {
            let dir = tempfile::tempdir().unwrap();
            let mut cfg: Config =
                toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
            cfg.filter.provider = crate::config::Provider::Ollama;
            cfg.filter.model = Some("m".into());
            cfg.filter.base_url = Some(url);
            cfg.filter.timeout_secs = Some(timeout);
            let inbox = Arc::new(Mutex::new(
                Inbox::open(dir.path().join("inbox.json")).unwrap(),
            ));
            inbox
                .lock()
                .unwrap()
                .add("MOM", "a", "SEE YOU SUN", 0)
                .unwrap();
            let f = filter::Screener::new(&cfg.filter).unwrap();
            let mut timeouts = HashMap::new();
            for _ in 1..MAX_FILTER_TIMEOUTS {
                screen_inbox(&cfg, &inbox, Some(&f), &mut timeouts);
                assert_eq!(inbox.lock().unwrap().unscreened().len(), 1);
            }
            screen_inbox(&cfg, &inbox, Some(&f), &mut timeouts);
            let ready = inbox.lock().unwrap().ready();
            if withheld {
                assert_eq!(ready.len(), 1);
                assert_eq!(ready[0].screened.as_deref(), Some(filter::WITHHELD));
            } else {
                // Unreachable is not slow: held until the service is back.
                assert!(ready.is_empty());
                assert_eq!(inbox.lock().unwrap().unscreened().len(), 1);
            }
        }
    }
}
