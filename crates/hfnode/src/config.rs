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
    /// The node's Google Voice number, for texting contacts' phones.
    pub google_voice: Option<GoogleVoice>,
    /// iMessage through Messages on this Mac (macOS only).
    pub imessage: Option<Imessage>,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    pub weather: Option<Weather>,
    /// Storm stand-down. `run` refuses to start without this section.
    pub storm: Option<Storm>,
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
    /// Capture device for the radio's USB audio codec. On Linux an ALSA device as
    /// passed to `arecord -D`; on macOS and Windows the input device's name, or a part
    /// of it that matches no other input. `hfnode devices` lists them.
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

/// When the node listens, and how it looks after the radio while it does. By default
/// it listens all the time; with `always = false` only in windows, and outside a
/// window it neither decodes nor transmits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// Listen all the time (the default), ignoring the three window fields below.
    #[serde(default = "yes")]
    pub always: bool,
    /// Windows repeat with this period, aligned to the top of the UTC hour.
    #[serde(default = "default_every")]
    pub every_minutes: u32,
    #[serde(default)]
    pub offset_minutes: u32,
    #[serde(default = "default_window")]
    pub window_minutes: u32,
    /// While listening and hearing nothing, set the radio up again and check it this
    /// often, as at a window start but without tuning: nothing is transmitted. It is
    /// also done before every transmission.
    #[serde(default = "default_check")]
    pub check_minutes: u32,
    /// Before a reply, tune again if the last tune (at start-up, at a window start or
    /// before an earlier reply) is older than this. A lockout after a high SWR or a
    /// tuner that could not match lasts until then, or until the next window.
    #[serde(default = "default_retune")]
    pub retune_minutes: u32,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            always: true,
            every_minutes: default_every(),
            offset_minutes: 0,
            window_minutes: default_window(),
            check_minutes: default_check(),
            retune_minutes: default_retune(),
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
    /// Where to email an alert when the node stops transmitting (the transmit
    /// inhibit, see `alert`). Unset, the inhibit is only logged.
    #[serde(default)]
    pub alert_to: Option<String>,
}

/// Someone the field operator can message by name, reached by iMessage, a text from
/// the node's Google Voice number, or email: see [`crate::gateway::route`] for which
/// is used. A contact needs at least one of `address`, `phone` and `imessage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    /// Name as sent in CW, e.g. `MOM`. Letters and digits only.
    pub name: String,
    /// Email address. A carrier email-to-SMS address (e.g. `5551234567@vtext.com`)
    /// still works where the carrier keeps its gateway, but is never used for TX
    /// while `phone` is set.
    #[serde(default)]
    pub address: Option<String>,
    /// Mobile number, texted from the node's Google Voice number (`[google_voice]`).
    #[serde(default)]
    pub phone: Option<Phone>,
    /// iMessage handles (a phone number or an Apple ID email), as a string or a list:
    /// replies from any of them are taken, and TX goes to the first.
    #[serde(default, deserialize_with = "one_or_many_handles")]
    pub imessage: Vec<Handle>,
}

/// A phone number in E.164 form, e.g. `+15551234567`. Written in the config with or
/// without spaces, dashes, dots and brackets; a 10-digit number is taken as +1.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Phone(String);

impl Phone {
    pub fn parse(s: &str) -> Option<Self> {
        canonical_phone(s).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is a US or Canadian number (+1 and 10 digits).
    pub fn is_nanp(&self) -> bool {
        self.0.len() == 12 && self.0.starts_with("+1")
    }
}

impl std::fmt::Display for Phone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Phone {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Phone::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "{s:?} is not a phone number: write it with its country code, e.g. \"+1 555 123 4567\""
            ))
        })
    }
}

/// An iMessage handle: a phone number, or an Apple ID email address (lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handle {
    Phone(Phone),
    Email(String),
}

impl Handle {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        // Never anything osascript could read as an option.
        if s.starts_with('-') {
            return None;
        }
        if s.contains('@') {
            return is_bare_address(s).then(|| Handle::Email(s.to_ascii_lowercase()));
        }
        Phone::parse(s).map(Handle::Phone)
    }

    /// As given to Messages, and as compared with Messages' handles ([`handle_key`]).
    pub fn as_str(&self) -> &str {
        match self {
            Handle::Phone(p) => p.as_str(),
            Handle::Email(e) => e,
        }
    }
}

impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn one_or_many_handles<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Handle>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let list = match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    };
    list.iter()
        .map(|s| {
            Handle::parse(s).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "{s:?} is not an iMessage handle: write a phone number with its country \
                     code (\"+1 555 123 4567\") or an Apple ID email address"
                ))
            })
        })
        .collect()
}

