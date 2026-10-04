//! Telling the owner when the node stops transmitting.
//!
//! When the station cannot confirm the radio is back on receive, or cannot trust
//! its transmit status, it inhibits transmitting and writes
//! [`crate::station::INHIBIT_FILE`]. The node keeps running and decoding but keys
//! nothing, also after a restart, until the file is removed with the node stopped.
//! From outside it looks healthy, so `hfnode run` emails `[email] alert_to` once
//! when the inhibit latches, and once at start-up if the file is already there.
//! Without `alert_to` the inhibit is only logged.
//!
//! The email goes out from a thread of its own: the station only puts a notice on a
//! channel, and an SMTP exchange can take minutes, which must not hold up decoding
//! or keying.

use crate::config::{Config, Email};
use crate::station::InhibitNotice;
use anyhow::{Context, Result};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// How long `hfnode run` waits, on its way out, for an alert still being sent.
pub const EXIT_GRACE: Duration = Duration::from_secs(30);

/// Limit on each SMTP connect, read and write (lettre's default is 60 s).
const SMTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Waits before trying a failed alert again; then it is given up (and logged).
const RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];

/// Sends one email: (subject, body).
pub type Deliver = Box<dyn FnMut(&str, &str) -> Result<(), String> + Send>;

/// The thread that turns [`InhibitNotice`]s into emails.
pub struct Alerts {
    tx: Option<Sender<InhibitNotice>>,
    done: Receiver<()>,
    emails: bool,
}

impl Alerts {
    /// Email `[email] alert_to`, or only log without it. `config` is the config
    /// file's path, quoted in the instructions.
    pub fn start(cfg: &Config, config: &Path) -> Self {
        // The owner runs the check from wherever they are, not from our directory.
        let config = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
        let to = cfg
            .email
            .as_ref()
            .and_then(|e| e.alert_to.as_deref().map(|to| (e, to)));
        let deliver = match to {
            Some((e, to)) => match smtp(e, to) {
                Ok(d) => {
                    log::info!("if transmitting is inhibited, {to} is emailed");
                    Some(d)
                }
                Err(err) => {
                    log::error!("cannot email inhibit alerts to {to}: {err:#}");
                    None
                }
            },
            None => {
                log::warn!("no [email] alert_to: if transmitting is inhibited, it is only logged");
                None
            }
        };
        Self::with_deliver(
            &cfg.station.node_call,
            &config,
            deliver,
            RETRY_AFTER.to_vec(),
        )
    }

    /// With any way of sending, and retry waits (for tests).
    pub fn with_deliver(
        node_call: &str,
        config: &Path,
        deliver: Option<Deliver>,
        retry_after: Vec<Duration>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<InhibitNotice>();
        let (done_tx, done) = mpsc::channel();
        let emails = deliver.is_some();
        let call = node_call.to_ascii_uppercase();
        let config = config.to_path_buf();
        thread::spawn(move || {
            let mut deliver = deliver;
            // Ends once every sender, the station's included, is gone.
            for n in rx {
                let Some(send) = deliver.as_mut() else {
                    log::warn!(
                        "transmit inhibited ({}); no [email] alert_to, so nobody was emailed",
                        n.reason
                    );
                    continue;
                };
                let (subject, body) = message(&call, &config, &n, crate::gateway::unix_now());
                let mut waits = retry_after.iter();
                loop {
                    match send(&subject, &body) {
                        Ok(()) => {
                            log::info!("emailed the transmit-inhibit alert");
                            break;
                        }
                        Err(e) => match waits.next() {
                            Some(w) => {
                                log::warn!("inhibit alert not sent ({e}); trying again in {w:?}");
                                thread::sleep(*w);
                            }
                            None => {
                                log::error!("inhibit alert not sent ({e}); giving up");
                                break;
                            }
                        },
                    }
                }
            }
            let _ = done_tx.send(());
        });
        Self {
            tx: Some(tx),
            done,
            emails,
        }
    }

