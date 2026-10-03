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
use mail_parser::MessageParser;
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

/// Which contact sent this, if any. Phone replies via SMS gateways often come from a
/// different domain than the one we send to (vtext.com vs vzwpix.com), so for
/// all-digit local parts the phone number alone is matched.
pub fn contact_for<'c>(contacts: &'c [Contact], from_addr: &str) -> Option<&'c Contact> {
    let from = from_addr.trim().to_ascii_lowercase();
    let local = |a: &str| a.split('@').next().unwrap_or("").to_string();
    let digits = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>();
    contacts.iter().find(|c| {
        let addr = c.address.to_ascii_lowercase();
        if addr == from {
            return true;
        }
        let (cl, fl) = (local(&addr), local(&from));
        cl.chars().all(|ch| ch.is_ascii_digit())
            && cl.len() >= 10
            && digits(&fl).ends_with(&cl[cl.len() - 10..])
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

/// Fetch unseen mail from contacts into the inbox. Mail from anyone else is left
/// unread and ignored: only known contacts can get text keyed on the air.
pub fn poll_imap(cfg: &Email, contacts: &[Contact], inbox: &Arc<Mutex<Inbox>>) -> Result<usize> {
    let client = imap::ClientBuilder::new(cfg.imap_host.as_str(), cfg.imap_port)
        .tls_kind(imap::TlsKind::Rust)
        .connect()
        .context("IMAP connect")?;
    let mut session = client
        .login(&cfg.username, password(cfg)?)
        .map_err(|(e, _)| anyhow!("IMAP login: {e}"))?;
    session.select("INBOX")?;
    let uids = session.uid_search("UNSEEN")?;
    let mut added = 0;
    for uid in uids {
        let fetches = session.uid_fetch(uid.to_string(), "BODY.PEEK[]")?;
        for f in fetches.iter() {
            let Some(raw) = f.body() else { continue };
            let Some(parsed) = MessageParser::default().parse(raw) else {
                continue;
            };
            let from = parsed
                .from()
                .and_then(|a| a.first())
                .and_then(|a| a.address())
                .unwrap_or("")
                .to_string();
            let Some(contact) = contact_for(contacts, &from) else {
                continue;
            };
            let text = strip_reply(&parsed.body_text(0).unwrap_or_default());
            if text.is_empty() {
                continue;
            }
            let source = parsed
                .message_id()
                .map(str::to_string)
                .unwrap_or_else(|| format!("uid:{uid}"));
            let mut ib = inbox.lock().map_err(|_| anyhow!("inbox lock poisoned"))?;
            if ib.add(&contact.name, &source, &text, super::unix_now())? {
                added += 1;
                log::info!("new message from {}", contact.name);
            }
            drop(ib);
            session.uid_store(uid.to_string(), "+FLAGS (\\Seen)")?;
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

    #[test]
    fn matches_contacts_including_sms_gateway_replies() {
        let contacts = vec![
            Contact {
                name: "MOM".into(),
                address: "5551234567@vtext.com".into(),
            },
            Contact {
                name: "BOB".into(),
                address: "Bob@Example.com".into(),
            },
        ];
        assert_eq!(
            contact_for(&contacts, "bob@example.com").unwrap().name,
            "BOB"
        );
        assert_eq!(
            contact_for(&contacts, "15551234567@vzwpix.com")
                .unwrap()
                .name,
            "MOM"
        );
        assert!(contact_for(&contacts, "spam@example.net").is_none());
    }
}