/// A phone number in E.164 form (`+` and 8-15 digits), from one written with spaces,
/// dashes, dots or brackets. Ten digits are a US/Canadian number; eleven starting
/// with 1 are too. Anything else (letters, short codes) is not a phone number.
pub fn canonical_phone(s: &str) -> Option<String> {
    let s: String = s
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    let (plus, digits) = match s.strip_prefix('+') {
        Some(d) => (true, d),
        None => (false, s.as_str()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match (plus, digits.len()) {
        (true, 8..=15) => Some(format!("+{digits}")),
        (false, 10) => Some(format!("+1{digits}")),
        (false, 11) if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

/// The form in which a Messages handle (`handle.id`, `chat.chat_identifier`) is
/// compared with contacts' handles: an email lowercased, a phone number in E.164.
/// `None` never matches. Only equal keys match: never by suffix or last digits.
pub fn handle_key(id: &str) -> Option<String> {
    let id = id.trim();
    if id.contains('@') {
        Some(id.to_ascii_lowercase())
    } else {
        canonical_phone(id)
    }
}

/// An address the mailer can send to and replies can be matched against: parsed as
/// a Mailbox too (which refuses some addresses that Address takes), with no name.
fn is_bare_address(s: &str) -> bool {
    s.parse::<lettre::Address>().is_ok()
        && s.parse::<lettre::message::Mailbox>()
            .is_ok_and(|m| m.name.is_none())
}

/// Domain of the addresses Google Voice forwards texts from (and takes replies at).
pub const GOOGLE_VOICE_DOMAIN: &str = "txt.voice.google.com";

/// Texting from a Google Voice number: texts to it are forwarded to the `[email]`
/// account (which must be the Google account that has the number), and the node
/// answers them by email. See docs/texting.md.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleVoice {
    /// The node's own Google Voice number.
    pub number: Phone,
    /// Put before every text, `{call}` being the field callsign that sent it.
    #[serde(default = "default_text_tag")]
    pub tag: String,
}

/// iMessage through Messages on this Mac, as the signed-in Apple ID. Works only when
/// the node is started by hfnode.command in Terminal. See docs/texting.md.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Imessage {
    /// Messages' database.
    #[serde(default = "default_chat_db")]
    pub db: PathBuf,
    /// How often to look for replies, in seconds.
    #[serde(default = "default_im_poll")]
    pub poll_secs: u64,
    /// Replies are read only within this many hours of an iMessage sent by TX.
    #[serde(default = "default_reply_hours")]
    pub reply_hours: u64,
    /// Put before every iMessage, `{call}` being the field callsign that sent it.
    #[serde(default = "default_text_tag")]
    pub tag: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Weather {
    /// Grid square for `WX` sent without a place, until the field callsign has
    /// confirmed a place of its own (then `WX` alone is that place, kept in
    /// `<state_dir>/wx_last.json`). E.g. where the field operator is headed.
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

/// No tuning or transmitting while the NWS forecasts or warns of thunder at the
/// station (crate::storm).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storm {
    /// `false` runs the node without the stand-down; `run` needs the section either
    /// way, so that is a choice someone made.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Where the station's antenna is, in decimal degrees (north and east
    /// positive). Not the field operator's location.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// How many hours of the hourly forecast, from now, to look through for thunder.
    #[serde(default = "default_storm_lookahead")]
    pub lookahead_hours: u32,
    /// How often to ask the NWS.
    #[serde(default = "default_storm_check")]
    pub check_minutes: u64,
    /// Minutes without thunder in the forecast or alerts before transmitting again.
    #[serde(default = "default_storm_clear")]
    pub clear_minutes: u64,
    /// Contact for api.weather.gov; `weather.user_agent` if not set.
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Screen inbound messages before they are keyed on air. Turning this off means
    /// third-party text is transmitted unreviewed.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Which model screens messages: the Claude API, or a model served by Ollama.
    #[serde(default)]
    pub provider: Provider,
    /// Claude only: environment variable holding the API key.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Model name. Defaults to `claude-opus-5-5` for Claude; required for Ollama.
    #[serde(default)]
    pub model: Option<String>,
    /// Where the model is served. Defaults to `https://api.anthropic.com` for Claude
    /// and `http://localhost:11434` for Ollama.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Ollama only: CPU threads the model may use (unset: Ollama decides). On a
    /// Raspberry Pi, leave cores free for the CW decoder.
    #[serde(default)]
    pub threads: Option<u32>,
    /// Ollama only: whether a model that can reason before answering does so.
    /// Unset, the model decides (those that can, do). `false` is much faster, but
    /// some models then judge worse: check with `hfnode filter test`. Some models,
    /// gpt-oss among them, reason whatever this says.
    #[serde(default)]
    pub think: Option<bool>,
    /// How long one screening may take. A message is held and tried again after a
    /// timeout, and withheld after the third. Defaults to 120 s for Claude and 600 s
    /// for Ollama, which may first have to load the model.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Extra policy text appended to the built-in instructions.
    #[serde(default)]
    pub extra_policy: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Claude,
    Ollama,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: Provider::default(),
            api_key_env: default_api_key_env(),
            model: None,
            base_url: None,
            threads: None,
            think: None,
            timeout_secs: None,
            extra_policy: String::new(),
        }
    }
}

impl Filter {
    /// The model to ask, with the provider's default filled in.
    pub fn model(&self) -> Option<&str> {
        match (&self.model, self.provider) {
            (Some(m), _) => Some(m.as_str()),
            (None, Provider::Claude) => Some("claude-opus-5-5"),
            (None, Provider::Ollama) => None,
        }
    }

    /// The service address, without a trailing `/`.
    pub fn base_url(&self) -> String {
        let default = match self.provider {
            Provider::Claude => "https://api.anthropic.com",
            Provider::Ollama => "http://localhost:11434",
        };
        self.base_url
            .as_deref()
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_string()
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(match self.provider {
            Provider::Claude => 120,
            Provider::Ollama => 600,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(m) = &self.model {
            if m.trim().is_empty() {
                bail!("filter.model is empty");
            }
            if m.trim() != m {
                bail!("filter.model {m:?} has spaces around it");
            }
        }
        match (self.provider, self.model.as_deref()) {
            (Provider::Ollama, None) => bail!(
                "filter.model is required with provider = \"ollama\": the name of a model \
                 you have pulled, as `ollama list` shows it"
            ),
            (Provider::Ollama, Some(m)) if m.starts_with("claude-") => bail!(
                "filter.model {m:?} is a Claude model, but provider = \"ollama\": set it to \
                 a model you have pulled, as `ollama list` shows it"
            ),
            _ => {}
        }
        if let Some(url) = &self.base_url {
            check_base_url(url, self.provider)?;
        }
        if let Some(t) = self.threads {
            if self.provider != Provider::Ollama {
                bail!("filter.threads only applies with provider = \"ollama\"");
            }
            if !(1..=256).contains(&t) {
                bail!("filter.threads must be 1-256");
            }
        }
        if self.think.is_some() && self.provider != Provider::Ollama {
            bail!("filter.think only applies with provider = \"ollama\"");
        }
        if self.timeout_secs.is_some_and(|t| !(1..=3600).contains(&t)) {
            bail!("filter.timeout_secs must be 1-3600");
        }
        if self.provider == Provider::Ollama {
            let room = crate::gateway::filter::local_room(&self.extra_policy);
            let min = crate::gateway::filter::LOCAL_MIN_ROOM;
            if room < min {
                bail!(
                    "filter.extra_policy is too long for a local model: shorten it by at \
                     least {} characters so a whole RX still fits its context window",
                    min - room
                );
            }
        }
        Ok(())
    }
}

/// A service address is a scheme, a host and an optional port, and nothing more.
/// The API key goes to Claude's, so it must be encrypted unless it stays on this
/// machine.
fn check_base_url(url: &str, provider: Provider) -> Result<()> {
    let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        _ => bail!("filter.base_url {url:?} must start with http:// or https://"),
    };
    let host = rest.strip_suffix('/').unwrap_or(rest);
    if host.is_empty() || host.contains(['/', '?', '#', '@', ' ']) {
        bail!(
            "filter.base_url {url:?} must be only the server's address, \
             like http://192.168.1.20:11434"
        );
    }
    if provider == Provider::Claude && !https && !is_loopback(host) {
        bail!("filter.base_url {url:?} would send the API key unencrypted: use https://");
    }
    Ok(())
}

/// Whether `host[:port]` names this machine.
fn is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn yes() -> bool {
    true
}
fn default_storm_lookahead() -> u32 {
    2
}
fn default_storm_check() -> u64 {
    5
}
fn default_storm_clear() -> u64 {
    30
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
/// The IC-7300's USB codec calls itself "USB Audio CODEC"; ALSA names the card CODEC.
fn default_audio_device() -> String {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        "USB Audio CODEC".into()
    } else {
        "plughw:CARD=CODEC,DEV=0".into()
    }
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
fn default_check() -> u32 {
    10
}
fn default_retune() -> u32 {
    60
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
fn default_text_tag() -> String {
    "{call} via HF radio, replies are read on air:".into()
}
fn default_chat_db() -> PathBuf {
    PathBuf::from("~/Library/Messages/chat.db")
}
fn default_im_poll() -> u64 {
    60
}
fn default_reply_hours() -> u64 {
    48
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

/// A path starting with `~` starts in the user's home directory, as in a shell: the
/// node may be started by launchd or Task Scheduler, where no shell expands it.
pub fn expand_home(path: &Path) -> Result<PathBuf> {
    expand_home_from(path, home_dir())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn expand_home_from(path: &Path, home: Option<PathBuf>) -> Result<PathBuf> {
    let mut parts = path.components();
    match parts.next() {
        Some(std::path::Component::Normal(first)) if first == "~" => match home {
            Some(h) => Ok(h.join(parts.as_path())),
            None => bail!(
                "{} starts with ~ but the home directory is not known here; use a full path",
                path.display()
            ),
        },
        _ => Ok(path.to_path_buf()),
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.expand_paths(home_dir())?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Expand `~` in the paths the node opens.
    fn expand_paths(&mut self, home: Option<PathBuf>) -> Result<()> {
        self.state_dir = expand_home_from(&self.state_dir, home.clone()).context("state_dir")?;
        self.auth.key_file =
            expand_home_from(&self.auth.key_file, home.clone()).context("auth.key_file")?;
        if let Some(im) = &mut self.imessage {
            im.db = expand_home_from(&im.db, home).context("imessage.db")?;
        }
        Ok(())
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
            let n = &c.name;
            if c.address.is_none() && c.phone.is_none() && c.imessage.is_empty() {
                bail!("contact {n} needs at least one of address, phone or imessage");
            }
            if let Some(a) = &c.address {
                if !is_bare_address(a) {
                    bail!("contact {n} address {a:?} is not an email address");
                }
                if domain_of(a).eq_ignore_ascii_case(GOOGLE_VOICE_DOMAIN) {
                    bail!(
                        "contact {n} address {a:?} is a Google Voice reply address: give the \
                         contact's own number as phone instead"
                    );
                }
            }
            if let Some(p) = &c.phone {
                if !p.is_nanp() {
                    bail!(
                        "contact {n} phone {p}: Google Voice texts US and Canadian numbers only \
                         (+1 and 10 digits); reach this contact with address or imessage instead"
                    );
                }
            }
        }
        // A phone number, handle or address identifies one contact, or replies could be
        // read out as from the wrong person.
        let keys: Vec<Vec<String>> = self.contacts.iter().map(Contact::keys).collect();
        for (i, c) in self.contacts.iter().enumerate() {
            for (j, o) in self.contacts[..i].iter().enumerate() {
                if let Some(k) = keys[i].iter().find(|k| keys[j].contains(k)) {
                    bail!(
                        "contacts {} and {} both use {k}: a phone number, iMessage handle or \
                         address can belong to one contact only",
                        o.name,
                        c.name
                    );
                }
            }
        }
        if let Some(gv) = &self.google_voice {
            if self.email.is_none() {
                bail!(
                    "[google_voice] needs [email]: Google Voice texts arrive in, and are \
                     answered from, the Gmail account that has the Google Voice number; add \
                     [email] for that account"
                );
            }
            if !gv.number.is_nanp() {
                bail!(
                    "google_voice.number {}: a Google Voice number is +1 and 10 digits, e.g. \
                     \"+1 555 000 1111\"",
                    gv.number
                );
            }
            if let Some(c) = self
                .contacts
                .iter()
                .find(|c| c.phone.as_ref() == Some(&gv.number))
            {
                bail!(
                    "contact {} phone is the node's own Google Voice number",
                    c.name
                );
            }
            check_tag("google_voice", &gv.tag)?;
        }
        if let Some(im) = &self.imessage {
            if !cfg!(target_os = "macos") {
                bail!(
                    "[imessage] works only on a Mac: Messages and its database exist only on \
                     macOS; remove [imessage] from this config"
                );
            }
            if !(15..=3600).contains(&im.poll_secs) {
                bail!("imessage.poll_secs must be 15-3600");
            }
            if !(1..=336).contains(&im.reply_hours) {
                bail!("imessage.reply_hours must be 1-336");
            }
            let home_relative = matches!(
                im.db.components().next(),
                Some(std::path::Component::Normal(first)) if first == "~"
            );
            if !im.db.is_absolute() && !home_relative {
                bail!(
                    "imessage.db {}: use a full path or one starting with ~",
                    im.db.display()
                );
            }
            check_tag("imessage", &im.tag)?;
        }
        if let Some(to) = self.email.as_ref().and_then(|e| e.alert_to.as_deref()) {
            // A bare address, as for contacts.
            let bare = to.parse::<lettre::Address>().is_ok()
                && to
                    .parse::<lettre::message::Mailbox>()
                    .is_ok_and(|m| m.name.is_none());
            if !bare {
                bail!("email.alert_to {to:?} is not an email address");
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
        if let Some(st) = self.storm.as_ref().filter(|st| st.enabled) {
            match (st.latitude, st.longitude) {
                (Some(lat), Some(lon))
                    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => {}
                (Some(_), Some(_)) => bail!(
                    "storm.latitude must be -90 to 90 and storm.longitude -180 to 180 \
                     (decimal degrees)"
                ),
                _ => bail!(
                    "storm.latitude and storm.longitude (the station's location) are \
                     required, or set storm.enabled = false"
                ),
            }
            if self.storm_user_agent().is_none() {
                bail!(
                    "storm.user_agent (or weather.user_agent) is required; api.weather.gov \
                     refuses requests without one"
                );
            }
            if !(1..=12).contains(&st.lookahead_hours) {
                bail!("storm.lookahead_hours must be 1-12");
            }
            if !(1..=30).contains(&st.check_minutes) {
                bail!("storm.check_minutes must be 1-30");
            }
            if st.clear_minutes > 240 {
                bail!("storm.clear_minutes must be 0-240");
            }
        }
        self.filter.validate()?;
        let sch = &self.schedule;
        if sch.window_minutes == 0 {
            bail!("schedule.window_minutes must be at least 1");
        }
        if sch.window_minutes > sch.every_minutes {
            bail!("schedule.window_minutes is longer than schedule.every_minutes");
        }
        if sch.offset_minutes >= sch.every_minutes {
            bail!("schedule.offset_minutes must be less than schedule.every_minutes");
        }
        if !(1..=1440).contains(&sch.check_minutes) {
            bail!("schedule.check_minutes must be 1-1440");
        }
        // Not below 10: each tune is a carrier of a few seconds that does not
        // identify the station.
        if !(10..=1440).contains(&sch.retune_minutes) {
            bail!("schedule.retune_minutes must be 10-1440");
        }
        // Bad audio settings would leave the decoder silently deaf.
        self.decoder_config()
            .validate()
            .map_err(|e| anyhow::anyhow!("audio.{e}"))?;
        Ok(())
    }

    /// The User-Agent for the storm check: `storm.user_agent`, else
    /// `weather.user_agent`.
    pub fn storm_user_agent(&self) -> Option<&str> {
        fn usable(ua: &str) -> Option<&str> {
            Some(ua.trim()).filter(|ua| !ua.is_empty())
        }
        self.storm
            .as_ref()
            .and_then(|st| st.user_agent.as_deref())
            .and_then(usable)
            .or_else(|| {
                self.weather
                    .as_ref()
                    .and_then(|w| usable(w.user_agent.as_str()))
            })
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

    /// Settings that work but are probably not what was meant. Logged when the node
    /// starts and printed by `hfnode messages check`.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        for c in &self.contacts {
            if let (Some(a), Some(_)) = (&c.address, &c.phone) {
                if crate::gateway::email::is_carrier_address(a) {
                    w.push(format!(
                        "{}: {a} is a carrier email-to-SMS gateway and is never used for TX \
                         while phone is set",
                        c.name
                    ));
                }
            }
            if !c.imessage.is_empty() && c.phone.is_none() && self.google_voice.is_some() {
                w.push(format!(
                    "{} can reply only within reply_hours of an iMessage TX; add phone so they \
                     can text the node's number first",
                    c.name
                ));
            }
        }
        if let Some(e) = &self.email {
            if self.google_voice.is_some() && e.authserv_id.is_none() {
                w.push(
                    "set email.authserv_id = \"mx.google.com\" so only Gmail's own \
                     Authentication-Results header is believed"
                        .into(),
                );
            }
            if e.imap_port != 993 {
                w.push(format!(
                    "IMAP on port {} uses STARTTLS without read timeouts; a stalled server can \
                     stop mail checks until the node restarts",
                    e.imap_port
                ));
            }
        }
        w
    }
}

impl Contact {
    /// What identifies this contact: its phone, iMessage handles and address (and
    /// the number in a carrier email-to-SMS address).
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.phone.iter().map(|p| p.to_string()).collect();
        keys.extend(self.imessage.iter().map(|h| h.to_string()));
        if let Some(a) = &self.address {
            let a = a.trim().to_ascii_lowercase();
            if let Some(n) = crate::gateway::email::carrier_number(&a) {
                keys.push(format!("+1{n}"));
            }
            keys.push(a);
        }
        keys
    }
}

fn domain_of(address: &str) -> &str {
    address.rsplit_once('@').map_or("", |(_, d)| d.trim())
}

/// A text tag names the field callsign and stays short, since it is sent with every
/// message.
fn check_tag(section: &str, tag: &str) -> Result<()> {
    if !tag.contains("{call}") || tag.chars().any(char::is_control) || tag.chars().count() > 60 {
        bail!("{section}.tag must contain {{call}}, be one line and at most 60 characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_means_home() {
        let home = Some(PathBuf::from("/home/op"));
        let ex = |p: &str| expand_home_from(Path::new(p), home.clone()).unwrap();
        assert_eq!(
            ex("~/hfnode/state"),
            Path::new("/home/op").join("hfnode/state")
        );
        assert_eq!(ex("~"), Path::new("/home/op"));
        assert_eq!(ex("/var/lib/hfnode"), Path::new("/var/lib/hfnode"));
        assert_eq!(ex("state/~x"), Path::new("state/~x"));
        assert_eq!(ex("~op/x"), Path::new("~op/x"));
        #[cfg(windows)]
        assert_eq!(
            ex(r"~\AppData\Local\hfnode"),
            Path::new("/home/op").join(r"AppData\Local\hfnode")
        );
        assert!(expand_home_from(Path::new("~/x"), None).is_err());
        assert_eq!(
            expand_home_from(Path::new("/abs"), None).unwrap(),
            Path::new("/abs")
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../hfnode.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.station.civ_address, 0x94);
        assert_eq!(
            cfg.email.unwrap().alert_to.as_deref(),
            Some("you@example.com")
        );
        // The example leaves the audio device to the per-system default, so the same
        // file works on Linux, macOS and Windows.
        assert_eq!(cfg.audio.device, default_audio_device());
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
        cfg.contacts[1].address = Some("bob at example.com".into());
        assert!(cfg.validate().is_err());
        let mut cfg = example();
        cfg.contacts[1].address = Some("Bob <bob@example.com>".into());
        assert!(cfg.validate().is_err());
        // Taken as an Address, but the mailer cannot send to them.
        for address in ["bob@[127.0.0.1]", "\"a b\"@example.com"] {
            assert!(address.parse::<lettre::message::Mailbox>().is_err());
            let mut cfg = example();
            cfg.contacts[1].address = Some(address.into());
            assert!(cfg.validate().is_err(), "{address}");
        }
        let mut cfg = example();
        cfg.contacts[1].name = cfg.contacts[0].name.clone();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn canonical_phones() {
        for s in [
            "(555) 123-4567",
            "555-123-4567",
            "15551234567",
            "+1 555 123 4567",
            "555.123.4567",
        ] {
            assert_eq!(canonical_phone(s).as_deref(), Some("+15551234567"), "{s}");
        }
        assert_eq!(
            canonical_phone("+44 20 7946 0000").as_deref(),
            Some("+442079460000")
        );
        for s in [
            "5551234",
            "+1234567",
            "555123456",
            "255512345678",
            "55512345a7",
            "",
            "+",
            "555/123/4567",
        ] {
            assert_eq!(canonical_phone(s), None, "{s}");
        }
        assert_eq!(
            handle_key("Mom@iCloud.com").as_deref(),
            Some("mom@icloud.com")
        );
        assert_eq!(handle_key("+15551234567").as_deref(), Some("+15551234567"));
        assert_eq!(handle_key("chat123456"), None);
    }

    /// The example with `extra` added to its contacts' section.
    fn with_contacts(contacts: &str) -> Result<Config> {
        let text = include_str!("../../../hfnode.example.toml");
        let start = text.find("[[contacts]]").unwrap();
        let end = text.find("[weather]").unwrap();
        let text = format!("{}{contacts}\n{}", &text[..start], &text[end..]);
        let cfg: Config = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn contact_routes() {
        let cfg = with_contacts(
            "[[contacts]]\nname = \"MOM\"\nphone = \"(555) 123-4567\"\nimessage = \"Mom@iCloud.com\"\n\
             [[contacts]]\nname = \"DAD\"\nimessage = [\"+1 555 222 3333\", \"dad@example.com\"]\naddress = \"dad@work.example\"\n",
        )
        .unwrap();
        let mom = &cfg.contacts[0];
        assert_eq!(mom.phone.as_ref().unwrap().as_str(), "+15551234567");
        assert_eq!(mom.imessage, [Handle::Email("mom@icloud.com".into())]);
        assert_eq!(mom.address, None);
        assert_eq!(
            cfg.contacts[1].imessage,
            [
                Handle::Phone(Phone::parse("5552223333").unwrap()),
                Handle::Email("dad@example.com".into())
            ]
        );
        // Not handles.
        for bad in ["-x@example.com", "5551234", "Mom <m@x.com>", "-15551234567"] {
            let e = with_contacts(&format!(
                "[[contacts]]\nname = \"MOM\"\nimessage = \"{bad}\"\n"
            ))
            .unwrap_err();
            assert!(
                format!("{e:#}").contains("is not an iMessage handle"),
                "{bad}: {e:#}"
            );
        }
        let e = with_contacts("[[contacts]]\nname = \"MOM\"\nphone = \"555-1234\"\n").unwrap_err();
        assert!(format!("{e:#}").contains("with its country code"), "{e:#}");
    }

    #[test]
    fn rejects_contacts_that_cannot_work() {
        let refused = |contacts: &str, says: &str| {
            let e = with_contacts(contacts).unwrap_err();
            assert!(format!("{e:#}").contains(says), "{contacts}: {e:#}");
        };
        refused(
            "[[contacts]]\nname = \"MOM\"\n",
            "needs at least one of address, phone or imessage",
        );
        refused(
            "[[contacts]]\nname = \"MOM\"\nphone = \"+44 20 7946 0000\"\n",
            "US and Canadian numbers only",
        );
        refused(
            "[[contacts]]\nname = \"MOM\"\naddress = \"15550001111.15551234567.tok@txt.voice.google.com\"\n",
            "is a Google Voice reply address",
        );
        refused(
            "[[contacts]]\nname = \"MOM\"\nphone = \"+1 555 000 1111\"\n",
            "the node's own Google Voice number",
        );
        // One phone number, handle or address, one contact.
        for (a, b) in [
            ("phone = \"5551234567\"", "phone = \"+15551234567\""),
            ("imessage = \"m@x.com\"", "imessage = \"M@X.com\""),
            ("address = \"m@x.com\"", "imessage = \"m@x.com\""),
            ("address = \"m@x.com\"", "address = \"M@x.com\""),
            (
                "phone = \"5551234567\"",
                "address = \"5551234567@vtext.com\"",
            ),
            ("imessage = \"+15551234567\"", "phone = \"555 123 4567\""),
        ] {
            refused(
                &format!("[[contacts]]\nname = \"MOM\"\n{a}\n[[contacts]]\nname = \"DAD\"\n{b}\n"),
                "contacts MOM and DAD both use",
            );
        }
        // The same number twice in one contact is fine.
        with_contacts(
            "[[contacts]]\nname = \"MOM\"\nphone = \"5551234567\"\nimessage = \"+1 555 123 4567\"\n",
        )
        .unwrap();
    }

    #[test]
    fn google_voice_and_imessage_settings() {
        let base = include_str!("../../../hfnode.example.toml");
        let parse = |text: &str| -> Result<Config> {
            let cfg: Config = toml::from_str(text)?;
            cfg.validate()?;
            Ok(cfg)
        };
        let cfg = parse(base).unwrap();
        let gv = cfg.google_voice.as_ref().unwrap();
        assert_eq!(gv.number.as_str(), "+15550001111");
        assert_eq!(gv.tag, "{call} via HF radio, replies are read on air:");
        // Google Voice is answered through [email].
        let start = base.find("\n[email]").unwrap();
        let end = base.find("\n[google_voice]").unwrap();
        let no_email = format!("{}{}", &base[..start], &base[end..]);
        let e = parse(&no_email).unwrap_err();
        assert!(
            e.to_string().contains("[google_voice] needs [email]"),
            "{e}"
        );
        for (from, to) in [
            ("number = \"+1 555 000 1111\"", "number = \"+44 20 7946 0000\""),
            (
                "# tag = \"{call} via HF radio, replies are read on air:\"   # starts every text",
                "tag = \"via HF radio:\"",
            ),
            (
                "# tag = \"{call} via HF radio, replies are read on air:\"   # starts every text",
                "tag = \"{call} sent this over HF radio, and replies to it are read out on the air:\"",
            ),
        ] {
            let text = base.replace(from, to);
            assert_ne!(text, base);
            assert!(parse(&text).is_err(), "{to}");
        }
        // [imessage] only on a Mac, and with sane settings there.
        let im = base.replace("# [imessage]", "[imessage]");
        assert_eq!(parse(&im).is_ok(), cfg!(target_os = "macos"));
        if cfg!(target_os = "macos") {
            for (from, to) in [
                ("# poll_secs = 60", "poll_secs = 14"),
                ("# reply_hours = 48 ", "reply_hours = 0 "),
                ("# db = \"~/Library/Messages/chat.db\"", "db = \"chat.db\""),
            ] {
                let text = im.replace(from, to);
                assert_ne!(text, im);
                assert!(parse(&text).is_err(), "{to}");
            }
        }
        let im: Config = toml::from_str(&im).unwrap();
        let w = im.imessage.unwrap();
        assert_eq!((w.poll_secs, w.reply_hours), (60, 48));
    }

    #[test]
    fn paths_are_expanded() {
        let mut cfg = example();
        cfg.state_dir = "~/hfnode/state".into();
        cfg.auth.key_file = "~/hfnode/node.key".into();
        cfg.imessage = Some(toml::from_str("").unwrap());
        cfg.expand_paths(Some(PathBuf::from("/home/op"))).unwrap();
        assert_eq!(cfg.state_dir, Path::new("/home/op").join("hfnode/state"));
        assert_eq!(
            cfg.auth.key_file,
            Path::new("/home/op").join("hfnode/node.key")
        );
        assert_eq!(
            cfg.imessage.unwrap().db,
            Path::new("/home/op").join("Library/Messages/chat.db")
        );
    }

    #[test]
    fn warnings_for_settings_that_work_badly() {
        let cfg = with_contacts(
            "[[contacts]]\nname = \"MOM\"\nphone = \"5551234567\"\naddress = \"5551234567@vtext.com\"\n\
             [[contacts]]\nname = \"DAD\"\nimessage = \"dad@example.com\"\n",
        )
        .unwrap();
        let w = cfg.warnings().join("\n");
        assert!(w.contains("never used for TX while phone is set"), "{w}");
        assert!(w.contains("DAD can reply only within reply_hours"), "{w}");
        assert!(w.contains("email.authserv_id"), "{w}");
    }

    #[test]
    fn rejects_a_bad_alert_address() {
        for (to, ok) in [
            (None, true),
            (Some("robin@example.com"), true),
            (Some("5551234567@vtext.com"), true),
            (Some("robin at example.com"), false),
            (Some("Robin <robin@example.com>"), false),
            (Some(""), false),
        ] {
            let mut cfg = example();
            cfg.email.as_mut().unwrap().alert_to = to.map(String::from);
            assert_eq!(cfg.validate().is_ok(), ok, "{to:?}");
        }
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
    fn filter_providers() {
        let parse = |filter: &str| -> Result<Config> {
            let text = include_str!("../../../hfnode.example.toml")
                .split("[filter]")
                .next()
                .unwrap()
                .to_string()
                + filter;
            let cfg: Config = toml::from_str(&text)?;
            cfg.validate()?;
            Ok(cfg)
        };
        // Omitted entirely: Claude, with its defaults.
        let f = parse("").unwrap().filter;
        assert!(f.enabled);
        assert_eq!(f.provider, Provider::Claude);
        assert_eq!(f.model(), Some("claude-opus-5-5"));
        assert_eq!(f.base_url(), "https://api.anthropic.com");
        assert_eq!(f.timeout_secs(), 120);
        // Ollama: local by default, and the model must be named.
        let f = parse("[filter]\nprovider = \"ollama\"\nmodel = \"gemma3:4b\"\n")
            .unwrap()
            .filter;
        assert_eq!(f.provider, Provider::Ollama);
        assert_eq!(f.model(), Some("gemma3:4b"));
        assert_eq!(f.base_url(), "http://localhost:11434");
        assert_eq!(f.timeout_secs(), 600);
        let f = parse(
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"http://mac.local:11434/\"\nthreads = 2\ntimeout_secs = 900\n",
        )
        .unwrap()
        .filter;
        assert_eq!(f.base_url(), "http://mac.local:11434");
        assert_eq!((f.threads, f.timeout_secs()), (Some(2), 900));
        assert_eq!(f.think, None);
        let f = parse("[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nthink = false\n")
            .unwrap()
            .filter;
        assert_eq!(f.think, Some(false));
        for bad in [
            "[filter]\nprovider = \"ollama\"\n",
            "[filter]\nprovider = \"ollama\"\nmodel = \" \"\n",
            "[filter]\nprovider = \"openai\"\n",
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"localhost:11434\"\n",
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"http://\"\n",
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nthreads = 0\n",
            "[filter]\nthreads = 2\n",
            "[filter]\nthink = false\n",
            "[filter]\ntimeout_secs = 0\n",
            "[filter]\nmodel = \"\"\n",
            "[filter]\nmodel = \" claude-opus-5-5\"\n",
            // A Claude model left in place when switching to Ollama.
            "[filter]\nprovider = \"ollama\"\nmodel = \"claude-opus-5-5\"\n",
            // Only the server's address.
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"http://mac.local:11434/api\"\n",
            "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"http://mac.local:11434?x=1\"\n",
            // The API key is never sent unencrypted off this machine.
            "[filter]\nbase_url = \"http://api.anthropic.com\"\n",
            "[filter]\nbase_url = \"http://192.168.1.20:8080\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1:9/",
            "http://[::1]:9",
            "https://proxy.example.com",
        ] {
            let f = parse(&format!("[filter]\nbase_url = \"{url}\"\n"))
                .unwrap()
                .filter;
            assert_eq!(f.base_url(), url.trim_end_matches('/'));
        }
        // Switching the example to Ollama names what is missing.
        let example = include_str!("../../../hfnode.example.toml");
        let switched = example.replace("provider = \"claude\"", "provider = \"ollama\"");
        assert_ne!(switched, example);
        let e = toml::from_str::<Config>(&switched)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(e.to_string().contains("ollama list"), "{e}");
        // Extra policy must leave a local model room for a whole RX.
        let room = crate::gateway::filter::local_room("");
        let min = crate::gateway::filter::LOCAL_MIN_ROOM;
        let policy = |n: usize| format!("extra_policy = \"{}\"\n", "X".repeat(n));
        let ollama = "[filter]\nprovider = \"ollama\"\nmodel = \"m\"\n";
        let fits = room - min - "\n\nAdditional station policy:\n".len();
        assert!(parse(&format!("{ollama}{}", policy(fits))).is_ok());
        let e = parse(&format!("{ollama}{}", policy(fits + 1))).unwrap_err();
        assert!(e.to_string().contains("by at least 1 characters"), "{e}");
        assert!(parse(&format!("[filter]\n{}", policy(fits + 1))).is_ok());
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
    fn listens_all_the_time_by_default() {
        let mut cfg = example();
        cfg.schedule = toml::from_str("").unwrap();
        cfg.validate().unwrap();
        let s = &cfg.schedule;
        assert!(s.always);
        assert!((0..3 * 86_400).step_by(37).all(|t| s.is_open(t)));
        assert_eq!((s.check_minutes, s.retune_minutes), (10, 60));
    }

    #[test]
    fn rejects_bad_schedule_intervals() {
        for (check, retune, ok) in [
            (10, 60, true),
            (1, 10, true),
            (1440, 1440, true),
            (0, 60, false),
            (1441, 60, false),
            (10, 9, false),
            (10, 1441, false),
        ] {
            let mut cfg = example();
            cfg.schedule.check_minutes = check;
            cfg.schedule.retune_minutes = retune;
            assert_eq!(cfg.validate().is_ok(), ok, "{check} {retune}");
        }
        let mut cfg = example();
        cfg.schedule.window_minutes = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn schedule_windows() {
        let s = Schedule {
            always: false,
            every_minutes: 60,
            offset_minutes: 0,
            window_minutes: 10,
            ..Schedule::default()
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

    #[test]
    fn storm_needs_the_station_location_and_a_contact() {
        let base = example();
        let st = base
            .storm
            .clone()
            .expect("the example has a [storm] section");
        assert!(st.enabled);
        base.validate().unwrap();
        let with = |f: &dyn Fn(&mut Storm)| {
            let mut cfg = base.clone();
            f(cfg.storm.as_mut().unwrap());
            cfg
        };
        for (lat, lon, ok) in [
            (Some(29.25), Some(-103.25), true),
            (Some(-90.0), Some(180.0), true),
            (None, Some(-103.25), false),
            (Some(29.25), None, false),
            (Some(91.0), Some(-103.25), false),
            (Some(29.25), Some(-181.0), false),
            (Some(f64::NAN), Some(-103.25), false),
        ] {
            let cfg = with(&|s| {
                s.latitude = lat;
                s.longitude = lon;
            });
            assert_eq!(cfg.validate().is_ok(), ok, "{lat:?} {lon:?}");
        }
        // Off, the location is not needed.
        let cfg = with(&|s| {
            s.enabled = false;
            s.latitude = None;
        });
        cfg.validate().unwrap();
        // The User-Agent falls back to [weather]; with neither, it is refused.
        let mut cfg = with(&|s| s.user_agent = None);
        assert_eq!(cfg.storm_user_agent(), Some("hfnode (you@example.com)"));
        let blank = with(&|s| s.user_agent = Some("  ".into()));
        assert_eq!(blank.storm_user_agent(), Some("hfnode (you@example.com)"));
        cfg.weather = None;
        assert!(cfg.validate().is_err());
        cfg.storm.as_mut().unwrap().user_agent = Some("me@example.com".into());
        assert_eq!(cfg.storm_user_agent(), Some("me@example.com"));
        cfg.validate().unwrap();
        for (f, field) in [
            (
                &(|s: &mut Storm| s.lookahead_hours = 0) as &dyn Fn(&mut Storm),
                "lookahead",
            ),
            (&|s: &mut Storm| s.lookahead_hours = 13, "lookahead"),
            (&|s: &mut Storm| s.check_minutes = 0, "check"),
            (&|s: &mut Storm| s.check_minutes = 31, "check"),
            (&|s: &mut Storm| s.clear_minutes = 241, "clear"),
        ] {
            assert!(with(f).validate().is_err(), "{field}");
        }
    }
}
