//! The running node: audio in, decode, session, transmit, on a listening schedule.
//!
//! By default the node listens all the time ([`crate::config::Schedule`]). It tunes
//! when it starts listening, at start-up or at the top of a window, and again
//! before a reply once that tune is older than `retune_minutes`, so a node that
//! listens all day puts out no carriers of its own until there is something to
//! send. While it hears nothing it sets the radio up again and checks it every
//! `check_minutes`, without transmitting, and the station does the same before
//! every transmission. The decoder also goes back to its starting speed after a
//! quiet minute, so hours of band noise do not garble the next caller's first
//! words.

use crate::audio::{Block, BlockReceiver};
use crate::config::Config;
use crate::gateway::imessage::{self, ImShared};
use crate::gateway::{self, filter, google_voice, route, LiveServices};
use crate::inbox::Inbox;
use crate::places::LastPlaces;
use crate::session::{Outcome, Services, Session, SessionConfig};
use crate::station::Station;
use anyhow::{Context, Result};
use auth::{CodeBook, SeqStore, Verifier};
use civ::Rig;
use cw::{events_to_text, DecodeEvent, Decoder, DecoderConfig};
use protocol::{sanitize, Vocabulary};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
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

/// Read email, texts and iMessages, and screen new messages, until the process
/// exits. Each inbound route polls in a thread of its own, so one that hangs (a
/// stalled mail server) holds up neither the others nor the screening.
pub fn spawn_inbound(cfg: Config, inbox: Arc<Mutex<Inbox>>, im: Option<Arc<ImShared>>) {
    if cfg.email.is_none() && cfg.imessage.is_none() {
        return;
    }
    for w in cfg.warnings() {
        log::warn!("{w}");
    }
    let notify = start_screening(cfg.clone(), inbox.clone());
    if let Some(email) = cfg.email.clone() {
        let cfg = cfg.clone();
        let inbox = inbox.clone();
        // Mail ignored under an earlier configuration is considered again once, and
        // the first look for Google Voice reply addresses reads back through the
        // mailbox.
        let mut first = true;
        spawn_route(
            "imap",
            Duration::from_secs(email.poll_secs.max(30)),
            move || {
                let gv = cfg.google_voice.as_ref().map(|g| &g.number);
                if let Some(n) = gv {
                    if let Err(e) =
                        google_voice::learn_gv(&email, n, &cfg.contacts, &cfg.state_dir, first)
                    {
                        log::warn!("Google Voice address check failed: {e:#}");
                    }
                }
                let added = gateway::email::poll_imap(&email, &cfg.contacts, &inbox, first, gv)?;
                first = false;
                Ok(added)
            },
            notify.clone(),
        );
    }
    match (cfg.imessage.clone(), im) {
        (Some(imcfg), Some(shared)) => {
            let cfg = cfg.clone();
            let runner = imessage::OsaRunner::osascript();
            let every = Duration::from_secs(imcfg.poll_secs);
            thread::Builder::new()
                .name("imessage".into())
                .spawn(move || {
                    let mut warned = Warned::default();
                    report_probe(&imessage::probe(&imcfg, &runner, &shared));
                    log_routes(&cfg, Some(&shared));
                    let mut probed = Instant::now();
                    loop {
                        if shared.db_ok() {
                            match imessage::poll(
                                &imcfg,
                                &cfg.contacts,
                                &cfg.state_dir,
                                &inbox,
                                gateway::unix_now(),
                            ) {
                                Ok(n) if n > 0 => {
                                    log::info!("{n} new iMessage(s)");
                                    let _ = notify.send(());
                                }
                                Ok(_) => {}
                                Err(e) => warned.warn(format!("iMessage check failed: {e:#}")),
                            }
                        }
                        let ready = shared.db_ok() && shared.send_ready().is_ok();
                        if !ready && probed.elapsed() >= REPROBE {
                            report_probe(&imessage::probe(&imcfg, &runner, &shared));
                            probed = Instant::now();
                        }
                        thread::sleep(every);
                    }
                })
                .expect("starting the iMessage thread");
        }
        _ => log_routes(&cfg, None),
    }
}

/// How often iMessage is checked again while it cannot be used.
const REPROBE: Duration = Duration::from_secs(600);

fn report_probe(r: &imessage::Readiness) {
    match r {
        imessage::Readiness::Ready => log::info!("iMessage ready"),
        imessage::Readiness::NotReady(why) => log::error!("iMessage not available: {why}"),
        imessage::Readiness::Unknown => {}
    }
}

/// How TX would reach each contact, in the log at start-up.
fn log_routes(cfg: &Config, im: Option<&ImShared>) {
    let gv = google_voice::GvStore::read(&cfg.state_dir);
    let state = gateway::imessage_state(cfg, im);
    let a = gateway::avail(cfg, cfg.email.is_some(), &state);
    for c in &cfg.contacts {
        let line = route::describe(c, &a, &gv);
        if line.starts_with("NO ROUTE") {
            log::warn!("route: {} {line}", c.name);
        } else {
            log::info!("route: {} {line}", c.name);
        }
    }
}

