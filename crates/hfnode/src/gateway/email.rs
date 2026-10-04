//! Outbound mail over SMTP and inbound replies over IMAP.
//!
//! Texts travel as email too: Google Voice forwards texts to the node's number to
//! the mailbox and texts back the replies the node emails (see
//! [`super::google_voice`]), and where a carrier still runs an email-to-SMS gateway
//! a phone can be reached through it.

use super::google_voice::{self, parse_gv_from};
use crate::config::{Contact, Email, Phone, GOOGLE_VOICE_DOMAIN};
use crate::inbox::Inbox;
use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use mail_parser::{HeaderName, MessageParser};
use rustls_connector::{rustls, rustls_native_certs, RustlsConnector};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn password(cfg: &Email) -> Result<String> {
    std::env::var(&cfg.password_env)
        .with_context(|| format!("environment variable {} is not set", cfg.password_env))
}

pub struct Mailer {
    transport: SmtpTransport,
    from: Mailbox,
    field_call: String,
}

impl Mailer {
    pub fn new(cfg: &Email, field_call: &str) -> Result<Self> {
        let creds = Credentials::new(cfg.username.clone(), password(cfg)?);
        // Port 465 is implicit TLS; anything else uses STARTTLS (normally 587).
        let builder = if cfg.smtp_port == 465 {
            SmtpTransport::relay(&cfg.smtp_host)?
        } else {
            SmtpTransport::starttls_relay(&cfg.smtp_host)?
        };
        Ok(Self {
            transport: builder.port(cfg.smtp_port).credentials(creds).build(),
            from: cfg.from_address.parse().context("email.from_address")?,
            field_call: field_call.to_string(),
        })
    }

    /// Email `text` as from the first field callsign.
    pub fn send(&self, to: &str, text: &str) -> Result<()> {
        self.send_as(to, text, &self.field_call)
    }

    /// Email `text` as from field callsign `call`, with a signature saying how it
    /// was sent.
    pub fn send_as(&self, to: &str, text: &str, call: &str) -> Result<()> {
        let body = format!(
            "{text}\n\n-- \nSent by {call} over HF radio. Reply to this message; keep it short and plain."
        );
        let to: Mailbox = to.parse().with_context(|| format!("bad address {to}"))?;
        self.deliver(to.clone(), Some(format!("From {call}")), body)?;
        log::info!("sent message to {to}");
        Ok(())
    }

    /// Email `text` alone, with no subject and no signature: for Google Voice, which
    /// texts the body. Logs nothing and keeps the address out of its errors, since a
    /// Google Voice reply address holds the conversation's token.
    pub fn send_text(&self, to: &str, text: &str) -> Result<()> {
        let to: Mailbox = to.parse().context("bad Google Voice reply address")?;
        self.deliver(to, None, text.to_string())
    }

    fn deliver(&self, to: Mailbox, subject: Option<String>, body: String) -> Result<()> {
        let mut msg = Message::builder().from(self.from.clone()).to(to);
        if let Some(s) = subject {
            msg = msg.subject(s);
        }
        let msg = msg.header(ContentType::TEXT_PLAIN).body(body)?;
        self.transport.send(&msg)?;
        Ok(())
    }
}

/// Carrier email-to-SMS/MMS gateway domains (US), one group per carrier network.
/// Replies from a phone come from one of its carrier's domains, often not the one
/// we send to (vtext.com vs vzwpix.com), so only for these is the sender matched by
/// phone number rather than full address, and only within one carrier's group.
pub const SMS_GATEWAYS: &[&[&str]] = &[
    // Verizon SMS (also Visible, Xfinity Mobile), MMS.
    &["vtext.com", "vzwpix.com", "mypixmessages.com"],
    // AT&T SMS, MMS.
    &["txt.att.net", "mms.att.net"],
    // T-Mobile, Sprint (now T-Mobile), Metro by T-Mobile.
    &[
        "tmomail.net",
        "messaging.sprintpcs.com",
        "pm.sprint.com",
        "mymetropcs.com",
    ],
    &["msg.fi.google.com"],                                  // Google Fi
    &["email.uscc.net", "mms.uscc.net"],                     // US Cellular SMS, MMS
    &["sms.cricketwireless.net", "mms.cricketwireless.net"], // Cricket SMS, MMS
    &["sms.myboostmobile.com", "myboostmobile.com"],         // Boost SMS, MMS
    &["mailmymobile.net"],                                   // Consumer Cellular
];

/// The carrier group in [`SMS_GATEWAYS`] that `domain` belongs to.
fn carrier(domain: &str) -> Option<&'static [&'static str]> {
    SMS_GATEWAYS.iter().copied().find(|g| g.contains(&domain))
}

