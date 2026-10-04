//! Connections to the outside world: email, texts and iMessage, weather and the
//! inbound filter.

pub mod email;
pub mod filter;
pub mod google_voice;
pub mod imessage;
pub mod route;
pub mod typedstream;
pub mod weather;

use crate::config::{Config, Contact};
use crate::inbox::{Inbox, Message};
use crate::session::{SendError, Services, WxError};
use google_voice::GvStore;
use imessage::{ImOutcome, ImSender, ImShared};
use route::{Avail, Route};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// When a message was received, for the inbox: the time its source gives (the
/// mailbox's arrival time, Messages' date), but never later than now.
pub fn stamp(source: Option<i64>, now: u64) -> u64 {
    match source.and_then(|s| u64::try_from(s).ok()) {
        Some(s) if s > 0 => s.min(now),
        _ => now,
    }
}

/// A Unix time as a UTC date, `2026-10-04`.
pub fn date(unix: u64) -> String {
    // Days to civil date (Howard Hinnant's algorithm).
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `tag` with the field callsign put in, before `text`: how a text or iMessage
/// starts, so the contact knows who sent it and that replies are read on air.
pub fn tagged(tag: &str, call: &str, text: &str) -> String {
    format!("{} {text}", tag.replace("{call}", call))
}

/// Which kind of route, for `hfnode messages send --via`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    IMessage,
    GoogleVoice,
    Email,
}

impl RouteKind {
    fn of(r: &Route) -> Self {
        match r {
            Route::IMessage(_) => Self::IMessage,
            Route::GoogleVoice(_) => Self::GoogleVoice,
            Route::Email(_) => Self::Email,
        }
    }
}

/// How a message went out.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub route: Route,
    /// For iMessage, what Messages recorded.
    pub detail: String,
}

/// The real [`Services`]: iMessage, Google Voice and SMTP for outbound, the shared
/// inbox for inbound, NWS for weather.
pub struct LiveServices {
    pub cfg: Config,
    pub inbox: Arc<Mutex<Inbox>>,
    pub mailer: Option<email::Mailer>,
    pub weather: Option<weather::Nws>,
    /// With `[imessage]`: Messages' state, shared with the node's iMessage thread.
    pub imessage: Option<ImSender>,
}

/// Why iMessage cannot send, or `Ok`.
pub fn imessage_state(cfg: &Config, shared: Option<&ImShared>) -> Result<(), String> {
    match (&cfg.imessage, shared) {
        (Some(_), Some(s)) => s.send_ready(),
        _ if !cfg!(target_os = "macos") => Err("iMessage works only on a Mac".into()),
        _ => Err("[imessage] is not configured".into()),
    }
}

/// What can send from here: for [`route::plan`].
pub fn avail<'a>(cfg: &'a Config, mailer: bool, imessage: &'a Result<(), String>) -> Avail<'a> {
    Avail {
        imessage: imessage.as_ref().map(|_| ()).map_err(String::as_str),
        gv_number: cfg
            .google_voice
            .as_ref()
            .filter(|_| mailer)
            .map(|g| &g.number),
        email: mailer,
    }
}

impl LiveServices {
    fn contact(&self, name: &str) -> Option<&Contact> {
        self.cfg.contacts.iter().find(|c| c.name == name)
    }

    /// The routes TX would try for `dest` now, or why there is none.
    pub fn plan(&self, dest: &str) -> Result<Vec<Route>, SendError> {
        let c = self
            .contact(dest)
            .ok_or_else(|| SendError::NoRoute(format!("no contact {dest}")))?;
        // Read on every send: the node's mail thread learns addresses as it goes.
        let gv = GvStore::read(&self.cfg.state_dir);
        let im = imessage_state(&self.cfg, self.imessage.as_ref().map(|s| &*s.shared));
        route::plan(c, &avail(&self.cfg, self.mailer.is_some(), &im), &gv)
            .map_err(SendError::NoRoute)
    }