/// The same failure is logged at most once an hour.
#[derive(Default)]
struct Warned(HashMap<String, Instant>);

impl Warned {
    fn warn(&mut self, msg: String) {
        let due = self
            .0
            .get(&msg)
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(3600));
        if due {
            log::warn!("{msg}");
            self.0.insert(msg, Instant::now());
        }
    }
}

/// Run `poll` every `every` in a thread of its own, waking the screening when it
/// adds messages.
fn spawn_route(
    name: &str,
    every: Duration,
    mut poll: impl FnMut() -> Result<usize> + Send + 'static,
    notify: Sender<()>,
) {
    let label = name.to_string();
    thread::Builder::new()
        .name(label.clone())
        .spawn(move || loop {
            match poll() {
                Ok(n) if n > 0 => {
                    log::info!("{n} new inbound message(s)");
                    let _ = notify.send(());
                }
                Ok(_) => {}
                Err(e) => log::warn!("{label} check failed: {e:#}"),
            }
            thread::sleep(every);
        })
        .expect("starting an inbound thread");
}

/// The screening thread, the only caller of [`screen_inbox`]: it screens whenever a
/// route adds messages, and every minute, to retry messages held while the filter
/// was down.
fn start_screening(cfg: Config, inbox: Arc<Mutex<Inbox>>) -> Sender<()> {
    let (notify, wake) = mpsc::channel::<()>();
    thread::Builder::new()
        .name("screen".into())
        .spawn(move || {
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
                log::warn!(
                    "inbound filter disabled: third-party text will be transmitted unscreened"
                );
                None
            };
            let mut timeouts = HashMap::new();
            loop {
                screen_inbox(&cfg, &inbox, filter.as_ref(), &mut timeouts);
                match wake.recv_timeout(Duration::from_secs(60)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    // Every route stopped: still retry held messages.
                    Err(RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_secs(60)),
                }
            }
        })
        .expect("starting the screening thread");
    notify
}

