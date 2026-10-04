//! `hfnode messages`: see how TX would reach each contact and what the node would
//! take from each inbound route, and send one message, all without the radio.

use crate::config::{Config, Contact, GOOGLE_VOICE_DOMAIN};
use crate::gateway::email::{
    accept_with, auth_headers, auth_summary, connect_imap, from_address, Via,
};
use crate::gateway::google_voice::{self, learn_decision, parse_gv_from, GvStore, Learn};
use crate::gateway::imessage::{self, ImShared, OsaRunner, Readiness};
use crate::gateway::{self, route, RouteKind};
use crate::inbox::Inbox;
use crate::node;
use crate::session::SendError;
use anyhow::{Context, Result};
use mail_parser::MessageParser;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// An unused Google Voice number can be taken back after about three months.
const GV_IDLE_WARN_DAYS: u64 = 60;

/// Check iMessage now (when configured), as the node does at start-up.
pub fn probe_imessage(cfg: &Config) -> Option<(Arc<ImShared>, Readiness)> {
    let im = cfg.imessage.as_ref()?;
    let shared = Arc::new(ImShared::new());
    let r = imessage::probe(im, &OsaRunner::osascript(), &shared);
    Some((shared, r))
}

/// `hfnode messages check`. Reads only: the mailbox is examined, never selected, so
/// no flag changes, and nothing in `state_dir` is written.
pub fn check(
    cfg: &Config,
    since_hours: u64,
    save_raw: Option<&Path>,
    dump: Option<i64>,
    out: &mut dyn Write,
) -> Result<()> {
    let probed = probe_imessage(cfg);
    let shared = probed.as_ref().map(|(s, _)| &**s);
    let gv = GvStore::read(&cfg.state_dir);
    let im_state = gateway::imessage_state(cfg, shared);
    let a = gateway::avail(cfg, cfg.email.is_some(), &im_state);

    writeln!(out, "Routes (how TX would reach each contact now):")?;
    let width = cfg.contacts.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in &cfg.contacts {
        writeln!(out, "  {:<width$}  {}", c.name, route::describe(c, &a, &gv))?;
    }
    for w in cfg.warnings() {
        writeln!(out, "  warning: {w}")?;
    }

    if let (Some(gvc), Some(mail)) = (&cfg.google_voice, &cfg.email) {
        writeln!(out, "\nGoogle Voice ({}):", gvc.number)?;
        for c in cfg.contacts.iter().filter(|c| c.phone.is_some()) {
            let p = c.phone.as_ref().map(|p| p.to_string()).unwrap_or_default();
            match gv.contacts.get(&p) {
                Some(e) => writeln!(
                    out,
                    "  {} {p}: reply address {} (learned {})",
                    c.name,
                    e.address,
                    gateway::date(e.learned_unix)
                )?,
                None => writeln!(
                    out,
                    "  {} {p}: none: have {} text {}, then wait for the node's next mail check",
                    c.name, c.name, gvc.number
                )?,
            }
        }
        match gv.last_sent_unix {
            Some(t) => {
                let days = gateway::unix_now().saturating_sub(t) / 86_400;
                writeln!(out, "  last text sent from the number {days} days ago")?;
                if days > GV_IDLE_WARN_DAYS {
                    writeln!(
                        out,
                        "  warning: send a text from the number soon (hfnode messages send \
                         --via google-voice): an unused number can be taken back"
                    )?;
                }
            }
            None => writeln!(out, "  no text sent from the number by the node yet")?,
        }
        if let Err(e) = gv_mailbox(cfg, mail, save_raw, out) {
            writeln!(out, "  mailbox: {e:#}")?;
        }
    }

    if let Some(mail) = &cfg.email {
        writeln!(out, "\nMailbox, last 2 days (what the node would take):")?;
        if let Err(e) = imap_dry_run(cfg, mail, out) {
            writeln!(out, "  mailbox: {e:#}")?;
        }
    }

    if let (Some(im), Some((_, r))) = (&cfg.imessage, &probed) {
        writeln!(out, "\niMessage:")?;
        for line in imessage::check_report(
            im,
            &cfg.contacts,
            &cfg.state_dir,
            r,
            since_hours,
            dump,
            gateway::unix_now(),
        ) {
            writeln!(out, "  {line}")?;
        }
    }
    Ok(())
}

