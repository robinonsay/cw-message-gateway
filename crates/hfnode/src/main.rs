//! `hfnode`: the HF CW message gateway node and its tools.

use anyhow::{bail, Context, Result};
use auth::{format_for_print, CodeBook};
use clap::{Parser, Subcommand};
use hfnode::commissioning::{self, Action};
use hfnode::config::{Config, RigKind};
use hfnode::gateway::OfflineServices;
use hfnode::handheld::{self, Handheld};
use hfnode::inbox::Inbox;
use hfnode::keyer;
use hfnode::session::{Outcome, Services};
use hfnode::station::{InhibitLatch, Station, StationConfig};
use hfnode::storm::StormHold;
use hfnode::{alert, audio, gateway, node, selftest};
use protocol::sanitize;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(version, about = "HF CW message gateway node for the IC-7300")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new secret key (keep it only on the node).
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the code table to carry in the field.
    Codes {
        #[arg(long)]
        config: PathBuf,
        /// First sequence number (default: one after the last used).
        #[arg(long)]
        from: Option<u64>,
        #[arg(long, default_value_t = 100)]
        count: u64,
    },
    /// Type field transmissions and see the node's replies. No radio or audio.
    Sim {
        #[arg(long)]
        config: PathBuf,
        /// Don't send email or call web services.
        #[arg(long)]
        offline: bool,
    },
    /// Decode CW from a WAV file.
    Decode {
        file: PathBuf,
        #[arg(long, default_value_t = 600.0)]
        pitch: f32,
    },
    /// Write a WAV file of CW, for testing the decoder.
    Synth {
        text: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 18.0)]
        wpm: f32,
        #[arg(long, default_value_t = 600.0)]
        pitch: f32,
        /// Signal-to-noise ratio in 2500 Hz; omit for a clean signal.
        #[arg(long)]
        snr: Option<f32>,
        /// Hand-keying timing jitter (0-0.2).
        #[arg(long, default_value_t = 0.0)]
        jitter: f32,
    },
    /// Decode live audio from the radio and print it. Never transmits.
    Listen {
        #[arg(long)]
        config: PathBuf,
    },
    /// Record the radio's audio to a WAV file, as the node hears it. Never transmits.
    Record {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 10)]
        seconds: u32,
    },
    /// List this computer's serial ports and audio inputs, to find the radio's. Opens
    /// nothing, so sends nothing to any radio. No config needed.
    Devices,
    /// Talk to the radio directly, for bench testing.
    Radio {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: RadioCmd,
    },
    /// Any radio on the keyer box (`station.rig = "keyer"`): check the box and the
    /// radio's audio, and the bring-up tests.
    Keyer {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: KeyerCmd,
    },
    /// Talk to a handheld running the CW firmware (`station.rig = "handheld"`), for
    /// bench testing and bring-up (docs/handheld.md).
    Handheld {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: HandheldCmd,
    },
    /// Run the node.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Ask the NWS once whether the storm stand-down would hold now, for the
    /// station's location in `[storm]`. Never touches the radio.
    Storm {
        #[arg(long)]
        config: PathBuf,
    },
    /// Texts, iMessage and email without the radio: see how TX would reach each
    /// contact and what the node would take, or send one message.
    Messages {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: MessagesCmd,
    },
    /// Try the inbound filter configured in `[filter]` (Claude or Ollama) without
    /// the radio. Nothing is transmitted or stored.
    Filter {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: FilterCmd,
    },
    /// Run the closed-loop scenarios against a mock IC-7300, and the `keyer-` ones
    /// against a mock keyer box and the radio it keys: no radio, sound card,
    /// network or config needed. Exits non-zero if any fails.
    Selftest {
        /// Run only these scenarios (exact name, or a prefix such as `fault-`).
        #[arg(long)]
        scenario: Vec<String>,
        /// Times faster than real time, 1 to 200 (the `keyer-` scenarios at most
        /// 5); lower it on a slow machine.
        #[arg(long, default_value_t = selftest::DEFAULT_SCALE)]
        scale: f32,
        /// Scenarios run at once (default: one per CPU).
        #[arg(long)]
        jobs: Option<usize>,
        /// List the scenarios and exit.
        #[arg(long)]
        list: bool,
        /// Print every check and the transcript of each scenario.
        #[arg(short, long)]
        verbose: bool,
        /// Sweep complete exchanges over speed x SNR x keying instead, several
        /// trials per cell, and print success matrices and where things break.
        /// Exits non-zero on a safety violation, a wrong message delivered, or a
        /// failure in the should-pass region.
        #[arg(long, conflicts_with_all = ["scenario", "list"])]
        sweep: bool,
        /// Sweep: field operator speeds, comma-separated.
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "5,8,10,13,15,18,20,25,30,35",
            requires = "sweep"
        )]
        wpm: Vec<f32>,
        /// Sweep: SNRs in 2500 Hz, comma-separated; `clean` for no noise.
        #[arg(
            long,
            value_delimiter = ',',
            allow_hyphen_values = true,
            default_value = "clean,20,10,6,3,0,-3,-6",
            requires = "sweep"
        )]
        snr: Vec<String>,
        /// Sweep: keying styles, comma-separated: `machine`, `hand`.
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "machine,hand",
            requires = "sweep"
        )]
        keying: Vec<String>,
        /// Sweep: trials per cell, each with its own noise and keying jitter.
        #[arg(long, default_value_t = selftest::SWEEP_TRIALS, requires = "sweep")]
        trials: u32,
        /// Sweep: follow each TX exchange with an RX exchange reading out a message.
        #[arg(long, requires = "sweep")]
        rx: bool,
        /// Sweep: write one row per run to this CSV file.
        #[arg(long, requires = "sweep")]
        csv: Option<PathBuf>,
    },
    /// Write the field side of a test session as WAV files, with a manifest of the
    /// expected decodes and replies. Codes come from a fixed test-only key.
    Testvectors {
        #[arg(long)]
        out: PathBuf,
        /// Speeds, comma-separated.
        #[arg(long, value_delimiter = ',', default_value = "12,18,25")]
        wpm: Vec<f32>,
        /// Signal-to-noise ratios in 2500 Hz, comma-separated; `clean` for none.
        #[arg(long, value_delimiter = ',', default_value = "clean,10")]
        snr: Vec<String>,
        /// Hand-keying timing jitter (0-0.2).
        #[arg(long, default_value_t = 0.03)]
        jitter: f32,
        #[arg(long, default_value_t = 600.0)]
        pitch: f32,
    },
}