pub fn live_services(
    cfg: &Config,
    inbox: Arc<Mutex<Inbox>>,
    im: Option<Arc<ImShared>>,
) -> Result<LiveServices> {
    let field_call = cfg.station.field_calls.first().cloned().unwrap_or_default();
    let mailer = match &cfg.email {
        Some(e) => Some(gateway::email::Mailer::new(e, &field_call)?),
        None => None,
    };
    let weather = cfg.weather.as_ref().map(gateway::weather::Nws::new);
    let imessage = match (&cfg.imessage, im) {
        (Some(c), Some(shared)) => Some(imessage::ImSender::new(c, &cfg.state_dir, shared)),
        _ => None,
    };
    Ok(LiveServices {
        cfg: cfg.clone(),
        inbox,
        mailer,
        weather,
        imessage,
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

/// Over prosigns that end a field transmission, as decoded (`+` is AR); the same
/// set that `protocol::parse` strips.
const OVERS: [&str; 5] = ["K", "KN", "+", "AR", "SK"];

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

/// Whether to listen: in a scheduled window, while a transaction is pending, or
/// while a transmission heard in the window is still being received.
fn listening(
    scheduled: bool,
    pending: bool,
    was_open: bool,
    decoder: &Decoder,
    events: &[DecodeEvent],
) -> bool {
    scheduled || pending || (was_open && (decoder.has_partial() || !events.is_empty()))
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

/// A reply that took longer than this to prepare (a send that hung) is not keyed:
/// the field operator has given up waiting and may be transmitting again. Their
/// repeated `OK`, or `AGN`, keys it.
const LATE_REPLY: Duration = Duration::from_secs(90);

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
    run_with(cfg, station, audio, session, svc, clock, LATE_REPLY)
}

fn run_with<R: Rig + 'static>(
    cfg: &Config,
    station: &mut Station<R>,
    audio: &BlockReceiver,
    session: &mut Session,
    svc: &mut dyn Services,
    clock: &dyn Fn() -> u64,
    late_reply: Duration,
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
        let open = listening(
            cfg.schedule.is_open(clock()),
            session.has_pending(now),
            was_open,
            &decoder,
            &events,
        );
        if open && !was_open {
            log::info!(
                "{}",
                if cfg.schedule.always {
                    "listening"
                } else {
                    "listening window open"
                }
            );
            if let Err(e) = station.start_window() {
                log::error!("tune failed at window start: {e}");
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
            let started = Instant::now();
            match session.handle(&text, started, svc) {
                Outcome::Silent(why) => log::info!("no reply: {why}"),
                Outcome::Transmit(t) if started.elapsed() > late_reply => log::warn!(
                    "reply not keyed: preparing it took {} s, longer than the field operator \
                     waits; a repeated OK or AGN keys it: {}",
                    started.elapsed().as_secs(),
                    t.text()
                ),
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

    #[derive(Default)]
    struct Fake {
        sent: Vec<(String, String)>,
        /// How long a send takes.
        delay: Duration,
    }

    impl Services for Fake {
        fn send_message(
            &mut self,
            dest: &str,
            _from_call: &str,
            text: &str,
        ) -> Result<(), crate::session::SendError> {
            thread::sleep(self.delay);
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
        rx_log: String,
        last_seq: u64,
    }

    const KEY: &[u8] = b"node unit test key 0123456789";

    /// Run the node on `audio` (8 kHz) with a simulated radio (time scale 100),
    /// delivering 50 ms blocks as fast as the node takes them.
    fn run_node(audio: Vec<f32>) -> Heard {
        run_node_with(audio, Fake::default(), LATE_REPLY)
    }

    fn run_node_with(audio: Vec<f32>, mut svc: Fake, late_reply: Duration) -> Heard {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.key");
        std::fs::write(&key, KEY).unwrap();
        // The paths are set after parsing, so that nothing in them needs escaping.
        let mut cfg: Config = toml::from_str(
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
            always = true
            [[contacts]]
            name = "MOM"
            address = "mom@example.com"
            "#,
        )
        .unwrap();
        cfg.state_dir = dir.path().join("state");
        cfg.auth.key_file = key.clone();
        cfg.validate().unwrap();
        assert_eq!(cfg.audio.sample_rate, 8000);

        let mut rig = SimRig::new();
        rig.time_scale = 100.0;
        let mut sc = StationConfig::from_config(&cfg.station);
        sc.poll = Duration::from_millis(2);
        sc.swr_delay = Duration::from_millis(2);
        let mut station = Station::new(rig, sc, None);
        station.configure().unwrap();

        let (tx, rx) = audio::queue(usize::MAX);
        let blocks: Vec<Vec<f32>> = audio.chunks(400).map(<[f32]>::to_vec).collect();
        let radio = station.rig();
        thread::spawn(move || {
            for samples in blocks {
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
        let mut session = build_session(&cfg).unwrap();
        let end = run_with(
            &cfg,
            &mut station,
            &rx,
            &mut session,
            &mut svc,
            &gateway::unix_now,
            late_reply,
        )
        .unwrap_err();
        assert!(end.to_string().contains("audio source ended"));
        let keyed = station.rig().lock().unwrap().sent.join(" ");
        Heard {
            sent: svc.sent,
            keyed,
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
    fn a_reply_that_took_too_long_is_keyed_only_when_asked_again() {
        let k = Keyer::new(8000, 610.0, 18.0);
        let mut audio = k.render(&format!("W5XXX 42 {} TX MOM HI K", code(42)), 3000.0);
        audio.extend(ms(cw::duration_ms("R 42 TX MOM HI ? DE N0DE K", 18) + 6000));
        audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
        audio.extend(ms(6000));
        // Heard nothing back: the operator sends OK again.
        audio.extend(k.render(&format!("OK 43 {} K", code(43)), 0.0));
        audio.extend(ms(8000));
        let slow = Fake {
            delay: Duration::from_millis(400),
            ..Fake::default()
        };
        let h = run_node_with(audio, slow, Duration::from_millis(100));
        assert_eq!(h.sent, [("MOM".into(), "HI".into())], "{}", h.rx_log);
        assert_eq!(h.keyed, "R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K");
    }

    #[test]
    fn screening_goes_on_while_a_route_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
        cfg.filter.enabled = false;
        let inbox = Arc::new(Mutex::new(
            Inbox::open(dir.path().join("inbox.json")).unwrap(),
        ));
        let notify = start_screening(cfg, inbox.clone());
        spawn_route(
            "hangs",
            Duration::from_secs(1),
            || loop {
                thread::sleep(Duration::from_secs(3600));
            },
            notify.clone(),
        );
        let ib = inbox.clone();
        let mut once = true;
        spawn_route(
            "adds",
            Duration::from_millis(50),
            move || {
                let added = once && ib.lock().unwrap().add("MOM", "a", "SEE YOU SUN", 0)?;
                once = false;
                Ok(usize::from(added))
            },
            notify,
        );
        let start = Instant::now();
        while inbox.lock().unwrap().ready().is_empty() {
            assert!(start.elapsed() < Duration::from_secs(2), "not screened");
            thread::sleep(Duration::from_millis(10));
        }
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
            assert!(h.keyed.starts_with(&read_back), "{}", h.keyed);
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
        assert_eq!(h.keyed, "R 42 TX MOM HI ? DE N0DE K SENT 43 DE N0DE K");
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
        // Real words after a K: it was not the over ("AGN K K" asks for chunk K).
        assert_eq!(over_end(&ev("AGN K ")), Some(6));
        assert_eq!(over_end(&ev("AGN K K ")), Some(8));
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
        assert!(listening(false, true, false, &idle, &[]));
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