/// Whether `address` is at a carrier email-to-SMS gateway.
pub fn is_carrier_address(address: &str) -> bool {
    let a = address.trim().to_ascii_lowercase();
    a.rsplit_once('@').is_some_and(|(_, d)| carrier(d).is_some())
}

/// The 10-digit number of a carrier email-to-SMS address (lowercase).
pub(crate) fn carrier_number(address: &str) -> Option<&str> {
    let (local, domain) = address.rsplit_once('@')?;
    carrier(domain)?;
    us_number(local)
}

/// A 10-digit US number from an address local part: exactly the number, optionally
/// with a leading `1` or `+1`.
pub(crate) fn us_number(local: &str) -> Option<&str> {
    let n = match local.len() {
        12 => local.strip_prefix("+1")?,
        11 => local.strip_prefix('1')?,
        _ => local,
    };
    (n.len() == 10 && n.bytes().all(|b| b.is_ascii_digit())).then_some(n)
}

/// Which contact sent this, if any.
///
/// - From a carrier gateway in [`SMS_GATEWAYS`] whose local part is exactly the
///   number (optional leading `1`) of a contact whose own address is at the same
///   carrier: that contact, unless our own mail server reports that the sender
///   failed SPF, DKIM or DMARC for the From domain (see [`failed`]). Carrier
///   gateways do not reliably sign their mail, so otherwise this match rests on the
///   From header alone, and a forged From such as `5551234567@vtext.com` gets
///   through to the filter. That is the price of SMS replies working at all; keep
///   the filter on.
/// - Anything else must equal a contact's address exactly and be vouched for by
///   our own mail server (see [`authenticated`]), so a forged From is refused.
pub fn contact_for<'c>(
    contacts: &'c [Contact],
    from_addr: &str,
    auth_results: &[&str],
    authserv_id: Option<&str>,
) -> Option<&'c Contact> {
    let from = from_addr.trim().to_ascii_lowercase();
    let (fl, fd) = from.rsplit_once('@')?;
    if let (Some(group), Some(n)) = (carrier(fd), us_number(fl)) {
        let by_number = contacts.iter().find(|c| {
            let Some(addr) = &c.address else {
                return false;
            };
            let addr = addr.trim().to_ascii_lowercase();
            let Some((cl, cd)) = addr.rsplit_once('@') else {
                return false;
            };
            group.contains(&cd) && us_number(cl) == Some(n)
        });
        if by_number.is_some() {
            return by_number.filter(|_| !failed(auth_results, &from, authserv_id));
        }
    }
    contacts
        .iter()
        .find(|c| {
            c.address
                .as_deref()
                .is_some_and(|a| a.trim().eq_ignore_ascii_case(&from))
        })
        .filter(|_| authenticated(auth_results, &from, authserv_id))
}

/// Whether our own mail server vouches for the sender: a `dkim=pass` whose signing
/// domain, or an `spf=pass` whose envelope sender domain, is the From domain (or a
/// parent or subdomain of it, as DMARC's relaxed alignment allows).
///
/// Only one Authentication-Results header is believed, because any further down
/// may have been written by the sender. With `authserv_id` set, it is the first
/// header carrying that id (the one our server prepended). Without it, it is the
/// topmost header, which on ordinary delivery is the receiving server's own. Set
/// `email.authserv_id` if your provider stacks other headers above its own, and
/// always if it adds none, since then the topmost one is whatever the sender wrote.
///
/// This authenticates the domain, not the mailbox: another user of the contact's
/// own provider could still pass, if that provider lets users set any From.
pub fn authenticated(auth_results: &[&str], from_addr: &str, authserv_id: Option<&str>) -> bool {
    passes(auth_results, from_addr, authserv_id, &["dkim", "spf"])
}

/// [`authenticated`] by DKIM alone. Google Voice mail must be signed: an SPF pass
/// only says the mail left through Google's servers, which other Google users'
/// mail does too.
pub(crate) fn dkim_authenticated(
    auth_results: &[&str],
    from_addr: &str,
    authserv_id: Option<&str>,
) -> bool {
    passes(auth_results, from_addr, authserv_id, &["dkim"])
}

fn passes(
    auth_results: &[&str],
    from_addr: &str,
    authserv_id: Option<&str>,
    methods: &[&str],
) -> bool {
    let Some((_, from_domain)) = from_addr.rsplit_once('@') else {
        return false;
    };
    let Some(header) = trusted_header(auth_results, authserv_id) else {
        return false;
    };
    header.results.iter().any(|r| {
        let signer = match r.result.as_str() {
            "pass" if methods.contains(&r.method.as_str()) => r.domain(),
            _ => None,
        };
        signer.is_some_and(|d| aligned(&d, from_domain))
    })
}

