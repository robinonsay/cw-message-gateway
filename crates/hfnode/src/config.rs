//! Node configuration, read from a TOML file. See `hfnode.example.toml`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub station: Station,
    pub audio: Audio,
    pub auth: Auth,
    #[serde(default)]
    pub schedule: Schedule,
    /// Where `last_seq`, the inbox and the radio health log are kept.
    pub state_dir: PathBuf,
    /// A pending transaction is forgotten after this long without `OK` or `NO`.
    #[serde(default = "default_pending_timeout")]
    pub pending_timeout_secs: u64,
    pub email: Option<Email>,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    pub weather: Option<Weather>,
    #[serde(default)]
    pub filter: Filter,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Station {
    /// The node's own callsign, sent as `DE <call>` on every transmission.
    pub node_call: String,
    /// Field callsigns allowed to open transactions.
    pub field_calls: Vec<String>,
    /// The agreed listening frequency in Hz (dial frequency in CW mode).
    pub frequency_hz: u64,
    pub serial_port: String,
    /// Must equal the radio's CI-V USB Baud Rate setting.
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// The radio's CI-V address.
    #[serde(default = "default_civ_address")]
    pub civ_address: u8,
    /// The last bring-up stage passed on this radio (docs/hardware-test-plan.md,
    /// "Bring-up stages"). Commands that need a later stage are refused.
    #[serde(default)]
    pub commissioned: crate::commissioning::Stage,
    /// RF output power in watts. The design calls for 30-50 W.
    #[serde(default = "default_power")]
    pub power_watts: u32,
    /// Keyer speed for the node's own transmissions.
    #[serde(default = "default_key_wpm")]
    pub key_speed_wpm: u32,
    /// Hard limit on one continuous keying run; the software watchdog forces receive
    /// after this. Keep it below the hardware PTT timer.
    #[serde(default = "default_max_key_secs")]
    pub max_key_seconds: u64,
    /// Stop transmitting for the rest of the window above this SWR.
    #[serde(default = "default_swr_limit")]
    pub swr_limit: f32,
    /// Longest chunk of a long transmission, in characters.
    #[serde(default = "default_chunk_chars")]
    pub chunk_chars: usize,
    /// Pause between chunks, in milliseconds.
    #[serde(default = "default_chunk_pause_ms")]
    pub chunk_pause_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    /// ALSA capture device for the radio's USB audio codec, as passed to `arecord -D`.
    #[serde(default = "default_audio_device")]
    pub device: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    /// The receiver's CW pitch, in Hz.
    #[serde(default = "default_pitch")]
    pub pitch_hz: f32,
    /// Silence after which a field transmission is considered finished.
    #[serde(default = "default_end_of_message_ms")]
    pub end_of_message_ms: u64,
    /// Decoder filter bandwidth in Hz.
    #[serde(default = "default_bandwidth")]
    pub bandwidth_hz: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// File holding the secret key. Generate with `hfnode keygen`.
    pub key_file: PathBuf,
    /// Code alphabet; defaults to A-Z.
    pub alphabet: Option<String>,
}

/// When the node listens. Outside a window it neither decodes nor transmits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// Listen all the time, ignoring the fields below.
    #[serde(default)]
    pub always: bool,
    /// Windows repeat with this period, aligned to the top of the UTC hour.
    #[serde(default = "default_every")]
    pub every_minutes: u32,
    #[serde(default)]
    pub offset_minutes: u32,
    #[serde(default = "default_window")]
    pub window_minutes: u32,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            always: false,
            every_minutes: default_every(),
            offset_minutes: 0,
            window_minutes: default_window(),
        }
    }
}

impl Schedule {
    /// Whether `unix_secs` falls inside a listening window.
    pub fn is_open(&self, unix_secs: u64) -> bool {
        if self.always {
            return true;
        }
        let minute = (unix_secs / 60) % (24 * 60);
        let period = self.every_minutes.max(1) as u64;
        let into = (minute + period * 24 * 60 - self.offset_minutes as u64) % period;
        into < self.window_minutes as u64
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Email {
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    pub username: String,
    /// Environment variable holding the password (never put it in the file).
    #[serde(default = "default_password_env")]
    pub password_env: String,
    pub from_address: String,
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// authserv-id your mail server puts in its Authentication-Results header (for
    /// example `mx.google.com`). Unset, the topmost such header is trusted; see
    /// `gateway::email::authenticated`.
    #[serde(default)]
    pub authserv_id: Option<String>,
}

/// Someone the field operator can message by name. SMS goes through the carrier's
/// email-to-SMS gateway (for example `5551234567@vtext.com`), so replies from a
/// phone arrive back by email too.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    /// Name as sent in CW, e.g. `MOM`. Letters and digits only.
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Weather {
    /// Grid square used when `WX` is sent without a place, e.g. where the field
    /// operator is headed.
    pub default_grid: String,
    /// api.weather.gov asks for a contact in the User-Agent.
    pub user_agent: String,
    /// How many forecast periods to send (each is about half a day).
    #[serde(default = "default_periods")]
    pub periods: usize,
    /// Numbered places the field operator can ask for as `WX <number>`. They are
    /// printed under the code table.
    #[serde(default)]
    pub presets: Vec<Preset>,
}

/// A usual spot, so that `WX 3` stands for a grid square.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Keyed after `WX`: 1 to 99.
    pub number: u32,
    /// The 4- or 6-character grid square the forecast is for.
    pub grid: String,
    /// What the place is, for the printed code table only. Never transmitted.
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Screen inbound messages before they are keyed on air. Turning this off means
    /// third-party text is transmitted unreviewed.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Extra policy text appended to the built-in instructions.
    #[serde(default)]
    pub extra_policy: String,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key_env: default_api_key_env(),
            model: default_model(),
            extra_policy: String::new(),
        }
    }
}