#[derive(Subcommand)]
enum MessagesCmd {
    /// Show, without changing anything, how TX would reach each contact and what
    /// the node would take from each inbound route. Reads the node's mailbox
    /// read-only (no flags set) and, on a Mac, Messages' database. Writes nothing in
    /// state_dir; the running node learns Google Voice reply addresses itself.
    /// --save-raw also writes Google Voice mails to DIR.
    Check {
        /// How far back to show contacts' iMessages, in hours.
        #[arg(long, default_value_t = 24)]
        since: u64,
        /// Save Google Voice mails as .eml files in this folder (they hold phone
        /// numbers, reply tokens and private texts).
        #[arg(long, value_name = "DIR")]
        save_raw: Option<PathBuf>,
        /// Print the raw attributedBody of this Messages row, if it is a contact's.
        #[arg(long, value_name = "ROWID")]
        dump: Option<i64>,
    },
    /// Send one message now by the route TX would use: a real text or email, tagged
    /// like a TX; an iMessage opens a reply window like a TX does. Exits 0 when
    /// sent, 1 when the route failed, 2 when there is no route.
    Send {
        /// Use only this kind of route.
        #[arg(long, value_enum)]
        via: Option<ViaArg>,
        /// The field callsign it is sent as (default: the first in the config).
        #[arg(long)]
        call: Option<String>,
        /// Contact name, as in the config.
        name: String,
        text: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ViaArg {
    Imessage,
    GoogleVoice,
    Email,
}

#[derive(Subcommand)]
enum FilterCmd {
    /// Screen a set of built-in sample replies (ordinary messages, spam, profanity,
    /// code groups) and show each verdict. Exits non-zero unless all come out as
    /// expected. With Claude this makes one paid API call per sample.
    Test,
    /// Screen one message and show what would be keyed.
    Screen {
        text: String,
        /// Contact name the message is from.
        #[arg(long, default_value = "TEST")]
        from: String,
    },
}

#[derive(Subcommand)]
enum RadioCmd {
    /// Read the frequency (receive only, safe).
    Status,
    /// Read-only preflight: identify the radio and check every setting that could
    /// make it transmit unexpectedly. Never writes to the radio.
    Check,
    /// Put the radio in the node's operating state (frequency, CW, power, keyer).
    Setup,
    /// Run the antenna tuner (transmits briefly).
    Tune,
    /// Key a short CW message and report SWR (transmits).
    Cw { text: String },
    /// Force the radio back to receive.
    Rx,
}

#[derive(Subcommand)]
enum KeyerCmd {
    /// Greet the box (its limits, why it last started, its key) and check the
    /// radio's audio: band level, no key held at the radio. Keys nothing.
    Check,
    /// Stop the box and confirm the radio's key open, by the box and the audio.
    Rx,
    /// Key TEXT through the box with every check `run` makes but the storm
    /// stand-down, and report whether the radio was heard sending it.
    Key { text: String },
    /// Key `DE <call>` and measure the sidetone: its delay, level and pitch.
    Sidetone,
    /// Hang the box's control loop mid-run: its watchdog must reset it and open
    /// the key within 0.5 s. Then identifies.
    Hangtest,
    /// Identify, then make the box hold its key down: its 1 s limit must open the
    /// key and trip it (unplug it and plug it in again afterwards).
    Stucktest,
    /// Key a long message and then stop talking to the box: its link timeout must
    /// open the key by itself, as it would if the node died or the cable came out.
    Linktest,
}

#[derive(Subcommand)]
enum HandheldCmd {
    /// Ask the firmware who it is, stop anything it is sending, read its status, and
    /// check the radio's frequency, mode, power and break-in against the config.
    /// Changes nothing and never transmits.
    Check,
    /// Key a short CW message through the station's safety layer (transmits).
    Key { text: String },
    /// Bring-up: key a long message and then go silent, as if the node had died; the
    /// firmware must stop on its own within its link timeout. Transmits for up to a
    /// few seconds, then sends the station's call.
    Linktest,
    /// Bring-up: key a long message and then have the firmware hang; its watchdog
    /// must reset the radio, which ends the transmission. Transmits for about 3 s,
    /// then sends the station's call.
    Hangtest,
    /// Stop the firmware's keyer and confirm receive.
    Rx,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The self-test runs the node many times over; its log is noise unless asked for.
    let level = if matches!(cli.cmd, Cmd::Selftest { .. }) {
        "off"
    } else {
        "info"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    ctrlc::set_handler(on_stop_signal).context("installing the stop-signal handler")?;
    // For tests/stop_signal.rs, which waits for this before sending a signal.
    log::debug!(target: "hfnode::signal", "stop-signal handler installed");
    match cli.cmd {
        Cmd::Keygen { out } => keygen(&out),
        Cmd::Codes {
            config,
            from,
            count,
        } => codes(&Config::load(&config)?, from, count),
        Cmd::Sim { config, offline } => sim(&Config::load(&config)?, offline),
        Cmd::Decode { file, pitch } => decode(&file, pitch),
        Cmd::Synth {
            text,
            out,
            wpm,
            pitch,
            snr,
            jitter,
        } => synth(&text, &out, wpm, pitch, snr, jitter),
        Cmd::Listen { config } => listen(&Config::load(&config)?),
        Cmd::Record {
            config,
            out,
            seconds,
        } => record(&Config::load(&config)?, &out, seconds),
        Cmd::Devices => devices(),
        Cmd::Radio { config, action } => radio(&Config::load(&config)?, action),
        Cmd::Keyer { config, action } => keyer_cmd(&Config::load(&config)?, action),
        Cmd::Handheld { config, action } => handheld_cmd(&Config::load(&config)?, action),
        Cmd::Run { config } => run(&config, &Config::load(&config)?),
        Cmd::Storm { config } => storm_check(&Config::load(&config)?),
        Cmd::Messages { config, action } => messages_cmd(&Config::load(&config)?, action),
        Cmd::Filter { config, action } => filter_cmd(&Config::load(&config)?, action),
        Cmd::Selftest {
            sweep: true,
            scale,
            jobs,
            verbose,
            wpm,
            snr,
            keying,
            trials,
            rx,
            csv,
            ..
        } => {
            let spec = selftest::SweepSpec {
                wpms: wpm,
                snrs: parse_snrs(&snr)?,
                keyings: keying
                    .iter()
                    .map(|k| {
                        selftest::Keying::parse(k)
                            .with_context(|| format!("--keying {k:?}: `machine` or `hand`"))
                    })
                    .collect::<Result<_>>()?,
                trials,
                rx,
            };
            run_sweep(spec, scale, jobs, verbose, csv.as_deref())
        }
        Cmd::Selftest {
            scenario,
            scale,
            jobs,
            list,
            verbose,
            ..
        } => run_selftest(&scenario, scale, jobs, list, verbose),
        Cmd::Testvectors {
            out,
            wpm,
            snr,
            jitter,
            pitch,
        } => testvectors(&out, &wpm, &snr, jitter, pitch),
    }
}

type DynRig = dyn civ::Rig + 'static;
type Radio = Arc<Mutex<DynRig>>;

/// The radio, once a command has passed the preflight and may write to it, with the
/// inhibit a failed stop latches.
static RADIO: Mutex<Option<(Radio, InhibitLatch)>> = Mutex::new(None);

/// From here on a stop signal puts `radio` back on receive before the program exits,
/// and latches `inhibit` if it cannot.
fn guard_radio(radio: Radio, inhibit: InhibitLatch) {
    *RADIO.lock().unwrap_or_else(|e| e.into_inner()) = Some((radio, inhibit));
}

/// Ctrl-C, or a stop from systemd or launchd (SIGINT, SIGTERM, SIGHUP, or on Windows
/// a console Ctrl-C or Ctrl-Break): with a radio in use, put it back on receive
/// first (see [`stop_radio`]) and exit while still holding it, so that nothing else
/// can key it in between. Exits 0 once receive is confirmed (a clean stop, so
/// neither systemd nor the start-up scripts in `deploy/` restart the node), 1 if
/// receive is not confirmed, and 130 if no radio was in use (interrupted, as without
/// this handler).
fn on_stop_signal() {
    let guarded = RADIO.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some((radio, inhibit)) = guarded else {
        std::process::exit(130);
    };
    let (_held, code) = stop_radio(&radio, &inhibit);
    std::process::exit(code);
}

/// Take the radio, stop the keyer and confirm receive. The radio may first finish
/// the text already in its keyer (at most 30 characters). Returns the radio, still
/// held, and the exit code.
///
/// A stop that cannot confirm receive latches the transmit inhibit, so that the node
/// transmits nothing after a restart until someone has looked at the radio: the
/// process exiting here is the one thing that cannot be taken back (the safety
/// audit's KB-2(iii) for the keyer box, K5 for the IC-7300).
fn stop_radio<'a>(
    radio: &'a Mutex<DynRig>,
    inhibit: &InhibitLatch,
) -> (MutexGuard<'a, DynRig>, i32) {
    log::warn!("stop requested: stopping the keyer and forcing receive");
    let mut rig = radio.lock().unwrap_or_else(|e| e.into_inner());
    let code = match hfnode::station::force_receive_or_latch(&mut *rig, inhibit) {
        Ok(()) => {
            log::info!("radio confirmed on receive; exiting");
            0
        }
        Err(e) => {
            log::error!("radio NOT confirmed on receive ({e}); check it before restarting");
            1
        }
    };
    (rig, code)
}

fn keygen(out: &Path) -> Result<()> {
    if out.exists() {
        bail!(
            "{} already exists; refusing to overwrite a key",
            out.display()
        );
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(&key)?;
    println!("wrote 256-bit key to {}", out.display());
    Ok(())
}

fn codes(cfg: &Config, from: Option<u64>, count: u64) -> Result<()> {
    let book: CodeBook = node::load_codebook(cfg)?;
    let last = auth::SeqStore::new(cfg.state_dir.join("last_seq")).load()?;
    let from = from.unwrap_or(last + 1);
    println!(
        "{} code table, sequence {from}-{}",
        cfg.station.node_call,
        from + count - 1
    );
    for line in SHEET_RULES {
        println!("{line}");
    }
    println!();
    let rows: Vec<String> = book
        .table(from, count)
        .map(|(s, c)| format!("{s:>5}  {}", format_for_print(&c)))
        .collect();
    // Three columns, read down.
    let per_col = rows.len().div_ceil(3);
    for i in 0..per_col {
        let line: Vec<&str> = (0..3)
            .filter_map(|c| rows.get(c * per_col + i).map(String::as_str))
            .collect();
        println!("{}", line.join("      "));
    }
    let presets = preset_lines(cfg);
    if !presets.is_empty() {
        println!();
        println!("Weather presets: send WX <number>.");
        for line in presets {
            println!("{line}");
        }
        println!(
            "WX alone: the last place you confirmed, {} until then.",
            default_wx(cfg)
        );
    }
    if !cfg.contacts.is_empty() {
        println!();
        println!("Contacts (TX route when printed):");
        for line in hfnode::messages::code_table_routes(cfg) {
            println!("{line}");
        }
    }
    Ok(())
}

fn messages_cmd(cfg: &Config, action: MessagesCmd) -> Result<()> {
    let mut out = std::io::stdout();
    match action {
        MessagesCmd::Check {
            since,
            save_raw,
            dump,
        } => hfnode::messages::check(cfg, since, save_raw.as_deref(), dump, &mut out),
        MessagesCmd::Send {
            via,
            call,
            name,
            text,
        } => {
            let via = via.map(|v| match v {
                ViaArg::Imessage => gateway::RouteKind::IMessage,
                ViaArg::GoogleVoice => gateway::RouteKind::GoogleVoice,
                ViaArg::Email => gateway::RouteKind::Email,
            });
            let result = hfnode::messages::send(cfg, via, call.as_deref(), &name, &text, &mut out)?;
            out.flush()?;
            std::process::exit(result as i32);
        }
    }
}

/// The rules printed under the code table's title; docs/operating.md shows them too.
const SHEET_RULES: [&str; 2] = [
    "Use each line once, in order; skipping lines is fine.",
    "Two lines per message (open, OK), and one more for each NO or AGN.",
];

/// The `[weather]` presets as printed under the code table, one per line.
fn preset_lines(cfg: &Config) -> Vec<String> {
    let Some(w) = &cfg.weather else {
        return Vec::new();
    };
    let mut presets: Vec<_> = w.presets.iter().collect();
    presets.sort_by_key(|p| p.number);
    presets
        .iter()
        .map(|p| {
            format!(
                "{:>5}  {:<6}  {}",
                p.number,
                p.grid.to_ascii_uppercase(),
                p.name
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

fn default_wx(cfg: &Config) -> String {
    cfg.weather
        .as_ref()
        .map(|w| w.default_grid.to_ascii_uppercase())
        .unwrap_or_default()
}

fn sim(cfg: &Config, offline: bool) -> Result<()> {
    let inbox = node::open_inbox(cfg)?;
    let mut session = node::build_session(cfg)?;
    let mut svc: Box<dyn Services> = if offline {
        Box::new(OfflineServices {
            inbox: inbox.clone(),
        })
    } else {
        let im = hfnode::messages::probe_imessage(cfg).map(|(shared, r)| {
            if let hfnode::gateway::imessage::Readiness::NotReady(why) = r {
                println!("iMessage not available: {why}");
            }
            shared
        });
        Box::new(node::live_services(cfg, inbox.clone(), im)?)
    };
    let book = node::load_codebook(cfg)?;
    println!("Type what the field operator sends, e.g.");
    println!(
        "  {} {} {} TX MOM RUNNING LATE K",
        cfg.station.field_calls[0],
        session.last_seq() + 1,
        book.code(session.last_seq() + 1)
    );
    println!(
        "Commands: /code N (show code N), /msg NAME TEXT (add a screened inbound message), /quit"
    );
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line == "/quit" {
            break;
        } else if let Some(n) = line.strip_prefix("/code ") {
            let n: u64 = n.trim().parse().context("sequence number")?;
            println!("  code {n}: {}", book.code(n));
        } else if let Some(rest) = line.strip_prefix("/msg ") {
            let (name, text) = rest.split_once(' ').unwrap_or((rest, ""));
            let mut ib = inbox.lock().map_err(|_| anyhow::anyhow!("inbox lock"))?;
            sim_add(&mut ib, name, text, gateway::unix_now())?;
            println!("  added message from {}", name.to_ascii_uppercase());
        } else if !line.is_empty() {
            match session.handle(line, Instant::now(), svc.as_mut()) {
                Outcome::Transmit(t) => {
                    for s in &t.segments {
                        println!("  NODE> {s}");
                    }
                    if !t.read_ids.is_empty() {
                        svc.mark_read(&t.read_ids);
                    }
                }
                Outcome::Silent(why) => println!("  (silence: {why})"),
            }
        }
    }
    Ok(())
}

/// Add a screened inbound message for `hfnode sim`.
fn sim_add(ib: &mut Inbox, name: &str, text: &str, now: u64) -> Result<()> {
    // Several messages can be added within a second: number them so that each has
    // its own source id and none is taken for a duplicate.
    let mut n = 0;
    let id = loop {
        let id = format!("sim:{now}:{n}");
        if ib.add(&name.to_ascii_uppercase(), &id, text, now)? {
            break id;
        }
        n += 1;
    };
    if let Some(m) = ib.unscreened().into_iter().find(|m| m.source_id == id) {
        ib.set_screened(m.id, &sanitize(text))?;
    }
    Ok(())
}

fn decode(file: &Path, pitch: f32) -> Result<()> {
    let (samples, sr) = audio::read_wav(file)?;
    let dc = cw::DecoderConfig::new(sr, pitch);
    dc.validate()
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let mut d = cw::Decoder::new(dc);
    let mut ev = d.push(&samples);
    ev.extend(d.flush());
    println!("{}", cw::events_to_text(&ev));
    eprintln!("(speed estimate {:.0} wpm)", d.wpm());
    Ok(())
}

fn synth(
    text: &str,
    out: &Path,
    wpm: f32,
    pitch: f32,
    snr: Option<f32>,
    jitter: f32,
) -> Result<()> {
    let sr = 8000;
    let mut k = cw::Keyer::new(sr, pitch, wpm);
    k.jitter = jitter;
    let mut s = k.render(&text.to_ascii_uppercase(), 1000.0);
    if let Some(snr) = snr {
        cw::Noise::new(1).add(
            &mut s,
            cw::Noise::sigma_for_snr(k.amplitude, snr, sr, 2500.0),
        );
    }
    audio::write_wav(out, &s, sr)?;
    println!(
        "wrote {:.1} s to {}",
        s.len() as f32 / sr as f32,
        out.display()
    );
    Ok(())
}

fn listen(cfg: &Config) -> Result<()> {
    let cap = audio::Capture::start(&cfg.audio.device, cfg.audio.sample_rate)?;
    let mut dc = cw::DecoderConfig::new(cfg.audio.sample_rate, cfg.audio.pitch_hz);
    dc.bandwidth_hz = cfg.audio.bandwidth_hz;
    let mut d = cw::Decoder::new(dc);
    println!("listening on {} (Ctrl-C to stop)", cfg.audio.device);
    let mut out = std::io::stdout();
    while let Some(block) = cap.samples.recv() {
        for e in d.push(&block.samples) {
            match e {
                cw::DecodeEvent::Char(c) => print!("{c}"),
                cw::DecodeEvent::Unknown(_) => print!("*"),
                cw::DecodeEvent::WordGap => print!(" "),
            }
        }
        out.flush()?;
    }
    Ok(())
}

fn record(cfg: &Config, out: &Path, seconds: u32) -> Result<()> {
    if !(1..=3600).contains(&seconds) {
        bail!("--seconds must be 1-3600");
    }
    let rate = cfg.audio.sample_rate;
    let cap = audio::Capture::start(&cfg.audio.device, rate)?;
    println!("recording {seconds} s from {}", cfg.audio.device);
    let want = rate as usize * seconds as usize;
    let mut samples = Vec::with_capacity(want);
    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds) + 10);
    while samples.len() < want {
        let left = deadline.saturating_duration_since(Instant::now());
        match cap.samples.recv_timeout(left) {
            Ok(b) => samples.extend_from_slice(&b.samples),
            Err(_) => bail!(
                "audio stopped after {:.1} s",
                samples.len() as f32 / rate as f32
            ),
        }
    }
    samples.truncate(want);
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    audio::write_wav(out, &samples, rate)?;
    println!(
        "wrote {} ({rate} Hz mono); peak level {:.0}% of full scale{}",
        out.display(),
        peak * 100.0,
        if peak >= 0.99 {
            ": clipping, turn the radio's ACC/USB AF output level down"
        } else if peak == 0.0 {
            ": silence, no audio is reaching hfnode"
        } else {
            ""
        }
    );
    Ok(())
}

fn devices() -> Result<()> {
    use civ::ports::Match;
    println!("Serial ports (station.serial_port):");
    let ports = civ::ports::list().unwrap_or_else(|e| {
        println!("  could not list serial ports: {e}");
        Vec::new()
    });
    if ports.is_empty() {
        println!("  none found; is the radio on and its USB cable connected?");
    }
    for p in &ports {
        let usb = p.usb.as_ref().map_or(String::new(), |u| {
            let parts: Vec<&str> = [&u.manufacturer, &u.product, &u.serial_number]
                .into_iter()
                .filter_map(|s| s.as_deref())
                .collect();
            format!("  USB {:04X}:{:04X} {}", u.vid, u.pid, parts.join(", "))
        });
        let keyer_box = p
            .usb
            .as_ref()
            .and_then(|u| u.product.as_deref())
            .is_some_and(keyer::is_keyer_box);
        let note = match p.radio_match() {
            _ if keyer_box => "  <- the keyer box",
            Match::Ic7300 => "  <- the IC-7300",
            Match::Cp210x => "  <- a CP210x bridge, as in the IC-7300",
            Match::No => "",
        };
        println!("  {}{usb}{note}", p.path);
        if let Some(stable) = &p.stable_path {
            println!("      same port, a name that does not change: {stable}");
        }
    }
    println!();
    println!("Audio inputs (audio.device; {}):", audio::DEVICE_HINT);
    match audio::input_devices() {
        Ok(inputs) if inputs.is_empty() => println!("  none found"),
        Ok(inputs) => {
            for d in &inputs {
                println!(
                    "  {:?}  {}{}",
                    d.name,
                    d.detail,
                    if d.looks_like_radio() {
                        "  <- the IC-7300's USB codec"
                    } else {
                        ""
                    }
                );
            }
        }
        Err(e) => println!("  could not list audio inputs: {e}"),
    }
    Ok(())
}

fn open_radio(cfg: &Config) -> Result<civ::ic7300::Ic7300> {
    if cfg!(target_os = "macos") && civ::ports::is_macos_dialin(&cfg.station.serial_port) {
        log::warn!(
            "station.serial_port {} is a dial-in device; use the /dev/cu. one",
            cfg.station.serial_port
        );
    }
    civ::ic7300::Ic7300::open(
        &cfg.station.serial_port,
        cfg.station.baud,
        cfg.station.civ_address,
    )
    .with_context(|| format!("opening radio on {}", cfg.station.serial_port))
}

/// Open the radio for a command that writes to it: the bring-up stage must allow the
/// command, and the read-only preflight must pass, before anything is written.
fn open_for(cfg: &Config, action: Action) -> Result<civ::ic7300::Ic7300> {
    commissioning::check(cfg.station.commissioned, action, cfg.station.power_watts)?;
    let mut rig = open_radio(cfg)?;
    let report = civ::preflight::preflight(&mut rig, action == Action::Run);
    for line in report.to_string().lines() {
        log::info!("preflight: {line}");
    }
    if !report.passed() {
        let failed: Vec<&str> = report.failures().map(|c| c.name).collect();
        bail!(
            "radio preflight failed ({}); nothing was written to the radio",
            failed.join(", ")
        );
    }
    Ok(rig)
}

/// Read the settings back after `configure`, and refuse to go on unless they are
/// what was asked for.
fn verify_setup(cfg: &Config, st: &Station<civ::ic7300::Ic7300>) -> Result<()> {
    let sc = StationConfig::from_config(&cfg.station);
    let rig = st.rig();
    let mut r = rig.lock().unwrap_or_else(|e| e.into_inner());
    let report = civ::preflight::verify_setup(
        &mut r,
        &civ::preflight::Setup {
            frequency_hz: sc.frequency_hz,
            power_watts: sc.power_watts,
            key_speed_wpm: sc.key_speed_wpm,
            break_in_delay_dots: sc.break_in_delay_dots,
        },
    );
    for line in report.to_string().lines() {
        log::info!("read-back: {line}");
    }
    if !report.passed() {
        let failed: Vec<&str> = report.failures().map(|c| c.name).collect();
        bail!(
            "the radio's settings do not read back as set ({})",
            failed.join(", ")
        );
    }
    Ok(())
}

fn radio(cfg: &Config, action: RadioCmd) -> Result<()> {
    use civ::Rig;
    match cfg.station.rig {
        RigKind::Ic7300 => {}
        RigKind::Keyer => bail!(
            "station.rig is \"keyer\": the radio itself is not controlled; use `hfnode keyer \
             --config C ...` (docs/keyer.md)"
        ),
        RigKind::Handheld => {
            bail!("station.rig is \"handheld\": use `hfnode handheld ...` (docs/handheld.md)")
        }
    }
    match action {
        RadioCmd::Status => {
            let mut rig = open_radio(cfg)?;
            println!("frequency {} Hz", rig.frequency()?);
            println!("transmitting: {}", rig.is_transmitting()?);
        }
        RadioCmd::Check => {
            let mut rig = open_radio(cfg)?;
            let report = civ::preflight::preflight(&mut rig, false);
            print!("{report}");
            println!(
                "bring-up stage passed (station.commissioned): {}",
                cfg.station.commissioned
            );
            if !report.passed() {
                bail!("preflight failed: fix the FAIL lines before anything else");
            }
            println!("preflight passed; nothing was written to the radio");
        }
        RadioCmd::Rx => {
            let mut rig = open_radio(cfg)?;
            // A radio this command cannot bring back to receive must stop the node
            // too, until someone has looked at it (the audit's K5).
            let inhibit = InhibitLatch::in_dir(&cfg.state_dir);
            hfnode::station::force_receive_or_latch(&mut rig, &inhibit)
                .context("radio not confirmed on receive")?;
            println!("receive (confirmed)");
        }
        RadioCmd::Setup | RadioCmd::Tune | RadioCmd::Cw { .. } => {
            let needs = match action {
                RadioCmd::Setup => Action::Setup,
                RadioCmd::Tune => Action::Tune,
                _ => Action::Cw,
            };
            // The health log and any transmit inhibit are written there.
            std::fs::create_dir_all(&cfg.state_dir)
                .with_context(|| format!("creating state_dir {}", cfg.state_dir.display()))?;
            let rig = open_for(cfg, needs)?;
            let mut st = Station::new(
                rig,
                StationConfig::from_config(&cfg.station),
                Some(cfg.state_dir.join("health.csv")),
            );
            guard_radio(st.rig(), st.inhibit_latch());
            st.configure()?;
            verify_setup(cfg, &st)?;
            println!(
                "configured and read back: {} Hz, CW, {} W, {} wpm",
                cfg.station.frequency_hz, cfg.station.power_watts, cfg.station.key_speed_wpm
            );
            if matches!(action, RadioCmd::Tune) {
                st.start_window()?;
                println!("tuned");
            }
            if let RadioCmd::Cw { text } = action {
                let t = hfnode::session::Transmission {
                    segments: vec![sanitize(&text)],
                    read_ids: Vec::new(),
                };
                st.transmit(&t).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("sent; see health.csv for the SWR reading");
            }
        }
    }
    Ok(())
}

/// The storm stand-down for `run`: the `[storm]` section is required, so that
/// running without it is a choice made in the config (`enabled = false`).
fn start_storm_watch(cfg: &Config) -> Result<Option<Arc<StormHold>>> {
    let Some(st) = &cfg.storm else {
        bail!(
            "no [storm] section: add one with the station's latitude and longitude (see \
             hfnode.example.toml), or set `enabled = false` in it to run without the storm \
             stand-down"
        );
    };
    if !st.enabled {
        log::warn!("storm stand-down is off (storm.enabled = false)");
        return Ok(None);
    }
    let ua = cfg.storm_user_agent().context("storm user agent")?;
    let hold = hfnode::storm::spawn(st, ua, Some(cfg.state_dir.join("health.csv")))?;
    log::info!(
        "storm stand-down on: no transmitting while thunder is forecast within {} h of \
         {:.4},{:.4}",
        st.lookahead_hours,
        st.latitude.unwrap_or_default(),
        st.longitude.unwrap_or_default()
    );
    Ok(Some(hold))
}

fn storm_check(cfg: &Config) -> Result<()> {
    let st = cfg
        .storm
        .as_ref()
        .context("no [storm] section in the config")?;
    let ua = cfg
        .storm_user_agent()
        .context("storm.user_agent or weather.user_agent is required")?;
    let mut nws = hfnode::storm::NwsStorm::new(st, ua)?;
    let found = nws
        .check(gateway::unix_now() as i64)
        .context("storm check failed, so `run` would hold: no tune, no transmit")?;
    match found {
        Some(why) => println!("storm: {why}\nthe node would not tune or transmit now"),
        None => println!(
            "clear: no thunder forecast in the next {} h and no storm alerts at the station",
            st.lookahead_hours
        ),
    }
    if !st.enabled {
        println!("(storm.enabled = false: `run` does not use this check)");
    }
    Ok(())
}

fn filter_cmd(cfg: &Config, action: FilterCmd) -> Result<()> {
    use gateway::filter::{self, Screener};
    let screener = Screener::new(&cfg.filter)?;
    println!("{}", screener.describe());
    if !cfg.filter.enabled {
        println!("note: filter.enabled is false, so the node does not use this filter");
    }
    match action {
        FilterCmd::Screen { text, from } => {
            let on_air = sanitize(&text);
            let v = screener.screen(&from.to_ascii_uppercase(), &on_air)?;
            println!("{:?}: {}", v.action, v.reason);
            println!("would key: {}", filter::apply(&on_air, &v));
            Ok(())
        }
        FilterCmd::Test => {
            let mut passed = 0;
            for (i, s) in filter::SAMPLES.iter().enumerate() {
                let start = Instant::now();
                let result = screener.screen(s.from, s.text);
                let secs = start.elapsed().as_secs_f32();
                println!("{:>2}. {}", i + 1, s.text);
                match result {
                    Ok(v) => {
                        let keyed = filter::apply(s.text, &v);
                        let ok = s.expect.met(s.text, &keyed);
                        passed += ok as usize;
                        println!(
                            "    {} {:?} in {secs:.1} s ({})",
                            if ok { "ok  " } else { "FAIL" },
                            v.action,
                            v.reason
                        );
                        if keyed != s.text {
                            println!("    would key: {keyed}");
                        }
                        if !ok {
                            println!("    expected: {:?}", s.expect);
                        }
                    }
                    Err(e) => {
                        println!("    FAIL filter unavailable after {secs:.1} s: {e:#}");
                        if filter::timed_out(&e) && cfg.filter.think != Some(false) {
                            println!(
                                "    a model that thinks may not finish in time: see filter.think"
                            );
                        }
                    }
                }
            }
            let total = filter::SAMPLES.len();
            println!("{passed} of {total} as expected");
            if passed < total {
                bail!("{} sample(s) not as expected", total - passed);
            }
            Ok(())
        }
    }
}

fn run(config: &Path, cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("creating state_dir {}", cfg.state_dir.display()))?;
    // First, so the first check is likely back before the first window.
    let storm = start_storm_watch(cfg)?;
    let alerts = alert::Alerts::start(cfg, config);
    let result = run_node(cfg, &alerts, storm);
    // The station is gone: dropping it forced receive, which can still latch the
    // inhibit. Let an alert already queued go out before the process exits.
    alerts.finish(alert::EXIT_GRACE);
    result
}

fn run_node(cfg: &Config, alerts: &alert::Alerts, storm: Option<Arc<StormHold>>) -> Result<()> {
    let inbox = node::open_inbox(cfg)?;
    // Checked by the node's iMessage thread, and not ready until then.
    let im = cfg
        .imessage
        .as_ref()
        .map(|_| Arc::new(hfnode::gateway::imessage::ImShared::new()));
    let parts = NodeParts {
        session: node::build_session(cfg)?,
        svc: node::live_services(cfg, inbox.clone(), im.clone())?,
        inbox,
        im,
        alerts,
        storm,
    };
    match cfg.station.rig {
        RigKind::Ic7300 => {
            let rig = open_for(cfg, Action::Run)?;
            serve(cfg, rig, None, parts, |st| verify_setup(cfg, st))
        }
        RigKind::Keyer => {
            let (rig, cap) = open_keyer(cfg, None)?;
            serve(cfg, rig, cap, parts, |_| Ok(()))
        }
        RigKind::Handheld => {
            let rig = open_handheld(cfg, Some(handheld::Action::Run))?;
            serve(cfg, rig, None, parts, |st| {
                st.check().context(
                    "the handheld is not set up as the config says: change it at the radio",
                )
            })
        }
    }
}

/// What `run` builds before opening the radio, whichever it is.
struct NodeParts<'a> {
    inbox: Arc<Mutex<Inbox>>,
    im: Option<Arc<hfnode::gateway::imessage::ImShared>>,
    session: hfnode::session::Session,
    svc: gateway::LiveServices,
    alerts: &'a alert::Alerts,
    storm: Option<Arc<StormHold>>,
}

/// Run the node on `rig`, once `verify` has passed on the station set up. `cap` is
/// the audio capture if the rig opened one already (the keyer box's rig listens to
/// the radio from the start).
fn serve<R: civ::Rig + 'static>(
    cfg: &Config,
    rig: R,
    cap: Option<audio::Capture>,
    parts: NodeParts,
    verify: impl FnOnce(&Station<R>) -> Result<()>,
) -> Result<()> {
    let NodeParts {
        inbox,
        im,
        mut session,
        mut svc,
        alerts,
        storm,
    } = parts;
    node::spawn_inbound(cfg.clone(), inbox, im);
    let mut station = Station::new(
        rig,
        StationConfig::from_config(&cfg.station),
        Some(cfg.state_dir.join("health.csv")),
    );
    guard_radio(station.rig(), station.inhibit_latch());
    // Email the owner when transmitting is inhibited: now, if tx-inhibited was
    // already there, or when it latches.
    station.notify_inhibit(alerts.sender());
    if let Some(hold) = storm {
        // So the start-up tune is not skipped just because the first answer (up
        // to three NWS requests of at most 20 s each) is still on its way.
        if !hold.wait_for_first_check(Duration::from_secs(60)) {
            log::warn!("no storm check yet: not tuning or transmitting until one clears");
        }
        station.set_storm_hold(hold);
    }
    station.configure()?;
    verify(&station)?;
    let cap = match cap {
        Some(c) => c,
        None => audio::Capture::start(&cfg.audio.device, cfg.audio.sample_rate)?,
    };
    log::info!(
        "{} listening on {} Hz",
        cfg.station.node_call,
        cfg.station.frequency_hz
    );
    node::run(cfg, &mut station, &cap.samples, &mut session, &mut svc)
}

/// The keyer box and the radio's audio, for a command that may key (`needs`, or
/// `run` if none): the bring-up stage must allow it. The audio is captured first,
/// as the rig listens to the radio from the start.
fn open_keyer(
    cfg: &Config,
    needs: Option<keyer::Action>,
) -> Result<(keyer::rig::KeyerRig, Option<audio::Capture>)> {
    let k = keyer_section(cfg)?;
    keyer::check_stage(k.commissioned, needs.unwrap_or(keyer::Action::Run))?;
    let (cap, monitor) = keyer::bench::start_listening(cfg)?;
    let rig = keyer::bench::open_rig(cfg, monitor.clone())?;
    let band = keyer::bench::wait_for_band(&monitor, Duration::from_secs(5));
    match band.level_db {
        Some(db) => log::info!("keyer: the band is at {db:.0} dBFS"),
        None => log::warn!("keyer: no band level yet: nothing is keyed until there is one"),
    }
    if let Some(db) = band.carrier_db {
        log::warn!("keyer: {}", keyer::bench::carrier_note(db));
    }
    Ok((rig, Some(cap)))
}

fn keyer_section(cfg: &Config) -> Result<&hfnode::config::Keyer> {
    if cfg.station.rig != RigKind::Keyer {
        bail!(
            "station.rig is not \"keyer\": `hfnode keyer` is for any radio on the keyer box \
             (docs/keyer.md)"
        );
    }
    cfg.keyer.as_ref().context("no [keyer] section")
}

fn keyer_cmd(cfg: &Config, action: KeyerCmd) -> Result<()> {
    use keyer::bench;
    let k = keyer_section(cfg)?;
    match action {
        KeyerCmd::Check | KeyerCmd::Rx => {
            let (_cap, monitor) = bench::start_listening(cfg)?;
            let mut rig = bench::open_rig(cfg, monitor)?;
            if matches!(action, KeyerCmd::Rx) {
                // A key this command cannot confirm open stops the node too, until
                // someone has looked at the radio (the audit's KB-2(iii)).
                let inhibit = InhibitLatch::in_dir(&cfg.state_dir);
                hfnode::station::force_receive_or_latch(&mut rig, &inhibit)
                    .context("the radio's key is not confirmed open")?;
                // The box's key is open; the radio's, as far as its audio shows.
                let band = bench::wait_for_band(&rig.monitor(), Duration::from_secs(5));
                if !band.audio {
                    bail!("no audio from the radio: the radio's key is not confirmed open");
                }
                if let Some(db) = band.carrier_db {
                    bail!(
                        "the radio's key is not confirmed open: {}",
                        bench::carrier_note(db)
                    );
                }
                println!("key open: the box is idle and no sidetone is heard");
                return Ok(());
            }
            let (report, ok) = bench::check(&mut rig, k.min_level_dbfs);
            print!("{report}");
            println!(
                "bring-up stage passed (keyer.commissioned): {}",
                k.commissioned
            );
            if !ok {
                bail!("check failed: fix the lines above before keying");
            }
            println!("all ok; nothing was keyed");
            Ok(())
        }
        KeyerCmd::Key { .. }
        | KeyerCmd::Sidetone
        | KeyerCmd::Hangtest
        | KeyerCmd::Stucktest
        | KeyerCmd::Linktest => {
            let needs = match action {
                KeyerCmd::Key { .. } | KeyerCmd::Sidetone => keyer::Action::Key,
                _ => keyer::Action::Test,
            };
            // The health log and any transmit inhibit are written there.
            std::fs::create_dir_all(&cfg.state_dir)
                .with_context(|| format!("creating state_dir {}", cfg.state_dir.display()))?;
            let (rig, _cap) = open_keyer(cfg, Some(needs))?;
            let sc = StationConfig::from_config(&cfg.station);
            let id = sc.station_id.clone();
            let mut st = Station::new(rig, sc, Some(cfg.state_dir.join("health.csv")));
            guard_radio(st.rig(), st.inhibit_latch());
            st.configure()?;
            match action {
                KeyerCmd::Key { text } => {
                    let text = sanitize(&text);
                    // Until both box tests have passed, one piece at a time: a long
                    // text is many minutes of keying on an untested box (the safety
                    // audit's KB-4).
                    if k.commissioned < keyer::Stage::Done
                        && text.chars().count() > keyer_core::MAX_TEXT
                    {
                        bail!(
                            "{} characters: until keyer.commissioned is `done`, `keyer key` sends                              one piece of at most {} characters at a time",
                            text.chars().count(),
                            keyer_core::MAX_TEXT
                        );
                    }
                    match bench::key(&mut st, &text)? {
                        Some(j) => println!("sent: {j}"),
                        None => println!("sent"),
                    }
                    Ok(())
                }
                KeyerCmd::Sidetone => {
                    let rep = bench::sidetone(&mut st, &id)?;
                    let (text, ok) = rep.explain(bench::pitch(cfg));
                    print!("{text}");
                    if !ok {
                        bail!("sidetone check failed");
                    }
                    // Kept for later runs: the level a tone must come near to be
                    // this radio's sidetone and not the band.
                    bench::save_sidetone(&cfg.state_dir, rep.judge.tone_db)?;
                    println!(
                        "sidetone level kept in state_dir/{}: a tone at the pitch within 10 dB of                          it counts as the key held at the radio",
                        bench::SIDETONE_FILE
                    );
                    Ok(())
                }
                KeyerCmd::Linktest => {
                    let rig = st.rig();
                    let waited = rig
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .link_test("TTTT TTTT TTTT TTTT")?;
                    println!(
                        "passed: the box opened its key by itself within {:.1} s of the node's                          last line, and reports the run ended by the link going quiet",
                        waited.as_secs_f32()
                    );
                    // The test text carries no call.
                    bench::key(&mut st, &id)?;
                    println!("identified: {id}");
                    Ok(())
                }
                _ => {
                    println!("{}", bench::MANUAL_STOP);
                    let rep = if matches!(action, KeyerCmd::Hangtest) {
                        bench::hangtest(&mut st, &id, 1.0)?
                    } else {
                        bench::stucktest(&mut st, &id, 1.0)?
                    };
                    println!(
                        "longest sidetone {} ms (limit {} ms)",
                        rep.longest.as_millis(),
                        rep.limit.as_millis()
                    );
                    for n in &rep.notes {
                        println!("{n}");
                    }
                    if !rep.passed {
                        bail!("test failed: do not go on to `run`");
                    }
                    println!("passed");
                    Ok(())
                }
            }
        }
    }
}

/// Open the handheld for `action` (`None`: nothing that keys it), once the bring-up
/// stage allows it.
fn open_handheld(cfg: &Config, action: Option<handheld::Action>) -> Result<Handheld> {
    if cfg.station.rig != RigKind::Handheld {
        bail!("station.rig is not \"handheld\": use `hfnode radio ...` for the IC-7300");
    }
    let h = cfg.handheld.as_ref().context("no [handheld] section")?;
    if let Some(a) = action {
        handheld::check_stage(h.commissioned, a)?;
    }
    if cfg!(target_os = "macos") && civ::ports::is_macos_dialin(&cfg.station.serial_port) {
        log::warn!(
            "station.serial_port {} is a dial-in device; use the /dev/cu. one",
            cfg.station.serial_port
        );
    }
    let rig = Handheld::open(cfg)?;
    log::info!("{}", rig.describe());
    Ok(rig)
}

fn handheld_cmd(cfg: &Config, action: HandheldCmd) -> Result<()> {
    let h = cfg.handheld.as_ref().context("no [handheld] section")?;
    match action {
        HandheldCmd::Check => {
            let mut rig = open_handheld(cfg, None)?;
            println!("{}", rig.describe());
            let s = rig.status()?;
            println!(
                "transmitting: {}; frequency quiet for {:.1} s",
                s.tx,
                s.quiet.as_secs_f32()
            );
            let wrong = handheld_settings(&mut rig, cfg);
            println!(
                "bring-up stage passed (handheld.commissioned): {}",
                h.commissioned
            );
            if wrong > 0 {
                bail!("{wrong} setting(s) to change at the radio, then run `check` again");
            }
        }
        HandheldCmd::Rx => {
            let mut rig = open_handheld(cfg, None)?;
            hfnode::station::force_receive(&mut rig).context("radio not confirmed on receive")?;
            println!("receive (confirmed)");
        }
        HandheldCmd::Key { .. } | HandheldCmd::Linktest => {
            let rig = open_handheld(cfg, Some(handheld::Action::Key))?;
            let mut st = handheld_station(cfg, rig)?;
            match action {
                HandheldCmd::Key { text } => {
                    send_text(&mut st, &text)?;
                    println!("sent; the handheld read back as on receive after each piece");
                }
                HandheldCmd::Linktest => {
                    // Keyed outside `transmit`, so its inhibit check is made here.
                    if st.tx_inhibited() {
                        bail!(
                            "transmitting is inhibited (state_dir/{}): see why in the log \
                             before clearing it",
                            hfnode::station::INHIBIT_FILE
                        );
                    }
                    let rig = st.rig();
                    let result = rig
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .link_test("TEST TEST TEST TEST TEST TEST");
                    // The test text carries no call: identify now. The station
                    // refuses if the handheld is not confirmed on receive.
                    let id = send_text(&mut st, &format!("DE {}", cfg.station.node_call));
                    let waited = result?;
                    id?;
                    println!(
                        "passed: the firmware stopped on its own within {:.1} s of the node's last \
                         command",
                        waited.as_secs_f32()
                    );
                }
                _ => unreachable!(),
            }
        }
        HandheldCmd::Hangtest => handheld_hang_test(cfg)?,
    }
    Ok(())
}

/// Check the handheld's frequency, mode, power and break-in against `cfg`, and
/// print each; the number that are wrong.
fn handheld_settings(rig: &mut Handheld, cfg: &Config) -> usize {
    use civ::Rig;
    let hz = cfg.station.frequency_hz;
    let checks = [
        ("frequency", rig.set_frequency(hz)),
        ("mode", rig.set_mode_cw()),
        ("power", rig.set_rf_power_watts(0)),
        ("break-in", rig.set_break_in(true)),
    ];
    let mut wrong = 0;
    for (what, r) in checks {
        match r {
            Ok(()) => println!("{what}: as configured"),
            Err(e) => {
                wrong += 1;
                println!("{what}: {e}");
            }
        }
    }
    wrong
}

/// The station's safety layer on `rig`, with the radio checked.
fn handheld_station(cfg: &Config, rig: Handheld) -> Result<Station<Handheld>> {
    // The health log and any transmit inhibit are written there.
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("creating state_dir {}", cfg.state_dir.display()))?;
    let st = Station::new(
        rig,
        StationConfig::from_config(&cfg.station),
        Some(cfg.state_dir.join("health.csv")),
    );
    guard_radio(st.rig(), st.inhibit_latch());
    st.configure()
        .context("the handheld is not set up as configured (`hfnode handheld check`)")?;
    st.check()
        .context("the handheld is not set up as configured (`hfnode handheld check`)")?;
    Ok(st)
}

