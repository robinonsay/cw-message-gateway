//! Outbound mail over SMTP and inbound replies over IMAP.
//!
//! SMS is handled as email: contacts' phones are reached through their carrier's
//! email-to-SMS gateway, and replies from the phone come back as email.

use crate::config::{Contact, Email};
use crate::inbox::Inbox;
use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use mail_parser::{HeaderName, MessageParser};
use std::sync::{Arc, Mutex};

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

    pub fn send(&self, to: &str, text: &str) -> Result<()> {
        let body = format!(
            "{text}\n\n-- \nSent by {} over HF radio. Reply to this message; keep it short and plain.",
            self.field_call
        );
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to.parse().with_context(|| format!("bad address {to}"))?)
            .subject(format!("From {}", self.field_call))
            .header(ContentType::TEXT_PLAIN)
            .body(body)?;
        self.transport.send(&msg)?;
        log::info!("sent message to {to}");
        Ok(())
    }
}

/// Carrier email-to-SMS/MMS gateway domains (US). Replies from a phone come from
/// one of these, often not the one we send to (vtext.com vs vzwpix.com), so only
/// for these is the sender matched by phone number rather than full address.
pub const SMS_GATEWAYS: &[&str] = &[
    "vtext.com",               // Verizon SMS (also Visible, Xfinity Mobile)
    "vzwpix.com",              // Verizon MMS
    "mypixmessages.com",       // Verizon MMS
    "txt.att.net",             // AT&T SMS
    "mms.att.net",             // AT&T MMS
    "tmomail.net",             // T-Mobile
    "messaging.sprintpcs.com", // Sprint (T-Mobile)
    "pm.sprint.com",           // Sprint (T-Mobile)
    "mymetropcs.com",          // Metro by T-Mobile
    "msg.fi.google.com",       // Google Fi
    "email.uscc.net",          // US Cellular SMS
    "mms.uscc.net",            // US Cellular MMS
    "sms.cricketwireless.net", // Cricket SMS
    "mms.cricketwireless.net", // Cricket MMS
    "sms.myboostmobile.com",   // Boost SMS
    "myboostmobile.com",       // Boost MMS
    "mailmymobile.net",        // Consumer Cellular
];

/// A 10-digit US number from an address local part: exactly the number, optionally
/// with a leading `1` or `+1`.
fn us_number(local: &str) -> Option<&str> {
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
///   number (optional leading `1`) of a contact whose own address is also at a
///   carrier gateway: that contact. Carrier gateways do not reliably sign their
///   mail, so this match rests on the From header alone and a forged From such as
///   `5551234567@vtext.com` gets through to the filter. That is the price of SMS
///   replies working at all; keep the filter on.
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
    if SMS_GATEWAYS.contains(&fd) {
        if let Some(n) = us_number(fl) {
            let by_number = contacts.iter().find(|c| {
                let addr = c.address.trim().to_ascii_lowercase();
                let Some((cl, cd)) = addr.rsplit_once('@') else {
                    return false;
                };
                SMS_GATEWAYS.contains(&cd) && us_number(cl) == Some(n)
            });
            if by_number.is_some() {
                return by_number;
            }
        }
    }
    contacts
        .iter()
        .find(|c| c.address.trim().eq_ignore_ascii_case(&from))
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
    let Some((_, from_domain)) = from_addr.rsplit_once('@') else {
        return false;
    };
    let header = match authserv_id {
        Some(id) => auth_results
            .iter()
            .filter_map(|h| parse_auth_results(h))
            .find(|h| h.authserv_id.eq_ignore_ascii_case(id.trim())),
        None => auth_results.first().and_then(|h| parse_auth_results(h)),
    };
    let Some(header) = header else {
        return false;
    };
    let domain = |v: &str| v.rsplit('@').next().unwrap_or("").to_string();
    header.results.iter().any(|r| {
        let signer = match (r.method.as_str(), r.result.as_str()) {
            ("dkim", "pass") => r
                .prop("header.d")
                .map(str::to_string)
                .or_else(|| r.prop("header.i").map(domain)),
            ("spf", "pass") => r.prop("smtp.mailfrom").map(domain),
            _ => None,
        };
        signer.is_some_and(|d| aligned(&d, from_domain))
    })
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

/// IMAP keyword set on mail the node will never take (not from a contact, no text),
/// so later polls skip it without marking it read in the mailbox.
pub const IGNORED_KEYWORD: &str = "$HfnodeIgnored";

/// A message the node takes: who it is from, its de-duplication id and its text.
#[derive(Debug)]
pub struct Accepted<'c> {
    pub contact: &'c Contact,
    pub source_id: String,
    pub text: String,
}