    /// Whether alerts are emailed (otherwise only logged).
    pub fn emails(&self) -> bool {
        self.emails
    }

    /// For [`crate::station::Station::notify_inhibit`].
    pub fn sender(&self) -> Sender<InhibitNotice> {
        self.tx.clone().expect("sender taken only by finish")
    }

    /// Wait, at most `grace`, for alerts already queued to be sent. Call once the
    /// station has been dropped: dropping it forces receive, which can still latch
    /// the inhibit, and its sender must be gone for this to return early.
    pub fn finish(mut self, grace: Duration) {
        drop(self.tx.take());
        if self.done.recv_timeout(grace).is_err() {
            log::warn!("stopped waiting for an inhibit alert to be sent");
        }
    }
}

/// An SMTP sender to `to`, set up as `gateway::email::Mailer` sets up its own.
fn smtp(e: &Email, to: &str) -> Result<Deliver> {
    let password = std::env::var(&e.password_env)
        .with_context(|| format!("environment variable {} is not set", e.password_env))?;
    // Port 465 is implicit TLS; anything else uses STARTTLS (normally 587).
    let builder = if e.smtp_port == 465 {
        SmtpTransport::relay(&e.smtp_host)?
    } else {
        SmtpTransport::starttls_relay(&e.smtp_host)?
    };
    let transport = builder
        .port(e.smtp_port)
        .credentials(Credentials::new(e.username.clone(), password))
        .timeout(Some(SMTP_TIMEOUT))
        .build();
    let from: Mailbox = e.from_address.parse().context("email.from_address")?;
    let to: Mailbox = to.parse().context("email.alert_to")?;
    Ok(Box::new(move |subject: &str, body: &str| {
        let msg = Message::builder()
            .from(from.clone())
            .to(to.clone())
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .map_err(|e| e.to_string())?;
        transport.send(&msg).map(|_| ()).map_err(|e| e.to_string())
    }))
}

/// The alert for `n`, from node `call` whose config file is `config`, sent at
/// `now` (Unix time): (subject, body), plain text.
pub fn message(call: &str, config: &Path, n: &InhibitNotice, now: u64) -> (String, String) {
    let when = n.at.map_or_else(|| "not recorded".to_string(), utc);
    let reason = if n.reason.is_empty() {
        "not recorded"
    } else {
        n.reason.as_str()
    };
    let (subject, mut lines) = if n.from_file {
        (
            format!("{call}: node restarted, still not transmitting (tx-inhibited)"),
            vec![
                format!(
                    "hfnode {call} started at {} with transmitting already",
                    utc(now)
                ),
                "inhibited by an earlier fault. It is running and decoding, but it does".into(),
                "not tune, read back or reply to the field operator until the inhibit".into(),
                "is cleared by hand.".into(),
            ],
        )
    } else {
        (
            format!("{call}: node stopped transmitting (tx-inhibited)"),
            vec![
                format!("hfnode {call} has stopped transmitting: it could not be sure the"),
                "radio was back on receive. It keeps running and decoding, but it does not".into(),
                "tune, read back or reply to the field operator, also after a restart,".into(),
                "until the inhibit is cleared by hand.".into(),
            ],
        )
    };
    lines.extend([
        String::new(),
        format!("Reason: {reason}"),
        format!("Inhibited at: {when}"),
        match &n.file {
            Some(f) => format!("Kept in: {}", f.display()),
            None => "Not written to disk: a restart clears it.".into(),
        },
        String::new(),
        "What to do:".into(),
        "1. Stop the node: sudo systemctl stop hfnode".into(),
        "2. Check the radio: power, the USB cable, the TX indicator off, nothing".into(),
        "   else keying it. Then run the read-only check; it must pass:".into(),
        format!(
            "   sudo -u hfnode hfnode radio --config {} check",
            config.display()
        ),
    ]);
    match &n.file {
        Some(f) => lines.extend([
            "3. Read the reason, then delete the file (deleting it while the node".into(),
            "   runs changes nothing):".into(),
            // The state directory is readable only by the hfnode user.
            format!("   sudo cat {}", f.display()),
            format!("   sudo rm {}", f.display()),
        ]),
        None => lines.push("3. There is no file to delete.".into()),
    }
    lines.extend([
        "4. Start the node: sudo systemctl start hfnode".into(),
        "   (if systemd gave up on it: sudo systemctl reset-failed hfnode first)".into(),
        String::new(),
        "Do not clear the inhibit before you know what happened.".into(),
        String::new(),
        "Sent by the node itself. Do not reply: a reply from one of the node's".into(),
        "contact addresses would be queued as a message for the field operator.".into(),
    ]);
    (subject, lines.join("\n") + "\n")
}