fn send_text(st: &mut Station<Handheld>, text: &str) -> Result<()> {
    let t = hfnode::session::Transmission {
        segments: vec![sanitize(text)],
        read_ids: Vec::new(),
    };
    st.transmit(&t).map_err(|e| anyhow::anyhow!("{e}"))
}

/// `hfnode handheld hangtest`: the firmware's watchdog must end a transmission
/// when the firmware hangs (docs/handheld.md, "Bring-up").
fn handheld_hang_test(cfg: &Config) -> Result<()> {
    let inhibit = cfg.state_dir.join(hfnode::station::INHIBIT_FILE);
    if inhibit.exists() {
        bail!(
            "transmitting is inhibited ({}): see why in the log before clearing it",
            inhibit.display()
        );
    }
    let mut rig = open_handheld(cfg, Some(handheld::Action::Hang))?;
    if handheld_settings(&mut rig, cfg) > 0 {
        bail!("change those at the radio first (`hfnode handheld check`)");
    }
    let text = rig.hang_test_text()?;
    println!(
        "keying, then hanging the firmware: its watchdog must reset the radio, which ends \
         the transmission about {} s later. Listen on the other handheld; if the node's \
         handheld is still sending after 10 s, switch it off.",
        handheld::WATCHDOG_RESET.as_secs()
    );
    let outcome = rig.hang_test(&text)?;
    drop(rig);
    let (hung_at, confirmed) = match outcome {
        handheld::HangTest::Hung { at, confirmed } => (at, confirmed),
        handheld::HangTest::Failed { error, keyed } => {
            // Stopped; but a carrier may have gone out, and needs the call.
            if keyed {
                let rig = open_handheld(cfg, None)?;
                let mut st = handheld_station(cfg, rig)?;
                send_text(&mut st, &format!("DE {}", cfg.station.node_call))?;
            }
            return Err(error);
        }
    };
    // Its port goes with the reset: wait for that before opening it again, so as
    // not to hold the old one. Opening stops anything it is sending and checks that
    // it reads receive.
    std::thread::sleep(handheld::WATCHDOG_RESET + Duration::from_secs(2));
    let reopened = loop {
        match open_handheld(cfg, None) {
            Ok(r) => break Ok(r),
            Err(e) if hung_at.elapsed() < Duration::from_secs(30) => {
                log::debug!("handheld not back yet: {e:#}");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => break Err(e),
        }
    };
    let rig = reopened.context(
        "the handheld did not answer after the hang, so its watchdog did not reset it: if it \
         is still transmitting, switch it off now",
    )?;
    // When it restarted, from how long it has been up.
    let restarted = rig
        .started_at()
        .map(|t| t.saturating_duration_since(hung_at));
    // The test text carries no call: identify now, whatever the result.
    let mut st = handheld_station(cfg, rig)?;
    send_text(&mut st, &format!("DE {}", cfg.station.node_call))?;
    let after = match restarted {
        Some(d) if handheld::HANG_RESTART.contains(&d) => d,
        Some(d) if d.is_zero() => bail!(
            "the handheld did not restart: the hang did not happen, or its watchdog did not \
             reset it (something else ended the transmission)"
        ),
        Some(d) => bail!(
            "the handheld restarted {:.1} s after the hang: not its watchdog's doing (about \
             {} s), so if you switched it off and on, the watchdog failed",
            d.as_secs_f32(),
            handheld::WATCHDOG_RESET.as_secs()
        ),
        None => bail!("the handheld reports being up for longer than this computer has"),
    };
    if !confirmed {
        println!("(the firmware's reply to the hang was lost; it restarted all the same)");
    }
    println!(
        "passed: the firmware's watchdog restarted it {:.1} s after it hung, and it reads \
         receive. If you heard the carrier stop on the other handheld, set commissioned = \
         \"done\".",
        after.as_secs_f32()
    );
    Ok(())
}

fn run_selftest(
    names: &[String],
    scale: f32,
    jobs: Option<usize>,
    list: bool,
    verbose: bool,
) -> Result<()> {
    let all = selftest::scenarios();
    if list {
        for s in &all {
            println!("{:<24} {}", s.name, s.about);
        }
        return Ok(());
    }
    if !(1.0..=selftest::MAX_SCALE).contains(&scale) {
        bail!("--scale must be from 1 to {}", selftest::MAX_SCALE);
    }
    let picked: Vec<_> = if names.is_empty() {
        all
    } else {
        let picked: Vec<_> = all
            .into_iter()
            .filter(|s| {
                names
                    .iter()
                    .any(|n| s.name == *n || (n.ends_with('-') && s.name.starts_with(n.as_str())))
            })
            .collect();
        for n in names {
            if !picked
                .iter()
                .any(|s| s.name == *n || (n.ends_with('-') && s.name.starts_with(n.as_str())))
            {
                bail!("no scenario {n:?} (see --list)");
            }
        }
        picked
    };
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, picked.len().max(1));
    let on_keyer = picked.iter().filter(|s| s.radio.keyer).count();
    println!(
        "{} scenarios at {scale}x real time, {jobs} at once: {} against the mock IC-7300{}",
        picked.len(),
        picked.len() - on_keyer,
        match on_keyer {
            0 => String::new(),
            n => format!(
                ", then {n} against the mock keyer box and radio (at most {}x)",
                selftest::KEYER_MAX_SCALE
            ),
        }
    );
    let t0 = Instant::now();
    let results = std::sync::Mutex::new(vec![None; picked.len()]);
    // The keyer scenarios hear their radio in real time, and the IC-7300 ones take
    // all the processor they can get: on a busy machine the first would then miss
    // real time. So the keyer ones run on their own, after the others.
    for keyer in [false, true] {
        let todo: Vec<usize> = (0..picked.len())
            .filter(|&i| picked[i].radio.keyer == keyer)
            .collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..jobs {
                sc.spawn(|| loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&i) = todo.get(k) else { break };
                    let s = &picked[i];
                    let out = selftest::run(s, scale);
                    if verbose {
                        print!("{}", out.render());
                    } else {
                        println!(
                            "{:<24} {}  {:>5.1} s  {}",
                            out.scenario,
                            if out.passed() { "PASS" } else { "FAIL" },
                            out.wall.as_secs_f32(),
                            out.summary()
                        );
                    }
                    results.lock().unwrap()[i] = Some(out);
                });
            }
        });
    }
    let results: Vec<_> = results
        .into_inner()
        .unwrap()
        .into_iter()
        .flatten()
        .collect();
    let failed: Vec<_> = results.iter().filter(|o| !o.passed()).collect();
    println!();
    println!(
        "{:<24} {:<6} {:>7} {:>9}",
        "scenario", "result", "wall", "radio"
    );
    for o in &results {
        println!(
            "{:<24} {:<6} {:>5.1} s {:>7.0} s",
            o.scenario,
            if o.passed() { "PASS" } else { "FAIL" },
            o.wall.as_secs_f32(),
            o.radio_time.as_secs_f32()
        );
    }
    println!(
        "\n{} passed, {} failed in {:.1} s",
        results.len() - failed.len(),
        failed.len(),
        t0.elapsed().as_secs_f32()
    );
    if !failed.is_empty() {
        if !verbose {
            for o in &failed {
                print!("\n{}", o.render());
            }
        }
        bail!("{} scenario(s) failed", failed.len());
    }
    Ok(())
}

