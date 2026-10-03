//! The running node: audio in, decode, session, transmit, on a listening schedule.

use crate::config::Config;
use crate::gateway::{self, filter, LiveServices};
use crate::inbox::Inbox;
use crate::session::{Outcome, Services, Session, SessionConfig};
use crate::station::Station;
use anyhow::{Context, Result};
use auth::{CodeBook, SeqStore, Verifier};
use civ::Rig;
use cw::{events_to_text, DecodeEvent, Decoder, DecoderConfig};
use protocol::{sanitize, Vocabulary};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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

pub fn build_session(cfg: &Config) -> Result<Session> {
    let book = load_codebook(cfg)?;
    let store = SeqStore::new(cfg.state_dir.join("last_seq"));
    let last = store.load().context("loading last_seq")?;
    log::info!("last_seq is {last}");
    Ok(Session::new(
        SessionConfig {
            node_call: cfg.station.node_call.to_ascii_uppercase(),
            pending_timeout: Duration::from_secs(cfg.pending_timeout_secs),
            chunk_chars: cfg.station.chunk_chars,
            max_rx_messages: 5,
            again_window: Duration::from_secs(cfg.pending_timeout_secs.max(600)),
        },
        Vocabulary {
            field_calls: cfg
                .station
                .field_calls
                .iter()
                .map(|c| c.to_ascii_uppercase())
                .collect(),
            contacts: cfg.contact_names(),
        },
        Verifier::new(book, last),
        store,
    ))
}

pub fn open_inbox(cfg: &Config) -> Result<Arc<Mutex<Inbox>>> {
    Ok(Arc::new(Mutex::new(Inbox::open(
        cfg.state_dir.join("inbox.json"),
    )?)))
}

/// Screen every unscreened message. Messages stay unscreened (and are never keyed)
/// if the filter cannot be reached.
pub fn screen_inbox(
    cfg: &Config,
    inbox: &Arc<Mutex<Inbox>>,
    filter: Option<&filter::ClaudeFilter>,
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
            match filter::ClaudeFilter::new(&cfg.filter) {
                Ok(f) => Some(f),
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
        loop {
            match gateway::email::poll_imap(&email, &cfg.contacts, &inbox) {
                Ok(n) if n > 0 => log::info!("{n} new inbound message(s)"),
                Ok(_) => {}
                Err(e) => log::warn!("IMAP poll failed: {e:#}"),
            }
            screen_inbox(&cfg, &inbox, filter.as_ref());
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

/// The main loop. Returns only on a fatal error or when the audio source ends.
pub fn run<R: Rig + 'static>(
    cfg: &Config,
    station: &mut Station<R>,
    audio: &Receiver<Vec<f32>>,
    session: &mut Session,
    svc: &mut dyn Services,
) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let rx_log = cfg.state_dir.join("rx.log");
    let mut decoder = decoder_for(cfg);
    let mut events: Vec<DecodeEvent> = Vec::new();
    let mut was_open = false;
    loop {
        let block = match audio.recv_timeout(Duration::from_secs(5)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => {
                log::warn!("no audio for 5 s");
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("audio source ended"),
        };
        let now = Instant::now();
        let open = cfg.schedule.is_open(gateway::unix_now()) || session.has_pending(now);
        if open && !was_open {
            log::info!("listening window open");
            if let Err(e) = station.start_window() {
                log::error!("tune failed at window start: {e}");
            }
            drain(audio);
            decoder = decoder_for(cfg);
            events.clear();
            was_open = true;
            continue;
        }
        if !open {
            if was_open {
                log::info!("listening window closed");
            }
            was_open = false;
            continue;
        }

        events.extend(decoder.push(&block));
        let heard = !events.is_empty() || decoder.key_down();
        if heard && !decoder.key_down() && decoder.idle_ms() >= cfg.audio.end_of_message_ms {
            events.extend(decoder.flush());
            let text = events_to_text(&events);
            events.clear();
            if text.is_empty() {
                continue;
            }
            log::info!("heard: {text}");
            let _ = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&rx_log)
                .and_then(|mut f| writeln!(f, "{},{text}", gateway::unix_now()));
            match session.handle(&text, Instant::now(), svc) {
                Outcome::Silent(why) => log::info!("no reply: {why}"),
                Outcome::Transmit(t) => {
                    log::info!("sending: {}", t.text());
                    if let Err(e) = station.transmit(&t) {
                        log::error!("transmit failed: {e}");
                    }
                    // Discard whatever was captured while transmitting.
                    drain(audio);
                    decoder = decoder_for(cfg);
                }
            }
        }
    }
}

fn drain(audio: &Receiver<Vec<f32>>) {
    while audio.try_recv().is_ok() {}
}
