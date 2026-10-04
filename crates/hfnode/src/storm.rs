//! Storm stand-down: no tuning or transmitting while thunder is forecast or warned
//! at the station.
//!
//! The IC-7300 manual says never to operate the transceiver during a lightning
//! storm (precautions, manual text lines 315-318). Unattended, nobody is home to
//! switch off, so a thread asks the US National Weather Service every
//! `check_minutes` about the station's own location (`[storm]` in the config, not
//! the field operator's):
//!
//! - the hourly forecast for the next `lookahead_hours`: any mention of thunder
//!   ("Slight Chance Showers And Thunderstorms", "T-storms") counts. The NWS warns
//!   only for severe thunderstorms, so the forecast, not the alerts, is the main
//!   trigger;
//! - the active alerts for the point: any whose event, headline or description
//!   mentions thunder, lightning or a tornado.
//!
//! It fails closed. The station is held until the first check succeeds, after any
//! failed check (no network, an NWS outage, a reply that is not a forecast, a
//! forecast that does not cover the present hour), and when no check has finished
//! for three intervals (a hung request, a dead thread). A hold ends only after
//! `clear_minutes` without thunder. While held, the station neither tunes nor keys,
//! and a transmission under way is stopped and the radio forced to receive
//! ([`crate::station`]). Listening goes on; nothing here switches the antenna or the
//! power.

use crate::config::Storm;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Whether the station must not transmit for weather, shared between the storm
/// thread and the station.
pub struct StormHold {
    /// Why transmitting must wait (None: clear), and when that was decided.
    state: Mutex<(Option<String>, Instant)>,
    /// A decision older than this no longer counts as clear.
    stale_after: Duration,
    /// Whether any check has finished.
    checked: AtomicBool,
}

impl StormHold {
    /// A hold that is on until the first check clears it.
    pub fn new(stale_after: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((Some("no storm check yet".into()), Instant::now())),
            stale_after,
            checked: AtomicBool::new(false),
        })
    }

    /// Why transmitting must wait, or None when it may go ahead.
    pub fn reason(&self) -> Option<String> {
        let (reason, at) = &*self.state.lock().unwrap_or_else(|e| e.into_inner());
        if at.elapsed() > self.stale_after {
            return Some(format!(
                "no storm check for {} min",
                at.elapsed().as_secs() / 60
            ));
        }
        reason.clone()
    }

    /// Record the latest decision.
    pub fn set(&self, reason: Option<String>) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = (reason, Instant::now());
        self.checked.store(true, Ordering::SeqCst);
    }

    /// Wait up to `timeout` for the first check to finish; whether it has.
    pub fn wait_for_first_check(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while !self.checked.load(Ordering::SeqCst) {
            if t0.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
        true
    }
}

/// Turns each check's finding into the hold, keeping it on for `clear_after` after
/// the last thunder.
pub struct Watch {
    clear_after: Duration,
    last_thunder: Option<(Instant, String)>,
}

impl Watch {
    pub fn new(clear_after: Duration) -> Self {
        Self {
            clear_after,
            last_thunder: None,
        }
    }

    /// The hold after a check at `now` found `found`: `Ok(Some(why))` thunder,
    /// `Ok(None)` none, `Err` the check failed.
    pub fn update(
        &mut self,
        now: Instant,
        found: Result<Option<String>, String>,
    ) -> Option<String> {
        match found {
            Err(e) => Some(format!("storm check failed: {e}")),
            Ok(Some(why)) => {
                self.last_thunder = Some((now, why.clone()));
                Some(why)
            }
            Ok(None) => match &self.last_thunder {
                Some((t, why)) if now.duration_since(*t) < self.clear_after => Some(format!(
                    "{} min clear wait after: {why}",
                    self.clear_after.as_secs() / 60
                )),
                _ => None,
            },
        }
    }
}

/// The storm check against api.weather.gov for one point.
pub struct NwsStorm {
    agent: ureq::Agent,
    user_agent: String,
    base: String,
    /// `lat,lon` to four decimals, as the NWS wants them.
    point: String,
    lookahead_hours: u32,
    /// The point's hourly forecast URL, looked up once.
    hourly_url: Option<String>,
}

impl NwsStorm {
    pub fn new(cfg: &Storm, user_agent: &str) -> Result<Self> {
        Self::with_base(cfg, user_agent, "https://api.weather.gov")
    }