/// SNRs as given to `--snr`: numbers, or `clean` for no noise.
fn parse_snrs(snrs: &[String]) -> Result<Vec<Option<f32>>> {
    snrs.iter()
        .map(|s| match s.trim() {
            "clean" | "none" => Ok(None),
            v => v
                .parse::<f32>()
                .map(Some)
                .with_context(|| format!("--snr {v:?}: a number or `clean`")),
        })
        .collect()
}

fn run_sweep(
    spec: selftest::SweepSpec,
    scale: f32,
    jobs: Option<usize>,
    verbose: bool,
    csv: Option<&Path>,
) -> Result<()> {
    if !(1.0..=selftest::MAX_SCALE).contains(&scale) {
        bail!("--scale must be from 1 to {}", selftest::MAX_SCALE);
    }
    if spec.wpms.iter().any(|w| !(5.0..=40.0).contains(w)) {
        bail!("--wpm must be from 5 to 40");
    }
    if spec.trials == 0 || spec.keyings.is_empty() || spec.snrs.is_empty() || spec.wpms.is_empty() {
        bail!("nothing to sweep");
    }
    let spec = spec.normalized();
    let runs = spec.runs().len();
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, runs);
    println!(
        "Sweep: {} per run, {} trials per cell, {runs} runs against the mock IC-7300 at \
         {scale}x real time, {jobs} at once",
        if spec.rx {
            "a TX exchange (open, read-back, OK, SENT) and an RX exchange"
        } else {
            "a TX exchange (open, read-back, OK, SENT)"
        },
        spec.trials
    );
    println!(
        "Message: TX {} {}{}. The operator repeats a transmission that gets no answer up \
         to 3 times, answers a wrong read-back NO and starts over once on fresh lines.",
        selftest::SWEEP_DEST,
        selftest::SWEEP_TEXT,
        if spec.rx {
            "; then RX of one waiting message"
        } else {
            ""
        }
    );
    println!();
    let t0 = Instant::now();
    let step = (runs / 10).max(1);
    let each = |done: usize, r: &selftest::TrialResult, out: &selftest::Outcome| {
        if r.hard_failure() {
            println!(
                "!!! HARD FAILURE: {} trial {}: {}\n{}",
                r.cell,
                r.trial + 1,
                r.why,
                out.render()
            );
        } else if verbose {
            println!(
                "{:<28} trial {} {:<4} {:>2} tx {:>5.1} s  {}",
                r.cell.to_string(),
                r.trial + 1,
                if r.success { "ok" } else { "FAIL" },
                r.transmissions,
                r.wall.as_secs_f32(),
                if r.success { "" } else { r.why.as_str() }
            );
            if !r.success {
                print!("{}", out.render());
            }
        }
        if done.is_multiple_of(step) || done == runs {
            eprintln!(
                "sweep: {done}/{runs} runs done, {:.0} s",
                t0.elapsed().as_secs_f32()
            );
        }
    };
    let results = selftest::sweep(&spec, scale, jobs, &each);
    let wall = t0.elapsed();
    if verbose {
        println!();
    }
    print!("{}", selftest::render_sweep(&spec, &results));
    let radio: f32 = results.iter().map(|r| r.radio_time.as_secs_f32()).sum();
    println!(
        "\n{runs} runs in {:.0} s ({:.0} s of radio time in all, {:.1} s per run on \
         average, {jobs} at once)",
        wall.as_secs_f32(),
        radio,
        results.iter().map(|r| r.wall.as_secs_f32()).sum::<f32>() / runs as f32
    );
    if let Some(path) = csv {
        std::fs::write(path, selftest::sweep_csv(&results))
            .with_context(|| format!("writing {}", path.display()))?;
        println!("raw results: {}", path.display());
    }
    let v = selftest::verdict(&results);
    if !v.ok() {
        bail!(
            "{} hard failure(s), {} should-pass trial(s) failed",
            v.hard.len(),
            v.region_failures.len()
        );
    }
    Ok(())
}