/// `YYYY-MM-DD HH:MM:SS UTC`.
fn utc(unix: u64) -> String {
    let (days, secs) = (unix / 86_400, unix % 86_400);
    // Days to a civil date (H. Hinnant, "chrono-Compatible Low-Level Date
    // Algorithms"), for dates from 1970.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const CONFIG: &str = "/etc/hfnode/hfnode.toml";

    fn latched() -> InhibitNotice {
        InhibitNotice {
            at: Some(1_791_120_363),
            reason: "radio not confirmed on receive (no reply from radio)".into(),
            from_file: false,
            file: Some("/var/lib/hfnode/tx-inhibited".into()),
        }
    }

    #[test]
    fn utc_formats_unix_time() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(1_791_120_363), "2026-10-04 13:26:03 UTC");
        assert_eq!(utc(4_102_444_799), "2099-12-31 23:59:59 UTC");
    }

    /// `message`, with every line short enough for any mail reader.
    fn plain(n: &InhibitNotice, now: u64) -> (String, String) {
        let (subject, body) = message("N0CALL", Path::new(CONFIG), n, now);
        assert!(body.lines().all(|l| l.len() <= 78), "{body}");
        (subject, body)
    }

    #[test]
    fn latch_alert_says_what_happened_and_what_to_do() {
        let (subject, body) = plain(&latched(), 1_791_120_400);
        assert_eq!(subject, "N0CALL: node stopped transmitting (tx-inhibited)");
        for want in [
            "hfnode N0CALL has stopped transmitting",
            "Reason: radio not confirmed on receive (no reply from radio)",
            "Inhibited at: 2026-10-04 13:26:03 UTC",
            "Kept in: /var/lib/hfnode/tx-inhibited",
            "Do not reply",
        ] {
            assert!(body.contains(want), "{want:?} missing from:\n{body}");
        }
        // The steps, in the order to take them.
        let mut from = 0;
        for step in [
            "sudo systemctl stop hfnode",
            "sudo -u hfnode hfnode radio --config /etc/hfnode/hfnode.toml check",
            "sudo cat /var/lib/hfnode/tx-inhibited",
            "sudo rm /var/lib/hfnode/tx-inhibited",
            "sudo systemctl start hfnode",
        ] {
            let at = body[from..].find(step);
            assert!(at.is_some(), "{step:?} missing or out of order in:\n{body}");
            from += at.unwrap() + step.len();
        }
    }

    #[test]
    fn startup_alert_says_it_was_already_inhibited() {
        let n = InhibitNotice {
            from_file: true,
            ..latched()
        };
        let (subject, body) = plain(&n, 1_791_206_763);
        assert_eq!(
            subject,
            "N0CALL: node restarted, still not transmitting (tx-inhibited)"
        );
        assert!(
            body.contains("started at 2026-10-05 13:26:03 UTC"),
            "{body}"
        );
        assert!(
            body.contains("Inhibited at: 2026-10-04 13:26:03 UTC"),
            "{body}"
        );
        // A file written by hand.
        let n = InhibitNotice {
            at: None,
            reason: String::new(),
            ..n
        };
        let (_, body) = plain(&n, 0);
        assert!(body.contains("Reason: not recorded"), "{body}");
        assert!(body.contains("Inhibited at: not recorded"), "{body}");
    }

    #[test]
    fn alert_without_a_file_says_a_restart_clears_it() {
        let n = InhibitNotice {
            file: None,
            ..latched()
        };
        let (_, body) = plain(&n, 0);
        assert!(
            body.contains("Not written to disk: a restart clears it."),
            "{body}"
        );
        assert!(body.contains("There is no file to delete"), "{body}");
        assert!(!body.contains("sudo rm"), "{body}");
    }

    type Sent = Arc<Mutex<Vec<(String, String)>>>;

    /// Records what it sends, after `delay`, failing the first `fail` times.
    fn recorder(delay: Duration, mut fail: u32) -> (Deliver, Sent) {
        let sent: Sent = Arc::default();
        let log = sent.clone();
        let d: Deliver = Box::new(move |subject: &str, body: &str| {
            thread::sleep(delay);
            if fail > 0 {
                fail -= 1;
                return Err("connection refused".into());
            }
            log.lock().unwrap().push((subject.into(), body.into()));
            Ok(())
        });
        (d, sent)
    }

    #[test]
    fn alerts_are_sent_off_the_callers_thread() {
        let (d, sent) = recorder(Duration::from_millis(500), 0);
        let alerts = Alerts::with_deliver("n0call", Path::new(CONFIG), Some(d), Vec::new());
        assert!(alerts.emails());
        let to = alerts.sender();
        let t0 = Instant::now();
        to.send(latched()).unwrap();
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "the latch does not wait"
        );
        assert!(sent.lock().unwrap().is_empty());
        drop(to);
        alerts.finish(Duration::from_secs(10));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].0,
            "N0CALL: node stopped transmitting (tx-inhibited)"
        );
    }

    #[test]
    fn a_failed_alert_is_tried_again_then_given_up() {
        let (d, sent) = recorder(Duration::ZERO, 1);
        let alerts = Alerts::with_deliver(
            "N0CALL",
            Path::new(CONFIG),
            Some(d),
            vec![Duration::from_millis(10)],
        );
        alerts.sender().send(latched()).unwrap();
        alerts.finish(Duration::from_secs(10));
        assert_eq!(sent.lock().unwrap().len(), 1, "sent on the second try");

        let (d, sent) = recorder(Duration::ZERO, 5);
        let alerts = Alerts::with_deliver(
            "N0CALL",
            Path::new(CONFIG),
            Some(d),
            vec![Duration::from_millis(10); 2],
        );
        alerts.sender().send(latched()).unwrap();
        let t0 = Instant::now();
        alerts.finish(Duration::from_secs(10));
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "gave up after 3 tries"
        );
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn finish_waits_no_longer_than_its_grace() {
        let (d, _) = recorder(Duration::from_secs(5), 0);
        let alerts = Alerts::with_deliver("N0CALL", Path::new(CONFIG), Some(d), Vec::new());
        alerts.sender().send(latched()).unwrap();
        let t0 = Instant::now();
        alerts.finish(Duration::from_millis(200));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn without_alert_to_or_a_password_alerts_are_only_logged() {
        let mut cfg: Config = toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
        cfg.email.as_mut().unwrap().password_env = "HFNODE_TEST_NEVER_SET_7F3A".into();
        assert!(!Alerts::start(&cfg, Path::new(CONFIG)).emails());
        cfg.email.as_mut().unwrap().alert_to = None;
        assert!(!Alerts::start(&cfg, Path::new(CONFIG)).emails());
        cfg.email = None;
        let alerts = Alerts::start(&cfg, Path::new(CONFIG));
        assert!(!alerts.emails());
        alerts.sender().send(latched()).unwrap();
        let t0 = Instant::now();
        alerts.finish(Duration::from_secs(10));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
}