/// Whether our own mail server (the header [`authenticated`] believes) reports an
/// SPF, DKIM or DMARC `fail` for the From domain.
pub fn failed(auth_results: &[&str], from_addr: &str, authserv_id: Option<&str>) -> bool {
    let Some((_, from_domain)) = from_addr.rsplit_once('@') else {
        return false;
    };
    let Some(header) = trusted_header(auth_results, authserv_id) else {
        return false;
    };
    header.results.iter().any(|r| {
        matches!(r.method.as_str(), "spf" | "dkim" | "dmarc")
            && r.result == "fail"
            && r.domain().is_some_and(|d| aligned(&d, from_domain))
    })
}

/// The one Authentication-Results header that is believed: see [`authenticated`].
fn trusted_header(auth_results: &[&str], authserv_id: Option<&str>) -> Option<AuthResults> {
    match authserv_id {
        Some(id) => auth_results
            .iter()
            .filter_map(|h| parse_auth_results(h))
            .find(|h| h.authserv_id.eq_ignore_ascii_case(id.trim())),
        None => auth_results.first().and_then(|h| parse_auth_results(h)),
    }
}

/// Relaxed alignment: same domain, or one is a subdomain of the other.
fn aligned(d: &str, from: &str) -> bool {
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    let from = from.trim_end_matches('.').to_ascii_lowercase();
    d.contains('.')
        && (d == from || from.ends_with(&format!(".{d}")) || d.ends_with(&format!(".{from}")))
}

/// One Authentication-Results header (RFC 8601).
#[derive(Debug)]
struct AuthResults {
    authserv_id: String,
    results: Vec<AuthResult>,
}

#[derive(Debug)]
struct AuthResult {
    method: String,
    result: String,
    props: Vec<(String, String)>,
}

impl AuthResult {
    fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The domain this result is about: DKIM's signing domain, SPF's envelope
    /// sender domain, or DMARC's From domain.
    fn domain(&self) -> Option<String> {
        let domain = |v: &str| v.rsplit('@').next().unwrap_or("").to_string();
        match self.method.as_str() {
            "dkim" => self
                .prop("header.d")
                .map(str::to_string)
                .or_else(|| self.prop("header.i").map(domain)),
            "spf" => self.prop("smtp.mailfrom").map(domain),
            "dmarc" => self.prop("header.from").map(domain),
            _ => None,
        }
    }
}

/// Parse an Authentication-Results value: comments dropped, quoted strings kept
/// whole, `;` between results and whitespace between tokens.
fn parse_auth_results(value: &str) -> Option<AuthResults> {
    let mut segments: Vec<Vec<String>> = vec![vec![]];
    let mut tok = String::new();
    let (mut quoted, mut depth) = (false, 0u32);
    let mut chars = value.chars();
    fn end(tok: &mut String, segments: &mut [Vec<String>]) {
        if !tok.is_empty() {
            segments.last_mut().unwrap().push(std::mem::take(tok));
        }
    }
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '\\' => tok.extend(chars.next()),
                '"' => quoted = false,
                _ => tok.push(c),
            }
        } else if depth > 0 {
            match c {
                '\\' => {
                    chars.next();
                }
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        } else {
            match c {
                '"' => quoted = true,
                '(' => {
                    end(&mut tok, &mut segments);
                    depth = 1;
                }
                ';' => {
                    end(&mut tok, &mut segments);
                    segments.push(vec![]);
                }
                // `a = b` is the same as `a=b`.
                '=' => {
                    if tok.is_empty() {
                        tok = segments.last_mut().unwrap().pop().unwrap_or_default();
                    }
                    tok.push('=');
                }
                c if c.is_whitespace() => {
                    if !tok.ends_with('=') {
                        end(&mut tok, &mut segments);
                    }
                }
                _ => tok.push(c),
            }
        }
    }
    end(&mut tok, &mut segments);
    // Some servers (Microsoft) leave out the authserv-id and start with a result.
    let authserv_id = match segments[0].first() {
        Some(t) if t.contains('=') => String::new(),
        Some(_) => segments[0].remove(0),
        None => return None,
    };
    let results = segments
        .into_iter()
        .filter_map(|s| {
            let mut s = s.into_iter();
            let first = s.next()?;
            let (method, result) = first.split_once('=')?;
            let method = method.split('/').next().unwrap_or("").to_ascii_lowercase();
            let props = s
                .filter_map(|p| {
                    let (k, v) = p.split_once('=')?;
                    k.contains('.')
                        .then(|| (k.to_ascii_lowercase(), v.to_ascii_lowercase()))
                })
                .collect();
            Some(AuthResult {
                method,
                result: result.to_ascii_lowercase(),
                props,
            })
        })
        .collect();
    Some(AuthResults {
        authserv_id,
        results,
    })
}