fn yes() -> bool {
    true
}
fn default_pending_timeout() -> u64 {
    600
}
fn default_baud() -> u32 {
    115_200
}
fn default_civ_address() -> u8 {
    0x94
}
fn default_power() -> u32 {
    40
}
fn default_key_wpm() -> u32 {
    18
}
fn default_max_key_secs() -> u64 {
    45
}
fn default_swr_limit() -> f32 {
    2.0
}
fn default_chunk_chars() -> usize {
    60
}
fn default_chunk_pause_ms() -> u64 {
    2000
}
fn default_audio_device() -> String {
    "plughw:CARD=CODEC,DEV=0".into()
}
fn default_sample_rate() -> u32 {
    8000
}
fn default_pitch() -> f32 {
    600.0
}
fn default_end_of_message_ms() -> u64 {
    3000
}
fn default_bandwidth() -> f32 {
    150.0
}
fn default_every() -> u32 {
    60
}
fn default_window() -> u32 {
    10
}
fn default_smtp_port() -> u16 {
    465
}
fn default_imap_port() -> u16 {
    993
}
fn default_password_env() -> String {
    "HFNODE_EMAIL_PASSWORD".into()
}
fn default_poll_secs() -> u64 {
    300
}
fn default_periods() -> usize {
    2
}
fn default_api_key_env() -> String {
    "ANTHROPIC_API_KEY".into()
}
fn default_model() -> String {
    "claude-opus-5-5".into()
}

