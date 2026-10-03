//! `hfnode`: the HF CW message gateway node and its tools.

use anyhow::{bail, Context, Result};
use auth::{format_for_print, CodeBook};
use clap::{Parser, Subcommand};
use hfnode::config::Config;
use hfnode::gateway::OfflineServices;
use hfnode::session::{Outcome, Services};
use hfnode::station::{Station, StationConfig};
use hfnode::{audio, gateway, node};
use protocol::sanitize;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    /// Talk to the radio directly, for bench testing.
    Radio {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: RadioCmd,
    },
    /// Run the node.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum RadioCmd {
    /// Read the frequency (receive only, safe).
    Status,
    /// Put the radio in the node's operating state (frequency, CW, power, keyer).
    Setup,
    /// Run the antenna tuner (transmits briefly).
    Tune,
    /// Key a short CW message and report SWR (transmits).
    Cw { text: String },
    /// Force the radio back to receive.
    Rx,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match Cli::parse().cmd {
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
        Cmd::Radio { config, action } => radio(&Config::load(&config)?, action),
        Cmd::Run { config } => run(&Config::load(&config)?),
    }
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
    println!("Use each line once, in order; skipping lines is fine. Two lines per message.");
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
    Ok(())
}

fn sim(cfg: &Config, offline: bool) -> Result<()> {
    let inbox = node::open_inbox(cfg)?;
    let mut session = node::build_session(cfg)?;
    let mut svc: Box<dyn Services> = if offline {
        Box::new(OfflineServices {
            inbox: inbox.clone(),
        })
    } else {
        Box::new(node::live_services(cfg, inbox.clone())?)
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
            let id = format!("sim:{}", gateway::unix_now());
            ib.add(&name.to_ascii_uppercase(), &id, text, gateway::unix_now())?;
            if let Some(m) = ib.unscreened().into_iter().find(|m| m.source_id == id) {
                ib.set_screened(m.id, &sanitize(text))?;
            }
            println!("  added message from {}", name.to_ascii_uppercase());
        } else if !line.is_empty() {
            match session.handle(line, Instant::now(), svc.as_mut()) {
                Outcome::Transmit(t) => {
                    for s in &t.segments {
                        println!("  NODE> {s}");
                    }
                }
                Outcome::Silent(why) => println!("  (silence: {why})"),
            }
        }
    }
    Ok(())
}

fn decode(file: &Path, pitch: f32) -> Result<()> {
    let (samples, sr) = audio::read_wav(file)?;
    let mut d = cw::Decoder::new(cw::DecoderConfig::new(sr, pitch));
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
    for block in cap.samples.iter() {
        for e in d.push(&block) {
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

fn open_radio(cfg: &Config) -> Result<civ::ic7300::Ic7300> {
    civ::ic7300::Ic7300::open(
        &cfg.station.serial_port,
        cfg.station.baud,
        cfg.station.civ_address,
    )
    .with_context(|| format!("opening radio on {}", cfg.station.serial_port))
}

fn radio(cfg: &Config, action: RadioCmd) -> Result<()> {
    use civ::Rig;
    let mut rig = open_radio(cfg)?;
    match action {
        RadioCmd::Status => {
            println!("frequency {} Hz", rig.frequency()?);
            println!("transmitting: {}", rig.is_transmitting()?);
        }
        RadioCmd::Rx => {
            rig.stop_cw()?;
            rig.set_transmit(false)?;
            println!("receive");
        }
        RadioCmd::Setup | RadioCmd::Tune | RadioCmd::Cw { .. } => {
            let mut st = Station::new(
                rig,
                StationConfig::from_config(&cfg.station),
                Some(cfg.state_dir.join("health.csv")),
            );
            st.configure()?;
            println!(
                "configured: {} Hz, CW, {} W, {} wpm",
                cfg.station.frequency_hz, cfg.station.power_watts, cfg.station.key_speed_wpm
            );
            if matches!(action, RadioCmd::Tune) {
                st.start_window()?;
                println!("tuned");
            }
            if let RadioCmd::Cw { text } = action {
                let t = hfnode::session::Transmission {
                    segments: vec![sanitize(&text)],
                };
                st.transmit(&t).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("sent; see health.csv for the SWR reading");
            }
        }
    }
    Ok(())
}

fn run(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let inbox = node::open_inbox(cfg)?;
    let mut session = node::build_session(cfg)?;
    let mut svc = node::live_services(cfg, inbox.clone())?;
    node::spawn_inbound(cfg.clone(), inbox);
    let rig = open_radio(cfg)?;
    let mut station = Station::new(
        rig,
        StationConfig::from_config(&cfg.station),
        Some(cfg.state_dir.join("health.csv")),
    );
    station.configure()?;
    let cap = audio::Capture::start(&cfg.audio.device, cfg.audio.sample_rate)?;
    log::info!(
        "{} listening on {} Hz",
        cfg.station.node_call,
        cfg.station.frequency_hz
    );
    node::run(cfg, &mut station, &cap.samples, &mut session, &mut svc)
}
