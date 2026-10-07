//! Any radio as the node's radio (`station.rig = "keyer"`): keyed through its key
//! jack by the Pico 2 keyer box (firmware/pico2-keyer), its receive audio taken from
//! its headphone jack through a sound card. Nothing is set or read on the radio
//! itself: frequency, mode, power and break-in are set by hand, and the node only
//! keys it, as a straight key would, and listens to it.
//!
//! The box times the Morse itself from text the node sends ([`proto`],
//! docs/keyer-protocol.md), with limits of its own that do not depend on the node:
//! a 1 s key-down limit that trips it, a 60 s run limit, a 2 s link timeout, a 1 s
//! rest after every run, a duty budget, a watch on its own key pin and a hardware
//! watchdog ([`keyer_core`], the same code the box runs). The node keeps a duty
//! window of its own on top (`[keyer] max_duty_percent`).

pub mod bench;
pub mod link;
pub mod mock;
pub mod monitor;
pub mod proto;
pub mod rig;
#[cfg(test)]
pub(crate) mod testbench;

use crate::config::{Config, RigKind};
use anyhow::{bail, Result};
use proto::Hello;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// The US amateur bands, in Hz, where the node may key CW (47 CFR 97.301 and
/// 97.305(a), from memory, not checked against the eCFR): 160 m to 70 cm, without
/// 60 m, whose channels need more care than a frequency in a config file. The node
/// cannot check the privileges of the operator's licence class within a band.
pub const BANDS_HZ: [(u64, u64); 13] = [
    (1_800_000, 2_000_000),
    (3_500_000, 4_000_000),
    (7_000_000, 7_300_000),
    (10_100_000, 10_150_000),
    (14_000_000, 14_350_000),
    (18_068_000, 18_168_000),
    (21_000_000, 21_450_000),
    (24_890_000, 24_990_000),
    (28_000_000, 29_700_000),
    (50_000_000, 54_000_000),
    (144_000_000, 148_000_000),
    (222_000_000, 225_000_000),
    (420_000_000, 450_000_000),
];

/// The longest run limit the box may report: [`keyer_core::limits::RUN_MS`].
pub const MAX_RUN_LIMIT: Duration = Duration::from_millis(keyer_core::limits::RUN_MS as u64);
/// The longest key-down limit the box may report: no element is longer than a dash
/// at 5 wpm (720 ms), and [`keyer_core::limits::KEY_DOWN_MS`] is the box's.
pub const MAX_KEY_DOWN_LIMIT: Duration =
    Duration::from_millis(keyer_core::limits::KEY_DOWN_MS as u64);
/// The box's link timeout must lie between these: long enough that one lost reply
/// (the node waits [`REPLY_TIMEOUT`] before sending again) does not end a run,
/// short enough to end one soon after the node dies.
pub const MIN_LINK_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_LINK_TIMEOUT: Duration =
    Duration::from_millis(keyer_core::limits::LINK_TIMEOUT_MS as u64);
/// How long the node waits for each reply from the box.
pub const REPLY_TIMEOUT: Duration = Duration::from_millis(300);
/// The shortest rest after a run the box may report, and the largest duty budget:
/// [`keyer_core::limits::REST_MS`] and [`keyer_core::limits::DUTY_BUDGET_MS`].
pub const MIN_REST: Duration = Duration::from_millis(keyer_core::limits::REST_MS as u64);
pub const MAX_DUTY_BUDGET: Duration =
    Duration::from_millis(keyer_core::limits::DUTY_BUDGET_MS as u64);
/// `[keyer] max_duty_percent` and `duty_window_secs` may be no looser than these.
pub const MAX_DUTY_PERCENT: u32 = 50;
pub const MAX_DUTY_WINDOW_SECS: u64 = 600;