    /// Send `text` to `dest` as from `from_call`, by the routes TX would use (only
    /// those of kind `only`, if given).
    pub fn deliver(
        &mut self,
        dest: &str,
        from_call: &str,
        text: &str,
        only: Option<RouteKind>,
    ) -> Result<Delivery, SendError> {
        let mut routes = self.plan(dest)?;
        if let Some(k) = only {
            routes.retain(|r| RouteKind::of(r) == k);
            if routes.is_empty() {
                return Err(SendError::NoRoute(format!(
                    "{dest} cannot be reached that way from here now"
                )));
            }
        }
        let c = self
            .contact(dest)
            .cloned()
            .ok_or_else(|| SendError::NoRoute(format!("no contact {dest}")))?;
        let n = &c.name;
        let mut last = String::new();
        for r in routes {
            match &r {
                Route::IMessage(h) => {
                    let (Some(im), Some(cfg)) = (&self.imessage, &self.cfg.imessage) else {
                        continue;
                    };
                    let report = im.send(&c, h, &tagged(&cfg.tag, from_call, text));
                    let detail = match &report.row {
                        Some(row) => format!(
                            "Messages: is_sent {}, error {}, {:.1} s",
                            row.is_sent,
                            row.error,
                            report.elapsed.as_secs_f32()
                        ),
                        None => format!(
                            "no sent message found, {:.1} s",
                            report.elapsed.as_secs_f32()
                        ),
                    };
                    match report.outcome {
                        ImOutcome::Confirmed => {
                            log::info!("sent to {n} by iMessage ({h})");
                            return Ok(Delivery { route: r, detail });
                        }
                        // Messages may still deliver it: no other route.
                        ImOutcome::Uncertain(e) => return Err(SendError::Gateway(e)),
                        ImOutcome::Definite(e) => {
                            log::warn!("iMessage to {n} failed before sending ({e}); trying the next route");
                            last = e;
                        }
                    }
                }
                Route::GoogleVoice(addr) => {
                    let (Some(m), Some(gv)) = (&self.mailer, &self.cfg.google_voice) else {
                        continue;
                    };
                    m.send_text(addr, &tagged(&gv.tag, from_call, text))
                        .map_err(|e| SendError::Gateway(format!("{e:#}")))?;
                    log::info!("sent to {n} by Google Voice");
                    if let Err(e) = GvStore::record_sent(&self.cfg.state_dir, unix_now()) {
                        log::error!("cannot record the Google Voice send: {e:#}");
                    }
                    return Ok(Delivery {
                        route: r,
                        detail: String::new(),
                    });
                }
                Route::Email(addr) => {
                    let Some(m) = &self.mailer else { continue };
                    m.send_as(addr, text, from_call)
                        .map_err(|e| SendError::Gateway(format!("{e:#}")))?;
                    log::info!("sent to {n} by email ({addr})");
                    return Ok(Delivery {
                        route: r,
                        detail: String::new(),
                    });
                }
            }
        }
        Err(SendError::Gateway(last))
    }
}

impl Services for LiveServices {
    fn send_message(&mut self, dest: &str, from_call: &str, text: &str) -> Result<(), SendError> {
        self.deliver(dest, from_call, text, None).map(|_| ())
    }

    fn ready_messages(&mut self) -> Vec<Message> {
        self.inbox.lock().map(|i| i.ready()).unwrap_or_default()
    }

    fn mark_read(&mut self, ids: &[u64]) {
        if let Ok(mut i) = self.inbox.lock() {
            if let Err(e) = i.mark_read(ids, unix_now()) {
                log::error!("inbox write failed: {e:#}");
            }
        }
    }

    fn weather(&mut self, grid: &str) -> Result<String, WxError> {
        let nws = self
            .weather
            .as_ref()
            .ok_or_else(|| WxError::Unavailable("weather is not configured".into()))?;
        nws.forecast(grid)
    }
}

/// Offline [`Services`] for `hfnode sim --offline`: logs instead of sending, canned
/// weather, and a shared inbox like the real thing.
pub struct OfflineServices {
    pub inbox: Arc<Mutex<Inbox>>,
}

impl Services for OfflineServices {
    fn send_message(&mut self, dest: &str, from_call: &str, text: &str) -> Result<(), SendError> {
        println!("  [offline] would send to {dest} as {from_call}: {text}");
        Ok(())
    }

    fn ready_messages(&mut self) -> Vec<Message> {
        self.inbox.lock().map(|i| i.ready()).unwrap_or_default()
    }

    fn mark_read(&mut self, ids: &[u64]) {
        if let Ok(mut i) = self.inbox.lock() {
            let _ = i.mark_read(ids, unix_now());
        }
    }

    fn weather(&mut self, grid: &str) -> Result<String, WxError> {
        Ok(format!("{grid} TDA SUNNY HI 95 WIND W 10 TNGT CLEAR LO 60"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_stamps() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_790_000_000), "2026-09-21");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(stamp(Some(100), 200), 100);
        assert_eq!(stamp(Some(300), 200), 200);
        assert_eq!(stamp(Some(0), 200), 200);
        assert_eq!(stamp(Some(-5), 200), 200);
        assert_eq!(stamp(None, 200), 200);
        assert_eq!(
            tagged(
                "{call} via HF radio, replies are read on air:",
                "W5XXX",
                "HI"
            ),
            "W5XXX via HF radio, replies are read on air: HI"
        );
    }
}