fn testvectors(out: &Path, wpms: &[f32], snrs: &[String], jitter: f32, pitch: f32) -> Result<()> {
    let snrs = parse_snrs(snrs)?;
    if wpms.iter().any(|w| !(5.0..=40.0).contains(w)) {
        bail!("--wpm must be from 5 to 40");
    }
    let files = selftest::write_vectors(out, wpms, &snrs, jitter, pitch)?;
    println!(
        "wrote {} WAV files, manifest.txt and test-only.key to {}",
        files.len(),
        out.display()
    );
    println!("TEST ONLY: the codes come from a fixed key anyone can compute.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use civ::Rig;

    fn keying() -> civ::sim::SimRig {
        let mut sim = civ::sim::SimRig::new();
        sim.set_mode_cw().unwrap();
        sim.set_break_in(true).unwrap();
        sim.stuck_key = true;
        sim.send_cw("TEST TEST TEST").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(sim.is_transmitting().unwrap());
        sim
    }

    /// A configuration for the keyer box on a port that does not exist, with the
    /// bring-up stage at `commissioned`.
    fn keyer_cfg(dir: &Path, commissioned: &str) -> Config {
        let path = dir.join("hfnode.toml");
        std::fs::write(
            &path,
            format!(
                r#"
state_dir = "{}"
[station]
node_call = "N0DE"
field_calls = ["N0CALL"]
frequency_hz = 7030000
key_speed_wpm = 20
max_key_seconds = 60
rig = "keyer"
serial_port = "/dev/does-not-exist"
[audio]
device = "default"
[keyer]
commissioned = "{commissioned}"
[auth]
key_file = "{}"
"#,
                dir.join("state").display(),
                dir.join("key").display(),
            ),
        )
        .unwrap();
        Config::load(&path).unwrap()
    }

    #[test]
    fn a_keying_command_is_refused_before_the_port_is_opened() {
        // Opening the port pulses DTR on Linux, which keys some radios: a command
        // the bring-up stage does not allow must stop before that, not after
        // (the safety audit's KB-10). A port that does not exist tells the two
        // apart: the error is the stage's, not the port's.
        let dir = tempfile::tempdir().unwrap();
        let cfg = keyer_cfg(dir.path(), "none");
        let e = match open_keyer(&cfg, Some(keyer::Action::Key)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("the port does not exist"),
        };
        assert!(e.contains("needs bring-up stage `listen`"), "{e}");
        // With the stage passed it gets as far as the port, and fails there.
        let cfg = keyer_cfg(dir.path(), "done");
        let e = match open_keyer(&cfg, Some(keyer::Action::Key)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("the port does not exist"),
        };
        assert!(!e.contains("bring-up stage"), "{e}");
    }

    #[test]
    fn a_stop_signal_puts_the_radio_on_receive() {
        let dir = tempfile::tempdir().unwrap();
        let inhibit = InhibitLatch::in_dir(dir.path());
        let radio: Radio = Arc::new(Mutex::new(keying()));
        let (held, code) = stop_radio(&radio, &inhibit);
        assert_eq!(code, 0);
        drop(held);
        assert!(!radio.lock().unwrap().is_transmitting().unwrap());
        // Nothing to stop the node starting again.
        assert!(!inhibit.is_set());
        assert!(!dir.path().join(hfnode::station::INHIBIT_FILE).exists());
    }

    #[test]
    fn a_stop_that_cannot_confirm_receive_inhibits_transmitting() {
        // The process is about to exit with the radio possibly still keying: the
        // only way to stop it keying again at the next start is the inhibit file
        // (the safety audit's KB-2(iii) for the keyer box, K5 for the IC-7300).
        let dir = tempfile::tempdir().unwrap();
        let inhibit = InhibitLatch::in_dir(dir.path());
        let mut sim = keying();
        sim.tx_jammed = true;
        let radio: Radio = Arc::new(Mutex::new(sim));
        assert_eq!(stop_radio(&radio, &inhibit).1, 1);
        assert!(inhibit.is_set());
        let file = dir.path().join(hfnode::station::INHIBIT_FILE);
        let why = std::fs::read_to_string(&file).expect("inhibit file written");
        assert!(why.contains("not confirmed on receive"), "{why}");
        // A node starting in that state transmits nothing.
        assert!(InhibitLatch::in_dir(dir.path()).is_set());
    }

    #[test]
    fn sim_messages_added_in_the_same_second_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut ib = Inbox::open(dir.path().join("inbox.json")).unwrap();
        sim_add(&mut ib, "mom", "first", 1000).unwrap();
        sim_add(&mut ib, "mom", "second", 1000).unwrap();
        let ready: Vec<String> = ib.ready().into_iter().map(|m| m.raw).collect();
        assert_eq!(ready, ["first", "second"]);
    }

    #[test]
    fn the_operating_guide_shows_the_code_sheet_as_printed() {
        let guide = include_str!("../../../docs/operating.md");
        for line in SHEET_RULES {
            assert!(
                guide.contains(line),
                "docs/operating.md does not show {line:?}"
            );
        }
    }

    #[test]
    fn the_readme_lists_every_command() {
        use clap::CommandFactory;
        let readme = include_str!("../../../README.md");
        let cli = Cli::command();
        let sub = |name: &str| {
            cli.find_subcommand(name)
                .expect(name)
                .get_subcommands()
                .map(move |c| format!("| `hfnode {name} --config C {}", c.get_name()))
                .collect::<Vec<_>>()
        };
        let rows = cli
            .get_subcommands()
            .map(|c| format!("| `hfnode {}", c.get_name()))
            .chain(sub("radio"))
            .chain(sub("keyer"));
        for row in rows.filter(|r| !r.ends_with(" help")) {
            assert!(readme.contains(&row), "README.md, Commands: no row {row}`");
        }
    }

    #[test]
    fn the_example_config_shows_the_default_alphabet() {
        let example = include_str!("../../../hfnode.example.toml");
        let shown = example
            .lines()
            .find_map(|l| l.trim().strip_prefix("# alphabet = "))
            .expect("hfnode.example.toml shows [auth] alphabet");
        let alphabet = shown.trim_matches('"');
        assert_eq!(alphabet, auth::DEFAULT_ALPHABET);
        CodeBook::with_alphabet(b"any key at all", alphabet).unwrap();
    }

    #[test]
    fn presets_are_printed_in_order_under_the_code_table() {
        let mut cfg: Config = toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
        cfg.weather.as_mut().unwrap().presets.reverse();
        cfg.weather.as_mut().unwrap().presets[0].name.clear();
        assert_eq!(
            preset_lines(&cfg),
            ["    1  DL89IG  Chisos Basin", "    2  DL89ME"]
        );
        cfg.weather = None;
        assert!(preset_lines(&cfg).is_empty());
    }
}