/// The Google Voice mail in the mailbox, as the node's address learning sees it.
fn gv_mailbox(
    cfg: &Config,
    mail: &crate::config::Email,
    save_raw: Option<&Path>,
    out: &mut dyn Write,
) -> Result<()> {
    let Some(gvc) = &cfg.google_voice else {
        return Ok(());
    };
    let mut s = connect_imap(mail)?;
    s.examine("INBOX")?;
    let mut uids: Vec<u32> = s
        .uid_search(format!("FROM \"{GOOGLE_VOICE_DOMAIN}\""))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    writeln!(out, "  {} Google Voice mail(s) in the mailbox", uids.len())?;
    let newest = &uids[uids.len().saturating_sub(20)..];
    for &uid in newest.iter().rev() {
        let fetches = s.uid_fetch(uid.to_string(), "(UID INTERNALDATE BODY.PEEK[])")?;
        let Some(f) = fetches.iter().find(|f| f.body().is_some()) else {
            continue;
        };
        let raw = f.body().unwrap_or_default();
        let date = f
            .internal_date()
            .map_or("?".into(), |d| d.format("%Y-%m-%d %H:%M").to_string());
        let Some(parsed) = MessageParser::default().parse(raw) else {
            writeln!(out, "  UID {uid} {date}: unparseable")?;
            continue;
        };
        let subject = parsed.subject().unwrap_or("").trim();
        let auth = auth_summary(&auth_headers(&parsed), mail.authserv_id.as_deref());
        let decision =
            match learn_decision(raw, &cfg.contacts, &gvc.number, mail.authserv_id.as_deref()) {
                Learn::Yes { contact, address } => {
                    format!("{}: one-to-one: would learn {address}", contact.name)
                }
                Learn::NotContact => "not from a contact's phone to this number".into(),
                Learn::NotAuthenticated(c) => format!("{}: no DKIM pass, not learned", c.name),
                Learn::NotOneToOne(c) => {
                    format!("{}: not confirmed one-to-one: RX only, not learned", c.name)
                }
            };
        writeln!(out, "  UID {uid} {date} \"{subject}\" [{auth}] {decision}")?;
        if let Some(dir) = save_raw {
            save_eml(dir, uid, raw)?;
        }
    }
    // Other mail from Google: number reclaim notices, bounces of replies.
    let mut other: Vec<u32> = s
        .uid_search("UNSEEN FROM \"google.com\"")?
        .into_iter()
        .collect();
    other.sort_unstable();
    for &uid in other.iter().rev().take(20) {
        let fetches = s.uid_fetch(uid.to_string(), "(UID BODY.PEEK[HEADER])")?;
        let Some(raw) = fetches.iter().find_map(|f| f.header()) else {
            continue;
        };
        let Some(parsed) = MessageParser::default().parse(raw) else {
            continue;
        };
        let from = from_address(&parsed);
        if from.to_ascii_lowercase().ends_with(GOOGLE_VOICE_DOMAIN) {
            continue;
        }
        writeln!(
            out,
            "  unread from {from}: \"{}\"",
            parsed.subject().unwrap_or("").trim()
        )?;
        if from.eq_ignore_ascii_case("voice-noreply@google.com") {
            if let Some(dir) = save_raw {
                let full = s.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")?;
                if let Some(body) = full.iter().find_map(|f| f.body()) {
                    save_eml(dir, uid, body)?;
                }
            }
        }
    }
    let _ = s.logout();
    if save_raw.is_some() {
        writeln!(
            out,
            "  these files hold phone numbers, reply tokens and private texts: redact before \
             committing (docs/texting.md)"
        )?;
    }
    Ok(())
}

/// Write one mail for a test fixture, readable by the owner only.
fn save_eml(dir: &Path, uid: u32, raw: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("uid-{uid}.eml"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)
        .and_then(|mut f| f.write_all(raw))
        .with_context(|| format!("writing {}", path.display()))
}