/// Keep only what the sender wrote: drop quoted text, signatures and footers.
pub fn strip_reply(body: &str) -> String {
    let mut out = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        if t.starts_with('>')
            || (lower.starts_with("on ") && lower.ends_with("wrote:"))
            || lower.starts_with("-----original message")
            || lower.starts_with("from:")
            || t == "--"
            || lower.starts_with("sent from my")
            || lower.starts_with("get outlook for")
        {
            break;
        }
        out.push(t);
    }
    out.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// IMAP keyword set on mail the node did not take (not from a contact, no text),
/// so later polls skip it without marking it read in the mailbox. Whether mail is
/// from a contact depends on the configuration, so the keyword is cleared again
/// when the node starts (see [`poll_imap`]).
pub const IGNORED_KEYWORD: &str = "$HfnodeIgnored";

/// UIDs as IMAP sequence sets of at most 200 each, to keep command lines short.
fn uid_sets(uids: &[u32]) -> Vec<String> {
    uids.chunks(200)
        .map(|c| c.iter().map(u32::to_string).collect::<Vec<_>>().join(","))
        .collect()
}

/// How a message reached the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Email,
    /// A carrier email-to-SMS gateway.
    Carrier,
    GoogleVoice,
}

/// A message the node takes: who it is from, its de-duplication id and its text.
#[derive(Debug)]
pub struct Accepted<'c> {
    pub contact: &'c Contact,
    pub source_id: String,
    pub text: String,
    pub via: Via,
}

/// A message the node does not take.
#[derive(Debug)]
pub struct Rejected<'c> {
    pub why: String,
    /// Worth a warning: probably a setup problem or a changed format, not just
    /// someone else's mail.
    pub warn: bool,
    /// A contact who sent something the node could not read: they get a notice in
    /// the inbox in place of their text.
    pub notice: Option<&'c Contact>,
    pub source_id: Option<String>,
}

impl<'c> Rejected<'c> {
    fn info(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            warn: false,
            notice: None,
            source_id: None,
        }
    }

    fn warn(why: impl Into<String>) -> Self {
        Self {
            warn: true,
            ..Self::info(why)
        }
    }
}

/// Decide whether one fetched message goes in the inbox. `Err` says why not.
pub fn accept<'c>(
    raw: &[u8],
    uid: u32,
    contacts: &'c [Contact],
    authserv_id: Option<&str>,
) -> Result<Accepted<'c>, String> {
    accept_with(raw, uid, contacts, authserv_id, None).map_err(|r| r.why)
}

/// [`accept`], with Google Voice texts to the number `gv` taken too.
///
/// Mail from Google Voice's domain is only ever judged as a Google Voice text: the
/// sender's number must be a contact's `phone`, our own mail server must report a
/// DKIM pass for it, and the body must be recognised as Google Voice's; anything
/// else is refused. A contact's text that cannot be read gets a notice.
pub fn accept_with<'c>(
    raw: &[u8],
    uid: u32,
    contacts: &'c [Contact],
    authserv_id: Option<&str>,
    gv: Option<&Phone>,
) -> Result<Accepted<'c>, Rejected<'c>> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Rejected::info("unparseable message"))?;
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .unwrap_or("")
        .trim()
        .to_string();
    // In header order, topmost first.
    let auth_results: Vec<&str> = parsed
        .headers()
        .iter()
        .filter(|h| h.name == HeaderName::AuthenticationResults)
        .filter_map(|h| {
            let raw = parsed
                .raw_message
                .get(h.offset_start as usize..h.offset_end as usize)?;
            std::str::from_utf8(raw).ok()
        })
        .collect();
    let source_id = parsed
        .message_id()
        .map(str::to_string)
        .unwrap_or_else(|| format!("uid:{uid}"));
    let domain = from
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default();

    if domain == GOOGLE_VOICE_DOMAIN {
        let gv = gv.ok_or_else(|| {
            Rejected::warn("Google Voice mail, but [google_voice] is not configured")
        })?;
        let addr = parse_gv_from(&from)
            .ok_or_else(|| Rejected::warn("Google Voice sender address not recognized"))?;
        if addr.node != *gv {
            return Err(Rejected::warn("Google Voice mail for another number"));
        }
        // The number is not logged: it is not a contact's.
        let contact = contacts
            .iter()
            .find(|c| c.phone.as_ref() == Some(&addr.sender))
            .ok_or_else(|| {
                Rejected::info("Google Voice text from a number that is not a contact's phone")
            })?;
        let n = &contact.name;
        if !dkim_authenticated(&auth_results, &from, authserv_id) {
            return Err(Rejected::warn(format!(
                "Google Voice mail from {n} without a DKIM pass from our mail server"
            )));
        }
        let text = match google_voice::gv_text(&parsed) {
            Ok(body) => body.text,
            Err(e) => {
                return Err(Rejected {
                    why: format!("Google Voice mail from {n}: {e}"),
                    warn: true,
                    notice: Some(contact),
                    source_id: Some(source_id),
                })
            }
        };
        let text = reacted(contact, text)?;
        return Ok(Accepted {
            contact,
            source_id,
            text,
            via: Via::GoogleVoice,
        });
    }

    let contact = contact_for(contacts, &from, &auth_results, authserv_id).ok_or_else(|| {
        Rejected::info(format!("from {from:?}: not a contact, or not authenticated"))
    })?;
    let text = strip_reply(&parsed.body_text(0).unwrap_or_default());
    if text.is_empty() {
        return Err(Rejected::info(format!(
            "from {}: no text in the body",
            contact.name
        )));
    }
    let via = if carrier(&domain).is_some() {
        Via::Carrier
    } else {
        Via::Email
    };
    let text = match via {
        Via::Carrier => reacted(contact, text)?,
        _ => text,
    };
    Ok(Accepted {
        contact,
        source_id,
        text,
        via,
    })
}