    pub fn with_base(cfg: &Storm, user_agent: &str, base: &str) -> Result<Self> {
        let (Some(lat), Some(lon)) = (cfg.latitude, cfg.longitude) else {
            anyhow::bail!("storm.latitude and storm.longitude are not set");
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Ok(Self {
            agent,
            user_agent: user_agent.to_string(),
            base: base.trim_end_matches('/').to_string(),
            point: format!("{lat:.4},{lon:.4}"),
            lookahead_hours: cfg.lookahead_hours,
            hourly_url: None,
        })
    }

    fn get(&self, url: &str) -> Result<Value> {
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/geo+json")
            .call()
            .with_context(|| format!("GET {url}"))?;
        Ok(resp.body_mut().read_json()?)
    }

    /// Check once, at `now` (Unix seconds): `Some(why)` if there is thunder.
    pub fn check(&mut self, now: i64) -> Result<Option<String>> {
        let hourly = match &self.hourly_url {
            Some(u) => u.clone(),
            None => {
                let p = self.get(&format!("{}/points/{}", self.base, self.point))?;
                let u = p["properties"]["forecastHourly"]
                    .as_str()
                    .context("the NWS has no hourly forecast for the station")?
                    .to_string();
                self.hourly_url = Some(u.clone());
                u
            }
        };
        let forecast = self
            .get(&hourly)
            .and_then(|fc| thunder_in_forecast(&fc, now, self.lookahead_hours));
        if forecast.is_err() {
            // Look the forecast up again next time, in case it moved.
            self.hourly_url = None;
        }
        // Thunder in the forecast is enough; "none" needs the alerts read as well.
        if let Some(why) = forecast? {
            return Ok(Some(why));
        }
        let alerts = self.get(&format!("{}/alerts/active?point={}", self.base, self.point))?;
        storm_alert(&alerts)
    }
}

fn mentions_thunder(text: &str) -> bool {
    let t = text.to_lowercase();
    ["thunder", "t-storm", "tstorm"]
        .iter()
        .any(|w| t.contains(w))
}

/// Thunder in the hourly forecast periods that overlap `now` to `now` +
/// `lookahead_hours`. The forecast must cover `now`: an old one would leave out the
/// hours that matter.
pub fn thunder_in_forecast(fc: &Value, now: i64, lookahead_hours: u32) -> Result<Option<String>> {
    let periods = fc["properties"]["periods"]
        .as_array()
        .context("the hourly forecast reply has no periods")?;
    let until = now + i64::from(lookahead_hours) * 3600;
    let mut covers_now = false;
    for p in periods {
        let time = |key: &str| {
            p[key]
                .as_str()
                .and_then(parse_time)
                .with_context(|| format!("hourly forecast {key} {}", p[key]))
        };
        let (start, end) = (time("startTime")?, time("endTime")?);
        if end <= now || start >= until {
            continue;
        }
        covers_now |= start <= now;
        let short = p["shortForecast"].as_str().unwrap_or("");
        let detail = p["detailedForecast"].as_str().unwrap_or("");
        if mentions_thunder(short) || mentions_thunder(detail) {
            let hour = p["startTime"]
                .as_str()
                .and_then(|s| s.get(11..16))
                .unwrap_or("");
            return Ok(Some(format!("thunder forecast {hour}: {short}")));
        }
    }
    anyhow::ensure!(
        covers_now,
        "the hourly forecast does not cover the present hour"
    );
    Ok(None)
}

/// An active alert whose event, headline or description mentions thunder,
/// lightning or a tornado.
pub fn storm_alert(alerts: &Value) -> Result<Option<String>> {
    let features = alerts["features"]
        .as_array()
        .context("the alerts reply is not an alert list")?;
    for f in features {
        let p = &f["properties"];
        let event = p["event"].as_str().unwrap_or("alert");
        let text = [&p["event"], &p["headline"], &p["description"]]
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if mentions_thunder(&text) || text.contains("lightning") || text.contains("tornado") {
            return Ok(Some(format!("alert: {event}")));
        }
    }
    Ok(None)
}

/// Unix seconds of an NWS time, `2026-10-04T14:00:00-05:00` (or `...Z`).
fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let offset = match &s[19..] {
        "Z" => 0,
        o if o.len() == 6 && matches!(&o[..1], "+" | "-") && &o[3..4] == ":" => {
            let secs =
                o.get(1..3)?.parse::<i64>().ok()? * 3600 + o.get(4..6)?.parse::<i64>().ok()? * 60;
            if &o[..1] == "-" {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };
    // Days since 1970-01-01 in the proleptic Gregorian calendar.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Start the storm thread for `cfg` and return the hold it keeps up to date. It
/// writes each change to the health log.
pub fn spawn(cfg: &Storm, user_agent: &str, health_log: Option<PathBuf>) -> Result<Arc<StormHold>> {
    let every = Duration::from_secs(cfg.check_minutes * 60);
    // Three intervals without a finished check and the hold comes back on.
    let hold = StormHold::new(every * 3);
    let mut source = NwsStorm::new(cfg, user_agent)?;
    let mut watch = Watch::new(Duration::from_secs(cfg.clear_minutes * 60));
    let shared = hold.clone();
    thread::Builder::new()
        .name("storm".into())
        .spawn(move || {
            let mut last: Option<Option<String>> = None;
            loop {
                let found = source
                    .check(crate::gateway::unix_now() as i64)
                    .map_err(|e| format!("{e:#}"));
                let reason = watch.update(Instant::now(), found);
                if last.as_ref() != Some(&reason) {
                    let value = match &reason {
                        Some(why) => {
                            log::warn!("storm stand-down: {why}");
                            format!("hold: {why}")
                        }
                        None => {
                            log::info!("storm stand-down lifted: no thunder near the station");
                            "clear".into()
                        }
                    };
                    if let Some(path) = &health_log {
                        crate::station::append_health(path, "storm", &value);
                    }
                }
                shared.set(reason.clone());
                last = Some(reason);
                thread::sleep(every);
            }
        })
        .context("starting the storm thread")?;
    Ok(hold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nws_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("1970-01-01T00:00:00-05:00"), Some(5 * 3600));
        // 2026-10-04T19:00:00Z
        assert_eq!(parse_time("2026-10-04T14:00:00-05:00"), Some(1_791_140_400));
        assert_eq!(parse_time("2026-10-04T19:00:00+00:00"), Some(1_791_140_400));
        assert_eq!(parse_time("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        for bad in [
            "",
            "2026-10-04",
            "2026-10-04 14:00:00Z",
            "2026-13-04T14:00:00Z",
            "2026-10-04T14:00:00",
            "2026-10-04T14:00:00-0500",
            "2026-10-04T14:00:00EST",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    const NOW: i64 = 1_791_140_400; // 2026-10-04T14:00:00-05:00

    fn hourly(shorts: &[&str]) -> Value {
        let periods: Vec<Value> = shorts
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({
                    "startTime": format!("2026-10-04T{:02}:00:00-05:00", 13 + i),
                    "endTime": format!("2026-10-04T{:02}:00:00-05:00", 14 + i),
                    "shortForecast": s,
                    "detailedForecast": "",
                })
            })
            .collect();
        serde_json::json!({ "properties": { "periods": periods } })
    }

    #[test]
    fn thunder_in_the_next_hours_holds() {
        // Periods start at 13:00; now is 14:00, so the 13:00 hour is over.
        let fc = hourly(&[
            "Thunderstorms",
            "Sunny",
            "Mostly Sunny",
            "Slight Chance T-storms",
        ]);
        assert_eq!(thunder_in_forecast(&fc, NOW, 2).unwrap(), None);
        assert_eq!(
            thunder_in_forecast(&fc, NOW, 3).unwrap(),
            Some("thunder forecast 16:00: Slight Chance T-storms".into())
        );
        let fc = hourly(&["Sunny", "Chance Showers And Thunderstorms"]);
        assert!(thunder_in_forecast(&fc, NOW, 1).unwrap().is_some());
        let fc = hourly(&["Sunny", "Rain Showers Likely", "Areas Of Fog"]);
        assert_eq!(thunder_in_forecast(&fc, NOW, 2).unwrap(), None);
    }

    #[test]
    fn a_forecast_that_misses_the_present_hour_is_an_error() {
        // Ends at 14:00: nothing about now.
        let fc = hourly(&["Sunny"]);
        assert!(thunder_in_forecast(&fc, NOW, 2).is_err());
        assert!(thunder_in_forecast(&serde_json::json!({"title": "oops"}), NOW, 2).is_err());
        let mut fc = hourly(&["Sunny", "Sunny"]);
        fc["properties"]["periods"][1]["startTime"] = "soon".into();
        assert!(thunder_in_forecast(&fc, NOW, 2).is_err());
    }

    #[test]
    fn alerts_that_mention_thunder_lightning_or_tornadoes_hold() {
        let alert = |event: &str, description: &str| {
            serde_json::json!({ "features": [ { "properties": {
                "event": event, "headline": "", "description": description } } ] })
        };
        for (event, description, held) in [
            ("Severe Thunderstorm Warning", "", true),
            ("Tornado Watch", "", true),
            (
                "Special Weather Statement",
                "Frequent cloud to ground lightning.",
                true,
            ),
            ("Flood Watch", "Heavy rain from thunderstorms.", true),
            ("Heat Advisory", "Hot.", false),
            ("Special Weather Statement", "Dense fog.", false),
        ] {
            assert_eq!(
                storm_alert(&alert(event, description)).unwrap().is_some(),
                held,
                "{event}"
            );
        }
        assert_eq!(
            storm_alert(&alert("Severe Thunderstorm Warning", "")).unwrap(),
            Some("alert: Severe Thunderstorm Warning".into())
        );
        assert_eq!(
            storm_alert(&serde_json::json!({"features": []})).unwrap(),
            None
        );
        assert!(storm_alert(&serde_json::json!({"title": "oops"})).is_err());
    }

    #[test]
    fn the_hold_stays_on_after_thunder_and_on_failures() {
        let mut w = Watch::new(Duration::from_secs(1800));
        let t0 = Instant::now();
        assert_eq!(w.update(t0, Ok(None)), None);
        assert!(w
            .update(t0, Err("timeout".into()))
            .unwrap()
            .contains("failed"));
        assert_eq!(w.update(t0, Ok(None)), None);
        assert_eq!(
            w.update(t0, Ok(Some("alert: Tornado Warning".into()))),
            Some("alert: Tornado Warning".into())
        );
        let later = t0 + Duration::from_secs(1799);
        assert_eq!(
            w.update(later, Ok(None)),
            Some("30 min clear wait after: alert: Tornado Warning".into())
        );
        assert_eq!(w.update(t0 + Duration::from_secs(1800), Ok(None)), None);
    }

    #[test]
    fn a_hold_starts_on_and_comes_back_when_checks_stop() {
        let hold = StormHold::new(Duration::from_millis(50));
        assert_eq!(hold.reason(), Some("no storm check yet".into()));
        assert!(!hold.wait_for_first_check(Duration::from_millis(10)));
        hold.set(None);
        assert!(hold.wait_for_first_check(Duration::ZERO));
        assert_eq!(hold.reason(), None);
        thread::sleep(Duration::from_millis(80));
        assert!(hold.reason().unwrap().starts_with("no storm check for"));
        hold.set(Some("alert: Tornado Warning".into()));
        assert_eq!(hold.reason(), Some("alert: Tornado Warning".into()));
    }

    /// Serve canned NWS replies on localhost: the points lookup, the hourly
    /// forecast (`hourly_status`, `hourly_body`) and the alerts (`alerts_status`,
    /// `alerts_body`).
    fn fake_nws(
        hourly_status: u16,
        hourly_body: String,
        alerts_status: u16,
        alerts_body: &'static str,
    ) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hourly_url = format!("{base}/hourly");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let (status, body) = if path.starts_with("/points/30.2672,-97.7431") {
                    (
                        200,
                        format!(r#"{{"properties":{{"forecastHourly":"{hourly_url}"}}}}"#),
                    )
                } else if path == "/hourly" {
                    (hourly_status, hourly_body.clone())
                } else if path.starts_with("/alerts/active?point=30.2672,-97.7431") {
                    (alerts_status, alerts_body.to_string())
                } else {
                    (404, "{}".to_string())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/geo+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    fn cfg() -> Storm {
        Storm {
            enabled: true,
            latitude: Some(30.267_153),
            longitude: Some(-97.743_061),
            lookahead_hours: 2,
            check_minutes: 5,
            clear_minutes: 30,
            user_agent: None,
        }
    }

    #[test]
    fn checks_the_station_point_and_fails_closed() {
        let sunny = hourly(&["Sunny", "Sunny", "Sunny"]).to_string();
        let stormy = hourly(&["Sunny", "Sunny", "Isolated Thunderstorms"]).to_string();
        let no_alerts = r#"{"features":[]}"#;
        let check = |hourly_status, body: &str, alerts_status, alerts| {
            NwsStorm::with_base(
                &cfg(),
                "test",
                &fake_nws(hourly_status, body.to_string(), alerts_status, alerts),
            )
            .unwrap()
            .check(NOW)
        };
        assert_eq!(check(200, &sunny, 200, no_alerts).unwrap(), None);
        assert_eq!(
            check(200, &stormy, 200, no_alerts).unwrap(),
            Some("thunder forecast 15:00: Isolated Thunderstorms".into())
        );
        assert_eq!(
            check(
                200,
                &sunny,
                200,
                r#"{"features":[{"properties":{"event":"Severe Thunderstorm Warning"}}]}"#
            )
            .unwrap(),
            Some("alert: Severe Thunderstorm Warning".into())
        );
        // Thunder in the forecast is enough even when the alerts are unavailable.
        assert!(check(200, &stormy, 503, "{}").unwrap().is_some());
        // But "none" needs both.
        assert!(check(200, &sunny, 503, "{}").is_err());
        assert!(check(500, "{}", 200, no_alerts).is_err());
        assert!(check(200, r#"{"title":"oops"}"#, 200, no_alerts).is_err());
        // Nothing listening at all.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert!(NwsStorm::with_base(&cfg(), "test", &closed)
            .unwrap()
            .check(NOW)
            .is_err());
    }
}