/// What the node would do with each mail of the last two days.
fn imap_dry_run(cfg: &Config, mail: &crate::config::Email, out: &mut dyn Write) -> Result<()> {
    let inbox = Inbox::open(cfg.state_dir.join("inbox.json")).ok();
    let gv = cfg.google_voice.as_ref().map(|g| &g.number);
    let mut s = connect_imap(mail)?;
    s.examine("INBOX")?;
    let since = imap_date(gateway::unix_now().saturating_sub(2 * 86_400));
    let mut uids: Vec<u32> = s
        .uid_search(format!("SINCE {since}"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let newest = &uids[uids.len().saturating_sub(50)..];
    for &uid in newest {
        let fetches = s.uid_fetch(uid.to_string(), "(UID FLAGS INTERNALDATE BODY.PEEK[])")?;
        let Some(f) = fetches.iter().find(|f| f.body().is_some()) else {
            continue;
        };
        let raw = f.body().unwrap_or_default();
        let date = f
            .internal_date()
            .map_or("?".into(), |d| d.format("%Y-%m-%d %H:%M").to_string());
        let flags: Vec<String> = f.flags().iter().map(|x| x.to_string()).collect();
        let seen = f
            .flags()
            .iter()
            .any(|x| matches!(x, imap::types::Flag::Seen));
        let parsed = MessageParser::default().parse(raw);
        let from = parsed.as_ref().map(from_address).unwrap_or_default();
        let head = format!("  UID {uid} {date} {from} [{}]", flags.join(" "));
        match accept_with(raw, uid, &cfg.contacts, mail.authserv_id.as_deref(), gv) {
            Ok(m) => {
                let via = match m.via {
                    Via::Email => "email",
                    Via::Carrier => "carrier SMS",
                    Via::GoogleVoice => "Google Voice",
                };
                writeln!(out, "{head}: {} by {via}: {}", m.contact.name, m.text)?;
                if m.via == Via::GoogleVoice {
                    if let Some(body) = parsed.as_ref().and_then(|p| google_voice::gv_text(p).ok())
                    {
                        writeln!(out, "      cut at: {}", body.cut_at)?;
                        if body.text != m.text {
                            writeln!(out, "      reaction: {} -> {}", body.text, m.text)?;
                        }
                    }
                }
                let held = inbox.as_ref().is_some_and(|i| i.has_source(&m.source_id));
                if seen && !held {
                    writeln!(
                        out,
                        "      read in the mailbox but not imported (opened in Gmail?); mark it \
                         unread to retry"
                    )?;
                }
            }
            Err(r) => {
                writeln!(out, "{head}: not taken: {}", r.why)?;
                if r.why.contains("not a contact's phone") {
                    if let Some(a) = parse_gv_from(&from) {
                        let phones: Vec<String> = cfg
                            .contacts
                            .iter()
                            .filter_map(|c| c.phone.as_ref().map(|p| format!("{} {p}", c.name)))
                            .collect();
                        writeln!(
                            out,
                            "      sender {} is none of: {}",
                            a.sender,
                            phones.join(", ")
                        )?;
                    }
                }
            }
        }
    }
    let _ = s.logout();
    Ok(())
}

/// An IMAP SEARCH date, `04-Oct-2026`.
fn imap_date(unix: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let d = gateway::date(unix);
    let (y, rest) = d.split_at(4);
    let m: usize = rest[1..3].parse().unwrap_or(1);
    format!("{}-{}-{y}", &rest[4..6], MONTHS[m.clamp(1, 12) - 1])
}

/// What `hfnode messages send` ended with, as its exit code.
pub enum SendResult {
    Sent = 0,
    Gateway = 1,
    NoRoute = 2,
}

/// `hfnode messages send`: one message now, by the route TX would use, tagged as a
/// TX is. An iMessage opens a reply window as a TX does.
pub fn send(
    cfg: &Config,
    via: Option<RouteKind>,
    call: Option<&str>,
    name: &str,
    text: &str,
    out: &mut dyn Write,
) -> Result<SendResult> {
    let name = name.to_ascii_uppercase();
    let Some(contact) = cfg.contacts.iter().find(|c| c.name == name) else {
        writeln!(out, "no contact {name}")?;
        return Ok(SendResult::NoRoute);
    };
    let text = protocol::sanitize(text);
    let call = call
        .map(str::to_ascii_uppercase)
        .or_else(|| {
            cfg.station
                .field_calls
                .first()
                .map(|c| c.to_ascii_uppercase())
        })
        .unwrap_or_default();
    let probed = probe_imessage(cfg);
    if let Some((_, Readiness::NotReady(why))) = &probed {
        writeln!(out, "iMessage not available: {why}")?;
    }
    learn_if_needed(cfg, contact, out);
    let inbox = node::open_inbox(cfg)?;
    let mut svc = node::live_services(cfg, inbox, probed.map(|(s, _)| s))?;
    match svc.deliver(&name, &call, &text, via) {
        Ok(d) => {
            let how = match &d.route {
                route::Route::IMessage(h) => format!("iMessage to {h}"),
                route::Route::GoogleVoice(a) => format!("Google Voice, by email to {a}"),
                route::Route::Email(a) => format!("email to {a}"),
            };
            writeln!(out, "SENT to {name} by {how}")?;
            if !d.detail.is_empty() {
                writeln!(out, "  {}", d.detail)?;
            }
            Ok(SendResult::Sent)
        }
        Err(SendError::Gateway(e)) => {
            writeln!(out, "FAIL GATEWAY: {e}")?;
            Ok(SendResult::Gateway)
        }
        Err(SendError::NoRoute(e)) => {
            writeln!(out, "NO ROUTE: {e}")?;
            Ok(SendResult::NoRoute)
        }
    }
}

/// Look for `contact`'s Google Voice reply address once, if it has a phone and none
/// is known yet (the node may not have checked the mail since they texted).
fn learn_if_needed(cfg: &Config, contact: &Contact, out: &mut dyn Write) {
    let (Some(p), Some(gvc), Some(mail)) = (&contact.phone, &cfg.google_voice, &cfg.email) else {
        return;
    };
    if GvStore::read(&cfg.state_dir)
        .reply_address(p, &gvc.number)
        .is_some()
    {
        return;
    }
    if let Err(e) = google_voice::learn_gv(mail, &gvc.number, &cfg.contacts, &cfg.state_dir, false)
    {
        let _ = writeln!(
            out,
            "looking for {}'s Google Voice address failed: {e:#}",
            contact.name
        );
    }
}

/// Under the code table: how TX reaches each contact, by kind only (no numbers or
/// addresses on paper).
pub fn code_table_routes(cfg: &Config) -> Vec<String> {
    let gv = GvStore::read(&cfg.state_dir);
    let im: Result<(), String> = if cfg.imessage.is_some() {
        Ok(())
    } else {
        Err(String::new())
    };
    let a = gateway::avail(cfg, cfg.email.is_some(), &im);
    cfg.contacts
        .iter()
        .map(|c| {
            let how = match route::plan(c, &a, &gv) {
                Ok(routes) => routes
                    .iter()
                    .map(|r| match r {
                        route::Route::IMessage(_) => "iMessage (if Messages is ready)",
                        r => r.kind(),
                    })
                    .collect::<Vec<_>>()
                    .join(", then "),
                Err(_) if c.phone.is_some() && cfg.google_voice.is_some() => {
                    "NO ROUTE: has not texted the node's Google Voice number yet".into()
                }
                Err(_) => "NO ROUTE: see hfnode messages check".into(),
            };
            format!("{:>5}  {how}", c.name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_dates() {
        assert_eq!(imap_date(0), "01-Jan-1970");
        assert_eq!(imap_date(1_791_100_000), "04-Oct-2026");
    }

    #[test]
    fn code_table_shows_kinds_only() {
        let mut cfg: Config = toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        cfg.state_dir = dir.path().to_path_buf();
        let lines = code_table_routes(&cfg);
        assert!(
            lines.iter().all(|l| !l.contains('@') && !l.contains("+1")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("BOB  email")), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("MOM  NO ROUTE: has not texted")),
            "{lines:?}"
        );
    }
}