/// A phone's reaction to the operator's text, shortened for the air; a removed
/// reaction is not read out at all. Anything else is returned unchanged.
fn reacted<'c>(contact: &'c Contact, text: String) -> Result<String, Rejected<'c>> {
    match sms_reaction(&text) {
        None => Ok(text),
        Some(Reaction::Said(said)) => {
            // The quoted words are the operator's own.
            log::info!("reaction from {}: {text}", contact.name);
            Ok(said.to_string())
        }
        Some(Reaction::Removed) => Err(Rejected::info(format!(
            "reaction removed by {}",
            contact.name
        ))),
    }
}

/// A reaction sent as a text by a phone that cannot send it as a reaction (an
/// iPhone tapback on an SMS conversation), e.g. `Liked “RUNNING LATE”`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    Said(&'static str),
    Removed,
}

/// Whether the whole of `text` is a reaction text. Only for texts (Google Voice and
/// carrier gateways): an email saying `Liked "the photos"` is just an email.
pub fn sms_reaction(text: &str) -> Option<Reaction> {
    const SAID: [(&str, &str); 6] = [
        ("Liked", "LIKED YOUR MSG"),
        ("Loved", "LOVED YOUR MSG"),
        ("Disliked", "DISLIKED YOUR MSG"),
        ("Laughed at", "LAUGHED AT YOUR MSG"),
        ("Emphasized", "EMPHASIZED YOUR MSG"),
        ("Questioned", "QUESTIONED YOUR MSG"),
    ];
    let text = text.trim();
    for (verb, said) in SAID {
        if let Some(rest) = text.strip_prefix(verb).and_then(|r| r.strip_prefix(' ')) {
            if is_quoted(rest) {
                return Some(Reaction::Said(said));
            }
        }
    }
    // "Reacted 😂 to “…”" and "Removed a like from “…”": anything in between.
    let ends_quoted = |rest: &str, word: &str| {
        rest.match_indices(word)
            .any(|(i, _)| i > 0 && is_quoted(&rest[i + word.len()..]))
    };
    if let Some(rest) = text.strip_prefix("Reacted ") {
        if ends_quoted(rest, " to ") {
            return Some(Reaction::Said("REACTED TO YOUR MSG"));
        }
    }
    let removed = text
        .strip_prefix("Removed a ")
        .or_else(|| text.strip_prefix("Removed an "));
    if removed.is_some_and(|rest| ends_quoted(rest, " from ")) {
        return Some(Reaction::Removed);
    }
    None
}

/// Whether `s` is a quotation: straight or curly quotes around at least one
/// character.
fn is_quoted(s: &str) -> bool {
    let inner = s
        .strip_prefix('"')
        .or_else(|| s.strip_prefix('\u{201C}'))
        .and_then(|r| r.strip_suffix('"').or_else(|| r.strip_suffix('\u{201D}')));
    inner.is_some_and(|i| !i.is_empty())
}

/// Read and write timeouts on the node's IMAP socket: a stalled server fails the
/// mail check instead of stopping it for good.
const IMAP_IO_TIMEOUT: Duration = Duration::from_secs(60);
const IMAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Log in to the node's mailbox.
pub(crate) fn connect_imap(cfg: &Email) -> Result<imap::Session<imap::Connection>> {
    let client = if cfg.imap_port == 993 {
        connect_tls(
            &cfg.imap_host,
            (cfg.imap_host.as_str(), cfg.imap_port),
            IMAP_CONNECT_TIMEOUT,
            IMAP_IO_TIMEOUT,
        )?
    } else {
        // STARTTLS: only the imap crate's own connection, which has no timeouts (the
        // node warns about this when it starts).
        imap::ClientBuilder::new(cfg.imap_host.as_str(), cfg.imap_port)
            .tls_kind(imap::TlsKind::Rust)
            .connect()
            .context("IMAP connect")?
    };
    client
        .login(&cfg.username, password(cfg)?)
        .map_err(|(e, _)| anyhow!("IMAP login: {e}"))
}