/// Decide whether one fetched message goes in the inbox. `Err` says why not.
pub fn accept<'c>(
    raw: &[u8],
    uid: u32,
    contacts: &'c [Contact],
    authserv_id: Option<&str>,
) -> Result<Accepted<'c>, String> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or("unparseable message")?;
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .unwrap_or("")
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
    let contact = contact_for(contacts, &from, &auth_results, authserv_id)
        .ok_or_else(|| format!("from {from:?}: not a contact, or not authenticated"))?;
    let text = strip_reply(&parsed.body_text(0).unwrap_or_default());
    if text.is_empty() {
        return Err(format!("from {}: no text in the body", contact.name));
    }
    let source_id = parsed
        .message_id()
        .map(str::to_string)
        .unwrap_or_else(|| format!("uid:{uid}"));
    Ok(Accepted {
        contact,
        source_id,
        text,
    })
}

/// Fetch unseen mail from contacts into the inbox. Only known contacts can get text
/// keyed on the air (see [`contact_for`]); anything else is tagged
/// [`IGNORED_KEYWORD`], left unread, and not fetched again. A message the server
/// fails to fetch or flag is logged and skipped, and tried again next poll.
pub fn poll_imap(cfg: &Email, contacts: &[Contact], inbox: &Arc<Mutex<Inbox>>) -> Result<usize> {
    let client = imap::ClientBuilder::new(cfg.imap_host.as_str(), cfg.imap_port)
        .tls_kind(imap::TlsKind::Rust)
        .connect()
        .context("IMAP connect")?;
    let mut session = client
        .login(&cfg.username, password(cfg)?)
        .map_err(|(e, _)| anyhow!("IMAP login: {e}"))?;
    session.select("INBOX")?;
    let mut uids: Vec<_> = session
        .uid_search(format!("UNSEEN NOT KEYWORD {IGNORED_KEYWORD}"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let mut added = 0;
    for uid in uids {
        let fetches = match session.uid_fetch(uid.to_string(), "BODY.PEEK[]") {
            Ok(f) => f,
            Err(e) => {
                log::warn!("IMAP fetch of UID {uid} failed, skipped: {e}");
                continue;
            }
        };
        let Some(raw) = fetches
            .iter()
            .filter(|f| f.uid.is_none_or(|u| u == uid))
            .find_map(|f| f.body())
        else {
            log::warn!("IMAP fetch of UID {uid} returned no body, skipped");
            continue;
        };
        let flag = match accept(raw, uid, contacts, cfg.authserv_id.as_deref()) {
            Ok(m) => {
                let mut ib = inbox.lock().map_err(|_| anyhow!("inbox lock poisoned"))?;
                // On error nothing is flagged, so the mail stays unseen for next time.
                // Ok(false) is a duplicate already on disk (the inbox never keeps
                // anything in memory that it failed to save).
                if ib.add(&m.contact.name, &m.source_id, &m.text, super::unix_now())? {
                    added += 1;
                    log::info!("new message from {}", m.contact.name);
                }
                "\\Seen"
            }
            Err(why) => {
                log::info!("ignoring UID {uid}: {why}");
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

#[cfg(test)]
mod tests {
    use super::*;

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
                address: "5551234567@vtext.com".into(),
            },
            Contact {
                name: "BOB".into(),
                address: "Bob@Example.com".into(),
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
            "+15551234567@tmomail.net",
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
        ] {
            assert_eq!(name(contact_for(&c, from, &[], None)), None, "{from}");
        }
        // A number-like address that is not at a carrier is not matched by number.
        let c = vec![Contact {
            name: "DAD".into(),
            address: "5551234567@example.com".into(),
        }];
        assert_eq!(
            name(contact_for(&c, "5551234567@vtext.com", &[], None)),
            None
        );
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