/// Bring-up stages for the keyer box (docs/keyer.md, "Bring-up"): `[keyer]
/// commissioned` names the last one passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Nothing has passed: `hfnode keyer check` and `rx`, `listen` and `record`
    /// only; none of them keys the radio.
    #[default]
    None,
    /// `hfnode listen` decoded the band correctly through the sound card. Allows
    /// `hfnode keyer key` and `sidetone`, with the operator at the radio.
    Listen,
    /// Short transmissions went out and were heard as sent, and the sidetone check
    /// passed. Allows `hfnode keyer hangtest` and `stucktest`.
    Keying,
    /// Both tests showed the box opening the key on its own. Allows `run`.
    Done,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Listen => "listen",
            Self::Keying => "keying",
            Self::Done => "done",
        })
    }
}

/// Commands that key the radio through the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Key,
    Test,
    Run,
}

/// Whether `action` may run with `[keyer] commissioned` at `stage`.
pub fn check_stage(stage: Stage, action: Action) -> Result<()> {
    let (needs, name) = match action {
        Action::Key => (Stage::Listen, "this keying command"),
        Action::Test => (Stage::Keying, "the box's hang and stuck-key tests"),
        Action::Run => (Stage::Done, "`run`"),
    };
    if stage < needs {
        bail!(
            "{name} needs bring-up stage `{needs}` to have passed, but \
             keyer.commissioned is `{stage}` (docs/keyer.md, \"Bring-up\")"
        );
    }
    Ok(())
}

/// The longest a keyer piece can take at `wpm`, in the box's own timing: 30 zeros,
/// the slowest characters there are.
pub fn longest_piece(wpm: u32) -> Option<Duration> {
    keyer_core::morse::run_ms(&[b'0'; keyer_core::MAX_TEXT], wpm)
        .ok()
        .map(|ms| Duration::from_millis(ms.into()))
}

/// Room left in `max_key_seconds` after the longest piece, for the radio's
/// break-in delay and the audio to show it back on receive.
pub const KEY_SECONDS_SPARE: Duration = Duration::from_secs(2);

/// The `[keyer]` settings, checked as part of [`Config::validate`].
pub fn validate(cfg: &Config) -> Result<()> {
    let s = &cfg.station;
    let k = match (s.rig, &cfg.keyer) {
        (RigKind::Ic7300 | RigKind::Handheld, _) => return Ok(()),
        (RigKind::Keyer, None) => {
            bail!("station.rig is \"keyer\" but there is no [keyer] section")
        }
        (RigKind::Keyer, Some(k)) => k,
    };
    if !BANDS_HZ
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&s.frequency_hz))
    {
        bail!(
            "station.frequency_hz {} is outside the US amateur bands (160 m to 70 cm, \
             60 m left out)",
            s.frequency_hz
        );
    }
    if s.serial_port.trim().is_empty() {
        bail!("station.serial_port must name the keyer box's serial port");
    }
    if s.max_key_seconds > MAX_RUN_LIMIT.as_secs() {
        bail!(
            "station.max_key_seconds must be {} or less with the keyer box: it ends any \
             keying run at its own limit",
            MAX_RUN_LIMIT.as_secs()
        );
    }
    // Every piece the station may send must fit the box's run limit and the
    // station's own watchdog, or a long message could never be keyed.
    let longest = longest_piece(s.key_speed_wpm).unwrap_or(Duration::MAX);
    if longest > MAX_RUN_LIMIT {
        bail!(
            "station.key_speed_wpm {} is too slow for the keyer box: 30 characters can \
             take {:.0} s, longer than its run limit of {} s (14 wpm or faster)",
            s.key_speed_wpm,
            longest.as_secs_f32(),
            MAX_RUN_LIMIT.as_secs()
        );
    }
    let need = longest + KEY_SECONDS_SPARE;
    if Duration::from_secs(s.max_key_seconds) < need {
        bail!(
            "station.max_key_seconds must be at least {} at {} wpm with the keyer box: \
             30 characters can take {:.1} s, and the radio must be heard back on receive \
             after them",
            need.as_secs_f32().ceil(),
            s.key_speed_wpm,
            longest.as_secs_f32()
        );
    }
    let pitch = k.sidetone_hz.unwrap_or(cfg.audio.pitch_hz);
    if !(300.0..=1200.0).contains(&pitch) || pitch >= cfg.audio.sample_rate as f32 / 2.5 {
        bail!(
            "keyer.sidetone_hz {pitch} must be 300-1200 Hz and well below half \
             audio.sample_rate"
        );
    }
    if !(-90.0..=-20.0).contains(&k.min_level_dbfs) {
        bail!("keyer.min_level_dbfs must be -90 to -20");
    }
    if !(10..=MAX_DUTY_PERCENT).contains(&k.max_duty_percent) {
        bail!("keyer.max_duty_percent must be 10-{MAX_DUTY_PERCENT}");
    }
    if !(60..=MAX_DUTY_WINDOW_SECS).contains(&k.duty_window_secs) {
        bail!("keyer.duty_window_secs must be 60-{MAX_DUTY_WINDOW_SECS}");
    }
    // Every piece must fit the window, counted at its whole length.
    let allows = k.duty_window_secs * u64::from(k.max_duty_percent) / 100;
    if allows < MAX_RUN_LIMIT.as_secs() {
        bail!(
            "keyer.max_duty_percent of keyer.duty_window_secs allows {allows} s keyed: it              must allow at least the box's run limit, {} s",
            MAX_RUN_LIMIT.as_secs()
        );
    }
    if let Some(b) = &k.firmware_build {
        if b.is_empty() || b.len() > 12 || b.contains(char::is_whitespace) {
            bail!("keyer.firmware_build must be the build `hfnode keyer check` reports");
        }
    }
    Ok(())
}