/// An IMAP connection over implicit TLS (port 993) whose socket times out.
pub(crate) fn connect_tls(
    host: &str,
    addrs: impl ToSocketAddrs,
    connect: Duration,
    io: Duration,
) -> Result<imap::Client<imap::Connection>> {
    let mut last = None;
    let mut tcp = None;
    for addr in addrs
        .to_socket_addrs()
        .with_context(|| format!("IMAP: looking up {host}"))?
    {
        match TcpStream::connect_timeout(&addr, connect) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let tcp = match (tcp, last) {
        (Some(tcp), _) => tcp,
        (None, Some(e)) => return Err(anyhow!(e).context(format!("IMAP connect to {host}"))),
        (None, None) => anyhow::bail!("IMAP: no address for {host}"),
    };
    tcp.set_read_timeout(Some(io))?;
    tcp.set_write_timeout(Some(io))?;
    let tls = tls_connector()
        .connect(host, tcp)
        .map_err(|e| anyhow!("IMAP TLS with {host}: {e}"))?;
    let mut client = imap::Client::new(Box::new(tls) as imap::Connection);
    client.read_greeting().context("IMAP greeting")?;
    Ok(client)
}

/// TLS with the system's root certificates, as the imap crate's own connection.
fn tls_connector() -> RustlsConnector {
    static CONNECTOR: OnceLock<RustlsConnector> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().unwrap_or_default() {
                let _ = roots.add(cert);
            }
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
                .into()
        })
        .clone()
}

