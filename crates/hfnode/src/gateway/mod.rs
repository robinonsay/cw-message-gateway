//! Connections to the outside world: email/SMS, weather and the inbound filter.

pub mod email;
pub mod filter;
pub mod weather;

use crate::config::Config;
use crate::inbox::{Inbox, Message};
use crate::session::{Services, WxError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The real [`Services`]: SMTP for outbound, the shared inbox for inbound, NWS for weather.
pub struct LiveServices {
    pub cfg: Config,
    pub inbox: Arc<Mutex<Inbox>>,
    pub mailer: Option<email::Mailer>,
    pub weather: Option<weather::Nws>,
}

impl Services for LiveServices {
    fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String> {
        let mailer = self.mailer.as_ref().ok_or("email is not configured")?;
        let contact = self
            .cfg
            .contacts
            .iter()
            .find(|c| c.name == dest)
            .ok_or_else(|| format!("no contact {dest}"))?;
        mailer
            .send(&contact.address, text)
            .map_err(|e| format!("{e:#}"))
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
    fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String> {
        println!("  [offline] would send to {dest}: {text}");
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