/// Whether a serial port's USB product string is the keyer box's: its firmware
/// reports [`keyer_core::NAME`] (firmware/pico2-keyer).
pub fn is_keyer_box(usb_product: &str) -> bool {
    usb_product.trim() == keyer_core::NAME
}

/// Whether the node can work with the box that sent `h`: the keyer box (its
/// name), this protocol version, and limits of its own no looser than the box's
/// ([`keyer_core::limits`]).
pub fn check_hello(h: &Hello) -> Result<()> {
    if h.name != keyer_core::NAME {
        bail!(
            "the device answering is {:?}, not the keyer box {}",
            h.name,
            keyer_core::NAME
        );
    }
    if h.version != keyer_core::VERSION {
        bail!(
            "the box speaks version {} of the keyer protocol; hfnode speaks {}: flash it              with this hfnode's firmware",
            h.version,
            keyer_core::VERSION
        );
    }
    if h.rest < MIN_REST {
        bail!(
            "the box's rest after a run is {} ms; it must be at least {} ms",
            h.rest.as_millis(),
            MIN_REST.as_millis()
        );
    }
    if h.duty_budget.is_zero() || h.duty_budget > MAX_DUTY_BUDGET {
        bail!(
            "the box's duty budget is {} s; it must be 1-{} s",
            h.duty_budget.as_secs(),
            MAX_DUTY_BUDGET.as_secs()
        );
    }
    if h.run_limit.is_zero() || h.run_limit > MAX_RUN_LIMIT {
        bail!(
            "the box's run limit is {} s; it must be 1-{} s",
            h.run_limit.as_secs(),
            MAX_RUN_LIMIT.as_secs()
        );
    }
    if h.key_down_limit.is_zero() || h.key_down_limit > MAX_KEY_DOWN_LIMIT {
        bail!(
            "the box's key-down limit is {} ms; it must be 1-{} ms",
            h.key_down_limit.as_millis(),
            MAX_KEY_DOWN_LIMIT.as_millis()
        );
    }
    if h.link_timeout < MIN_LINK_TIMEOUT || h.link_timeout > MAX_LINK_TIMEOUT {
        bail!(
            "the box's link timeout is {} ms; it must be {}-{} ms",
            h.link_timeout.as_millis(),
            MIN_LINK_TIMEOUT.as_millis(),
            MAX_LINK_TIMEOUT.as_millis()
        );
    }
    Ok(())
}

/// With `[keyer] firmware_build` set, whether the box runs that build.
pub fn check_build(h: &Hello, want: Option<&str>) -> Result<()> {
    match want {
        Some(b) if h.build != b => bail!(
            "the box runs firmware build {:?}, not keyer.firmware_build {b:?}: flash the              checked UF2, or set keyer.firmware_build to the build you checked",
            h.build
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