/// Fetch unseen mail from contacts into the inbox. Only known contacts can get text
/// keyed on the air (see [`contact_for`] and [`accept_with`]); anything else is
/// tagged [`IGNORED_KEYWORD`], left unread, and not fetched again. A message the
/// server fails to fetch or flag is logged and skipped, and tried again next poll.
///
/// With `retry_ignored` (the node's first poll after starting), the keyword is
/// first cleared from every message, so mail turned away under an older
/// configuration (a contact not yet listed, a wrong `email.authserv_id`) is
/// considered again.
pub fn poll_imap(
    cfg: &Email,
    contacts: &[Contact],
    inbox: &Arc<Mutex<Inbox>>,
    retry_ignored: bool,
    gv: Option<&Phone>,
) -> Result<usize> {
    let mut session = connect_imap(cfg)?;
    session.select("INBOX")?;
    if retry_ignored {
        let mut tagged: Vec<u32> = session
            .uid_search(format!("KEYWORD {IGNORED_KEYWORD}"))?
            .into_iter()
            .collect();
        tagged.sort_unstable();
        for set in uid_sets(&tagged) {
            session
                .uid_store(set, format!("-FLAGS ({IGNORED_KEYWORD})"))
                .context("clearing the ignored keyword")?;
        }
        if !tagged.is_empty() {
            log::info!("considering {} ignored message(s) again", tagged.len());
        }
    }
    let mut uids: Vec<_> = session
        .uid_search(format!("UNSEEN NOT KEYWORD {IGNORED_KEYWORD}"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let mut added = 0;
    for uid in uids {
        let fetches = match session.uid_fetch(uid.to_string(), "(BODY.PEEK[] INTERNALDATE)") {
            Ok(f) => f,
            Err(e) => {
                log::warn!("IMAP fetch of UID {uid} failed, skipped: {e}");
                continue;
            }
        };
        let Some(fetch) = fetches
            .iter()
            .filter(|f| f.uid.is_none_or(|u| u == uid))
            .find(|f| f.body().is_some())
        else {
            log::warn!("IMAP fetch of UID {uid} returned no body, skipped");
            continue;
        };
        let raw = fetch.body().unwrap_or_default();
        let now = super::unix_now();
        let received = super::stamp(fetch.internal_date().map(|d| d.timestamp()), now);
        let flag = match accept_with(raw, uid, contacts, cfg.authserv_id.as_deref(), gv) {
            Ok(m) => {
                let mut ib = inbox.lock().map_err(|_| anyhow!("inbox lock poisoned"))?;
                // On error nothing is flagged, so the mail stays unseen for next time.
                // Ok(false) is a duplicate already on disk (the inbox never keeps
                // anything in memory that it failed to save).
                if ib.add(&m.contact.name, &m.source_id, &m.text, received)? {
                    added += 1;
                    log::info!("new message from {}", m.contact.name);
                }
                "\\Seen"
            }
            Err(r) => {
                if r.warn {
                    log::warn!("ignoring UID {uid}: {}", r.why);
                } else {
                    log::info!("ignoring UID {uid}: {}", r.why);
                }
                if let Some(c) = r.notice {
                    let source = r.source_id.unwrap_or_else(|| format!("uid:{uid}"));
                    let mut ib = inbox.lock().map_err(|_| anyhow!("inbox lock poisoned"))?;
                    // Not flagged unless the notice is safely stored.
                    if ib.add(&c.name, &format!("gv-unreadable:{source}"), UNREADABLE, now)? {
                        added += 1;
                    }
                }
                IGNORED_KEYWORD
            }
        };
        if let Err(e) = session.uid_store(uid.to_string(), format!("+FLAGS ({flag})")) {
            log::warn!("IMAP flag {flag} on UID {uid} failed: {e}");
        }
    }
    session.logout()?;
    Ok(added)
}

/// What the field operator hears in place of a contact's text the node could not
/// read.
pub const UNREADABLE: &str = "TEXT NOT READABLE SEE NODE LOG";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets_are_chunked() {
        assert!(uid_sets(&[]).is_empty());
        assert_eq!(uid_sets(&[3, 5, 9]), ["3,5,9"]);
        let many: Vec<u32> = (1..=450).collect();
        let sets = uid_sets(&many);
        assert_eq!(sets.len(), 3);
        assert!(sets[2].starts_with("401,") && sets[2].ends_with(",450"));
    }

    #[test]
    fn strips_quotes_and_signatures() {
        let body = "Sounds good, see you Sunday!\nLove, Mom\n\nOn Sat, Oct 4 2026, hfnode wrote:\n> RUNNING LATE\n";
        assert_eq!(strip_reply(body), "Sounds good, see you Sunday! Love, Mom");
        assert_eq!(strip_reply("ok\n\nSent from my iPhone"), "ok");
    }

    fn contacts() -> Vec<Contact> {
        vec![
            Contact {
                name: "MOM".into(),
                address: Some("5551234567@vtext.com".into()),
                ..Default::default()
            },
            Contact {
                name: "BOB".into(),
                address: Some("Bob@Example.com".into()),
                ..Default::default()
            },
        ]
    }

    const GOOGLE_PASS: &str = " mx.google.com;\r\n       dkim=pass header.i=@example.com header.s=20230601 header.b=abc;\r\n       spf=pass (google.com: domain of bob@example.com designates 192.0.2.1 as permitted sender) smtp.mailfrom=bob@example.com;\r\n       dmarc=pass (p=NONE sp=NONE dis=NONE) header.from=example.com\r\n";

    fn name(c: Option<&Contact>) -> Option<&str> {
        c.map(|c| c.name.as_str())
    }

    #[test]
    fn sms_replies_match_only_exact_numbers_at_carrier_gateways() {
        let c = contacts();
        for from in [
            "5551234567@vtext.com",
            "15551234567@vzwpix.com",
            "+15551234567@mypixmessages.com",
        ] {
            assert_eq!(
                name(contact_for(&c, from, &[], None)),
                Some("MOM"),
                "{from}"
            );
        }
        for from in [
            "promo5551234567@spam.biz",
            "5551234567@anything.com",
            "promo5551234567@vtext.com",
            "95551234567@vtext.com",
            "5551234567@vtext.com.evil.example",
            "spam@example.net",
            // MOM's number, but at another carrier.
            "5551234567@tmomail.net",
            "5551234567@txt.att.net",
        ] {
            assert_eq!(name(contact_for(&c, from, &[], None)), None, "{from}");
        }
        // A number-like address that is not at a carrier is not matched by number.
        let c = vec![Contact {
            name: "DAD".into(),
            address: Some("5551234567@example.com".into()),
            ..Default::default()
        }];
        assert_eq!(
            name(contact_for(&c, "5551234567@vtext.com", &[], None)),
            None
        );
    }

    #[test]
    fn sms_replies_that_our_server_failed_are_refused() {
        let c = contacts();
        for header in [
            "mx.google.com; spf=fail smtp.mailfrom=5551234567@vtext.com; dkim=none",
            "mx.google.com; dkim=fail header.d=vtext.com",
            "mx.google.com; dmarc=fail header.from=vtext.com",
        ] {
            assert_eq!(
                name(contact_for(&c, "5551234567@vtext.com", &[header], None)),
                None,
                "{header}"
            );
        }
        // Only our own server's header counts, for the fail as for a pass.
        let forged_fail = "evil.example; spf=fail smtp.mailfrom=5551234567@vtext.com";
        let ours = "mx.google.com; spf=pass smtp.mailfrom=5551234567@vtext.com";
        assert_eq!(
            name(contact_for(
                &c,
                "5551234567@vtext.com",
                &[forged_fail, ours],
                Some("mx.google.com")
            )),
            Some("MOM")
        );
        // A softfail, or a fail about another domain, is not an explicit fail.
        for header in [
            "mx.google.com; spf=softfail smtp.mailfrom=5551234567@vtext.com",
            "mx.google.com; dkim=fail header.d=relay.example",
        ] {
            assert_eq!(
                name(contact_for(&c, "5551234567@vtext.com", &[header], None)),
                Some("MOM"),
                "{header}"
            );
        }
    }

    #[test]
    fn email_contacts_need_an_aligned_pass_from_our_server() {
        let c = contacts();
        // From alone is not enough.
        assert_eq!(name(contact_for(&c, "bob@example.com", &[], None)), None);
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[GOOGLE_PASS], None)),
            Some("BOB")
        );
        assert_eq!(
            name(contact_for(
                &c,
                "BOB@example.com",
                &[GOOGLE_PASS],
                Some("mx.google.com")
            )),
            Some("BOB")
        );
        // Passing, but for the sender's own domain, not the one in From.
        let unaligned =
            "mx.google.com; dkim=pass header.d=evil.example; spf=pass smtp.mailfrom=x@evil.example";
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[unaligned], None)),
            None
        );
        let failed =
            "mx.google.com; dkim=fail header.d=example.com; spf=softfail smtp.mailfrom=example.com";
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[failed], None)),
            None
        );
        // A forged header below ours is not believed.
        assert_eq!(
            name(contact_for(
                &c,
                "bob@example.com",
                &[failed, GOOGLE_PASS],
                None
            )),
            None
        );
        // With an authserv-id set, only that server's header counts.
        let forged_top = "evil.example; dkim=pass header.d=example.com";
        assert_eq!(
            name(contact_for(
                &c,
                "bob@example.com",
                &[forged_top, failed],
                Some("mx.google.com")
            )),
            None
        );
        assert_eq!(
            name(contact_for(
                &c,
                "bob@example.com",
                &[forged_top, GOOGLE_PASS],
                Some("MX.google.com")
            )),
            Some("BOB")
        );
        // Subdomain alignment, `=` spacing, comments and quotes holding `;`.
        let odd = "mx.example.net 1; spf = pass (a; b) reason=\"x; dkim=pass\" smtp.mailfrom=bob@mail.example.com";
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[odd], None)),
            Some("BOB")
        );
        let quoted_only = "mx.example.net; spf=fail reason=\"x; dkim=pass header.d=example.com\"";
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[quoted_only], None)),
            None
        );
        // Microsoft leaves out the authserv-id.
        let microsoft = "spf=pass (sender IP is 192.0.2.1) smtp.mailfrom=example.com; dkim=pass (signature was verified) header.d=example.com;dmarc=pass action=none header.from=example.com;compauth=pass reason=100";
        assert_eq!(
            name(contact_for(&c, "bob@example.com", &[microsoft], None)),
            Some("BOB")
        );
    }

    #[test]
    fn accepts_only_authenticated_contact_mail_with_text() {
        let c = contacts();
        let mail = |headers: &str, body: &str| {
            format!("{headers}From: Bob <bob@example.com>\r\nTo: hfnode@example.org\r\nMessage-ID: <1@example.com>\r\nSubject: Re: From N0CALL\r\n\r\n{body}")
        };
        let good = mail(
            &format!("Authentication-Results:{GOOGLE_PASS}"),
            "See you Sunday\r\n",
        );
        let m = accept(good.as_bytes(), 7, &c, None).unwrap();
        assert_eq!(
            (
                m.contact.name.as_str(),
                m.source_id.as_str(),
                m.text.as_str()
            ),
            ("BOB", "1@example.com", "See you Sunday")
        );
        // Forged From with no authentication from our server.
        assert!(accept(mail("", "Buy now").as_bytes(), 7, &c, None).is_err());
        // An empty reply is not taken (and so gets flagged as ignored).
        let empty = mail(
            &format!("Authentication-Results:{GOOGLE_PASS}"),
            "> quoted only\r\n",
        );
        assert!(accept(empty.as_bytes(), 7, &c, None).is_err());
        let sms = "From: 5551234567@vzwpix.com\r\nTo: hfnode@example.org\r\n\r\nok\r\n";
        let m = accept(sms.as_bytes(), 9, &c, None).unwrap();
        assert_eq!(
            (m.contact.name.as_str(), m.source_id.as_str()),
            ("MOM", "uid:9")
        );
    }
}