/// The IC-7300's transmitter frequency coverage in Hz, inclusive, from the manual's
/// "Frequency coverage" table (Section 16 SPECIFICATIONS, p. 16-2). Which of these
/// the radio actually transmits on depends on its version.
const TX_COVERAGE_HZ: [(u64, u64); 12] = [
    (1_800_000, 1_999_999),
    (3_500_000, 3_999_999),
    (5_255_000, 5_405_000),
    (7_000_000, 7_300_000),
    (10_100_000, 10_150_000),
    (14_000_000, 14_350_000),
    (18_068_000, 18_168_000),
    (21_000_000, 21_450_000),
    (24_890_000, 24_990_000),
    (28_000_000, 29_700_000),
    (50_000_000, 54_000_000),
    (70_000_000, 70_500_000),
];

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let s = &self.station;
        if s.node_call.trim().is_empty() {
            bail!("station.node_call is required");
        }
        if s.field_calls.is_empty() {
            bail!("station.field_calls must list at least one callsign");
        }
        // Callsigns are keyed with CI-V 17, which sends only its own character set
        // (p. 19-13); a callsign uses letters, digits and "/".
        for call in std::iter::once(&s.node_call).chain(&s.field_calls) {
            if !call.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
                bail!("callsign {call:?}: only letters, digits and \"/\" can be keyed");
            }
        }
        // Only rates and addresses the radio can be set to (p. 12-10, 12-11).
        civ::ic7300::check_link_settings(s.baud, s.civ_address)
            .map_err(|e| anyhow::anyhow!("station.baud / station.civ_address: {e}"))?;
        if !(1..=100).contains(&s.power_watts) {
            bail!("station.power_watts must be 1-100");
        }
        if s.max_key_seconds == 0 || s.max_key_seconds > 120 {
            bail!("station.max_key_seconds must be 1-120");
        }
        if !(1.1..=3.0).contains(&s.swr_limit) {
            bail!("station.swr_limit must be between 1.1 and 3.0");
        }
        // The radio's keyer runs 6-48 wpm (14 0C: "00 00=6wpm, 02 55=48wpm", p. 19-3);
        // stuck-key timing is computed from this value, so it must match what is sent.
        if !(6..=48).contains(&s.key_speed_wpm) {
            bail!("station.key_speed_wpm must be 6-48");
        }
        if !TX_COVERAGE_HZ
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&s.frequency_hz))
        {
            bail!(
                "station.frequency_hz {} is outside the IC-7300's amateur transmit coverage",
                s.frequency_hz
            );
        }
        for c in &self.contacts {
            if c.name.is_empty()
                || !c
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
            {
                bail!(
                    "contact name {:?} must be uppercase letters and digits",
                    c.name
                );
            }
        }
        for (i, c) in self.contacts.iter().enumerate() {
            if self.contacts[..i].iter().any(|o| o.name == c.name) {
                bail!("contact name {:?} is listed twice", c.name);
            }
            // A bare address: the gateway sends to it (parsed as a Mailbox, which
            // refuses some addresses that Address takes) and matches replies
            // against it.
            let bare = c.address.parse::<lettre::Address>().is_ok()
                && c.address
                    .parse::<lettre::message::Mailbox>()
                    .is_ok_and(|m| m.name.is_none());
            if !bare {
                bail!(
                    "contact {} address {:?} is not an email address",
                    c.name,
                    c.address
                );
            }
        }
        if let Some(w) = &self.weather {
            if !protocol::is_grid(&w.default_grid) {
                bail!(
                    "weather.default_grid {:?} is not a grid square",
                    w.default_grid
                );
            }
            if w.user_agent.trim().is_empty() {
                bail!(
                    "weather.user_agent is required; api.weather.gov refuses requests without one"
                );
            }
            if !(1..=6).contains(&w.periods) {
                bail!("weather.periods must be 1-6");
            }
            for (i, p) in w.presets.iter().enumerate() {
                if !(1..=99).contains(&p.number) {
                    bail!("weather preset number {} must be 1-99", p.number);
                }
                if w.presets[..i].iter().any(|o| o.number == p.number) {
                    bail!("weather preset {} is listed twice", p.number);
                }
                if !protocol::is_grid(&p.grid) {
                    bail!(
                        "weather preset {} grid {:?} is not a grid square",
                        p.number,
                        p.grid
                    );
                }
                if p.name.chars().any(char::is_control) {
                    bail!("weather preset {} name must be one line", p.number);
                }
            }
        }
        if self.schedule.window_minutes > self.schedule.every_minutes {
            bail!("schedule.window_minutes is longer than schedule.every_minutes");
        }
        if self.schedule.offset_minutes >= self.schedule.every_minutes {
            bail!("schedule.offset_minutes must be less than schedule.every_minutes");
        }
        // Bad audio settings would leave the decoder silently deaf.
        self.decoder_config()
            .validate()
            .map_err(|e| anyhow::anyhow!("audio.{e}"))?;
        Ok(())
    }

    /// Decoder settings for the receiver audio.
    pub fn decoder_config(&self) -> cw::DecoderConfig {
        let mut d = cw::DecoderConfig::new(self.audio.sample_rate, self.audio.pitch_hz);
        d.bandwidth_hz = self.audio.bandwidth_hz;
        d
    }

    pub fn contact_names(&self) -> Vec<String> {
        self.contacts.iter().map(|c| c.name.clone()).collect()
    }

    /// The weather presets as (number, uppercased grid); empty without `[weather]`.
    pub fn weather_presets(&self) -> Vec<(u32, String)> {
        self.weather
            .iter()
            .flat_map(|w| &w.presets)
            .map(|p| (p.number, p.grid.to_ascii_uppercase()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../hfnode.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.station.civ_address, 0x94);
    }

    fn example() -> Config {
        toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap()
    }

    #[test]
    fn rejects_key_speed_outside_radio_range() {
        for (wpm, ok) in [(5, false), (6, true), (48, true), (49, false), (80, false)] {
            let mut cfg = example();
            cfg.station.key_speed_wpm = wpm;
            assert_eq!(cfg.validate().is_ok(), ok, "{wpm} wpm");
        }
    }

    #[test]
    fn rejects_link_settings_the_radio_does_not_have() {
        for (baud, addr, ok) in [
            (115_200, 0x94, true),
            (4800, 0x02, true),
            (19_200, 0xDF, true),
            (115_201, 0x94, false),
            (230_400, 0x94, false),
            (115_200, 0x00, false),
            (115_200, 0xE0, false),
        ] {
            let mut cfg = example();
            cfg.station.baud = baud;
            cfg.station.civ_address = addr;
            assert_eq!(cfg.validate().is_ok(), ok, "{baud} {addr:02X}");
        }
    }

    #[test]
    fn callsigns_must_be_keyable() {
        for (call, ok) in [
            ("N0CALL", true),
            ("N0CALL/P", true),
            ("n0call", true),
            ("N0 CALL", false),
            ("N0CALL-1", false),
            ("Ñ0CALL", false),
        ] {
            let mut cfg = example();
            cfg.station.node_call = call.into();
            assert_eq!(cfg.validate().is_ok(), ok, "node {call}");
            let mut cfg = example();
            cfg.station.field_calls.push(call.into());
            assert_eq!(cfg.validate().is_ok(), ok, "field {call}");
        }
    }

    #[test]
    fn commissioning_defaults_to_nothing_passed() {
        assert_eq!(
            example().station.commissioned,
            crate::commissioning::Stage::None
        );
    }

    #[test]
    fn rejects_frequency_outside_tx_coverage() {
        for (hz, ok) in [
            (7_030_000, true),
            (1_800_000, true),
            (29_700_000, true),
            (70_500_000, true),
            (0, false),
            (7_300_001, false),
            (11_000_000, false),
            (145_000_000, false),
        ] {
            let mut cfg = example();
            cfg.station.frequency_hz = hz;
            assert_eq!(cfg.validate().is_ok(), ok, "{hz} Hz");
        }
    }

    #[test]
    fn rejects_bad_contacts() {
        let mut cfg = example();
        cfg.contacts[1].address = "bob at example.com".into();
        assert!(cfg.validate().is_err());
        let mut cfg = example();
        cfg.contacts[1].address = "Bob <bob@example.com>".into();
        assert!(cfg.validate().is_err());
        // Taken as an Address, but the mailer cannot send to them.
        for address in ["bob@[127.0.0.1]", "\"a b\"@example.com"] {
            assert!(address.parse::<lettre::message::Mailbox>().is_err());
            let mut cfg = example();
            cfg.contacts[1].address = address.into();
            assert!(cfg.validate().is_err(), "{address}");
        }
        let mut cfg = example();
        cfg.contacts[1].name = cfg.contacts[0].name.clone();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_weather() {
        let mut cfg = example();
        cfg.weather.as_mut().unwrap().user_agent = " ".into();
        assert!(cfg.validate().is_err());
        for (periods, ok) in [(0, false), (1, true), (6, true), (7, false), (1000, false)] {
            let mut cfg = example();
            cfg.weather.as_mut().unwrap().periods = periods;
            assert_eq!(cfg.validate().is_ok(), ok, "{periods} periods");
        }
    }

    #[test]
    fn weather_presets() {
        let cfg = example();
        let presets = cfg.weather_presets();
        assert!(!presets.is_empty(), "the example shows presets");
        assert!(presets.iter().all(|(_, g)| protocol::is_grid(g)));
        let bad = |edit: fn(&mut Weather)| {
            let mut cfg = example();
            edit(cfg.weather.as_mut().unwrap());
            cfg.validate().is_err()
        };
        assert!(bad(|w| w.presets[0].number = 0));
        assert!(bad(|w| w.presets[0].number = 100));
        assert!(bad(|w| w.presets[0].grid = "ZZ99".into()));
        assert!(bad(|w| w.presets[0].name = "two\nlines".into()));
        assert!(bad(|w| {
            let again = w.presets[0].clone();
            w.presets.push(again);
        }));
        // Lowercase subsquare letters are fine and come out uppercased.
        let mut cfg = example();
        cfg.weather.as_mut().unwrap().presets[0].grid = "DL89ig".into();
        cfg.validate().unwrap();
        assert_eq!(cfg.weather_presets()[0].1, "DL89IG");
    }

    #[test]
    fn rejects_offset_beyond_period() {
        let mut cfg = example();
        cfg.schedule.offset_minutes = 59;
        cfg.validate().unwrap();
        cfg.schedule.offset_minutes = 60;
        assert!(cfg.validate().is_err());
        cfg.schedule.offset_minutes = 100_000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_audio_parameters() {
        let text = include_str!("../../../hfnode.example.toml");
        let base: Config = toml::from_str(text).unwrap();
        for (sr, pitch, bw) in [
            (0, 600.0, 150.0),
            (8000, 600.0, 0.0),
            (8000, 600.0, -150.0),
            (8000, 9000.0, 150.0),
            (8000, 600.0, f32::NAN),
        ] {
            let mut cfg = base.clone();
            cfg.audio.sample_rate = sr;
            cfg.audio.pitch_hz = pitch;
            cfg.audio.bandwidth_hz = bw;
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.starts_with("audio."), "{sr} {pitch} {bw}: {err}");
        }
    }

    #[test]
    fn schedule_windows() {
        let s = Schedule {
            always: false,
            every_minutes: 60,
            offset_minutes: 0,
            window_minutes: 10,
        };
        assert!(s.is_open(0));
        assert!(s.is_open(9 * 60 + 59));
        assert!(!s.is_open(10 * 60));
        assert!(s.is_open(3600 * 5 + 30));
        let s = Schedule {
            offset_minutes: 30,
            ..s
        };
        assert!(!s.is_open(0));
        assert!(s.is_open(35 * 60));
        assert!(Schedule { always: true, ..s }.is_open(12345));
    }
}
