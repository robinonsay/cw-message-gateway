//! `hfnode`: the HF CW message gateway node and its tools.

use anyhow::{bail, Context, Result};
use auth::{format_for_print, CodeBook};
use clap::{Parser, Subcommand};
use hfnode::commissioning::{self, Action};
use hfnode::config::Config;
use hfnode::gateway::OfflineServices;
use hfnode::inbox::Inbox;
use hfnode::session::{Outcome, Services};
use hfnode::station::{Station, StationConfig};
use hfnode::{alert, audio, gateway, node, selftest};
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
    /// Run the closed-loop scenarios against a mock IC-7300: no radio, sound card,
    /// network or config needed. Exits non-zero if any fails.
    Selftest {
        /// Run only these scenarios (exact name, or a prefix such as `fault-`).
        #[arg(long)]
        scenario: Vec<String>,
        /// Times faster than real time, 1 to 200; lower it on a slow machine.
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The self-test runs the node many times over; its log is noise unless asked for.
    let level = if matches!(cli.cmd, Cmd::Selftest { .. }) {
        "off"
    } else {
        "info"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
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
        Cmd::Radio { config, action } => radio(&Config::load(&config)?, action),
        Cmd::Run { config } => run(&config, &Config::load(&config)?),
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
    Ok(())
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

fn open_radio(cfg: &Config) -> Result<civ::ic7300::Ic7300> {
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
            hfnode::station::force_receive(&mut rig).context("radio not confirmed on receive")?;
            println!("receive (confirmed)");
        }
        RadioCmd::Setup | RadioCmd::Tune | RadioCmd::Cw { .. } => {
            let needs = match action {
                RadioCmd::Setup => Action::Setup,
                RadioCmd::Tune => Action::Tune,
                _ => Action::Cw,
            };
            let rig = open_for(cfg, needs)?;
            let mut st = Station::new(
                rig,
                StationConfig::from_config(&cfg.station),
                Some(cfg.state_dir.join("health.csv")),
            );
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

fn run(config: &Path, cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let alerts = alert::Alerts::start(cfg, config);
    let result = run_node(cfg, &alerts);
    // The station is gone: dropping it forced receive, which can still latch the
    // inhibit. Let an alert already queued go out before the process exits.
    alerts.finish(alert::EXIT_GRACE);
    result
}

fn run_node(cfg: &Config, alerts: &alert::Alerts) -> Result<()> {
    let inbox = node::open_inbox(cfg)?;
    let mut session = node::build_session(cfg)?;
    let mut svc = node::live_services(cfg, inbox.clone())?;
    let rig = open_for(cfg, Action::Run)?;
    node::spawn_inbound(cfg.clone(), inbox);
    let mut station = Station::new(
        rig,
        StationConfig::from_config(&cfg.station),
        Some(cfg.state_dir.join("health.csv")),
    );
    // Email the owner when transmitting is inhibited: now, if tx-inhibited was
    // already there, or when it latches.
    station.notify_inhibit(alerts.sender());
    station.configure()?;
    verify_setup(cfg, &station)?;
    let cap = audio::Capture::start(&cfg.audio.device, cfg.audio.sample_rate)?;
    log::info!(
        "{} listening on {} Hz",
        cfg.station.node_call,
        cfg.station.frequency_hz
    );
    node::run(cfg, &mut station, &cap.samples, &mut session, &mut svc)
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
    println!(
        "{} scenarios against the mock IC-7300 at {scale}x real time, {jobs} at once",
        picked.len()
    );
    let t0 = Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(vec![None; picked.len()]);
    std::thread::scope(|sc| {
        for _ in 0..jobs {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(s) = picked.get(i) else { break };
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
